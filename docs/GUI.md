# GUI 方案（Tauri 2 + Vue 3）

> **状态：G1–G5 代码已落地（2026-11-05），G3b / G6 未完成。**
> 已实现：Tauri 2 + Vue 3 骨架、`apm-app` 无 UI 内核、`.app` 打包与 ad-hoc 签名、
> Info.plist 用途声明、设备扫描与连接、诊断日志与诊断页、**实时电平表与推流（G2）**、
> **声卡选择与会话 WAV（G3）**、**音质报告页（G4）**、**按键映射编辑（G5）**，
> 以及 G2 的前置——`record` / `sink` 编排已从 `cli` 迁进 `apm-app`。
> 真机已验证的只有 G0 的构建与打包；**G1–G5 的验收条目需要接上设备、并由人在系统弹窗
> 点「允许蓝牙」才能跑**，本轮屏幕处于锁屏状态，探到的是 `no_adapter`，所以那些条目
> 一律记「未验证」，不写成通过。
> 实施记录、与方案的偏差、跑过的门禁结果见 §14（自检结论在 §14.2）；
> `Send` 相关三个坑的收尾在 §14.3。
> 本文描述要做什么、怎么切分、怎么验收；具体调度与口径以实现为准。

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
| 实时（已连过设备时默认） | 上面这一屏 | G2 |
| 设备烧写（首次启动默认） | 随包固件版本与 app 分区 MD5、USB 设备列表、读版本 / 一键烧写与进度、结果与重试指引 | M6 |
| 音质报告 | 对会话 WAV 跑 `analyze`，表格 + JSON 导出，明确区分"软件通过 / 真机听感待定" | G4 |
| 按键映射 | 3 个事件的键位编辑（一个键一个动作）、导入导出、恢复默认 | G5（M3） |
| 诊断 | 事件日志列表（时间 / 级别 / 来源）、复制诊断包、`selftest` 结果 | G1 |

「设备烧写」页的四条硬约束（口径见 [`PRODUCT.md`](PRODUCT.md) 的「开箱体验」）：

- **只有用户点「检测」或「写入固件」才碰设备。** `probe` 会让设备复位进下载模式，
  后台轮询等于把正在推流的设备偷偷断掉。
- **不擦 NVS。** 只写 bootloader / 分区表 / app 三段；重新烧写不清配对与映射。
- **校验失败不显示"完成"。** 后端返回 `Err` 时前端只能显示错误与重试指引。
- **复位打断的 BLE 要自己接回来。** 检测/烧写必然复位设备，BLE 一定断——这是 esptool 的
  机制，不是缺陷。但"点一次检测就得手动重连一次"不能算可接受的操作成本：
  `firmware_probe` / `firmware_flash` 先记下当前连的是哪台，做完调
  `AppCore::reconnect_after_reset`（等旧链路拆干净 → 带退避重连 5 次，中间失败不置
  `Error` 状态，避免界面上闪红色横幅）。重连失败不影响检测结果本身。

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
| `set_auto_connect` | `enabled` | `()` | 启动自动连接开关，立即落盘（§14.15） |
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
| `firmware_status` | — | `FirmwareStatus` | 随包镜像（版本/大小/MD5）+ 候选串口。**不碰设备**，随便调 |
| `firmware_probe` | `{ port }` | `ProbeResult` | **会让设备复位**。返回当前版本与 `up_to_date` / `blank` / `mismatch` / `unknown` |
| `firmware_flash` | `{ port }` | `FlashReport` | 三段写 + `FlashMd5` 校验；进度走 `firmware-progress`；校验不通过返回 `Err` |

事件（`emit`）：

| 事件 | 频率 | 载荷 |
| --- | --- | --- |
| `state-changed` | 边沿 | `{ state, error?: AppError }` |
| `telemetry` | 5 Hz | `{ rms_dbfs, peak_dbfs, buffer_ms, blocks, lost_blocks, seq_jumps, device_dropped, underruns, silent_frames, lock_misses, bytes_written, elapsed_ms }` |
| `event-log` | 边沿 | `{ ts_ms, level, source, message }` |
| `session-done` | 边沿 | `SessionSummary` |
| `battery` | 边沿 | `{ pct }`：`0..100` 或 `null`。连接后读一次 + 设备数值变化时推送；`null` = 设备明确报不可用（无 Battery Service / 读超时 / SOC 无效），界面显示「电量不可用」，不显示 0%、也不隐藏 |
| `firmware-progress` | 写入期间连续 | `0..100`。按字节数折算，三段不是各占 33% |

约定：`telemetry` 的 `ts_ms` 用**单调时钟**（`Instant`），不要用 `SystemTime` 或前端 `Date.now()` 做释放计时——前端计时器在后台会被节流。

约定：**连接后的一次性结论不能只走事件。** 自动连接（`auto_connect`）会在前端注册监听之前
就完成连接并读电量，Tauri 不重放事件，届时顶栏会永远空白（KI-009）。所以电量同时进
`AppSnapshot.battery_pct` / `battery_known`，前端 `onMounted` 先按快照回填，
事件的职责只是"之后的变化"。新增同类字段时照此办理。

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
   ⚠️ **ad-hoc 没有稳定身份**：重建后是否会重弹授权**不确定**（实测五次里三次弹、两次没弹）。
   开发期按"可能要重授"对待；G6 用正式证书根治。
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

