# GUI 方案（Tauri 2 + Vue 3）

> **状态：G0 已落地（2026-11-05），G1–G6 未开始。**
> 已实现：Tauri 2 + Vue 3 骨架、`apm-app` 无 UI 内核、`.app` 打包与 ad-hoc 签名、
> Info.plist 用途声明、设备扫描与连接、诊断日志与诊断页。
> 未验证：首次蓝牙授权弹窗与"扫到设备"（需要人在系统弹窗点「允许」，本轮两次启动都停在等待授权）。
> 实施记录与本方案的偏差见 §14。
> 截至 2026-11-04，`desktop/crates` 只有 `protocol` / `link` / `sink` / `cli` 四个 crate，
> `desktop/Cargo.lock` 中 `tauri` 命中数为 0，没有任何前端目录。
> 本文只描述要做什么、怎么切分、怎么验收；确认后再写第一行代码。

## 1. 为什么现在做，以及它真正解决什么

GUI 的第一价值**不是**界面好看，是两件被环境卡死的事：

| 问题 | 现状 | GUI 之后 |
| --- | --- | --- |
| macOS 蓝牙权限（TCC） | 裸二进制调 CoreBluetooth 直接被系统 abort；只能把 `apmctl` 塞进 `/tmp/APMCTL.app` + 原生启动器重定向 stdout 这种 hack 跑 | 正规 `.app` bundle 自带 `Info.plist` 用途声明，双击启动，系统弹一次授权即可 |
| 回环自动验收 | `sink --verify` / `probe` 读到 -120 dBFS：宿主（WorkBuddy / Terminal / Warp）都没有 `NSMicrophoneUsageDescription`，麦克风权限拿不到 | GUI 以自己身份申请麦克风权限 → 探针可用 → 端到端延迟与"音频真的进了声卡"可以自动测 |
| 日常使用 | 每次要开终端敲 `./scripts/apmctl-macos.sh sink --to "BlackHole 2ch"` | 一个窗口：点连接、点开始 |

所以 G0（骨架 + 权限打通）是整个 GUI 的**前置门槛**，不是"顺手第一步"。G0 不过关，不做 G1。

## 2. 选型

| 方案 | 权限/bundle | 复用现有 Rust crate | 图表与中文排版 | 结论 |
| --- | --- | --- | --- | --- |
| **Tauri 2 + Vue 3 + Vite + TS** | 原生 `.app`，可声明任意 usage description | 同进程直接 `use apm_link` / `apm_sink`，音频零序列化 | Web 技术栈，canvas 手绘电平表最省事 | **选它** |
| egui / eframe | 需自己搭 bundle | 可以 | 中文需要手动加载字体、交互控件偏原始 | 备用；只在"不想引入 Node 工具链"时考虑 |
| Slint | 需自己搭 bundle | 可以 | 声明式还行，生态与图表弱 | 否 |
| Electron | 可以 | **不行**，音频要跨进程传 | 最好 | 否，为音频路径引入无意义的 IPC |
| 本地 Web 服务 + 浏览器 | **解决不了**：权限仍挂在宿主浏览器上 | 可以 | 好 | 否，问题 1 没解决 |

固定版本：Tauri 2.x、Vue 3.4+、Vite 5、TypeScript 5。Node 仅构建期需要，不进运行期。

## 3. 架构：先把应用内核抽出来，GUI 只是壳

**这是本方案最重要的一条决定。** 现在 `scan/status/record/sink` 的编排逻辑全在
`crates/cli/src/cmd.rs` 里。GUI 如果直接调 `link` + `sink`，就会出现第三份编排逻辑，
和 CLI 慢慢分叉——本项目在 C/Rust 协议上已经用共享向量防过一次这种分叉，桌面端内部
不该再犯。

```text
                 ┌───────────────────────┐
   apmctl (CLI) ─┤                       │
                 │   crates/app (新)      ├─ protocol
 apm-gui (Tauri)─┤   状态机 / 编排 / 指标  ├─ link
                 │   事件广播 / 会话记录   ├─ sink
                 └───────────────────────┘
```

