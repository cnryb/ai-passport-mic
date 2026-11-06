// device/main/app_main.c
// 应用入口：音频采集 -> ADPCM -> BLE 通知；按键事件 + PTT；STATUS 心跳。
//
// 数据通路的契约见 docs/ARCHITECTURE.md。这里的取舍是：
//   - 采集任务永远读 I2S（不读会让 DMA 溢出），只在"采集开启"时才编码并发送；
//   - 未连接或未订阅时块直接丢弃并计入丢失，不假装在录音；
//   - 按键回调只入队，真正的 BLE 通知在事件任务里做。
#include "audio.h"
#include "battery.h"
#include "board.h"
#include "buttons.h"
#include "link.h"
#include "protocol.h"

#include "esp_log.h"
#include "freertos/FreeRTOS.h"
#include "freertos/queue.h"
#include "freertos/task.h"
#include "nvs_flash.h"

#include <string.h>

static const char *TAG = "apm_app";

#define APM_EVT_QUEUE_LEN 16

typedef enum {
    APM_EVT_SRC_BUTTON = 0,
    APM_EVT_SRC_CTRL   = 1,
} apm_evt_src_t;

typedef struct {
    uint8_t  src;
    uint8_t  btn;
    uint8_t  btn_ev;
    uint8_t  ctrl_type;
    uint16_t ctrl_len;
    uint8_t  ctrl_payload[APM_FRAME_MAX_PAYLOAD];
} apm_evt_t;

static QueueHandle_t s_evt_q;

// 采集开关 = PTT 按住 或 PC 下发了 START。两者分开存：CTRL START 不应该被
// 一次 OK 抬手顶掉，PTT 也不应该被 CTRL STOP 之后的抬手"复活"。
static volatile bool s_ptt_held;
static volatile bool s_forced_capture;
static volatile uint32_t s_blocks_sent;
static volatile uint16_t s_blocks_dropped;
static volatile uint32_t s_blocks_congested;
static volatile uint32_t s_blocks_muted;
static volatile bool s_audio_error;

static bool capture_on(void) { return s_forced_capture || s_ptt_held; }

// 一条会话 = 从开始采集到停止采集。计数器按会话归零，否则 STATUS 里的
// dropped / sent 是**自设备启动起**的累计值，PC 端会把上一场的丢块算到这一场头上
// （KI-005 附录里踩过这个坑）。
static void reset_session_counters(void)
{
    s_blocks_sent = 0;
    s_blocks_dropped = 0;
    s_blocks_congested = 0;
}

// ---------------------------------------------------------------------------
// 按键回调：只入队
// ---------------------------------------------------------------------------
static void on_button(apm_btn_t btn, apm_btn_ev_t ev)
{
    // 低电量被动提示：平时是空操作，只置一个计数，不影响按键路径。
    apm_battery_on_button();

    apm_evt_t e = { 0 };
    e.src = APM_EVT_SRC_BUTTON;
    e.btn = (uint8_t)btn;
    e.btn_ev = (uint8_t)ev;
    // 队列满就丢：按键事件比音频块更能容忍延迟，但不能阻塞音频路径。
    if (xQueueSend(s_evt_q, &e, 0) != pdTRUE) {
        ESP_LOGW(TAG, "事件队列满，丢弃按键事件 btn=%u ev=%u", btn, ev);
    }
}

// ---------------------------------------------------------------------------
// CTRL 回调：运行在 NimBLE host 任务里，只入队
// ---------------------------------------------------------------------------
static void on_ctrl(uint8_t type, const uint8_t *payload, uint16_t len)
{
    apm_evt_t e = { 0 };
    e.src = APM_EVT_SRC_CTRL;
    e.ctrl_type = type;
    e.ctrl_len = len;
    if (len > sizeof e.ctrl_payload) {
        ESP_LOGW(TAG, "CTRL 载荷过长: %u", len);
        return;
    }
    if (len) memcpy(e.ctrl_payload, payload, len);
    if (xQueueSend(s_evt_q, &e, 0) != pdTRUE) {
        ESP_LOGW(TAG, "事件队列满，丢弃 CTRL type=0x%02X", type);
    }
}

