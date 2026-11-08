# device/ —— 固件（M1 已实现，真机采集与 BLE 链路已验证）

ESP32-C3 上的采集与 BLE 传输固件。**代码已落地，能编译、能烧录，采集、BLE 链路与三键判定
都在真机上跑过**；省电配置已生效但**电流一个都没量**。逐条证据见文末「验证状态」，
未验证的项在那里明确标着「未验证」，不要当成"已通过"。

## 技术栈

| 项 | 值 |
| --- | --- |
| 框架 | ESP-IDF **v5.5.3**（固定版本） |
| 语言 | C |
| 目标 | ESP32-C3，8 MB Flash，无 PSRAM |
| BLE | NimBLE，单连接 peripheral，自定义 GATT（CTRL / PROFILE / STATUS / EVENT / AUDIO） |
| 控制台 | USB-Serial-JTAG（C3 原生 USB）；UART0 的 TX=GPIO21 与本板背光冲突所以不用 |
| 组件 | `espressif/esp_codec_dev` 1.6.2（`main/idf_component.yml`） |

## 目录

```text
device/
  CMakeLists.txt          # PROJECT_VER = 0.1.0（对外 semver，别改回 git describe）
  partitions.csv          # nvs + phy_init + factory（无 OTA）
  sdkconfig.defaults      # 目标芯片、8MB Flash、USB-Serial-JTAG、NimBLE 与省电基线
  main/
    app_main.c            # 任务编排：采集 / 事件 / STATUS 心跳
    protocol.c/.h         # 帧、CRC16-CCITT、IMA-ADPCM（纯 C，可在主机单测）
    power_policy.c/.h     # 连接参数两档 / LINK_SLOW 判据 / 采集态状态机 / 发射功率滞回（纯 C）
    audio.c/.h            # ES8311 + I2S 全双工，16 kHz / 16-bit / mono；共享 I2C 总线
    battery.c/.h          # CW2017 电量计（阈值 20% 判低电）
    link.c/.h             # NimBLE GATT、广播、MTU 协商、通知、标准 Battery Service
    buttons.c/.h          # 单 ADC 三键判定（去抖 / 单击 / 双击 / 长按 / 按下抬起；PC 只消费 PRESS）
    screen.c/.h           # ST7789P3 硬件层：SPI/面板/背光、逐带刷屏、屏幕任务
    screen_render.c/.h    # 渲染纯逻辑：字形查表、UTF-8、文本/电量条/整屏布局（纯 C）
    screen_state.c/.h     # 亮灭状态机 + 按键标签存储（纯 C）
    screen_texts.h        # 屏幕上的全部文案（字库字符集从这里提取）
    font16_data.h         # 16x16 点阵字库（自动生成，勿手改）
    board.h               # 引脚与硬件常量（唯一事实来源）
  test/
    test_protocol.c       # 主机侧单测，不依赖 ESP-IDF
    test_power_policy.c   # 同上，覆盖省电判定逻辑
    test_screen_render.c  # 同上，覆盖渲染与亮灭状态机
    gen_vectors.c         # 从 C 实现导出跨端向量（scripts/gen-protocol-vectors.sh 调用）
    preview_screen.c      # 开发辅助：把整屏渲染存成 PPM，主机上看排版（不进门禁）
```

`protocol.c`、`power_policy.c`、`screen_render.c`、`screen_state.c` 是**刻意做成不依赖 ESP-IDF
的纯逻辑模块**：这样判定逻辑能在主机上直接单测。新写的固件判定逻辑请尽量放进这类模块，
否则它只能在真机上"看一眼"。

屏幕这一层是照着这条规矩拆的：**画什么**（`screen_render.c`）、**什么时候亮灭**
（`screen_state.c`）都是纯逻辑，`screen.c` 只碰 SPI / LEDC / FreeRTOS。
"采集中不许熄屏""长按开麦键不切屏""逐带渲染与整屏渲染必须等价"这类规则因此能被自动检查。

## 构建

```bash
source ../scripts/activate-idf.sh                 # 或先按 docs/ENVIRONMENT.md 方案 A 处理 venv
idf.py set-target esp32c3                          # 只需一次
idf.py build
idf.py merge-bin -o "$PWD/build/ai-passport-mic-merged.bin"   # -o 相对 build/ 解析，必须给绝对路径
```

或者直接跑封装脚本（会顺带处理 venv 回退）：

```bash
../scripts/build-firmware.sh
```