| 决定 | 内容 | 理由 |
| --- | --- | --- |
| D1 | 新增 `desktop/crates/app`（`apm-app`），承载状态机、采集与写入编排、指标聚合、会话记录；`cli` 与 `gui` 都只调它 | 一套逻辑两个前端，避免第三份实现 |
| D2 | 音频数据**不跨进程、不跨 IPC**，前端只拿数字 | 160 B×50/s 喂给 webview 没有意义，且必然引入抖动 |
| D3 | 逐块事件不上桥：前端只收 5 Hz `telemetry` 快照 + 边沿事件 `state-changed` / `event-log` / `error` | webview 的 rAF 是 60 Hz，50 Hz 事件会让主线程排队 |
| D4 | Tauri 侧只做 command → `apm-app` 调用 + 事件转发，不放业务分支 | 命令层薄，测试仍可在 `apm-app` 里做（无 UI 可跑） |
| D5 | 事件日志结构化落盘（JSON Lines），GUI 显示可过滤列表 | 出问题要能拿到证据，而不是"界面上看见一行红字" |

### 线程与数据流

```text
btleplug 通知线程 ──decode──▶ 有界 mpsc(32 块) ──▶ sink writer 线程 ──▶ cpal 回调
        │                        │                        │
        └── seq 跳变计数          └── 解码后即统计 RMS/峰值   └── underrun / lock_miss
                                          │
                          指标聚合线程（200 ms tick）
                                          │
                              emit("telemetry", snapshot)  ──▶ 前端 canvas
```

| 线程 | 职责 | 阻塞风险 | 约束 |
| --- | --- | --- | --- |
| BLE 回调（btleplug 自带） | 收通知、解析帧、写有界 channel | **不能阻塞** | channel 满则丢最旧并计数，不 `blocking_send` |
| sink writer | 写 cpal 环形缓冲、落 WAV | 磁盘 IO | WAV 单独缓冲 256 KB，不占音频路径 |
| cpal 回调 | 只从预填缓冲取数 | 实时线程 | 已实现的 `try_lock` 失败 → 整块静音 + `lock_misses` |
| 指标聚合 | 200 ms 算一次快照 + `emit` | 无 | 不做 DB / 不做网络 |
| Tauri 主线程 | webview | 前端卡会卡这里 | 前端单帧工作量必须 < 8 ms |

`stop` 的顺序沿用 CLI 修过的坑：**先停止写声卡 → 再 CTRL STOP → 最后断链**，否则收尾会多出几次欠载。

## 4. 状态机

单一枚举，前后端共用同一份 serde 定义（`snake_case`），前端不允许自造状态。

| 状态 | 含义 | 允许的操作 |
| --- | --- | --- |
| `no_adapter` | 没有蓝牙适配器 / 未授权 | 无（只显示修复指引 + 重新检测） |
| `idle` | 已就绪，未扫未连 | scan / 选声卡 / selftest |
| `scanning` | 扫描中，有窗口期（默认 6 s） | stop_scan |
| `connecting` | 连接 + 订阅中（超时 10 s） | 取消 |
| `ready` | 已连、已订阅，未推流 | start / disconnect / 改声卡 |
| `streaming` | 正在写声卡 | stop / 改缓冲目标 |
| `stopping` | 收尾中（有上限 2 s） | 无 |
| `error{code, detail}` | 明确失败 | 回到 `idle`（可重试） |

迁移要点（只列容易写错的）：

- `scanning → connecting` 必须**先停扫描再连**，btleplug 同时扫描与连接会拿到不稳定的句柄。
- `ready → streaming` 的前置条件：声卡已选中且采样率 ∈ {8000, 16000, 32000, 44100, 48000}；不满足直接进 `error{sink_rate_unsupported}`，**不允许**先开流再报错。
- 设备断开 → 无论当前在哪个状态，一律 `stopping → idle`，并 `sink.clear()`。禁止留着旧缓冲继续喂静音。
- `error` 不是终态，但必须带 `code`；前端按 `code` 显示对应修复指引，不显示裸字符串。

错误码（固定枚举，前端做映射表）：

| code | 触发 | 界面给什么 |
| --- | --- | --- |
| `bt_unauthorized` | TCC 拒绝 | "系统设置 → 隐私与安全性 → 蓝牙" + 重新检测按钮 |
| `bt_unavailable` | 适配器关闭 | "打开蓝牙" |
| `device_not_found` | 扫描窗口内没发现本项目服务 | 检查设备是否上电 / 是否被别的机器占用 |
| `connect_timeout` | 10 s 未完成 | 重试 |
| `subscribe_failed` | 特征值订阅失败 | 重连；连 3 次失败提示重启设备 |
| `sink_missing` | 选定设备消失 | 重新枚举 + BlackHole 安装指引（**不静默安装**） |
| `sink_rate_unsupported` | 声卡不支持 16 kHz | 显示声卡支持的采样率，给"不做重采样"的说明链接 |
| `protocol_mismatch` | 块长 ≠ 165 / magic 不符 | 提示固件与桌面端版本不一致 |
| `busy` | 当前状态不允许该操作（连接中又点扫描、已连接又扫描） | 提示先停止正在进行的操作 |

