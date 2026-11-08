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

固定版本：Tauri 2.x、Vue 3.4+、Vite 6、TypeScript 5。Node 仅构建期需要，不进运行期。

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
| D3 | 逐块事件不上桥：前端只收 5 Hz `telemetry` 快照 + 边沿事件（`state-changed` / `devices` / `event-log` / `session-done` / `battery` / `stream-aborted`，全部包在单一 `app-event` 通道里按 `type` 分派，见 §6） | webview 的 rAF 是 60 Hz，50 Hz 事件会让主线程排队 |
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
| BLE 回调（btleplug 自带） | 收通知、解析帧、写有界 channel | **不能阻塞** | channel 满则丢**最新**一块并计数（`overflow_drops`），不 `blocking_send`。理由见 §14.2 第 2 条 |
| sink writer | 写 cpal 环形缓冲、落 WAV | 磁盘 IO | WAV 走 `BufWriter`（默认 8 KB 行缓冲），不占音频路径；磁盘慢时降级为只报错 |
| cpal 回调 | 只从预填缓冲取数 | 实时线程 | `try_lock` 失败 → 整块静音 + `lock_misses`（**次数**，与 `silent_frames` 是两回事） |
| 指标聚合 | 200 ms 算一次快照 + `emit` | 无 | 不做 DB / 不做网络 |
| Tauri 主线程 | webview | 前端卡会卡这里 | 前端单帧工作量必须 < 8 ms |

`stop` 的顺序沿用 CLI 修过的坑：**先停止写声卡 → 再 CTRL STOP → 最后断链**，否则收尾会多出几次欠载。

## 4. 状态机

单一枚举，前后端共用同一份 serde 定义（`snake_case`），前端不允许自造状态。

| 状态 | 含义 | 允许的操作 |
| --- | --- | --- |
| `no_adapter` | 没有蓝牙适配器 / 未授权 | 无（只显示修复指引 + 重新检测） |
| `idle` | 已就绪，未扫未连 | scan / 选声卡 / 自动连接 |
| `scanning` | 扫描中，有窗口期（默认 6 s） | stop_scan |
| `connecting` | 连接 + 订阅中（超时 10 s） | 取消 |
| `ready` | 已连、已订阅，未推流 | start / disconnect / 改声卡 |
| `streaming` | 正在写声卡 | stop / 改缓冲目标 |
| `stopping` | 收尾中（`join` 声卡线程，**无硬超时**；实测毫秒级） | 无 |
| `error{code, detail}` | 明确失败 | 回到 `idle`（可重试） |

迁移要点（只列容易写错的）：

- `scanning → connecting` 必须**先停扫描再连**，btleplug 同时扫描与连接会拿到不稳定的句柄。
- `ready → streaming` 的前置条件：声卡已选中且**原生支持 16 kHz**（本项目不做隐式重采样）；
  不满足直接进 `error{sink_rate_unsupported}`，**不允许**先开流再报错。
  注意 `set_sink` 阶段就会提前拒绝一次，不等点「开始」。
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
| `protocol_mismatch` | 块长 ≠ 165 / magic 不符；或 CTRL START 写入失败 | 提示固件与桌面端版本不一致 |
| `keymap_invalid` | 映射校验失败：未知事件名/键名/修饰键，或开麦键（绑 `ptt-toggle` 的键）不是恰好一个 | 指出具体哪一条不合法；开麦键缺失或重复都报错，重复时列出冲突的事件名 |
| `flash_no_device` | 没有可烧写的串口：VID/PID 不符或不是 ESP32-C3 | 直连电脑、换 USB 口、确认已上电 |
| `firmware_image_invalid` | 随包镜像缺失或解析失败 | 安装包不完整，重装 |
| `flash_failed` | 烧写过程失败 | 重插重试；反复失败按住 BOOT 再插线 |
| `flash_verify_failed` | 回读 MD5 与镜像不一致 | 设备大概率还在下载模式，直接重试 |
| `capture_not_starting` | 开流后 3 s 块数 < 10（设备没开始发送/链路慢档）；或对 `LINK_SLOW` 的会话要音质报告 | 复位设备后重连再试；反复出现见 KI-005 |
| `busy` | 当前状态不允许该操作（连接中又点扫描、已连接又扫描） | 提示先停止正在进行的操作 |
| `internal` | 未预期错误 | 指向诊断日志 |

## 5. 界面

单窗口，默认 900×620，最小 780×520。不做多窗口、不做托盘常驻（见 §12）。

### 5.1 版面骨架：三条固定带 + 一个滚动区

