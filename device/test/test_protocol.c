// device/test/test_protocol.c
// protocol.c 的主机侧单测：不依赖 ESP-IDF，用系统 clang 直接编译运行。
//
//   ./scripts/test-protocol.sh
//
// 覆盖：CRC 已知向量、控制帧往返与四类错误、ADPCM 编解码往返 SNR、
// 静音块收敛、丢块自同步（解码只依赖块头）、块长常量、采集端病态检测、
// PING/PONG 的 4 字节回显与长度校验。
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

// PING/PONG：设备只做 4 字节回显，长度不对就丢弃（不做任意长度透传）。
static void test_ping_pong(void)
{
    printf("[ping/pong]\n");

    const uint8_t seq[4] = { 0x78, 0x56, 0x34, 0x12 };
    uint8_t payload[8];
    memset(payload, 0xA5, sizeof payload);

    size_t n = apm_build_pong(payload, sizeof payload, seq, sizeof seq);
    CHECK(n == 4, "4 字节 PING 回 4 字节 PONG（实际 %zu）", n);
    CHECK(memcmp(payload, seq, 4) == 0, "PONG 载荷是原样回显");

    // 帧层：PING 带的载荷能被对端解出来，PONG 在同一通道上可区分
    uint8_t frame[APM_FRAME_MAX_LEN];
    size_t flen = apm_frame_encode(APM_MSG_PING, seq, sizeof seq, frame, sizeof frame);
    CHECK(flen > 0, "PING 帧编码成功（%zu 字节）", flen);
    apm_frame_t f;
    CHECK(apm_frame_decode(frame, flen, &f) == (int)flen && f.type == APM_MSG_PING &&
              f.payload_len == 4 && memcmp(f.payload, seq, 4) == 0,
          "PING 帧往返一致");

    flen = apm_frame_encode(APM_MSG_PONG, payload, n, frame, sizeof frame);
    CHECK(apm_frame_decode(frame, flen, &f) == (int)flen && f.type == APM_MSG_PONG,
          "PONG 帧类型与 PING 可区分");

    // 长度校验：0 / 3 / 5 / 8 都不回
    CHECK(apm_build_pong(payload, sizeof payload, seq, 0) == 0, "长度 0 丢弃");
    CHECK(apm_build_pong(payload, sizeof payload, seq, 3) == 0, "长度 3 丢弃");
    CHECK(apm_build_pong(payload, sizeof payload, seq, 5) == 0, "长度 5 丢弃");
    CHECK(apm_build_pong(payload, 3, seq, 4) == 0, "输出容量不足丢弃");
    CHECK(apm_build_pong(NULL, sizeof payload, seq, 4) == 0, "空输出指针丢弃");
    CHECK(apm_build_pong(payload, sizeof payload, NULL, 4) == 0, "空载荷指针丢弃");
}