// ---------------------------------------------------------------------------
// 链路状态回调：断链 = 上一条会话结束
// ---------------------------------------------------------------------------
// 这一段是 KI-005 的直接解药。以前 CTRL STOP 是唯一能清 s_forced_capture 的路径，
// 所以 PC 被 pkill / 蓝牙掉线 / 用户关掉 GUI 时，设备会带着 START 一直"在采集"：
//   - 新客户端一订阅 AUDIO 就收到音频，而它还没发 START（GUI 那次 450 ms 16 条告警）；
//   - STATUS 一直报 CAPTURING，PC 侧状态机对不上；
//   - 每 20 ms 白编码一块再去撞一个注定失败的 notify。
// 断链就该回 idle，不等 STOP。
static void on_link_state(bool connected)
{
    if (connected) {
        ESP_LOGI(TAG, "链路已连接，等待 CTRL START 或 PTT");
        return;
    }
    if (s_forced_capture || s_ptt_held) {
        ESP_LOGW(TAG, "断链：清掉采集态（START=%d PTT=%d），不等 STOP",
                 (int)s_forced_capture, (int)s_ptt_held);
    }
    s_forced_capture = false;
    s_ptt_held = false;
}

// ---------------------------------------------------------------------------
// 采集任务
// ---------------------------------------------------------------------------
static void audio_task(void *arg)
{
    (void)arg;

    static int16_t pcm[APM_AUDIO_BLOCK_SAMPLES];
    static uint8_t block[APM_AUDIO_BLOCK_LEN];
    apm_adpcm_state_t adpcm;
    apm_adpcm_reset(&adpcm);

    uint16_t seq = 0;
    bool was_capturing = false;

    for (;;) {
        if (!apm_audio_ready()) {
            vTaskDelay(pdMS_TO_TICKS(20));
            continue;
        }

        // 无论是否采集都要读：停读会让 I2S DMA 溢出，恢复时先听到一段旧数据。
        esp_err_t err = apm_audio_read(pcm, APM_AUDIO_BLOCK_SAMPLES);
        if (err != ESP_OK) {
            s_audio_error = true;
            vTaskDelay(pdMS_TO_TICKS(20));
            continue;
        }
        s_audio_error = false;

        if (!capture_on()) {
            was_capturing = false;
            continue;
        }

        if (!was_capturing) {
            // PTT 重新按下：中间样本没编码，跨块预测状态已经断开，必须复位。
            apm_adpcm_reset(&adpcm);
            was_capturing = true;
        }

        // 采集端病态数据：满量程且逐样本变号，这不是声音，是 I2S/ES8311 的瞬态
        // 读出（复现见 desktop/crates/protocol/tests/glitch_repro.rs）。
        // 直接编码出去就是对面听到的 0.8 s 爆音，所以在这里截住，压成静音。
        if (apm_pcm_is_corrupt(pcm, APM_AUDIO_BLOCK_SAMPLES)) {
            memset(pcm, 0, sizeof(int16_t) * APM_AUDIO_BLOCK_SAMPLES);
            // 病态状态不能带进后续块，否则解码端会跟着抖一段。
            apm_adpcm_reset(&adpcm);
            s_blocks_muted++;
            uint32_t now_ms = (uint32_t)(xTaskGetTickCount() * portTICK_PERIOD_MS);
            static uint32_t last_log_ms;
            if (now_ms - last_log_ms >= 1000) {
                last_log_ms = now_ms;
                ESP_LOGW(TAG, "采集端病态块已压静音（累计 %lu 块）",
                         (unsigned long)s_blocks_muted);
            }
        }

        if (apm_adpcm_encode_block(&adpcm, pcm, APM_AUDIO_BLOCK_SAMPLES, block) !=
            APM_AUDIO_BLOCK_LEN) {
            s_blocks_dropped++;
            continue;
        }

        block[0] = (uint8_t)(seq & 0xFF);
        block[1] = (uint8_t)((seq >> 8) & 0xFF);
        seq++;

        int rc = apm_link_notify_audio(block, APM_AUDIO_BLOCK_LEN);
        if (rc == APM_LINK_TX_OK) {
            s_blocks_sent++;
        } else {
            // 序号仍然递增：PC 端据此看到跳变，能区分"没说话"和"丢了块"。
            if (s_blocks_dropped < UINT16_MAX) s_blocks_dropped++;
            // 单独统计"协议栈推不动"：这是链路/连接参数的证据，不是设备没采。
            if (rc == APM_LINK_TX_CONGESTED) s_blocks_congested++;
        }
    }
}