| 阶段 | 内容 | 验收（必须实测） | 当前 |
| --- | --- | --- | --- |
| **G0 骨架 + 权限** | Tauri 空窗口；`detect_adapter` + `scan_start` 打通 | 双击 `.app` 弹出蓝牙授权；6 s 内列出 `AI Passport Mic`，RSSI 与 `apmctl scan` 相差 ≤ 6 dBm | **PASS（2026-11-05 真机）**：授权弹窗正常、6 s 扫到 7 个设备、RSSI 两边都是 −50 dBm。明细见 §14.7 |
| **G1 连接 + 状态机 + 日志** | connect/subscribe/disconnect、错误码映射、诊断 tab | 连上后 `state=ready`；拔设备电源 → **≤ 8 s** 内进 `idle` 且日志有记录；关系统蓝牙 → `bt_unavailable` | **部分 PASS**：`ready` 与 `ready → stopping → idle` 已验（修掉了两处 btleplug 挂死）；实测 **7.64 s**，符合 8 s 口径。**口径已于 2026-11-05 由 2 s 调整为 8 s**，理由见 §14.7；「关蓝牙 → `bt_unavailable`」未测 |
| **G2 实时电平 + 推流** | 电平表、指标条、start/stop、缓冲滑块 | 连续 10 min：0 欠载、0 静音帧；前端 ≥ 30 fps；CPU < 8%（M 系）、内存 < 150 MB | 代码完成。**GUI 侧 10 min 已跑（§14.10）**：50.03 块/秒、丢块 0、设备丢弃 0、缓冲 77–90 ms、漂移 +94 ppm；**欠载 4 次 / 142 帧 → "0 欠载" FAIL（KI-006）**。CPU/内存按全家桶口径均超：**CPU 20.7%**（主进程 3.2%）、**内存 166.9 MB**（主进程 26 MB）；前端 fps 未测。详见 §14.10 |
| **G3 声卡选择 + 会话 WAV** | 枚举/切换声卡、落盘、收尾顺序 | GUI 与 `apmctl record` 同时录 15 s，块数差 ≤ 1、丢块一致；会话 WAV 能被 `analyze` 直接吃 | 代码完成（枚举带采样率能力、不支持 16 kHz 的设备提前拒绝）；**对拍未做** |
| **G3b 回环自动验收** | 用 GUI 自身身份申请麦克风，恢复 `probe` / `--verify` | 推 440 Hz 正弦 → 回读 RMS > -30 dBFS；给出稳态延迟数值（这是延迟预算第一次可自动测） | **未实现**：`apmctl sink --verify` 仍在 CLI，GUI 侧没有回环入口 |
| **G4 音质报告** | analyze 视图 + JSON 导出 | 报告字段与 `apmctl analyze --json` 逐字段一致；未测字段为 null | 代码完成：报告构造只有一份（`apm_app::report`），GUI 的 JSON 由后端同一函数生成；采样率提醒（`rate_note`）与下混标记已与 `apmctl` 对齐（2026-11-05） |
| **G5 按键映射** | **3 个事件**（一个键一个动作）的编辑、导入导出、恢复默认 | 改成非默认键位 → 设备按键触发正确；OK 的 PTT 停止路径在任何配置下都可用 | 编辑/校验/落盘完成，`ok.release` 强制回填 `ptt-stop`；**"按键触发正确"依赖 M3 注入层，未实现**。事件表已于 2026-11-05 两次归并：15 → 6（§14.11）→ 3（§14.12） |
| **G6 打包** | 签名、公证、版本号、崩溃日志脱敏 | 干净机器上从 dmg 装 → 首次启动授权 → 完成 G1~G3 验收 | **未做**：需要 Apple 开发者证书；当前只有 ad-hoc 签名 |
| **M6 设备烧写页**（不在 G 系列，归 ROADMAP 的 M6） | 随包镜像信息、USB 设备枚举、读版本、分段写入、进度与结果 | 空片插上 → 一键写完 → 校验通过 → 能连上 BLE；重新烧写后配对与映射保留 | **真机已通（2026-11-06，`apmctl firmware probe/flash` 同一条内核路径）**：设备 `7552506-dirty` → `0.1.0`，三段写入 4.9 s、`FlashMd5` 一致、烧完自行复位回应用模式并恢复广播；烧写后配对仍在（NVS 未动）。**空片首烧未验**。约束见 §5 的「设备烧写」三条 |

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

表里的四份文件已随 G0 与 G1–G5 两轮落地更新；`../desktop/README.md` 的技术栈表、
目录树与「已知限制」也同步改过，"没有 GUI"那条已删。

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

### 14.1 G1–G5（2026-11-05）

按方案落地的部分不重复，这里只记**与方案不一致**的地方和踩到的坑。

| # | 变化 | 原因 |
| --- | --- | --- |
| 1 | 声卡对象放在一条独立的 **std 线程**里，不用 tokio 任务 | `cpal::Stream` 在 0.15 是 `!Send`（`NotSendSyncAcrossAllPlatforms`），放进 `Arc<AppCore>` 编译不过；线程内自持 `Sink` + WAV 写入器，主线程只喂有界 channel。**这条约束的上游变化与升级判据见 [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) KI-004** |
| 2 | channel 满时丢**最新**一块并计数，不是丢最旧 | `mpsc` 两端只能各自操作；32 块 = 640 ms，真满了说明声卡线程已经卡死，新旧数据都救不回来。计数进 `overflow_drops`，界面单独显示 |
| 3 | ~~新增 `apm_link::Ctrl` 句柄~~ **已被 §14.3 取代** | 当时的理由：`Link` 里有 `Pin<Box<dyn Stream + Send>>`，`&Link` 不是 `Send`，Tauri 命令没法 await `&Link` 的 async 方法。现在通知流装进 `Arc<AsyncMutex<…>>`，`Link` 回到 `Send + Sync + Clone`，`Ctrl` 已删除 |
| 4 | ~~判活改成 `apm_link::is_alive(handle)`~~ **已被 §14.3 取代** | 同上，现在就是 `Link::is_alive(&self)` |
| 5 | `SinkInfo.uid` 目前等于设备名 | cpal 0.15 没有 `Device::id()`；macOS 上设备名唯一。字段留着是为了以后上 WASAPI 不用改前端契约。cpal 0.17 起有 `Device::id()`，升级时顺手换掉（同 KI-004） |
| 6 | 事件是 **15** 个，不是文档早先写的 12 个（**已被 §14.11 取代：现在 6 个**） | schema 是 3 个按键 × 5 种手势（短按/双击/长按/按下/抬起），`config/README.md` 才是权威 |
| 7 | 新增错误码 `keymap_invalid` | 映射校验失败是真实分支，塞进 `internal` 会让用户查到"未预期错误" |
| 8 | 新增命令 `set_save_wav` / `analyze_session_json` / `reveal_path` | 勾选状态要持久化；"复制 JSON"必须由后端同一份报告函数生成，否则和 `apmctl analyze --json` 迟早不一致；访达定位需要路径 |
| 9 | **未实现** `run_selftest` | `.app` 里没有 `config/protocol-vectors.json`，跑也是"找不到向量文件"。协议自检仍由 `apmctl selftest` 承担 |
| 10 | 电平表：后端 5 Hz + 前端 rAF 插值 | 5 Hz 数据直接画会一跳一跳。前端只做动画（上升跟手、下降 24 dB/s，峰值 1.5 s 保持后 12 dB/s 释放），不自己算丢块率 |
| 11 | 会话 WAV 增量写：先占 44 字节头，收尾回填长度 | 一次会话可能几十分钟（16 kHz × 2 B/s ≈ 32 KB/s），攒内存没意义 |
| 12 | 断连判定 = 5 s 静默 + `is_connected` 探活；断链后再探一次适配器 | 拔电源不会产生任何通知，"什么都没有"才是信号；探适配器才能区分"设备没了"（`idle`）和"蓝牙被关了"（`no_adapter`） |
| 13 | `CTRL START` 失败时状态回到 `ready`，不是 `error` | 设备还连着，落到 `error` 会让"再点一次开始"被状态机挡住 |
| 14 | 会话 WAV 命名 `s<epoch_ms>.wav`，落在 `~/Library/Application Support/com.cnryb.ai-passport-mic/sessions/` | 与配置、按键映射同一个目录，卸载时一起清 |

