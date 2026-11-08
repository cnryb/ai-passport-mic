# device/ —— 固件（M1 已实现，真机链路已验证）

ESP32-C3 上的采集与 BLE 传输固件。**代码已落地，能编译、能烧录，采集与 BLE 链路已在
真机上跑通**；三键的按下判定只间接验过一次（按 OK 能在 PC 侧看到 PRESS），
UP / DOWN 还没逐项核对过。逐条证据见文末「验证状态」，
未验证的项在那里明确标着「未验证」，不要当成"已通过"。

## 技术栈

| 项 | 值 |
| --- | --- |
| 框架 | ESP-IDF **v5.5.3**（固定版本） |
| 语言 | C |
| 目标 | ESP32-C3，8 MB Flash，无 PSRAM |
| BLE | NimBLE，单连接 peripheral，自定义 GATT（CTRL / PROFILE / STATUS / EVENT / AUDIO） |
| 控制台 | USB-Serial-JTAG（C3 原生 USB）；UART0 的 TX=GPIO21 与本板背光冲突所以不用 |

## 目录

```text
device/
  CMakeLists.txt
  partitions.csv          # nvs + phy_init + factory（无 OTA）
  sdkconfig.defaults      # 目标芯片、8MB Flash、USB-Serial-JTAG、NimBLE 基线
  main/
    idf_component.yml     # esp_codec_dev 1.6.2
    CMakeLists.txt
    app_main.c            # 任务编排：采集 / 事件 / STATUS 心跳
    protocol.c/.h         # 帧、CRC16-CCITT、IMA-ADPCM（纯 C，可在主机单测）
    audio.c/.h            # ES8311 + I2S 全双工，16 kHz / 16-bit / mono；共享 I2C 总线
    battery.c/.h          # CW2017 电量计 + 低电量背光呼吸指示（阈值 20%）
    link.c/.h             # NimBLE GATT、广播、MTU 协商、通知、标准 Battery Service
    buttons.c/.h          # 单 ADC 三键判定（去抖 / 单击 / 双击 / 长按 / 按下抬起；PC 只消费 PRESS）
    board.h               # 引脚与硬件常量（唯一事实来源）
  test/
    test_protocol.c       # 主机侧单测，不依赖 ESP-IDF
```

## 构建

```bash
export IDF_PYTHON_ENV_PATH="$HOME/.espressif/python_env/idf5.5_py3.11_env"  # 见 docs/ENVIRONMENT.md 方案 A
source ../scripts/activate-idf.sh
idf.py set-target esp32c3     # 只需一次
idf.py build
idf.py merge-bin -o "$PWD/build/ai-passport-mic-merged.bin"   # -o 相对 build/ 解析，必须给绝对路径
```

`-o` 的路径是相对 `build/` 目录解析的，写相对路径会报 `No such file or directory`。

协议层不用装 ESP-IDF 也能单独验证：

```bash
../scripts/test-protocol.sh
```

## 数据通路

```text
ES8311 ──I2S──► 320 samples ──IMA-ADPCM──► [seq][predictor][index][nibbles] ──► AUDIO notify
OK 按下/抬起 ──► PTT 采集开关（固件侧 s_ptt_held；PC 侧映射成 ptt-toggle）
按键事件 ──► EVENT notify（press/click/double/long/release；PC 只消费 press）
1 s 心跳 ──► STATUS notify（state + flags + 丢块数 + 已发块数）
```

三条实现约束（都有对应的日志或状态位，不要静默退化）：

1. 采集任务**始终**读 I2S，即使没在采集也读后丢弃 —— 停读会让 DMA 溢出，恢复时先听到旧数据。
2. 未连接 / 未订阅 / MTU 不足时块直接丢弃并计入 `dropped`，`seq` 仍然递增，
   PC 端据此区分"没说话"和"丢了块"。
3. 音频初始化失败不阻断 BLE 启动：设备仍然广播，STATUS 上报 `ERROR` 且带 `AUDIO_ERROR` 位。

## 硬件事实（来自上游 BSP，勿凭记忆改）

