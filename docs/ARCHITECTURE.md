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
│ UP / DOWN / OK ──► event queue ──► custom GATT event（无 HID 路径）           │
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
- **Battery Service（标准）**: `0x180F` / Battery Level `0x2A19`（见下）

### 连接参数：8.25 KB/s 是链路契约的一部分

50 块/秒 × 165 字节 = **8.25 KB/s**。这个速率只在快速连接间隔下推得动，所以它跟帧格式
一样属于跨端契约，不是固件的实现细节：

**两档间隔：采集中快速档，空闲省电档。**

| 状态 | 间隔 | 用途 | 判据 |
| --- | --- | --- | --- |
| 采集中（CAPTURING） | **15–30 ms**，`latency=0` | 撑住 50 块/秒 | `LINK_SLOW` 判据用 **40 ms** |
| 空闲（IDLE / READY） | **80 ms**，`latency=0` | 射频唤醒次数降到 1/3 左右 | 不作为 `LINK_SLOW` 依据 |

- 设备广播里带 `0x0d`（从机首选连接间隔）= **15–30 ms**；建连后 600 ms 再主动发一次
  参数更新，被拒按 1 s 退避重试，最多 6 次；
- **进入采集中**（CTRL START 或 PTT 按下）立刻请求 15–30 ms；
- **离开采集中**（CTRL STOP / PTT 抬起 / 断链）请求 **80 ms**；
- 运行中每次心跳检查实际间隔：**采集态 > 40 ms 就重新请求**（5 s 节流）并置 STATUS 的
  `LINK_SLOW` 位；**空闲态 > 120 ms**（主机没接受省电档、或落在更慢档）才重新请求，
  且**不**置 `LINK_SLOW`——空闲态本来就不需要 50 块/秒，报慢档是误伤；
- 实测（macOS 15.3.1）：请求 15–30 ms 会被接受并落在 30 ms；请求 15–20 会被主机拒绝。
  这段历史见 [`GUI_IMPLEMENTATION_LOG.md`](GUI_IMPLEMENTATION_LOG.md) §14.8 与 [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) KI-005。

**为什么空闲档取 80 ms 而不是更大**：从 80 ms 切回 30 ms 只需要等 1–2 个旧间隔，
**60–160 ms 内生效**，塞得进「OK 按下到首块音频 p95 ≤ 250 ms」的预算（见「延迟预算」）。
取 200 ms 时切换要约 200–400 ms，会把开场拖成一段可听的停顿——这是被明确否掉的。

**为什么写进契约**：间隔一旦被主机放到 500 ms 量级，吞吐就掉到 ≈2 块/秒（每 500 ms
才送得出一块），块率随之坍塌 —— 而设备侧的采集与编码完全正常。这种会话产出的欠载、
漂移数字全是链路假象，不能当音质结论用。

**PC 端配套约束**：设备每秒发一次 STATUS 心跳是链路探活的依据（`LINK_SILENCE = 5 s`）。
空闲档下心跳频率**不变**（仍 1 s），所以 PC 侧不需要跟着改判活阈值；变的只是音频通知
不发了。若将来把心跳也降频，必须同时放宽 `LINK_SILENCE`，否则会误判「设备消失」。

### Battery Service（电量）

电量来自设备 I2C 总线上的 CW2017 电量计（0x63，与 ES8311 共总线），走 **蓝牙标准**
Battery Service，不占用自定义协议帧：

- 特征值 `0x2A19`（READ + NOTIFY）：1 字节 = 电量百分比（0..100），**裸值不套帧头**。
- 设备每 1 s 采样一次 SOC，只在数值变化时通知；连接后 PC 端主动读一次。
- **CW2017 没有内置电池模型，必须写入 80 字节 profile（BATINFO，`0x10–0x5F`）。**
  profile 缺失时芯片**不会报错**，只会把 SOC 停在 `0xFFxx`：本板实测 `0xFF00 / 0xFEE8`，
  而 VCELL 一直是正常的 3.9 V——KI-007 因此被误判成"电池坏了/芯片没醒"。
  固件开机读 BATINFO 与随固件的官方 UTL 520 mAh profile 逐字节比对，不一致才写：
  切 `CONFIG=0xF0`（sleep）→ 逐字节写 `0x10–0x5F` → 回读校验 → 置 `0x0B` 的 UPDATE 位（`0x80`）
  → `0x30 → 0x00` 重启让芯片重新加载并重算。校验不过**不置** UPDATE 位：
  宁可继续报"不可用"，也不要让芯片带着半份 profile 算出一个看似正常的错误百分比。
  **换电芯必须重新标定**，写错 profile 不会报错、只会给出错误的百分比。
  实现见 `device/main/battery.c` 的 `profile_matches` / `profile_install`。
- **0xFF / ATT 错误 = 电量不可用**（旧固件、电量计缺失，或芯片回报 SOC 无效），
  PC 端不得显示成 0%，**也不得静默隐藏**：GUI 顶栏显示「电量不可用」，
  靠 `AppEvent::Battery { pct: None }` 传递（`pct: Some(0..100)` 才是真百分比）。
  静默隐藏会让人以为这个功能没做——KI-007 现场就是这么误判的。
