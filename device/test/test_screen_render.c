// device/test/test_screen_render.c
// screen_render.c / screen_state.c 的主机侧单测：不依赖 ESP-IDF。
//
//   ./scripts/test-protocol.sh      # 与协议、省电逻辑单测一起跑
//
// 覆盖两类东西：
//   - 渲染纯逻辑：UTF-8 解码、缺字形兜底、越界裁剪、电量条取值；
//   - 亮灭状态机：长按切屏、灭屏时按下唤醒、采集中不许熄屏、长按开麦键不切屏、
//     自动熄屏计时、标签落库时的缺字形过滤。
//
// 这两块以前都属于"只能在真机上按一下看看"的类别。抽成纯逻辑之后，
// "采集中长按不该熄屏"这类规则第一次能被自动检查。
#include "screen_render.h"
#include "screen_state.h"

#include <stdio.h>
#include <string.h>

static int g_pass = 0;
static int g_fail = 0;

#define CHECK(cond, fmt, ...)                                                     \
    do {                                                                          \
        if (cond) {                                                               \
            g_pass++;                                                             \
            printf("  ok   " fmt "\n", ##__VA_ARGS__);                            \
        } else {                                                                  \
            g_fail++;                                                             \
            printf("  FAIL " fmt "  (%s:%d)\n", ##__VA_ARGS__, __FILE__, __LINE__); \
        }                                                                         \
    } while (0)

// ---------------------------------------------------------------------------
// 画布工具
// ---------------------------------------------------------------------------
#define W 240
#define H 320
static uint16_t g_px[W * H];

static apm_canvas_t canvas(void)
{
    apm_canvas_t c = { g_px, W, H, 0 };
    return c;
}

/// 画布里非背景色像素数：用来判断"确实画了东西"。
static int painted(const apm_canvas_t *c)
{
    int n = 0;
    for (int i = 0; i < c->w * c->h; i++) {
        if (c->px[i] != APM_COLOR_BG) n++;
    }
    return n;
}

// ---------------------------------------------------------------------------
// UTF-8
// ---------------------------------------------------------------------------
static void test_utf8(void)
{
    printf("[utf8]\n");
    uint32_t cps[8];

    size_t n = apm_utf8_decode("AB", cps, 8);
    CHECK(n == 2 && cps[0] == 'A' && cps[1] == 'B', "ASCII 解出 2 个码位");

    n = apm_utf8_decode("电量", cps, 8);
    CHECK(n == 2 && cps[0] == 0x7535 && cps[1] == 0x91CF, "两个汉字解出 U+7535 U+91CF");

    // 截断的 3 字节序列开头：不能越界读、不能返回垃圾。
    n = apm_utf8_decode("\xE7\x94", cps, 8);
    CHECK(n == 1 && cps[0] == 0xFFFD, "截断序列记一个 U+FFFD（实际 %zu 个，0x%X）", n,
          n ? cps[0] : 0u);

    // 孤立续字节。
    n = apm_utf8_decode("\x80", cps, 8);
    CHECK(n == 1 && cps[0] == 0xFFFD, "孤立续字节记 U+FFFD");

    // 容量不足时截断而不是溢出。
    n = apm_utf8_decode("ABCDEF", cps, 3);
    CHECK(n == 3, "out_cap=3 时只写 3 个码位（实际 %zu）", n);
}

// ---------------------------------------------------------------------------
// 字形与绘制
// ---------------------------------------------------------------------------
static void test_glyph_lookup(void)
{
    printf("[glyph]\n");
    CHECK(apm_font16_has('A'), "字库含 'A'");
    CHECK(apm_font16_has(0x7535), "字库含 U+7535 电");
    CHECK(apm_font16_has('0'), "字库含数字 '0'");
    // 字库里刻意没有的字符（未收录的汉字），必须返回 false 而不是乱给一个字形。
    CHECK(!apm_font16_has(0x9F98), "未收录的 U+9F98 不在字库里");
    CHECK(apm_font16_glyph(0x9F98) == NULL, "未收录字符返回 NULL");
}

static void test_draw_text(void)
{
    printf("[draw_text]\n");
    apm_canvas_t c = canvas();

    apm_canvas_clear(&c, APM_COLOR_BG);
    int w = apm_draw_text(&c, 0, 0, "AB", APM_COLOR_FG, W);
    CHECK(w == 2 * 8, "两个 ASCII 占 16 px（实际 %d）", w);
    CHECK(painted(&c) > 0, "画布上有非背景像素");

    // CJK 是 16 px 全宽。
    apm_canvas_clear(&c, APM_COLOR_BG);
    w = apm_draw_text(&c, 0, 0, "电量", APM_COLOR_FG, W);
    CHECK(w == 2 * 16, "两个汉字占 32 px（实际 %d）", w);

    // max_px 限制：放不下的整个字形不画，不切半个。
    apm_canvas_clear(&c, APM_COLOR_BG);
    w = apm_draw_text(&c, 0, 0, "ABCDEFGH", APM_COLOR_FG, 20);
    CHECK(w == 16, "max_px=20 只放得下 2 个 ASCII（实际 %d px）", w);

    // 右边界：30 个 ASCII 正好满宽，第 31 个必须停在边界外而不是越界写。
    apm_canvas_clear(&c, APM_COLOR_BG);
    w = apm_draw_text(&c, 0, 0, "0123456789012345678901234567890123456789", APM_COLOR_FG, W);
    CHECK(w == 30 * 8, "240 px 宽最多 30 个 ASCII（实际 %d px）", w);

    // 混排：ASCII 8 px + 汉字 16 px，总宽 24。
    apm_canvas_clear(&c, APM_COLOR_BG);
    w = apm_draw_text(&c, 0, 0, "电A量", APM_COLOR_FG, W);
    CHECK(w == 16 + 8 + 16, "混排宽度按字形累加（实际 %d px）", w);

    // 缺字形画 '?'，不是空白。这条是"加了文案忘了重生成字库"时的唯一可见信号。
    apm_canvas_clear(&c, APM_COLOR_BG);
    int before = painted(&c);
    apm_draw_glyph(&c, 0, 0, 0x9F98, APM_COLOR_FG);
    CHECK(painted(&c) > before, "缺字形时画 '?' 而不是留空");

    // 完全越界的绘制不能崩、不能写坏内存（用金丝雀检查）。
    static uint16_t guard[W * H + 8];
    for (int i = 0; i < W * H + 8; i++) guard[i] = 0xBEEF;
    apm_canvas_t g = { guard, W, H, 0 };
    apm_draw_text(&g, 300, 400, "A", APM_COLOR_FG, W);           // 完全在画布外
    apm_draw_text(&g, -100, -100, "A", APM_COLOR_FG, W);         // 完全在画布外（负）
    apm_draw_text(&g, W - 4, H - 4, "ABCDE", APM_COLOR_FG, W);   // 跨右下角
    bool intact = true;
    for (int i = W * H; i < W * H + 8; i++) {
        if (guard[i] != 0xBEEF) intact = false;
    }
    CHECK(intact, "越界绘制不写坏画布外内存");
}

static void test_utf8_text_width(void)
{
    printf("[text_width]\n");
    CHECK(apm_text_width("ABC", W) == 3 * 8, "3 个 ASCII = 24 px");
    CHECK(apm_text_width("电量", W) == 2 * 16, "2 个汉字 = 32 px");
    CHECK(apm_text_width("ABCDEF", 16) == 16, "max_px 限制宽度");
    CHECK(apm_glyph_width('A') == 8, "ASCII 字形宽 8");
    CHECK(apm_glyph_width(0x7535) == 16, "汉字字形宽 16");
}

// ---------------------------------------------------------------------------
// 整屏布局
// ---------------------------------------------------------------------------
static void test_screen_layout(void)
{
    printf("[layout]\n");
    apm_canvas_t c = canvas();

    apm_screen_view_t v = {
        .battery_pct = 78,
        .low_battery = false,
        .connected = true,
        .capturing = false,
        .labels = { "ctrl+enter", "esc x2", "开麦 关麦" },
        .capture_btn = 2,
        .hint = "长按UP/DOWN键 息屏",
    };
    apm_screen_draw(&c, &v);
    CHECK(painted(&c) > 500, "整屏渲染出足够多的像素（%d）", painted(&c));

    // 电量条：78% 时填充宽度应约为 208 * 78 / 100 = 162 px，且是强调色。
    // 扫描整屏找最长的一行强调色 —— 不写死 y，否则改一次布局常量测试就假红
    // （强调色也用于开麦键的键名与星号，但那些都远窄于电量条）。
    int filled = 0;
    for (int y = 0; y < H; y++) {
        int row = 0;
        for (int x = 0; x < W; x++) {
            if (c.px[y * W + x] == APM_COLOR_ACCENT) row++;
        }
        if (row > filled) filled = row;
    }
    CHECK(filled >= 160 && filled <= 164, "78%% 电量条的填充宽度约 162 px（实际 %d）", filled);

    // 电量不可用：显示「电量不可用」，且电量条只留空槽（不填色）。
    apm_canvas_clear(&c, APM_COLOR_BG);
    v.battery_pct = -1;
    v.low_battery = false;
    apm_screen_draw(&c, &v);
    filled = 0;
    for (int y = 0; y < H; y++) {
        int row = 0;
        for (int x = 0; x < W; x++) {
            if (c.px[y * W + x] == APM_COLOR_ACCENT) row++;
        }
        if (row > filled) filled = row;
    }
    CHECK(filled <= 6 * 8, "电量不可用时电量条不填色（最长强调色行 %d px）", filled);
    CHECK(painted(&c) > 500, "电量不可用时其余内容照常渲染");

    // 采集中用醒目色，且优先于「已连接」显示。
    apm_canvas_clear(&c, APM_COLOR_BG);
    v.battery_pct = 55;
    v.capturing = true;
    apm_screen_draw(&c, &v);
    int found_capture = 0;
    for (int i = 0; i < W * H; i++) {
        if (c.px[i] == APM_COLOR_CAPTURE) found_capture++;
    }
    CHECK(found_capture > 0, "采集中用醒目色绘制状态行（%d 像素）", found_capture);

    // 低电量用警示色。
    apm_canvas_clear(&c, APM_COLOR_BG);
    v.capturing = false;
    v.low_battery = true;
    apm_screen_draw(&c, &v);
    int found_warn = 0;
    for (int i = 0; i < W * H; i++) {
        if (c.px[i] == APM_COLOR_WARN) found_warn++;
    }
    CHECK(found_warn > 0, "低电量用警示色（%d 像素）", found_warn);

    // 空标签不能崩。
    apm_canvas_clear(&c, APM_COLOR_BG);
    apm_screen_view_t e = { .battery_pct = 50, .capture_btn = 2 };
    apm_screen_draw(&c, &e);
    CHECK(1, "labels 全 NULL 时不崩");
}

static void test_band_equals_full(void)
{
    printf("[layout/band]\n");
    // 真机是逐带刷屏（一次 16 行）推上去的。若"逐带渲染"和"整屏渲染"结果不一致，
    // 主机上看到的预览就是假的 —— 这条测试把它钉死。
    static uint16_t full_px[W * H];
    static uint16_t band_px[W * H];

    apm_screen_view_t v = {
        .battery_pct = 42,
        .low_battery = true,
        .connected = false,
        .capturing = false,
        .labels = { "ctrl+enter", "esc x2", "开麦 关麦" },
        .capture_btn = 0,
        .hint = "长按UP/DOWN键 息屏",
    };

    apm_canvas_t full = { full_px, W, H, 0 };
    apm_screen_draw(&full, &v);

    // 按真机的分带方式拼出整屏。
    const int band_rows = 16;
    for (int y0 = 0; y0 < H; y0 += band_rows) {
        int rows = H - y0;
        if (rows > band_rows) rows = band_rows;
        static uint16_t band[W * 16];
        apm_canvas_t c = { band, W, rows, y0 };
        apm_screen_draw(&c, &v);
        memcpy(&band_px[y0 * W], band, (size_t)rows * W * sizeof(uint16_t));
    }

    int diff = 0;
    for (int i = 0; i < W * H; i++) {
        if (full_px[i] != band_px[i]) diff++;
    }
    CHECK(diff == 0, "逐带渲染与整屏渲染逐像素相同（差异 %d）", diff);
}

// ---------------------------------------------------------------------------
// 亮灭状态机
// ---------------------------------------------------------------------------
static void test_screen_power(void)
{
    printf("[screen/power]\n");
    apm_screen_t s;
    apm_screen_init(&s, 0);

    CHECK(s.on, "开机默认亮屏（让用户看见设备起来了）");

    // **只有长按才动屏幕**：press / click / double 一律不响应。
    // 这是 2026-11-08 真机反馈改的：上一版"灭屏时按下就点亮"会让单击 UP/DOWN 也亮屏。
    uint32_t t = 1000;
    apm_screen_evt_t e = apm_screen_key_event(&s, 1, false, t);
    CHECK(e == APM_SCREEN_EVT_NONE && s.on, "亮屏时单击 DOWN 不动屏幕");

    // 长按非开麦键 → 熄灭。
    e = apm_screen_key_event(&s, 1, true, t + 600);
    CHECK(e == APM_SCREEN_EVT_TOGGLE && !s.on, "长按 DOWN 熄屏");

    // 灭屏时**单击**不点亮。
    e = apm_screen_key_event(&s, 1, false, t + 1000);
    CHECK(e == APM_SCREEN_EVT_NONE && !s.on, "灭屏时单击 DOWN 不点亮（关键回归）");
    e = apm_screen_key_event(&s, 0, false, t + 1200);
    CHECK(e == APM_SCREEN_EVT_NONE && !s.on, "灭屏时单击 UP 不点亮（关键回归）");

    // 灭屏时**长按**才点亮。
    e = apm_screen_key_event(&s, 1, true, t + 1600);
    CHECK(e == APM_SCREEN_EVT_WAKE && s.on, "灭屏时长按 DOWN 点亮");
}

static void test_long_press_toggles(void)
{
    printf("[screen/long_toggle]\n");
    apm_screen_t s;
    apm_screen_init(&s, 0);

    // 亮 → 长按 → 灭 → 长按 → 亮。每次长按只做一次切换，
    // 不存在"一次长按触发两次"（那是以前 woke_by_press 标记要解决的问题，
    // 现在因为 press 根本不响应屏幕，天然没有这个问题）。
    apm_screen_evt_t e = apm_screen_key_event(&s, 1, true, 1000);
    CHECK(e == APM_SCREEN_EVT_TOGGLE && !s.on, "第一次长按：灭");

    e = apm_screen_key_event(&s, 1, true, 2000);
    CHECK(e == APM_SCREEN_EVT_WAKE && s.on, "第二次长按：亮");

    e = apm_screen_key_event(&s, 0, true, 3000);
    CHECK(e == APM_SCREEN_EVT_TOGGLE && !s.on, "长按 UP 同样能灭");

    e = apm_screen_key_event(&s, 0, true, 4000);
    CHECK(e == APM_SCREEN_EVT_WAKE && s.on, "长按 UP 同样能亮");
}

static void test_capture_key_long_press(void)
{
    printf("[screen/capture_key]\n");
    apm_screen_t s;
    apm_screen_init(&s, 0);

    // 默认开麦键是 ok（2）：长按它不切屏 —— 长按已经触发了开麦，
    // 再熄屏等于一个手势干两件事。
    apm_screen_evt_t e = apm_screen_key_event(&s, 2, true, 1000);
    CHECK(e == APM_SCREEN_EVT_IGNORED && s.on, "长按开麦键 OK 不熄屏");

    // PC 把开麦键换成 up 之后，长按 up 不切屏、长按 ok 才切屏。
    apm_screen_set_capture_btn(&s, 0);
    e = apm_screen_key_event(&s, 0, true, 2000);
    CHECK(e == APM_SCREEN_EVT_IGNORED && s.on, "开麦键换成 UP 后长按 UP 不熄屏");
    e = apm_screen_key_event(&s, 2, true, 3000);
    CHECK(e == APM_SCREEN_EVT_TOGGLE && !s.on, "此时长按 OK 才熄屏");

    // 断链后回到默认开麦键。
    apm_screen_reset_labels(&s);
    CHECK(apm_screen_capture_btn(&s) == 2, "reset_labels 后开麦键回到默认 ok");
}

static void test_capturing_is_independent(void)
{
    printf("[screen/capturing_independent]\n");

    // 屏幕**完全独立于采集状态**（2026-11-08 用户要求「开麦不需要点亮屏幕，省电更重要」）。
    //
    // 曾经有三条"采集中"约束：不许长按熄屏、force_off 被拒绝、不自动熄屏。
    // 它们保证"用户看得见正在采集"，但代价是录音期间背光常亮。
    // 现在全部去掉：采集中也照常 30 s 熄屏、也允许长按熄屏。
    //
    // 这几条测试用"反向断言"钉住解耦——如果有人把约束加回去，这里就会红。
    apm_screen_t s;
    apm_screen_init(&s, 0);

    // 采集中长按 → 照常熄屏（不再 IGNORED）。
    apm_screen_evt_t e = apm_screen_key_event(&s, 1, true, 1000);
    CHECK(e == APM_SCREEN_EVT_TOGGLE && !s.on, "采集中长按照样能熄屏（省电优先）");

    // 采集中 force_off 照常生效。
    apm_screen_t s2;
    apm_screen_init(&s2, 0);
    CHECK(apm_screen_force_off(&s2), "采集中 force_off 照常生效");
    CHECK(!s2.on, "force_off 之后是灭的");

    // 采集中也照常自动熄屏。
    apm_screen_t s3;
    apm_screen_init(&s3, 0);
    CHECK(apm_screen_auto_off_tick(&s3, APM_SCREEN_IDLE_OFF_MS),
          "采集中也照常 30 s 自动熄屏（省电优先）");
    CHECK(!s3.on, "自动熄屏后是灭的");
}

static void test_idle_off(void)
{
    printf("[screen/idle_off]\n");
    apm_screen_t s;
    apm_screen_init(&s, 0);

    CHECK(!apm_screen_auto_off_tick(&s, APM_SCREEN_IDLE_OFF_MS - 1), "不到 30 s 不熄屏");
    CHECK(s.on, "仍亮");
    CHECK(apm_screen_auto_off_tick(&s, APM_SCREEN_IDLE_OFF_MS), "到 30 s 熄屏");
    CHECK(!s.on, "已熄灭");
    // 幂等：再 tick 不会重复报"刚刚熄灭"（调用方据此决定要不要发命令）。
    CHECK(!apm_screen_auto_off_tick(&s, APM_SCREEN_IDLE_OFF_MS + 5000), "灭屏后 tick 返回 false");

    // 按键刷新计时。
    apm_screen_turn_on(&s, 0);
    apm_screen_touch(&s, APM_SCREEN_IDLE_OFF_MS - 100);
    CHECK(!apm_screen_auto_off_tick(&s, APM_SCREEN_IDLE_OFF_MS), "按键后计时被刷新，不熄屏");

    // 采集期间持续刷新计时：停采后应重新给满 30 s。
    apm_screen_turn_on(&s, 0);
    apm_screen_auto_off_tick(&s, 10);
    apm_screen_auto_off_tick(&s, APM_SCREEN_IDLE_OFF_MS * 3);
    CHECK(!apm_screen_auto_off_tick(&s, APM_SCREEN_IDLE_OFF_MS * 3 + 100),
          "采集中一直在刷新计时，停采后不立刻黑屏");
}

static void test_dirty_not_lost_while_off(void)
{
    printf("[dirty/off_screen]\n");

    // 这个 bug 在 2026-11-08 真机上出现过：屏幕 30 s 自动熄屏 → 用户在 PC 上连接设备 →
    // 设备收到"已连接"和按键标签，但屏幕灭着没画，而且 dirty 被清掉了。
    // 用户按 UP/DOWN 点亮屏幕后看到的仍是上电那一帧（「未连接」+ 默认文案「快捷键」），
    // 与设备实际状态不符。根因就是"没画也把 dirty 清了"。

    CHECK(apm_screen_take_dirty(true, true), "屏幕亮着 + 有待画内容 → 应该画");
    CHECK(!apm_screen_take_dirty(false, true), "屏幕亮着但没变化 → 不画（不做无意义的重画）");
    CHECK(!apm_screen_take_dirty(true, false),
          "**屏幕灭着 + 有待画内容 → 不能画，且不能清标记**（否则点亮后显示旧内容）");
    CHECK(!apm_screen_take_dirty(false, false), "屏幕灭着且没变化 → 什么都不做");

    // 模拟真实时序：灭屏期间来了一次连接 + 标签，然后点亮。
    bool dirty = false;
    bool on = true;

    // 1) 熄屏
    on = false;
    // 2) 灭屏期间收到 set_connected(true) / set_labels(...)：只会置 dirty
    dirty = true;
    // 3) 屏幕任务每 100 ms 跑一次，此时不该消费掉
    if (apm_screen_take_dirty(dirty, on)) {
        dirty = false;
    }
    CHECK(dirty, "灭屏期间 dirty 被保留（这是本 bug 的关键断言）");

    // 4) 用户按 UP 点亮
    on = true;
    bool should_paint = apm_screen_take_dirty(dirty, on);
    if (should_paint) dirty = false;
    CHECK(should_paint, "点亮后立刻补画（否则用户看到旧内容）");
    CHECK(!dirty, "画过之后 dirty 才被清掉");
}

// ---------------------------------------------------------------------------
// 按键标签
// ---------------------------------------------------------------------------
static void test_labels(void)
{
    printf("[labels]\n");
    apm_screen_t s;
    apm_screen_init(&s, 0);

    // 未收到 PC 标签：用内置默认文案。
    CHECK(!apm_screen_labels_from_pc(&s), "初始标签不来自 PC");
    CHECK(strcmp(apm_screen_label(&s, 2), "开麦 关麦") == 0, "OK 默认标签是「开麦 关麦」");

    // 收到 PC 标签（键名照抄 keymap 原文，纯 ASCII）。
    CHECK(apm_screen_set_label(&s, 0, "enter", strlen("enter")), "UP 标签设置成功");
    CHECK(apm_screen_labels_from_pc(&s), "标签标记为来自 PC");
    CHECK(strcmp(apm_screen_label(&s, 0), "enter") == 0, "UP 标签生效");
    CHECK(strcmp(apm_screen_label(&s, 1), "快捷键") == 0,
          "DOWN 没设置时仍用默认文案（不是空串）");

    // 缺字形的字符被替换成 '?'，且返回 false 让调用方记日志。
    bool clean = apm_screen_set_label(&s, 1, "龘", strlen("龘"));
    CHECK(!clean, "标签含未收录字形时返回 false（调用方据此记 WARN）");
    CHECK(strcmp(apm_screen_label(&s, 1), "?") == 0, "未收录字形显示为 '?'");

    // 超长文本截断到 APM_LABEL_MAX_BYTES。
    char longtext[64];
    memset(longtext, 'A', sizeof longtext - 1);
    longtext[sizeof longtext - 1] = '\0';
    apm_screen_set_label(&s, 2, longtext, sizeof longtext - 1);
    size_t len = strlen(apm_screen_label(&s, 2));
    CHECK(len <= APM_LABEL_MAX_BYTES, "超长标签被截断到 %zu <= %d 字节", len, APM_LABEL_MAX_BYTES);

    // 非法键号不写入。
    CHECK(!apm_screen_set_label(&s, 9, "x", 1), "非法键号返回 false");

    // 断链：回到默认文案。
    apm_screen_reset_labels(&s);
    CHECK(!apm_screen_labels_from_pc(&s), "reset 后不再是 PC 标签");
    CHECK(strcmp(apm_screen_label(&s, 0), "快捷键") == 0, "reset 后 UP 回到默认文案");
}

static void test_label_utf8_truncation(void)
{
    printf("[labels/utf8_truncation]\n");
    apm_screen_t s;
    apm_screen_init(&s, 0);
    // 9 个汉字 = 27 字节 > 24，截断必须落在字符边界上（不能留下半个汉字）。
    //
    // **用的字必须在字库里**，否则会先被替换成 '?'，就测不到"截断"这件事了。
    // 这里从 screen_texts.h 的默认标签里拼出来（「开麦 关麦」+「快捷键」），
    // 而不是自己造一串字 —— 自己造的字一旦不在字库里，这条测试会以错误的原因失败。
    const char *text = "开麦关麦快捷键开麦";
    CHECK(strlen(text) == 27, "测试串是 9 个汉字 = 27 字节（实际 %zu）", strlen(text));
    apm_screen_set_label(&s, 0, text, strlen(text));
    const char *got = apm_screen_label(&s, 0);
    CHECK(strlen(got) == 24, "截断到 24 字节（实际 %zu）", strlen(got));
    uint32_t cps[APM_LABEL_MAX_BYTES];
    size_t n = apm_utf8_decode(got, cps, APM_LABEL_MAX_BYTES);
    bool no_garbage = true;
    for (size_t i = 0; i < n; i++) {
        if (cps[i] == 0xFFFD) no_garbage = false;
    }
    CHECK(no_garbage, "截断后没有半个汉字（没有 U+FFFD）");
    CHECK(n == 8, "24 字节最多 8 个汉字（实际 %zu）", n);
}

// ---------------------------------------------------------------------------
int main(void)
{
    printf("== screen_render / screen_state 单测 ==\n");
    test_utf8();
    test_glyph_lookup();
    test_draw_text();
    test_utf8_text_width();
    test_screen_layout();
    test_band_equals_full();
    test_screen_power();
    test_long_press_toggles();
    test_capture_key_long_press();
    test_capturing_is_independent();
    test_idle_off();
    test_dirty_not_lost_while_off();
    test_labels();
    test_label_utf8_truncation();

    printf("\n%d passed, %d failed\n", g_pass, g_fail);
    return g_fail == 0 ? 0 : 1;
}
