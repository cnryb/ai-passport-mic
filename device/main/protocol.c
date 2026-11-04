// device/main/protocol.c
// 协议实现：CRC16-CCITT、控制帧编解码、IMA-ADPCM。
//
// IMA-ADPCM 的 step/index 表来自 IMA 公开的 ADPCM 规范（DVI4 / IMA ADPCM），
// 编码器状态跨块继承，解码器每块由块头自同步 —— 这是"丢一块不影响后续音频"的前提。
#include "protocol.h"

#include <string.h>

// ---------------------------------------------------------------------------
// CRC16-CCITT (CRC-16/CCITT-FALSE: poly 0x1021, init 0xFFFF, 无反转, xorout 0)
// 校验向量："123456789" -> 0x29B1
// ---------------------------------------------------------------------------
uint16_t apm_crc16_ccitt(const uint8_t *data, size_t len)
{
    uint16_t crc = 0xFFFF;
    for (size_t i = 0; i < len; i++) {
        crc ^= (uint16_t)data[i] << 8;
        for (int bit = 0; bit < 8; bit++) {
            crc = (crc & 0x8000u) ? (uint16_t)((crc << 1) ^ 0x1021u) : (uint16_t)(crc << 1);
        }
    }
    return crc;
}

// ---------------------------------------------------------------------------
// 控制帧
// ---------------------------------------------------------------------------
size_t apm_frame_encode(uint8_t type, const uint8_t *payload, size_t payload_len,
                        uint8_t *out, size_t out_cap)
{
    if (!out) return 0;
    if (payload_len > APM_FRAME_MAX_PAYLOAD) return 0;
    if (payload_len > 0 && !payload) return 0;

    size_t total = APM_FRAME_MIN_LEN + payload_len;
    if (out_cap < total) return 0;

    out[0] = APM_FRAME_MAGIC0;
    out[1] = APM_FRAME_MAGIC1;
    out[2] = APM_PROTO_VERSION;
    out[3] = type;
    out[4] = (uint8_t)(payload_len & 0xFF);
    out[5] = (uint8_t)((payload_len >> 8) & 0xFF);
    if (payload_len) memcpy(&out[APM_FRAME_PREFIX_LEN], payload, payload_len);

    // CRC 覆盖 version..payload 末尾，即从 offset 2 起。
    uint16_t crc = apm_crc16_ccitt(&out[2], APM_FRAME_PREFIX_LEN - 2 + payload_len);
    out[APM_FRAME_PREFIX_LEN + payload_len]     = (uint8_t)(crc & 0xFF);
    out[APM_FRAME_PREFIX_LEN + payload_len + 1] = (uint8_t)((crc >> 8) & 0xFF);
    return total;
}

int apm_frame_decode(const uint8_t *buf, size_t len, apm_frame_t *out)
{
    if (!buf || !out) return APM_FRAME_ERR_SHORT;
    if (len < APM_FRAME_MIN_LEN) return APM_FRAME_ERR_SHORT;
    if (buf[0] != APM_FRAME_MAGIC0 || buf[1] != APM_FRAME_MAGIC1) return APM_FRAME_ERR_MAGIC;
    if (buf[2] != APM_PROTO_VERSION) return APM_FRAME_ERR_VERSION;

    uint16_t payload_len = (uint16_t)(buf[4] | ((uint16_t)buf[5] << 8));
    if (payload_len > APM_FRAME_MAX_PAYLOAD) return APM_FRAME_ERR_LEN;

    size_t total = APM_FRAME_MIN_LEN + payload_len;
    if (len < total) return APM_FRAME_ERR_LEN;

    uint16_t crc_calc = apm_crc16_ccitt(&buf[2], APM_FRAME_PREFIX_LEN - 2 + payload_len);
    uint16_t crc_wire = (uint16_t)(buf[APM_FRAME_PREFIX_LEN + payload_len] |
                                   ((uint16_t)buf[APM_FRAME_PREFIX_LEN + payload_len + 1] << 8));
    if (crc_calc != crc_wire) return APM_FRAME_ERR_CRC;

    out->version     = buf[2];
    out->type        = buf[3];
    out->payload_len = payload_len;
    out->payload     = payload_len ? &buf[APM_FRAME_PREFIX_LEN] : NULL;
    return (int)total;
}

