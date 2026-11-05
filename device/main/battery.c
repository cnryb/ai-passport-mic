// device/main/battery.c
// CW2017 电量计 + 低电量按键短闪指示。
//
// 硬件事实来自上游 FoloToy BSP（gitee.com/FoloToy/ai-passport bsp_pins.h，MIT）：
// 电量计与 ES8311 共用一条 I2C 总线（0x63）；GPIO21 背光是 LEDC PWM。
// 屏幕第一版不驱动，背光只当**被动查询式**低电量指示灯：平时恒灭（零功耗），
// SOC <= 20% 时按任意键，背光短闪 3 次。充电口旁的充电指示灯是充电 IC 硬件直驱
// （上游 BSP 能力契约无 LED 接口），MCU 不可控。
//
// 协议依据（CW2015/CW2017 系列寄存器，两代布局一致）：
//   - SOC = reg 0x04（整数 %）+ reg 0x05（1/256 分数）；
//   - RRT = reg 0x06，写 0xAA 触发 SOC 立即重算（上电后首读可能还是旧值）。
// VCELL（0x02/0x03）的换算系数未经实测核对，第一版不读不报，避免给未验证的数值。
//
// Unverified（真机未验）：CW2017 实际读数、短闪观感。
#include "battery.h"

#include "audio.h"
#include "board.h"
#include "link.h"

#include "driver/ledc.h"
#include "esp_log.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"

#include <string.h>

static const char *TAG = "apm_battery";

// CW2017 寄存器（数据手册布局）
#define APM_CW2017_SOC_REG 0x04
#define APM_CW2017_RRT_REG 0x06
#define APM_CW2017_RRT_REFRESH 0xAA

// 峰值占空比取满量程的 APM_BL_BREATH_PEAK_PCT%（约 4%）——夜里可辨、白天观感待
// 真机核对。短闪节奏见 battery_task 注释。
#define APM_BL_STEP_MS        50

static i2c_master_dev_handle_t s_cw;
static bool     s_ready;
static uint8_t  s_soc;      // 最近一次成功读数
static bool     s_have_soc; // 是否读到过至少一次
static bool     s_low;

bool apm_battery_ready(void) { return s_have_soc; }
uint8_t apm_battery_soc(void) { return s_soc; }

static esp_err_t cw_read(uint8_t reg, uint8_t *buf, size_t len)
{
    return i2c_master_transmit_receive(s_cw, &reg, 1, buf, len, 50);
}

static esp_err_t cw_write(uint8_t reg, uint8_t val)
{
    uint8_t b[2] = { reg, val };
    return i2c_master_transmit(s_cw, b, 2, 50);
}

esp_err_t apm_battery_init(void)
{
    if (s_ready) return ESP_OK;

    i2c_master_bus_handle_t bus = apm_audio_i2c_bus();
    if (!bus) {
        return ESP_ERR_INVALID_STATE;
    }

    i2c_device_config_t dev_cfg = {
        .dev_addr_length = I2C_ADDR_BIT_LEN_7,
        .device_address  = APM_I2C_CW2017_ADDR,
        .scl_speed_hz    = 400000,
    };
    esp_err_t err = i2c_master_bus_add_device(bus, &dev_cfg, &s_cw);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "CW2017 设备句柄创建失败: %s", esp_err_to_name(err));
        return err;
    }

    // 触发一次 SOC 重算。只做一次：每写一次都强制芯片重算，没有意义。
    err = cw_write(APM_CW2017_RRT_REG, APM_CW2017_RRT_REFRESH);
    if (err != ESP_OK) {
        // 无应答多半是板上真没焊这颗芯片：不卡启动，电量只是不可用。
        ESP_LOGW(TAG, "CW2017 无应答（%s），电量功能不可用", esp_err_to_name(err));
        s_cw = NULL;
        return err;
    }

    s_ready = true;
    ESP_LOGI(TAG, "CW2017 就绪 I2C 0x%02X，低电量阈值 %d%%",
             APM_I2C_CW2017_ADDR, APM_BATTERY_LOW_SOC_PCT);
    return ESP_OK;
}

esp_err_t apm_battery_read(uint8_t *soc)
{
    if (!s_ready) return ESP_ERR_INVALID_STATE;
    if (!soc) return ESP_ERR_INVALID_ARG;

    uint8_t buf[2];
    esp_err_t err = cw_read(APM_CW2017_SOC_REG, buf, 2);
    if (err != ESP_OK) return err;

    // MSB = 整数 %，LSB = 1/256 分数。读到非法大值时按 0 容错而不是崩溃。
    uint32_t pct = (uint32_t)buf[0] * 100 + ((uint32_t)buf[1] * 100) / 256;
    if (buf[0] > 100) pct = 0;
    if (pct > 100) pct = 100;
    *soc = (uint8_t)pct;
    return ESP_OK;
}

