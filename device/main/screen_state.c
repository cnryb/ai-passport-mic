// device/main/screen_state.c
// 亮灭状态机 + 按键标签存储。纯逻辑，不依赖 ESP-IDF（见 screen_state.h 的说明）。
#include "screen_state.h"

#include "protocol.h"        // apm_btn_t
#include "screen_render.h"   // apm_font16_has（标签落库前先过滤缺字形字符）
#include "screen_texts.h"

#include <string.h>

// ---------------------------------------------------------------------------
// 内部：默认文案
// ---------------------------------------------------------------------------
static const char *default_label(uint8_t btn)
{
    switch (btn) {
    case APM_BTN_UP:   return APM_TXT_KEY_UP_DEF;
    case APM_BTN_DOWN: return APM_TXT_KEY_DOWN_DEF;
    case APM_BTN_OK:   return APM_TXT_KEY_OK_DEF;
    default:           return APM_TXT_EMPTY;
    }
}

static const char *key_name(uint8_t btn)
{
    switch (btn) {
    case APM_BTN_UP:   return APM_TXT_KEY_UP;
    case APM_BTN_DOWN: return APM_TXT_KEY_DOWN;
    case APM_BTN_OK:   return APM_TXT_KEY_OK;
    default:           return APM_TXT_EMPTY;
    }
}
const char *apm_screen_key_name(uint8_t btn) { return key_name(btn); }

/// 把文本里字库没有的字符替换成 '?'，并保证以 '\0' 结尾。
///
/// 为什么在**落库时**做而不是绘制时：绘制时替换会让"标签里有缺字"这件事永远看不见——
/// 屏幕上显示 '?'，日志里却是原文，两边对不上。落库时替换 + 返回 false，调用方能记一条
/// WARN 把原文打出来，问题当场可查。
static bool sanitize_into(const char *text, size_t len, char *out, size_t out_cap)
{
    if (!out || out_cap == 0) return false;
    out[0] = '\0';
    if (!text || len == 0) return true;

    size_t o = 0;
    size_t i = 0;
    bool clean = true;
    while (i < len && o + 1 < out_cap) {
        uint32_t cps[1];
        size_t n = apm_utf8_decode(text + i, cps, 1);
        if (n == 0) break;

        // 判断这一步吃掉了几个字节：解码器不回报长度，这里按首字节算。
        unsigned char b = (unsigned char)text[i];
        size_t step = b < 0x80 ? 1 : (b & 0xE0) == 0xC0 ? 2 : (b & 0xF0) == 0xE0 ? 3 : 4;
        if (i + step > len) step = len - i;   // 截断的尾巴按实际剩下的算

        uint32_t cp = cps[0];
        if (!apm_font16_has(cp)) {
            clean = false;
            cp = '?';
        }

        // 重新编码到 out（只处理 BMP 与 '?'，屏幕文案用不到辅助平面）。
        if (cp < 0x80) {
            if (o + 1 >= out_cap) break;
            out[o++] = (char)cp;
        } else if (cp < 0x800) {
            if (o + 2 >= out_cap) break;
            out[o++] = (char)(0xC0 | (cp >> 6));
            out[o++] = (char)(0x80 | (cp & 0x3F));
        } else {
            if (o + 3 >= out_cap) break;
            out[o++] = (char)(0xE0 | (cp >> 12));
            out[o++] = (char)(0x80 | ((cp >> 6) & 0x3F));
            out[o++] = (char)(0x80 | (cp & 0x3F));
        }
        i += step;
    }
    out[o] = '\0';
    if (i < len) clean = false;   // 没吃完 = 被 out_cap 截断了
    return clean;
}

// ---------------------------------------------------------------------------
// 生命周期
// ---------------------------------------------------------------------------
void apm_screen_init(apm_screen_t *s, uint32_t now_ms)
{
    if (!s) return;
    memset(s, 0, sizeof *s);
    s->on = true;                  // 开机先亮：让用户看见设备起来了
    s->last_activity_ms = now_ms;
    s->capture_btn = APM_BTN_OK;   // 与 config/README.md 的默认映射一致
    s->labels_from_pc = false;
}

uint8_t apm_screen_capture_btn(const apm_screen_t *s)
{
    if (!s) return APM_BTN_OK;
    if (s->capture_btn > APM_BTN_OK) return APM_BTN_OK;
    return s->capture_btn;
}