// ---------------------------------------------------------------------------
// 事件任务：按键事件转发 + PTT + CTRL 命令
// ---------------------------------------------------------------------------
static apm_state_t current_state(void)
{
    if (!apm_audio_ready()) return APM_STATE_ERROR;
    if (capture_on()) return APM_STATE_CAPTURING;
    if (apm_link_connected()) return APM_STATE_READY;
    return APM_STATE_IDLE;
}

static uint8_t current_flags(void)
{
    uint8_t flags = 0;
    if (apm_link_connected()) flags |= APM_FLAG_BLE_CONNECTED;
    if (apm_link_audio_ready()) flags |= APM_FLAG_AUDIO_SUBSCRIBED;
    if (s_ptt_held) flags |= APM_FLAG_PTT_HELD;
    if (!apm_audio_ready() || s_audio_error) flags |= APM_FLAG_AUDIO_ERROR;
    // 连接间隔撑不住 50 块/秒时提前告诉 PC：这种会话的数据不能当音质/漂移结论用（KI-005）。
    if (apm_link_conn_itvl_ms() > APM_CONN_ITVL_OK_MS) flags |= APM_FLAG_LINK_SLOW;
    return flags;
}

static void handle_event(const apm_evt_t *e)
{
    if (e->src == APM_EVT_SRC_BUTTON) {
        apm_btn_t btn = (apm_btn_t)e->btn;
        apm_btn_ev_t ev = (apm_btn_ev_t)e->btn_ev;

        if (btn == APM_BTN_OK) {
            // OK 的按下/抬起默认就是 PTT 开关（见 config/README.md 的硬约束）。
            if (ev == APM_BTN_EV_PRESS) {
                if (!capture_on()) reset_session_counters();
                s_ptt_held = true;
            } else if (ev == APM_BTN_EV_RELEASE) {
                s_ptt_held = false;
            }
        }
        apm_link_notify_event(btn, ev, current_flags());
        return;
    }

    switch (e->ctrl_type) {
    case APM_MSG_START:
        if (!capture_on()) reset_session_counters();
        s_forced_capture = true;
        ESP_LOGI(TAG, "CTRL START（连接间隔 %u ms，MTU %u）",
                 apm_link_conn_itvl_ms(), apm_link_mtu());
        break;
    case APM_MSG_STOP:
        s_forced_capture = false;
        ESP_LOGI(TAG, "CTRL STOP");
        break;
    case APM_MSG_PING:
        // 延迟探针：原样回包，且只认 4 字节（不做任意长度透传）。
        if (e->ctrl_len == 4) {
            int rc = apm_link_notify_pong(e->ctrl_payload, e->ctrl_len);
            // 每个 PING 一行日志会把串口刷满，降到 DEBUG。
            ESP_LOGD(TAG, "CTRL PING -> PONG rc=%d", rc);
        } else {
            ESP_LOGW(TAG, "CTRL PING 载荷长度非法: %u（期望 4）", e->ctrl_len);
        }
        break;
    case APM_MSG_SET_GAIN:
        if (e->ctrl_len >= 1) {
            int gain = (int)(int8_t)e->ctrl_payload[0];
            esp_err_t err = apm_audio_set_gain(gain);
            ESP_LOGI(TAG, "CTRL SET_GAIN %d dB -> %s", gain, esp_err_to_name(err));
        }
        break;
    default:
        ESP_LOGW(TAG, "未知 CTRL type=0x%02X", e->ctrl_type);
        break;
    }
}

