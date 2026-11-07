# GUI 实施记录（G0–G5）

> 从 [`GUI.md`](GUI.md) §14 拆出来的按时间追加的实施日志。
> 这里记的是**当时**发生了什么、踩了什么坑、怎么修的；
> 设计决策与当前契约看 [`GUI.md`](GUI.md)，未解决问题看 [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md)。
> **越早的段落越可能与现状冲突**——它们是快照，不是现状描述。

---

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
| 5 | `SinkInfo.uid` 已改用 `Device::id()`（**2026-11-06 起**） | cpal 0.18 提供 `Device::id()`（macOS 上是 CoreAudio UID 字符串），取不到时才回退设备名。同名设备（两块同型号 USB 声卡）以前会被认成一个，现在能区分；`set_sink` 按 UID 优先匹配。字段名保留是为了不动前端契约 |
| 6 | 事件是 **15** 个，不是文档早先写的 12 个（**已被 §14.11 取代：现在 6 个**） | schema 是 3 个按键 × 5 种手势（短按/双击/长按/按下/抬起），`config/README.md` 才是权威 |
| 7 | 新增错误码 `keymap_invalid` | 映射校验失败是真实分支，塞进 `internal` 会让用户查到"未预期错误" |
| 8 | 新增命令 `set_save_wav` / `analyze_session_json` / `reveal_path` | 勾选状态要持久化；"复制 JSON"必须由后端同一份报告函数生成，否则和 `apmctl analyze --json` 迟早不一致；访达定位需要路径 |
| 9 | **未实现** `run_selftest` | `.app` 里没有 `config/protocol-vectors.json`，跑也是"找不到向量文件"。协议自检仍由 `apmctl selftest` 承担 |
| 10 | 电平表：后端 5 Hz + 前端 rAF 插值 | 5 Hz 数据直接画会一跳一跳。前端只做动画（上升跟手、下降 24 dB/s，峰值 1.5 s 保持后 12 dB/s 释放），不自己算丢块率 |
| 11 | 会话 WAV 增量写：先占 44 字节头，收尾回填长度 | 一次会话可能几十分钟（16 kHz × 2 B/s ≈ 32 KB/s），攒内存没意义 |
| 12 | 断连判定 = 5 s 静默 + `is_connected` 探活；断链后再探一次适配器 | 拔电源不会产生任何通知，"什么都没有"才是信号；探适配器才能区分"设备没了"（`idle`）和"蓝牙被关了"（`no_adapter`） |
| 13 | `CTRL START` 失败时状态回到 `ready`，不是 `error` | 设备还连着，落到 `error` 会让"再点一次开始"被状态机挡住 |
| 14 | 会话 WAV 命名 `s<epoch_ms>.wav`，落在 `~/Library/Application Support/com.cnryb.ai-passport-mic/sessions/` | 与配置、按键映射同一个目录，卸载时一起清 |

**当时仍然没做**（本节是 2026-11-05 首轮快照；后续进展见 §14.13 / §14.14 / §14.15 与 `GUI.md` §10 的当前列）：

- **G3b 回环自动验收**：`--verify` 还在 CLI，GUI 没有回环入口，麦克风权限没在 App 身份下验过。**仍未做。**
- **G6 签名公证**：只有 ad-hoc 签名，没有 Apple 开发者证书。**仍未做。**
- **M3 按键注入**：当时映射表只能编辑与校验，**不会真的敲键盘**。
  **已实现（§14.13 采集钩子 + §14.14 设备按键直连）**，只剩真机端到端验收。
- **真机验收**：G1–G5 的每一条实测都需要设备在手 + 人在弹窗点「允许」。
  本轮当时只验证到"内核启动、枚举到 BlackHole 2ch 并写进配置、无适配器时正确落到 `no_adapter`"；
  随后 §14.7 补了 G0 全 PASS 与 G1 部分 PASS（断连口径同日由 2 s 调整为 **8 s**），
  §14.10 补了 G2 的 10 min 实测（欠载 FAIL）。

### 14.2 本轮跑过的门禁（2026-11-05）

