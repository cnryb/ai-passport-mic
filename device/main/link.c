// device/main/link.c
// NimBLE peripheral：单连接、自定义 GATT（CTRL / PROFILE / STATUS / EVENT / AUDIO）。
//
// 参考: FoloToy ai-passport main/demo_ble.c (MIT) —— 借用 NimBLE 在 C3 上的
//       初始化与广播/重广播的结构；GATT 服务、MTU 协商与音频通知是本项目自写。
#include "link.h"

#include "audio.h"
#include "board.h"
#include "power_policy.h"

#include "esp_bt.h"      // esp_ble_tx_power_set / esp_power_level_t（自适应发射功率）
#include "esp_log.h"
#include "esp_timer.h"
#include "host/ble_hs.h"
#include "host/util/util.h"
#include "nimble/nimble_port.h"
#include "nimble/nimble_port_freertos.h"
#include "services/gap/ble_svc_gap.h"
#include "services/gatt/ble_svc_gatt.h"

#include <string.h>

static const char *TAG = "apm_link";

// 单包含下 165 字节音频块：MTU 至少 168。取 247 留出 STATUS/EVENT 的余量。
#define APM_BLE_MTU            247
#define APM_BLE_MIN_AUDIO_MTU  (APM_AUDIO_BLOCK_LEN + 3)

// ---- 连接参数（KI-005 的根因就在这一段）------------------------------------
// 50 块/秒 × 165 字节 = 8.25 KB/s。这个速率只有在 15–30 ms 的连接间隔下才推得动；
// 间隔一旦被主机放到 500 ms 量级，协议栈里每次能塞进去的块屈指可数，notify 大面积
// 失败，PC 侧看到的就是"设备几乎不发"（实测 1.5–2 块/秒）。
//
// 所以这里做三件事：广播里带上从机首选参数、连上后延时请求、失败退避重试。
//
// **档位数值与判定全部在 power_policy.c**（纯函数，有主机侧单测）。这里只负责
// 把"该请求什么"翻译成 NimBLE 调用，不重复定义 15/30/80/40 这些数——
// 一份数值两处实现，改一处漏一处是这类改动最常见的翻车方式。
#define APM_CONN_UPD_FIRST_MS    600   // 连上立刻请求常被主机拒绝，先等一拍
#define APM_CONN_UPD_RETRY_MS    1000
#define APM_CONN_UPD_MAX_TRIES   6
#define APM_CONN_UPD_WATCHDOG_MS 5000  // 运行中每隔这么久检查一次实际间隔

// a7c52e00-31d4-4d2a-8f6b-5e9c1b3f7aNN（文本序）；BLE 侧按 LSB-first 存 16 字节。
// 展开成裸字节列表，才能直接喂给 BLE_UUID128_INIT（多一对花括号会变成标量初始化）。
#define APM_UUID128_BYTES(low) (low), 0x7a, 0x3f, 0x1b, 0x9c, 0x5e, 0x6b, 0x8f, \
                               0x2a, 0x4d, 0xd4, 0x31, 0x00, 0x2e, 0xc5, 0xa7

static const ble_uuid128_t s_svc_uuid  = BLE_UUID128_INIT(APM_UUID128_BYTES(0x00));
static const ble_uuid128_t s_ctrl_uuid = BLE_UUID128_INIT(APM_UUID128_BYTES(0x01));
static const ble_uuid128_t s_prof_uuid = BLE_UUID128_INIT(APM_UUID128_BYTES(0x02));
static const ble_uuid128_t s_stat_uuid = BLE_UUID128_INIT(APM_UUID128_BYTES(0x03));
static const ble_uuid128_t s_evt_uuid  = BLE_UUID128_INIT(APM_UUID128_BYTES(0x04));
static const ble_uuid128_t s_aud_uuid  = BLE_UUID128_INIT(APM_UUID128_BYTES(0x05));

// 标准 Battery Service（0x180F / 0x2A19，1 字节 = 百分比）。走标准 UUID 而不是
// 塞进自定义服务：系统（macOS 蓝牙面板等）与 GUI 都能免驱动读取。
static const ble_uuid16_t s_batt_svc_uuid = BLE_UUID16_INIT(0x180F);
static const ble_uuid16_t s_batt_chr_uuid = BLE_UUID16_INIT(0x2A19);

static uint16_t s_h_ctrl;
static uint16_t s_h_prof;
static uint16_t s_h_stat;
static uint16_t s_h_evt;
static uint16_t s_h_aud;
static uint16_t s_h_batt;

static uint16_t s_conn = BLE_HS_CONN_HANDLE_NONE;
static uint16_t s_mtu;
static uint8_t  s_own_addr_type;
static bool     s_sub_audio;
static bool     s_sub_status;
static bool     s_sub_event;
static const char *s_name = APM_DEVICE_NAME;

