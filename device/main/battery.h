// device/main/battery.h
// CW2017 电量计（I2C 0x63，与 ES8311 共总线）+ 低电量背光呼吸指示。
// 电量百分比同时通过 BLE 标准 Battery Service 上报（见 link.c / ARCHITECTURE.md）。
#pragma once

#include "esp_err.h"
#include <stdbool.h>
#include <stdint.h>

// SOC 低于等于该值（百分比）视为低电量：GUI/系统侧据此提示；
// 设备端按任意键时背光短闪 3 次（平时背光恒灭，零功耗）。
#define APM_BATTERY_LOW_SOC_PCT 20

// 初始化电量计设备句柄并触发一次 SOC 重算。依赖共享 I2C 总线
// （apm_audio_i2c_bus()），总线没起来时返回 ESP_ERR_INVALID_STATE。可重复调用。
esp_err_t apm_battery_init(void);

// 读当前 SOC（0..100）。未初始化返回 ESP_ERR_INVALID_STATE；I2C 失败原样返回。
esp_err_t apm_battery_read(uint8_t *soc);

// 最近一次成功读到的 SOC；没读到过返回 false。
bool apm_battery_ready(void);
uint8_t apm_battery_soc(void);

// 启动电量任务：每秒采样 SOC，变化时经 BLE 上报；背光保持熄灭（被动查询式）。
// 返回 ESP_ERR_INVALID_STATE 表示未初始化（调用方记日志即可，不能因此卡启动）。
esp_err_t apm_battery_start(void);

// 按键回调里调用：仅当处于低电量时置位一次背光短闪（3 次，约 600 ms），
// 非低电量时是空操作。运行在按键回调上下文，只置一个 volatile 计数，不会阻塞。
void apm_battery_on_button(void);
