// device/main/buttons.h
// 三键（UP / DOWN / OK）手势判定：键共享一个 ADC 引脚，靠分压区分。
//
// 事件语义与 config/README.md 对齐：
//   APM_BTN_EV_PRESS   -> ok.press   （按下瞬间，PTT 起点）
//   APM_BTN_EV_RELEASE -> ok.release （抬起瞬间，PTT 终点）
//   APM_BTN_EV_CLICK   -> *.short
//   APM_BTN_EV_DOUBLE  -> *.double
//   APM_BTN_EV_LONG    -> *.long
#pragma once

#include "esp_err.h"
#include "protocol.h"

typedef void (*apm_btn_cb_t)(apm_btn_t btn, apm_btn_ev_t ev);

// 启动采样任务（10 ms 周期）。回调在按键任务上下文执行，必须只做入队这类有界操作。
esp_err_t apm_buttons_start(apm_btn_cb_t cb);

// 当前 ADC 电压（mV）。判为松开时约 3300；读取失败返回 -1。
// 换分压电阻后用它重新标定 board.h 里的窗口。
int apm_buttons_read_mv(void);

// 当前判定到的键（APM_BTN_* 或 3 = 松开），用于 STATUS 诊断。
int apm_buttons_current_key(void);