- **设备端在电量无效时返回 `0xFF`，不回 ATT 错误。** 规范上两种都算"不可用"，
  但 macOS 的 btleplug 遇到 ATT 错误时 `read()` 会永久挂起，PC 端若没给这一步设超时，
  整个连接流程会卡在「连接中」不动（KI-008）。PC 端仍需自带 2 s 超时兜底。
- 低电量阈值 20%（`device/main/battery.h` 的 `APM_BATTERY_LOW_SOC_PCT`）由设备端判定：
  GUI 状态栏高亮提示；设备端背光（GPIO21，LEDC PWM）做**被动查询式**指示——
  低电量时按任意键背光短闪 3 次，平时恒灭（零功耗）。不做常驻呼吸/闪烁：
  "没电了还持续点亮背光"与省电目标冲突，已明确否掉。
- 充电口旁的充电指示灯由充电 IC **硬件直驱**（充电时常亮、充满熄灭），MCU 未接线，
  固件不可控、不可变色；上游 BSP 能力契约也没有 LED 接口。
- 旧固件没有该服务时，PC 端把电量标记为"不可用"并在界面上显示「电量不可用」——
  **不静默隐藏、也不显示 0%**（0% 会被误读成没电），其余链路不受影响。

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
| `0x04` PING | PC → 设备 | `seq u32-LE` | 延迟测量。设备必须原样回 PONG |
| `0x81` STATUS | 设备 → PC | 8 字节 | 1 s 心跳，也可直接读 STATUS 特征值 |
| `0x82` EVENT | 设备 → PC | `btn u8, ev u8, flags u8` | 按键事件 |
| `0x83` PONG | 设备 → PC | `seq u32-LE` | 回显 PING 的 4 字节；长度不是 4 的 PING 直接丢弃 |

PING/PONG 的三条硬约束：

- **PONG 走 EVENT 特征值**（与 `0x82` 共用通道），不新开特征值：它是控制面的回包，
  和音频流无关；新开一个特征值会改动 GATT 表，所有客户端都要重新发现服务。
  PC 端按 `frame.msg` 分派，不能按特征值猜类型。
- **回显而不是回填设备时间戳**：设备没有与主机同步的时钟，回填自己的 tick 只会引入
  "两个时钟怎么对齐"的新问题。PC 端用同一个单调时钟测一次往返，误差只有链路本身。
- **只回 4 字节**：设备不做长度透传，否则它可以被当成任意数据反射器用。

PING/PONG 测到的是**链路往返时延**（含两端协议栈、连接间隔、设备任务调度），
是音频单程时延的上界估计（≈ RTT/2），**不等于**端到端声学延迟 —— 后者还要经过
codec 采集、20 ms 分块、抖动缓冲、声卡播放。口径见 [`SOUND_QUALITY.md`](SOUND_QUALITY.md)。

STATUS 载荷：`state u8, flags u8, dropped u16-LE, sent u32-LE`。

- `state`：`0` IDLE / `1` READY / `2` CAPTURING / `3` ERROR
- `flags`：bit0 BLE 已连接、bit1 AUDIO 已订阅、bit2 PTT 按住、bit3 音频错误、
  **bit4 `LINK_SLOW`**（**采集态**实际连接间隔 > 40 ms，链路撑不住 50 块/秒 —— 这种会话的
  欠载/漂移数字是链路假象，不能当音质结论用，见 [KI-005](KNOWN_ISSUES.md)）

  `LINK_SLOW` **只在 CAPTURING 时才有意义**：空闲档（80 ms）下不置位，否则每次空闲都会被
  PC 判成脏会话。PC 端不需要改判据——它本来就只在推流时才把 STATUS 喂给 `Pipeline`。

  PC 侧对 `LINK_SLOW` 的处置是硬约束，不是建议（2026-11-06 落地）：

  - 一旦在本场会话里见到 bit4，就**粘住整场**（`Pipeline::link_slow`）。慢档期间产生的
    欠载/漂移无法事后剔除，中途恢复正常也不能洗白；
  - `Telemetry.link_slow` 推给界面，指标条显示「链路慢档」并把"欠载/漂移是假象"说清楚；
  - `SessionSummary` / `SessionRecord` 带上该标记，`analyze_session` **直接拒绝出音质报告**
    （错误码 `capture_not_starting`）。给一份"软件分析通过"比不给更糟 —— 它会被当成版本对比基线。
- `dropped` / `sent` 是**本会话**计数（设备在每次 START 或 PTT 首次按下时归零），
  不是自设备启动起的累计值。


### 音频块

- 块头 5 字节：`seq u16-LE`、`predictor i16-LE`、`index u8`。
- nibble 打包：**低 4 位放偶数号采样**，高 4 位放奇数号采样。
- 块头里的 `predictor` / `index` 是**本块编码开始前**的状态。编码器跨块继承，
  解码器每块由块头重置 —— 因此丢一块不会污染后续音频，PC 端可以只靠 `seq`
  判断丢块，不必等链路恢复。
