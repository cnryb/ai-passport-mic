# 技术架构

## 总览

![AI Passport Mic 技术架构](architecture.svg)

矢量图源文件见 [`architecture.svg`](architecture.svg)（随系统深浅色自适应）。下面是与图等价的
纯文本版，便于在终端和 diff 里对比：

```text
┌──────────────────────────── AI Passport / ESP32-C3 ────────────────────────────┐
│ ES8311 microphone                                                            │
│   │ I2S 16 kHz / 16-bit / mono                                               │
│   ▼                                                                          │
│ 320 samples ──► IMA-ADPCM ──► [seq][predictor][index][nibbles] ──► BLE notify │
│                                                                              │
│ UP / DOWN / OK ──► event queue ──► HID keyboard + custom GATT event           │
└──────────────────────────────────────────────────────────────────────────────┘
                                      │ BLE
                                      ▼
┌──────────────────────────────── PC ────────────────────────────────────────────┐
│ Rust device link ──► protocol / ADPCM decode ──► audio buffer                  │
│                                                   ├─► virtual audio device     │
│                                                   └─► WAV + quality metrics    │
│ keymap engine ──► OS shortcut injector                                         │
└──────────────────────────────────────────────────────────────────────────────┘
```

## 为什么第一版走自定义 GATT

既有参考实现已验证 BLE 自定义 GATT + IMA-ADPCM 能完成低延迟音频和 HID 按键（来源见 [REFERENCES](REFERENCES.md)）。第一版由 PC 软件把 PCM 写入虚拟声卡，会议软件把它当作普通麦克风。

**为什么不是"让系统直接认成蓝牙麦克风"**：这条路在这颗芯片上不存在，别白花时间。

| 方案 | 为什么不可行 / 代价 |
| --- | --- |
| HFP（免提协议） | HFP 是**经典蓝牙（BR/EDR）**的 profile。ESP32-C3 只有 BLE 射频，没有经典蓝牙 —— 不是"暂不做"，是芯片层面做不到 |
| LE Audio（LC3 / 单播） | 需要 Bluetooth **5.2+**，C3 是 5.0；且 macOS 对第三方 LE Audio 输入设备的开放度仍然很低（Apple 到 WWDC26 仍未支持 Auracast） |
| 自研 AudioServerPlugIn 驱动 | 技术上可行，但要 Apple 开发者账号签名 + 公证，驱动异常会拖垮整机音频，且成为长期维护项 |
| 换硬件（USB Audio Class） | 插线就不是"纯无线"了；要保住无线又走 HFP，得换成支持经典蓝牙的芯片 |

代价：用户需要安装虚拟声卡；PC 软件必须保持运行。这个取舍被明确接受为第一版方案。

## BLE 服务

- Service: `a7c52e00-31d4-4d2a-8f6b-5e9c1b3f7a00`
- CTRL: `...7a01`
- PROFILE: `...7a02`
- STATUS: `...7a03`
- EVENT: `...7a04`
- AUDIO: `...7a05`

控制帧：

```text
[0x56 0x4D][version=1][type][len u16-LE][payload][CRC16-CCITT u16-LE]
```

音频块：

```text
[seq u16-LE][predictor i16-LE][index u8][IMA-ADPCM nibbles...]
```

20 ms @ 16 kHz = 320 个采样；ADPCM 为 160 字节，加 5 字节块头共 165 字节，约 66 kbps。

## 协议细节（实现契约）

下面这些是写代码时定下来的细节，`device/main/protocol.c` 与主机单测
`device/test/test_protocol.c` 都按它执行。要改先改这里，两端同步。

### CRC

- 算法：CRC-16/CCITT-FALSE（poly `0x1021`，init `0xFFFF`，不反转，xorout `0x0000`）。
- 覆盖范围：从 `version` 到 payload 最后一个字节，**不含**两字节 magic。
- 校验向量：`"123456789"` → `0x29B1`（单测里钉死）。

### 控制帧类型

