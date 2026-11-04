// device/main/buttons.c
// 三键共用一个 ADC 引脚的判定：采样 -> 去抖 -> 按下/单击/双击/长按/抬起。
//
// 参考: FoloToy ai-passport components/bsp/src/bsp_button.c 与 include/bsp_pins.h (MIT)
//       —— 分压窗口与"不能用内部上拉"的结论来自那份 BSP；手势状态机是本项目自写，
//       因为本项目要把事件直接映射成 config/README.md 的事件名。
#include "buttons.h"

#include "board.h"

#include "esp_adc/adc_cali.h"
#include "esp_adc/adc_cali_scheme.h"
#include "esp_adc/adc_oneshot.h"
#include "esp_log.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"

static const char *TAG = "apm_btn";

#define APM_BTN_RELEASED 3   // 伪键值：无按键

static adc_oneshot_unit_handle_t s_adc;
static adc_cali_handle_t         s_cali;
static apm_btn_cb_t              s_cb;

// 状态机
static int   s_stable;        // 已确认的键（0..2 或 3=松开）
static int   s_pending;       // 待确认的键
static int   s_pending_cnt;   // 连续相同的采样次数
static int   s_last_key;      // 最近一次抬起的键（用于单击/双击归属）
static int64_t s_press_ms;
static int64_t s_release_ms;
static bool  s_long_fired;
static bool  s_waiting_second;
static bool  s_double_pending;

static void emit(apm_btn_t btn, apm_btn_ev_t ev)
{
    if (!s_cb) return;
    s_cb(btn, ev);
}

static esp_err_t adc_init(void)
{
    adc_oneshot_unit_init_cfg_t unit_cfg = {
        .unit_id  = (adc_unit_t)APM_BTN_ADC_UNIT,
        .ulp_mode = ADC_ULP_MODE_DISABLE,
    };
    esp_err_t err = adc_oneshot_new_unit(&unit_cfg, &s_adc);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "ADC 单元初始化失败: %s", esp_err_to_name(err));
        return err;
    }

    adc_oneshot_chan_cfg_t chan_cfg = {
        .atten    = ADC_ATTEN_DB_12,     // 量程 ~0-3.3 V
        .bitwidth = ADC_BITWIDTH_DEFAULT,
    };
    err = adc_oneshot_config_channel(s_adc, (adc_channel_t)APM_BTN_ADC_CHANNEL, &chan_cfg);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "ADC 通道配置失败: %s", esp_err_to_name(err));
        return err;
    }

    adc_cali_curve_fitting_config_t cali_cfg = {
        .unit_id  = (adc_unit_t)APM_BTN_ADC_UNIT,
        .chan     = (adc_channel_t)APM_BTN_ADC_CHANNEL,
        .atten    = ADC_ATTEN_DB_12,
        .bitwidth = ADC_BITWIDTH_DEFAULT,
    };
    if (adc_cali_create_scheme_curve_fitting(&cali_cfg, &s_cali) == ESP_OK) {
        ESP_LOGI(TAG, "ADC 校准: curve fitting");
    } else {
        s_cali = NULL;
        ESP_LOGW(TAG, "ADC 校准不可用，退回线性换算（按键窗口可能不准）");
    }
    return ESP_OK;
}

int apm_buttons_read_mv(void)
{
    if (!s_adc) return -1;

    int raw = 0;
    if (adc_oneshot_read(s_adc, (adc_channel_t)APM_BTN_ADC_CHANNEL, &raw) != ESP_OK) {
        return -1;
    }
    if (s_cali) {
        int mv = 0;
        if (adc_cali_raw_to_voltage(s_cali, raw, &mv) == ESP_OK) return mv;
    }
    // 12 bit + 12 dB 衰减：满量程约 3.3 V
    return (raw * 3300) / 4095;
}