| 检查 | 命令 | 结果 |
| --- | --- | --- |
| 基线自检 | `./scripts/verify.sh` | **PASS**（Structure / Docs links / Scripts / Protocol 44 passed / Vectors / Desktop / Env 全绿） |
| 桌面端单测 | `cd desktop && cargo test` | **PASS**（本节为 2026-11-05 快照：93 passed / 1 ignored）。**2026-11-07 复核（删诊断仪表盘之后）**：**143 passed / 0 failed / 2 ignored**——`apm-app` 82（+2 ignored）、`apm-protocol` 37、`c-vectors` 5、`glitch_repro` 3、`apm-link` 5、`apm-sink` 7、`apmctl` 4。以 `desktop/README.md` 的验证状态表为准 |
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
| `cpal::Stream` 是 `!Send` | **已消掉**（2026-11-06）：cpal 升到 **0.18.2**，`cpal::Stream` 现在满足 `Send`，`sink/src/lib.rs` 有编译期断言 `assert_send::<cpal::Stream>()`。设计仍保留"声卡独占线程"（实时线程不该被别处 await 打断），但它已从"没得选"变成"有得选、选择保留"。见 [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) KI-004（已关闭） | 升级三项判据全过：`test-sink.sh` 前后一致、`cargo test` 全绿、真机欠载 39→24（更高负载下）。`sink/Cargo.toml` 已钉 `cpal = "0.18"` |
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
| 关系统蓝牙 → `no_adapter` | 必须 | **2026-11-07 凌晨 PASS**（第一次 FAIL 落到 `idle`，修完两个 bug 才成立），见 §14.20 | PASS |

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

**顺带发现（文档与实现不一致，2026-11-07 已把文档改成如实描述，代码仍未统一）**：`AGENTS.md` 写的"编排只有一份——
`connect` / `start_stream` / `stop_stream` 全在 `apm-app`，`cli` 只解析参数与打印"
**对 `record` / `sink` / `ping` / `status` / `scan` 不成立**。`crates/cli/src/cmd.rs` 仍然自己调
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

#### 2026-11-07 复测：性能三项终于全部"可测"，且 CPU/内存确实超标

§14.10 那次是**人盯活动监视器抄数字**，抄完就复现不了，而且口径（主进程 vs 全家桶）当场
就有分歧。这次把测量做成脚本：`./scripts/test-gui-perf.sh`。

**release 包实测（90 s 采样 × 84 次，推流稳态）**：

| 项 | 实测 | 口径 | 结果 |
| --- | --- | --- | --- |
| CPU 均值 | **14.2%** | < 8% | **FAIL** |
| 内存峰值 | **208 MB** | < 150 MB | **FAIL** |
| 前端帧率 | **59.8**（最低 58.8 / 最高 60.0，6 个窗口） | ≥ 30 | **PASS** |

分进程：

| 进程 | 内存 |
| --- | --- |
| apm-gui（主进程） | 86 MB |
| WebKit GPU helper | 53 MB |
| WebKit WebContent | 70 MB |
| WebKit Networking | 15 MB |

**关键结论：大头不在主进程，在 WebKit 的 WebContent + GPU helper。** 主进程只占 41%，
只测主进程会得出"内存 86 MB、勉强合格"的错误结论（§14.10 就是 26 MB vs 166.9 MB 的分歧）。

**前端帧率终于有数据了**。之前一直写"未测（没有采集手段）"，因为 WebView 里没有可无人值守
读取的帧计数口。现在在 `LevelMeter` 的**真实渲染循环**里数帧、每 5 s 通过 `report_fps`
命令写进诊断日志（`source=fps`），脚本读日志汇总。**59.8 fps 说明渲染侧完全不掉帧**，
CPU 超标不是渲染卡顿，而是渲染**太勤**。

**测量过程中踩的四个坑（都写进脚本注释了，别重犯）**：

1. **按进程名匹配会漏掉 CPU 大头**。WebKit helper 的 `comm` 是
   `/System/Library/.../com.apple.WebKit.GPU.xpc/...`，**完全不含应用名**。
   第一版据此量出"CPU 3.4%、内存 79 MB"——看起来很好，实际只算了主进程。
2. **按 PID 或父子链推断归属会算错**。那些 helper 的 `ppid` 是 1（launchd 拉起），
   `lsof` 也看不到它们打开应用 bundle 的文件。按"PID >= 主进程"当代理更糟：
   会把同时开着的 Cursor 自己的 WebKit 也算进来。
   最终用**启动前后的 PID 差值**锁定。
3. **差值会夹带系统守护进程**。应用启动会顺带让 macOS 按需拉起
   `replayd`(48 MB)、`triald`、`ospredictiond`、`intelligenceflowd`、`osanalyticshelper`。
   不按 comm 白名单过滤，内存会虚高 80+ MB。
