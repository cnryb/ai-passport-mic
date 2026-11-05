# 路线图

## M0：项目初始化（已完成）

- [x] 确定产品边界：纯无线麦克风，不做 ASR / 文字注入 / AI Agent。
- [x] 仓库骨架：`device/`、`desktop/`、`config/`、`docs/`、`scripts/`。
- [x] 文档：产品、架构（含协议契约）、环境、音质评估方法、参考实现。
- [x] 按键映射 schema 与可执行样例（`config/`）。
- [x] 环境脚本：`check-env.sh`（只读体检）、`activate-idf.sh`、`verify.sh`（基线自检）。

> M0 的定义是「仓库能交给下一个人接手」，不是「软件能用」。

## M1：固件最小链路（真机链路已通过，按键手势待核对）

- [x] ESP-IDF 工程骨架 + 8 MB 分区表 + USB-Serial-JTAG 控制台。
- [x] ES8311 采集：16 kHz / 16-bit / mono（**真机通过**：15 s 连续录音 748 块、0 丢块）。
- [x] IMA-ADPCM 编码，20 ms 一块（320 samples → 160 字节 + 5 字节块头）。
- [x] BLE 自定义 GATT：CTRL / PROFILE / STATUS / EVENT / AUDIO 五个特征值。
- [x] 按键（UP/DOWN/OK）事件：单 ADC 分压判定 + 手势状态机 + GATT 事件。
- [x] 构建产物：`idf.py build` 通过、`idf.py merge-bin` 出合并镜像。
- [ ] ~~本地 HID 键盘路径~~ → **第一版不做**，理由见下面的说明。
- [x] 烧录、MTU 与通知、块丢失统计：MTU 247（需求 ≥168），音频/状态/事件三路订阅均建立。
- [x] PTT 启停：`apmctl record` 走 CTRL START/STOP 强制采集，不依赖 OK 键。
- [ ] **按键电压窗口标定 + 手势逐项核对**：UP/DOWN/OK × 短按/双击/长按/按下/抬起共 12 个
      事件，没有在真机上依次按过。换板要改 `main/board.h` 后重新标定。
- [x] **延迟测量手段（链路）**：控制帧 `0x04 PING` / `0x83 PONG` 两端落地，
      `apmctl ping` 输出 RTT 的 min / 中位 / p95 / max 与抖动，支持 `--json`。
      口径是**链路往返**，不能当端到端延迟用。**真机已测（2026-11-05）：中位 38.2 ms /
      p95 41.5 ms / 抖动 3.0 ms，20 发 20 收 0 超时。**
- [ ] **端到端声学延迟未测**：还缺"声源 → 麦克风 → codec → 缓冲 → 声卡"这一整段的测量，
      卡在宿主麦克风权限（见 [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) 的 KI-003）。

> **关于 HID 路径的取舍。** 原来这条写的是"本地 HID + GATT 事件双路径"。
> 但 `docs/PRODUCT.md` 和 `config/README.md` 把"映射到哪个键"定在 PC 端，
> 固件再发一份 HID 报告就等于绕过映射表，两条路径的语义会打架
> （用户改了映射，HID 那边不跟着变）。所以第一版只保留 GATT 事件路径；
> 等"PC 软件没运行也要能按键"这个需求真的出现，再单独评估 HID，别顺手加。

## M2：桌面端最小链路（主链路已通；漂移与毛刺已拦，待真机复测）

- [x] Rust 设备链路：BLE 连接、订阅、序号检测、ADPCM 解码。
- [x] 协议层与固件用同一份共享向量逐字节对拍（`config/protocol-vectors.json`）。
- [x] `apmctl record` 输出 WAV，作为链路验收工具；序号跳变单独计数上报。
- [x] 断连 / 未订阅 → 明确状态（STATUS 的 flags 位与 `record` 的报错路径）。
- [x] 输出到虚拟声卡（BlackHole / VB-CABLE），40–80 ms 自适应抖动缓冲：
      真机 20 s / 999 块 / 0 丢块 / 0 欠载，缓冲稳在 68–76 ms；
      会议软件里选 `BlackHole 2ch` 当麦克风可用。
- [x] 声卡不可用时明确报错：`Sink::open` 打不开就失败并列出可用设备，不做隐式重采样。
      GUI 侧的提示与引导留到 G3。
- [x] `apmctl analyze` 输出指标与 JSON 报告（原属 M4，提前落地）。
- [x] 真机验证 scan / status / record / sink 四条路径（数据见 `desktop/README.md` 验证状态表）。
- [x] **时钟漂移补偿**：设备与主机各自 16 kHz 不同源，20 s 累计约 84 ms。改成按缓冲存量
      做单样本插删（比例控制，上限 1%，`crates/sink/src/playout.rs`）。仿真里 ±5000 ppm
      两个方向跑 60 s 均 0 欠载 / 0 超容量丢弃。
      **真机 60 s 已复测（2026-11-05）**：补偿 −57 ppm、3000 块 0 丢块、0 欠载、缓冲稳在
      55–95 ms（KI-002 只差 10 min 那次）。