- 块长固定 165 字节，MTU 必须 ≥ 168；固件协商到 247 并把它作为发送前置条件。

### 按键事件

`ev`：`0` PRESS / `1` CLICK / `2` DOUBLE / `3` LONG / `4` RELEASE。

**键位映射只用 `PRESS`**（`keymap-3` 起，一个键一个动作；当前 schema 是 `keymap-4`，
在 v3 上加了两个采集钩子）。`RELEASE` / `CLICK` / `DOUBLE` / `LONG` 仍然照旧上报——
这是跨端协议，改它要动帧格式、UUID 与共享向量，收益只是省几帧通知——但 PC 端不再为它们
提供映射项。

**开麦键不是固定键**：`up` / `down` / `ok` 里恰好一个绑保留动作 `ptt-toggle`，那个键
就是开麦键（默认 `ok`）。这是 PC 端的映射约束，固件不感知——固件只上报"哪个键被按了"。

副作用要知道：**双击 = 两次 `PRESS`**，在 toggle 语义下等于"开了又关"。
PC 端有切换窗口防抖（`apm_app::core` 的 `stream_starting` 占位）：开/关麦进行中（约几百毫秒）
再来的 PRESS 会被忽略，所以落在切换窗口内的双击只算一次动作；窗口外的双击仍是
一次正常的开+关。三键方案的语义代价还在，真要双击语义得单独加事件。
见 [`config/README.md`](../config/README.md)。

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
| `apmctl` | 扫描、状态、录音与音质分析命令行（验收工具） | 已实现 `scan` / `status` / `battery` / `record` / `sink` / `ping` / `probe` / `analyze` / `selftest` / `firmware ports\|probe\|flash` |
 | 虚拟声卡输出 | 把解码后的 PCM 写进 BlackHole / VB-CABLE，带抖动缓冲 | 已实现 `desktop/crates/sink`（cpal 0.18.2，40–80 ms 自适应抖动缓冲）；已验证到会议软件端到端可用，回环探针受麦克风权限限制 |
| GUI 应用 | 设备状态、按键映射、虚拟声卡选择、实时电平与音质报告 | G0–G5 已落地 `desktop/crates/gui`（Tauri 2 + Vue 3，五个 tab）；G3b/G6 未做；真机验收见 [`GUI.md`](GUI.md) §10 |
| `app` 内核 | 状态机、设备发现/连接编排、指标聚合、事件广播、诊断日志 | 已实现 `desktop/crates/app`（`apm-app`，**不依赖任何 GUI 框架**）：`detect_adapter` / `connect` / `start_stream` / `stop_stream` / 采集解码写声卡全在内核（`core` / `session` / `pipeline`）；GUI 只调它。**CLI 只有 `firmware` / `analyze` / `selftest` 走内核**，`scan` / `record` / `sink` / `ping` / `status` 仍直接调 `apm_link`（[`GUI.md`](GUI.md) 决定 D1 与 §14.9 的补充说明，待重构） |
| `firmware` | USB 设备枚举、版本比对、分段烧写、回读校验 | 已实现 `desktop/crates/app/src/firmware.rs`（用 `espflash` 4.7 作库，不调外部命令、不需要装 ESP-IDF）；单元测试覆盖镜像解析、分区表守卫、MD5 向量；**真机烧写 2026-11-06 已通**（检测 + 写入 + FlashMd5 校验），空片首烧仍未验。见「固件烧写与版本比对」 |

跨端一致性不靠"两份实现看起来一样"：`device/test/gen_vectors.c` 从 C 实现导出
`config/protocol-vectors.json`，Rust 的 `tests/c_vectors.rs` 读同一份文件逐字节比对
（CRC、帧、ADPCM 编码与跨块状态）。改了 `device/main/protocol.c` 必须重跑
`scripts/gen-protocol-vectors.sh`，否则对拍会用旧向量。

`device/main/protocol.c` 已按上面的契约实现了帧、CRC 与 ADPCM（编码 + 解码，解码用于主机
单测和后续 PC 端对照）。PC 端的 Rust 实现必须能通过同一组测试向量，否则两端不算对齐。

> 实现约束：桌面端**不要**引入语音识别、文字注入这类功能——它们不在第一版范围内
> （见 `docs/PRODUCT.md` 的非目标）。**固件烧写是例外**：它属于开箱流程，走 USB 而不走
> BLE，见下面的「固件烧写与版本比对」。

## 音频输出策略

目标不是让设备直接出现在系统麦克风列表，而是：

```text
AI Passport -> PC app -> BlackHole/VB-CABLE -> Zoom/Meet/OBS
```

这样第一版可以控制采样率、抖动缓冲和丢块策略，且不依赖系统蓝牙音频栈。

## 固件烧写与版本比对

裸片出货意味着 PC 软件要自己完成首烧（口径见 [`PRODUCT.md`](PRODUCT.md) 的「开箱体验」）。
这条路和 BLE 完全无关：它走 **USB**，用的是芯片 ROM 里固化的 bootloader，
设备哪怕一片空白也能烧。