`-o` 的路径相对 `build/` 目录解析，写相对路径会报 `No such file or directory`。

协议、省电逻辑与屏幕逻辑不用装 ESP-IDF 也能单独验证（三个目标合计 207 项）：

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
| 手势时序 | 去抖 20 ms、双击窗口 180 ms、长按 500 ms（`APM_BTN_*`，与 `config/README.md` 一致） |
| 屏幕 | ST7789P3 240×320，SPI2 mode 0 @ 80 MHz；MOSI=GPIO9 / SCLK=GPIO8 / CS=GPIO1 / DC=GPIO20；**RST 未接 MCU**（走 SWRESET） |
| 背光 | GPIO21，LEDC timer0/ch0，10 bit，5 kHz（所以控制台走 USB-Serial-JTAG） |

来源：`gitee.com/FoloToy/ai-passport` 的 `components/bsp/include/bsp_pins.h`（MIT）。
换板只改 `main/board.h`，改完用 `apm_buttons_read_mv()` 重新标定按键窗口。

## 硬约束

1. **不要整份 fork 上游仓库。** 需要引脚、codec 初始化这类基础设施时按本项目结构重写，
   并在文件头注明来源与 MIT 许可（见 [`../docs/REFERENCES.md`](../docs/REFERENCES.md)）。
2. 协议改动先改 [`../docs/ARCHITECTURE.md`](../docs/ARCHITECTURE.md)，两端同步，
   并让 `test_protocol.c` 覆盖到、重跑 `gen-protocol-vectors.sh`。
3. 按键回调里不许阻塞（BLE 通知、音频都放别的任务）；回调只入队。
4. 烧录会改动设备上的数据（绑定、配置）；动手前先跟用户确认，不要默认执行。
5. 没有真机证据时，不许在文档或提交里写"音质合格""按键可用""省了多少电"。

## 验证状态

> 数字会变（镜像字节数、RTT 等），**以 `../scripts/build-firmware.sh`、`test-protocol.sh`
> 与 `apmctl` 的实际输出为准**。最近一次复核 2026-11-08。

