// device/main/link.h
// BLE 链路：自定义 GATT 服务 + 广播 + 通知。协议见 docs/ARCHITECTURE.md。
#pragma once

#include "esp_err.h"
#include "power_policy.h"
#include "protocol.h"
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

// 收到 CTRL 写（PC -> 设备）时的回调。运行在 NimBLE host 任务里，
// 只能入队，不要阻塞、不要碰音频。
typedef void (*apm_ctrl_cb_t)(uint8_t type, const uint8_t *payload, uint16_t len);

// 链路状态变化（true = 已连接）。同样跑在 NimBLE host 任务里，只能置标志位。
// 存在的理由：断链时上一条会话的 START 状态必须被清掉，不能等 STOP（KI-005）。
typedef void (*apm_conn_cb_t)(bool connected);

esp_err_t apm_link_start(const char *device_name);
void      apm_link_set_ctrl_cb(apm_ctrl_cb_t cb);
void      apm_link_set_conn_cb(apm_conn_cb_t cb);

bool apm_link_connected(void);
// PC 已连接且 AUDIO 特征值已订阅。返回 false 时音频块只能丢弃并计入丢块。
bool apm_link_audio_ready(void);

// 通知结果。区分"没人收"和"协议栈推不动"：前者是预期行为，后者是链路问题，
// 混成一个丢块数会让 KI-005 这类故障永远看不出成因。
typedef enum {
    APM_LINK_TX_OK        =  0,
    APM_LINK_TX_NO_SINK   = -1,  // 未连接 / PC 没订阅 AUDIO
    APM_LINK_TX_MTU       = -2,  // MTU 不足，165 字节块发不出去
    APM_LINK_TX_CONGESTED = -3,  // 协议栈缓冲满（链路吞吐撑不住 50 块/秒）
} apm_link_tx_t;

// 返回 `apm_link_tx_t`。`<0` 时调用方计入丢块。
int apm_link_notify_audio(const uint8_t *block, size_t len);

// 当前实际连接间隔（ms）；未连接或还没读到返回 0。
uint16_t apm_link_conn_itvl_ms(void);

// 告诉链路"现在在不在采集"，据此在快速档 / 省电档之间切换连接参数。
//
// 采集中：15–30 ms（撑住 50 块/秒）；空闲：80 ms（射频唤醒降到 1/3）。
// 幂等，可从任意任务调用（内部只在档位真的变化时才发请求）。
// **必须在 START/STOP/PTT 与断链清态这三处都调**，否则档位会停在上一场的状态。
void apm_link_set_capturing(bool capturing);

// 每次心跳调用：检查实际间隔是否符合当前档位，不合格就重新请求（内部有节流）。
// 采集态判据 40 ms（并置 LINK_SLOW），空闲态 120 ms（不置位，只纠偏）。
void apm_link_ensure_conn_params(void);

// 每次心跳调一次：校对连接参数 + 按 RSSI 评估发射功率。取代原来的 ensure_fast_conn。
void apm_link_housekeeping(void);

// 档位数值与判据**只在 power_policy.h 定义一次**（纯函数，有主机侧单测）。
// 这里不再重复 15/30/80/40 这些数——一份数值两处实现，改一处漏一处是这类改动的典型翻车点。

// 当前是否处于采集中（供 STATUS 的 LINK_SLOW 判定使用）。
bool apm_link_capturing(void);
int apm_link_notify_event(apm_btn_t btn, apm_btn_ev_t ev, uint8_t flags);
// PONG 与 EVENT 共用特征值；seq/len 为 PING 的载荷（必须 4 字节）。
int apm_link_notify_pong(const uint8_t *seq, size_t len);
int apm_link_notify_status(apm_state_t state, uint8_t flags, uint16_t drops, uint32_t sent);
// 标准 Battery Service（0x2A19）：pct 为百分比 0..100，裸值不套帧。同时更新读缓存。
int apm_link_notify_battery(uint8_t pct);

// 当前协商到的 MTU（未连接时为 0），用于日志与 STATUS 诊断。
uint16_t apm_link_mtu(void);