static apm_ctrl_cb_t s_ctrl_cb;
static apm_conn_cb_t s_conn_cb;

// 实际协商到的连接间隔（1.25 ms 为单位）；0 = 还没读到。
static uint16_t s_itvl_125us;
// 上次请求参数更新的时刻与次数，用于重试节流。
static uint32_t s_upd_req_ms;
static uint8_t  s_upd_tries;
static esp_timer_handle_t s_upd_timer;
// 当前是否在采集（决定请求哪一档间隔）。由 apm_link_set_capturing 维护，
// 断链时清回 false —— 下一条连接按空闲档起，等 START/PTT 再拉快。
static volatile bool s_capturing;
// 自适应发射功率：状态（当前档位 + 滞回计数）与上次评估时刻。
// 档位初值在 apm_link_start() 里用 apm_tx_init() 设成 sdkconfig 的开机默认
// （+9 dBm），与 `CONFIG_BT_CTRL_DFT_TX_POWER_LEVEL_P9` 保持一致。
static apm_tx_state_t s_tx;
static uint32_t s_tx_eval_ms;

static uint32_t now_ms(void) { return (uint32_t)(esp_timer_get_time() / 1000); }

static uint8_t  s_status_payload[8];
static uint16_t s_status_len;
// Battery Service 缓存。0xFF = 电量计不可用/还没读到过（BAS 合法值只有 0..100），
// 读路径对 >100 返回 ATT 错误，PC 端按"电量不可用"处理而不是误报 0%。
static uint8_t  s_battery_pct = 0xFF;

static int gatt_access(uint16_t conn_handle, uint16_t attr_handle,
                       struct ble_gatt_access_ctxt *ctxt, void *arg);
static int gap_event(struct ble_gap_event *event, void *arg);

// ---------------------------------------------------------------------------
// GATT
// ---------------------------------------------------------------------------
static const struct ble_gatt_chr_def s_chrs[] = {
    {
        .uuid       = &s_ctrl_uuid.u,
        .access_cb  = gatt_access,
        .arg        = (void *)1,  // CTRL
        .flags      = BLE_GATT_CHR_F_WRITE | BLE_GATT_CHR_F_WRITE_NO_RSP,
        .val_handle = &s_h_ctrl,
    },
    {
        .uuid       = &s_prof_uuid.u,
        .access_cb  = gatt_access,
        .arg        = (void *)2,  // PROFILE
        .flags      = BLE_GATT_CHR_F_READ,
        .val_handle = &s_h_prof,
    },
    {
        .uuid       = &s_stat_uuid.u,
        .access_cb  = gatt_access,
        .arg        = (void *)3,  // STATUS
        .flags      = BLE_GATT_CHR_F_READ | BLE_GATT_CHR_F_NOTIFY,
        .val_handle = &s_h_stat,
    },
    {
        .uuid       = &s_evt_uuid.u,
        .access_cb  = gatt_access,
        .arg        = (void *)4,  // EVENT
        .flags      = BLE_GATT_CHR_F_NOTIFY,
        .val_handle = &s_h_evt,
    },
    {
        .uuid       = &s_aud_uuid.u,
        .access_cb  = gatt_access,
        .arg        = (void *)5,  // AUDIO
        .flags      = BLE_GATT_CHR_F_NOTIFY,
        .val_handle = &s_h_aud,
    },
    { 0 },
};

static const struct ble_gatt_chr_def s_batt_chrs[] = {
    {
        .uuid       = &s_batt_chr_uuid.u,
        .access_cb  = gatt_access,
        .arg        = (void *)6,  // Battery Level
        .flags      = BLE_GATT_CHR_F_READ | BLE_GATT_CHR_F_NOTIFY,
        .val_handle = &s_h_batt,
    },
    { 0 },
};

static const struct ble_gatt_svc_def s_svcs[] = {
    {
        .type            = BLE_GATT_SVC_TYPE_PRIMARY,
        .uuid            = &s_svc_uuid.u,
        .characteristics = s_chrs,
    },
    {
        .type            = BLE_GATT_SVC_TYPE_PRIMARY,
        .uuid            = &s_batt_svc_uuid.u,
        .characteristics = s_batt_chrs,
    },
    { 0 },
};

static int gatt_access(uint16_t conn_handle, uint16_t attr_handle,
                       struct ble_gatt_access_ctxt *ctxt, void *arg)
{
    (void)conn_handle;
    (void)attr_handle;
    int which = (int)(intptr_t)arg;