// ---------------------------------------------------------------------------
// IMA-ADPCM
// ---------------------------------------------------------------------------
static const int16_t s_step_table[89] = {
        7,     8,     9,    10,    11,    12,    13,    14,    16,    17,
       19,    21,    23,    25,    28,    31,    34,    37,    41,    45,
       50,    55,    60,    66,    73,    80,    88,    97,   107,   118,
      130,   143,   157,   173,   190,   209,   230,   253,   279,   307,
      337,   371,   408,   449,   494,   544,   598,   658,   724,   796,
      876,   963,  1060,  1166,  1282,  1411,  1552,  1707,  1878,  2066,
     2272,  2499,  2749,  3024,  3327,  3660,  4026,  4428,  4871,  5358,
     5894,  6484,  7132,  7845,  8630,  9493, 10442, 11487, 12635, 13899,
    15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
};

static const int8_t s_index_table[8] = { -1, -1, -1, -1, 2, 4, 6, 8 };

void apm_adpcm_reset(apm_adpcm_state_t *st)
{
    if (!st) return;
    st->predictor = 0;
    st->index     = 0;
}

static uint8_t encode_nibble(apm_adpcm_state_t *st, int16_t sample)
{
    int step = s_step_table[st->index];

    int diff = (int)sample - (int)st->predictor;
    uint8_t sign = 0;
    if (diff < 0) {
        sign = 8;
        diff = -diff;
    }

    int vpdiff = step >> 3;
    uint8_t delta = 0;
    if (diff >= step) {
        delta |= 4;
        diff -= step;
        vpdiff += step;
    }
    step >>= 1;
    if (diff >= step) {
        delta |= 2;
        diff -= step;
        vpdiff += step;
    }
    step >>= 1;
    if (diff >= step) {
        delta |= 1;
        vpdiff += step;
    }

    int predictor = (int)st->predictor + (sign ? -vpdiff : vpdiff);
    if (predictor > 32767) predictor = 32767;
    if (predictor < -32768) predictor = -32768;
    st->predictor = (int16_t)predictor;

    int index = (int)st->index + s_index_table[delta];
    if (index < 0) index = 0;
    if (index > 88) index = 88;
    st->index = (int8_t)index;

    return (uint8_t)(sign | delta);
}

static int16_t decode_nibble(apm_adpcm_state_t *st, uint8_t nibble)
{
    int step = s_step_table[st->index];

    int vpdiff = step >> 3;
    if (nibble & 4) vpdiff += step;
    if (nibble & 2) vpdiff += step >> 1;
    if (nibble & 1) vpdiff += step >> 2;

    int predictor = (int)st->predictor + ((nibble & 8) ? -vpdiff : vpdiff);
    if (predictor > 32767) predictor = 32767;
    if (predictor < -32768) predictor = -32768;
    st->predictor = (int16_t)predictor;

    int index = (int)st->index + s_index_table[nibble & 7];
    if (index < 0) index = 0;
    if (index > 88) index = 88;
    st->index = (int8_t)index;

    return st->predictor;
}