**仍然没做**（不许在交付里写成已完成）：

- **G3b 回环自动验收**：`--verify` 还在 CLI，GUI 没有回环入口，麦克风权限没在 App 身份下验过。
- **G6 签名公证**：只有 ad-hoc 签名，没有 Apple 开发者证书。
- **M3 按键注入**：映射表能编辑、能校验，但**不会真的替你敲键盘**。按键映射页明确写了这一点。
- **真机验收**：G1–G5 的每一条实测（拔电源 2 s 内回 idle、10 min 0 欠载、GUI/CLI 对拍）都需要设备在手 + 人在弹窗点「允许」，本轮只验证到"内核启动、枚举到 BlackHole 2ch 并写进配置、无适配器时正确落到 `no_adapter`"。

### 14.2 本轮跑过的门禁（2026-11-05）

| 检查 | 命令 | 结果 |
| --- | --- | --- |
| 基线自检 | `./scripts/verify.sh` | **PASS**（Structure / Docs links / Scripts / Protocol 44 passed / Vectors / Desktop / Env 全绿） |
| 桌面端单测 | `cd desktop && cargo test` | **PASS** 93 passed / 0 failed / **1 ignored**，7 个测试目标：`apm-app` 41、`apm-protocol` 31、`c-vectors` 5、`apm-link` 5、`apm-sink` 4、`apmctl` 4、`glitch_repro` 3 |
| 静态检查 | `cargo clippy --all-targets` | **PASS**，**0 警告**（顺手清掉了 `protocol/src/glitch.rs` 与 `sink/src/playout.rs` 里两条既存的 `collapsible_if`） |
| 前端类型 | `npx vue-tsc --noEmit` | **PASS** |
| 前端构建 | `npm run build` | **PASS** |
| 打包 | `./scripts/build-gui.sh` | **PASS**，`.app` 30.29 MiB，`Info.plist` 用途声明与 bundle id 校验通过 |
| 真声卡冒烟 | `./scripts/test-sink.sh` | **PASS**（本机有 BlackHole：cpal 按 16 kHz 打开、推 0.5 s 块、`clear` 收尾都成立；**不代表音质合格**） |

| 启动冒烟 | `open "…/AI Passport Mic.app"` | **部分通过**：进程存活、写出 `settings.json`（`sink: "BlackHole 2ch"`, `target_ms: 80`）、日志确认状态 `idle → no_adapter`。**界面渲染未确认**（屏幕锁屏，截图为全黑） |

启动那次的实际日志（`~/Library/Logs/AI Passport Mic/app-20261105.jsonl`）值得单独说清楚，
因为它容易被误读：

```
应用内核启动 v0.1.0
配置：声卡=None 缓冲目标=80 ms 录 WAV=false
检测蓝牙适配器
输出声卡改为 BlackHole 2ch          ← 配置里没有声卡时自动选默认，并写回
适配器探测超时：6 s 内没拿到蓝牙适配器状态
状态 idle → no_adapter
```

- 「适配器探测超时」**不是**"蓝牙关着"：同机 `system_profiler SPBluetoothDataType` 显示
  `State: On`。真实原因是 TCC 蓝牙授权弹窗没人点（屏幕锁着），
  6 s 超时兜底把界面从"检测中"解出来，这正是 §14 第 5 条那个 bug 的修复在起作用。
- 「配置：声卡=None」是因为冒烟前删了 `settings.json`，随后自动选默认声卡并写回，
  不是读取配置失败。

**没跑过的**：`./scripts/build-firmware.sh`（本轮没改固件）、G1–G5 的人工验收路径、任何需要蓝牙适配器的场景。

### 14.3 三个坑的收尾（2026-11-05 第二轮）

§14.1 记了三个"只能这么写"的坑。这一轮把能消掉的消掉了，消不掉的明确标成债。

| 坑 | 处置 | 结果 |
| --- | --- | --- |
| `&Link` 不是 `Send`（因为有 `Pin<Box<dyn Stream + Send>>`） | **彻底消掉**：通知流装进 `Arc<AsyncMutex<…>>`（`Mutex<T>: Sync` 只需 `T: Send`），`Link` 变成 `Send + Sync + Clone` | `apm_link::Ctrl` 整个删除；`is_alive` 从自由函数变回 `Link::is_alive(&self)`；`next_notification` / `ping` 从 `&mut self` 变 `&self`。`core` 不再握着锁跨 await，Tauri 命令不用等那 700 ms 窗口 |
| `cpal::Stream` 是 `!Send` | **消不掉**（钉在 cpal 0.15）：改成可执行的债 —— [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) **KI-004** 写清上游 0.17/0.18 已给 CoreAudio 加 `Send`、破坏性改动清单、以及"升级前后各跑一次 `test-sink.sh`"的判据 | 设计不变（声卡独占线程），但它从"没得选"变成"有得选，暂时不选" |
| 真声卡测试"没声卡就 return"，`cargo test` 显示 ok | **消掉假通过**：改 `#[test] #[ignore]`，新增 [`scripts/test-sink.sh`](../scripts/test-sink.sh) 先探设备再跑，没设备就明说 SKIP | `cargo test` 现在会显示 `1 ignored`，不再假装测过；本机有 BlackHole，脚本实跑 **PASS** |