    if (ctxt->op == BLE_GATT_ACCESS_OP_WRITE_CHR) {
        if (which != 1) return BLE_ATT_ERR_UNLIKELY;

        uint16_t len = OS_MBUF_PKTLEN(ctxt->om);
        if (len == 0 || len > APM_FRAME_MAX_LEN) {
            ESP_LOGW(TAG, "CTRL 写入长度非法: %u", len);
            return BLE_ATT_ERR_INVALID_ATTR_VALUE_LEN;
        }

        uint8_t frame[APM_FRAME_MAX_LEN];
        uint16_t copied = 0;
        if (os_mbuf_copydata(ctxt->om, 0, len, frame) != 0) {
            return BLE_ATT_ERR_UNLIKELY;
        }
        copied = len;

        apm_frame_t f;
        int rc = apm_frame_decode(frame, copied, &f);
        if (rc < 0) {
            ESP_LOGW(TAG, "CTRL 帧解码失败: %d", rc);
            return BLE_ATT_ERR_INVALID_ATTR_VALUE_LEN;
        }
        ESP_LOGD(TAG, "CTRL type=0x%02X len=%u", f.type, f.payload_len);
        if (s_ctrl_cb) s_ctrl_cb(f.type, f.payload, f.payload_len);
        return 0;
    }

    if (ctxt->op == BLE_GATT_ACCESS_OP_READ_CHR) {
        if (which == 2) {
            // PROFILE 第一版只读：协议版本、当前增益、采样率档位、保留。
            uint8_t prof[4] = { APM_PROTO_VERSION, (uint8_t)apm_audio_gain(), 0, 0 };
            return os_mbuf_append(ctxt->om, prof, sizeof prof) == 0
                       ? 0 : BLE_ATT_ERR_INSUFFICIENT_RES;
        }
        if (which == 3) {
            // 读 STATUS 必须回**完整帧**：通知路径是 apm_frame_encode 包过的，
            // 而 s_status_payload 只有 payload。之前直接回 payload，PC 端按帧解析
            // 就会报 "magic 不匹配"，每次连接第一次读都失败。
            uint16_t plen = s_status_len;
            if (plen == 0) {
                // 还没跑过 status_task：现造一个合法帧，别让对端拿 ATT 错误
                plen = (uint16_t)apm_build_status(s_status_payload, sizeof s_status_payload,
                                                  APM_STATE_IDLE, 0, 0, 0);
            }
            uint8_t frame[APM_FRAME_MAX_LEN];
            size_t flen = apm_frame_encode(APM_MSG_STATUS, s_status_payload, plen,
                                           frame, sizeof frame);
            if (flen == 0) return BLE_ATT_ERR_UNLIKELY;
            return os_mbuf_append(ctxt->om, frame, flen) == 0
                       ? 0 : BLE_ATT_ERR_INSUFFICIENT_RES;
        }
        if (which == 6) {
            // Battery Level（0x2A19）：1 字节百分比，原始值，不带帧头。
            //
            // 0xFF 表示"电量计还没给出有效值"。这里**必须原样返回 0xFF，绝不能回 ATT 错误**。
            // 2026-11-06 现场：回 BLE_ATT_ERR_UNLIKELY 时，macOS 的 btleplug `read()`
            // **挂住不返回**，PC 端 connect 卡在"读电量"这一步，界面永远停在"连接中"，
            // 而设备侧日志显示连接与订阅都已成功 —— 排查时极易误判成广播/连接问题。
            // 架构约定本来就是"0xFF / ATT 错误都算不可用"（docs/ARCHITECTURE.md），
            // 返回 0xFF 对两端都安全：PC 端用 `pct <= 100` 把它降级成"不可用"。
            return os_mbuf_append(ctxt->om, &s_battery_pct, 1) == 0
                       ? 0 : BLE_ATT_ERR_INSUFFICIENT_RES;
        }
        return BLE_ATT_ERR_UNLIKELY;
    }

    return BLE_ATT_ERR_UNLIKELY;
}

// ---------------------------------------------------------------------------
// 广播
// ---------------------------------------------------------------------------
// 0x0d「从机首选连接间隔」：min/max 各 2 字节小端，单位 1.25 ms。
// 12 = 15 ms、24 = 30 ms。放在广播里，主机在建连那一刻就有据可依，
// 不用等第一条 CTRL —— KI-005 的坏会话都是从"建连时参数就偏了"开始的。
static const uint8_t s_pref_itvl_range[4] = { 12, 0, 24, 0 };

