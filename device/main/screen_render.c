// device/main/screen_render.c
// 纯逻辑渲染：不依赖 ESP-IDF，可在主机上单测（见 device/test/test_screen_render.c）。
#include "screen_render.h"

#include "font16_data.h"
#include "screen_texts.h"

#include <stdio.h>
#include <string.h>

// ---------------------------------------------------------------------------
// 画布
// ---------------------------------------------------------------------------
void apm_canvas_clear(apm_canvas_t *c, uint16_t color)
{
    if (!c || !c->px) return;
    for (int i = 0; i < c->w * c->h; i++) c->px[i] = color;
}

void apm_canvas_fill_rect(apm_canvas_t *c, int x, int y, int w, int h, uint16_t color)
{
    if (!c || !c->px || w <= 0 || h <= 0) return;
    // 整屏坐标 → 画布坐标。调用方永远按整屏坐标布局，带偏移在这里统一消化。
    y -= c->y_origin;
    // 逐行裁剪：调用方给的矩形允许部分越界（布局算错了不该整屏不画）。
    int x0 = x < 0 ? 0 : x;
    int y0 = y < 0 ? 0 : y;
    int x1 = x + w > c->w ? c->w : x + w;
    int y1 = y + h > c->h ? c->h : y + h;
    for (int row = y0; row < y1; row++) {
        uint16_t *p = &c->px[row * c->w];
        for (int col = x0; col < x1; col++) p[col] = color;
    }
}

void apm_canvas_hline(apm_canvas_t *c, int x, int y, int w, uint16_t color)
{
    apm_canvas_fill_rect(c, x, y, w, 1, color);
}

// ---------------------------------------------------------------------------
// 字库
// ---------------------------------------------------------------------------
// 线性查找会每次扫 130 条 × 32 字节。屏幕上最多几十个字形，一帧几十次查找，
// 每次 130 次比较——完全可以忽略，不值得为它加一张查表。
// 若将来字库涨到几千字，再改成两段式（ASCII 直查 + 二分）。
const uint8_t *apm_font16_glyph(uint32_t cp)
{
    for (unsigned i = 0; i < s_font16_count; i++) {
        if (s_font16[i].cp == cp) return s_font16[i].bits;
    }
    return NULL;
}

bool apm_font16_has(uint32_t cp) { return apm_font16_glyph(cp) != NULL; }

int apm_glyph_width(uint32_t cp)
{
    // ASCII 在 Unifont 里是 8x16，位图存的是左对齐（高字节有内容、低字节全 0）。
    // 缺字形时按 '?'（ASCII）的宽度算，避免布局在缺字时整行右移。
    return (cp >= 0x20 && cp <= 0x7E) ? APM_GLYPH_W_HALF : APM_GLYPH_W_FULL;
}

// ---------------------------------------------------------------------------
// UTF-8 解码
// ---------------------------------------------------------------------------
// 只做屏幕文案需要的那一档：合法 UTF-8 原样解；非法/截断的序列各算一个 U+FFFD。
// 不用 ESP-IDF 的 utf8 工具，因为这里要能被主机单测编译。
size_t apm_utf8_decode(const char *s, uint32_t *out, size_t out_cap)
{
    if (!s || !out || out_cap == 0) return 0;

    size_t n = 0;
    const unsigned char *p = (const unsigned char *)s;
    while (*p && n < out_cap) {
        uint32_t cp;
        int extra;
        if (*p < 0x80) {
            cp = *p;
            extra = 0;
        } else if ((*p & 0xE0) == 0xC0) {
            cp = *p & 0x1F;
            extra = 1;
        } else if ((*p & 0xF0) == 0xE0) {
            cp = *p & 0x0F;
            extra = 2;
        } else if ((*p & 0xF8) == 0xF0) {
            cp = *p & 0x07;
            extra = 3;
        } else {
            // 孤立的续字节或 5 字节以上：吞一个字节，记一个替换符。
            out[n++] = 0xFFFD;
            p++;
            continue;
        }

        int i = 1;
        for (; i <= extra; i++) {
            if ((p[i] & 0xC0) != 0x80) break;   // 序列被截断
            cp = (cp << 6) | (p[i] & 0x3F);
        }
        if (i <= extra) {
            // 截断：吃掉已消费的字节，记一个替换符（不越界读）。
            out[n++] = 0xFFFD;
            p += i;
            continue;
        }
        out[n++] = cp;
        p += extra + 1;
    }
    return n;
}