```text
┌────────────────────────────────────────────────────────────────┐
│ ● 推流中  AI Passport Mic  电量 15%·请充电  -48 dBm  01:12      │ 42px 固定
├────────────────────────────────────────────────────────────────┤
│ 实时  音质报告  按键映射  诊断②  设备烧写                        │ 36px 固定
├────────────────────────────────────────────────────────────────┤
│ capture_not_starting  开流后 3 秒内只收到 2 个音频块  展开详情 ▾ │ 34px 固定
├────────────────────────────────────────────────────────────────┤
│                                                                │
│                    ← 唯一 flex:1 + overflow-y:auto →           │
│                     窗口再小也滚得动，不丢控件                  │
│                                                                │
├────────────────────────────────────────────────────────────────┤
│ 声卡[BlackHole 2ch▾] 缓冲──●──80ms □保存WAV          [ 停止 ]   │ 56px 固定
└────────────────────────────────────────────────────────────────┘
```

**这条结构是硬约束，不要退回"卡片按顺序堆叠"的写法。** 2026-11-07 之前
`.app` / `main` / 实时页三层都是 `overflow: visible`，`body` 又是 `overflow: hidden`，
默认窗口下推流态合计正好 592/592 零余量；一旦出现错误横幅（+60px）或链路慢档提示，
底部控制卡就被顶出视口，且**滚不到**——最小窗口 780×520 下溢出 193px。而那时
恰恰最需要点「开始/停止」。（实测表见 [`GUI_IMPLEMENTATION_LOG.md`](GUI_IMPLEMENTATION_LOG.md) §14.23）

所以：

- 顶栏 / Tabs / 错误条 / 底部操作条四个带一律 `flex: 0 0 auto`，内容区是唯一的
  `flex: 1 1 auto; overflow-y: auto`。任何新增区块先问"它进哪个带"。
- 页面容器（`.page`）用 `flex: 1 0 auto`——**可以长高，不允许被压缩**。
  写成 `1 1 auto` 会让 flex 在空间不足时压扁内部卡片，而卡片是 `overflow: visible`，
  内容会溢出边界盖到下一张卡上（2026-11-07 设备卡展开时的真实事故）。
  注意 `min-height: 0` 会抵消这条保护，页面容器上不要写它。
- 高度需要"填满剩余空间"的 canvas，父级必须 `position: relative` 且 canvas 用
  `position: absolute; inset: 0`。直接给 canvas `height: 100%` 会在父级高度未定时
  退化成 `auto`，转而采用自身 `height` 属性，与"按 clientHeight 回写属性"的绘制逻辑
  形成反馈循环，卡片会自己长高。
- **电平表是两层 canvas**（静态层 + 动态层，2026-11-07）：网格/刻度/10 秒曲线只在
  尺寸或数据变化时重画，每帧只画竖条与读数。**不要改回"离屏层 + 每帧 drawImage"**——
  1684×492 的位图按 60 fps 贴等于每秒搬运约 200 MB，实测总任务时间反而升高。
  见 `GUI_IMPLEMENTATION_LOG.md` §14.26。
- **「开始 / 停止」固定在底部操作条右端**，不跟任何卡片走。它与声卡、缓冲、保存 WAV
  同排，因为这四个是"每次开流都要碰"的东西，位置不该随页面内容变化。
- 新增固定带前先算账：默认 620 高、最小 520 高，每加一条固定带都会从内容区扣掉。

实时页内部（自上而下）：电平表（吃掉剩余高度）→ 指标三组（链路 / 声卡 / 延迟，
多出的 7 项收在「显示全部指标」里）→ 设备卡（默认折叠成 42px 一行，展开才显示
扫描列表与关麦兜底）。

**设备卡展开时指标组会自动降级成一行摘要**（丢块 / 欠载 / 缓冲 +「展开指标」）。
这是视图层的取舍：那一刻用户在换设备，把高度让给电平表比把它压到 168px 更好；
摘要仍保留三个判断依据，不是把数据藏起来。收起设备卡即恢复三组。

### 5.2 指标分三组，一组回答一个问题

| 组 | 回答什么 | 字段 |
| --- | --- | --- |
| 链路 | 有没有丢东西 | `blocks` / `lost_blocks` / `seq_jumps` / `device_dropped` |
| 声卡 | 声卡侧有没有出问题 | `underruns` / `underruns_underfed` / `silent_frames` / `lock_misses` / `overflow_drops` |
| 延迟 | 延迟到底多少 | `buffer_ms` / `fill_p50_ms` / `fill_p95_ms` / `ppm` |

**同一屏内同一个量只出现一次。** 之前画布里画了「缓冲 `buffer_ms`」、指标条里又有
「缓冲 `buffer_ms`」、顶栏有「存量 `fill_p50`」、指标条里还有「存量 p50/p95」——
四个数字长得像但口径不同，用户没法判断哪个是延迟。现在延迟结论只引「延迟」组。

### 5.3 视觉刻度（`:root` token，别引入半档）