### 为什么是 USB 而不是 BLE OTA

| 方案 | 结论 |
| --- | --- |
| USB（esptool 协议） | **采用**。ROM bootloader 在芯片内，空片可用；约 0.7 MB 的镜像在 921600 波特率下 10–20 s 量级 |
| BLE 自定义 OTA | 不采用。前提是设备已有能跑起来的固件（裸片没有）；要双分区；分钟级；断连即失败 |
| 只做升级不做首烧 | 不成立。裸片没有任何固件，OTA 的前提不存在 |

### 硬件前提（不满足则整个体验假设作废）

- 板子的 USB 口必须接 ESP32-C3 的**原生 USB** D-/D+（GPIO18/19），不是外置串口芯片。
- 固件侧已开 `CONFIG_ESP_CONSOLE_USB_SERIAL_JTAG`（见 `device/sdkconfig`）。
- 设备枚举为 VID `0x303A` / PID `0x1001`。这是乐鑫 USB-Serial-JTAG 的标识，
  **不是** CH340（`0x1A86`）或 CP210x（`0x10C4`）。
- 免驱范围：macOS / Linux 原生 CDC-ACM；Windows 10+ 走系统自带 usbser；Windows 7 不支持。
- 烧写不需要按 BOOT：esptool 通过 USB 控制线（DTR/RTS 序列）软复位进下载模式。
  **但这是"通常成立"，不是"物理保证"**——固件把 USB 外设配置乱掉时可能需要手动复位。

### flash 布局与「NVS 不能碰」

| 区域 | 偏移 | 烧写时 |
| --- | --- | --- |
| bootloader | `0x0` | 写 |
| 分区表 | `0x8000` | 写 |
| **nvs（用户配置）** | `0x9000` | **跳过** |
| phy_init（RF 校准） | `0xF000` | **跳过** |
| factory app | `0x10000` | 写 |

**这一条最容易做错。** `idf.py merge-bin` 产出的是从 `0x0` 到 app 末尾的**连续**镜像，
空隙用 `0xFF` 填满。如果拿它从 `0x0` 整段写下去，`0x9000` 的 NVS 与 `0xF000` 的 phy_init
会一起被填成 `0xFF`——用户每插一次设备就丢一次按键映射和蓝牙配对；phy_init 被抹掉后
射频还要重新校准。表现是"插上 USB 就掉配对"，很容易被误判成蓝牙问题。

所以烧写必须**切片**，中间那段一个字都不写：

```text
[0x0000, 0x8000)  → 写到 0x0000    bootloader + 填充
[0x8000, 0x9000)  → 写到 0x8000    分区表 + 填充
[0x9000, 0x10000)                  不写（nvs + phy_init）
[0x10000, EOF)    → 写到 0x10000   app
```

切之前先读镜像 `0x8000` 处的分区表（32 字节一条，magic `0x50AA`，在镜像里按小端存成
`AA 50` 两个字节）核一遍：确实是 `nvs@0x9000` + `factory@0x10000` 才按上面的常量切；
对不上就拒绝烧写。

### 一次会话写完三段，校验要另开一次

两条实现约束，改了就会坏（2026-11-06 真机撞出来的，记在这里防回退）：

1. **三段必须一次 `write_bins_to_flash` 写完。** 它的收尾是 `finish(reboot = true)`，
   每调用一次就复位一次设备；分三次调等于写完 bootloader 就让芯片跑起来，
   第二段的 SPI attach 必然超时。
2. **写完之后的校验必须重开一次连接。** 第一段会话结束时设备已经在跑应用，
   旧连接上发 `FlashMd5` 没人应答（真机报 `Timeout while running FlashMd5 command`）。
   重连会把它拉回下载模式，校验完再复位回应用模式。
3. **退出时一定要复位。** espflash 的 `Flasher` 没有 `Drop`，进了下载模式没人负责退出；
   读失败/写失败/提前 return 都要把设备放回应用模式，否则用户看到的是"设备不见了"。
   实现里用 `ResetGuard` 兜住。

### 串口枚举：一台设备只能出现一次

macOS 上 `serialport::available_ports()` 会把同一个 USB 串口的 callout（`/dev/cu.*`）
和 dialin（`/dev/tty.*`）各返回一条。不去重的话，界面就会"插了一台设备、列表里两台"，
而且拿 dialin 那条去连必然失败（它等载波，不是给 esptool 用的）。
`firmware::list_ports()` 按 VID/PID + 序列号去重并优先保留 `cu`。
**不要**在分区表变了之后还按旧常量硬切。

**本机实测（2026-11-06，`device/build/ai-passport-mic-merged.bin`）**：镜像总长
`0xAFE40`（720448 字节），`0x9000`–`0xFFFF` 整段是 `0xFF` 填充（nvs 与 phy_init 的位置）。
"整段写会清配置"不是理论风险，是每次都会发生。

### 版本比对（不改协议、不要求设备有固件）