static int advertise(void)
{
    struct ble_hs_adv_fields fields = { 0 };
    fields.flags = BLE_HS_ADV_F_DISC_GEN | BLE_HS_ADV_F_BREDR_UNSUP;
    fields.name = (const uint8_t *)s_name;
    fields.name_len = (uint8_t)strlen(s_name);
    fields.name_is_complete = 1;
    fields.slave_itvl_range = s_pref_itvl_range;

    int rc = ble_gap_adv_set_fields(&fields);
    if (rc != 0) {
        ESP_LOGE(TAG, "广播数据设置失败: %d", rc);
        return rc;
    }

    // 128 位服务 UUID 放扫描响应，避免广播包 31 字节放不下。
    struct ble_hs_adv_fields rsp = { 0 };
    rsp.uuids128 = &s_svc_uuid;
    rsp.num_uuids128 = 1;
    rsp.uuids128_is_complete = 1;
    rc = ble_gap_adv_rsp_set_fields(&rsp);
    if (rc != 0) {
        ESP_LOGE(TAG, "扫描响应设置失败: %d", rc);
        return rc;
    }

    struct ble_gap_adv_params params = { 0 };
    params.conn_mode = BLE_GAP_CONN_MODE_UND;
    params.disc_mode = BLE_GAP_DISC_MODE_GEN;
    params.itvl_min  = BLE_GAP_ADV_ITVL_MS(100);
    params.itvl_max  = BLE_GAP_ADV_ITVL_MS(150);

    rc = ble_gap_adv_start(s_own_addr_type, NULL, BLE_HS_FOREVER, &params, gap_event, NULL);
    if (rc == 0) {
        ESP_LOGI(TAG, "开始广播: %s", s_name);
    } else {
        ESP_LOGE(TAG, "广播启动失败: %d", rc);
    }
    return rc;
}

// ---------------------------------------------------------------------------
// 连接参数
// ---------------------------------------------------------------------------
static void refresh_itvl(void)
{
    struct ble_gap_conn_desc desc;
    if (s_conn == BLE_HS_CONN_HANDLE_NONE) {
        s_itvl_125us = 0;
        return;
    }
    if (ble_gap_conn_find(s_conn, &desc) == 0) s_itvl_125us = desc.conn_itvl;
}

// 前向声明：schedule_update 在定时器不可用时要退化成直接请求（见其注释）。
static void request_conn_params(void);

static void schedule_update(uint32_t delay_ms)
{
    // 定时器创建失败时（RAM 紧张）退化成就地请求：宁可失去"延迟一拍"的保护，
    // 也不能让切档指令被静默丢掉 —— 丢了就是空闲仍在 30 ms，或采集时停在 80 ms 掉块。
    if (!s_upd_timer) {
        request_conn_params();
        return;
    }
    esp_timer_stop(s_upd_timer);
    if (esp_timer_start_once(s_upd_timer, (uint64_t)delay_ms * 1000) != ESP_OK) {
        ESP_LOGW(TAG, "连接参数重试定时器启动失败");
    }
}

/// 按当前档位发一次参数更新请求。
///
/// 两档的目标值不同，但**参数相同**（latency=0、超时 4 s）：空闲档不用 peripheral
/// latency 换电，是因为 latency 会让设备在 N 个连接事件里完全不响应主机，
/// 一旦 PTT 按下需要立刻拉回快速档，主机的参数更新指令自己就会被 latency 拖住 ——
/// 那正是我们最不能慢的时刻。
static void request_conn_params(void)
{
    if (s_conn == BLE_HS_CONN_HANDLE_NONE) return;

    apm_conn_itvl_t want = apm_conn_itvl_for(s_capturing);

    struct ble_gap_upd_params up = {
        .itvl_min = BLE_GAP_CONN_ITVL_MS(want.itvl_min_ms),
        .itvl_max = BLE_GAP_CONN_ITVL_MS(want.itvl_max_ms),
        .latency  = 0,
        .supervision_timeout = 400,  // 4 s
    };
    s_upd_req_ms = now_ms();
    s_upd_tries++;

    int rc = ble_gap_update_params(s_conn, &up);
    if (rc == 0) {
        ESP_LOGI(TAG, "请求连接参数 %u-%u ms（%s，第 %u 次）",
                 want.itvl_min_ms, want.itvl_max_ms,
                 s_capturing ? "采集中" : "空闲", s_upd_tries);
    } else {
        ESP_LOGW(TAG, "连接参数请求失败: %d（第 %u 次）", rc, s_upd_tries);
        // 主机拒绝/忙的时候不能一次就放弃：KI-005 的坏会话就是"没重试"躺了一整场。
        //
        // **必须判 s_upd_timer 非空**：schedule_update 在没有定时器时会退化成直接调
        // request_conn_params，不加这个判断就成了无限递归（栈溢出而不是重试）。
        if (s_upd_timer && s_upd_tries < APM_CONN_UPD_MAX_TRIES) {
            schedule_update(APM_CONN_UPD_RETRY_MS);
        }
    }
}

static void on_update_timer(void *arg)
{
    (void)arg;
    request_conn_params();
}

