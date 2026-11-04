// device/main/audio.h
// ES8311 采集：I2C 控制口 + I2S 全双工数据口，16 kHz / 16-bit / mono。
#pragma once

#include "esp_err.h"
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

// 初始化 I2C 总线、I2S 通道与 ES8311，并按 16k/16bit/mono 打开。可重复调用。
esp_err_t apm_audio_init(void);

// 阻塞读取 samples 个 16-bit 单声道采样（一次 20 ms = 320 采样）。
// 返回 ESP_ERR_INVALID_STATE 表示音频未就绪，调用方必须据此上报错误而不是当作静音。
esp_err_t apm_audio_read(int16_t *pcm, size_t samples);

// 麦克风 PGA 增益，单位 dB（0..42）。超出范围返回 ESP_ERR_INVALID_ARG。
esp_err_t apm_audio_set_gain(int gain_db);

int apm_audio_gain(void);
bool apm_audio_ready(void);