- 每个 app 镜像在**镜像内偏移 `0x20`** 处有 `esp_app_desc_t`，magic `0xABCD5432`
  （小端字节序 `32 54 CD AB`）。
- `version[32]` 在描述符内偏移 `0x10`，即 **flash 绝对地址 `0x10030`**
  （`0x10000` app 分区 + `0x20` 描述符 + `0x10` 版本字段）。
- 读设备这 32 字节，与随包镜像同位置比对，就能判断"要不要烧"。
  **不需要**新增 BLE 命令，也不需要设备能跑起来。
- 读设备**必须走 `read_flash`（stub 路径）**，不能用 `read_flash_rom`：
  后者是 ROM 专用命令，stub 起来之后直接被拒（真机报 `The bootloader returned an error`）。
- 读不到、全 `0xFF`、magic 不对 → 按"空片或未知"处理，转烧写。

前提是 `version` 必须可比较。ESP-IDF 默认用 `git describe` 填这个字段，历史产物是
`7552506-dirty`——带 git hash 和 `-dirty`（说明构建时工作区有未提交改动），既不能对外
显示，也没法做版本序比较。**已在 `device/CMakeLists.txt` 固定成 semver**
（`set(PROJECT_VER "0.1.0")`）。改版本号要同步 `docs/ROADMAP.md` 的 M6 段并重跑
`./scripts/build-firmware.sh`。

**本机实测（2026-11-06，固定 `PROJECT_VER=0.1.0` 后重建）**：

| 检查项 | 结果 |
| --- | --- |
| `merged[0x10020..0x10024]` | `32 54 CD AB`（magic 正确） |
| `merged[0x10030..0x10050]` | `0.1.0` |
| `merged[0x10000..]` 与单独构建的 `ai-passport-mic.bin` | 逐字节相同（654912 字节） |

也就是说**版本字段和 app 段都能纯离线从镜像里读出来**，不需要连设备。

### 烧写后的校验

写完必须**校验 app 分区的实际内容**，不能信任写操作的应答：供电不稳、劣质线材、
无源 HUB 分线都可能静默丢字节，而 esptool 的应答早就返回了。

实现用 esptool 的 `FlashMd5`：让设备自己算 app 分区的 MD5，与本地对同一段数据算的
MD5 比对。不用"整段回读再算"是因为回读 650 KB 要十几秒，而 `FlashMd5` 只要一条命令——
要验的不是"能不能读"，是"写进去的对不对"。

校验不通过就报"未完成"，不允许报"已完成"让用户接着去连蓝牙。

**真机验证（2026-11-06，ESP32-C3 实板）**：`apmctl firmware probe` 读出设备
`0.1.0`、`apmctl firmware flash` 三段写入 5.0 s、FlashMd5 一致，写完后设备自行
复位回应用模式并恢复 BLE 广播。

### 失败与恢复

- 烧写中途断线/断电：设备回到 ROM 下载模式，重插可继续。**不做一次性状态**，
  也不要把"失败"写成终态。
- 校验失败：明确提示重试，并保留设备仍在下载模式这一事实（用户不用拔插）。
- **不提供 chip erase 入口**：整片擦除会连 NVS 与 RF 校准一起清掉，那是维修动作，
  不是开箱动作。要清用户配置必须是独立的、用户显式点的"恢复出厂设置"。

### 与 GUI 的关系

烧写逻辑放 `apm-app::firmware`（无 UI，CLI 也能用），GUI 只负责显示状态、进度、结果。
按 [`GUI.md`](GUI.md) 的决定 D1，不允许在 `crates/gui` 里再写一套。

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
| PC 抖动缓冲 | 目标 40–80 ms，**实测存量 p95 才是验收口径** |
| 虚拟声卡缓冲 | 10–40 ms |
| 稳态合计 | p95 ≤ 150 ms |
| OK 按下到首块音频 | p95 ≤ 250 ms |

这些是验收目标，不是当前实测值。必须在真实设备、目标主机和虚拟声卡组合上测量。

**"抖动缓冲"这一项必须用实测存量，不能用滑块上的目标值。** 原因有两条：

1. 延迟守卫（[`playout.rs`](../desktop/crates/sink/src/playout.rs) 的 `TRIM_OVER_MS`）
   会把稳态存量留在"目标 + 余量"附近。目标 80 ms 时实测中位约 97 ms
   —— 拿 80 去填这张表会系统性低估二十多毫秒。
2. 攒批交付（见下）意味着缓冲必须按**批间空档**而不是块周期来定，
   "目标"只是控制器的一个参数，不是它实际维持的水位。

所以 `SessionSummary` / `Telemetry` 里同时给三组数，缺一不可：

| 字段 | 含义 |
| --- | --- |
| `base_target_ms` / `target_start_ms` | **用户选的**基准（滑块上的数）。自适应不改它 |
| `boost_ms` / `target_boost_max_ms` | 自适应**临时**加上的余量，会话内峰值 |
| `buffer_p50_ms` / `buffer_p95_ms`（`fill_p50_ms` / `fill_p95_ms`） | **实测存量**分位数。延迟结论只能引这一组 |

