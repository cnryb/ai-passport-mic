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

既有参考实现已验证 BLE 自定义 GATT + IMA-ADPCM 能完成低延迟音频和 HID 按键（来源见 [REFERENCES](REFERENCES.md)）。标准 BLE HFP 在 macOS/Windows 上的角色、编码协商和系统音频路由差异很大，不适合作为项目第一步。第一版由 PC 软件把 PCM 写入虚拟声卡，会议软件把它当作普通麦克风。

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

## PC 端模块（规划，尚未实现）

模块划分是设计决定，先定下来再写代码；名称按本项目的 `ai-passport-mic` 语义命名，
不要沿用参考实现的内部命名。

| 模块 | 职责 |
| --- | --- |
| `protocol` | 帧、载荷、ADPCM 编解码、按键映射 schema、WAV/音质指标 |
| `link` | BLE 扫描/连接、特征值订阅（后续再评估串口兜底） |
| `apmctl` | 扫描、状态、监听、录音与音质分析命令行（验收工具） |
| GUI 应用 | 设备状态、按键映射、虚拟声卡选择、实时电平与音质报告 |
| 虚拟声卡 | BlackHole / Loopback（macOS）或 VB-CABLE（Windows），非本项目实现 |

> 实现约束：桌面端**不要**引入固件烧录、语音识别、文字注入这类功能——它们不在第一版
> 范围内（见 `docs/PRODUCT.md` 的非目标）。

## 音频输出策略

目标不是让设备直接出现在系统麦克风列表，而是：

```text
AI Passport -> PC app -> BlackHole/VB-CABLE -> Zoom/Meet/OBS
```

这样第一版可以控制采样率、抖动缓冲和丢块策略，且不依赖系统蓝牙音频栈。后续若需要“无额外软件”的体验，再评估 HFP 或原生驱动。

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
- 快捷键注入必须使用按下/抬起配对，异常路径要释放 Ctrl / Alt / Shift / Win。
- 修改设备名时保留旧蓝牙名别名，避免已配对设备因名称缓存失效。
- 固件构建与真机验证分开报告；没有硬件证据不得宣称通过。