// ---------------------------------------------------------------------------
// GAP
// ---------------------------------------------------------------------------
static int gap_event(struct ble_gap_event *event, void *arg)
{
    (void)arg;

    switch (event->type) {
    case BLE_GAP_EVENT_CONNECT:
        if (event->connect.status == 0) {
            s_conn = event->connect.conn_handle;
            s_itvl_125us = 0;
            s_upd_tries = 0;
            refresh_itvl();
            ESP_LOGI(TAG, "已连接 conn=%u itvl=%u ms", s_conn, apm_link_conn_itvl_ms());
            if (s_conn_cb) s_conn_cb(true);

            // 建连瞬间就请求参数更新，macOS 大概率拒（见 docs/GUI.md 实测），
            // 先等 APM_CONN_UPD_FIRST_MS 再发，失败由 request_conn_params 退避重试。
            schedule_update(APM_CONN_UPD_FIRST_MS);
        } else {
            ESP_LOGW(TAG, "连接失败 status=%d，重新广播", event->connect.status);
            advertise();
        }
        return 0;

    case BLE_GAP_EVENT_DISCONNECT:
        ESP_LOGI(TAG, "断开 reason=0x%02X", event->disconnect.reason);
        s_conn = BLE_HS_CONN_HANDLE_NONE;
        s_mtu = 0;
        s_itvl_125us = 0;
        s_upd_tries = 0;
        // 档位跟着会话一起清：下一条连进来时按"空闲"起，等 START/PTT 再拉快。
        s_capturing = false;
        if (s_upd_timer) esp_timer_stop(s_upd_timer);
        s_sub_audio = s_sub_status = s_sub_event = false;
        // 先告诉应用层"会话没了"，再重新广播：应用层要在这里清掉 START/PTT。
        if (s_conn_cb) s_conn_cb(false);
        advertise();
        return 0;

    case BLE_GAP_EVENT_ADV_COMPLETE:
        ESP_LOGI(TAG, "广播结束，重新开始");
        advertise();
        return 0;

    case BLE_GAP_EVENT_SUBSCRIBE:
        if (event->subscribe.attr_handle == s_h_aud) {
            s_sub_audio = event->subscribe.cur_notify;
        } else if (event->subscribe.attr_handle == s_h_stat) {
            s_sub_status = event->subscribe.cur_notify;
        } else if (event->subscribe.attr_handle == s_h_evt) {
            s_sub_event = event->subscribe.cur_notify;
        }
        ESP_LOGI(TAG, "订阅: audio=%d status=%d event=%d", s_sub_audio, s_sub_status,
                 s_sub_event);
        return 0;

    case BLE_GAP_EVENT_MTU:
        s_mtu = event->mtu.value;
        ESP_LOGI(TAG, "MTU = %u（音频块需要 >= %d）", s_mtu, APM_BLE_MIN_AUDIO_MTU);
        if (s_mtu < APM_BLE_MIN_AUDIO_MTU) {
            ESP_LOGW(TAG, "MTU 不足，音频块会被拆包，延迟与丢块都会变差");
        }
        return 0;

    case BLE_GAP_EVENT_CONN_UPDATE: {
        if (event->conn_update.status == 0) {
            refresh_itvl();
            s_upd_tries = 0;  // 生效了，把重试预算还回去，留给运行中的看门狗
            struct ble_gap_conn_desc desc;
            if (ble_gap_conn_find(event->conn_update.conn_handle, &desc) == 0) {
                ESP_LOGI(TAG, "连接参数已更新 itvl=%.1f ms latency=%u timeout=%u ms",
                         desc.conn_itvl * 1.25, desc.conn_latency,
                         (unsigned)desc.supervision_timeout * 10);
            }
            if (apm_link_conn_itvl_ms() > APM_CONN_ITVL_OK_MS) {
                ESP_LOGW(TAG, "连接间隔 %u ms 仍然超过 %d ms，50 块/秒会推不动",
                         apm_link_conn_itvl_ms(), APM_CONN_ITVL_OK_MS);
            }
        } else {
            refresh_itvl();
            ESP_LOGW(TAG, "连接参数更新被拒: %d（当前 %u ms，第 %u 次）",
                     event->conn_update.status, apm_link_conn_itvl_ms(), s_upd_tries);
            if (s_upd_tries < APM_CONN_UPD_MAX_TRIES) schedule_update(APM_CONN_UPD_RETRY_MS);
        }
        return 0;
    }

    case BLE_GAP_EVENT_REPEAT_PAIRING: {
        // 对端丢了绑定信息又要求重新配对：删掉旧绑定，允许重配。
        struct ble_gap_conn_desc desc;
        if (ble_gap_conn_find(event->repeat_pairing.conn_handle, &desc) == 0) {
            ble_store_util_delete_peer(&desc.peer_id_addr);
        }
        return BLE_GAP_REPEAT_PAIRING_RETRY;
    }

    default:
        return 0;
    }
}

// ---------------------------------------------------------------------------
// 通知
// ---------------------------------------------------------------------------
static int notify(uint16_t attr, const uint8_t *data, size_t len)
{
    if (s_conn == BLE_HS_CONN_HANDLE_NONE || attr == 0) return -1;

    struct os_mbuf *om = ble_hs_mbuf_from_flat(data, (uint16_t)len);
    if (!om) return -2;

    // ble_gatts_notify_custom 无论成败都接管 om，失败不要重复释放。
    int rc = ble_gatts_notify_custom(s_conn, attr, om);
    if (rc != 0) return -3;
    return 0;
}