| 类别 | 档位 | 用途 |
| --- | --- | --- |
| 字号 | 10 / 11 / 12 / 13 | 角标单位与卡内小说明 / 辅助说明与指标标签 / 正文与控件 / 卡片标题与大读数 |
| 间距 | 4 / 8 / 12 / 16 | 控件间 8，卡片内 12，**卡片之间 16**（`--gap-card`） |
| 圆角 | 6 / 10 / 999 | 控件 / 卡片 / pill |

电平表主读数（26px）在 canvas 里手绘，不参与 CSS 字号体系。
改之前有 10 / 10.5 / 11 / 11.5 / 12 / 12.5 / 13 / 13.5 八档字号混用。
新加样式一律引用 token；`grep -rn 'font-size: 1[0-3]\.5px' desktop/crates/gui/frontend/src`
应当返回空。

**卡片之间必须比卡片内松一档**：卡内 12、卡间 16。两者都取 12 时，
两张卡的边界读起来像同一块内容；卡间取 10 这类半档则最容易在各组件里漂移。

Tab（右上角切换，不引 vue-router，条件渲染即可）。顺序按使用频率排：
日常两个（实时 / 音质报告）在前，一次性操作（设备烧写）在最后。

| Tab | 内容 | 依赖阶段 |
| --- | --- | --- |
| 实时（已连过设备时默认） | 电平表 + 指标三组 + 折叠的设备卡（见 §5.1） | G2 |
| 音质报告 | 对会话 WAV 跑 `analyze`，**一张表**（指标 / 实测 / 判定 / 阈值同列）+ JSON 导出，明确区分"软件通过 / 真机听感待定" | G4 |
| 按键映射 | **按角色分三块**：麦克风（开麦键三选一 + 开麦方式占位）/ 快捷键映射（其余按键）/ 采集时注入（2 个钩子），加导入导出、恢复默认、辅助功能权限引导 | G5（M3） |
| 诊断 | 事件日志列表（时间 / 级别 / 来源）、复制诊断包、打开日志目录、**版本号与日志路径**。tab 上的角标是"未读 error 数"，**进页面即清零**（不是恒亮的错误指示灯）。**没有 `selftest` 入口**（`run_selftest` 未实现，见 §6） | G1 |
| 设备烧写（首次启动默认） | 随包固件版本与 app 分区 MD5、USB 设备列表、读版本 / 一键烧写与进度、结果与重试指引 | M6 |

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

命令（`invoke`）——全部幂等友好。返回 `Result<T, AppError>` 的命令会带错误码；
纯读取类命令（`snapshot` / `scan_stop` / `keymap_view` / `keymap_text` / `inject_permission` /
`diagnostics_text` / `log_path`）直接返回裸值，不包 `Result`。`AppError { code, detail }`：

| 命令 | 入参 | 返回 | 备注 |
| --- | --- | --- | --- |
| `snapshot` | — | `AppSnapshot`（裸值） | 前端启动时拉一次，之后靠事件 |
| `detect_adapter` | — | `()` 或 `AppError` | 首次启动与"重新检测"。**没有 `AdapterState` 类型**：适配器不可用走 `bt_unavailable` 错误码 + `AppState::NoAdapter` |
| `scan_start` | `window_ms?`（≤ 12000，默认 6000） | `()` | 到点自动停；列表靠 `devices` 事件推 |
| `scan_stop` | — | `()`（裸值） | 提前结束扫描（**不返回列表**，列表走事件） |
| `connect` | `device_id` | `()` | 进度靠 `state-changed` |
| `set_auto_connect` | `enabled` | `()` | 启动自动连接开关，立即落盘（§14.15） |
| `disconnect` | — | `()` | |
| `list_sinks` | — | `Vec<SinkView>` | `SinkView{ name, min_rate, max_rate, supports_16k }`（只带界面会判读的字段；UID/声道数在真正需要按 UID 匹配时再加） |
| `set_sink` | `name` | `()` | 存到配置，下次启动恢复。按设备名匹配（本项目只在 macOS 发布，同名设备暂不是问题） |
| `set_buffer_target` | `ms`（**40–80**，超出夹紧） | `()` | 流中也可改，取下一批生效。这是**用户基准**，自适应不会改它；上限 80 是延迟预算决定的，不是笔误 |
| `start_stream` | `save_wav: bool` | `String`（`session_id`） | 前置校验不满足 → `AppError` |
| `stop_stream` | — | `SessionSummary` | 含块数/丢块/欠载/时长/落盘路径 |
| `analyze_session` | `session_id` | `QualityReport` | 未测字段写 null，不猜 |
| `analyze_session_json` | `session_id` | `String` | 与 `apmctl analyze --json` 同一份报告函数生成，前端只贴不拼 |
| `reveal_path` | `path` | `()` | 在访达里选中该文件 |
| `keymap_view` | — | `KeymapView`（裸值） | 3 按键 + 2 钩子 + `ptt_event`（当前开麦键） |
| `keymap_save` | `bindings` | `KeymapView` | 落盘前校验；开麦键恰好一个，缺了兜底回 `ok`，重复整份拒绝 |
| `keymap_import` | `text` | `KeymapView` | 校验失败整份拒绝，不做部分生效 |
| `keymap_reset` | — | `KeymapView` | 恢复默认 |
| `keymap_text` | — | `String`（裸值） | 导出当前映射原文 |
| `inject_permission` | — | `bool`（裸值） | 查 `AXIsProcessTrusted()`，映射页横幅轮询它 |
| `open_accessibility_settings` | — | `()` | 一键跳到系统设置的辅助功能面板 |
| `set_save_wav` | `save` | `()` | 勾选状态持久化 |
| `set_auto_stop_min` | `minutes`（1–1440，夹紧） | `()` | 关麦兜底时长。立即落盘；看门狗每轮现读，**运行中改也生效** |
| `diagnostics_text` | — | `String`（裸值） | 生成一段可粘贴的纯文本（版本/设备/指标/最近日志） |
| `log_path` | — | `String`（裸值） | 诊断页显示日志文件路径 |
| `reveal_log` | — | `()` | 打开日志目录 |
| `firmware_status` | — | `FirmwareStatus` | 随包镜像（版本/大小/MD5）+ 候选串口。**不碰设备**，随便调 |
| `firmware_probe` | `{ port }` | `ProbeResult` | **会让设备复位**。返回当前版本与 `up_to_date` / `blank` / `mismatch` / `unknown` |
| `firmware_flash` | `{ port }` | `FlashReport` | 三段写 + `FlashMd5` 校验；校验不通过返回 `Err`（返回值里 `verified` 恒为 true） |

