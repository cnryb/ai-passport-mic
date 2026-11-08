// device/test/gen_vectors.c
// 生成跨端共享测试向量：config/protocol-vectors.json
//
// 为什么要有这个文件：固件是 C，桌面端是 Rust，两份实现"看起来一样"没有意义，
// 必须用同一组输入跑出同一组字节。这里从 C 实现导出 CRC / 帧 / ADPCM 的输出，
// Rust 侧 tests/c_vectors.rs 读同一份 JSON 逐字节比对。
//
// 用法: ./scripts/gen-protocol-vectors.sh
#include "protocol.h"

#include <math.h>
#include <stdio.h>
#include <string.h>

static void fill_sine(int16_t *pcm, size_t n, double freq, double amp, size_t offset)
{
    for (size_t i = 0; i < n; i++) {
        double t = (double)(offset + i) / (double)APM_AUDIO_SAMPLE_RATE;
        pcm[i] = (int16_t)lrint(amp * 32767.0 * sin(2.0 * M_PI * freq * t));
    }
}

static void print_hex(const uint8_t *data, size_t len)
{
    for (size_t i = 0; i < len; i++) printf("%02x", data[i]);
}

static void print_int16_array(const int16_t *data, size_t len)
{
    printf("[");
    for (size_t i = 0; i < len; i++) {
        printf("%s%d", i ? "," : "", data[i]);
    }
    printf("]");
}

