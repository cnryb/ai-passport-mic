// device/main/screen_texts.h
// 屏幕上的**全部**文案，集中在这一处。
//
// 为什么单独一个文件：`scripts/gen-font-subset.py` 从这里提取所有字符串字面量，
// 算出点阵字库要收录哪些字符。文案写散在 screen.c 里，脚本就得去解析整个 C 文件，
// 而且改了文案很容易忘了重生成字库 —— 表现是屏幕上一个方框，只有上真机才看得见。
//
// 规则：
//   1. **字符串字面量只写在这里**，screen.c 只引用宏；
//   2. 加字之后必须跑 `./scripts/gen-font-subset.py`（verify.sh 会检查是否最新）；
//   3. 字库里没有的字符会被画成 '?'，不会崩，但那已经是缺陷了。
#pragma once

// ---- 静态文案 ----
#define APM_TXT_TITLE          "AI PASSPORT MIC"
#define APM_TXT_BATTERY        "电量"
#define APM_TXT_BATTERY_NA     "电量不可用"
#define APM_TXT_LINK_ON        "已连接"
#define APM_TXT_LINK_OFF       "未连接"
#define APM_TXT_CAPTURING      "采集中"
#define APM_TXT_KEYS           "按键功能"
#define APM_TXT_CAPTURE_MARK   "开麦键"
// 底部操作提示。只有一份：灭屏时屏幕看不见，不需要"按任意键亮屏"的文案
// （真要写那句也只可能出现在灭屏状态下，等于永远看不见）。
#define APM_TXT_HINT_ON        "长按UP/DOWN键 息屏"

// ---- 默认按键标签（PC 下发 SET_KEY_LABELS 之前显示这一组）----
//
// 与 config/button-map.example.json 的默认语义一致：OK 是开麦键（ptt-toggle），
// UP / DOWN 是用户自定义快捷键。标签只能说"是什么角色"——固件不知道 UP 具体映射成
// 了哪个键，那是 PC 端映射表的事，PC 连上后会覆盖这三行。
//
// **PC 下发的标签只用两类字符**（见 ARCHITECTURE.md 的 `SET_KEY_LABELS` 节）：
//   - 键名照抄 keymap 里的原文（`enter` / `ctrl+enter` / `f5`），即纯 ASCII；
//   - 角色词只用中文：「开麦」「关麦」「快捷键」「无」。
// 这样字库只需要覆盖 ASCII 与这几个中文词，不需要一张键名翻译表 ——
// 有翻译表就有第二份真相，改一处漏一处是这类改动的典型翻车方式。
#define APM_TXT_KEY_UP         "UP"
#define APM_TXT_KEY_DOWN       "DOWN"
#define APM_TXT_KEY_OK         "OK"
#define APM_TXT_KEY_UP_DEF     "快捷键"
#define APM_TXT_KEY_DOWN_DEF   "快捷键"
#define APM_TXT_KEY_OK_DEF     "开麦 关麦"
#define APM_TXT_NO_BINDING     "无"

// ---- 百分比与占位 ----
#define APM_TXT_PCT_FMT        "%u%%"
#define APM_TXT_UNKNOWN_CHAR   "?"
#define APM_TXT_EMPTY          ""