> **没有 `run_selftest`。** 早期方案里列过，但 `.app` 不带 `config/protocol-vectors.json`，
> 跑起来只会报"找不到向量文件"。协议自检仍归 `apmctl selftest`（见 `desktop/README.md`）。
> **命令名是 `diagnostics_text` 不是 `copy_diagnostics`**：后端只返回文本，写剪贴板在前端做。

事件（`emit`）：**主通道是 `app-event`**，载荷是 `AppEvent`，用 `type` 字段区分
（`kebab-case`）。前端 `listen('app-event')` 后按 `type` 分派。下表是 `AppEvent` 的变体：

| `type` | 频率 | 载荷 | 备注 |
| --- | --- | --- | --- |
| `state-changed` | 边沿 | `{ state }` | 错误详情在 `state: { kind: "error", code, detail }` 里，不另开字段 |
| `devices` | 扫描期间边沿 | `{ devices }` | **全量快照，不是增量**：每次推的都是当前完整列表（只有 id 集合或 RSSI 变化时才推）。前端直接替换，不要 merge 追加。载荷类型是 `DeviceView`，与 `apm_link::DeviceInfo` **形状相同但不是同一类型**（`state.rs` 有显式 `From`，前者只留界面要的字段） |
| `event-log` | 边沿 | `{ entry }` | `entry = { ts_ms, level, source, message }` |
| `telemetry` | 5 Hz | `{ telemetry }` | 字段见 `lib/types.ts` 的 `Telemetry`，比早期文档列的多（含 `ppm` / `glitch_events` / `overflow_drops` / `bad_len_blocks` 等） |
| `session-done` | 边沿 | `{ summary }` | `SessionSummary`。2026-11-06 起含 `link_slow`：为真时这一场不能进音质对比 |
| `battery` | 边沿 | `{ pct }` | `0..100` 或 `null`。`null` = 设备明确报不可用（无 Battery Service / 读超时 / SOC 无效），界面显示「电量不可用」，不显示 0%、也不隐藏 |
| `stream-aborted` | 边沿 | `{ code, detail }` | **内核主动中止推流**（不是用户点的停止）。目前唯一来源是"开流后 3 s 块数 < 10"（KI-005 护栏）。状态已回 `ready`，用户能直接重试；前端弹一次横幅，**不用 `error` 状态**——那会让「开始」按钮一直禁用 |

**例外：`firmware-progress` 是**第二个独立 Tauri 通道（不走 `app-event`）**，载荷是裸数字
`0..100`，按字节数折算，三段不是各占 33%。前端 `listen<number>('firmware-progress')`。

约定：`telemetry` **没有 `ts_ms` 字段**——它只有 `elapsed_ms`（从进入 `streaming` 起算的单调时长）。不要用 `SystemTime` 或前端 `Date.now()` 做释放计时，前端计时器在后台会被节流。

约定：**连接后的一次性结论不能只走事件。** 自动连接（`auto_connect`）会在前端注册监听之前
就完成连接并读电量，Tauri 不重放事件，届时顶栏会永远空白（KI-009）。所以电量同时进
`AppSnapshot.battery_pct` / `battery_known`，前端 `onMounted` 先按快照回填，
事件的职责只是"之后的变化"。新增同类字段时照此办理。

