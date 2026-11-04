# AI Passport Mic

把 FoloToy AI Passport（ESP32-C3）做成一个**纯无线麦克风**：设备采集声音，通过 BLE
送到 PC，由 PC 端输出到虚拟声卡，会议软件选择该虚拟声卡作为麦克风。第一版不包含语音
识别、文字输入或云端 ASR。

> **当前状态：初始化基线。**
> 这个仓库目前只有产品/架构/环境/音质文档、按键映射样例和三个脚本，
> **没有固件代码，也没有桌面端代码**。`device/` 与 `desktop/` 是空的骨架目录，
> 各自带一份 README 说明计划怎么落地。
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
| [`device/`](device/) | 固件工作区（ESP-IDF 5.5.3 / ESP32-C3 / 8 MB Flash / 无 PSRAM） | 空骨架，见 `device/README.md` |
| [`desktop/`](desktop/) | PC 软件工作区（Rust + Tauri 2 + Vue 3 方向） | 空骨架，见 `desktop/README.md` |
| [`config/`](config/) | 按键映射样例与字段说明 | 样例 + 说明（无代码消费） |
| [`docs/`](docs/) | [产品](docs/PRODUCT.md)、[架构](docs/ARCHITECTURE.md)、[环境](docs/ENVIRONMENT.md)、[音质](docs/SOUND_QUALITY.md)、[路线图](docs/ROADMAP.md)、[参考实现](docs/REFERENCES.md) | 已就绪 |
| [`scripts/`](scripts/) | 环境检查、ESP-IDF 激活、初始化校验 | 可用 |

## 环境

本机实测的宿主环境：macOS 15.3.1 arm64、Xcode CLT、Rust 1.95、Node.js 22、Python 3.13。
固件目标工具链固定为 **ESP-IDF v5.5.3**。

```bash
# 1. 检查本机工具与 ESP-IDF 版本（只读，不修改任何东西）
./scripts/check-env.sh

# 2. 在当前 shell 激活 ESP-IDF
source ./scripts/activate-idf.sh

# 3. 初始化基线自检：结构、文档内链、脚本可执行位、环境快照
./scripts/verify.sh
```

默认 ESP-IDF 路径是 `$HOME/esp/esp-idf-v5.5.3`；装在别处就设置 `IDF_PATH` 后再 source。
**注意**：ESP-IDF 的 python 虚拟环境与 `python3` 小版本绑定（`idf5.5_py3.13_env`），
升级系统 python 后需要重跑 `install.sh`。排查方式见 [`docs/ENVIRONMENT.md`](docs/ENVIRONMENT.md)。

## 按键映射

示例配置见 [`config/button-map.example.json`](config/button-map.example.json)，
字段说明见 [`config/README.md`](config/README.md)。映射支持 UP / DOWN / OK 的短按、
双击、长按、按下、抬起，以及 Ctrl / Shift / Alt / Win 修饰键。OK 的按下和抬起默认是
PTT 采集开关，不能在没有替代停止机制时移除。

> 该 schema 目前只有文档与样例，**尚无代码实现**；实现时以 `config/README.md` 为准，
> 不要静默改动字段名。

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