int main(void)
{
    uint8_t frame[APM_FRAME_MAX_LEN];
    uint8_t payload[8];
    uint8_t block[APM_AUDIO_BLOCK_LEN];
    int16_t pcm[APM_AUDIO_BLOCK_SAMPLES];
    int16_t decoded[APM_AUDIO_BLOCK_SAMPLES];

    printf("{\n");
    printf("  \"schema\": \"ai-passport-mic-protocol-vectors-1\",\n");
    printf("  \"generator\": \"device/test/gen_vectors.c\",\n");

    // ---- CRC ----
    printf("  \"crc\": [\n");
    const char *vectors[] = { "123456789", "", "AI Passport Mic" };
    for (size_t i = 0; i < sizeof(vectors) / sizeof(vectors[0]); i++) {
        const char *v = vectors[i];
        size_t n = 0;
        while (v[n]) n++;
        uint16_t crc = apm_crc16_ccitt((const uint8_t *)v, n);
        printf("    {\"data\": \"%s\", \"crc\": %u}%s\n", v, (unsigned)crc,
               (i + 1 == sizeof(vectors) / sizeof(vectors[0])) ? "" : ",");
    }
    printf("  ],\n");

    // ---- 控制帧 ----
    printf("  \"frames\": [\n");

    size_t n = apm_frame_encode(APM_MSG_START, NULL, 0, frame, sizeof frame);
    printf("    {\"name\": \"start_empty\", \"msg\": %u, \"payload_hex\": \"\", \"frame_hex\": \"",
           (unsigned)APM_MSG_START);
    print_hex(frame, n);
    printf("\"},\n");

    size_t plen = apm_build_status(payload, sizeof payload, APM_STATE_CAPTURING,
                                   APM_FLAG_BLE_CONNECTED | APM_FLAG_AUDIO_SUBSCRIBED, 7,
                                   123456);
    n = apm_frame_encode(APM_MSG_STATUS, payload, plen, frame, sizeof frame);
    printf("    {\"name\": \"status_capturing\", \"msg\": %u, \"payload_hex\": \"",
           (unsigned)APM_MSG_STATUS);
    print_hex(payload, plen);
    printf("\", \"frame_hex\": \"");
    print_hex(frame, n);
    printf("\"},\n");

    plen = apm_build_event(payload, sizeof payload, APM_BTN_OK, APM_BTN_EV_DOUBLE, 0x04);
    n = apm_frame_encode(APM_MSG_EVENT, payload, plen, frame, sizeof frame);
    printf("    {\"name\": \"event_ok_double\", \"msg\": %u, \"payload_hex\": \"",
           (unsigned)APM_MSG_EVENT);
    print_hex(payload, plen);
    printf("\", \"frame_hex\": \"");
    print_hex(frame, n);
    printf("\"},\n");

    // PING：4 字节 seq，设备原样回显成 PONG
    payload[0] = 0x78;
    payload[1] = 0x56;
    payload[2] = 0x34;
    payload[3] = 0x12;
    n = apm_frame_encode(APM_MSG_PING, payload, 4, frame, sizeof frame);
    printf("    {\"name\": \"ping_seq_12345678\", \"msg\": %u, \"payload_hex\": \"",
           (unsigned)APM_MSG_PING);
    print_hex(payload, 4);
    printf("\", \"frame_hex\": \"");
    print_hex(frame, n);
    printf("\"},\n");

    plen = apm_build_status(payload, sizeof payload, APM_STATE_IDLE, 0, 0, 0);
    n = apm_frame_encode(APM_MSG_STATUS, payload, plen, frame, sizeof frame);
    printf("    {\"name\": \"status_idle\", \"msg\": %u, \"payload_hex\": \"",
           (unsigned)APM_MSG_STATUS);
    print_hex(payload, plen);
    printf("\", \"frame_hex\": \"");
    print_hex(frame, n);
    printf("\"},\n");

    // SET_KEY_LABELS：PC 下发按键标签。两端必须编出同一串字节。
    uint8_t label_payload[APM_FRAME_MAX_PAYLOAD];
    apm_key_labels_t labels = {
        .pc_pid      = 0x5A,
        .capture_btn = APM_BTN_OK,
        .count       = 2,
    };
    labels.items[0].btn = APM_BTN_UP;
    labels.items[0].len = 8;
    memcpy(labels.items[0].text, "ctrl+ent", 8);
    labels.items[1].btn = APM_BTN_DOWN;
    labels.items[1].len = 5;
    memcpy(labels.items[1].text, "esc*2", 5);
    plen = apm_key_labels_encode(&labels, label_payload, sizeof label_payload);
    n = apm_frame_encode(APM_MSG_SET_KEY_LABELS, label_payload, plen, frame, sizeof frame);
    printf("    {\"name\": \"key_labels\", \"msg\": %u, \"payload_hex\": \"",
           (unsigned)APM_MSG_SET_KEY_LABELS);
    print_hex(label_payload, plen);
    printf("\", \"frame_hex\": \"");
    print_hex(frame, n);
    printf("\"}\n");
    printf("  ],\n");

    // ---- ADPCM：两块连续，第二块的状态继承自第一块 ----
    printf("  \"adpcm\": [\n");
    apm_adpcm_state_t st;
    apm_adpcm_reset(&st);
    for (int k = 0; k < 2; k++) {
        fill_sine(pcm, APM_AUDIO_BLOCK_SAMPLES, 1000.0, 0.4,
                  (size_t)k * APM_AUDIO_BLOCK_SAMPLES);

        printf("    {\"index\": %d, \"state_before\": {\"predictor\": %d, \"index\": %d}, ",
               k, st.predictor, st.index);
        printf("\"pcm\": ");
        print_int16_array(pcm, APM_AUDIO_BLOCK_SAMPLES);

        apm_adpcm_encode_block(&st, pcm, APM_AUDIO_BLOCK_SAMPLES, block);

        printf(", \"block_hex\": \"");
        print_hex(block, APM_AUDIO_BLOCK_LEN);
        printf("\", ");

        apm_adpcm_decode_block(block, decoded, APM_AUDIO_BLOCK_SAMPLES);
        printf("\"decoded\": ");
        print_int16_array(decoded, APM_AUDIO_BLOCK_SAMPLES);

        printf(", \"state_after\": {\"predictor\": %d, \"index\": %d}}%s\n", st.predictor,
               st.index, (k == 1) ? "" : ",");
    }
    printf("  ]\n");
    printf("}\n");
    return 0;
}