size_t apm_adpcm_encode_block(apm_adpcm_state_t *st, const int16_t *pcm, size_t samples,
                              uint8_t *block)
{
    if (!st || !pcm || !block || (samples % 2) != 0) return 0;

    size_t data_len = samples / 2;
    // 块头写的是【本块编码开始前】的状态：解码端据此自同步，不依赖上一块。
    block[0] = 0;  // seq 由调用方填写
    block[1] = 0;
    block[2] = (uint8_t)((uint16_t)st->predictor & 0xFF);
    block[3] = (uint8_t)(((uint16_t)st->predictor >> 8) & 0xFF);
    block[4] = (uint8_t)st->index;

    uint8_t *out = &block[APM_AUDIO_BLOCK_HEADER_LEN];
    for (size_t i = 0; i < data_len; i++) {
        uint8_t lo = encode_nibble(st, pcm[i * 2]);
        uint8_t hi = encode_nibble(st, pcm[i * 2 + 1]);
        // 低 nibble 放偶数号采样，高 nibble 放奇数号采样。
        out[i] = (uint8_t)((lo & 0x0F) | ((hi & 0x0F) << 4));
    }
    return APM_AUDIO_BLOCK_HEADER_LEN + data_len;
}

void apm_adpcm_decode_block(const uint8_t *block, int16_t *pcm, size_t samples)
{
    if (!block || !pcm) return;

    apm_adpcm_state_t st;
    st.predictor = (int16_t)((uint16_t)block[2] | ((uint16_t)block[3] << 8));
    st.index     = (int8_t)block[4];
    if (st.index < 0) st.index = 0;
    if (st.index > 88) st.index = 88;

    const uint8_t *in = &block[APM_AUDIO_BLOCK_HEADER_LEN];
    for (size_t i = 0; i < samples / 2; i++) {
        pcm[i * 2]     = decode_nibble(&st, (uint8_t)(in[i] & 0x0F));
        pcm[i * 2 + 1] = decode_nibble(&st, (uint8_t)((in[i] >> 4) & 0x0F));
    }
}

// ---------------------------------------------------------------------------
// 采集端病态检测
// ---------------------------------------------------------------------------
bool apm_pcm_is_corrupt(const int16_t *pcm, size_t samples)
{
    if (!pcm) return false;

    int run = 0;
    int prev_sign = 0;
    for (size_t i = 0; i < samples; i++) {
        int16_t x = pcm[i];
        int sign = 0;
        bool sat = false;
        if (x >= APM_PCM_SATURATION) {
            sat = true;
            sign = 1;
        } else if (x <= -APM_PCM_SATURATION) {
            sat = true;
            sign = -1;
        }

        if (sat) {
            run = (prev_sign != 0 && sign != prev_sign) ? run + 1 : 1;
            prev_sign = sign;
        } else {
            run = 0;
            prev_sign = 0;
        }

        if (run >= APM_PCM_CORRUPT_RUN) return true;
    }
    return false;
}

// ---------------------------------------------------------------------------
// 载荷组装
// ---------------------------------------------------------------------------
size_t apm_build_status(uint8_t *payload, size_t cap, apm_state_t state, uint8_t flags,
                        uint16_t block_drops, uint32_t blocks_sent)
{
    if (!payload || cap < 8) return 0;
    payload[0] = (uint8_t)state;
    payload[1] = flags;
    payload[2] = (uint8_t)(block_drops & 0xFF);
    payload[3] = (uint8_t)((block_drops >> 8) & 0xFF);
    payload[4] = (uint8_t)(blocks_sent & 0xFF);
    payload[5] = (uint8_t)((blocks_sent >> 8) & 0xFF);
    payload[6] = (uint8_t)((blocks_sent >> 16) & 0xFF);
    payload[7] = (uint8_t)((blocks_sent >> 24) & 0xFF);
    return 8;
}

size_t apm_build_event(uint8_t *payload, size_t cap, apm_btn_t btn, apm_btn_ev_t ev,
                       uint8_t flags)
{
    if (!payload || cap < 3) return 0;
    payload[0] = (uint8_t)btn;
    payload[1] = (uint8_t)ev;
    payload[2] = flags;
    return 3;
}

size_t apm_build_pong(uint8_t *payload, size_t cap, const uint8_t *ping_payload,
                      size_t ping_len)
{
    // 只认 4 字节：设备不做任意长度透传，避免被当成数据反射器。
    if (!payload || !ping_payload || cap < 4 || ping_len != 4) return 0;
    memcpy(payload, ping_payload, 4);
    return 4;
}