// ---------------------------------------------------------------------------
// 亮灭
// ---------------------------------------------------------------------------
void apm_screen_touch(apm_screen_t *s, uint32_t now_ms)
{
    if (!s) return;
    s->last_activity_ms = now_ms;
}

void apm_screen_turn_on(apm_screen_t *s, uint32_t now_ms)
{
    if (!s) return;
    s->on = true;
    s->last_activity_ms = now_ms;
}

apm_screen_evt_t apm_screen_key_event(apm_screen_t *s, uint8_t btn, bool is_long, uint32_t now_ms)
{
    if (!s) return APM_SCREEN_EVT_NONE;
    if (btn > APM_BTN_OK) return APM_SCREEN_EVT_NONE;

    // 只要有按键动作就刷新活动计时：哪怕这个事件不改屏幕，用户也是在操作设备。
    s->last_activity_ms = now_ms;

    // **只有长按才动屏幕。** press / click / double 一律不响应。
    //
    // 2026-11-08 真机反馈改的：上一版在"灭屏时按下"就点亮，于是单击 UP/DOWN 也会亮屏。
    if (!is_long) return APM_SCREEN_EVT_NONE;

    // 长按「开麦键」不切屏：长按必然先经过 press，而 press 已经上报给 PC 开了麦。
    // 若这里再切屏，一次长按等于"开麦 + 熄屏/亮屏"，两个动作绑在一个手势上。
    if (btn == apm_screen_capture_btn(s)) return APM_SCREEN_EVT_IGNORED;

    // 屏幕状态与采集状态**完全解耦**（2026-11-08 用户要求「省电更重要」）：
    // 采集中也允许长按熄屏，也照常 30 s 自动熄。以前那两条"采集中不许熄屏"的约束
    // 是在保证"用户看得见正在采集"，但代价是录音期间背光一直亮着——对 500 mAh 的
    // 电池来说那是持续耗电，用户明确要求以省电优先。
    if (s->on) {
        s->on = false;
        return APM_SCREEN_EVT_TOGGLE;
    }

    s->on = true;
    return APM_SCREEN_EVT_WAKE;
}

bool apm_screen_auto_off_tick(apm_screen_t *s, uint32_t now_ms)
{
    if (!s || !s->on) return false;
    if (now_ms - s->last_activity_ms < APM_SCREEN_IDLE_OFF_MS) return false;
    s->on = false;
    return true;
}

bool apm_screen_take_dirty(bool dirty, bool screen_on)
{
    // 核心一条：**屏幕灭着就不算"已经处理"**。返回 false 让调用方保留 dirty，
    // 等点亮后再来问一次。
    return dirty && screen_on;
}

bool apm_screen_force_off(apm_screen_t *s)
{
    if (!s) return false;
    bool was = s->on;
    s->on = false;
    return was;
}

// ---------------------------------------------------------------------------
// 按键标签
// ---------------------------------------------------------------------------
bool apm_screen_set_label(apm_screen_t *s, uint8_t btn, const char *text, size_t len)
{
    if (!s || btn > APM_BTN_OK) return false;
    bool clean = sanitize_into(text, len, s->labels[btn], sizeof s->labels[btn]);
    s->labels_from_pc = true;
    return clean;
}

void apm_screen_set_capture_btn(apm_screen_t *s, uint8_t btn)
{
    if (!s) return;
    s->capture_btn = btn > APM_BTN_OK ? APM_SCREEN_KEY_NONE : btn;
}

const char *apm_screen_label(const apm_screen_t *s, uint8_t btn)
{
    if (!s || btn > APM_BTN_OK) return APM_TXT_EMPTY;
    if (s->labels_from_pc && s->labels[btn][0] != '\0') return s->labels[btn];
    return default_label(btn);
}

bool apm_screen_labels_from_pc(const apm_screen_t *s)
{
    return s ? s->labels_from_pc : false;
}

void apm_screen_reset_labels(apm_screen_t *s)
{
    if (!s) return;
    memset(s->labels, 0, sizeof s->labels);
    s->labels_from_pc = false;
    s->capture_btn = APM_BTN_OK;   // 回到默认开麦键，否则断了 PC 后长按判断会错位
}