## 5. 界面

单窗口，默认 900×620，最小 780×520。不做多窗口、不做托盘常驻（见 §12）。

```text
┌────────────────────────────────────────────────────────────────┐
│ ● READY   AI Passport Mic        RSSI -48 dBm   缓冲 74 ms  01:12│
├────────────────────────────────────────────────────────────────┤
│                                                                │
│  -6  ▏                                                         │
│ -12  ████████████████████████████████▏             -12.4 dBFS  │
│ -18  ██████████████████████▏                       峰值 -9.8   │
│ -24  ████████████████▏                                         │
│ -36  ████████▏                                                 │
│ -48  ███▏                                                      │
│ -60  ▎                                                         │
│      ├────────────── 最近 10 秒 ──────────────┤                │
├────────────────────────────────────────────────────────────────┤
│ 设备 [扫描 ▾] AI Passport Mic (F4:12:34:56:78:9A)  [连接] [断开]│
│ 声卡 [BlackHole 2ch ▾] [重新枚举]                                  │
│ 缓冲 [———●————] 80 ms        [ 开始 ] [ 停止 ]   ☐ 同时保存 WAV  │
├────────────────────────────────────────────────────────────────┤
│ 块 2998  丢 0  跳变 0  设备丢弃 0  欠载 0  静音帧 0  锁定 0      │
└────────────────────────────────────────────────────────────────┘
```

Tab（右上角切换，不引 vue-router，条件渲染即可）：

| Tab | 内容 | 依赖阶段 |
| --- | --- | --- |
| 实时（默认） | 上面这一屏 | G2 |
| 诊断 | 事件日志列表（时间 / 级别 / 来源）、复制诊断包、`selftest` 结果 | G1 |
| 音质报告 | 对会话 WAV 跑 `analyze`，表格 + JSON 导出，明确区分"软件通过 / 真机听感待定" | G4 |
| 按键映射 | 12 个事件的键位编辑、导入导出、恢复默认 | G5（M3） |

前端工程约束：

- 电平表和历史柱用 **canvas 手绘**，不用 DOM 条、不引图表库。50 Hz 更新 DOM 必卡。
- 不引 UI 组件库（Element/Naive 之类）：体积、样式漂移和"看起来像后台管理"都不值得，手写 CSS 更可控。
- 状态全部来自 `telemetry` 快照与 `state-changed`，前端**不自己算**丢块率和欠载数。
- 深色为主（麦克风类工具常驻使用），跟随系统仅切换一张 CSS 变量表。

## 6. IPC 契约

命令（`invoke`）——全部幂等友好，返回 `Result<T, AppError>`，`AppError { code, detail }`：

| 命令 | 入参 | 返回 | 备注 |
| --- | --- | --- | --- |
| `snapshot` | — | `AppSnapshot` | 前端启动时拉一次，之后靠事件 |
| `detect_adapter` | — | `AdapterState` | 首次启动与"重新检测" |
| `scan_start` | `window_ms?`（≤ 12000） | `()` | 到点自动 `scan_stop` |
| `scan_stop` | — | `Vec<DeviceInfo>` | `DeviceInfo{ id, name, rssi, is_ours }` |
| `connect` | `device_id` | `()` | 进度靠 `state-changed` |
| `disconnect` | — | `()` | |
| `list_sinks` | — | `Vec<SinkInfo>` | `SinkInfo{ name, uid, channels, min_rate, max_rate }` |
| `set_sink` | `name` | `()` | 存到配置，下次启动恢复 |
| `set_buffer_target` | `ms`（40–200） | `()` | 流中也可改，取下一批生效 |
| `start_stream` | `{ save_wav: bool }` | `{ session_id }` | 前置校验不满足 → `AppError` |
| `stop_stream` | — | `SessionSummary` | 含块数/丢块/欠载/时长/落盘路径 |
| `run_selftest` | — | `SelftestReport` | 与 `apmctl selftest` 同一份向量 |
| `analyze_session` | `session_id` | `QualityReport` | 未测字段写 null，不猜 |
| `copy_diagnostics` | — | `String` | 生成一段可粘贴的纯文本（版本/设备/指标/最近 50 条事件） |
| `reveal_log` | — | `()` | 打开日志目录 |

