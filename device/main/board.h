// device/main/board.h
// AI Passport 硬件事实的单一来源：引脚、总线、按键分压窗口、设备名。
//
// 引脚与外设取值来自 FoloToy AI Passport 的 BSP（gitee.com/FoloToy/ai-passport
// components/bsp/include/bsp_pins.h，MIT）。换板只改这个文件。
// 文档依据：docs/ENVIRONMENT.md、docs/ARCHITECTURE.md。
#pragma once

// ---------------------------------------------------------------------------
// 显示（第一版不驱动屏幕，仅保留事实，避免与 GPIO 复用冲突）
// 注意：GPIO21 是屏幕背光，所以控制台必须走 USB-Serial-JTAG，不能用 UART0。
// ---------------------------------------------------------------------------
#define APM_LCD_BL_GPIO 21

// ---------------------------------------------------------------------------
// I2C：ES8311 音频 codec（唯一使用者，电量计不在第一版范围内）
// ---------------------------------------------------------------------------
#define APM_I2C_PORT       0          // I2C_NUM_0
#define APM_I2C_SDA_GPIO   10
#define APM_I2C_SCL_GPIO   7
#define APM_ES8311_ADDR    0x18       // 7 位地址；audio_codec_new_i2c_ctrl 需要 8 位形式

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
