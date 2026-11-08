// device/main/screen.h
// ST7789P3 屏幕：显示电量与按键功能、亮灭控制。
//
// 分工（重要）：
//   - **纯逻辑**（画什么、什么时候亮灭、标签怎么存）在 screen_render.c /
//     screen_state.c，主机上有 70 项单测；
//   - 本文件只管硬件：SPI/面板初始化、背光、把像素推给屏、任务与锁。
//
// 硬件事实（引脚、SPI mode、厂商初始化序列）见 board.h 与 docs/ARCHITECTURE.md
// 的「设备屏幕」节。引脚的原始来源是上游 FoloToy BSP（MIT）。
#pragma once

#include "screen_state.h"

#include "esp_err.h"

#include <stdbool.h>
#include <stdint.h>

// 初始化 SPI 总线、ST7789 面板与背光。**失败不阻塞启动**：屏幕坏了设备仍然是可用的
// 麦克风，调用方只记日志（与音频初始化失败的处理一致）。
esp_err_t apm_screen_start(void);

// 屏幕子系统是否可用（面板 + 背光都起来了）。
bool apm_screen_ready(void);

// ---------------------------------------------------------------------------
// 亮灭
// ---------------------------------------------------------------------------
// 按键事件入口。按键回调（10 ms 轮询任务的上下文）里调，只置状态、不刷屏。
//
// `btn` 用协议里的编号（0=up 1=down 2=ok）；`is_long` 为 true 时才是"长按"事件。
// **只有长按会改变屏幕亮灭**（2026-11-08 真机反馈改的，见 screen_state.c 的注释）。
// 返回是否真的动了背光（调用方只用于日志）。
bool apm_screen_on_key(uint8_t btn, bool is_long);

// 每 1 s 心跳调一次：处理自动熄屏与内容刷新（电量/状态变化时才重画）。
void apm_screen_tick(void);

// 立刻点亮/熄灭（采集中拒绝熄灭）。返回是否生效。
bool apm_screen_wake(void);
bool apm_screen_off(void);

// 低电量提示：灭屏时让背光短闪 3 次。亮屏时由屏幕内容自己表达（不做无意义闪烁）。
void apm_screen_blink_low_battery(void);

// ---------------------------------------------------------------------------
// 内容
// ---------------------------------------------------------------------------
// 电量（-1 = 不可用）。变化时才重画。
void apm_screen_set_battery(int pct, bool low);

// 采集状态。变化时才重画；采集中会禁止熄屏。
void apm_screen_set_capturing(bool capturing);

// 链路连接状态。
void apm_screen_set_connected(bool connected);

// PC 下发的按键标签（SET_KEY_LABELS）。返回 false 表示有文本含字库未收录的字符
// （已被替换成 '?'，调用方应当记一条 WARN 把原文打出来）。
bool apm_screen_set_labels(uint8_t capture_btn, const char *up, const char *down,
                           const char *ok);

// 断链时调用：标签回到内置默认文案（PC 走了，屏幕不该继续显示旧映射）。
void apm_screen_clear_labels(void);