事件（`emit`）：

| 事件 | 频率 | 载荷 |
| --- | --- | --- |
| `state-changed` | 边沿 | `{ state, error?: AppError }` |
| `telemetry` | 5 Hz | `{ rms_dbfs, peak_dbfs, buffer_ms, blocks, lost_blocks, seq_jumps, device_dropped, underruns, silent_frames, lock_misses, bytes_written, elapsed_ms }` |
| `event-log` | 边沿 | `{ ts_ms, level, source, message }` |
| `session-done` | 边沿 | `SessionSummary` |

约定：`telemetry` 的 `ts_ms` 用**单调时钟**（`Instant`），不要用 `SystemTime` 或前端 `Date.now()` 做释放计时——前端计时器在后台会被节流。

## 7. 指标口径（必须写死，不然界面上两个数字会对不上）

| 指标 | 定义 | 来源 |
| --- | --- | --- |
| `rms_dbfs` | `20·log10(rms/32768)`，下限夹到 -90 | 解码后、写 sink 前统计（一个采样不漏） |
| `peak_dbfs` | 同窗口最大绝对值，界面保持 1.5 s 后按 12 dB/s 释放 | 同上 |
| `buffer_ms` | sink 内可用样本数 / 16000 × 1000 | sink 侧，**不是** channel 深度 |
| `underruns` | cpal 回调取不到足够样本的次数 | sink 侧 |
| `silent_frames` | 因 `try_lock` 失败而输出整块静音的次数 | sink 侧，单独计，不并入 underrun |
| `lost_blocks` | 序号跳变推算的缺失块 + 设备 STATUS 上报的丢弃 | 两者分列显示，不合并成一个"丢块"数字 |
| `elapsed_ms` | 从 `streaming` 进入时刻起的单调时长 | app 层 |

「设备侧丢弃」和「序号跳变」必须分开显示——CLI 已经踩过这个坑：只看序号跳变会得到
"0 丢块"的乐观结论，而设备在发送阶段丢的块序号根本没出来。

## 8. 权限与打包

| 平台 | 需要的声明 | 注意 |
| --- | --- | --- |
| macOS | `NSBluetoothAlwaysUsageDescription`、`NSMicrophoneUsageDescription` | 缺蓝牙键会 abort；缺麦克风键只影响探针/回环验证 |
| Windows | 无 TCC | VB-CABLE 设备名含空格，`set_sink` 要按 UID 匹配而不是模糊名 |
| Linux | 第一阶段不覆盖 | — |

三条硬约束：

1. **bundle identifier 固定为 `com.cnryb.ai-passport-mic`**，开发期也不要改。macOS TCC 授权按 bundle id 记，改了等于所有用户重新授权一次。
2. 开发期用 ad-hoc 签名（`codesign -s -`）；首次启动必须由 LaunchServices 启动（`open`）才能触发授权弹窗，直接跑 `target/debug/apm-gui` 不会弹。
3. 打包不夹带 BlackHole（GPL-3.0，随包分发需商业许可），只做检测 + 指引 + 装完一键验证。

## 9. 目录与命名

```text
desktop/
  Cargo.toml                 # workspace 增加 members: crates/app, crates/gui
  crates/
    app/                     # apm-app：无 UI 内核（状态机、编排、指标、会话）
    gui/                     # apm-gui：Tauri 2 主程序
      Cargo.toml
      tauri.conf.json        # identifier = com.cnryb.ai-passport-mic
      icons/
      src/main.rs            # tauri::Builder + 14 个 command 的薄封装
      frontend/              # Vue 3 + Vite + TS（package.json 在此）
        src/{App.vue,views/,components/,lib/}
```

GUI 必须放在同一个 workspace 里，否则 `link` / `sink` 的依赖会走出两份 lockfile，
版本一漂移就是"CLI 能跑 GUI 不能跑"。

## 10. 分阶段与验收

每个阶段单独可验收，交付时**分项报状态**，不许压成一句"GUI 好了"。