// ---------------------------------------------------------------------------
// 文本绘制
// ---------------------------------------------------------------------------
int apm_draw_glyph(apm_canvas_t *c, int x, int y, uint32_t cp, uint16_t color)
{
    if (!c || !c->px) return 0;

    const uint8_t *bits = apm_font16_glyph(cp);
    if (!bits) {
        // 缺字形画 '?'：**看得见的缺陷**比空白好排查。空白会被误当成"这行没内容"，
        // 而 '?' 一眼就能看出"字库没收录这个字"。
        bits = apm_font16_glyph('?');
        if (!bits) return 0;
        cp = '?';
    }

    int gw = apm_glyph_width(cp);
    for (int row = 0; row < APM_FONT_H; row++) {
        int py = y + row - c->y_origin;
        if (py < 0 || py >= c->h) continue;
        uint8_t hi = bits[row * 2];
        uint8_t lo = gw == APM_GLYPH_W_FULL ? bits[row * 2 + 1] : 0;
        uint16_t v = (uint16_t)((hi << 8) | lo);
        if (v == 0) continue;
        uint16_t *line = &c->px[py * c->w];
        for (int col = 0; col < gw; col++) {
            if (!(v & (0x8000u >> col))) continue;
            int pxx = x + col;
            if (pxx < 0 || pxx >= c->w) continue;
            line[pxx] = color;
        }
    }
    return gw;
}

int apm_draw_text(apm_canvas_t *c, int x, int y, const char *utf8, uint16_t color, int max_px)
{
    if (!c || !utf8 || max_px <= 0) return 0;

    // 一行最多 30 个 ASCII 字形（240 / 8），栈上 32 个够用。
    uint32_t cps[32];
    size_t n = apm_utf8_decode(utf8, cps, sizeof cps / sizeof cps[0]);

    int used = 0;
    for (size_t i = 0; i < n; i++) {
        int gw = apm_glyph_width(cps[i]);
        if (used + gw > max_px) break;                 // 放不下的整个字形不画
        int gx = x + used;
        if (gx + gw > c->w || gx >= c->w) break;       // 越过画布右边界
        apm_draw_glyph(c, gx, y, cps[i], color);
        used += gw;
    }
    return used;
}

int apm_text_width(const char *utf8, int max_px)
{
    if (!utf8 || max_px <= 0) return 0;
    uint32_t cps[32];
    size_t n = apm_utf8_decode(utf8, cps, sizeof cps / sizeof cps[0]);
    int used = 0;
    for (size_t i = 0; i < n; i++) {
        int gw = apm_glyph_width(cps[i]);
        if (used + gw > max_px) break;
        used += gw;
    }
    return used;
}

int apm_draw_text_centered(apm_canvas_t *c, int y, const char *utf8, uint16_t color)
{
    if (!c || !utf8) return 0;
    int w = apm_text_width(utf8, c->w);
    if (w <= 0) return 0;
    int x = (c->w - w) / 2;
    return apm_draw_text(c, x < 0 ? 0 : x, y, utf8, color, c->w);
}

// ---------------------------------------------------------------------------
// 整屏布局
// ---------------------------------------------------------------------------
// 坐标写死在这里而不是算出来：这块屏的分辨率与字号都不会变，
// 一张直观的常量表比一堆 "if (h > ...)" 更容易核对。
#define APM_PAD_X       16
#define APM_Y_TITLE      6
#define APM_Y_BATT      34
#define APM_Y_BAR       58
#define APM_BAR_H       14
#define APM_Y_LINK      88
#define APM_Y_KEYS_HDR 116
#define APM_Y_KEY0     142
#define APM_KEY_ROW_H   24
#define APM_Y_HINT     294

#define APM_BAR_X       APM_PAD_X
#define APM_BAR_W       (APM_SCREEN_W - 2 * APM_PAD_X)
// 键名列固定占 5 个 ASCII 宽（"DOWN " 或 "UP   "），再加一个星号位。
#define APM_KEY_COL_W   (5 * APM_GLYPH_W_HALF + APM_GLYPH_W_HALF)
#define APM_KEY_LABEL_X (APM_PAD_X + APM_KEY_COL_W)