| 项 | 状态 |
| --- | --- |
| `idf.py build`（esp32c3 / IDF 5.5.3） | **通过，0 warning**（2026-11-08 复核）。app 665,200 B |
| `idf.py merge-bin` | **通过**：合并镜像 730,736 B（2026-11-08 复核）。字节数随固件改动而变，别当常量引用 |
| 固件主机侧单测（207 项） | **通过（2026-11-08 复核）**：`protocol.c` 71 项（含稳态 ADPCM 往返 SNR 32.89 dB、毛刺判据、PING/PONG 长度校验、`SET_KEY_LABELS` 编解码与非法帧拒绝）+ `power_policy.c` 65 项 + `screen_render.c`/`screen_state.c` 71 项（UTF-8 解码、缺字形兜底、越界裁剪、逐带渲染与整屏渲染逐像素等价、长按切屏、采集中不许熄屏、标签截断落在字符边界上）。随 `verify.sh` 跑 |
| 真机烧录（esptool / 460800） | 通过，hash 校验通过，复位后 app 名 `ai-passport-mic` |
| 按键 ADC 初始化 | 通过（curve fitting 校准）。曾因 `APM_BTN_ADC_UNIT` 写成 1 命中 `ADC_UNIT_2` 而失败，已修为 0 |
| 三键判定 | **通过（2026-11-06 真机）**：UP / DOWN / OK 各按一次，`apmctl status` 打出对应 `按键: <键>.press`，计数 4/1/4。见 `../docs/GUI_IMPLEMENTATION_LOG.md` §14.16 |
| 15 个手势码（3 键 × press/click/double/long/release） | **通过（2026-11-06，15/15）**：这是协议层证据，不是产品验收——PC 从 `keymap-3` 起只消费 `PRESS`，另外四码只记日志。见 §14.17 |
| 三键电压窗口 | **只验了分类结果，未用万用表复核实际电压**。换板要改 `board.h` 重新标定，这条是标定依据。见 `../docs/TODO.md` P0 |
| ES8311 采集出声 | 通过：真机 15 s 连续录音 748 块、0 丢块、长度异常 0 |
| BLE 连接 / MTU / 通知 | 通过：MTU 247（需求 ≥168），音频/状态/事件三路订阅均建立 |
| 连接间隔 | 实测由主机决定：macOS 落在 **30 ms**；固件请求常被拒（host 554，疑似与主机自身参数更新冲突），压到 15–20 ms 也被拒 |
| 两档连接参数 | **通过（2026-11-07 真机）**：采集中块率 50.000/s、序号跳变 0；空闲态 PING RTT 中位 **117–118 ms**（≈80 ms 间隔）vs 采集态 **58.8 ms**（≈30 ms 间隔），比例与间隔比一致，两档确实都生效 |
| `LINK_SLOW` 判据 | 代码完成 + 7 项单测。**曾误报**（进入采集后头 1–2 个间隔读到旧的空闲档 80 ms），已加 2000 ms 宽限期并在采集态**上升沿**记时间戳。**"拦得住脏会话"那一半仍只到单测**，本机造不出慢档会话 |
| BLE modem sleep | **已生效（2026-11-07 真机）**：日志 `省电状态：低功耗时钟=main XTAL，modem sleep=已启用（ESP_OK）`。判据是 `esp_bt_sleep_enable()` 返回值，**不是** `BLE_INIT` 的 `Using main XTAL as clock source`（后者没开 modem sleep 也会打印） |
| 省电的电流收益 | **完全没有测**：广播 / 空闲 / 采集中三个场景一个都没量。上面几条只说明"开关打开了、行为变了"，**不能当省电数字引用**。填表见 [`../docs/ENVIRONMENT.md`](../docs/ENVIRONMENT.md) 的功耗基线表 |
| `glitch_events` 回归 | **通过**：动了 codec 工作模式与 PM/时钟后，12–15 s 录音多次均 `glitch_events = 0`，KI-001 未被诱发 |
| PING/PONG 回包 | **真机已测（2026-11-05）**：20 发 20 收、0 超时；RTT 中位 38.2 ms / p95 41.5 ms / 抖动 3.0 ms。2026-11-07 同日对照为 58.8 ms（基线固件同样 58.9 ms），差异来自主机侧状态 |
| 端到端声学延迟 | **未测量**：ping 只覆盖链路，声学回环卡在宿主麦克风权限（见 [`../docs/KNOWN_ISSUES.md`](../docs/KNOWN_ISSUES.md) KI-003） |
| 电量计（CW2017） | **已解决（2026-11-06 真机）**：BATINFO 全 0 导致 SOC 无效；写入官方 UTL 520 mAh profile 后 SOC 实测 77–79%。见 KI-007 |
| 屏幕（ST7789P3）初始化 | **通过（2026-11-08 真机）**：`面板就绪 240x320` + `背光就绪 gpio=21（80% 为亮屏）` + `屏幕就绪：30 s 无操作自动熄屏`。SPI2 / panel_io / ST7789 驱动 / 厂商初始化序列 / LEDC 全部无错误返回。**面板上实际画出来的东西（反色、镜像、观感）仍需人眼看** |
| 屏幕自动熄屏 | **通过（2026-11-08 真机）**：`I (31813) apm_screen: 无操作自动熄屏`，距启动亮屏约 **30.2 s**，与 `APM_SCREEN_IDLE_OFF_MS` 一致 |
| 屏幕标签下发（`SET_KEY_LABELS`） | **通过（2026-11-08 真机）**：`apmctl screen-labels` 下发用户真实映射（`enter` / `esc` / `开麦 关麦`），设备日志 `CTRL SET_KEY_LABELS：开麦键=已设置，3 条标签`，**无缺字形 WARN**；断链 `reason=0x213`。屏幕观感未确认 |
| 屏幕亮灭手势 | **部分真机验证（2026-11-08）**：单击不响应 / 长按切屏已由用户实测确认；采集态不点亮屏幕已由真机日志确认（`CTRL START` 后无任何背光动作）。规则在 `screen_state.c`（80 项单测，含"采集中照常熄屏"的反向断言）。**未验**：灭屏时长按点亮的手感、"ST7789 是否真进 Sleep In / 背光是否真降到 0"（要量电流） |
| 空片首烧 / 烧写中断后恢复 | **未验证**：M6 的其余路径（检测 / 写入 / FlashMd5 / 复位回应用模式）2026-11-06 真机已通，但整片 `0xFF` 的裸片没上过电，也没故意拔过线 |
| 量产板 USB 走线 | **未核对**：必须是 C3 原生 USB D-/D+（GPIO18/19），不是 CH340。换板后本机 M6 结论全部作废 |

真机验证顺序见 [`../docs/ROADMAP.md`](../docs/ROADMAP.md) 的 M1，测完再往这张表里填数字。