int apm_link_notify_audio(const uint8_t *block, size_t len)
{
    if (!s_sub_audio) return APM_LINK_TX_NO_SINK;
    if (s_mtu && s_mtu < APM_BLE_MIN_AUDIO_MTU) return APM_LINK_TX_MTU;

    int rc = notify(s_h_aud, block, len);
    if (rc == 0) return APM_LINK_TX_OK;
    // -1 = 连接在两次检查之间断了；其余（mbuf 取不到 / 协议栈拒收）都算拥塞。
    return rc == -1 ? APM_LINK_TX_NO_SINK : APM_LINK_TX_CONGESTED;
}

int apm_link_notify_event(apm_btn_t btn, apm_btn_ev_t ev, uint8_t flags)
{
    if (!s_sub_event) return -1;
    uint8_t payload[3];
    size_t plen = apm_build_event(payload, sizeof payload, btn, ev, flags);
    if (plen == 0) return -2;

    uint8_t frame[APM_FRAME_MAX_LEN];
    size_t flen = apm_frame_encode(APM_MSG_EVENT, payload, plen, frame, sizeof frame);
    if (flen == 0) return -2;
    return notify(s_h_evt, frame, flen);
}

int apm_link_notify_pong(const uint8_t *seq, size_t len)
{
    // 与 EVENT 共用特征值：PONG 是控制面回包，不新开特征值（见 ARCHITECTURE.md）。
    if (!s_sub_event) return -1;

    uint8_t payload[4];
    size_t plen = apm_build_pong(payload, sizeof payload, seq, len);
    if (plen == 0) return -2;

    uint8_t frame[APM_FRAME_MAX_LEN];
    size_t flen = apm_frame_encode(APM_MSG_PONG, payload, plen, frame, sizeof frame);
    if (flen == 0) return -2;
    return notify(s_h_evt, frame, flen);
}

int apm_link_notify_status(apm_state_t state, uint8_t flags, uint16_t drops, uint32_t sent)
{
    s_status_len = (uint16_t)apm_build_status(s_status_payload, sizeof s_status_payload,
                                              state, flags, drops, sent);
    if (!s_sub_status) return -1;

    uint8_t frame[APM_FRAME_MAX_LEN];
    size_t flen = apm_frame_encode(APM_MSG_STATUS, s_status_payload, s_status_len,
                                   frame, sizeof frame);
    if (flen == 0) return -2;
    return notify(s_h_stat, frame, flen);
}

int apm_link_notify_battery(uint8_t pct)
{
    // 即使没人订阅也要更新缓存：特征值读路径回的是这份缓存。
    s_battery_pct = pct;
    if (s_conn == BLE_HS_CONN_HANDLE_NONE || s_h_batt == 0) return -1;
    return notify(s_h_batt, &pct, 1);
}

// ---------------------------------------------------------------------------
// 生命周期
// ---------------------------------------------------------------------------
static void on_sync(void)
{
    int rc = ble_hs_util_ensure_addr(0);
    if (rc == 0) rc = ble_hs_id_infer_auto(0, &s_own_addr_type);
    if (rc != 0) {
        ESP_LOGE(TAG, "地址推断失败: %d", rc);
        return;
    }
    rc = ble_att_set_preferred_mtu(APM_BLE_MTU);
    if (rc != 0) ESP_LOGW(TAG, "设置首选 MTU 失败: %d", rc);

    advertise();
}

static void on_reset(int reason)
{
    ESP_LOGE(TAG, "NimBLE host 复位 reason=%d", reason);
    s_conn = BLE_HS_CONN_HANDLE_NONE;
    s_sub_audio = s_sub_status = s_sub_event = false;
}

static void host_task(void *param)
{
    (void)param;
    nimble_port_run();
    nimble_port_freertos_deinit();
}