新增的回归门（都是编译期/生命周期层面的，不依赖硬件）：

- `link_is_send_sync_and_clone` —— 谁把 `Arc<AsyncMutex<…>>` 拆掉，这条立刻红；
- `shared_link_can_cross_await_boundary` —— 构造一个持有 `&Link` 的 `Send` future，
  能构造出来就说明 auto-trait 没退化；
- `core_stays_send_and_sync` —— `AppCore: Send + Sync`、`SessionHandle`/`Pipeline: Send`，
  有人把声卡对象挪进内核会当场编译失败；
- `session_can_restart_after_finish` —— 线程真的回收了，能连着开第二条会话；
- `drop_without_finish_still_closes_wav` —— 异常路径 `Drop` 仍 join 线程并回填 WAV 头。

**仍然没解决**：`cpal` 版本没升（KI-004）；真声卡测试只证明"打开/写入/收尾成立"，
**不证明音质、延迟、长时间稳定性**；`SinkInfo.uid` 还是设备名。

### 14.4 release 打包与重新授权（2026-11-05 第三轮）

| 变化 | 原因 | 结果 |
| --- | --- | --- |
| `build-gui.sh --release` **原来是坏的** | Tauri 2 里 release 是**默认**行为，没有 `--release` 这个 flag；脚本把 `${profile}` 原样传给 `tauri build`，被当成位置参数报 `unexpected argument '--release' found`。只有 `-d/--debug` 可选 | 改成按 profile 决定是否加 `-d`：`--release` → `tauri build --bundles app`，默认 → `tauri build --debug --bundles app`。修完出包通过 |
| G2 的性能指标必须用 **release** 包测 | debug 包带边界检查、不打优化，CPU 与帧率都不能当验收数据 | `.app` 出在 `desktop/target/release/bundle/macos/` 与 debug 包**同 bundle id**，TCC 记录不因路径变化而重置（但见下一条） |
| **重新打包后要不要重新授权：不确定** | ad-hoc 签名没有稳定身份，macOS 可能按路径、也可能按代码哈希认，行为不稳 | 实测五次：debug 路径探测成功 → 换 release 路径 **弹窗** → release 原地重建 **不弹** → 改 `core.rs` 重建 **弹** → 再改重建 **又弹** → 改 keymap 重建 **不弹**。结论：**重建后可能弹也可能不弹，无法预测**；开发期一律按"可能要重授"对待，别把"上一次没弹"当保证。G6 换成正式开发者证书才能根治 |

这一条对 G0 的验收口径有影响：**"首次启动弹授权"要在最终那个构建上验一次**，
不能拿早先一次 debug 包的授权成功来顶。`apmctl-macos.sh` 那套 `APMCTL.app` 是另一套
bundle，因此 CLI 的授权与 GUI 的授权互不相干，两处都可能各弹一次。

### 14.5 `--smoke-scan` 不生效 → 改成哨兵文件（2026-11-06 已修，入口打通）

§14 第 4 条给的无人值守验收开关，2026-11-05 那轮**用不了**：

| 尝试 | 结果 |
| --- | --- |
| `open <abs-path>.app --args --smoke-scan` | 诊断日志里 `SMOKE` 出现 **0** 次，只有 boot 那条"检测蓝牙适配器"；`pgrep -fl` 显示的 argv 里也没有该参数 |
| `open -n <abs-path>.app --args --smoke-scan` | 同上 |
| `open -a "AI Passport Mic" --args --smoke-scan` | 同上 |
| `launchctl setenv APM_GUI_SMOKE scan` | 被系统拒绝：`Not privileged to set domain environment` |
| 直接跑 `Contents/MacOS/apm-gui --smoke-scan` | **exit 134（SIGABRT）**——TCC 以缺蓝牙用途声明 abort，印证 §8 第 2 条"必须 LaunchServices 启动" |

**修法（2026-11-06）**：参数改从**哨兵文件**进来，`open` 只负责用 LaunchServices 启动。

```bash
# 数据目录：~/Library/Application Support/com.cnryb.ai-passport-mic/
printf firmware > ~/Library/Application\ Support/com.cnryb.ai-passport-mic/smoke
open "/path/to/AI Passport Mic.app"
```

`smoke_mode()` 依次看：命令行 `--smoke-<mode>` → 环境变量 `APM_GUI_SMOKE` → 哨兵文件；
**读到就删**（一次性，不会下次启动又跑）。当前支持两个模式：

| mode | 干什么 | 退出码 |
| --- | --- | --- |
| `scan` | 检测适配器 → 扫描 6 s，统计带本项目服务的设备数 | 0 = 至少一台 |
| `firmware` | 连接 → USB 检测（必然复位设备）→ 自动重连，验"检测不会把 BLE 永久打断" | 0 = 重连回 ready |

**实测（2026-11-06）**：哨兵文件路径生效，日志出现 `FW_SMOKE ...` 并按退出码退出；
`--args` 那条仍然无效，别再试。

> ⚠️ 每次 `build-gui.sh` 重新 ad-hoc 签名都会让 TCC 的蓝牙授权失效，App 首次访问
> CoreBluetooth 要**重新点一次「允许」**。冒烟前若日志里是
> `适配器探测超时 / no_adapter`，先人工点授权，再跑冒烟。

### 14.6 GUI 与 CLI 不能同时占设备（不是 bug，但要写进流程）

