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

    APM_MSG_STATUS   = 0x81,  // payload: state u8, flags u8, block_drops u16, blocks_sent u32
    APM_MSG_EVENT    = 0x82,  // payload: button u8, event u8, flags u8
    APM_MSG_PONG     = 0x83,  // payload: 4 字节序号原样回传，用于延迟测量
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

// ---- 状态帧组装（固件侧用，主机单测也覆盖）----
size_t apm_build_status(uint8_t *payload, size_t cap, apm_state_t state, uint8_t flags,
                        uint16_t block_drops, uint32_t blocks_sent);
size_t apm_build_event(uint8_t *payload, size_t cap, apm_btn_t btn, apm_btn_ev_t ev,
                       uint8_t flags);
