// device/main/board.h
// AI Passport 硬件事实的单一来源：引脚、总线、按键分压窗口、设备名。
//
// 引脚与外设取值来自 FoloToy AI Passport 的 BSP（gitee.com/FoloToy/ai-passport
// components/bsp/include/bsp_pins.h，MIT）。换板只改这个文件。
// 文档依据：docs/ENVIRONMENT.md、docs/ARCHITECTURE.md。
#pragma once

// ---------------------------------------------------------------------------
// 电源键：机身**左侧**，硬件锁存，**不接 MCU**（所以这里没有对应的 GPIO 宏）
//
// 位置：左侧与 USB-C 充电口、绿色充电指示灯同侧；右侧才是上 / 下 / OK 三键。
// 行为（FoloToy 官方上手文档 guides/getting-started）：
//   开机 按住 0.5 s；关机 长按 2 s；休眠唤醒 按右侧任意功能键（不是电源键）。
//
// 为什么要写在这里：**关机态下插 USB 只给充电 IC 上电（绿灯常亮），C3 的电源轨是
// 断的** —— PC 侧看到的正是"USB 不枚举 + BLE 不广播 + 扫描 0 个设备"。2026-11-06
// 那次"今早连不上设备"就是这一条，却被先误判成没电、固件没起来、数据线坏。
// 现场排查顺序：先按电源键 0.5 s，再看绿灯，最后才怀疑线材。
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 显示：ST7789P3 240x320，4 线 SPI（SPI2_HOST）
//
// 引脚与"为什么要用 mode 0 / 为什么 RST 是 -1"来自上游 FoloToy BSP
// （components/bsp/include/bsp_pins.h，MIT）。
// ---------------------------------------------------------------------------
#define APM_LCD_SPI_HOST    SPI2_HOST
#define APM_LCD_MOSI_GPIO   9
#define APM_LCD_SCLK_GPIO   8
#define APM_LCD_CS_GPIO     1
#define APM_LCD_DC_GPIO     20
#define APM_LCD_RST_GPIO    (-1)      // -1 = 未接 MCU，走 SWRESET
#define APM_LCD_PCLK_HZ     (80 * 1000 * 1000)
#define APM_LCD_SPI_MODE    0         // ST7789 SCK 空闲低、上升沿采样
#define APM_LCD_INVERT      1         // 本板屏幕出厂需 INVON；换屏呈负片时改 0
#define APM_LCD_W           240
#define APM_LCD_H           320
// 刷屏行缓冲高度：每次推 16 行（240 × 16 × 2 = 7.5 KB）。
// 不存整帧——240×320×2 = 150 KB，C3 的 RAM 放不下。
#define APM_LCD_BAND_ROWS   16
//
// 注意：**GPIO21 是背光**，所以控制台必须走 USB-Serial-JTAG，不能用 UART0
// （见 sdkconfig.defaults）。**RST 未接 MCU**（硬接 3.3V），复位只能靠
// esp_lcd_panel_reset() 走 SWRESET 软复位。本板的屏出厂即需反色
// （参考例程 TFT_init() 末尾无条件发 0x21 INVON），故 APM_LCD_INVERT = 1。

// ---------------------------------------------------------------------------
// 背光：GPIO21，LEDC PWM（参数来自上游 BSP）。**所有权归 screen.c**。
// battery.c 原来的低电量短闪改成向 screen 申请（apm_screen_blink_low_battery）。
// 充电口旁的充电指示灯是充电 IC 硬件直驱，MCU 不可控。
// ---------------------------------------------------------------------------
#define APM_BL_GPIO            21
#define APM_BL_LEDC_TIMER      0          // LEDC_TIMER_0
#define APM_BL_LEDC_CHANNEL    0          // LEDC_CHANNEL_0
#define APM_BL_LEDC_RESOLUTION 10         // bit，最大占空比 1023
#define APM_BL_LEDC_FREQ_HZ    5000
// 亮屏时的背光占空比（满量程的百分比）。80% 而不是 100%：
// 这块屏在 100% 下近看偏刺眼，80% 足够看清且省一点背光电。
#define APM_BL_ON_PCT          80
// 低电量短闪的峰值占空比（灭屏时的被动提示，只要"看得见"）。
#define APM_BL_FLASH_PCT       20