static int classify(int mv)
{
    if (mv < 0) return -1;
    if (mv <= APM_BTN_MV_UP_MAX) return APM_BTN_UP;
    if (mv <= APM_BTN_MV_DOWN_MAX) return APM_BTN_DOWN;
    if (mv <= APM_BTN_MV_OK_MAX) return APM_BTN_OK;
    return APM_BTN_RELEASED;
}

static void on_press(int key, int64_t now)
{
    s_press_ms = now;
    s_long_fired = false;
    emit((apm_btn_t)key, APM_BTN_EV_PRESS);

    // 第二次按下落在双击窗口内且是同一个键，才记为双击候选。
    if (s_waiting_second && key == s_last_key &&
        (now - s_release_ms) <= APM_BTN_DOUBLE_MS) {
        s_double_pending = true;
    } else {
        s_double_pending = false;
    }
    s_waiting_second = false;
}

static void on_release(int key, int64_t now)
{
    emit((apm_btn_t)key, APM_BTN_EV_RELEASE);

    if (s_long_fired) {
        // 长按已经上报过，抬起不再补发单击。
        s_long_fired = false;
        s_double_pending = false;
        s_waiting_second = false;
        return;
    }
    if (s_double_pending) {
        emit((apm_btn_t)key, APM_BTN_EV_DOUBLE);
        s_double_pending = false;
        s_waiting_second = false;
        return;
    }
    s_last_key = key;
    s_release_ms = now;
    s_waiting_second = true;
}

static void button_task(void *arg)
{
    (void)arg;
    const TickType_t period = pdMS_TO_TICKS(10);

    for (;;) {
        int mv = apm_buttons_read_mv();
        int raw = classify(mv);
        int64_t now = esp_timer_get_time() / 1000;

        if (raw >= 0) {
            if (raw != s_pending) {
                s_pending = raw;
                s_pending_cnt = 0;
            } else if (s_pending_cnt < 2) {
                s_pending_cnt++;   // 连续 2 次相同（约 20 ms）才认可
            }

            if (s_pending_cnt >= 2 && s_pending != s_stable) {
                int prev = s_stable;
                s_stable = s_pending;
                if (prev == APM_BTN_RELEASED) {
                    on_press(s_stable, now);
                } else if (s_stable == APM_BTN_RELEASED) {
                    on_release(prev, now);
                } else {
                    // 直接从一个键跳到另一个键：按"先抬起再按下"处理，保证事件成对。
                    on_release(prev, now);
                    on_press(s_stable, now);
                }
            }
        }

        if (s_stable != APM_BTN_RELEASED && !s_long_fired &&
            (now - s_press_ms) >= APM_BTN_LONG_MS) {
            s_long_fired = true;
            emit((apm_btn_t)s_stable, APM_BTN_EV_LONG);
        }

        if (s_waiting_second && s_stable == APM_BTN_RELEASED &&
            (now - s_release_ms) > APM_BTN_DOUBLE_MS) {
            s_waiting_second = false;
            emit((apm_btn_t)s_last_key, APM_BTN_EV_CLICK);
        }

        vTaskDelay(period);
    }
}

esp_err_t apm_buttons_start(apm_btn_cb_t cb)
{
    s_cb = cb;
    s_stable = APM_BTN_RELEASED;
    s_pending = APM_BTN_RELEASED;
    s_pending_cnt = 0;
    s_last_key = APM_BTN_OK;

    esp_err_t err = adc_init();
    if (err != ESP_OK) return err;

    BaseType_t ok = xTaskCreate(button_task, "apm_btn", 3072, NULL, 4, NULL);
    if (ok != pdPASS) {
        ESP_LOGE(TAG, "按键任务创建失败");
        return ESP_ERR_NO_MEM;
    }
    ESP_LOGI(TAG, "按键就绪（UP<=%d mV, DOWN<=%d mV, OK<=%d mV 为按下）",
             APM_BTN_MV_UP_MAX, APM_BTN_MV_DOWN_MAX, APM_BTN_MV_OK_MAX);
    return ESP_OK;
}

int apm_buttons_current_key(void) { return s_stable; }
