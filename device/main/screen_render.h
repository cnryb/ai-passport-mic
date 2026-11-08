// device/main/screen_render.h
// 屏幕渲染的**纯逻辑**：字形查找、文本绘制、电量条、布局。
//
// 为什么不放进 screen.c：这些判断（缺字形怎么办、文本超宽怎么截、电量条怎么填）
// 全都能在主机上测，而 screen.c 一旦 include 了 esp_lcd 就只能在真机上"看一眼"。
// 这与 protocol.c / power_policy.c 是同一条规矩（见 device/README.md）。
//
// 本文件**不得包含任何 ESP-IDF 头文件**。它只往一块调用方给的 RGB565 缓冲里写像素。
#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

// ---------------------------------------------------------------------------
// 颜色（RGB565）
// ---------------------------------------------------------------------------
#define APM_COLOR_BG        0x0000   // 黑底：熄屏时与背光全灭一致，开机不会闪白
#define APM_COLOR_FG        0xFFFF   // 主文字
#define APM_COLOR_DIM       0x8410   // 次要文字（灰）
#define APM_COLOR_ACCENT    0x07E0   // 强调（电量充足 / 开麦键）
#define APM_COLOR_WARN      0xFD20   // 低电量
#define APM_COLOR_BAR_BG    0x39E7   // 电量条底槽
#define APM_COLOR_CAPTURE   0xF800   // 「采集中」标记

// ---------------------------------------------------------------------------
// 字形宽度
//
// 字库里的 ASCII（U+0020..U+007E）来自 Unifont 的 **8x16** 字形，只占点阵左半边；
// CJK 是 16x16。所以：
//   - ASCII 按 8 px 画（只取左侧 8 列），CJK 按 16 px 画；
//   - 一行里混排时按实际宽度累加。
//
// 不这么做的后果：`ctrl+enter` 这类键名会宽一倍，一行放不下 10 个字符，
// 屏幕上的标签被截成 `ctrl+ente`。
#define APM_GLYPH_W_FULL  16
#define APM_GLYPH_W_HALF  8
#define APM_FONT_H        16

// ---------------------------------------------------------------------------
// 画布
// ---------------------------------------------------------------------------
typedef struct {
    uint16_t *px;     // RGB565，行优先
    int       w;
    int       h;
    /// 这块画布对应**整屏的第几行**。
    ///
    /// 正常情况下是 0（画布就是整屏）。逐带刷屏时画布只覆盖整屏的一段，
    /// 这里记下起点，所有绘制函数把整屏坐标 `y` 落到画布上的 `y - y_origin`，
    /// 越界的行由边界裁剪吃掉。这样**布局代码完全不用知道刷屏是怎么分带的**——
    /// 否则每个 draw_* 都要多带一个 y_offset 参数，漏传一处就是一条画错的位置。
    int       y_origin;
} apm_canvas_t;

void apm_canvas_clear(apm_canvas_t *c, uint16_t color);
void apm_canvas_fill_rect(apm_canvas_t *c, int x, int y, int w, int h, uint16_t color);
void apm_canvas_hline(apm_canvas_t *c, int x, int y, int w, uint16_t color);

// ---------------------------------------------------------------------------
// 字库
// ---------------------------------------------------------------------------
// 查一个 Unicode 码位的字形位图（16 行 × 2 字节）。找不到返回 NULL。
const uint8_t *apm_font16_glyph(uint32_t cp);

// 字库是否收录该字符（供上层决定要不要替换成 '?'）。
bool apm_font16_has(uint32_t cp);

// 该码位占几个像素宽：ASCII 8，其余 16。缺字形时按 '?' 的宽度（8）算。
int apm_glyph_width(uint32_t cp);

// 把 UTF-8 串解码成码位。返回写入 out 的个数；非法字节按 U+FFFD 计。
// out_cap 不够时**截断**（不返回错误），因为屏幕文案截断比整条不显示更合理。
size_t apm_utf8_decode(const char *s, uint32_t *out, size_t out_cap);

// ---------------------------------------------------------------------------
// 文本绘制
// ---------------------------------------------------------------------------
// 在 (x, y) 左上角绘一个字形。缺字形时画 '?'（这是可见的缺陷，不是静默跳过）。
// 返回该字形的像素宽度。
int apm_draw_glyph(apm_canvas_t *c, int x, int y, uint32_t cp, uint16_t color);

// 画一行 UTF-8 文本，画到 x + max_px 为止（放不下的整个字形不画，不切半个）。
// 返回实际绘制的像素宽度。
int apm_draw_text(apm_canvas_t *c, int x, int y, const char *utf8, uint16_t color, int max_px);

// 文本像素宽度，最多取 max_px。
int apm_text_width(const char *utf8, int max_px);

// 在一条水平带的水平范围内居中绘制。返回绘制宽度。
int apm_draw_text_centered(apm_canvas_t *c, int y, const char *utf8, uint16_t color);

// ---------------------------------------------------------------------------
// 整屏布局
// ---------------------------------------------------------------------------
// 屏幕 240x320，竖屏。排版自上而下：
//
//   y=  6   标题
//   y= 34   「电量 78%」
//   y= 58   电量条（14 px 高）
//   y= 88   连接状态 / 采集中
//   y=116   「按键功能」小标题 + 分隔线
//   y=142   UP    + 动作
//   y=166   DOWN  + 动作
//   y=190   OK    + 动作
//   y=294   底部操作提示
#define APM_SCREEN_W 240
#define APM_SCREEN_H 320

typedef struct {
    /// 电量百分比；<0 表示电量不可用（屏幕显示「电量不可用」而不是 0%）
    int  battery_pct;
    bool low_battery;
    bool connected;
    bool capturing;
    /// 三个键的动作文本，下标 0/1/2 = up/down/ok
    const char *labels[3];
    uint8_t capture_btn;   // 哪个键是开麦键（0..2），画一个 * 标记
    /// 底部操作提示。NULL 表示不画。
    const char *hint;
} apm_screen_view_t;

/// 把一屏内容画到画布上。
///
/// 画布可以是整屏（`h = APM_SCREEN_H`、`y_origin = 0`），也可以只是整屏的一条带
/// （`h` 小、`y_origin` 是这条带的起始行）。布局坐标永远是整屏坐标，
/// 带偏移由画布自己消化。`apm_screen_draw` 与逐带刷屏用的是**同一份布局代码**，
/// 不存在"整屏预览和真机排版不一致"这种可能。
void apm_screen_draw(apm_canvas_t *c, const apm_screen_view_t *v);