// ---------------------------------------------------------------------------
// 背光：LEDC PWM。不用时占空比恒 0（等效熄灭，不破坏省电初衷）。
// ---------------------------------------------------------------------------
static void bl_init(void)
{
    ledc_timer_config_t timer = {
        .speed_mode      = LEDC_LOW_SPEED_MODE,
        .timer_num       = (ledc_timer_t)APM_BL_LEDC_TIMER,
        .duty_resolution = (ledc_timer_bit_t)APM_BL_LEDC_RESOLUTION,
        .freq_hz         = APM_BL_LEDC_FREQ_HZ,
        .clk_cfg         = LEDC_AUTO_CLK,
    };
    esp_err_t err = ledc_timer_config(&timer);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "背光 LEDC timer 配置失败: %s（指示灯不可用）", esp_err_to_name(err));
        return;
    }

    ledc_channel_config_t chan = {
        .gpio_num   = APM_BL_GPIO,
        .speed_mode = LEDC_LOW_SPEED_MODE,
        .channel    = (ledc_channel_t)APM_BL_LEDC_CHANNEL,
        .timer_sel  = (ledc_timer_t)APM_BL_LEDC_TIMER,
        .duty       = 0,
        .hpoint     = 0,
    };
    err = ledc_channel_config(&chan);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "背光 LEDC channel 配置失败: %s", esp_err_to_name(err));
    }
}

static void bl_duty(uint32_t duty)
{
    uint32_t max = (1u << APM_BL_LEDC_RESOLUTION) - 1;
    if (duty > max) duty = max;
    ledc_set_duty(LEDC_LOW_SPEED_MODE, (ledc_channel_t)APM_BL_LEDC_CHANNEL, duty);
    ledc_update_duty(LEDC_LOW_SPEED_MODE, (ledc_channel_t)APM_BL_LEDC_CHANNEL);
}

// ---------------------------------------------------------------------------
// 电量任务：1 s 采样 SOC（芯片内部也是 1 s 刷新），变化时 BLE 上报。
//
// 背光是**被动查询式**指示，不是常驻呼吸：平时占空比恒 0，零功耗；
// 只有低电量时用户按任意键，才短闪 3 次（每次亮 50 ms、间隔 150 ms，约 600 ms），
// 总耗电可忽略。理由：屏幕背光持续点亮会加速耗电，"没电了还更费电"不可接受；
// 按键查询只在用户主动想知道电量的时刻花一点电。峰值占空比取满量程的
// APM_BL_BREATH_PEAK_PCT%（约 4%），真机观感待验收。
// I2C 偶发失败按"维持上次状态"处理，不抖动。
// ---------------------------------------------------------------------------
static volatile uint32_t s_flash_steps;  // 剩余闪烁步数（50 ms 一步），0 = 不闪

static uint32_t bl_flash_duty(uint32_t step_remaining)
{
    // 一步 50 ms：亮 1 步、灭 3 步为一组，共 3 组（12 步 ≈ 600 ms）。
    // 步计数从高往低走：remaining % 4 == 0 的那一步正好落在每个"亮"位上。
    if (step_remaining == 0) return 0;
    uint32_t phase = (step_remaining - 1) % 4;
    if (phase != 0) return 0;
    return (uint32_t)APM_BL_BREATH_PEAK_PCT * ((1u << APM_BL_LEDC_RESOLUTION) - 1) / 100;
}

void apm_battery_on_button(void)
{
    // 低电量 + 背光可用才闪；其他情况按键不点亮背光（平时零功耗）。
    if (s_low && s_have_soc) {
        s_flash_steps = 12;
    }
}

static void battery_task(void *arg)
{
    (void)arg;
    uint8_t last_notified = 0xFF;  // 0xFF 不可能来自合法读数，保证首读必通知
    uint32_t tick = 0;

    for (;;) {
        if (tick % 20 == 0) {  // 20 步 * 50 ms = 1 s
            uint8_t soc = 0;
            if (apm_battery_read(&soc) == ESP_OK) {
                s_soc = soc;
                s_have_soc = true;
                bool low = soc <= APM_BATTERY_LOW_SOC_PCT;
                if (low != s_low) {
                    s_low = low;
                    ESP_LOGI(TAG, "电量 %u%%%s", soc,
                             low ? "（低电量：按任意键背光短闪提示）" : "");
                }
                if (soc != last_notified) {
                    // 只在读到合法值后才调：缓存（读特征值回的是它）不会被污染。
                    apm_link_notify_battery(soc);
                    last_notified = soc;
                }
            }
        }

        // 闪烁请求由按键回调置位，这里按步消费；走完或非低电都回到 0。
        uint32_t remaining = s_flash_steps;
        if (remaining > 0 && s_low) {
            bl_duty(bl_flash_duty(remaining));
            s_flash_steps = remaining - 1;
        } else {
            bl_duty(0);
            s_flash_steps = 0;
        }

        tick++;
        vTaskDelay(pdMS_TO_TICKS(APM_BL_STEP_MS));
    }
}

esp_err_t apm_battery_start(void)
{
    esp_err_t err = apm_battery_init();
    if (err != ESP_OK) return err;

    bl_init();

    if (xTaskCreate(battery_task, "apm_batt", 3072, NULL, 4, NULL) != pdPASS) {
        ESP_LOGE(TAG, "电量任务创建失败");
        return ESP_FAIL;
    }
    return ESP_OK;
}