**自适应不得改写用户选的基准。** 用户的"把缓冲调到 80 ms"必须一直是 80 ms；
自适应只能在它之上临时加余量、稳定后再收回。旧实现直接改目标值，
于是跑稳 5 s 后目标会一路降到 40 ms，而界面上仍显示 80 —— 那既是谎报，也让
下一次抖动立刻欠载。

## 功耗与省电

设备是 500 mAh 电池供电。**当前没有任何功耗实测数字**（`TODO.md` / `ROADMAP.md`
的「功耗与热测」仍未做），所以下面这些是**设计选择**，不是实测结论。

### 已开启的（2026-11-07）

| 项 | 配置 | 为什么 |
| --- | --- | --- |
| BLE modem sleep（Mode 1） | `CONFIG_BT_CTRL_MODEM_SLEEP=y` + `BT_CTRL_LPCLK_SEL_MAIN_XTAL` | 事件之间关 RF/PHY/BB。这是最大的一项 |
| 空闲连接间隔 80 ms | 见上「连接参数」 | 射频唤醒次数降到 1/3 左右 |
| CPU DFS 160 / 80 / 40 MHz | `CONFIG_PM_ENABLE=y` + `esp_pm_configure` | 空闲降频。**不开 tickless**，见下 |
| ES8311 只开 ADC | `WORK_MODE_ADC` + `DEV_TYPE_IN` | 本项目只采集不播放，DAC/输出级是白烧（省的是模拟侧，不是 I2S TX 通道——duplex 下它关不掉） |
| 发射功率按 RSSI 自适应 | −3 / +3 / +9 dBm 三档，5 s 评估一次 | 缩短不必要的射程。**不是省电主力**，见下 |
| 电池采样 1 s | `battery_task` 只在背光闪烁时切到 50 ms，按键用 `xTaskNotifyGive` 立刻唤醒 | 原来恒 20 Hz 轮询，纯浪费 |

**LPCLK 取 main XTAL 而不是外部 32k 晶振**：本板 `CONFIG_RTC_CLK_SRC_INT_RC=y`，
没有外部 32k 晶振的证据。用 main XTAL 的代价是 light sleep 期间主晶振保持上电
（收益比 32k 小），但不会因为"选了 32k 而板上没有"退化成不可预期状态。
若将来确认板上有 32k 晶振，换成 `BT_CTRL_LPCLK_SEL_EXT_32K_XTAL` + `RTC_CLK_SRC_EXT_CRYS`
能再降一档。

**发射功率按 RSSI 自适应**：`link.c` 每秒读一次 `ble_gap_conn_rssi()`，带滞回分三档
（−3 / +3 / +9 dBm）。近距离降到 −3 dBm 省射频电流，远了自动升回去。
**注意这不是省电主力**：连接间隔 30 ms、每周期 1–2 个 165 字节包，发射占空比只有
百分之几，降功率对平均电流的影响是 mA 量级。它的真实价值是"缩短不必要的射程"。

### 明确不做的，以及为什么

| 不做的项 | 原因 |
| --- | --- |
| **`FREERTOS_USE_TICKLESS_IDLE` / light sleep** | 按键是 10 ms 硬轮询（去抖要连续 2 次采样），可睡窗口 ≤8 ms，进出 light sleep 的开销会吃掉大半收益。PTT 手感优先 |
| **空闲关 ES8311 + I2S** | 能让空闲时真正进 light sleep，但开麦延迟会变大，且 KI-001 的满量程毛刺候选原因里就有 codec 时钟瞬态。属于单独一轮的工作，不是这次的范围 |
| **deep sleep** | 三键共用一个 ADC 分压进 GPIO0，**没有可用的 GPIO 中断唤醒源**，只能定时唤醒，比 light sleep 好不了多少。要真休眠得改硬件 |
| **屏幕** | 第一版不驱动，背光 GPIO21 恒 0（`battery.c` 的 `bl_duty(0)`）。ST7789 本身的静态漏电固件管不了 |

**这些取舍是可逆的**：`PM_ENABLE` + DFS 已经落地，将来若决定关 codec，只要在
`audio_task` 的空闲分支加 `esp_codec_dev_close()`，light sleep 会立刻可用
（省电效果随之跃升）。不需要重做架构。

### 2026-11-07 真机实测（两档连接参数）

| 状态 | PING RTT 中位 | 说明 |
| --- | --- | --- |
| 已连接空闲（请求 80 ms） | **117–118 ms** | ≈1.5 个 80 ms 间隔 |
| 采集中（请求 15–30 ms，落在 30 ms） | **58.8 ms** | ≈2 个 30 ms 间隔 |

**两档确实都生效了**（比例与间隔比一致）。

⚠️ `KI-003` 记的历史采集态 RTT 是 **38.2 ms**。本次实测 58.8 ms 更大，但做了对照：
**把改动全部 `git stash` 掉、重建基线固件、烧回设备、跑同样的 600 s，RTT 仍是 58.9 ms**。
所以这个差异来自主机侧当天状态，不是本次改动。**下次看到 RTT 数字变化，先做基线对照。**