## 7. 指标口径（必须写死，不然界面上两个数字会对不上）

| 指标 | 定义 | 来源 |
| --- | --- | --- |
| `rms_dbfs` | `20·log10(rms/32768)`，下限夹到 -90 | 解码后、写 sink 前统计。**统计的是毛刺抑制之后**的样本（被压成 0 的样本按 0 参与 RMS） |
| `peak_dbfs` | 同窗口最大绝对值，界面保持 1.5 s 后按 12 dB/s 释放 | 同上 |
| `buffer_ms` | sink 内可用样本数 / 16000 × 1000 | sink 侧，**不是** channel 深度 |
| `underruns` | cpal 回调取不到足够样本的**次数**（一次回调里缺多少帧都只记 1 次） | sink 侧 |
| `underruns_underfed` | 其中**上游断供**的次数：读空时距上次 `push` 已 > 30 ms（`apm_sink::UNDERFED_MS`） | sink 侧。`underruns − underruns_underfed` 才是"消费端跑太快"，只有这部分能靠抬缓冲改善 |
| `fill_p50_ms` / `fill_p95_ms` | **实测存量**分位数（整场累计，4 ms 桶） | sink 侧。**延迟结论只能引这一组**，不能用 `target_ms` |
| `silent_frames` | 欠载时补进去的**帧数**（不是次数，也不并入 `underruns`） | sink 侧。CLI 拿它折算毫秒 |
| `lock_misses` | cpal 回调 `try_lock` 抢不到锁、整块输出静音的**次数** | sink 侧。与 `silent_frames` 分开：那是"缓冲没数据"，这是"锁被占着" |
| `ppm` | 设备与声卡的时钟差，**墙钟法**：`(推入帧数 / (16000 × 实际秒数) − 1) × 1e6` | sink 侧。**不是** `(丢−补)/消费`——那个式子会把自己的补偿动作当成漂移（见 `ARCHITECTURE.md` 的可靠性约束） |
| `overflow_drops` | channel 满时丢掉的块数（丢的是**最新**一块，不是最旧） | app 层 |
| `lost_blocks` | 序号跳变推算出的缺失块数。**不含**设备 STATUS 上报的丢弃（那是 `device_dropped`） | app 层 |
| `device_dropped` | 设备 STATUS 上报的发送阶段丢弃 | app 层，与 `lost_blocks` 分列显示 |
| `glitch_events` | 命中毛刺抑制的**次数**（`apm_protocol::glitch` 报了几次事件）；被压掉的**样本数**只在会话收尾的 `glitch_muted_ms` 里 | app 层 |
| `elapsed_ms` | 从 `streaming` 进入时刻起的单调时长 | app 层。**这是唯一的时基**——`Telemetry` 没有 `ts_ms` 字段 |
| `blocks` | 本场收到的音频块数（含后来被判为坏长度的块） | app 层 |
| `base_target_ms` | **用户选的**缓冲基准（滑块上的数）。自适应不改它 | sink 侧 |
| `boost_ms` | 自适应**临时**加上的余量。非 0 说明最近发生过欠载 | sink 侧 |
| `target_ms` | 当前**实际生效**目标 = `base + boost`（被 40–80 夹住）。**不要**把它当延迟 | sink 侧 |
| `seq_jumps` | 发生序号跳变的**次数**（一次跳变可能丢多块，与 `lost_blocks` 不同） | app 层 |
| `bad_len_blocks` | 长度 ≠ 165 的块数，通常意味着 MTU 不足被截断 | app 层 |
| `bytes_written` | 已写入声卡的字节数 | sink 侧 |
| `link_slow` | 设备报过 `LINK_SLOW`（连接间隔 > 40 ms）→ 本场欠载/漂移是链路假象，不能当音质结论 | app 层，见到即整场粘住 |

**只在会话收尾可见的字段**（`SessionSummary`，不在实时 `telemetry` 里）：

| 指标 | 定义 | 来源 |
| --- | --- | --- |
| `frames_dropped` | 缓冲真·超容量丢弃的帧数（"延迟失控"） | sink 侧 |
| `frames_trimmed` | 延迟守卫一次性修剪掉的旧帧数（回调停摆积压，"主动降延迟"） | sink 侧。与 `frames_dropped` 必须分开 |
| `frames_inserted` / `frames_skipped` | 漂移补偿插值 / 插删的帧数 | sink 侧 |
| `target_start_ms` / `target_end_ms` | 会话首尾**用户选的**缓冲基准（运行中改滑块就会变），看用户意图有没有变 | sink 侧 |
| `target_boost_max_ms` | 会话期间自适应**最多**临时加过多少余量。0 = 全程没被抬过 | sink 侧 |
| `buffer_p50_ms` / `buffer_p95_ms` | 实测存量分位数。**报告里的"抖动缓冲延迟"只能用它** | sink 侧 |
| `underruns_underfed` | 其中上游断供的次数（见上表） | sink 侧 |
| `glitch_muted_ms` | 被毛刺抑制压成静音的样本折算成的毫秒数（实时 `glitch_events` 只给次数） | app 层 |

