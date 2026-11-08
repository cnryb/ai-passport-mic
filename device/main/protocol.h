// device/main/protocol.h
// AI Passport Mic 跨端协议：控制帧、CRC16、IMA-ADPCM 音频块。
//
// 契约来源：docs/ARCHITECTURE.md。改动这里之前先改文档，两端同步。
// 本文件刻意不依赖 ESP-IDF，便于在主机上用 clang 直接编译跑单测
// （见 device/test/test_protocol.c 与 scripts/test-protocol.sh）。
#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

// ---------------------------------------------------------------------------
// 音频块
// ---------------------------------------------------------------------------
// 20 ms @ 16 kHz = 320 采样；IMA-ADPCM 4 bit/采样 = 160 字节；块头 5 字节。
#define APM_AUDIO_SAMPLE_RATE      16000
#define APM_AUDIO_BLOCK_MS         20
#define APM_AUDIO_BLOCK_SAMPLES    320
#define APM_AUDIO_BLOCK_DATA_LEN   160
#define APM_AUDIO_BLOCK_HEADER_LEN 5
#define APM_AUDIO_BLOCK_LEN        (APM_AUDIO_BLOCK_HEADER_LEN + APM_AUDIO_BLOCK_DATA_LEN)

// ---------------------------------------------------------------------------
// 控制帧
// ---------------------------------------------------------------------------
// [0x56 0x4D][version=1][type][len u16-LE][payload][CRC16-CCITT u16-LE]
// CRC 覆盖 version..payload 末尾（不含两字节 magic）。
#define APM_FRAME_MAGIC0        0x56
#define APM_FRAME_MAGIC1        0x4D
#define APM_PROTO_VERSION       1
#define APM_FRAME_PREFIX_LEN    6  // magic(2) + version(1) + type(1) + len(2)
#define APM_FRAME_CRC_LEN       2
#define APM_FRAME_MAX_PAYLOAD   64
#define APM_FRAME_MIN_LEN       (APM_FRAME_PREFIX_LEN + APM_FRAME_CRC_LEN)
#define APM_FRAME_MAX_LEN       (APM_FRAME_MAX_PAYLOAD + APM_FRAME_MIN_LEN)

// 帧类型。0x01-0x7F 为 PC -> 设备（走 CTRL 特征值），0x81-0xFF 为设备 -> PC。
typedef enum {
    APM_MSG_START    = 0x01,  // payload: 空。强制开始采集（不依赖 PTT）
    APM_MSG_STOP     = 0x02,  // payload: 空。强制停止采集
    APM_MSG_SET_GAIN = 0x03,  // payload: int8 麦克风 PGA 增益(dB, 0..42)
    APM_MSG_PING     = 0x04,  // payload: seq u32-LE。必须原样回 PONG，长度不是 4 就丢弃
    APM_MSG_SET_KEY_LABELS = 0x05,  // payload: 见 apm_key_labels_t（设备屏幕的按键标签）

    APM_MSG_STATUS   = 0x81,  // payload: state u8, flags u8, block_drops u16, blocks_sent u32
    APM_MSG_EVENT    = 0x82,  // payload: button u8, event u8, flags u8
    APM_MSG_PONG     = 0x83,  // payload: seq u32-LE 原样回传，用于延迟测量（走 EVENT 特征值）
} apm_msg_type_t;

// STATUS.payload[0]
typedef enum {
    APM_STATE_IDLE      = 0,
    APM_STATE_READY     = 1,
    APM_STATE_CAPTURING = 2,
    APM_STATE_ERROR     = 3,
} apm_state_t;

// STATUS.payload[1] 位定义
#define APM_FLAG_BLE_CONNECTED   0x01
#define APM_FLAG_AUDIO_SUBSCRIBED 0x02
#define APM_FLAG_PTT_HELD        0x04
#define APM_FLAG_AUDIO_ERROR     0x08
// bit4：实际连接间隔 > 40 ms，链路撑不住 50 块/秒（KI-005）。
// 置位时音频块会大量丢在协议栈里，这一场的数据不能当音质/漂移结论用。
#define APM_FLAG_LINK_SLOW       0x10

// EVENT.payload
typedef enum {
    APM_BTN_UP   = 0,
    APM_BTN_DOWN = 1,
    APM_BTN_OK   = 2,
} apm_btn_t;

// 与 config/README.md 的事件名对应：
// press -> ok.press / click -> *.short / double -> *.double / long -> *.long
typedef enum {
    APM_BTN_EV_PRESS   = 0,
    APM_BTN_EV_CLICK   = 1,
    APM_BTN_EV_DOUBLE  = 2,
    APM_BTN_EV_LONG    = 3,
    APM_BTN_EV_RELEASE = 4,
} apm_btn_ev_t;

typedef struct {
    uint8_t        version;
    uint8_t        type;
    uint16_t       payload_len;
    const uint8_t *payload;  // 指向解码入参缓冲区，不复制
} apm_frame_t;

// 解码结果
typedef enum {
    APM_FRAME_OK = 0,
    APM_FRAME_ERR_SHORT = -1,     // 不足最小帧长
    APM_FRAME_ERR_MAGIC = -2,     // magic 不匹配
    APM_FRAME_ERR_VERSION = -3,   // 版本不支持
    APM_FRAME_ERR_LEN = -4,       // 声明长度越界
    APM_FRAME_ERR_CRC = -5,       // CRC 校验失败
} apm_frame_result_t;