另外用对照实验排除了 modem sleep 的影响：关掉它之后采集态 RTT 是 58.2 ms
（开着 58.8 ms），说明唤醒开销不体现在这个量级上。

### 怎么验（改任何一项都要跑）

省电的全部决策逻辑都在 `device/main/power_policy.c`——**它不依赖 ESP-IDF**，
所以能在主机上直接单测（`device/test/test_power_policy.c`，65 项，随
`./scripts/test-protocol.sh` 一起跑）。这样做的理由：这些判断原先埋在 NimBLE 回调与
FreeRTOS 任务里，主机侧编不起来，2026-11-07 落地那批改动等于"没有任何测试能跑"。
改判据时先改纯函数 + 补测试，再动 `link.c` / `app_main.c` 的胶水。

1. `./scripts/verify.sh` —— 结构/内链/**固件主机侧单测（协议 44 + 省电 65）**/向量/`cargo test`
2. `./scripts/test-longrun.sh` —— **3 轮 × 600 s**，欠载看**中位数**、块率与链路干净性看**每一轮**。
   **改了连接参数必须跑这一条**：空闲档切回快速档若慢了，会表现为开流前几秒欠载。
   判据为什么用中位数见 [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) KI-006 ——
   单轮方差极大（同条件 1.0 → 8.5 次/分），单轮结果不构成结论。
   `--from-logs "logs/*.log"` 可以只判读已有日志、不重跑（换判据复核历史跑分、或对判据做离线验证）；
3. `apmctl record` + `analyze` —— 比对 `glitch_events`。KI-001 根因未定，
   任何与时钟/供电相关的改动都要重新看这个数；
4. `apmctl ptt-latency` —— 「OK 按下到首块音频」。**注意它依赖两个前提**：
   固件在采集态上升沿丢块（已实现），且工具把 PC 端队列排空到真正静默
   （否则会量出 1 ms 的假值 —— 7 µs 内到达的块不可能是刚走完 BLE 的）；
5. 功耗本身：三个场景各量一次平均电流（未连接广播 / 已连接空闲 / 采集中），
   填进 [`ENVIRONMENT.md`](ENVIRONMENT.md) 的功耗基线表。

## 可靠性约束

- **抖动诊断的计时点必须在链路层，不能在被诊断的消费循环里。** 音频通知带
  `apm_link::AudioBlock` 的三个量：`received_at`（btleplug 交出即打点）、
  `read_waited`（这次问下一条等了多久）。`Pipeline` 据此算三件事：
  - **到达间隔** = 相邻 `received_at` 之差（回答"数据来得多不匀"）；
  - **PC 处理耗时** = `received_at` → 解码完成（回答"我们处理得多慢"）；
  - **本地停顿** = 到达间隔 − `read_waited`（回答"这一刻我们自己花掉了多少"）。

  三者必须分开报。只报到达间隔会把自己的停顿记到链路头上；只报处理耗时又会漏掉
  "两次读取之间卡住"这种情形（2026-11-07 的 KI-006 先后被这两个坑各骗过一次）。
- **"停顿归谁"要靠 RTT 与音频时间对齐**，不能靠猜。`apmctl sink --rtt` 在同一条连接、
  同一时间轴上每 500 ms 发 PING：若尖端期间 RTT 平稳而音频晚到，说明链路通畅、
  晚的只是音频通知这一条路（射频/连接间隔/带宽都被排除）。实测 0.57–1.00×。
  **不要并行开第二个 `apmctl` 进程来测 RTT**：设备只支持 1 条 BLE 连接，
  且 `Link::ping()` 会自己消费通知流、把音频吃掉。
- **核心事实：macOS 的 CoreBluetooth 是"攒批交付"，不是"逐块交付"**（2026-11-07 实测）。
  细粒度到达间隔呈极明显双峰：相邻块 <5 ms 占 38–43%，而"正常 20 ms 一块"的区间
  占 **0%**。一批里几块几乎同时到，批与批之间隔 25–30 ms。
  推论（写代码时必须知道）：
  - 平均块率会非常精确（50.000/s），**但这不代表交付均匀**；
  - 任何"按 20 ms 均匀到达"设计的缓冲都会周期性欠载 —— 抖动缓冲必须按
    **批间空档**（实测 100–400 ms）而不是块周期来估算需求；
  - 这不是本项目的 bug，换连接参数或抬缓冲都改不了它。
  详见 [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) KI-006 的「同日第四轮」。
- **日志落盘不得阻塞调用方。** `AppCore::record` 只做有界内存入队，落盘在
  `apm-log` 写线程里。它会在音频通知循环里被调用，同步写盘等于把磁盘长尾加进
  音频路径与抖动测量。
- **改配置不得在持有内核锁时写磁盘。** `AppCore::inner` 是内核唯一的锁，BLE 通知
  循环也要拿它 —— 在锁内 `Settings::save()` 等于让音频块排在 fsync 后面。
  统一走 `persist(settings 快照, source)`：**锁内只改内存并克隆一份，锁外落盘**。