「设备侧丢弃」和「序号跳变」必须分开显示——CLI 已经踩过这个坑：只看序号跳变会得到
"0 丢块"的乐观结论，而设备在发送阶段丢的块序号根本没出来。

### 7.1 块到达间隔（KI-006 的诊断口径）

**平均块率 50/s 不等于"喂得均匀"。** 2026-11-07 的真机数据：块率精确 50.000/s、序号跳变 0、
设备丢弃 0，**但仍有 14 次欠载**——原因是块**到达的时刻不均匀**：最大间隔 120–180 ms，
超过 80 ms 缓冲上限的情况各轮 100 次以上。

`Pipeline` 因此记录每两个音频块的到达间隔，桶边界见 `ARRIVAL_BUCKETS_MS`：

| 桶 | 含义 |
| --- | --- |
| 0–30 ms | 正常（20 ms 块 + 少量调度余量） |
| 30–50 / 50–60 / 60–70 ms | 轻微抖动，存量能吸收 |
| **80–100 ms** | 超过缓冲上限：存量正好在目标时会欠载 |
| **100–200 ms+** | 长尖峰：几乎必然欠载 |

`apmctl sink` 收尾会打印这个直方图与判定。**注意「迟到次数 ≠ 欠载次数」**：
存量接近目标时，80–100 ms 的间隔多数还能被吸收（实测 100 次迟到只造成 14 次欠载）。
不要用两者的比值判断对错。

`apmctl sink --tone` 是这条的**对照组**：不连设备、按 `(i+1) × 20 ms` 精确配速推正弦，
间隔恒定（最大 30 ms）。它跑出 **0 运行期欠载**，说明声卡侧本身是干净的。
⚠️ `--tone` 的统计必须在**收尾等待之前**取——喂入一停，缓冲里剩的 80 ms 必然被抽干，
那是"我们不再喂了"，不是欠载。

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
      src/main.rs            # tauri::Builder + 30 个 command 的薄封装
      frontend/              # Vue 3 + Vite + TS（package.json 在此）
        src/{App.vue,components/,lib/}   # 没有 views/：tab 用条件渲染，不引 vue-router