// ---------------------------------------------------------------------------
// I2C：ES8311 音频 codec 与 CW2017 电量计共用一条总线
// 电量计事实来自上游 FoloToy BSP（bsp_pins.h，MIT）。VCELL（0x02/0x03）的
// 换算系数未经实测核对，第一版只读 SOC，避免上报未验证的数值。
// ---------------------------------------------------------------------------
#define APM_I2C_PORT       0          // I2C_NUM_0
#define APM_I2C_SDA_GPIO   10
#define APM_I2C_SCL_GPIO   7
#define APM_ES8311_ADDR    0x18       // 7 位地址；audio_codec_new_i2c_ctrl 需要 8 位形式
#define APM_I2C_CW2017_ADDR 0x63      // 7 位地址

// ---------------------------------------------------------------------------
// 背光：GPIO21，LEDC PWM（参数来自上游 BSP）。屏幕第一版不驱动，背光
// 被低电量指示复用（见 battery.c）：SOC <= 阈值时**按任意键短闪 3 次**，
// 平时恒灭（零功耗）。充电口旁的充电指示灯是充电 IC 硬件直驱，MCU 不可控。
// ---------------------------------------------------------------------------
#define APM_BL_GPIO            21
#define APM_BL_LEDC_TIMER      0          // LEDC_TIMER_0
#define APM_BL_LEDC_CHANNEL    0          // LEDC_CHANNEL_0
#define APM_BL_LEDC_RESOLUTION 10         // bit，最大占空比 1023
#define APM_BL_LEDC_FREQ_HZ    5000
#define APM_BL_BREATH_PEAK_PCT 4          // 呼吸峰值占空比（满量程的百分比）

// ---------------------------------------------------------------------------
// I2S：全双工，同端口共用 MCLK/BCLK/WS
// ---------------------------------------------------------------------------
#define APM_I2S_PORT       0          // I2S_NUM_0
#define APM_I2S_MCLK_GPIO  6
#define APM_I2S_BCLK_GPIO  5
#define APM_I2S_WS_GPIO    3
#define APM_I2S_DOUT_GPIO  2
#define APM_I2S_DIN_GPIO   4
#define APM_I2S_PA_GPIO    (-1)       // 功放常通，未接 MCU

// ---------------------------------------------------------------------------
// 按键：三个键共用一个 ADC 引脚，靠分压电阻区分（不是三个独立 GPIO）
//
//   3.3V ── 10k 外部上拉 ──┬── ADC(GPIO0)
//                          └── 按键 ── 分压电阻 ── GND
//
// 上 ≈ 0 mV / 下 ≈ 300 mV / 确定 ≈ 595 mV / 松开 ≈ 3300 mV
// 窗口边界取相邻档中点；换阻值后必须用 apm_buttons_read_mv() 重新标定。
// 不能用内部上拉（约 45kΩ，三档会挤到 0~154 mV 并随温漂重叠）。
// ---------------------------------------------------------------------------
// 注意：adc_unit_t 枚举从 0 开始（ADC_UNIT_1 = 0、ADC_UNIT_2 = 1），
// 这里写 1 会变成 ADC_UNIT_2，C3 上 adc_oneshot 直接报 "adc unit not supported"。
#define APM_BTN_ADC_UNIT     0        // ADC_UNIT_1
#define APM_BTN_ADC_CHANNEL  0        // ADC_CHANNEL_0 = GPIO0
#define APM_BTN_GPIO         0
#define APM_BTN_MV_UP_MAX    150
#define APM_BTN_MV_DOWN_MAX  447
#define APM_BTN_MV_OK_MAX    1900     // 超过即视为松开

// 手势时序（ms）。双击窗口 180 ms 与 config/README.md 的硬约束一致。
#define APM_BTN_DEBOUNCE_MS  20
#define APM_BTN_DOUBLE_MS    180
#define APM_BTN_LONG_MS      500

// ---------------------------------------------------------------------------
// 设备身份
// ---------------------------------------------------------------------------
#define APM_DEVICE_NAME      "AI Passport Mic"

// 音频参数（与 docs/ARCHITECTURE.md 一致）
#define APM_AUDIO_HZ         16000
#define APM_AUDIO_BITS       16
#define APM_AUDIO_CHANNELS   1
#define APM_AUDIO_DEFAULT_GAIN_DB 30