4. **只认本次启动后的日志行**。用 `tail -40` 找 `PERF_STREAMING` 会命中上一轮的记录，
   于是脚本以为已开流、立刻开始采样，实际应用还在启动——采到的是残留 PID。

**还加了一个无人值守入口** `--smoke-perf[:秒]`：连设备 → 开流 → 保持 N 秒。
此前 GUI 只有 `scan` / `firmware` 两个冒烟入口，性能测量只能靠人点「开始」，
不可复现。

**结论：G2 的性能项确实没过，不是"测不出来"。** 首要优化点是 §14.10 指出的那条：
**电平表 60 fps 全速渲染，无论窗口是否可见**。帧率 59.8 说明它跑满了 ——
待机/不可见时降到 5 fps 甚至暂停，应能同时压 CPU 与 WebKit 内存。

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
钩子禁 `ptt-toggle`。**真机端到端已于 2026-11-06 验过，见 §14.16。**

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
自动重连（运行中掉链后自己重连）**未做**，仍要手动扫一次。**真机已于 2026-11-06 验过，见 §14.16。**

### 14.16 M3 注入 + KI-009 + 自动连接真机验收（2026-11-06，人在场）

设备 MAC `98:c3:77:f6:c6:44`，release 包（`build-gui.sh --release`，`.app` 12.23 MiB，
随包固件 720448 字节）。**验收前先 `tccutil reset Accessibility com.cnryb.ai-passport-mic`**，
确保测的是"从未授权"的首次路径。

#### 自动连接 + KI-009 电量：**PASS**

`last_device_id` 预置为扫描到的 `aa9f293c-…`（首次启动没有记录，`auto_connect()` 会静默跳过——
这是设计行为，不是 bug）。启动后日志：

```text
21:48:34.218 应用内核启动 v0.1.0
21:48:34.444 检测蓝牙适配器
21:48:34.473 自动连接上次设备：AI Passport Mic
21:48:34.473 状态 idle → connecting
21:48:34.473 连接 aa9f293c-e765-f2c5-6c02-fad0d5f291bc
21:48:35.946 电量 95%                                    ← 与下一行同毫秒
21:48:35.946 状态 connecting → ready
21:48:35.946 已连接并订阅 status/audio/event
21:48:36.127 state=1 flags=0x03 sent=30001 dropped=0     ← 心跳正常
```

**连接耗时 1.47 s**，电量结论与 `ready` 同一毫秒落地。KI-009 的修法（`AppSnapshot.battery_pct`
+ `battery_known` 先按快照回填）成立：自动连接场景下前端拿到快照时电量已在里面，
**不再依赖 `Battery` 事件在监听注册前是否发出**。

> 口径提醒：本条只证明「后端在连接时就算出了电量并进了快照」，顶栏文字由前端按快照渲染
> （`App.vue` 的 `batteryKnown`）。本轮无屏幕录制权限，未截图核对像素。

#### M3 注入端到端：**PASS**

临时把映射改成 `up→a`、`down→b`（默认 `up`/`down` 不绑定动作，验不出注入），
目标应用 TextEdit，设备按键实按：

| 阶段 | 日志 | 结论 |
| --- | --- | --- |
| 未授权时按 UP（3 次） | `按键 up 注入失败：没有"辅助功能"权限：系统设置 → 隐私与安全性 → 辅助功能…` | 护栏生效：**明确报错而不是静默丢弃**，文案与文档一致 |
| 授权后按 UP | `按键 up 已注入 a` | **PASS** |
| 按 DOWN | `按键 down 已注入 b` | **PASS** |
| 按 OK（开） | `ready → streaming`、`打开输出 BlackHole 2ch（目标缓冲 80 ms）`、`PTT 开麦完成` | **PASS** |
| 按 OK（关） | `streaming → stopping`、`手动停止：303 块 / 6.1 s，欠载 0，丢块 0（设备侧 0）` | **PASS** |

第二次 PTT 周期：`116 块 / 2.3 s，欠载 0，丢块 0（设备侧 0）`，同样干净。

**顺带观测（不算缺陷，但要记）**：每次 `PTT 开麦完成` 前有 6–8 条
`收到音频块 165 字节但当前未推流`。这是 OK 按下 → `start_stream` 完成之间设备已在发
（约 100 ms 窗口），属 KI-005「连接→START 间隔」的良性残留；本轮**没有出现**长时间的
「连上就自己在发」，断链清态那处固件修复仍然有效。

#### 按键手势覆盖度（如实声明）

本轮实际只按到 **UP / DOWN / OK 各自的 PRESS / CLICK / RELEASE**：