- **音频路径上不做每块分配。** 解码后的 PCM 走缓冲池（`session::PcmPool`）：
  生产者借槽位、声卡线程用完归还；池空时才退回新分配。
  50 块/秒 × 20 分钟 ≈ 6 万次小块 `malloc/free`，全部发生在 BLE 通知任务上，
  而这条线程的分配停顿会直接表现为"块到达间隔变长"—— 正是 KI-006 要区分的那件事。
- **欠载必须按归因分开计数。** `underruns` 是总数，其中
  `underruns_underfed` 是"距上次推入 > 30 ms 仍读空"的次数（上游断供）。
  剩下的才是"消费端读得比生产快"。两者的处置完全不同：
  - 消费型 → 抬缓冲目标有效；
  - 断供型 → 抬缓冲无效，要查链路到达与 PC 端自身停顿。

混成一个数字时，抬缓冲会让断供型也"变好一点"，于是真因被永久掩盖
—— KI-006 曾因此绕了两轮。
- **跨 await 的状态迁移必须校验链路代次。** `Inner::link_epoch` 在每次建立新链路时 +1、
  在 `teardown` 入口 +1。所有"入口拍快照 → 中间 await → 回来落状态"的路径
  （`start_stream` / `stop_stream_with_reason` / 连接失败回退）都必须用
  `set_state_if_link(epoch, ..)` 落状态，**不得**在 await 之后直接 `set_state`。

  不这么做的后果是**陈旧写**：开流在 `CTRL START` 的往返窗口里被断链收尾插进来，
  收尾把状态落到 `idle`，开流回来又把状态写回 `Ready` —— 界面显示"已连接"，
  但 `link` 是 `None`，点开麦必然失败；而 `disconnect()` 看到 `link_stop` 与 `link`
  都空，什么都不做，用户只能重启 App。
- **开流必须有并发占位。** `Inner::stream_starting` 在入口校验的同一把锁里置位，
  覆盖"STOP → 等 120 ms → 开声卡 → START"整段窗口。这段时间状态仍是 `ready`、
  `stream` 仍是 `None`，没有占位就会有两条 `Pipeline` 被创建：后写入的覆盖先写入的，
  而被覆盖的那个在 async 上下文里同步 `join` 声卡线程，阻塞 tokio worker，
  声卡还会被打开两次。占位**只能由通过校验的那个调用撤销**，
  被拒的调用不得清除它。
- **"收到通知"与"链路安静"是两件事。** `next_notification` 的返回值语义：
  `Ok(Some(_))` = 收到东西（含解析失败）、`Ok(None)` = 这段窗口真的没有通知、
  `Err(_)` = 通知流结束。解析失败（CRC 错、载荷长度不对、电量值越界、未知特征值）
  必须返回 `Ok(Some(Notification::Malformed { .. }))`，**不能**退化成 `Ok(None)`。

  `monitor_loop` 靠 `Ok(None)` 决定"静默 5 s 就探活"，所以把坏帧算成安静会让
  设备明明在发却被误判为消失，同时"设备在发但帧坏了"这个现象在日志里彻底消失。
- 音频块按序号检测丢失和乱序；报告记录丢块数，不用“音频能播”掩盖链路质量。
- 设备断连后 PC 端必须停止向虚拟声卡写入旧缓冲，避免会议软件持续收到静音伪装成在线。
- 设备与声卡各有一颗晶振，"16 kHz" 不是同一个数（实测 20 s 差约 84 ms）。PC 端必须做
  长期漂移补偿：按**墙钟 + 推入帧数**估计漂移，再按该速率做单样本插删（上限 1%），
  **不允许**靠周期性欠载或整帧丢弃兜底。实现见 `desktop/crates/sink/src/playout.rs`。
  **不能用缓冲存量估漂移**：CoreAudio 每隔几秒会停一下回调（实测 3–4 s 一次、几十毫秒），
  存量随之堆高，用存量估漂移就会把"消费端停顿"误判成"设备时钟快"，
  进而用删除真实音频的方式去纠正一个已经过去的事件（2026-11-06 实测：10 min 删掉
  39347 帧 ≈ 2.5 s 人声，而真漂移是 0）。停摆积压另由**延迟守卫**一次性丢弃旧数据，
  与漂移补偿是两件事。
- 采集端偶发满量程逐样本变号的瞬态读出，两端各拦一次：固件编码前压成静音（
  `apm_pcm_is_corrupt`），PC 端解码后再兜一层（`apm_protocol::glitch`）。判据与口径见
  [`SOUND_QUALITY.md`](SOUND_QUALITY.md) 的「毛刺」。
- 快捷键注入用 CGEvent 的"完整事件 + flags"一次性发出（修饰键写在事件 flags 里），
  不单独发修饰键按下/抬起，因此不存在"修饰键按下后没释放"的残留状态；
  设备侧 PRESS 走点按（down 紧接 up），hold 语义仍未实现。
- 修改设备名时保留旧蓝牙名别名，避免已配对设备因名称缓存失效。
- 固件构建与真机验证分开报告；没有硬件证据不得宣称通过。
