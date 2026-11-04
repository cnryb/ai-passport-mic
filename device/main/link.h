// device/main/link.h
// BLE 链路：自定义 GATT 服务 + 广播 + 通知。协议见 docs/ARCHITECTURE.md。
#pragma once

#include "esp_err.h"
#include "protocol.h"
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

// 收到 CTRL 写（PC -> 设备）时的回调。运行在 NimBLE host 任务里，
// 只能入队，不要阻塞、不要碰音频。
typedef void (*apm_ctrl_cb_t)(uint8_t type, const uint8_t *payload, uint16_t len);

esp_err_t apm_link_start(const char *device_name);
void      apm_link_set_ctrl_cb(apm_ctrl_cb_t cb);

bool apm_link_connected(void);
// PC 已连接且 AUDIO 特征值已订阅。返回 false 时音频块只能丢弃并计入丢块。
bool apm_link_audio_ready(void);

// 返回 0 表示已交给协议栈；<0 表示本次通知失败（调用方计入丢块）。
int apm_link_notify_audio(const uint8_t *block, size_t len);
int apm_link_notify_event(apm_btn_t btn, apm_btn_ev_t ev, uint8_t flags);
int apm_link_notify_status(apm_state_t state, uint8_t flags, uint16_t drops, uint32_t sent);

// 当前协商到的 MTU（未连接时为 0），用于日志与 STATUS 诊断。
uint16_t apm_link_mtu(void);