- [x] **满量程毛刺拦截**：6 次真机录音里 2 次出现约 0.8 s 的 ±32767 交替噪声且 `序号跳变=0`。
      用编码→解码复现排除了编解码（`tests/glitch_repro.rs`），确认垃圾在**进 ADPCM 之前**
      就产生了。固件编码前压成静音（`apm_pcm_is_corrupt`），PC 端解码后再兜一层
      （`apm_protocol::glitch`），`analyze` 把 `glitch_events` 计入结论。
- [ ] **毛刺的根因（为什么 I2S/ES8311 会读出这种数据）仍未定位**，只是不再让它进会议软件。
      排查方案、候选假设与验收标准见 [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) KI-001 ——
      在拿到串口抓到的原始 I2S 数据之前，不要写成"已解决"。
- [ ] 回环自动验收（`sink --verify`）受宿主麦克风权限阻塞，靠 G3b 用 GUI 自身身份恢复。
- [x] **10 min 连续推流真机（2026-11-05，KI-001 的验收口径）**：29998 块（精确 50/s）、
      序号跳变 0、长度异常 0、设备侧丢弃 0、**采集端毛刺 0 段**、漂移 +86 ppm。
      但**欠载 10 次**（763 静音帧 ≈ 48 ms），不满足"0 欠载"，见
      [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) **KI-006**。
- [x] **设备异常断开后不恢复**：实测块率会从 50/s 崩到 1.5/s，复位后恢复；记为
      [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) **KI-005**。做长测试前必须先复位设备。

## M2.5：GUI（G1–G5 代码已落地，G3b/G6 未做）

Tauri 2 + Vue 3，分期 G0–G6，各期单独验收。完整方案见 [`GUI.md`](GUI.md)。

已定决策（2026-11-04，见 `GUI.md` §12）：CLI 保留为诊断通道、首发仅 macOS、
会话 WAV 默认不落盘、按键映射进首发但不卡前期、不做托盘常驻。

本轮门禁（2026-11-05）：`./scripts/verify.sh` 全绿、`cargo test` 89 passed / 0 failed、
`./scripts/build-gui.sh` 通过（`.app` 30.29 MiB）。明细见 `GUI.md` §14.2；
它证明的是"能编译、单测过、打得出来"，**不等于** G1–G5 已验收。

- [x] **前置：把 `record` / `sink` 的编排迁进 `apm-app`。** 采集、解码、丢块统计、
      声卡写入、指标聚合现在只有一份实现（`crates/app/src/{session,pipeline}.rs`），
      `cli` 只解析参数与打印，GUI 走同一条管线。
- [x] G0 骨架 + macOS 蓝牙/麦克风权限（后续所有阶段的前置门槛）。
      `.app` 构建、ad-hoc 签名、Info.plist 用途声明、无 TCC abort 启动均已验证；
      **2026-11-05 真机 PASS**：授权弹窗正常、6 s 扫到 7 个设备（目标在列）、
      RSSI 与 `apmctl scan` 都是 −50 dBm。明细见 [`GUI.md`](GUI.md) §14.7。
- [x] G1 连接 + 状态机 + 结构化事件日志（代码）。加了 5 s 静默 + `is_connected` 判活，
      断链后探一次适配器决定回 `idle` 还是 `no_adapter`。
      **2026-11-05 真机部分 PASS**：`ready` 与 `ready → stopping → idle` 已验；
      修掉两处 btleplug 挂死（`is_connected` / `disconnect` 在设备消失后不返回，
      会让状态永久卡在 `ready` 或 `stopping`）。断链检测实测 **7.64 s**；
      **口径同日由「2 s 内」调整为「≤ 8 s」**（项目负责人确认），因为轮询判活最快就是
      5 s 静默 + 2 s 探活。事件驱动（`CentralEvent::DeviceDisconnected`，可压到 <1 s）
      另开一轮；
      「关蓝牙 → `bt_unavailable`」仍未测。
- [x] G2 实时电平 + 推流 + 指标条（代码）：`streaming` 状态不再是占位，5 Hz `telemetry`
      + canvas 电平表 + 缓冲滑块（运行中改目标，取下一批回调生效）。
      **2026-11-05 真机 15.6 min**：46949 块 / 939 s = 50.00 块/秒、丢块 0、设备侧丢弃 0、
      缓冲 77–90 ms、漂移 +94 ppm；**欠载 13 次（≈84 ms）**，与 CLI 同量级（KI-006），
      "0 欠载"不通过。CPU 20.7% / 内存 166.9 MB（按 App 全家桶算）也超 G2 口径，
      明细见 [`GUI.md`](GUI.md) §14.10。