esp_err_t apm_link_start(const char *device_name)
{
    if (device_name) s_name = device_name;

    // 发射功率状态复位到 sdkconfig 的开机默认档（+9 dBm）。放在这里而不是静态初值，
    // 是为了让它和 apm_tx_init 里的常量保持单一来源。
    apm_tx_init(&s_tx);

    // 连接参数重试用的单次定时器（任务上下文，允许在回调里再启动自己）。
    if (!s_upd_timer) {
        const esp_timer_create_args_t args = {
            .callback = on_update_timer,
            .arg      = NULL,
            .name     = "apm_conn_upd",
        };
        esp_err_t terr = esp_timer_create(&args, &s_upd_timer);
        if (terr != ESP_OK) {
            ESP_LOGW(TAG, "连接参数定时器创建失败: %s（退化为只在建连时请求一次）",
                     esp_err_to_name(terr));
            s_upd_timer = NULL;
        }
    }

    esp_err_t err = nimble_port_init();

    if (err != ESP_OK) {
        ESP_LOGE(TAG, "nimble_port_init 失败: %s", esp_err_to_name(err));
        return err;
    }

    ble_hs_cfg.sync_cb  = on_sync;
    ble_hs_cfg.reset_cb = on_reset;
    // 第一版不强制配对：音频链路本身不承载隐私内容，先保证可用性。
    ble_hs_cfg.store_status_cb = ble_store_util_status_rr;

    ble_svc_gap_init();
    ble_svc_gatt_init();

    int rc = ble_svc_gap_device_name_set(s_name);
    if (rc != 0) {
        ESP_LOGE(TAG, "设置设备名失败: %d", rc);
        return ESP_FAIL;
    }

    rc = ble_gatts_count_cfg(s_svcs);
    if (rc != 0) return ESP_FAIL;
    rc = ble_gatts_add_svcs(s_svcs);
    if (rc != 0) return ESP_FAIL;

    nimble_port_freertos_init(host_task);

    // 把省电的实际状态打出来，**别拿 BLE_INIT 的 "Using main XTAL as clock source" 当证据**：
    // 那条日志在没开 modem sleep 时也会打印（`lpclk_sel` 的默认值就是 MAIN_XTAL），
    // 所以它证明不了 modem sleep 生效。这里查的是运行期真状态：
    //   - esp_bt_get_lpclk_src() 返回当前低功耗时钟源；
    //   - esp_bt_sleep_enable() 只有 sleep_mode == MODE_1 才返回 OK，
    //     用它探测"控制器到底有没有被配成 modem sleep"。
    // 探测本身是幂等的（已启用时再 enable 一次无害），但**不改配置**，只读。
    {
        esp_bt_sleep_clock_t clk = esp_bt_get_lpclk_src();
        const char *clk_name = "unknown";
        switch (clk) {
        case ESP_BT_SLEEP_CLOCK_MAIN_XTAL:  clk_name = "main XTAL"; break;
        case ESP_BT_SLEEP_CLOCK_EXT_32K_XTAL: clk_name = "ext 32k XTAL"; break;
        case ESP_BT_SLEEP_CLOCK_RTC_SLOW:   clk_name = "internal RC"; break;
        default: break;
        }
        // 用返回值判断"是不是 MODE_1"，而不是相信 sdkconfig。
        esp_err_t se = esp_bt_sleep_enable();
        ESP_LOGI(TAG, "省电状态：低功耗时钟=%s，modem sleep=%s（%s）",
                 clk_name,
                 se == ESP_OK ? "已启用" : "未启用",
                 esp_err_to_name(se));
        if (se != ESP_OK) {
            ESP_LOGW(TAG, "modem sleep 未生效！检查 CONFIG_BT_CTRL_MODEM_SLEEP / _MODE_1");
        }
    }

    ESP_LOGI(TAG, "NimBLE 已启动");
    return ESP_OK;
}

void apm_link_set_ctrl_cb(apm_ctrl_cb_t cb) { s_ctrl_cb = cb; }

void apm_link_set_conn_cb(apm_conn_cb_t cb) { s_conn_cb = cb; }

uint16_t apm_link_conn_itvl_ms(void)
{
    if (s_itvl_125us == 0) return 0;
    // 1.25 ms 为单位：ms = itvl * 5 / 4（四舍五入）
    return (uint16_t)((s_itvl_125us * 5u + 2u) / 4u);
}

bool apm_link_capturing(void) { return s_capturing; }

/// 切换档位。**只记录意图并立刻请求**，不假设主机会照办 —— 实际是否生效由
/// BLE_GAP_EVENT_CONN_UPDATE 与下一次心跳校对。
void apm_link_set_capturing(bool capturing)
{
    if (s_capturing == capturing) return;  // 幂等：START 与 PTT 可能同时置位
    s_capturing = capturing;

    if (s_conn == BLE_HS_CONN_HANDLE_NONE) return;

    // 切档是"这一刻就要生效"的动作，不能等心跳，也不能等退避定时器：
    // PTT 按下到首块音频只有 250 ms 预算，慢了就是开场卡顿。
    //
    // 但仍然走定时器而不是就地调用 `ble_gap_update_params`：这个函数可能从事件任务
    // 或按键回调里被调到，而本文件其余的参数请求全部在 esp_timer 任务上下文里发，
    // 保持单一签发点，避免"NimBLE host 正忙时从别的任务直接操作 GAP"这类偶发问题。
    s_upd_tries = 0;
    ESP_LOGI(TAG, "切到%s档连接参数", capturing ? "采集" : "空闲");
    schedule_update(1);
}