| 用途 | 引脚 / 值 |
| --- | --- |
| I2C SDA / SCL | GPIO10 / GPIO7 |
| ES8311 地址 | `0x18`（7 位；`audio_codec_new_i2c_ctrl` 要 8 位形式） |
| I2S MCLK / BCLK / WS | GPIO6 / GPIO5 / GPIO3 |
| I2S DOUT / DIN | GPIO2 / GPIO4 |
| 按键（三键共用） | GPIO0 / ADC1_CH0；窗口 `≤150` UP、`≤447` DOWN、`≤1900` OK、`>1900` 松开 |
| 背光 | GPIO21（所以控制台走 USB-Serial-JTAG） |

来源：`gitee.com/FoloToy/ai-passport` 的 `components/bsp/include/bsp_pins.h`（MIT）。
换板只改 `main/board.h`，改完用 `apm_buttons_read_mv()` 重新标定按键窗口。

## 硬约束

1. **不要整份 fork 上游仓库。** 需要引脚、codec 初始化这类基础设施时按本项目结构重写，
   并在文件头注明来源与 MIT 许可（见 [`../docs/REFERENCES.md`](../docs/REFERENCES.md)）。
2. 协议改动先改 [`../docs/ARCHITECTURE.md`](../docs/ARCHITECTURE.md)，两端同步，
   并让 `test_protocol.c` 覆盖到。
3. 按键回调里不许阻塞（BLE 通知、音频都放别的任务）；回调只入队。
4. 烧录会改动设备上的数据（绑定、配置）；动手前先跟用户确认，不要默认执行。
5. 没有真机证据时，不许在文档或提交里写"音质合格""按键可用"。

## 验证状态

| 项 | 状态 |
| --- | --- |
| `idf.py build`（esp32c3 / IDF 5.5.3） | 通过，0 warning；app 654,912 B |
| `idf.py merge-bin` | 通过，合并镜像 720,448 B（`0xAFE40`） |
| 协议主机单测（44 项） | 通过；稳态 ADPCM 往返 SNR 32.89 dB；含毛刺判据与 PING/PONG 长度校验 |
| 真机烧录（esptool / 460800） | 通过，hash 校验通过，复位后 app 名 `ai-passport-mic` |
| 按键 ADC 初始化 | 通过（curve fitting 校准）。曾因 `APM_BTN_ADC_UNIT` 写成 1 命中 `ADC_UNIT_2` 而失败，已修为 0 |
| 按键手势判定 | **三键未逐项核对**。间接旁证只有 OK：`docs/GUI.md` §14.14 记录过"用户按 OK 没反应"，说明 OK 的 PRESS 到过 PC。UP / DOWN 无任何真机证据 |
| ES8311 采集出声 | 通过：真机 15 s 连续录音 748 块、0 丢块、长度异常 0 |
| BLE 连接 / MTU / 通知 | 通过：MTU 247（需求 ≥168），音频/状态/事件三路订阅均建立 |
| 连接间隔 | 实测由主机决定：macOS 落到 **30 ms**；固件请求常被拒（host 554，疑似与主机自身参数更新冲突），压到 15–20 ms 也被拒 |
| **两档连接参数（2026-11-07 代码完成，真机未验）** | 采集中请求 15–30 ms、空闲请求 **80 ms**，随 START/STOP/PTT/断链切档。`LINK_SLOW` 改为**只在采集态**判定（否则空闲档会把每场空闲误判成脏会话）。**切档速度与省电效果都要真机验证** |
| **BLE modem sleep** | **已开启（代码完成，真机未验）**：Mode 1 + main XTAL 低功耗时钟。`CONFIG_BT_CTRL_SLEEP_MODE_EFF=1` / `SLEEP_CLOCK_EFF=1` 已在生成的 sdkconfig 里核对过。**没有电流实测** |
| PING/PONG 回包 | **真机已测（2026-11-05）**：20 发 20 收、0 超时；RTT 中位 38.2 ms / p95 41.5 ms / 抖动 3.0 ms |
| 链路往返时延 | **已测量**（同上）。口径是链路，见 `docs/SOUND_QUALITY.md` |
| 端到端声学延迟 | **未测量**：ping 只覆盖链路，声学回环卡在宿主麦克风权限（见 `../docs/KNOWN_ISSUES.md` KI-003） |
| 电量计（CW2017） | **已解决（2026-11-06 真机）**：BATINFO 全 0 导致 SOC 无效；写入官方 UTL 520 mAh profile 后 SOC 实测 77–79%。见 KI-007 |

真机验证顺序见 [`../docs/ROADMAP.md`](../docs/ROADMAP.md) 的 M1，测完再往这张表里填数字。