| 事件 | 计数 |
| --- | --- |
| `button=0`（UP）PRESS / CLICK / RELEASE | 4 / 4 / 4 |
| `button=1`（DOWN）PRESS / CLICK / RELEASE | 1 / 1 / 1 |
| `button=2`（OK）PRESS / CLICK / RELEASE | 4 / 4 / 4 |

**DOUBLE（双击）与 LONG（长按）这一轮一次都没产生**，所以「手势核对」在本轮只到 9/15——
按计数应覆盖 3 键 × 5 手势 = 15 项（文档早先写 12 项是漏了 RELEASE，见 §14.11 第 6 条）。
**不要因为本节 PASS 就认为手势已全部核对过**；DOUBLE / LONG 在 §14.17 单独补测。

### 14.17 手势 15 项补测：DOUBLE 与 LONG（2026-11-06，人在场）

§14.16 只覆盖 press / click / release，本节补上双击与长按。工具换成 `apmctl status`
（它把按键事件直接打印成 `按键: up.double` 这种可读形式，比读 GUI jsonl 更省事）。

**固件的两条判据**（`device/main/buttons.c`，实测与代码一致）：

| 手势 | 判据 | 代码位置 |
| --- | --- | --- |
| DOUBLE | 同一个键松开后 **≤ 180 ms**（`APM_BTN_DOUBLE_MS`）再次按下 | `on_press` 的 `s_double_pending` |
| LONG | 按住 **≥ 500 ms**（`APM_BTN_LONG_MS`）且尚未上报过 | `button_task` 的 `s_long_fired` |

**实测输出（原样，24 条事件）**：

```text
— 双击组：每键 press → release → press → release → double —
按键: up.press    → up.release    → up.press    → up.release    → up.double
按键: down.press  → down.release  → down.press  → down.release  → down.double
按键: ok.press    → ok.release    → ok.press    → ok.release    → ok.double
— 长按组：每键 press → long → release —
按键: up.press    → up.long    → up.release
按键: down.press  → down.long  → down.release
按键: ok.press    → ok.long    → ok.release
```

**两条行为核对（都对）**：

1. **双击 / 长按都不补发 `CLICK`。** 24 条事件里 `click` 出现 0 次。这是设计：
   `on_release` 在 `s_double_pending` 与 `s_long_fired` 两个分支都提前 `return`，
   只在"松手后 180 ms 内没有第二次按下"时才走末尾的 `emit(CLICK)`。
   所以 **`click` 只能由单击产生** —— 它已在 §14.16 三键验过，两轮合起来正好 15/15。
2. **`ok.long` 期间的 PTT 语义是"按下即开、松开即关"。** 全程 `state` 与 `flags` 同步：

   | 时刻 | 事件 | `flags` | 位含义 | 状态 |
   | --- | --- | --- | --- | --- |
   | 长按开始 | `ok.press` | `0x07` | BLE `0x01` + AUDIO_SUB `0x02` + **PTT_HELD `0x04`** | `capturing`，`ptt=是` |
   | 按住中 | `ok.long` | `0x07` | 同上（PTT 仍按住） | `capturing`，块数 38 → 88 → 137 持续增长 |
   | 松手 | `ok.release` | `0x03` | **PTT 位已清** | 回 `ready`，`ptt=否`，`dropped=0` |

   注意：GUI 侧的 OK 映射是 `ptt-toggle`（按一下开、再按一下关，见 §14.12），
   而**固件仍然在长按期间把 `PTT_HELD` 置位并在松开时清掉**——两者语义不同但不冲突：
   PC 只消费 `EVENT_PRESS`，`long`/`release` 仅记日志。这也解释了 TODO 里那条
   **「按住不放 (hold)」功能洞**为什么固件侧"已有 PRESS/RELEASE 与 `s_ptt_held`"，
   缺的只是 PC 端消费 `RELEASE` 和一个 hold 映射项。

**电压窗口仍未用万用表复核**（`board.h` 的 150 / 447 / 1900 mV）：本节证明的是
**分类结果正确**，不是窗口边界正确。换板重标时仍要按 `apm_buttons_read_mv()` 量一遍。

### 14.18 KI-005 的 PC 侧护栏（2026-11-06，代码完成）

KI-005 的根因（连接间隔跑偏）在固件侧已修，但**"别把脏数据当真"是 PC 侧的责任**，
此前一直没做。本次补两条：

#### 护栏一：开流后 3 s 块数 < 10 → 早停

