// device/main/audio.c
// ES8311 采集路径。
//
// 参考: FoloToy ai-passport components/bsp/src/bsp_audio.c (MIT)
//       —— 借用的是硬件事实与踩坑结论（I2S 全双工、open 前先 enable 一次、
//       open 之后不要覆写 REG01~06 时钟分频、no_dac_ref 用于单声道 ADC），
//       代码按本项目结构重写，只保留第一版需要的 IN 路径。
#include "audio.h"

#include "board.h"

#include "driver/i2c_master.h"
#include "driver/i2s_std.h"
#include "esp_codec_dev.h"
#include "esp_codec_dev_defaults.h"
#include "esp_log.h"
#include "es8311_codec.h"

static const char *TAG = "apm_audio";

static i2c_master_bus_handle_t s_bus;
static i2s_chan_handle_t       s_tx;
static i2s_chan_handle_t       s_rx;
static const audio_codec_ctrl_if_t *s_ctrl;
static const audio_codec_data_if_t *s_data;
static const audio_codec_gpio_if_t *s_gpio;
static const audio_codec_if_t      *s_codec;
static esp_codec_dev_handle_t       s_dev;
static bool     s_ready;
static int      s_gain_db = APM_AUDIO_DEFAULT_GAIN_DB;

static esp_err_t i2c_bus_init(void)
{
    if (s_bus) return ESP_OK;

    i2c_master_bus_config_t cfg = {
        .i2c_port          = APM_I2C_PORT,
        .sda_io_num        = APM_I2C_SDA_GPIO,
        .scl_io_num        = APM_I2C_SCL_GPIO,
        .clk_source        = I2C_CLK_SRC_DEFAULT,
        .glitch_ignore_cnt = 7,
        .flags.enable_internal_pullup = true,
    };
    esp_err_t err = i2c_new_master_bus(&cfg, &s_bus);
    if (err != ESP_OK) {
        s_bus = NULL;
        ESP_LOGE(TAG, "I2C 总线创建失败 (%s)；检查 SDA=GPIO%d / SCL=GPIO%d",
                 esp_err_to_name(err), APM_I2C_SDA_GPIO, APM_I2C_SCL_GPIO);
        return err;
    }
    ESP_LOGI(TAG, "I2C 就绪 SDA=GPIO%d SCL=GPIO%d", APM_I2C_SDA_GPIO, APM_I2C_SCL_GPIO);
    return ESP_OK;
}

static esp_err_t i2s_init(void)
{
    i2s_chan_config_t chan = {
        .id            = APM_I2S_PORT,
        .role          = I2S_ROLE_MASTER,
        .dma_desc_num  = 6,
        .dma_frame_num = 240,
        .auto_clear_after_cb = true,
        .auto_clear_before_cb = false,
        .intr_priority = 0,
    };
    esp_err_t err = i2s_new_channel(&chan, &s_tx, &s_rx);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "i2s_new_channel 失败: %s", esp_err_to_name(err));
        return err;
    }

    // 建通道时给的采样率只是初值，实际速率由 esp_codec_dev_open() 按需重配。
    i2s_std_config_t std = {
        .clk_cfg = {
            .sample_rate_hz = APM_AUDIO_HZ,
            .clk_src        = I2S_CLK_SRC_DEFAULT,
            .ext_clk_freq_hz = 0,
            .mclk_multiple  = I2S_MCLK_MULTIPLE_256,
        },
        .slot_cfg = {
            .data_bit_width = I2S_DATA_BIT_WIDTH_16BIT,
            .slot_bit_width = I2S_SLOT_BIT_WIDTH_AUTO,
            .slot_mode      = I2S_SLOT_MODE_STEREO,
            .slot_mask      = I2S_STD_SLOT_BOTH,
            .ws_width       = I2S_DATA_BIT_WIDTH_16BIT,
            .ws_pol         = false,
            .bit_shift      = true,
            .left_align     = true,
            .big_endian     = false,
            .bit_order_lsb  = false,
        },
        .gpio_cfg = {
            .mclk = APM_I2S_MCLK_GPIO,
            .bclk = APM_I2S_BCLK_GPIO,
            .ws   = APM_I2S_WS_GPIO,
            .dout = APM_I2S_DOUT_GPIO,
            .din  = APM_I2S_DIN_GPIO,
            .invert_flags = { .mclk_inv = false, .bclk_inv = false, .ws_inv = false },
        },
    };

    if ((err = i2s_channel_init_std_mode(s_tx, &std)) != ESP_OK) {
        ESP_LOGE(TAG, "I2S TX 初始化失败: %s", esp_err_to_name(err));
        return err;
    }
    if ((err = i2s_channel_init_std_mode(s_rx, &std)) != ESP_OK) {
        ESP_LOGE(TAG, "I2S RX 初始化失败: %s", esp_err_to_name(err));
        return err;
    }

    // esp_codec_dev_open() 内部会先 disable 通道，而 disable 要求通道处于 RUNNING；
    // 刚 init 的通道是 READY，会刷一条 "channel has not been enabled yet"。
    // 先 enable 一次让那次 disable 合法（此时 codec 还没配好，不会出声）。
    err = i2s_channel_enable(s_tx);
    if (err == ESP_OK) err = i2s_channel_enable(s_rx);
    if (err != ESP_OK) ESP_LOGE(TAG, "I2S 通道 enable 失败: %s", esp_err_to_name(err));
    return err;
}

