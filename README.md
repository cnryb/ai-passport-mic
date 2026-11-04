# AI Passport Mic

把 FoloToy AI Passport（ESP32-C3）做成一个**纯无线麦克风**：设备采集声音，通过 BLE
送到 PC，由 PC 端输出到虚拟声卡，会议软件选择该虚拟声卡作为麦克风。第一版不包含语音
识别、文字输入或云端 ASR。

> **当前状态：M1 固件真机跑通采集与 BLE 链路；M2 主链路打通——虚拟声卡能当麦克风用；
> GUI 只到 G0（骨架 + 权限 + 设备发现）。**
> `device/` 是完整的 ESP-IDF 工程：ES8311 采集、IMA-ADPCM 编码、自定义 GATT 传输、
> 单 ADC 三键手势判定。能编译、能烧录、能在真机上连续采集。
> `desktop/` 有协议层、BLE 链路、虚拟声卡输出和 `apmctl` CLI（scan / status / record /
> sink / probe / analyze / selftest），协议与固件用同一份共享向量逐字节对拍。
> 真机已验证：烧录、ES8311 16 kHz 采集、MTU 247、scan / status / record / sink，
> 以及**会议软件里当麦克风用**（虚拟声卡输出已实现并实测）。
> 仍未验证：按键手势逐项核对、端到端延迟（缺 PING/PONG）、长时间（>10 分钟）稳定性、
> GUI 首次蓝牙授权。已知未解：20 s 约 84 ms 时钟漂移、偶发满量程毛刺。
> 注意 macOS 上 BLE 必须走 .app bundle 拿权限，见 [`desktop/README.md`](desktop/README.md)。
>
> 上游的 FoloToy AI Passport 固件、以及既有的 VibeMic 桌面实现只作为**设计参考**，
> 不整份拷贝进本仓库（约定见 [`docs/REFERENCES.md`](docs/REFERENCES.md)）。

## 第一版边界

| 能力 | 第一版决定 |
| --- | --- |
| 音频传输 | BLE 自定义 GATT，16 kHz / 16-bit / mono，IMA-ADPCM，20 ms 一块 |
| PC 输出 | 写入系统虚拟声卡（macOS: BlackHole / Loopback；Windows: VB-CABLE） |
| 设备按键 | UP / DOWN / OK 可配置快捷键；OK 的按下/松开保留为 PTT |
| 音质评估 | WAV 客观指标 + 固定场景录音 + 主观评分 + 延迟/稳定性记录 |
| 麦克风即系统输入 | 通过虚拟声卡实现，不在第一版实现蓝牙 HFP 或系统音频驱动 |

**不在第一版范围**：本地/云端语音识别、自动打字、AI Agent、Wi-Fi 推流、标准蓝牙耳机
麦克风协议、iOS/Android App。

## 目录

| 路径 | 内容 | 状态 |
| --- | --- | --- |
| [`device/`](device/) | 固件工作区（ESP-IDF 5.5.3 / ESP32-C3 / 8 MB Flash / 无 PSRAM） | M1 已实现并真机跑通（按键手势未逐项核对）；见 `device/README.md` |
| [`desktop/`](desktop/) | PC 软件工作区（Rust workspace：protocol / link / sink / app / cli / gui） | 协议 + 链路 + 声卡输出 + CLI 已实现；GUI 到 G0，见 `desktop/README.md` |
| [`config/`](config/) | 按键映射样例、字段说明、跨端协议向量 | 样例 + 说明 + `protocol-vectors.json`（由 C 实现导出） |
| [`docs/`](docs/) | [产品](docs/PRODUCT.md)、[架构](docs/ARCHITECTURE.md)、[环境](docs/ENVIRONMENT.md)、[音质](docs/SOUND_QUALITY.md)、[路线图](docs/ROADMAP.md)、[参考实现](docs/REFERENCES.md)、[GUI 方案](docs/GUI.md) | 已就绪 |
| [`scripts/`](scripts/) | 环境体检、IDF 激活、基线自检、协议单测与向量导出、固件构建、GUI 打包、macOS app-bundle 运行器 | 可用 |

## 环境