设备只支持一条 BLE 连接。GUI 开着（哪怕没点连接、只是探测过）时起 `apmctl sink`，
块率会从 50 块/秒掉到 1.5 块/秒，欠载和漂移数值全部失真。**所有 CLI 侧长测前必须先退 GUI。**

配套的另一种脏数据来自设备自己（异常断开后不恢复），实测与处置见
[`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) **KI-005**。


### 14.7 G0 / G1 真机验收（2026-11-05，人在场那一轮）

#### G0 权限与扫描：**PASS**

| 验收项 | 结果 |
| --- | --- |
| 双击 `.app` 弹出蓝牙授权 | 出现，手动点「允许」后生效（同一个 `com.cnryb.ai-passport-mic` 在不同构建上弹了不止一次，见 §14.4） |
| 6 s 内列出 `AI Passport Mic` | 扫描窗口 6000 ms，一次扫到 **7 个设备**，目标设备在列 |
| RSSI 与 `apmctl scan` 相差 ≤ 6 dBm | 两边都是 **−50 dBm**，差 0 dBm |

#### G1 连接与断链：**部分 PASS**

| 验收项 | 口径 | 实测 | 结论 |
| --- | --- | --- | --- |
| 连上后 `state=ready` | 必须 | `idle → connecting → ready`，连接耗时 **2.1 s**（第一次 5.0 s，冷启动广播窗口） | PASS |
| 拔设备电源 → 回到 `idle` | **≤ 8 s**（2026-11-05 由 2 s 调整） | **7.64 s**（5 s 静默 + 2 s 探活 + 0.64 s 调度） | PASS |
| 断链后能从 `ready` 走回 `idle` | 必须 | `ready → stopping → idle`，`stopping` 只停留 1 ms | PASS（修完才成立，见下） |
| 关系统蓝牙 → `bt_unavailable` | 必须 | 未测（关蓝牙会一起断掉本机的 BLE 鼠标，留到专场） | SKIP |

**这一轮修掉的两个真 bug**（都只有真机能暴露）：

1. `btleplug::Peripheral::is_connected()` 在设备消失后**不返回**。原来的判定是
   「5 s 静默 → 问一次链路」，问的那一句直接挂死，监控循环再也不动，
   **状态永久停在 `ready`、界面继续显示"已连接"、日志一行不写**。
   修法：探活加 2 s 超时（`LINK_PROBE_TIMEOUT`），超时按"已断开"处理并记 warn。
2. `Peripheral::disconnect()` 同样会挂。修完第 1 条之后，状态走到 `stopping` 就停住，
   永远回不到 `idle`。修法：断开调用加 3 s 超时（`DISCONNECT_TIMEOUT`），
   并且当监控循环已经判定链路丢失（`link_lost`）时**直接跳过**这次调用——
   对一只已经不在的外设调断开只会白等。

**为什么原来的"2 s 内进 idle"是错的口径**：实现上是**轮询**判活（设备 1 Hz 心跳 → 5 s 静默
才认定可疑 → 再探活），最快也不可能低于 5 s；`is_connected()` 卡住时还要再加 2 s。
这条口径是从"系统会立刻推断连事件"的假设写下来的，但本实现没有订阅
btleplug 的 `CentralEvent::DeviceDisconnected`。

**决定（2026-11-05，项目负责人确认）：口径改成 ≤ 8 s**，与轮询设计对齐。
事件驱动那条路仍然值得做（能把反应压到 < 1 s），但另开一轮：work 在 `apm-link` 要保存
并暴露 manager 的事件流，轮询保留作兜底。

### 14.8 G2 第一次尝试：**未通过，且这一跑的数据不可信**（2026-11-05）

按 G2 的口径（连续 10 min：0 欠载、0 静音帧；前端 ≥ 30 fps；CPU < 8%、内存 < 150 MB）试了一跑，
结果是废的：

| 指标 | 实测 | 说明 |
| --- | --- | --- |
| 块率 | 约 **28 块/秒**（名义 50/s） | 一半的块根本没到 |
| 丢块 / 跳变 | 3498 / 3398 | 序号跳变说明链路层就丢了 |
| 设备丢弃 | **3992** | 设备自己报的发送侧丢弃，干净会话是 0 |
| 欠载 / 静音帧 | 6100 / 1089273 | 声卡在狂补静音 |
| 缓冲 / 漂移 | 掉到 20 ms / 打到 −9997 ppm 下限 | 存量被抽干 |

**结论：不是 GUI 性能问题，是设备又进了 KI-005 的"半死"态。**证据链：同一台机器、同一条链路，
复位后 `apmctl sink --seconds 30` 立刻回到 1498 块 / 0 丢块 / 0 欠载 / 设备侧丢弃 0。
差别只在"连接 → START"的间隔（CLI 立刻发，GUI 等了约 90 秒）。详见
[`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) **KI-005 的第二种触发方式**。

**因此 G2 的四条口径一个都没拿到有效数据**：

- 0 欠载 / 0 静音帧：这次不成立（但成因是设备态，不是声卡）；
- 前端 ≥ 30 fps：没有采集手段，未测；
- CPU < 8% / 内存 < 150 MB：**自动化侧读不到** —— 沙箱内 `ps` / `top` 都是
  `operation not permitted`，退出沙箱后仍被拦；用 UI 脚本操作活动监视器被辅助功能权限挡住。
  这一项要么人肉读活动监视器，要么在应用内加一个"诊断页显示自身 CPU/内存"的功能（后者更可靠，
  也符合"GUI 自己能报自己"的方向）。

**下次怎么跑**：复位设备 → 立刻连接 → **30 s 内点开始**（别让它在 `ready` 上干等）→ 跑满 10 min。

### 14.9 开流前预置 STOP（2026-11-05 修复）

针对 §14.8 那次作废的跑法，改了两处：

| 位置 | 改动 |
| --- | --- |
| `apm-app` `start_stream` | 校验通过后、开声卡之前，先发一次 `CTRL STOP`，静默 `CAPTURE_RESET_SETTLE = 120 ms`，再开声卡、再 `START`。STOP 对 idle 设备是幂等的，失败只记 debug 不阻塞 |
| `apmctl record` / `apmctl sink` | 同样在 `start_capture()` 前调 `reset_capture_state()`（STOP + 120 ms），跟内核语义对齐 |

