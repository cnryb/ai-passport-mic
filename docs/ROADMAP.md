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
- [ ] **端到端延迟未测**：控制帧 `0x83 PONG` 只在常量里定义，固件 `app_main.c` 没有对应
      分支，PC 端也没有发起 PING 的代码 —— 延迟测量缺手段，不是缺数据。

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
      两个方向跑 60 s 均 0 欠载 / 0 超容量丢弃。**真机未复测。**
- [x] **满量程毛刺拦截**：6 次真机录音里 2 次出现约 0.8 s 的 ±32767 交替噪声且 `序号跳变=0`。
      用编码→解码复现排除了编解码（`tests/glitch_repro.rs`），定位到采集端瞬态读出。
      固件编码前压成静音（`apm_pcm_is_corrupt`），PC 端解码后再兜一层
      （`apm_protocol::glitch`），`analyze` 把 `glitch_events` 计入结论。
      **"为什么 codec 会读出这种数据"仍未定，真机未复测。**
- [ ] 回环自动验收（`sink --verify`）受宿主麦克风权限阻塞，靠 G3b 用 GUI 自身身份恢复。

## M2.5：GUI（G0 已落地，G1–G6 未开始）

Tauri 2 + Vue 3，分期 G0–G6，各期单独验收。完整方案见 [`GUI.md`](GUI.md)。

已定决策（2026-11-04，见 `GUI.md` §12）：CLI 保留为诊断通道、首发仅 macOS、
会话 WAV 默认不落盘、按键映射进首发但不卡前期、不做托盘常驻。

- [ ] **前置：把 `record` / `sink` 的编排迁进 `apm-app`。** 目前这两个命令的编排仍在
      `crates/cli/src/cmd.rs`，`apm-app` 也还没依赖 `apm-sink`。不迁就做 G2，等于让 GUI
      和 CLI 各长一套编排 —— 协议层已经用共享向量防过一次这种分叉，这里别再犯。
- [x] G0 骨架 + macOS 蓝牙/麦克风权限（后续所有阶段的前置门槛）。
      `.app` 构建、ad-hoc 签名、Info.plist 用途声明、无 TCC abort 启动均已验证；
      **首次授权弹窗与"扫到设备"这一条未验证**（需要人在系统弹窗点允许）。
- [ ] G1 连接 + 状态机 + 结构化事件日志。连接/断开代码已在 `apm-app`，
      但"拔电源 2 s 内进 idle""关系统蓝牙 → bt_unavailable"两条验收没做。
- [ ] G2 实时电平 + 推流 + 指标条（`AppState::Streaming` 目前是占位）。
- [ ] G3 声卡选择 + 会话 WAV；G3b 用 GUI 自身身份恢复回环自动验收。
- [ ] G4 音质报告视图；G5 按键映射编辑；G6 签名打包公证。


## M3：按键自定义

- 编辑器支持 UP / DOWN / OK 的短按、双击、长按、按下、抬起。
- 配置持久化、恢复默认、导入/导出（schema 见 `config/README.md`）。
- 保证 OK 的 PTT 停止路径不会被配置破坏。
- 快捷键注入无论成功或异常，都要释放所有按下的修饰键。

## M4：音质验收

- `apmctl analyze` 输出完整 JSON 报告（字段见 `docs/SOUND_QUALITY.md`）。
- 增加启动延迟与稳态端到端延迟测量。**前置：先补 PING/PONG** —— 控制帧 `0x83 PONG`
  只在 `protocol.h` / `frame.rs` 里定义了常量，固件没有回包分支，PC 端也没有发起 PING
  的代码。没有这一层，"延迟"只能靠主观感受。
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