| type | 方向 | 载荷 | 说明 |
| --- | --- | --- | --- |
| `0x01` START | PC → 设备 | 空 | 强制开始采集，不依赖 PTT |
| `0x02` STOP | PC → 设备 | 空 | 强制停止采集 |
| `0x03` SET_GAIN | PC → 设备 | `int8` dB | 麦克风 PGA 增益，0–42 |
| `0x81` STATUS | 设备 → PC | 8 字节 | 1 s 心跳，也可直接读 STATUS 特征值 |
| `0x82` EVENT | 设备 → PC | `btn u8, ev u8, flags u8` | 按键事件 |
| `0x83` PONG | 设备 → PC | 4 字节原样回传 | 延迟测量。**两端都未实现**：固件 `app_main.c` 没有回包分支，PC 端也没有发起 PING 的代码 |

STATUS 载荷：`state u8, flags u8, dropped u16-LE, sent u32-LE`。

- `state`：`0` IDLE / `1` READY / `2` CAPTURING / `3` ERROR
- `flags`：bit0 BLE 已连接、bit1 AUDIO 已订阅、bit2 PTT 按住、bit3 音频错误

### 音频块

- 块头 5 字节：`seq u16-LE`、`predictor i16-LE`、`index u8`。
- nibble 打包：**低 4 位放偶数号采样**，高 4 位放奇数号采样。
- 块头里的 `predictor` / `index` 是**本块编码开始前**的状态。编码器跨块继承，
  解码器每块由块头重置 —— 因此丢一块不会污染后续音频，PC 端可以只靠 `seq`
  判断丢块，不必等链路恢复。
- 块长固定 165 字节，MTU 必须 ≥ 168；固件协商到 247 并把它作为发送前置条件。

### 按键事件

`ev`：`0` PRESS / `1` CLICK / `2` DOUBLE / `3` LONG / `4` RELEASE。
与 `config/README.md` 的事件名对应：`CLICK → *.short`，`PRESS/RELEASE → ok.press / ok.release`。

固件侧手势时序：去抖 20 ms、双击窗口 180 ms、长按 500 ms。
长按在按住满 500 ms 时**立即**上报，抬起后不再补发单击。

固件只走 GATT 事件路径，不额外发 BLE HID 报告 —— 映射表在 PC 端，固件再发一份 HID
就等于绕过它。取舍记录在 [`ROADMAP.md`](ROADMAP.md) 的 M1。

实现位置：`device/main/buttons.c`（判定）、`device/main/link.c`（EVENT 通知）、
`device/main/app_main.c`（PTT 与事件分发）。

## PC 端模块

模块划分是设计决定；名称按本项目的 `ai-passport-mic` 语义命名，不沿用参考实现的内部命名。

| 模块 | 职责 | 状态 |
| --- | --- | --- |
| `protocol` | 帧、载荷、ADPCM 编解码、按键映射 schema、WAV/音质指标 | 已实现 `desktop/crates/protocol`，含与 C 实现逐字节对拍的测试 |
| `link` | BLE 扫描/连接、特征值订阅 | 已实现 `desktop/crates/link`（btleplug），已在真机跑通（MTU 247） |
| `apmctl` | 扫描、状态、录音与音质分析命令行（验收工具） | 已实现 `scan` / `status` / `record` / `sink` / `probe` / `analyze` / `selftest` |
| 虚拟声卡输出 | 把解码后的 PCM 写进 BlackHole / VB-CABLE，带抖动缓冲 | 已实现 `desktop/crates/sink`（cpal 0.15，40–80 ms 自适应抖动缓冲）；已验证到会议软件端到端可用，回环探针受麦克风权限限制 |
| GUI 应用 | 设备状态、按键映射、虚拟声卡选择、实时电平与音质报告 | G0 已落地 `desktop/crates/gui`（Tauri 2 + Vue 3：骨架、蓝牙权限、设备扫描）；方案与后续分期见 [`GUI.md`](GUI.md) |
| `app` 内核 | 状态机、设备发现/连接编排、指标聚合、事件广播、诊断日志 | 部分实现 `desktop/crates/app`（`apm-app`，**不依赖任何 GUI 框架**）：`detect_adapter` / `scan` / `connect` 已在内核；`record` / `sink` 的编排仍在 `crates/cli/src/cmd.rs`，**待迁入**（[`GUI.md`](GUI.md) 决定 D1） |