// SET_KEY_LABELS：PC 下发按键标签。格式见 docs/ARCHITECTURE.md。
static void test_key_labels(void)
{
    printf("[set_key_labels]\n");

    // 正常往返：三条都带文本。
    apm_key_labels_t in = {
        .pc_pid      = 0x5A,
        .capture_btn = APM_BTN_OK,
        .count       = 3,
        .items = {
            { APM_BTN_UP,   3, "F5 " },
            { APM_BTN_DOWN, 6, "esc x2" },
            { APM_BTN_OK,   0, "" },
        },
    };
    // 上面 "F5 " 有尾随空格，改成精确长度：F5 = 2 字节
    in.items[0].len = 2;
    in.items[1].len = 6;

    uint8_t payload[APM_FRAME_MAX_PAYLOAD];
    size_t n = apm_key_labels_encode(&in, payload, sizeof payload);
    CHECK(n == 3 + (2 + 2) + (2 + 6) + (2 + 0), "编码长度 = 3 + Σ(2+len)（实际 %zu）", n);

    apm_key_labels_t out;
    CHECK(apm_key_labels_decode(payload, n, &out), "解码成功");
    CHECK(out.pc_pid == 0x5A, "pc_pid 往返一致");
    CHECK(out.capture_btn == APM_BTN_OK, "capture_btn 往返一致");
    CHECK(out.count == 3, "条目数往返一致");
    CHECK(out.items[0].btn == APM_BTN_UP && out.items[0].len == 2 &&
              memcmp(out.items[0].text, "F5", 2) == 0,
          "UP 条目往返一致");
    CHECK(out.items[1].btn == APM_BTN_DOWN && out.items[1].len == 6 &&
              memcmp(out.items[1].text, "esc x2", 6) == 0,
          "DOWN 条目往返一致");
    CHECK(out.items[2].len == 0, "OK 条目空文本也能往返");

    // count = 0：清空成默认文案，是合法帧。
    apm_key_labels_t empty = { .pc_pid = 1, .capture_btn = APM_KEY_LABEL_NO_CAPTURE,
                               .count = 0 };
    n = apm_key_labels_encode(&empty, payload, sizeof payload);
    CHECK(n == 3, "count=0 的载荷是 3 字节");
    CHECK(apm_key_labels_decode(payload, n, &out) && out.count == 0,
          "count=0 解码成功（清空标签）");
    CHECK(out.capture_btn == APM_KEY_LABEL_NO_CAPTURE, "capture_btn=0xFF 表示未知");

    // ---- 非法帧一律整帧拒绝，不做部分应用 ----
    CHECK(!apm_key_labels_decode(payload, 2, &out), "长度 <3 拒绝");
    CHECK(!apm_key_labels_decode(NULL, 3, &out), "空指针拒绝");
    CHECK(!apm_key_labels_decode(payload, 3, NULL), "空输出指针拒绝");

    uint8_t bad[APM_FRAME_MAX_PAYLOAD];
    bad[0] = 0; bad[1] = 2; bad[2] = 4;   // count=4 > 3
    CHECK(!apm_key_labels_decode(bad, 3, &out), "count 超过 3 拒绝");

    bad[0] = 0; bad[1] = 2; bad[2] = 1; bad[3] = 9; bad[4] = 0;   // btn=9 非法
    CHECK(!apm_key_labels_decode(bad, 5, &out), "非法键号拒绝");

    bad[0] = 0; bad[1] = 2; bad[2] = 1; bad[3] = 0; bad[4] = 10;  // 声明 10 字节但没内容
    CHECK(!apm_key_labels_decode(bad, 5, &out), "text_len 超出实际长度拒绝");

    // 尾部多余字节：长度声明与实际内容不符。
    bad[0] = 0; bad[1] = 2; bad[2] = 0; bad[3] = 0xFF;
    CHECK(!apm_key_labels_decode(bad, 4, &out), "尾部多余字节拒绝");

    // 同一个键出现两次：自相矛盾的帧，拒绝而不是猜意图。
    bad[0] = 0; bad[1] = 2; bad[2] = 2;
    bad[3] = APM_BTN_UP; bad[4] = 1; bad[5] = 'a';
    bad[6] = APM_BTN_UP; bad[7] = 1; bad[8] = 'b';
    CHECK(!apm_key_labels_decode(bad, 9, &out), "重复键号拒绝");

    // 单条文本超上限：解码拒绝。
    bad[0] = 0; bad[1] = 2; bad[2] = 1;
    bad[3] = APM_BTN_UP; bad[4] = APM_KEY_LABEL_MAX_BYTES + 1;
    memset(&bad[5], 'x', APM_KEY_LABEL_MAX_BYTES + 1);
    CHECK(!apm_key_labels_decode(bad, 5 + APM_KEY_LABEL_MAX_BYTES + 1, &out),
          "单条文本超过 %d 字节拒绝", APM_KEY_LABEL_MAX_BYTES);

    // 恰好上限：允许。
    bad[4] = APM_KEY_LABEL_MAX_BYTES;
    CHECK(apm_key_labels_decode(bad, 5 + APM_KEY_LABEL_MAX_BYTES, &out),
          "单条文本恰好 %d 字节允许", APM_KEY_LABEL_MAX_BYTES);

    // 编码侧：容量不足 / 非法参数返回 0。
    apm_key_labels_t big = in;
    CHECK(apm_key_labels_encode(&in, payload, 3) == 0, "输出容量不足返回 0");
    CHECK(apm_key_labels_encode(NULL, payload, sizeof payload) == 0, "空入参返回 0");
    big.count = 4;
    CHECK(apm_key_labels_encode(&big, payload, sizeof payload) == 0, "条目数超限返回 0");

    // 帧层往返：能装进控制帧并原样解出。
    apm_key_labels_t rt = { .pc_pid = 9, .capture_btn = APM_BTN_UP, .count = 1,
                            .items = { { APM_BTN_UP, 6, "enter" } } };
    n = apm_key_labels_encode(&rt, payload, sizeof payload);
    uint8_t frame[APM_FRAME_MAX_LEN];
    size_t flen = apm_frame_encode(APM_MSG_SET_KEY_LABELS, payload, n, frame, sizeof frame);
    CHECK(flen > 0, "SET_KEY_LABELS 能装进控制帧（%zu 字节）", flen);
    apm_frame_t f;
    CHECK(apm_frame_decode(frame, flen, &f) == (int)flen && f.type == APM_MSG_SET_KEY_LABELS,
          "帧类型可识别");
    CHECK(apm_key_labels_decode(f.payload, f.payload_len, &out) && out.capture_btn == APM_BTN_UP,
          "帧内载荷能解出 capture_btn");
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
    test_ping_pong();
    test_key_labels();
    printf("\n%d passed, %d failed\n", g_pass, g_fail);
    return g_fail == 0 ? 0 : 1;
}
