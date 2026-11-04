// device/test/test_protocol.c
// protocol.c 的主机侧单测：不依赖 ESP-IDF，用系统 clang 直接编译运行。
//
//   ./scripts/test-protocol.sh
//
// 覆盖：CRC 已知向量、控制帧往返与四类错误、ADPCM 编解码往返 SNR、
// 静音块收敛、丢块自同步（解码只依赖块头）、块长常量。
#include "protocol.h"

#include <math.h>
#include <stdio.h>
#include <string.h>

static int g_fail = 0;
static int g_pass = 0;

#define CHECK(cond, fmt, ...)                                                    \
    do {                                                                         \
        if (cond) {                                                              \
            g_pass++;                                                            \
            printf("  ok   " fmt "\n", ##__VA_ARGS__);                           \
        } else {                                                                 \
            g_fail++;                                                            \
            printf("  FAIL " fmt "  (%s:%d)\n", ##__VA_ARGS__, __FILE__, __LINE__); \
        }                                                                        \
    } while (0)

static void test_crc(void)
{
    printf("[crc]\n");
    const char *v = "123456789";
    uint16_t crc = apm_crc16_ccitt((const uint8_t *)v, 9);
    CHECK(crc == 0x29B1, "CRC-16/CCITT-FALSE(\"123456789\") = 0x%04X (期望 0x29B1)", crc);
}

static void test_frame_roundtrip(void)
{
    printf("[frame]\n");
    uint8_t buf[APM_FRAME_MAX_LEN];

    // 空 payload
    size_t n = apm_frame_encode(APM_MSG_START, NULL, 0, buf, sizeof buf);
    CHECK(n == APM_FRAME_MIN_LEN, "空载荷帧长 = %zu（期望 %d）", n, APM_FRAME_MIN_LEN);
    apm_frame_t f;
    int r = apm_frame_decode(buf, n, &f);
    CHECK(r == (int)n && f.type == APM_MSG_START && f.payload_len == 0,
          "空载荷帧解码 type=0x%02X len=%u", f.type, f.payload_len);

    // 带载荷
    uint8_t payload[8];
    size_t plen = apm_build_status(payload, sizeof payload, APM_STATE_CAPTURING,
                                   APM_FLAG_BLE_CONNECTED | APM_FLAG_AUDIO_SUBSCRIBED,
                                   7, 123456);
    CHECK(plen == 8, "status 载荷长 = %zu", plen);
    n = apm_frame_encode(APM_MSG_STATUS, payload, plen, buf, sizeof buf);
    CHECK(n == APM_FRAME_MIN_LEN + 8, "status 帧长 = %zu", n);
    r = apm_frame_decode(buf, n, &f);
    CHECK(r == (int)n && f.type == APM_MSG_STATUS && f.payload_len == 8,
          "status 帧解码 type=0x%02X len=%u", f.type, f.payload_len);
    CHECK(memcmp(f.payload, payload, 8) == 0, "status 载荷逐字节一致");

    uint32_t sent = (uint32_t)f.payload[4] | ((uint32_t)f.payload[5] << 8) |
                    ((uint32_t)f.payload[6] << 16) | ((uint32_t)f.payload[7] << 24);
    CHECK(sent == 123456u, "blocks_sent 小端解码 = %u", sent);

    // 事件帧
    size_t elen = apm_build_event(payload, sizeof payload, APM_BTN_OK, APM_BTN_EV_PRESS, 0);
    n = apm_frame_encode(APM_MSG_EVENT, payload, elen, buf, sizeof buf);
    r = apm_frame_decode(buf, n, &f);
    CHECK(r == (int)n && f.payload[0] == APM_BTN_OK && f.payload[1] == APM_BTN_EV_PRESS,
          "事件帧 btn=%u ev=%u", f.payload[0], f.payload[1]);

    // 容量不足必须返回 0，不能写出界
    CHECK(apm_frame_encode(APM_MSG_START, NULL, 0, buf, 4) == 0, "容量不足返回 0");
    uint8_t big[APM_FRAME_MAX_PAYLOAD + 1] = { 0 };
    CHECK(apm_frame_encode(APM_MSG_START, big, sizeof big, buf, sizeof buf) == 0,
          "payload 超限返回 0");
}

static void test_frame_errors(void)
{
    printf("[frame errors]\n");
    uint8_t buf[APM_FRAME_MAX_LEN];
    size_t n = apm_frame_encode(APM_MSG_START, NULL, 0, buf, sizeof buf);
    apm_frame_t f;

    CHECK(apm_frame_decode(buf, 3, &f) == APM_FRAME_ERR_SHORT, "截断到 3 字节 -> ERR_SHORT");

    uint8_t bad_magic[APM_FRAME_MAX_LEN];
    memcpy(bad_magic, buf, n);
    bad_magic[1] = 0x00;
    CHECK(apm_frame_decode(bad_magic, n, &f) == APM_FRAME_ERR_MAGIC, "magic 错误 -> ERR_MAGIC");

    uint8_t bad_ver[APM_FRAME_MAX_LEN];
    memcpy(bad_ver, buf, n);
    bad_ver[2] = 2;
    CHECK(apm_frame_decode(bad_ver, n, &f) == APM_FRAME_ERR_VERSION,
          "版本 2 -> ERR_VERSION（不做静默兼容）");

    uint8_t bad_len[APM_FRAME_MAX_LEN];
    memcpy(bad_len, buf, n);
    bad_len[4] = 200;  // 声明 200 字节载荷，实际没有
    CHECK(apm_frame_decode(bad_len, n, &f) == APM_FRAME_ERR_LEN, "声明长度越界 -> ERR_LEN");

    uint8_t bad_crc[APM_FRAME_MAX_LEN];
    memcpy(bad_crc, buf, n);
    bad_crc[3] ^= 0xFF;  // 改类型但不改 CRC
    CHECK(apm_frame_decode(bad_crc, n, &f) == APM_FRAME_ERR_CRC, "篡改 type -> ERR_CRC");
}

// 1 kHz 正弦，幅度 0.4 满量程，320 采样（20 ms）
static void fill_sine(int16_t *pcm, size_t samples, double freq, double amp, size_t offset)
{
    for (size_t i = 0; i < samples; i++) {
        double t = (double)(offset + i) / (double)APM_AUDIO_SAMPLE_RATE;
        pcm[i] = (int16_t)lrint(amp * 32767.0 * sin(2.0 * M_PI * freq * t));
    }
}

static double snr_db(const int16_t *ref, const int16_t *dec, size_t n)
{
    double sig = 0.0, err = 0.0;
    for (size_t i = 0; i < n; i++) {
        double s = ref[i];
        double e = (double)dec[i] - (double)ref[i];
        sig += s * s;
        err += e * e;
    }
    if (err == 0.0) return 99.0;
    return 10.0 * log10(sig / err);
}

static void test_adpcm(void)
{
    printf("[adpcm]\n");
    CHECK(APM_AUDIO_BLOCK_LEN == 165, "音频块长 = %d（期望 165）", APM_AUDIO_BLOCK_LEN);
    CHECK(APM_AUDIO_BLOCK_SAMPLES / 2 == APM_AUDIO_BLOCK_DATA_LEN, "nibble 打包长度一致");

    int16_t pcm[APM_AUDIO_BLOCK_SAMPLES];
    int16_t dec[APM_AUDIO_BLOCK_SAMPLES];
    uint8_t block[APM_AUDIO_BLOCK_LEN];

    // 稳态：先跑两块建立状态，再测第三块
    apm_adpcm_state_t enc;
    apm_adpcm_reset(&enc);
    double worst_snr = 99.0;
    for (int k = 0; k < 4; k++) {
        fill_sine(pcm, APM_AUDIO_BLOCK_SAMPLES, 1000.0, 0.4,
                  (size_t)k * APM_AUDIO_BLOCK_SAMPLES);
        size_t n = apm_adpcm_encode_block(&enc, pcm, APM_AUDIO_BLOCK_SAMPLES, block);
        CHECK(n == APM_AUDIO_BLOCK_LEN, "第 %d 块编码长度 = %zu", k, n);
        apm_adpcm_decode_block(block, dec, APM_AUDIO_BLOCK_SAMPLES);
        double s = snr_db(pcm, dec, APM_AUDIO_BLOCK_SAMPLES);
        if (k >= 2 && s < worst_snr) worst_snr = s;
        printf("       block %d SNR = %.2f dB\n", k, s);
    }
    CHECK(worst_snr >= 18.0, "稳态 ADPCM 往返 SNR = %.2f dB（阈值 18 dB）", worst_snr);

    // 静音块应收敛到 0 附近，不出现爆音
    int16_t silence[APM_AUDIO_BLOCK_SAMPLES] = { 0 };
    apm_adpcm_reset(&enc);
    apm_adpcm_encode_block(&enc, silence, APM_AUDIO_BLOCK_SAMPLES, block);
    apm_adpcm_decode_block(block, dec, APM_AUDIO_BLOCK_SAMPLES);
    int max_abs = 0;
    for (size_t i = 0; i < APM_AUDIO_BLOCK_SAMPLES; i++) {
        int v = dec[i] < 0 ? -dec[i] : dec[i];
        if (v > max_abs) max_abs = v;
    }
    CHECK(max_abs <= 8, "静音块解码最大绝对值 = %d（期望 <= 8）", max_abs);

    // 满量程方波：必须钳位，不允许整数溢出回绕
    int16_t loud[APM_AUDIO_BLOCK_SAMPLES];
    for (size_t i = 0; i < APM_AUDIO_BLOCK_SAMPLES; i++) {
        loud[i] = (i < APM_AUDIO_BLOCK_SAMPLES / 2) ? 32767 : -32768;
    }
    apm_adpcm_reset(&enc);
    apm_adpcm_encode_block(&enc, loud, APM_AUDIO_BLOCK_SAMPLES, block);
    apm_adpcm_decode_block(block, dec, APM_AUDIO_BLOCK_SAMPLES);
    CHECK(dec[APM_AUDIO_BLOCK_SAMPLES - 1] < 0, "满量程方波末段为负（无回绕）");
    int mono_rise = 0;
    for (size_t i = 1; i < APM_AUDIO_BLOCK_SAMPLES; i++) {
        if ((dec[i - 1] < 0) != (dec[i] < 0)) mono_rise++;
    }
    CHECK(mono_rise == 1, "满量程方波只翻转 1 次（实际 %d）", mono_rise);
}

static void test_adpcm_lost_block(void)
{
    printf("[adpcm lost-block self sync]\n");
    uint8_t blocks[3][APM_AUDIO_BLOCK_LEN];
    int16_t pcm[APM_AUDIO_BLOCK_SAMPLES];
    int16_t in_order[APM_AUDIO_BLOCK_SAMPLES];
    int16_t only_third[APM_AUDIO_BLOCK_SAMPLES];

    apm_adpcm_state_t enc;
    apm_adpcm_reset(&enc);
    for (int k = 0; k < 3; k++) {
        fill_sine(pcm, APM_AUDIO_BLOCK_SAMPLES, 800.0, 0.3,
                  (size_t)k * APM_AUDIO_BLOCK_SAMPLES);
        apm_adpcm_encode_block(&enc, pcm, APM_AUDIO_BLOCK_SAMPLES, blocks[k]);
        blocks[k][0] = (uint8_t)k;  // 模拟固件填 seq
    }

    // 顺序解第 3 块（先解前两块建立状态）
    for (int k = 0; k < 3; k++) {
        apm_adpcm_decode_block(blocks[k], (k == 2) ? in_order : only_third,
                               APM_AUDIO_BLOCK_SAMPLES);
    }
    // 只解第 3 块（模拟前两块丢了）
    apm_adpcm_decode_block(blocks[2], only_third, APM_AUDIO_BLOCK_SAMPLES);

    CHECK(memcmp(in_order, only_third, sizeof in_order) == 0,
          "丢块后第 3 块解码结果与顺序解码逐采样一致（块头自同步）");
}

static void test_pcm_corrupt(void)
{
    printf("[pcm-corrupt]\n");
    static int16_t pcm[APM_AUDIO_BLOCK_SAMPLES];

    // 1) 正常语音：不能误判
    fill_sine(pcm, APM_AUDIO_BLOCK_SAMPLES, 300.0, 0.5, 0);
    CHECK(!apm_pcm_is_corrupt(pcm, APM_AUDIO_BLOCK_SAMPLES), "正常正弦不算病态");

    // 2) 麦克风削波：平顶、同号连续 —— 是真实声音，不能误判
    for (int i = 0; i < APM_AUDIO_BLOCK_SAMPLES; i++) {
        pcm[i] = (i / 40) % 2 ? -32768 : 32767;
    }
    CHECK(!apm_pcm_is_corrupt(pcm, APM_AUDIO_BLOCK_SAMPLES), "平顶削波不算病态");

    // 3) 真机观察到的形态：满量程且逐样本变号
    for (int i = 0; i < APM_AUDIO_BLOCK_SAMPLES; i++) {
        pcm[i] = (i % 2) ? -32768 : 32767;
    }
    CHECK(apm_pcm_is_corrupt(pcm, APM_AUDIO_BLOCK_SAMPLES), "满量程逐样本变号判为病态");

    // 4) 阈值边界：刚好 APM_PCM_CORRUPT_RUN 个才触发
    memset(pcm, 0, sizeof pcm);
    for (int i = 0; i < APM_PCM_CORRUPT_RUN - 1; i++) {
        pcm[i] = (i % 2) ? -32768 : 32767;
    }
    CHECK(!apm_pcm_is_corrupt(pcm, APM_AUDIO_BLOCK_SAMPLES), "%d 个样本还不够触发",
          APM_PCM_CORRUPT_RUN - 1);
    pcm[APM_PCM_CORRUPT_RUN - 1] =
        ((APM_PCM_CORRUPT_RUN - 1) % 2) ? -32768 : 32767;
    CHECK(apm_pcm_is_corrupt(pcm, APM_AUDIO_BLOCK_SAMPLES), "%d 个样本触发",
          APM_PCM_CORRUPT_RUN);

    // 5) 静音不能误判
    memset(pcm, 0, sizeof pcm);
    CHECK(!apm_pcm_is_corrupt(pcm, APM_AUDIO_BLOCK_SAMPLES), "静音不算病态");
}

int main(void)
{
    printf("protocol host tests\n");
    test_crc();
    test_frame_roundtrip();
    test_frame_errors();
    test_adpcm();
    test_adpcm_lost_block();
    test_pcm_corrupt();
    printf("\n%d passed, %d failed\n", g_pass, g_fail);
    return g_fail == 0 ? 0 : 1;
}