跨端一致性不靠"两份实现看起来一样"：`device/test/gen_vectors.c` 从 C 实现导出
`config/protocol-vectors.json`，Rust 的 `tests/c_vectors.rs` 读同一份文件逐字节比对
（CRC、帧、ADPCM 编码与跨块状态）。改了 `device/main/protocol.c` 必须重跑
`scripts/gen-protocol-vectors.sh`，否则对拍会用旧向量。

`device/main/protocol.c` 已按上面的契约实现了帧、CRC 与 ADPCM（编码 + 解码，解码用于主机
单测和后续 PC 端对照）。PC 端的 Rust 实现必须能通过同一组测试向量，否则两端不算对齐。

> 实现约束：桌面端**不要**引入固件烧录、语音识别、文字注入这类功能——它们不在第一版
> 范围内（见 `docs/PRODUCT.md` 的非目标）。

## 音频输出策略

目标不是让设备直接出现在系统麦克风列表，而是：

```text
AI Passport -> PC app -> BlackHole/VB-CABLE -> Zoom/Meet/OBS
```

这样第一版可以控制采样率、抖动缓冲和丢块策略，且不依赖系统蓝牙音频栈。

## 分发与安装成本

**用户必须自己装一个虚拟声卡**，这是第一版绕不开的成本：

| 平台 | 要装什么 | 之后怎么用 |
| --- | --- | --- |
| macOS | BlackHole 2ch（免费，GPL-3.0） | 本程序写到 `BlackHole 2ch` 的**输出**；会议软件的麦克风选 `BlackHole 2ch` 的**输入** |
| Windows | VB-CABLE（免费） | 同上，名字是 `CABLE Input` / `CABLE Output` |

许可证上必须清楚的一点：

- BlackHole 是 **GPL-3.0**。没有商业例外，**不能**把它的二进制塞进闭源安装包里分发；
  要随包分发就得找 Existential Audio 买商业许可。
- "让用户自己去官网或 `brew install blackhole-2ch` 安装"不涉及分发，完全没问题 ——
  它保持独立软件的身份。

所以第一版对外分发时，安装器能做的只有三件事：检测缺失、给下载指引、装完后一键验证
路由是否打通。**不要**替用户静默安装。

要彻底去掉这个依赖，只有两条路，代价都在别处：自研 AudioServerPlugIn 驱动
（签名 + 公证 + 系统级故障风险 + 长期维护），或改硬件形态（USB 麦克风 / 换支持经典
蓝牙的芯片）。M5 之前不要悄悄改这个取舍。

## 延迟预算

| 环节 | 目标 |
| --- | --- |
| 设备采集 + 编码块 | 20 ms |
| BLE 连接间隔与通知 | 15–30 ms |
| PC 抖动缓冲 | 40–80 ms |
| 虚拟声卡缓冲 | 10–40 ms |
| 稳态合计 | p95 ≤ 150 ms |
| OK 按下到首块音频 | p95 ≤ 250 ms |

这些是验收目标，不是当前实测值。必须在真实设备、目标主机和虚拟声卡组合上测量。

## 可靠性约束

- 音频块按序号检测丢失和乱序；报告记录丢块数，不用“音频能播”掩盖链路质量。
- 设备断连后 PC 端必须停止向虚拟声卡写入旧缓冲，避免会议软件持续收到静音伪装成在线。
- 设备与声卡各有一颗晶振，"16 kHz" 不是同一个数（实测 20 s 差约 84 ms）。PC 端必须做
  长期漂移补偿：按缓冲区存量趋势做单样本插删（上限 1%），**不允许**靠周期性欠载或整帧
  丢弃兜底。实现见 `desktop/crates/sink/src/playout.rs`。
- 采集端偶发满量程逐样本变号的瞬态读出，两端各拦一次：固件编码前压成静音（
  `apm_pcm_is_corrupt`），PC 端解码后再兜一层（`apm_protocol::glitch`）。判据与口径见
  [`SOUND_QUALITY.md`](SOUND_QUALITY.md) 的「毛刺」。
- 快捷键注入必须使用按下/抬起配对，异常路径要释放 Ctrl / Alt / Shift / Win。
- 修改设备名时保留旧蓝牙名别名，避免已配对设备因名称缓存失效。
- 固件构建与真机验证分开报告；没有硬件证据不得宣称通过。