| 层 | 改动 |
| --- | --- |
| `apm-app` 常量 | `EARLY_STOP_WINDOW = 3 s`、`EARLY_STOP_MIN_BLOCKS = 10`。正常链路 3 s 约 150 块，10 是极宽松下限——只拦"几乎没数据"，不拦"稍慢"（那是 KI-006 欠载的范畴） |
| `Pipeline::unstarted(window, min_blocks)` | 判据是**两个条件同时成立**。只看块数不行：刚开流 10 ms 时块数当然是 0 |
| `AppCore::spawn_early_stop` | 开流后派生一次性任务，过窗口判一次就退出。判完若会话已结束（用户手动停/断链）则什么都不做 |
| 收尾方式 | 走 `stop_stream()` 而**不是**只发 `CTRL STOP`：它负责触发 `capture.stop` 钩子、回填 WAV 头、把状态机放回 `ready`。只发 STOP 会留一条"界面在推流、声卡在补静音"的僵尸会话 |
| 报错方式 | 新事件 `stream-aborted`，不落 `error` 状态 |

**为什么不用 `error` 状态**：早停之后用户应该能**立刻重试**。落到 `error` 会让「开始」
按钮一直禁用（`canStart` 要求 `stateKind === 'ready'`），用户还得手动断开重连——
为一个"设备没发数据"的问题制造第二次操作成本，不合理。

#### 护栏二：`LINK_SLOW` → 提示 + 拒绝音质报告

先补一个**一直缺的东西**：`docs/ARCHITECTURE.md` 早就写死了 bit4 是 `LINK_SLOW`，
固件也在发，但 **Rust 协议层根本没有这个常量**（`frame.rs` 只有 0x01–0x08）。
本轮补上 `FLAG_LINK_SLOW = 0x10` / `Status::link_slow()`，并加了一条断言位值的单测
（`status_flag_bits_match_firmware`），避免两端再次分叉。

链路：

```text
Status.link_slow (bit4)
  → Pipeline.link_slow      粘住整场（中途恢复也不洗白）
  → Telemetry.link_slow     指标条显示「链路慢档」+ "欠载/漂移是假象"说明
  → SessionSummary.link_slow → SessionRecord.link_slow（收尾回填）
  → analyze_session()       直接拒绝出报告（CaptureNotStarting）
```

**为什么"粘住整场"而不是"跟着当前位走"**：慢档期间产生的欠载与漂移已经记进会话累计值，
无法事后剔除。就算设备中途被纠回 30 ms，前面那截脏数据还在 WAV 和计数里。

#### 验证状态（如实）

| 项 | 状态 |
| --- | --- |
| `cargo test -p apm-app` | **PASS** 65 passed（含新增 3 条：`detects_capture_that_never_starts`、`early_stop_boundary_uses_min_blocks_not_zero`、`link_slow_sticks_for_whole_session`） |
| `analyze_rejects_link_slow_session` / `analyze_does_not_reject_clean_session` | **PASS**（含"干净会话不被误伤"这一侧） |
| `status_flag_bits_match_firmware` | **PASS**（bit4 = 0x10；干净会话 flags 必须是 0x07） |
| `cargo clippy --all-targets` | **PASS**，0 警告 |
| `vue-tsc --noEmit` / `npm run build` | **PASS** |
| `build-gui.sh --release` | **PASS**，`.app` 12.26 MiB，Info.plist 校验通过 |
| **真机不误伤（2026-11-06 21:56）** | **PASS**。release 包重打后（签名变更触发一次蓝牙重授权，`GUI.md` §14.4 的已知行为，点「允许」+ 重启即恢复）。按 OK 开麦 16.2 s：**811 块 / 16.2 s = 50.0 块/秒，0 丢块、0 设备丢弃、0 次早停、无 LINK_SLOW 告警**，收尾正常回 `ready` 且 `capture.stop` 钩子触发。这是最关键的一条——护栏再准，误伤正常会话就不可接受 |
| **真机触发（造脏会话）** | **未验证**。本机 2026-11-06 全天都谈到 30 ms，造不出慢档会话（KI-005 慢间隔路径自当日起未复现）。**护栏的"拦截"那一半只到单测**，别写成"真机通过" |

**顺带得到一条 KI-006 的旁证**：上述 16.2 s 会话里有 **4 次欠载 / 989 帧 ≈ 22 ms**，
折算 ≈15 次/分钟，与 600 s 那轮的 157 次 ≈15.7 次/分钟几乎一致。这支持"KI-006 是
稳定速率事件、而非随负载累积"，但 16 s 样本太短，**不是结论**，只作为下次长测的对照点。