static void event_task(void *arg)
{
    (void)arg;
    apm_evt_t e;
    for (;;) {
        if (xQueueReceive(s_evt_q, &e, portMAX_DELAY) == pdTRUE) {
            handle_event(&e);
        }
    }
}

// ---------------------------------------------------------------------------
// STATUS 心跳
// ---------------------------------------------------------------------------
static void status_task(void *arg)
{
    (void)arg;
    uint32_t last_congested = 0;
    uint32_t last_log_ms = 0;

    for (;;) {
        // 心跳顺带做两件事：把跑偏的连接间隔拉回快速档；把"推不动"的证据打到串口。
        // KI-005 之所以查了两轮还只能靠猜，就是因为设备侧没有任何一行日志能说明
        // "块是采到了、编好了，卡在发不出去"。
        apm_link_ensure_fast_conn();

        apm_link_notify_status(current_state(), current_flags(),
                               s_blocks_dropped, s_blocks_sent);

        uint32_t now = (uint32_t)(xTaskGetTickCount() * portTICK_PERIOD_MS);
        uint32_t congested = s_blocks_congested;
        if (congested != last_congested) {
            if (now - last_log_ms >= 5000) {
                ESP_LOGW(TAG, "发送拥塞：累计 %u 块丢在协议栈（连接间隔 %u ms，MTU %u）",
                         (unsigned)congested, apm_link_conn_itvl_ms(), apm_link_mtu());
                last_congested = congested;
                last_log_ms = now;
            }
        } else {
            // 没拥塞时把窗口一直往后推，下一次拥塞第一秒就能报出来。
            last_log_ms = now;
        }

        vTaskDelay(pdMS_TO_TICKS(1000));
    }
}

// ---------------------------------------------------------------------------
void app_main(void)
{
    esp_err_t err = nvs_flash_init();
    if (err == ESP_ERR_NVS_NO_FREE_PAGES || err == ESP_ERR_NVS_NEW_VERSION_FOUND) {
        ESP_ERROR_CHECK(nvs_flash_erase());
        err = nvs_flash_init();
    }
    ESP_ERROR_CHECK(err);

    s_evt_q = xQueueCreate(APM_EVT_QUEUE_LEN, sizeof(apm_evt_t));
    if (!s_evt_q) {
        ESP_LOGE(TAG, "事件队列创建失败");
        return;
    }

    err = apm_audio_init();
    if (err != ESP_OK) {
        // 音频失败不阻止 BLE 起来：PC 端要能看到 ERROR 状态，而不是设备直接消失。
        ESP_LOGE(TAG, "音频初始化失败: %s（继续启动 BLE，STATUS 会上报 ERROR）",
                 esp_err_to_name(err));
    }

    // 先挂回调再启动 NimBLE：主机任务起来之后随时可能收到 CTRL 写。
    apm_link_set_ctrl_cb(on_ctrl);
    apm_link_set_conn_cb(on_link_state);
    err = apm_link_start(APM_DEVICE_NAME);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "BLE 启动失败: %s", esp_err_to_name(err));
    }

    err = apm_buttons_start(on_button);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "按键启动失败: %s", esp_err_to_name(err));
    }

    // 电量计失败不阻止启动：BLE 电量服务只是没有数据，其余功能不受影响。
    err = apm_battery_start();
    if (err != ESP_OK) {
        ESP_LOGW(TAG, "电量计启动失败: %s（Battery Service 将保持 0%）",
                 esp_err_to_name(err));
    }

    xTaskCreate(audio_task, "apm_audio", 4096, NULL, 9, NULL);
    xTaskCreate(event_task, "apm_event", 3072, NULL, 6, NULL);
    xTaskCreate(status_task, "apm_status", 3072, NULL, 5, NULL);

    ESP_LOGI(TAG, "启动完成：%s，%d Hz / %d bit / mono，块长 %d 字节",
             APM_DEVICE_NAME, APM_AUDIO_HZ, APM_AUDIO_BITS, APM_AUDIO_BLOCK_LEN);
}