| 阶段 | 内容 | 验收（必须实测） |
| --- | --- | --- |
| **G0 骨架 + 权限** | Tauri 空窗口；`detect_adapter` + `scan_start` 打通 | 双击 `.app` 弹出蓝牙授权；6 s 内列出 `AI Passport Mic`，RSSI 与 `apmctl scan` 相差 ≤ 6 dBm |
| **G1 连接 + 状态机 + 日志** | connect/subscribe/disconnect、错误码映射、诊断 tab | 连上后 `state=ready`；拔设备电源 → 2 s 内进 `idle` 且日志有记录；关系统蓝牙 → `bt_unavailable` |
| **G2 实时电平 + 推流** | 电平表、指标条、start/stop、缓冲滑块 | 连续 10 min：0 欠载、0 静音帧；前端 ≥ 30 fps；CPU < 8%（M 系）、内存 < 150 MB |
| **G3 声卡选择 + 会话 WAV** | 枚举/切换声卡、落盘、收尾顺序 | GUI 与 `apmctl record` 同时录 15 s，块数差 ≤ 1、丢块一致；会话 WAV 能被 `analyze` 直接吃 |
| **G3b 回环自动验收** | 用 GUI 自身身份申请麦克风，恢复 `probe` / `--verify` | 推 440 Hz 正弦 → 回读 RMS > -30 dBFS；给出稳态延迟数值（这是延迟预算第一次可自动测） |
| **G4 音质报告** | analyze 视图 + JSON 导出 | 报告字段与 `apmctl analyze --json` 逐字段一致；未测字段为 null |
| **G5 按键映射** | 12 事件编辑、导入导出、恢复默认 | 改成非默认键位 → 设备按键触发正确；OK 的 PTT 停止路径在任何配置下都可用 |
| **G6 打包** | 签名、公证、版本号、崩溃日志脱敏 | 干净机器上从 dmg 装 → 首次启动授权 → 完成 G1~G3 验收 |

依赖关系：G0 → G1 → G2 → G3 串行；G3b、G4、G5 可并行；G6 等 G5。
**G2 之前必须先完成 §12 第 1 条的编排迁移** —— `apm-app` 目前只接管了 `detect_adapter` /
`scan` / `connect`，`record` / `sink` 的编排还在 `crates/cli/src/cmd.rs`。

## 11. 风险与已知坑

| 风险 | 影响 | 应对 |
| --- | --- | --- |
| Tauri 2 + 现有 `rust-toolchain.toml`（1.95.0） | 编译失败 | Tauri 2 要求 1.77+，1.95 满足；只是首次 `cargo build` 会拉 300+ crate，冷编译 5–8 min |
| cpal 设备热插拔 | 装完 BlackHole 后 `coreaudiod` 未加载，枚举不到 | 界面提供"重新枚举"按钮；文档写清 `killall coreaudiod` |
| webview 后台节流 | 窗口最小化后 `telemetry` 堆积 | 事件用 `emit` 且前端做"只保留最后一帧"合并；不要用 `setInterval` 驱动渲染 |
| WAV 落盘抖动 | 磁盘 IO 影响音频路径 | 独立线程 + 256 KB 缓冲；磁盘慢时降级为"只报错，不阻塞音频" |
| 偶发满量程毛刺（未定位） | 界面上会看到 -0.0 dBFS 尖峰 | GUI 不用尖峰触发任何自动动作；诊断包里带原始块序号，便于下次定位 |
| 仪表读数与 CLI 不一致 | 用户不信数据 | §7 口径写死，G3 用"同时录"做对拍 |

## 12. 已确认决策（2026-11-04 定稿）

以下 5 条已由项目负责人确认，不再是待定项。改这些决定要同步改本章并说明理由。

| # | 问题 | 结论 | 对实现的直接后果 |
| --- | --- | --- | --- |
| 1 | CLI 在 GUI 之后怎么定位 | **保留为诊断通道**（GUI 主入口，`apmctl` 继续用于脚本化与免 UI 取证） | `apm-app` 必须**不依赖任何 GUI 类型**（不引 tauri、不含窗口/事件循环概念）；`scan/record/sink` 的编排迁进 `apm-app`，`cli` 只做参数解析与输出格式化；`scripts/` 里依赖 `apmctl` 的验证脚本不用重写 |
| 2 | 首发平台 | **只做 macOS** | G0 只做 `.app` / dmg；声卡枚举只走 CoreAudio，不写 WASAPI 分支；VB-CABLE / NSIS 留到首发后单独一轮；跨平台抽象点到为止，不提前造 |
| 3 | 会话 WAV 是否默认落盘 | **默认关，用户勾选才录** | `start_stream{save_wav:false}` 为默认；界面勾选框状态要持久化；隐私口径写进 README；未勾选时磁盘 IO 完全不参与音频路径 |
| 4 | 按键映射编辑 | **进首发（G5），但不作为首发门槛** | G0 的 tab 路由要给第 4 个 tab 留位置（先做只读展示当前映射）；G0–G3 不依赖 M3 的注入层；G5 未完成不算首发阻塞项，但要在交付里如实标为 SKIP |
| 5 | 托盘常驻 / 菜单栏图标 | **不做** | 关窗即退出；不做"窗口关闭但进程常驻"的状态分支，省掉一整套生命周期复杂度。后续要加再说 |