static void draw_battery_bar(apm_canvas_t *c, int pct, bool low)
{
    apm_canvas_fill_rect(c, APM_BAR_X, APM_Y_BAR, APM_BAR_W, APM_BAR_H, APM_COLOR_BAR_BG);
    if (pct < 0) return;   // 电量不可用：只画空槽

    if (pct > 100) pct = 100;
    int fill = APM_BAR_W * pct / 100;
    if (fill > 0) {
        apm_canvas_fill_rect(c, APM_BAR_X, APM_Y_BAR, fill, APM_BAR_H,
                             low ? APM_COLOR_WARN : APM_COLOR_ACCENT);
    }
}

// 键名 + 开麦标记。键名全是 ASCII，宽度固定，星号落在第 5 个 ASCII 位上。
static void draw_key_name(apm_canvas_t *c, int y, const char *name, bool is_capture)
{
    apm_draw_text(c, APM_PAD_X, y, name, is_capture ? APM_COLOR_ACCENT : APM_COLOR_FG,
                  APM_KEY_COL_W);
    if (is_capture) {
        apm_draw_text(c, APM_PAD_X + 4 * APM_GLYPH_W_HALF, y, "*", APM_COLOR_ACCENT,
                      APM_GLYPH_W_HALF);
    }
}

static void draw_key_row(apm_canvas_t *c, int y, const char *name, const char *act,
                         bool is_capture)
{
    draw_key_name(c, y, name, is_capture);
    if (!act || !act[0]) return;
    apm_draw_text(c, APM_KEY_LABEL_X, y, act, APM_COLOR_FG, c->w - APM_KEY_LABEL_X);
}

void apm_screen_draw(apm_canvas_t *c, const apm_screen_view_t *v)
{
    if (!c || !v || !c->px) return;

    apm_canvas_clear(c, APM_COLOR_BG);

    // ---- 标题 ----
    apm_draw_text_centered(c, APM_Y_TITLE, APM_TXT_TITLE, APM_COLOR_DIM);

    // ---- 电量 ----
    if (v->battery_pct < 0) {
        apm_draw_text(c, APM_PAD_X, APM_Y_BATT, APM_TXT_BATTERY_NA, APM_COLOR_DIM,
                      c->w - 2 * APM_PAD_X);
    } else {
        char line[32];
        snprintf(line, sizeof line, "%s %u%%", APM_TXT_BATTERY,
                 (unsigned)(v->battery_pct > 100 ? 100 : v->battery_pct));
        // 低电量时用警示色：这是用户唯一能"一眼看到"的提示，比只改电量条颜色更醒目。
        apm_draw_text(c, APM_PAD_X, APM_Y_BATT, line,
                      v->low_battery ? APM_COLOR_WARN : APM_COLOR_FG, c->w - 2 * APM_PAD_X);
    }
    draw_battery_bar(c, v->battery_pct, v->low_battery);

    // ---- 连接状态（采集中优先显示，那个信息更重要）----
    if (v->capturing) {
        apm_draw_text(c, APM_PAD_X, APM_Y_LINK, APM_TXT_CAPTURING, APM_COLOR_CAPTURE,
                      c->w - 2 * APM_PAD_X);
    } else {
        apm_draw_text(c, APM_PAD_X, APM_Y_LINK,
                      v->connected ? APM_TXT_LINK_ON : APM_TXT_LINK_OFF,
                      v->connected ? APM_COLOR_FG : APM_COLOR_DIM, c->w - 2 * APM_PAD_X);
    }

    // ---- 按键说明 ----
    apm_draw_text(c, APM_PAD_X, APM_Y_KEYS_HDR, APM_TXT_KEYS, APM_COLOR_DIM,
                  c->w - 2 * APM_PAD_X);
    apm_canvas_hline(c, APM_PAD_X, APM_Y_KEYS_HDR + APM_FONT_H + 2, c->w - 2 * APM_PAD_X,
                     APM_COLOR_BAR_BG);

    static const char *const names[3] = { APM_TXT_KEY_UP, APM_TXT_KEY_DOWN, APM_TXT_KEY_OK };
    for (int i = 0; i < 3; i++) {
        draw_key_row(c, APM_Y_KEY0 + i * APM_KEY_ROW_H, names[i], v->labels[i],
                     v->capture_btn == (uint8_t)i);
    }

    // ---- 底部提示 ----
    if (v->hint && v->hint[0]) {
        apm_draw_text_centered(c, APM_Y_HINT, v->hint, APM_COLOR_DIM);
    }
}