为什么值得加：这是"设备停在半死态"的唯一低成本兜底。加之前，GUI 只要在 `ready` 上多停一会儿
就必然半死（§14.8 实测块率 28/s、设备侧丢弃 3992）；加之后每次开流都先把设备打回 idle 再启动。
代价是点"开始"到出声多 120 ms。

**2026-11-05 深夜补充**：这一条现在只是**兜底**，不再是主解。KI-005 的根因已定位到
"连接间隔跑偏 + 断链不清 START"，固件侧已修（首选参数广播、参数更新重试、断链回调、
`LINK_SLOW` 标志）。`STOP` 预置保留——它对旧固件仍然有效，对修好的固件是一次幂等的
120 ms 开销。详见 [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) KI-005 的「修复」小节。

**顺带发现（文档与实现不一致，已记录未修）**：`AGENTS.md` 写的"编排只有一份——
`connect` / `start_stream` / `stop_stream` 全在 `apm-app`，`cli` 只解析参数与打印"
**对 `record` / `sink` 不成立**。`crates/cli/src/cmd.rs` 仍然自己调
`apm_link::connect()`、自己的通知循环、自己的 `start_capture()` / `stop_capture()` / `disconnect()`；
共用的只有 `apm_app::Pipeline`（解码、丢块、毛刺抑制）与 `apm_app::report`。

后果不是纯文档问题：本轮这个"开流前 STOP"就得在**两处**各改一遍，正是 §12 决定 D1 想避免的分叉。
要真正统一，`apm-app` 需要一个能覆盖"不落 WAV / 不枚举声卡 / 自定义时长"的会话入口，
`record` 与 `sink` 都改成调它。**这属于重构，另开一轮，别顺手做。**

### 14.10 G2 第二次尝试：链路全绿，欠载仍有 4 次（2026-11-05 13:29–13:39）

加了 §14.9 的"开流前预置 STOP"之后重跑。这次是**同一台机器、同一设备、GUI 全程前台**：

| 指标 | 10 min 时读数 | 口径 | 结论 |
| --- | --- | --- | --- |
| 块数 | **46949 / 939.0 s = 50.00 块/秒**（收尾统计；10 min 采样点是 30922/618 s） | 50/s | PASS |
| 丢块 / 跳变 | **0 / 0** | 0 | PASS |
| 设备侧丢弃 | **0** | 0 | PASS（对比 §14.8 的 3992） |
| 缓冲存量 | 77–90 ms（目标 80） | 不触底 | PASS |
| 漂移补偿 | +12 ~ +99 ppm | ±5000 内 | PASS |
| 欠载 / 静音帧 | **13 次 / 1351 帧（≈84 ms）**（10 min 时是 4 次） | 0 | **FAIL**（同 KI-006） |
| 锁定失败 / 溢出 | 1 / 0 | 0 | 锁定失败 1 次可忽略 |
| 前端 ≥ 30 fps | 未测 | ≥30 | 未测（没有采集手段） |
| CPU / 内存 | 见下 | CPU < 8%、内存 < 150 MB | 部分 |

**内存（活动监视器实测，按进程拆开）**：

| 进程 | 内存 | 线程 |
| --- | --- | --- |
| AI Passport Mic Graphics and Media | 76.4 MB | 9 |
| tauri://localhost（WebView） | 58.5 MB | 7 |
| AI Passport Mic（主进程） | **26.0 MB** | 27 |
| AI Passport Mic Networking | 6.0 MB | 4 |
| **合计** | **166.9 MB** | — |

⚠️ **"内存 < 150 MB"这条口径必须写死按哪个数**：按主进程 26 MB 算宽松通过；按 App 全家桶
合计 166.9 MB 算超了 16.9 MB。Tauri 的 WebView 与音视频 helper 是独立进程，只看主进程会漏掉
83% 的占用。建议后续口径改成"全部相关进程 RSS 合计"。

**CPU（活动监视器 %CPU，采样于推流稳定期）**：

| 进程 | % CPU | % GPU |
| --- | --- | --- |
| AI Passport Mic Graphics and Media | **11.4** | 2.6 |
| tauri://localhost（WebView） | **6.1** | 0.0 |
| AI Passport Mic（主进程） | **3.2** | 0.0 |
| AI Passport Mic Networking | 0.0 | 0.0 |
| **合计** | **20.7** | — |

⚠️ 同内存那条：**按主进程 3.2% 算 PASS，按全家桶 20.7% 算 FAIL（超 `CPU < 8%` 口径 2.6 倍）**。
占比最大的是 Graphics and Media helper——电平表 60 fps canvas 渲染 + 音频回调。
要压下来得从"电平表只在窗口可见时按 60 fps 画、后台降到 5 fps"这类地方下手，
现在无论窗口是否可见都在全速渲染。

**结论**：这一跑证明了 §14.9 的修复有效——同样的 GUI 操作路径，设备侧丢弃从 3992 变成 0，
块率从 28/s 回到 50.03/s。**G2 的实测结论（按"全家桶"口径）**：

| 口径 | 实测 | 结论 |
| --- | --- | --- |
| 块率 / 丢块 / 设备侧丢弃 | 50.03/s、0、0 | PASS |
| 缓冲 / 漂移 | 77–90 ms、+94 ppm | PASS |
| 0 欠载 / 0 静音帧 | 4 次 / 142 帧 | **FAIL**（KI-006） |
| CPU < 8% | 主进程 3.2% / 全家桶 20.7% | **FAIL**（全家桶口径） |
| 内存 < 150 MB | 主进程 26 MB / 全家桶 166.9 MB | **FAIL**（全家桶口径） |
| 前端 ≥ 30 fps | 未测 | 未测 |

也就是说：**链路和音质链路的四项全过，卡住的是"性能"与"偶发欠载"**，而不是功能。

**这一跑的最终数字（以会话收尾统计为准，不是中途截图）**：第一段 **46949 块 / 939.0 s =
50.00 块/秒**、欠载 13、静音 1351 帧（≈84 ms）、丢块 0、设备侧丢弃 0；
第二段（点了停止后又点开始）987 块 / 19.7 s、欠载 2、同样 0 丢块。