### 14.19 关麦兜底（2026-11-06，代码完成 + 真机两轮实测）

`docs/PRODUCT.md` 的「按键」节写明：OK 是 toggle 式，"松手自动停"不存在，
**必须配超时兜底**。这条从 v3 归并起就挂着，本轮落地。

#### 设计

| 决策 | 取值 | 理由 |
| --- | --- | --- |
| 默认时长 | **10 分钟** | `PRODUCT.md` / `config/README.md` 的定稿口径 |
| 可调范围 | **1 – 1440 分钟** | 下限 1 min：再短会把一次正常发言掐掉。上限 24 h |
| 生效范围 | **所有推流** | 项目负责人 2026-11-06 确认（见下"为什么不只拦 toggle"） |
| 检查周期 | 5 s | 兜底粒度是分钟级，跳得再快也只是多几次加锁 |
| 配置读取 | **每轮现读**，不在开流时快照 | 用户运行中把时长调大要立刻生效 |
| 收尾路径 | `stop_stream_with_reason()` | 触发 `capture.stop` 钩子、回填 WAV 头、状态回 `ready` |
| 界面位置 | 「缓冲」滑块右侧「关麦兜底 N 分钟」数字输入 | 与缓冲同属"运行中可改"的参数 |

**为什么不只拦 toggle**：初版方案倾向"只对 OK 键开麦生效"（界面点「开始」算故意长会话）。
项目负责人选了"所有推流 + 可配置"，理由是**界面开流一样可能忘记停**，而有配置项
就不会挡住 G2 的长测（把 10 调到 60 即可）。这条选择写在这里，免得日后被当成 bug 改回去。

#### 顺带修的一处日志误导

兜底最初复用 `stop_stream()`，而它的收尾日志把原因写死成「**手动**停止」。首次真机验证时
看到了这一行：

```text
连续采集已达 1 分钟（3002 块）：触发关麦兜底，自动停止
手动停止：3002 块 / 60.0 s，欠载 14，丢块 0（设备侧 0）      ← 不是用户点的！
```

**这正是排障最需要的那条线索被抹掉的地方**：事后看日志的人会把"忘记关麦被兜底了"
读成"用户自己停的"。改成 `stop_stream_with_reason(reason)`，两个自动停止点各自传原因：

| 调用方 | reason |
| --- | --- |
| 用户点停止 / OK 键 | `"手动停止"` |
| 关麦兜底 | `"关麦兜底（连续采集 N 分钟）"` |
| 早停看门狗 | `"早停（开流后无音频）"` |
| 链路断开 | `"链路结束"`（原本就有） |

#### 真机验证：两轮，可复现

把时长临时设成 1 分钟（改 `settings.json` 后重启），按 OK 开麦后**放着不动**：

| 轮次 | 开麦 | 兜底触发 | 差值 | 块数 | 收尾日志 |
| --- | --- | --- | --- | --- | --- |
| 第一轮 | 22:35:39.780 | 22:36:39.811 | **60.03 s** | 3002（50.03/s） | `手动停止` ← 文案问题，已修 |
| 第二轮 | 22:46:26.549 | 22:47:26.582 | **60.03 s** | 3002（50.03/s） | `关麦兜底（连续采集 1 分钟）` |

两轮都：0 丢块、0 设备丢弃、状态干净回 `ready`、`capture.stop` 钩子触发。
**误差 30 ms**，说明看门狗读的是会话自身的 `Instant`，不受 tick 周期漂移影响。

#### 验证状态（如实）

| 项 | 状态 |
| --- | --- |
| `cargo test -p apm-app` | **PASS** 68 passed（新增 `auto_stop_fires_only_after_limit`、`auto_stop_min_is_clamped`、`old_settings_without_auto_stop_default_to_ten`） |
| `cargo clippy --all-targets` | **PASS**，0 警告 |
| `vue-tsc --noEmit` / `npm run build` | **PASS** |
| `build-gui.sh --release` | **PASS** |
| **真机触发（1 min）** | **PASS，两轮可复现**，见上表 |
| **真机 10 min 默认口径** | **未跑**。验的是"机制到时会停"，不是"10 分钟正好停"——后者由同一个 `Duration` 比较保证，再等 10 分钟一次的边际价值很低 |
| UI 上改时长 | 代码路径已通（`set_auto_stop_min` → 立即落盘 + 看门狗现读），**未点过界面**（本轮无屏幕控制权限，走的是改 settings.json + 重启） |
### 14.20 G1 补测「关蓝牙 → no_adapter」：挖出两个真 bug（2026-11-07 凌晨）