本机实测的宿主环境：macOS 15.3.1 arm64、Xcode CLT、Rust 1.95、Node.js 22、Python 3.13。
固件目标工具链固定为 **ESP-IDF v5.5.3**。

```bash
# 1. 检查本机工具与 ESP-IDF 版本（只读，不修改任何东西）
./scripts/check-env.sh

# 2. 在当前 shell 激活 ESP-IDF
source ./scripts/activate-idf.sh

# 3. 基线自检：结构、文档内链、脚本可执行位、协议单测、环境快照
./scripts/verify.sh

# 4. 只验证协议层（不需要 ESP-IDF）：CRC / 控制帧 / ADPCM 共 27 项
./scripts/test-protocol.sh

# 5. 桌面端：协议层 + 与固件的逐字节对拍
cd desktop && cargo test
```

本机如果报 `idf5.5_py3.13_env not found`，先按 [`docs/ENVIRONMENT.md`](docs/ENVIRONMENT.md)
的「方案 A」把 `IDF_PYTHON_ENV_PATH` 指向已有的 `idf5.5_py3.11_env`，再 source。

默认 ESP-IDF 路径是 `$HOME/esp/esp-idf-v5.5.3`；装在别处就设置 `IDF_PATH` 后再 source。
**注意**：ESP-IDF 的 python 虚拟环境与 `python3` 小版本绑定（`idf5.5_py3.13_env`），
升级系统 python 后需要重跑 `install.sh`。排查方式见 [`docs/ENVIRONMENT.md`](docs/ENVIRONMENT.md)。

## 按键映射

示例配置见 [`config/button-map.example.json`](config/button-map.example.json)，
字段说明见 [`config/README.md`](config/README.md)。映射支持 UP / DOWN / OK 的短按、
双击、长按、按下、抬起，以及 Ctrl / Shift / Alt / Win 修饰键。OK 的按下和抬起默认是
PTT 采集开关，不能在没有替代停止机制时移除。

> 两端代码都有了：固件把按键手势编码成 BLE 事件发出去，PC 端 `apm-protocol` 能解析并
> 校验映射表（含 PTT 约束）。**但还没有把事件翻译成实际按键注入** —— 那是 M3 的事。
> 实现时以 `config/README.md` 为准，不要静默改动字段名。

## 声音质量评估

音质不能只看“能听见”。第一版采用客观指标与真实场景测试结合，方法与阈值见
[`docs/SOUND_QUALITY.md`](docs/SOUND_QUALITY.md)：

- 语音 RMS 建议 `-30 ~ -12 dBFS`，峰值不高于 `-1 dBFS`。
- 削波样本比例小于 `0.1%`，直流偏置绝对值小于满量程的 `1%`。
- 记录 ADPCM 序号丢块数；稳态端到端延迟目标 p95 ≤ `150 ms`。
- 安静房间、办公室、背景音乐三种环境，20 cm / 50 cm / 1 m 三种距离，0° / 45° / 背向三种角度。

## 技术架构

```text
AI Passport (ESP32-C3)
  microphone -> 16 kHz PCM -> IMA-ADPCM -> BLE GATT notify
                                      \-> BLE HID button events
                                                   |
                                                   v
PC: 设备链路 -> ADPCM 解码 -> 虚拟声卡 -> 会议软件
                            \-> WAV 录制 / 音质报告
```

协议帧、BLE 服务 UUID 与延迟预算见 [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)。
这些是**设计决定**，实现时必须与文档一致；改协议先改文档。

## 开发约定

1. **不整份拷贝上游仓库**。参考实现只用来对齐协议、硬件引脚和踩坑经验，代码按本项目
   自己的结构重写。
2. 新代码落地时，同时更新对应子目录的 `README.md` 与本文的目录表。
3. 交付时如实区分「已验证」与「未验证」：编译通过不等于真机通过。
4. 不在仓库里提交凭据、私钥、个人信息或未脱敏日志。

## 许可证

仓库整体采用 MIT 许可证（见 [`LICENSE`](LICENSE)）。若日后参考或复用上游
FoloToy AI Passport 的代码，需在相应目录内保留其 MIT 许可原文与版权声明。