**欠载率对比**（同机同设备）：

| 路径 | 时长 | 欠载 | 折合每分钟 |
| --- | --- | --- | --- |
| CLI `sink`（§KI-006） | 600 s | 10 | 1.0 次/min |
| GUI（本节） | 939 s | 13 | 0.83 次/min |

两条路径同一个量级，**说明 KI-006 与 GUI/WebView 无关**，是声卡回调或漂移控制器层面的问题。

**另一个小发现**：点「停止」之后日志里还会出现 2 条 `收到音频块但当前未推流`——STOP 之后设备
仍有余量在发（在途的块）。这属于预期，但会让人误以为"没用停"。若要干净，可以在 STOP 后
把音频通知静默 200 ms 再退订。

### 14.11 按键事件归并：15 → 6（2026-11-05，**已被 §14.12 取代**）

**背景**：一台无线麦克风只有 UP / DOWN / OK 三个键，映射表却有 3 × 5 = 15 行
（短按 / 双击 / 长按 / 按下 / 抬起）。用户记不住，固件还要维护双击窗口（180 ms）与
长按阈值（500 ms）两套定时器，联调时每加一个手势就要重测一遍时序。

**决定（项目负责人确认）**：归并到 **3 键 × 按下/抬起 = 6 个事件**，保留 PTT 的
"按住说话、松手停"语义。没有采用"每键一个动作"的开关式方案——那会把 PTT 从
按住改成切换，得同时改固件状态机。

| 改动面 | 内容 |
| --- | --- |
| `apm_protocol::keymap` | `SCHEMA` → `ai-passport-mic-keymap-2`；`EVENTS` 常量改成 6 个（**唯一一份**事件表）；`is_supported_event` 只认 `press`/`release` |
| v1 兼容 | 读 `keymap-1` 文件时**静默丢弃** `short`/`double`/`long`，其余照旧校验——整份拒绝等于把用户上次存的映射无声清空。落盘/导出即 v2 |
| `apm_app::keymap_store` | `EVENTS` 不再本地硬编码 15 个字符串，直接取 `keymap::EVENTS`（v1 时两处各抄一份，差点只改一半） |
| GUI | `EVENT_LABEL` 6 条；导入框提示改 v2；页面说明写明"注入层属于 M3，尚未实现" |
| `config/README.md` | 重写「事件名」「迁移：v1 → v2」「与固件的关系」三节，`button-map.example.json` 换成 v2 样例（单测直接 `include_str!` 它，坏掉会当场红） |
| **固件 / 协议** | **一行没改**。EVENT 帧仍然上报 5 种手势码（0..4），跨端向量、UUID、帧格式都不动；PC 端只消费 `PRESS`/`RELEASE` |

**为什么不动固件**：改协议要同步 `ARCHITECTURE.md`、重生成 `config/protocol-vectors.json`、
两端一起重测，收益只是省几帧通知。固件的 `buttons.c` 手势判定留着不碍事——
它现在只是"多发了几种 PC 不认识的事件"。真要删，等下一版固件顺路做。

**新的坑（已规避）**：v1 的 `up.short` 在 v2 里是**非法事件名**，会走 `UnknownEvent`。
所以 `parse` 必须先在 schema 层判断版本，再决定"丢弃"还是"报错"——顺序反了就会让
所有老用户打开就报错。

### 14.12 再归并一步：6 → 3，一个键一个动作（2026-11-05）

§14.11 归到 6 个之后，项目负责人要求**再减到 3 个**："每个键一个就行"。于是：

| | v1 | v2（§14.11） | **v3（本节）** |
| --- | --- | --- | --- |
| 事件数 | 15 | 6 | **3** |
| 事件名 | `up.short` … | `up.press` / `up.release` … | **`up` / `down` / `ok`** |
| PTT | 按下开 / 抬起停 | 按下开 / 抬起停 | **按一下开、再按一下关（`ptt-toggle`）** |

**这是一个语义变更，不只是删行。** 之前 OK 是"按住说话"，松手自动停；现在是 toggle，
按一次开、再按一次关。好处是三个键三个动作，零手势记忆；代价必须写清楚：

> **toggle 没有"松手自动停"。** 开着麦去接杯水，回来还在采。
> M3 的注入层落地时必须同时做两件事：① 界面上永远能看到"正在采集"；
> ② 一个超时兜底（例如连续 10 分钟自动停）。

| 改动面 | 内容 |
| --- | --- |
| `apm_protocol::keymap` | `SCHEMA` → `ai-passport-mic-keymap-3`；`EVENTS = ["up","down","ok"]`；`PTT_TOGGLE` 取代 `ptt-start`/`ptt-stop`；错误类型 `PttStopMissing` → `PttToggleMissing` |
| 迁移 | `Legacy::{None,V2,V1}` 三态：v2 的 `press` **优先**、`release` 仅在缺 press 时补位；v1 先丢手势再按 v2 规则走；两个旧动作名统一改写成 `ptt-toggle`。**迁移必须在事件名校验之前**，顺序反了老文件直接报错 |
| `apm_app::keymap_store` | `locked` 从 `ok.release` 改成 `ok`；兜底强制回填 `ptt-toggle` |
| GUI | `EVENT_LABEL` 3 条；页面说明改成"开关式 + 忘记关麦是真实风险" |
| 配置 | `config/README.md` 重写（事件名 / 迁移 v1·v2 → v3 / 与固件的关系），`button-map.example.json` 换 v3 |
| 固件 / 协议 | **仍然一行没改**：EVENT 帧照发 0..4 五种手势码，PC 只消费 `PRESS` |

**已知副作用（写进 `ARCHITECTURE.md` 了）**：双击 = 两次 `PRESS`，在 toggle 下等于"开了又关"，
看起来像没反应。三键方案必然的代价；真要双击语义，得单独加事件而不是指望 toggle 兼容。