G1 的最后一项一直挂着没测（`GUI.md` §10 的"未测"、`TODO.md` 的 P0）。本轮补测，
第一次跑就 **FAIL**，而且失败方式很有代表性。

#### 第一次：状态落在 `idle` 而不是 `no_adapter`

```text
链路探活 2 s 未返回，按已断开处理
设备已断开：7 s 没有通知，且链路报告未连接
状态 ready → stopping
状态 stopping → idle          ← 期望是 no_adapter
```

界面会显示「待连接」，用户看到的第一反应是"设备没开机"，而真实原因是蓝牙被关了。

#### Bug 1：`probe_adapter()` 判不出"蓝牙关闭"

旧实现：

```rust
async fn default_adapter() -> Result<Adapter> { /* 取第一个 adapter */ }
pub async fn probe_adapter() -> Result<()> {
    default_adapter().await.map(|_| ())   // 只检查"有没有 adapter 对象"
}
```

macOS 关掉蓝牙后，`manager.adapters()` **仍然返回一个 adapter**（只是扫不到设备）。
所以 `probe_adapter()` 永远返回 `Ok(())`。旁证：关蓝牙状态下跑 `apmctl scan`，
输出是「没发现设备」而不是「适配器不可用」——两者在旧代码里同形。

**修法**：改用 `adapter.adapter_state()` 判 `CentralState`：

| `CentralState` | 处理 |
| --- | --- |
| `PoweredOn` | `Ok(())` |
| `PoweredOff` | `Err("蓝牙已关闭：请在菜单栏或系统设置里打开蓝牙")` |
| `Unknown` | `Err(...)` —— 刚启动/未授权时状态还没同步，让调用方重试 |

#### Bug 2（更隐蔽）：关蓝牙走的路径**根本没调探测**

修完 Bug 1 立刻复测——**毫无变化**，日志里连那条错误都没有。

原因在 `monitor_loop` 的两处断链出口：

```rust
Ok(None) => {
    if alive == Some(false) {
        ...
        return None;              // ← 直接进 idle，从不探测
    }
}
Err(err) => {
    ...
    probe_adapter().await         // ← 只有这条会探测
}
```

现实里关蓝牙走的是**第一条**：链路只是"静默 + `is_connected` 返回 false"，
**不抛错**。所以只修 Bug 1 完全不起作用。

**修法**：抽出 `probe_after_link_loss()`，两条路径共用；并且把"为什么"写进日志
（`断链后检查适配器：{err}（状态落到 no_adapter）`）——这是用户唯一的诊断线索。

#### 修好后的真机日志

```text
21:51:29.648 warn ble  链路探活 2 s 未返回，按已断开处理
21:51:29.648 warn ble  设备已断开：7 s 没有通知，且链路报告未连接
21:51:29.663 warn ble  断链后检查适配器：蓝牙已关闭：请在菜单栏或系统设置里打开蓝牙（状态落到 no_adapter）
21:51:29.663 info app  状态 ready → stopping
21:51:29.663 info ble  链路已由监控循环判定断开，跳过主动断开调用
21:51:29.663 info app  状态 stopping → no_adapter
21:51:29.663 info ble  已断开
```

`stopping` 只停留 0 ms 就被覆盖成 `no_adapter`，符合 `teardown` 的"先收尾再落终态"设计。

#### 这一轮最值得记的教训

**"代码改了、编译过了、单测过了"不等于修好了。** Bug 1 修完时看起来完全正确，
甚至 `probe_adapter()` 单独跑（用临时 example 双向验证）也确凿无疑——
但端到端仍然毫无变化，因为真正走的路径压根没调它。

如果当时以"日志里应该出现这句话"作为验收口径，而没真去关一次蓝牙，这个 bug 会被漏掉。
这正是 `AGENTS.md` 硬规则 5「验证要如实」要防的事：**没接硬件就不能写'通过'**。

#### 口径澄清

`GUI.md` §10 原文写的是「关系统蓝牙 → `bt_unavailable`」。实际实现落到的是
**`no_adapter`** 这个状态（`AppState::NoAdapter`），`bt_unavailable` 是 `ErrorCode`
里的另一个东西（用于"检测不到适配器"的错误载荷）。**以实际行为为准，本节记为
「关蓝牙 → `no_adapter`」**，与 `core.rs` 的 `AppState::NoAdapter` 一致。
### 14.21 G3 对拍：GUI 与 CLI 的统计口径一致（2026-11-07 凌晨）