```

GUI 必须放在同一个 workspace 里，否则 `link` / `sink` 的依赖会走出两份 lockfile，
版本一漂移就是"CLI 能跑 GUI 不能跑"。

## 10. 分阶段与验收

每个阶段单独可验收，交付时**分项报状态**，不许压成一句"GUI 好了"。

| 阶段 | 内容 | 验收（必须实测） | 当前 |
| --- | --- | --- | --- |
| **G0 骨架 + 权限** | Tauri 空窗口；`detect_adapter` + `scan_start` 打通 | 双击 `.app` 弹出蓝牙授权；6 s 内列出 `AI Passport Mic`，RSSI 与 `apmctl scan` 相差 ≤ 6 dBm | **PASS（2026-11-05 真机）**：授权弹窗正常、6 s 扫到 7 个设备、RSSI 两边都是 −50 dBm。明细见 §14.7 |
| **G1 连接 + 状态机 + 日志** | connect/subscribe/disconnect、错误码映射、诊断 tab | 连上后 `state=ready`；拔设备电源 → **≤ 8 s** 内进 `idle` 且日志有记录；关系统蓝牙 → `no_adapter` | **PASS（2026-11-07 真机补齐）**：`ready`、`ready → stopping → idle`（拔电源 7.64 s）、**关蓝牙 → `no_adapter`** 三项都验过了。关蓝牙这项第一次测时**落在 `idle`（FAIL）**，查出两个真 bug，见 §14.20。**口径已于 2026-11-05 由 2 s 调整为 8 s**，理由见 §14.7 |
| **G2 实时电平 + 推流** | 电平表、指标条、start/stop、缓冲滑块 | 连续 10 min：**欠载阈值型**（频率 ≤ 5 次/分 · 单次 ≤ 50 ms · 累计静音占比 ≤ 1%）、块率 50/s ±0.5%、跳变/设备丢弃 = 0；前端 ≥ 30 fps；CPU < 8%（M 系）、内存 < 150 MB | **欠载口径 2026-11-07 改为阈值型**（原"0 欠载"，KI-006 根因=CoreBluetooth 攒批交付）。**性能三项 2026-11-07 release 包实测**（`./scripts/test-gui-perf.sh`）：**CPU 14.2% → FAIL**、**内存 208 MB → FAIL**、**前端 59.8 fps（最低 58.8）→ PASS**。欠载项：CLI 侧 600 s 实测 1.0 次/分 → PASS；**GUI 侧未按新口径重跑** |
| **G3 声卡选择 + 会话 WAV** | 枚举/切换声卡、落盘、收尾顺序 | **同条件先后各录一段**（不是同时录，理由见下）；**块率差 ≤ 0.5%**、丢块一致、设备侧丢弃一致；会话 WAV 能被 `analyze` 直接吃 | **PASS（2026-11-07 真机）**：GUI 2292 块 / 45.84 s、CLI 2299 块 / 45.98 s，**两边块率都是 50.000 /s（差 0.000%）**、丢块与设备丢弃都是 0、采样率误差都是 0。明细见 §14.21 |

> **口径修订（2026-11-07，实测后改）：判据从「块数差 ≤ 1」改为「块率差 ≤ 0.5%」。**
> 原判据隐含"两次录制时长完全相同"这个前提，但两次独立录制**必然**有时长差。
> 实测：两边块率完全一致（50.000 /s），块数却差 7 —— 因为 GUI 录了 45.84 s、
> CLI 录了 45.98 s。按 CLI 块率折算 GUI 应有 2290 块，实测 2292，只差 2。
> 用块数判会让一个正确的实现在正确的时候被判失败；块率才是"统计口径是否一致"的判据。

> **口径修订（2026-11-06，项目负责人确认）：G3 由「同时录」改为「先后各录」。**
> 原文写"GUI 与 `apmctl record` 同时录 15 s"，但设备只支持一条 BLE 连接
> （`device/sdkconfig.defaults` 的 `CONFIG_BT_NIMBLE_MAX_CONNECTIONS=1`），
> 两个客户端同时连是物理上做不到的；硬跑只会得到 §14.6 描述的"块率 1.5/s"脏数据。
> 改后的口径仍能发现"两个客户端统计口径不一致"这类真问题，只是不再验并发。
| **G3b 回环自动验收** | 用 GUI 自身身份申请麦克风，恢复 `probe` / `--verify` | 推 440 Hz 正弦 → 回读 RMS > -30 dBFS；给出稳态延迟数值（这是延迟预算第一次可自动测） | **未实现**：`apmctl sink --verify` 仍在 CLI，GUI 侧没有回环入口 |
| **G4 音质报告** | analyze 视图 + JSON 导出 | 报告字段与 `apmctl analyze --json` 逐字段一致；未测字段为 null | 代码完成：报告构造只有一份（`apm_app::report`），GUI 的 JSON 由后端同一函数生成；采样率提醒（`rate_note`）与下混标记已与 `apmctl` 对齐（2026-11-05） |
| **G5 按键映射** | **按角色分三块**的编辑器（麦克风 / 快捷键映射 / 采集时注入），`keymap-4` 的导入导出与恢复默认 | 改成非默认键位 → 设备按键触发正确；开麦键换到任意键后 PTT 开关路径仍可用 | 编辑/校验/落盘完成；开麦键是**恰好一个**绑 `ptt-toggle` 的键（默认 `ok`，可换 `up`/`down`），缺了兜底回 `ok`、重复整份拒绝。**注入层（M3）已实现并在真机验过**（§14.13 钩子 + §14.14 设备按键直连 + §14.16 端到端注入 `a`/`b` PASS），"按键触发正确"这条已闭环。**唯一缺口是「按住不放 (hold)」**（固件侧 PRESS/RELEASE 齐，缺 PC 端消费；界面上已置灰占位）。事件表已于 2026-11-05 两次归并：15 → 6（§14.11）→ 3（§14.12），随后加钩子升到 v4 |
| **G6 打包** | 签名、公证、版本号、崩溃日志脱敏 | 干净机器上从 dmg 装 → 首次启动授权 → 完成 G1~G3 验收 | **未做**：需要 Apple 开发者证书；当前只有 ad-hoc 签名 |
| **M6 设备烧写页**（不在 G 系列，归 ROADMAP 的 M6） | 随包镜像信息、USB 设备枚举、读版本、分段写入、进度与结果 | 空片插上 → 一键写完 → 校验通过 → 能连上 BLE；重新烧写后配对与映射保留 | **真机已通（2026-11-06，`apmctl firmware probe/flash` 同一条内核路径）**：设备 `7552506-dirty` → `0.1.0`，三段写入 4.9 s、`FlashMd5` 一致、烧完自行复位回应用模式并恢复广播；烧写后配对仍在（NVS 未动）。**空片首烧未验**。约束见 §5 的「设备烧写」三条 |

依赖关系：G0 → G1 → G2 → G3 串行；G3b、G4、G5 可并行；G6 等 G5。
编排迁移（§12 第 1 条）**主线已完成**：`detect_adapter` / `scan` / `connect` / `start_stream` /
`stop_stream` / 采集解码写声卡全在 `apm-app`（`core` / `session` / `pipeline`），
`gui` 只调它。`cli` 的 `scan` / `record` / `sink` / `ping` / `status` 仍直接调 `apm_link`
（待重构，见 §12 第 1 条与 §14.9 的补充说明）。

## 11. 风险与已知坑

| 风险 | 影响 | 应对 |
| --- | --- | --- |
| Tauri 2 + 现有 `rust-toolchain.toml`（1.95.0） | 编译失败 | Tauri 2 要求 1.77+，1.95 满足；只是首次 `cargo build` 会拉 300+ crate，冷编译 5–8 min |
| cpal 设备热插拔 | 装完 BlackHole 后 `coreaudiod` 未加载，枚举不到 | 界面提供"重新枚举"按钮；文档写清 `killall coreaudiod` |
| webview 后台节流 | 窗口最小化后 `telemetry` 堆积 | 事件用 `emit` 且前端做"只保留最后一帧"合并；不要用 `setInterval` 驱动渲染 |
| WAV 落盘抖动 | 磁盘 IO 影响音频路径 | 独立线程 + `BufWriter` 行缓冲；磁盘慢时降级为"只报错，不阻塞音频" |
| 偶发满量程毛刺（未定位） | 界面上会看到 -0.0 dBFS 尖峰 | GUI 不用尖峰触发任何自动动作；诊断包里带原始块序号，便于下次定位 |
| 仪表读数与 CLI 不一致 | 用户不信数据 | §7 口径写死，G3 用"先后各录一轮"做对拍（设备 `MAX_CONNECTIONS=1`，两个客户端同时录在物理上做不到；判据是块率差 ≤ 0.5%，见 §14.21） |

## 12. 已确认决策（2026-11-04 定稿）

以下 5 条已由项目负责人确认，不再是待定项。改这些决定要同步改本章并说明理由。

| # | 问题 | 结论 | 对实现的直接后果 |
| --- | --- | --- | --- |
| 1 | CLI 在 GUI 之后怎么定位 | **保留为诊断通道**（GUI 主入口，`apmctl` 继续用于脚本化与免 UI 取证） | `apm-app` 必须**不依赖任何 GUI 类型**（不引 tauri、不含窗口/事件循环概念）；`scan/record/sink` 的编排迁进 `apm-app`，`cli` 只做参数解析与输出格式化；`scripts/` 里依赖 `apmctl` 的验证脚本不用重写 |
| 2 | 首发平台 | **只做 macOS** | G0 只做 `.app` / dmg；声卡枚举只走 CoreAudio，不写 WASAPI 分支；VB-CABLE / NSIS 留到首发后单独一轮；跨平台抽象点到为止，不提前造 |
| 3 | 会话 WAV 是否默认落盘 | **默认关，用户勾选才录** | `start_stream{save_wav:false}` 为默认；界面勾选框状态要持久化；隐私口径写进 README；未勾选时磁盘 IO 完全不参与音频路径 |
| 4 | 按键映射编辑 | **进首发（G5），但不作为首发门槛** | G0 的 tab 路由要给第 4 个 tab 留位置（先做只读展示当前映射）；G0–G3 不依赖 M3 的注入层；G5 未完成不算首发阻塞项，但要在交付里如实标为 SKIP |
| 5 | 托盘常驻 / 菜单栏图标 | **不做** | 关窗即退出；不做"窗口关闭但进程常驻"的状态分支，省掉一整套生命周期复杂度。后续要加再说 |

第 1 条是这轮最容易被写歪的地方：迁移前 `cli/src/cmd.rs` 直接调 `link` + `sink` 做编排。
**主线已迁完**（`detect_adapter` / `connect` / `start_stream` / `stop_stream` / 采集解码写声卡
全在 `apm-app`），但 `record` / `sink` / `ping` / `status` / `scan` 这几条 CLI 路径**仍然自己
调 `apm_link`**（`scan` / `connect` / `start_capture` / `stop_capture` / `ping`），共用的只有
`apm_app::Pipeline` 与 `apm_app::report`。后果不是纯文档问题：§14.9 那个"开流前 STOP"
就得在两处各改一遍。要真正统一，`apm-app` 需要一个覆盖"不落 WAV / 不枚举声卡 / 自定义时长"
的会话入口，`record` 与 `sink` 都改成调它——**这属于重构，另开一轮**，详见 §14.9 的补充说明。

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

## 14. 实施记录

按计划落地的部分不重复，**与方案不一致的地方和踩到的坑**按时间追加在
[`GUI_IMPLEMENTATION_LOG.md`](GUI_IMPLEMENTATION_LOG.md)。

拆出去的理由：这份日志有 900 行、21 个小节，全是"当时发生了什么"。放在设计文档里
会让"当前契约是什么"和"某天试过什么"混在同一个文件里，读者要读到最后才发现某段
早已被后面的段落推翻。设计留在本文件，过程放那边。