static esp_err_t codec_init(void)
{
    s_ctrl = audio_codec_new_i2c_ctrl(&(audio_codec_i2c_cfg_t){
        .port       = APM_I2C_PORT,
        .addr       = APM_ES8311_ADDR << 1,  // 该接口要 8 位地址形式
        .bus_handle = s_bus,
    });
    if (!s_ctrl) {
        ESP_LOGE(TAG, "ES8311 控制口创建失败 —— 用 i2c 扫描确认 0x%02X 是否应答", APM_ES8311_ADDR);
        return ESP_FAIL;
    }

    s_data = audio_codec_new_i2s_data(&(audio_codec_i2s_cfg_t){
        .port      = APM_I2S_PORT,
        .tx_handle = s_tx,
        .rx_handle = s_rx,
    });
    if (!s_data) {
        ESP_LOGE(TAG, "I2S 数据口创建失败");
        return ESP_ERR_NO_MEM;
    }

    s_gpio = audio_codec_new_gpio();
    if (!s_gpio) {
        ESP_LOGE(TAG, "codec GPIO 接口创建失败");
        return ESP_ERR_NO_MEM;
    }

    s_codec = es8311_codec_new(&(es8311_codec_cfg_t){
        .ctrl_if     = s_ctrl,
        .gpio_if     = s_gpio,
        .codec_mode  = ESP_CODEC_DEV_WORK_MODE_BOTH,
        .pa_pin      = APM_I2S_PA_GPIO,
        .pa_reverted = false,
        .master_mode = false,
        .use_mclk    = true,
        .hw_gain     = { .pa_voltage = 5.0f, .codec_dac_voltage = 3.3f },
        .no_dac_ref  = true,  // 单声道采集必须走 ADC，不能借 DAC 参考
    });
    if (!s_codec) {
        ESP_LOGE(TAG, "ES8311 codec 创建失败（I2C 无应答或寄存器配置失败）");
        return ESP_FAIL;
    }

    s_dev = esp_codec_dev_new(&(esp_codec_dev_cfg_t){
        .dev_type = ESP_CODEC_DEV_TYPE_IN_OUT,
        .codec_if = s_codec,
        .data_if  = s_data,
    });
    if (!s_dev) {
        ESP_LOGE(TAG, "esp_codec_dev 创建失败");
        return ESP_ERR_NO_MEM;
    }
    return ESP_OK;
}

static esp_err_t codec_open(void)
{
    esp_codec_dev_sample_info_t fs = {
        .bits_per_sample = APM_AUDIO_BITS,
        .channel         = APM_AUDIO_CHANNELS,
        .channel_mask    = ESP_CODEC_DEV_MAKE_CHANNEL_MASK(0),
        .sample_rate     = APM_AUDIO_HZ,
        .mclk_multiple   = 0,  // 0 = 驱动按默认 256xfs 取 MCLK
    };

    int ret = esp_codec_dev_open(s_dev, &fs);
    if (ret != ESP_CODEC_DEV_OK) {
        ESP_LOGE(TAG, "esp_codec_dev_open 失败: %d", ret);
        return ESP_FAIL;
    }

    // ⚠ open 之后不要覆写 ES8311 的时钟分频寄存器（REG01~06），驱动已按
    //   MCLK/采样率算好；覆写会导致 ADC 时序错乱、录音全是杂音。这里只设模拟增益。
    ret = esp_codec_dev_set_in_gain(s_dev, (float)s_gain_db);
    if (ret != ESP_CODEC_DEV_OK) {
        ESP_LOGE(TAG, "设置麦克风增益失败: %d", ret);
        return ESP_FAIL;
    }
    return ESP_OK;
}

esp_err_t apm_audio_init(void)
{
    if (s_ready) return ESP_OK;

    esp_err_t err = i2c_bus_init();
    if (err != ESP_OK) return err;

    if (!s_tx && !s_rx) {
        err = i2s_init();
        if (err != ESP_OK) return err;
    }

    if (!s_dev) {
        err = codec_init();
        if (err != ESP_OK) return err;
    }

    err = codec_open();
    if (err != ESP_OK) return err;

    s_ready = true;
    ESP_LOGI(TAG, "ES8311 就绪 %d Hz / %d bit / %d ch, gain=%d dB",
             APM_AUDIO_HZ, APM_AUDIO_BITS, APM_AUDIO_CHANNELS, s_gain_db);
    return ESP_OK;
}

esp_err_t apm_audio_read(int16_t *pcm, size_t samples)
{
    if (!s_ready || !s_dev || !pcm) return ESP_ERR_INVALID_STATE;

    int bytes = (int)(samples * sizeof(int16_t) * APM_AUDIO_CHANNELS);
    int ret = esp_codec_dev_read(s_dev, pcm, bytes);
    if (ret != ESP_CODEC_DEV_OK) {
        ESP_LOGW(TAG, "采集失败: %d", ret);
        return ESP_FAIL;
    }
    return ESP_OK;
}

esp_err_t apm_audio_set_gain(int gain_db)
{
    if (gain_db < 0 || gain_db > 42) return ESP_ERR_INVALID_ARG;
    s_gain_db = gain_db;
    if (!s_ready || !s_dev) return ESP_OK;  // 下次 open 时生效

    int ret = esp_codec_dev_set_in_gain(s_dev, (float)gain_db);
    if (ret != ESP_CODEC_DEV_OK) {
        ESP_LOGE(TAG, "增益设置失败: %d", ret);
        return ESP_FAIL;
    }
    ESP_LOGI(TAG, "麦克风增益 = %d dB", gain_db);
    return ESP_OK;
}

int apm_audio_gain(void) { return s_gain_db; }

bool apm_audio_ready(void) { return s_ready; }