G3 一直标着"对拍未做"。本轮补测，**第一次按原判据（块数差 ≤ 1）判了 FAIL**，
查下来是判据本身有问题，不是实现有问题。

#### 怎么测的

设备只支持一条 BLE 连接（`sdkconfig.defaults` 的 `CONFIG_BT_NIMBLE_MAX_CONNECTIONS=1`），
所以**不能同时录**。按修订后的口径「同条件先后各录」：

1. 启动 GUI → 自动连上 → 按 OK 开麦约 46 s → 再按 OK 停 → 取 `SessionSummary`
2. `osascript` 退出 GUI（释放连接）→ `apmctl record --seconds 46`
3. 两边都开 `save_wav`，用 `apmctl analyze` 交叉校验 WAV

#### 结果

| 指标 | GUI | CLI | 判据 | 结论 |
| --- | --- | --- | --- | --- |
| 块数 | 2292 | 2299 | 差 7 | **原判据 FAIL** |
| 时长 | 45.84 s | 45.98 s | — | — |
| **块率** | **50.000 /s** | **50.000 /s** | 差 ≤ 0.5% | **PASS（0.000%）** |
| 丢块 | 0 | 0 | 一致 | **PASS** |
| 设备侧丢弃 | 0 | 0 | 一致 | **PASS** |
| 采样数 | 733440 | 735680 | — | — |
| **采样率** | **16000.0 /s** | **16000.0 /s** | 16 kHz | **PASS（零误差）** |
| 毛刺 | 无 | 无 | 0 段 | **PASS** |

#### 为什么块数差 7 不算问题

采样数差 `735680 − 733440 = 2240`，折合 `2240 / 16000 = 0.140 s`，
与时长差 `45.98 − 45.84 = 0.14 s` **完全吻合**。

也就是说：**两个客户端都没有丢数据，差异百分之百来自"录了多久"**。
按 CLI 块率折算，GUI 在 45.84 s 内应有 `45.84 × 50.000 = 2290` 块，实测 2292，
只差 2 块（0.09%）。

**原判据「块数差 ≤ 1」隐含了"两次录制时长完全相同"这个前提**，而两次独立录制
必然有时长差（谁也没法精确到毫秒同时开始同时结束）。这条判据会让正确实现在
正确的时候被判失败 —— 这正是"口径写错"而不是"代码写错"。

#### 顺带确认的两件事

1. **两份 WAV 都能被 `apmctl analyze` 直接吃**（G3 的另一条要求），
   字段与 `apmctl analyze --json` 一致 —— 因为报告构造只有一份（`apm_app::report`）。
2. **两份都被判 `software-fail`，原因是 RMS -51.35 / -56.62 dBFS**（要求 -30 ~ -12）。
   这是**录音时环境太安静**，不是链路问题：G3 测的是块数/丢块口径，音量属 G4 范畴。
   要拿到 `software-pass` 得对着麦克风正常说话，或者放参考音源。

#### 口径修订

`GUI.md` §10 的 G3 判据已从「块数差 ≤ 1」改为「**块率差 ≤ 0.5%** + 丢块一致 +
设备侧丢弃一致」。理由如上：块率才是"统计口径是否一致"的判据，块数不是。

#### 顺带修掉一个测试污染（同类问题会反复出现）

G3 要开 `save_wav`，我直接改了用户配置 `~/Library/Application Support/.../settings.json`。
结果 `verify.sh` 立刻红了一条：

```text
thread 'core::tests::starts_idle_and_logs_boot' panicked:
默认不录 WAV
```

**根因**：`AppCore::new(log_dir)` 的 `log_dir` 只隔离日志目录，
`settings` 走的是**全局** `Settings::load()`，读用户真实配置文件。
所以任何人在界面上勾一次「同时保存 WAV」，`cargo test` 就会失败。

**修法**：加 `AppCore::with_settings(log_dir, settings)`，测试用
`Settings::default()` 构造，不碰磁盘。验证方式是把用户配置改成
`save_wav: true` 再跑一遍——必须仍然全绿（现在确实全绿）。

**教训**：测试读到用户机器的外部状态，是"昨天绿今天红"的常见来源。
凡是 `load()` 磁盘/环境变量的构造函数，都该留一个可注入的入口。