第 1 条是这轮最容易被写歪的地方：`cli/src/cmd.rs` 现在直接调 `link` + `sink` 做编排。
迁进 `apm-app` 之后，**CLI 里不应该再出现任何音频流程控制代码**，只留解析与打印。
否则两个前端会各长出一套编排，回到本项目在协议层已经用共享向量防过一次的分叉问题。

## 13. 落地后要同步的文档


| 文件 | 改什么 |
| --- | --- |
| [`ROADMAP.md`](ROADMAP.md) | M2 增补 GUI 阶段（G0–G3b），M3/M4 指向对应 tab |
| [`ARCHITECTURE.md`](ARCHITECTURE.md) | 模块表把「GUI 应用」从"未开始"改为实现路径；补 app 内核与 GUI 的边界 |
| [`../desktop/README.md`](../desktop/README.md) | 技术栈表 GUI 行、目录树加 `crates/app` 与 `crates/gui`、删掉"没有 GUI"这条已知限制 |
| [`../AGENTS.md`](../AGENTS.md) | 交付状态清单加 `GUI (G0-G6): PASS / FAIL / SKIP` |

顺带发现（**已于 2026-11-05 修正**）：`ROADMAP.md` M2 曾写着"输出到虚拟声卡 —— 未实现"、
"真机验证 scan/status/record —— 未做"，而这几项都已实测通过；`device/README.md` 标题还挂着
"未真机验证"，`AGENTS.md` 交付清单也没有 GUI 行。这一轮连同上面四份文件一起对齐了。

表里剩一条没做：`../desktop/README.md` 的技术栈表与目录树已随 G0 落地更新，
「没有 GUI」那条限制已删；后续 G1/G2 落地时按同样方式补。

## 14. 实施记录（G0）

按计划落地的部分不再重复，这里只记**与方案不一致**的地方和踩到的坑。

| # | 变化 | 原因 |
| --- | --- | --- |
| 1 | 命令 `copy_diagnostics` 改成 `diagnostics_text` | 后端只返回文本，写剪贴板放前端（`navigator.clipboard` 不可用时回退 `execCommand`），这样后端不用引剪贴板插件 |
| 2 | 新增命令 `log_path` | 诊断页要显示日志文件路径，没必要为此拉整份快照 |
| 3 | 新增错误码 `busy` | 状态机里"当前状态不允许该操作"是真实分支（连接中又点扫描、已连接又扫描）。塞进 `internal` 会让用户查到莫名其妙的原因 |
| 4 | 新增 `--smoke-scan` 启动参数 | G0 的"扫到设备"需要一个无人值守、可复现的验收路径。它走的是同一套检测 + 扫描代码，不是另写一份 |
| 5 | 适配器探测加 6 s 超时 | **实测踩到的真 bug**：授权弹窗没人点、或蓝牙关闭时，`btleplug` 永远不返回，界面会无限卡在"检测中"。超时后落到 `no_adapter` 并明确提示两种可能原因 |
| 6 | workspace `default-members` 排除 `crates/gui` | tauri 会拉 400+ 依赖，放进默认成员会让 `cargo test` / `verify.sh` 每次都冷编译；GUI 单独 `cargo build -p apm-gui` |
| 7 | 构建期用本机 crates 镜像（`desktop/.cargo/config.toml`，已 gitignore） | crates.io 直连实测 17 KB/s，400+ 依赖拉不完；镜像下 272 个 crate 24 秒拉完。**这是本机配置，不入库** |

`Info.plist` 里两个 usage description 经 `codesign` 校验已绑定进签名
（`Info.plist entries=18`，`codesign --verify --deep --strict` 通过）。
`bundle.macOS.signingIdentity = "-"`（ad-hoc）：不设它时 Tauri 只做 linker 签名，
`Info.plist=not bound`，TCC 认不出这是哪个 App。