void apm_link_ensure_conn_params(void)
{
    if (s_conn == BLE_HS_CONN_HANDLE_NONE) return;
    refresh_itvl();

    uint16_t ms = apm_link_conn_itvl_ms();
    // 两档判据、以及"空闲态不判 LINK_SLOW"的理由都在 power_policy.c（有单测）。
    if (apm_conn_itvl_ok(s_capturing, ms)) return;

    // 已经有重试挂在定时器上就别叠一层，否则每次心跳都在发同一个请求。
    if (s_upd_timer && esp_timer_is_active(s_upd_timer)) return;
    if (now_ms() - s_upd_req_ms < APM_CONN_UPD_WATCHDOG_MS) return;

    if (s_capturing) {
        ESP_LOGW(TAG, "连接间隔 %u ms 撑不住 50 块/秒，重新请求快速参数", ms);
    } else {
        ESP_LOGI(TAG, "空闲间隔 %u ms 偏离 %d ms，重新请求省电档",
                 ms, APM_CONN_ITVL_IDLE_MS);
    }
    s_upd_tries = 0;
    request_conn_params();
}

// ---------------------------------------------------------------------------
// 自适应发射功率
// ---------------------------------------------------------------------------
// 连接间隔 30 ms、每周期 1–2 个 165 B 包，发射占空比只有百分之几，所以降功率对
// **平均电流**的影响是 mA 量级，不是省电主力（真正的大头是 modem sleep 与空闲档）。
// 它的价值是"缩短不必要的射程"：桌面上 1 m 内不需要 +9 dBm。
//
// 三档映射与滞回全在 power_policy.c（有主机侧单测，含边界抖动那条）。
// 这里只做两件事：把 dBm 翻成 NimBLE 的枚举、真的写寄存器。
#define APM_TX_EVAL_MS 5000   // 5 s 评一次，别让 RSSI 抖动带着功率抖

/// dBm -> NimBLE 功率枚举。三档之外的取值按最近的档处理（不 assert：
/// 一次断言在固件里就是整机重启，为了一个功率档不值得）。
static esp_power_level_t tx_level_from_dbm(int8_t dbm)
{
    if (dbm >= APM_TX_FAR_DBM)  return ESP_PWR_LVL_P9;
    if (dbm >= APM_TX_MID_DBM)  return ESP_PWR_LVL_P3;
    return ESP_PWR_LVL_N3;
}

static void apply_tx_power(int8_t dbm)
{
    esp_power_level_t lvl = tx_level_from_dbm(dbm);

    // 广播与已建立的连接分开设：`ESP_BLE_PWR_TYPE_DEFAULT` 不覆盖已存在的连接句柄。
    esp_err_t e1 = esp_ble_tx_power_set(ESP_BLE_PWR_TYPE_ADV, lvl);
    esp_err_t e2 = esp_ble_tx_power_set(ESP_BLE_PWR_TYPE_CONN_HDL0, lvl);
    if (e1 != ESP_OK || e2 != ESP_OK) {
        // 失败时**不回滚 s_tx**：下一次评估会再试一次（滞回逻辑认为已经在目标档，
        // 所以这里必须把状态退回去，否则永远不会重试）。
        ESP_LOGW(TAG, "发射功率切到 %d dBm 失败（adv=%s conn=%s），维持原值",
                 dbm, esp_err_to_name(e1), esp_err_to_name(e2));
        apm_tx_init(&s_tx);   // 回到开机默认档，让下一轮重新评估
        return;
    }
    ESP_LOGI(TAG, "发射功率 -> %d dBm", dbm);
}

/// 按当前 RSSI 选档。没有连接 / 读不到 RSSI 时什么都不做。
static void update_tx_power(void)
{
    if (s_conn == BLE_HS_CONN_HANDLE_NONE) return;
    uint32_t now = now_ms();
    if (now - s_tx_eval_ms < APM_TX_EVAL_MS) return;
    s_tx_eval_ms = now;

    int8_t rssi = 0;
    if (ble_gap_conn_rssi(s_conn, &rssi) != 0) return;

    // 决策（含"升立刻降要两次"的方向不对称）在纯函数里，见 test_power_policy.c。
    if (apm_tx_update(&s_tx, rssi)) {
        ESP_LOGD(TAG, "RSSI=%d dBm，发射功率切到 %d dBm", rssi, s_tx.dbm);
        apply_tx_power(s_tx.dbm);
    }
}

/// 每次心跳调一次：校对连接参数 + 评估发射功率。
void apm_link_housekeeping(void)
{
    apm_link_ensure_conn_params();
    update_tx_power();
}


bool apm_link_connected(void) { return s_conn != BLE_HS_CONN_HANDLE_NONE; }

bool apm_link_audio_ready(void)
{
    return apm_link_connected() && s_sub_audio && s_mtu >= APM_BLE_MIN_AUDIO_MTU;
}

uint16_t apm_link_mtu(void) { return s_mtu; }