- [x] G3 声卡选择 + 会话 WAV（代码）：枚举带采样率范围，不支持 16 kHz 的设备提前拒绝；
      WAV 增量写、收尾回填头；GUI 与 CLI 共用同一份会话统计。
      **GUI 与 `apmctl record` 的同时录对拍没做**。
- [ ] G3b 用 GUI 自身身份恢复回环自动验收。**未实现**（`--verify` 仍在 CLI）。
- [x] G4 音质报告视图（代码）：报告构造只有一份（`apm_app::report`），
      GUI 的表格与「复制 JSON」都出自它，未测字段是 null。
- [x] G5 按键映射（代码）：**3 个事件**（一个键一个动作；2026-11-05 两步归并
      15 → 6 → 3，schema 升到 `keymap-3`，v1/v2 都能迁移）的编辑、导入、导出、
      恢复默认、落盘；
      `ok.release` 强制回填 `ptt-stop`。**注入层（M3）未实现，改了映射不会真的敲键盘**。
- [ ] G6 签名打包公证。**未做**，需要 Apple 开发者证书，当前只有 ad-hoc 签名。


## M3：按键自定义

- [x] 编辑器支持 UP / DOWN / OK 的**一个键一个动作**（3 个事件，随 G5 落地）。
      2026-11-05 两次归并：15（手势）→ 6（按下/抬起）→ 3（每键一动作）。
      固件仍上报 5 种手势码，PC 端只消费 PRESS；协议与跨端向量都不动。
      **OK 变成开关式后少了"松手自动停"**，M3 注入层必须补超时兜底。
- [x] 配置持久化、恢复默认、导入/导出（schema 见 `config/README.md`）。
- [x] 保证 OK 的 PTT 停止路径不会被配置破坏（`ok.release` 保存时强制回填 `ptt-stop`）。
- [x] **注入层（macOS 点按）已实现（2026-11-05）**：`apm-app::inject` 用 CGEvent
      把映射动作发到当前前台应用；按键事件（`up`/`down`）由采集钩子机制承接——
      目前按键事件本身**尚未接到采集状态**，先落地的是采集钩子
      （开麦/关麦各注入一次，见 `hooks.capture.start/stop`）。
- [x] **修饰键释放**：注入用"事件自带 flags"的方式发，不单独发修饰键事件，
      不存在"按下去没松开"的状态（单测覆盖 `ptt-toggle` 不可注入）。
- [ ] **权限引导**：没有辅助功能权限时 CGEventPost 会静默丢弃；现在只记日志，
      要补"界面上给出权限指引 + 一键打开系统设置"。
- [ ] **按键事件接到采集**：`up` / `down` 的映射动作现在只在钩子路径生效，
      按键事件 → 注入的直连还没接。
- [ ] **超时兜底**：toggle 语义下忘记关麦是真实风险（连续 10 分钟自动停）。
- [ ] **"按住不放"模式**：微信/钉钉"按住说话"类应用需要 hold，尚未做。

## M4：音质验收

- `apmctl analyze` 输出完整 JSON 报告（字段见 `docs/SOUND_QUALITY.md`）。
- 链路往返时延：`apmctl ping` 已落地并**真机实测（2026-11-05）**——20/20 收到、0 超时，
  中位 38.2 ms / p95 41.5 ms / 抖动 3.0 ms。口径是链路，不是端到端。
- 启动延迟（OK 按下 → 首块音频）：可基于 EVENT 与块到达时间做，但需要事件到达时刻
  与首块到达时刻在同一时钟下，尚未实现。
- 稳态端到端延迟：需要声学回环，前置是 G3b（见 KI-003）。
- 完成固定场景矩阵，建立版本对比基线。
- 评估是否需要重采样、AGC、降噪或更高码率编码。

## M5：产品化

- 安装包、签名、自动更新、崩溃日志脱敏。
- 虚拟声卡安装引导与故障诊断：检测 BlackHole / VB-CABLE 是否就绪、给下载指引、
  装完一键验证路由。**不静默安装**（BlackHole 是 GPL-3.0，随包分发要商业许可）。
- 长时间稳定性、功耗与热测试。
- 再评估"免装第三方"的可行性：自研 AudioServerPlugIn 驱动，或换硬件形态。
  注意 HFP 在 ESP32-C3 上不可行（无经典蓝牙），LE Audio 也受 C3 的 BT 5.0 限制，
  详见 [`ARCHITECTURE.md`](ARCHITECTURE.md) 的「为什么第一版走自定义 GATT」。

## 已知约束

- 只做 BLE 自定义 GATT：ESP32-C3 没有经典蓝牙，HFP 走不通；LE Audio 需要 BT 5.2+，
  C3 是 5.0。代价是用户必须自己装虚拟声卡（macOS BlackHole / Windows VB-CABLE）。
- 设备端无 PSRAM，音频缓冲与 UI 内存都要按 C3 的余量规划。
- 参考实现可以看，但不能整份搬进来（见 `docs/REFERENCES.md`）。