// IMA-ADPCM 编解码状态。编码器跨块继承；解码器每块用块头重置，因此丢块不会污染后续音频。
typedef struct {
    int16_t predictor;
    int8_t  index;  // 0..88
} apm_adpcm_state_t;

// ---- CRC ----
uint16_t apm_crc16_ccitt(const uint8_t *data, size_t len);

// ---- 控制帧 ----
// 返回写入 out 的字节数；0 表示失败（容量不足或 payload 超限）。
size_t apm_frame_encode(uint8_t type, const uint8_t *payload, size_t payload_len,
                        uint8_t *out, size_t out_cap);
// 成功返回帧总长（>0），失败返回 APM_FRAME_ERR_*（<0）。
int apm_frame_decode(const uint8_t *buf, size_t len, apm_frame_t *out);

// ---- ADPCM ----
void apm_adpcm_reset(apm_adpcm_state_t *st);
// 编码 samples 个采样为 block[0..4] 头 + nibbles，返回写入字节数（5 + samples/2）。
size_t apm_adpcm_encode_block(apm_adpcm_state_t *st, const int16_t *pcm, size_t samples,
                              uint8_t *block);
// 从块解出 samples 个采样；块头的 predictor/index 覆盖解码状态。
void apm_adpcm_decode_block(const uint8_t *block, int16_t *pcm, size_t samples);

// ---- 采集端病态检测 ----
// 真机上偶发 ±32767 逐样本变号的噪声，来自 I2S/ES8311 的瞬态读出，不是编解码算错
// （复现见 desktop/crates/protocol/tests/glitch_repro.rs）。
// 判据：连续 APM_PCM_CORRUPT_RUN 个样本 |x| >= APM_PCM_SATURATION 且逐样本变号。
// 人声与麦克风削波都是平顶、同号连续，不会命中。
#define APM_PCM_SATURATION 32000
#define APM_PCM_CORRUPT_RUN 8

// 返回 true 表示这段 PCM 是采集端病态数据，调用方必须丢弃而不是编码上链路。
bool apm_pcm_is_corrupt(const int16_t *pcm, size_t samples);

// ---- 状态帧组装（固件侧用，主机单测也覆盖）----
size_t apm_build_status(uint8_t *payload, size_t cap, apm_state_t state, uint8_t flags,
                        uint16_t block_drops, uint32_t blocks_sent);
size_t apm_build_event(uint8_t *payload, size_t cap, apm_btn_t btn, apm_btn_ev_t ev,
                       uint8_t flags);
// PONG 载荷 = PING 载荷原样回显。ping_len 必须为 4，否则返回 0（设备不做任意长度透传）。
size_t apm_build_pong(uint8_t *payload, size_t cap, const uint8_t *ping_payload,
                      size_t ping_len);

// ---------------------------------------------------------------------------
// SET_KEY_LABELS（0x05）：设备屏幕的按键标签
//
// 载荷（变长，最多 3 条，键的固定顺序 up / down / ok）：
//   [0] pc_pid u8      发起方进程标识的低 8 位（仅用于日志与"换了个 PC"的判断）
//   [1] capture_btn u8 本机开麦键：0=up 1=down 2=ok 0xFF=未知
//   [2] count u8       条目数 0..3
//   [3..] 每条： [btn u8][text_len u8][text: UTF-8，text_len 字节]
//
// 契约见 docs/ARCHITECTURE.md 的 SET_KEY_LABELS 节。要点：
//   - **设备不解析动作语义，只显示文本**（PC 负责把 ctrl+enter 翻成一句人话）；
//   - 文本长度上限 APM_KEY_LABEL_MAX_BYTES，超出部分丢弃；
//   - 缺字形的字符由屏幕层替换成 '?'，协议层不做字符校验。
// ---------------------------------------------------------------------------
#define APM_KEY_LABEL_MAX_BYTES 24
#define APM_KEY_LABEL_MAX_ITEMS 3

typedef struct {
    uint8_t pc_pid;
    uint8_t capture_btn;                       // 0..2，或 APM_KEY_LABEL_NO_CAPTURE
    uint8_t count;                             // 0..APM_KEY_LABEL_MAX_ITEMS
    struct {
        uint8_t btn;
        uint8_t len;
        char    text[APM_KEY_LABEL_MAX_BYTES];  // 不保证 '\0' 结尾；用 len
    } items[APM_KEY_LABEL_MAX_ITEMS];
} apm_key_labels_t;

#define APM_KEY_LABEL_NO_CAPTURE 0xFF

// 解析 SET_KEY_LABELS 载荷。
// 返回 true 表示整帧合法并已写入 *out（会先清零）。
// 返回 false 表示帧格式非法（长度不符 / count 越界 / 键号重复），**不做部分应用**：
// 半份标签比没有标签更容易让人误判。
bool apm_key_labels_decode(const uint8_t *payload, size_t len, apm_key_labels_t *out);

// 组装 SET_KEY_LABELS 载荷（PC 侧参考实现 + 跨端向量用）。
// 返回写入字节数；0 表示失败（容量不足 / 条目过多 / 单条文本超长）。
size_t apm_key_labels_encode(const apm_key_labels_t *in, uint8_t *out, size_t out_cap);