**测试**：v1 → v3、v2 → v3 两条迁移各有一格（`migrates_v1_by_dropping_gestures`、
`migrates_v2_press_wins_over_release`），另有 `rejects_legacy_ptt_action_in_v3` 钉住
"旧动作名写进 v3 必须报错，不能被悄悄放过"。

### 14.13 注入层落地：开麦/关麦自动点按快捷键（2026-11-05）

**需求原文**：「我需要对着麦克风说话的时候，同时触发快捷键」——确认后定型为
**一次性点按，开麦与关麦各触发一个可配置的快捷键**。

| 层 | 改动 |
| --- | --- |
| `apm_protocol::keymap` | schema → `keymap-4`；新增 `HOOK_EVENTS = ["capture.start","capture.stop"]` 与 `hooks` 表；钩子里禁止 `ptt-toggle`（开麦瞬间再 toggle 会自己关掉自己，错误类型 `HookCannotToggle`）；`resolve()` 统一查两张表 |
| `apm_app::inject`（新模块） | CGEvent 键注入：键码表覆盖 `is_supported_key` 白名单里 macOS 真实存在的键（`printscreen`/`menu`/`insert` 没有，明确报"做不到"而不是静默丢弃）；修饰键走事件 flags，**不单独发修饰键事件**，因此不存在"按下去没松开" |
| `apm_app::core` | `fire_hook(event)`：开麦成功 → `capture.start`；手动停止 → `capture.stop`；**链路断开导致的收尾也要触发**（否则上次注入的"开"就永远没人复位）。注入跑在独立 std 线程（`apm-inject`），失败只记日志，**绝不影响音频路径**；每次现读映射表 |
| `apm_app::keymap_store` | 界面上 3 按键 + 2 钩子一起展示（`all_events()`），存盘分流：按键进 `bindings`、钩子进 `hooks` |
| GUI | `EVENT_LABEL` 加两行（"开麦时 · 注入" / "关麦时 · 注入"）；按键映射页顶部**权限引导横幅**：`inject_permission` 命令查 `AXIsProcessTrusted()`，缺权限时分步引导 + `open_accessibility_settings` 一键跳到系统设置的辅助功能面板，缺权限状态下每 2 s 自动复查，授权后横幅自动消失（无需重启） |

**为什么钩子不能绑 `ptt-toggle`**：开麦那一刻注入一个"toggle 采集"，等于自己把自己关掉。
`KeyMap::parse` 直接拒绝，错误文案解释原因。

**权限的坑（macOS）**：没有辅助功能权限时 `CGEventPost` **不报错、静默丢弃**。
所以注入前先 `AXIsProcessTrusted()` 探一次，没权限直接报
「没有"辅助功能"权限：系统设置 → 隐私与安全性 → 辅助功能，把本 App 打开后重试」，
而不是让用户以为功能坏了。**这个权限需要人工去系统设置里勾**，应用内无法替代——
能做的是把人送到那一页（按键映射页的引导横幅）并在勾完之后自动感知。

**测试**：`ptt-toggle` 不可注入、白名单键码全覆盖校验、钩子 round-trip 不丢、
钩子禁 `ptt-toggle`。（真机端到端——键真的落到目标应用——需要人工授权限后验证，尚未做。）

### 14.14 设备按键直连：OK 开关采集，up/down 注入（2026-11-05）

真机验收发现：设备按键事件到 PC 后**只记了日志、没有任何动作**（`handle_notification`
的 `Notification::Event` 分支），用户按 OK"没反应"——钩子触发的开/关其实都是界面按钮
触发的。本次把 M3 的「按键事件直连」补上：

| 项 | 说明 |
| --- | --- |
| PRESS 分发 | `Notification::Event` 且 `event == msg::EVENT_PRESS` 时进 `handle_button_press`：`button_name()` 映射 0/1/2 → `up`/`down`/`ok`，现读映射表解析动作 |
| `ok`（`ptt-toggle`） | `toggle_ptt`：没在推流就 `start_stream`（用当前设置的 save_wav），在推流就 `stop_stream`——与界面按钮同一条路径，钩子照常触发 |
| `up` / `down` | 配了普通键 → `inject_in_background` 点按注入；未绑定 = 不动作 |
| 防抖 | `ptt_switching: AtomicBool`：切换进行中再来的 PRESS 忽略。DOUBLE 的两次 PRESS 相隔约 180 ms，落在切换窗口内只算一次动作——修复了 ARCHITECTURE 里"开了又关"的已知副作用 |
| 日志 | 动作结果落 `button` 源：`PTT 开麦完成` / `关麦完成` / busy 记 debug / 失败记 warn |

### 14.15 启动自动连接上次设备（2026-11-05）

**需求原文**：「每次重启之后都要重新去搜索、连接这个设备，能不能自动连接」。

| 层 | 改动 |
| --- | --- |
| `apm_app::settings` | 新增 `last_device_id` / `last_device_name`（连接成功即写盘，手动断开不清除）与 `auto_connect` 开关（默认开）；旧 settings.json 无新字段时反序列化回默认值，不炸 |
| `apm_app::core` | `connect()` 成功后记住设备；新增 `auto_connect()`：仅在 `idle` 且开关打开时复用 `connect()`（同一套状态机/超时/监控派生），**失败不算事故**——落回 `idle` 记 warn，不弹错误横幅；新增 `set_auto_connect()` |
| GUI 启动 | `setup` 的后台任务在 `detect_adapter` 成功后调 `auto_connect()`，不阻塞界面；状态经 `state-changed` 事件照常驱动前端 |
| GUI 界面 | 设备卡头部新增「自动连接」勾选框（`set_auto_connect` 命令）；idle 且有记录时显示"上次连接：X，启动时自动连接" |

**为什么可行**：macOS 的 `PeripheralId` 是 CoreBluetooth identifier，跨进程重启稳定；
`apm_link::connect(Some(id))` 自带扫描循环（10 s 窗口），设备已上电并广播即可直连，
不依赖"先扫描过一次"。

**边界（如实声明）**：设备没上电/不在广播时自动连接 10 s 后放弃，回到待连接；
自动重连（运行中掉链后自己重连）**未做**，仍要手动扫一次。真机验收未跑（需人工接设备）。
