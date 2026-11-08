// device/main/screen_state.h
// 屏幕**亮灭状态机**与按键标签存储：不依赖 ESP-IDF 的纯逻辑，有主机侧单测。
//
// 为什么单独抽出来：亮灭规则不只有"按一下开、按一下关"，还有四条互相牵制的约束
// （采集中不许熄屏、开麦键长按不切屏、自动熄屏计时、灭屏时按键先唤醒）。
// 这些判断如果埋进 screen.c，主机侧编不起来，就只能靠真机上"按几下看看"。
#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

// 亮屏后多久无操作自动熄屏。30 s 足够看清电量与按键说明，又不至于长时间亮着耗电。
// 取这个数没有实测依据，是"看得清"与"不白烧背光"的折中；要调就改这里。
#define APM_SCREEN_IDLE_OFF_MS 30000

// 按键标签文本上限（UTF-8 字节）。屏幕一行最多 15 个 16 px 字形，
// 24 字节够放 8 个汉字或 8 个 ASCII 字符外加一点余量。
#define APM_LABEL_MAX_BYTES 24

// 三个键的编号沿用协议里的 apm_btn_t（0=up 1=down 2=ok）。
#define APM_SCREEN_KEY_COUNT 3
#define APM_SCREEN_KEY_NONE  0xFF

typedef enum {
    APM_SCREEN_EVT_NONE = 0,
    APM_SCREEN_EVT_WAKE,      // 灭屏时收到按键：点亮，但不改变"这次按键的业务含义"
    APM_SCREEN_EVT_TOGGLE,    // 长按非开麦键：亮→灭
    APM_SCREEN_EVT_IGNORED,   // 该事件不改屏幕（例如长按开麦键）
} apm_screen_evt_t;

typedef struct {
    bool     on;
    uint32_t last_activity_ms;   // 最后一次"用户操作"的时刻
    uint8_t  capture_btn;         // 开麦键（APM_SCREEN_KEY_NONE = 未知，按默认 ok 处理）
    char     labels[APM_SCREEN_KEY_COUNT][APM_LABEL_MAX_BYTES + 1];
    bool     labels_from_pc;      // 标签是否来自 PC（false = 用内置默认文案）
} apm_screen_t;

// ---------------------------------------------------------------------------
// 生命周期
// ---------------------------------------------------------------------------
/// 初始化到"亮着"。开机先亮一下，让用户看到设备已经起来了，
/// 之后由自动熄屏接管（这与"开机就黑屏，得按一下才知道开没开"是两种体验）。
void apm_screen_init(apm_screen_t *s, uint32_t now_ms);

/// 当前开麦键。未从 PC 收到标签时返回默认值 ok（2）。
uint8_t apm_screen_capture_btn(const apm_screen_t *s);

// ---------------------------------------------------------------------------
// 亮灭
// ---------------------------------------------------------------------------
/// 每收到一个按键事件调一次。
///
/// **只有长按才动屏幕**（press / click / double 都不响应）。返回的 `apm_screen_evt_t`
/// 告诉调用方要不要真的去动背光：
///   WAKE   → 亮（长按一个灭着的屏幕）
///   TOGGLE → 灭（长按一个亮着的屏幕）
///   IGNORED→ 该事件不改屏幕（长按开麦键）
apm_screen_evt_t apm_screen_key_event(apm_screen_t *s, uint8_t btn, bool is_long,
                                      uint32_t now_ms);

/// 每次心跳调一次：处理自动熄屏。返回 true 表示"这次要真的熄灭"。
///
/// **与采集状态无关**：采集中也照常 30 s 熄屏。屏幕只为"看一眼电量/状态"存在，
/// 持续点亮对 500 mAh 电池是不必要的耗电（2026-11-08 用户明确要求省电优先）。
bool apm_screen_auto_off_tick(apm_screen_t *s, uint32_t now_ms);

/// 主动熄屏（例如将来接电源键）。返回 true 表示状态真的从亮变灭。
bool apm_screen_force_off(apm_screen_t *s);

/// 主动点亮。
void apm_screen_turn_on(apm_screen_t *s, uint32_t now_ms);

/// 消费一次"待重画"标记。
///
/// 规则（2026-11-08 真机踩出来的）：**只有真的画了才清标记**。
/// 熄屏期间发生的变化（PC 连上、标签下发）只置标记；若此时把标记清掉又不画，
/// 数据就永久丢了 —— 真机表现是"点亮后仍显示未连接 + 默认文案「快捷键」"。
///
/// 返回 true 表示调用方**现在应该重画**（屏幕亮着且有待画内容）。
bool apm_screen_take_dirty(bool dirty, bool screen_on);

/// 记一次"用户有操作"（刷新自动熄屏计时）。采集中也记，不然后续判断用的是旧时刻。
void apm_screen_touch(apm_screen_t *s, uint32_t now_ms);

// ---------------------------------------------------------------------------
// 按键标签（来自 PC 的 SET_KEY_LABELS）
// ---------------------------------------------------------------------------
/// 设置某个键的标签。`text` 为 NULL 或空串 = 该键恢复默认文案。
/// 返回 false 表示参数非法（btn 越界 / 文本超长被截断）。
bool apm_screen_set_label(apm_screen_t *s, uint8_t btn, const char *text, size_t len);

/// 设置开麦键（0..2），其余值视为未知（按默认 ok 处理）。
void apm_screen_set_capture_btn(apm_screen_t *s, uint8_t btn);

/// 取某键的显示文本。PC 没下发过就返回内置默认文案（见 screen_texts.h 的映射）。
/// 返回值一定非空，且只含字库收录的字符（缺字符的已替换成 '?'）。
const char *apm_screen_label(const apm_screen_t *s, uint8_t btn);

/// 键名（`UP` / `DOWN` / `OK`），屏幕左侧那一列用。
const char *apm_screen_key_name(uint8_t btn);

/// 标签是否来自 PC（供界面/日志区分"默认文案"与"PC 下发的真实映射"）。
bool apm_screen_labels_from_pc(const apm_screen_t *s);

/// 清空 PC 下发状态，回到内置默认文案（断链时调用：PC 走了，屏幕不该继续显示旧映射）。
void apm_screen_reset_labels(apm_screen_t *s);
