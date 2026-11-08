// device/main/screen.c
// ST7789P3 硬件层：SPI/面板初始化、背光、按行带推屏、屏幕任务。
//
// 这里只做"把像素推上去"和"背光开灭"这两件硬件事。**画什么在 screen_render.c，
// 什么时候亮灭在 screen_state.c**，两者都是不依赖 ESP-IDF 的纯逻辑（70 项单测）。
//
// 厂商初始化序列（porch / power / gamma）来自上游 FoloToy BSP 的 bsp_display.c（MIT），
// 那是面板厂给的参考例程 TFT_init() 里的值，**不是 ST7789 的通用默认值**；
// 换屏必须向供应商要新的一份，照抄这份大概率显示异常。
//
// 省电口径：熄屏 = 背光占空比 0 + ST7789 Sleep In。面板静态电流固件管不了的部分
// 就不管；这里能做的都做了。**当前没有实测电流数字**（见 ENVIRONMENT.md）。
#include "screen.h"

#include "board.h"
#include "screen_render.h"
#include "screen_state.h"
#include "screen_texts.h"

#include "driver/gpio.h"
#include "driver/ledc.h"
#include "driver/spi_master.h"
#include "esp_lcd_panel_io.h"
#include "esp_lcd_panel_ops.h"
#include "esp_lcd_panel_vendor.h"
#include "esp_log.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"

#include <string.h>

static const char *TAG = "apm_screen";

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------
static esp_lcd_panel_handle_t    s_panel;
static esp_lcd_panel_io_handle_t s_io;
static bool                      s_bus_ready;
static bool                      s_bl_ready;
static bool                      s_ready;

// 屏幕状态被三个上下文访问：按键回调（apm_btn 任务）、心跳（apm_status 任务）、
// 屏幕任务。用一把互斥锁保证"读-改-写 + 重画"是原子的，避免半份状态被画出去。
static SemaphoreHandle_t s_mutex;
static apm_screen_t      s_state;
static bool              s_dirty;          // 需要重画
static TaskHandle_t      s_task;
static uint32_t          s_flash_until_ms; // 低电量短闪的截止时刻，0 = 不闪
static bool              s_panel_asleep;

// DMA 完成信号量。`esp_lcd_panel_io_spi` 把 color 数据**排队后立刻返回、不拷贝**，
// 所以"推完"不等于"发完"。每次 draw_bitmap 之后等这个信号量，保证缓冲可以安全复用。
// 由 panel IO 的 on_color_trans_done 回调给出。
static SemaphoreHandle_t s_dma_done;

// 行带缓冲。**必须两块**：DMA 是异步的（见 redraw_locked 的注释），
// 一块在发的时候另一块正在被渲染。多花的 RAM 是 240 × 16 × 2 = 7.5 KB。
// static 而不是 malloc：大小固定，放 .bss 比在启动路径里申请更容易看出来内存去哪了。
static uint16_t s_band[2][APM_LCD_W * APM_LCD_BAND_ROWS];
// 整帧画布：**只在重画时存在**。C3 的 RAM 装不下常驻的 150 KB 全帧，
// 所以这里也只做"逐带渲染"——画布指针每次指向 s_band 的同一块内存，
// 用 y_offset 把整屏坐标平移到带内坐标。
static apm_canvas_t s_canvas;

// 当前显示内容（供重画时读取，避免每次重画都重新拼一遍）
static apm_screen_view_t s_view;
static char s_label_buf[3][APM_LABEL_MAX_BYTES + 1];

static uint32_t now_ms(void) { return (uint32_t)(esp_timer_get_time() / 1000); }

// 前向声明：panel_on 要用到重画，而重画函数定义在它后面。
static void refresh_view_locked(void);
static void redraw_locked(void);

// ---------------------------------------------------------------------------
// 背光
// ---------------------------------------------------------------------------
static void bl_set_duty_pct(int pct)
{
    if (!s_bl_ready) return;
    if (pct < 0) pct = 0;
    if (pct > 100) pct = 100;
    uint32_t max = (1u << APM_BL_LEDC_RESOLUTION) - 1;
    uint32_t duty = (uint32_t)max * (uint32_t)pct / 100u;
    ledc_set_duty(LEDC_LOW_SPEED_MODE, (ledc_channel_t)APM_BL_LEDC_CHANNEL, duty);
    ledc_update_duty(LEDC_LOW_SPEED_MODE, (ledc_channel_t)APM_BL_LEDC_CHANNEL);
}

static esp_err_t bl_init(void)
{
    ledc_timer_config_t timer = {
        .speed_mode      = LEDC_LOW_SPEED_MODE,
        .timer_num       = (ledc_timer_t)APM_BL_LEDC_TIMER,
        .duty_resolution = (ledc_timer_bit_t)APM_BL_LEDC_RESOLUTION,
        .freq_hz         = APM_BL_LEDC_FREQ_HZ,
        .clk_cfg         = LEDC_AUTO_CLK,
    };
    esp_err_t err = ledc_timer_config(&timer);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "背光 LEDC timer 配置失败: %s", esp_err_to_name(err));
        return err;
    }

    ledc_channel_config_t chan = {
        .gpio_num   = APM_BL_GPIO,
        .speed_mode = LEDC_LOW_SPEED_MODE,
        .channel    = (ledc_channel_t)APM_BL_LEDC_CHANNEL,
        .timer_sel  = (ledc_timer_t)APM_BL_LEDC_TIMER,
        .duty       = 0,
        .hpoint     = 0,
    };
    err = ledc_channel_config(&chan);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "背光 LEDC channel 配置失败: %s", esp_err_to_name(err));
        ledc_timer_rst(LEDC_LOW_SPEED_MODE, (ledc_timer_t)APM_BL_LEDC_TIMER);
        return err;
    }
    s_bl_ready = true;
    ESP_LOGI(TAG, "背光就绪 gpio=%d（%d%% 为亮屏）", APM_BL_GPIO, APM_BL_ON_PCT);
    return ESP_OK;
}

// ---------------------------------------------------------------------------
// 面板
// ---------------------------------------------------------------------------
typedef struct {
    uint8_t  cmd;
    uint8_t  data[16];
    uint8_t  len;
    uint16_t delay_ms;
} st_init_cmd_t;

// ST7789P3 厂商专属序列。0x11 SLPOUT / 0x3A COLMOD / 0x21 INVON / 0x29 DISPON /
// 0x36 MADCTL 由 esp_lcd 的 panel 驱动与下面几个调用完成，这里不重复。
static const st_init_cmd_t ST7789P3_CMDS[] = {
    { 0xB2, { 0x05, 0x05, 0x00, 0x33, 0x33 }, 5, 0 },   // PORCTRL 帧率 porch
    { 0xB7, { 0x35 }, 1, 0 },                           // GCTRL 栅极
    { 0xBB, { 0x21 }, 1, 0 },                           // VCOMS
    { 0xC0, { 0x2C }, 1, 0 },                           // LCMCTRL
    { 0xC2, { 0x01 }, 1, 0 },                           // VDVVRHEN
    { 0xC3, { 0x0B }, 1, 0 },                           // VRHS
    { 0xC4, { 0x20 }, 1, 0 },                           // VDVSET
    { 0xC6, { 0x0F }, 1, 0 },                           // FRCTRL2 60 Hz 点反转
    { 0xD0, { 0xA7, 0xA1 }, 2, 0 },                     // PWCTRL1
    { 0xD0, { 0xA4, 0xA1 }, 2, 0 },                     // PWCTRL1（参考例程重发，覆盖上一条）
    { 0xD6, { 0xA1 }, 1, 0 },
    { 0xE0, { 0xD0, 0x04, 0x08, 0x0A, 0x09, 0x05, 0x2D, 0x43, 0x49, 0x09, 0x16, 0x15, 0x26,
              0x2B }, 14, 0 },                           // PVGAMCTRL 正伽马
    { 0xE1, { 0xD0, 0x03, 0x09, 0x0A, 0x0A, 0x06, 0x2E, 0x44, 0x40, 0x3A, 0x15, 0x15, 0x26,
              0x2A }, 14, 10 },                          // NVGAMCTRL 负伽马
};

// DMA 完成回调。**在 ISR 上下文执行**：只能做 give，不能打日志、不能拿锁。
static bool IRAM_ATTR on_color_done(esp_lcd_panel_io_handle_t io, esp_lcd_panel_io_event_data_t *edata,
                                    void *user_ctx)
{
    (void)io;
    (void)edata;
    (void)user_ctx;
    BaseType_t hp_task_woken = pdFALSE;
    if (s_dma_done) {
        xSemaphoreGiveFromISR(s_dma_done, &hp_task_woken);
    }
    return hp_task_woken == pdTRUE;   // true = 需要让更高优先级任务在 ISR 返回后立即运行
}

static void panel_release_failed(void)
{
    // 回滚顺序：面板 → IO → 总线。任一步失败就把它留成 NULL 避免二次释放。
    if (s_panel && esp_lcd_panel_del(s_panel) == ESP_OK) s_panel = NULL;
    if (!s_panel && s_io && esp_lcd_panel_io_del(s_io) == ESP_OK) s_io = NULL;
    if (!s_io && s_bus_ready && spi_bus_free(APM_LCD_SPI_HOST) == ESP_OK) s_bus_ready = false;
}

static esp_err_t panel_init(void)
{
    spi_bus_config_t bus = {
        .mosi_io_num     = APM_LCD_MOSI_GPIO,
        .sclk_io_num     = APM_LCD_SCLK_GPIO,
        .miso_io_num     = -1,   // 本板未接 MISO
        .quadwp_io_num   = -1,
        .quadhd_io_num   = -1,
        .max_transfer_sz = APM_LCD_W * APM_LCD_BAND_ROWS * 2,
    };
    esp_err_t err = spi_bus_initialize(APM_LCD_SPI_HOST, &bus, SPI_DMA_CH_AUTO);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "SPI 总线初始化失败（%s）：检查 MOSI=GPIO%d / SCLK=GPIO%d 是否冲突",
                 esp_err_to_name(err), APM_LCD_MOSI_GPIO, APM_LCD_SCLK_GPIO);
        return err;
    }
    s_bus_ready = true;

    esp_lcd_panel_io_spi_config_t io_cfg = {
        .cs_gpio_num       = APM_LCD_CS_GPIO,
        .dc_gpio_num       = APM_LCD_DC_GPIO,
        .pclk_hz           = APM_LCD_PCLK_HZ,
        .spi_mode          = APM_LCD_SPI_MODE,
        .lcd_cmd_bits      = 8,
        .lcd_param_bits    = 8,
        .trans_queue_depth = 10,
        // 一块带子就是一次 color 传输，所以"队列深度 1"就够了：
        // 我们**同步等**每一带发完才画下一带（见 redraw_locked）。
        // 深度留 10 只为不让驱动在回收事务时阻塞，不改变同步语义。
        .on_color_trans_done = on_color_done,
        .user_ctx            = NULL,
    };
    err = esp_lcd_new_panel_io_spi((esp_lcd_spi_bus_handle_t)APM_LCD_SPI_HOST, &io_cfg, &s_io);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "panel IO 创建失败: %s", esp_err_to_name(err));
        panel_release_failed();
        return err;
    }

    esp_lcd_panel_dev_config_t dev = {
        .reset_gpio_num = APM_LCD_RST_GPIO,   // -1 → SWRESET 软复位
        .rgb_ele_order  = LCD_RGB_ELEMENT_ORDER_RGB,
        // **必须显式声明小端。** 我们直接往缓冲里写 `uint16_t` RGB565（比如 0x07E0），
        // 在小端 MCU 上它在内存里是 `E0 07`，SPI 先发 E0（低字节）。
        // 不设这一项时驱动默认 big-endian，面板会把 E0 当高字节 → 颜色全错
        // （绿变红、白变黄）。上游 BSP 没设是因为它走 LVGL，由 LV_COLOR_16_SWAP 翻转。
        .data_endian    = LCD_RGB_DATA_ENDIAN_LITTLE,
        .bits_per_pixel = 16,
    };
    err = esp_lcd_new_panel_st7789(s_io, &dev, &s_panel);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "面板创建失败: %s", esp_err_to_name(err));
        panel_release_failed();
        return err;
    }

    err = esp_lcd_panel_reset(s_panel);
    if (err == ESP_OK) err = esp_lcd_panel_init(s_panel);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "面板复位/初始化失败: %s", esp_err_to_name(err));
        panel_release_failed();
        return err;
    }

    for (size_t i = 0; i < sizeof(ST7789P3_CMDS) / sizeof(ST7789P3_CMDS[0]); i++) {
        const st_init_cmd_t *c = &ST7789P3_CMDS[i];
        err = esp_lcd_panel_io_tx_param(s_io, c->cmd, c->data, c->len);
        if (err != ESP_OK) {
            ESP_LOGE(TAG, "厂商初始化命令 0x%02X 失败: %s", c->cmd, esp_err_to_name(err));
            panel_release_failed();
            return err;
        }
        if (c->delay_ms) vTaskDelay(pdMS_TO_TICKS(c->delay_ms));
    }

    err = esp_lcd_panel_invert_color(s_panel, APM_LCD_INVERT ? true : false);
    if (err == ESP_OK) err = esp_lcd_panel_mirror(s_panel, false, false);
    if (err == ESP_OK) err = esp_lcd_panel_set_gap(s_panel, 0, 0);
    if (err == ESP_OK) err = esp_lcd_panel_disp_on_off(s_panel, true);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "面板配置失败: %s", esp_err_to_name(err));
        panel_release_failed();
        return err;
    }

    ESP_LOGI(TAG, "面板就绪 %dx%d", APM_LCD_W, APM_LCD_H);
    return ESP_OK;
}

// ---------------------------------------------------------------------------
// 重画
// ---------------------------------------------------------------------------
// 把整屏分成 APM_LCD_BAND_ROWS 行一带，逐带渲染 + 推送。
//
// 为什么不做"整帧渲染再一次推"：整帧要 150 KB RAM，C3 没有。
//
// **必须双缓冲**（2026-11-08 真机踩出来的）：
// `esp_lcd_panel_io_spi.c` 的 `tx_color` 把 `tx_buffer` 直接挂到 SPI 事务上，
// `spi_device_queue_trans` 之后**立刻返回，不拷贝数据**（见该文件 392 行附近）。
// 一带 240×16×2 = 7680 字节在 80 MHz 上要 ~768 µs，而渲染一带只要几十 µs ——
// 单缓冲时下一带会在 DMA 还在读上一带时就把它覆盖掉。
//
// 症状：屏幕确实有内容（画的也确实是字），但笔画是几条带互相污染的碎片。
// 单缓冲在主机侧任何测试都测不出来（DMA 是硬件行为），只能靠真机 + 双缓冲解决。
static void redraw_locked(void)
{
    if (!s_panel) return;

    s_canvas.w = APM_LCD_W;

    for (int y0 = 0; y0 < APM_LCD_H; y0 += APM_LCD_BAND_ROWS) {
        int rows = APM_LCD_H - y0;
        if (rows > APM_LCD_BAND_ROWS) rows = APM_LCD_BAND_ROWS;

        // 画到**与上一带不同的那块**缓冲：上一带的 DMA 可能还在读它。
        int slot = (y0 / APM_LCD_BAND_ROWS) & 1;
        s_canvas.px = s_band[slot];
        s_canvas.h = rows;
        s_canvas.y_origin = y0;
        apm_screen_draw(&s_canvas, &s_view);

        esp_err_t err = esp_lcd_panel_draw_bitmap(s_panel, 0, y0, APM_LCD_W, y0 + rows,
                                                  s_band[slot]);
        if (err != ESP_OK) {
            ESP_LOGW(TAG, "推屏失败（y=%d..%d）: %s", y0, y0 + rows, esp_err_to_name(err));
            return;
        }

        // **必须等这一带真的发完**才能进入下一带。
        //
        // `draw_bitmap` 返回时数据只是进了 SPI 队列（`spi_device_queue_trans` 之后
        // 立即返回，见 esp_lcd_panel_io_spi.c 的 tx_color）。不等的话下一带的渲染
        // 会覆盖上一带还在被 DMA 读的缓冲 —— 症状是屏幕上有字但笔画错乱。
        //
        // 超时取 200 ms：一带 7680 字节 @80 MHz 只要 ~0.8 ms，200 ms 是"SPI 真的卡死"的量级。
        if (s_dma_done) {
            xSemaphoreTake(s_dma_done, pdMS_TO_TICKS(200));
        }
    }
}

// ---------------------------------------------------------------------------
// 亮灭（硬件动作）
// ---------------------------------------------------------------------------
// 熄屏顺序：先关背光再让面板 Sleep In。
// 反过来会先看到面板把 RAM 清掉的一帧（花屏），然后才黑。
static void panel_off(void)
{
    bl_set_duty_pct(0);
    if (s_panel && !s_panel_asleep) {
        esp_err_t err = esp_lcd_panel_disp_sleep(s_panel, true);
        if (err != ESP_OK) ESP_LOGW(TAG, "ST7789 Sleep In 失败: %s", esp_err_to_name(err));
        s_panel_asleep = true;
    }
}

// 亮屏：让面板退出 Sleep In + 点背光，**不在这个函数里重画**。
//
// 为什么不在这里重画：点亮必须是"立刻"的。按键回调（apm_btn 任务）拿着锁只等 50 ms，
// 而整屏重画（20 带 × 逐带等 DMA）要 30–50 ms，正好卡在超时边上 —— 会偶发丢掉唤醒。
//
// 那内容变了怎么办？**由 screen_task 补画**：
//   - 灭屏期间 s_dirty 不再被清掉（见 screen_task 的注释），点亮后任务会看到它；
//   - 用户看到的是：背光先亮（立刻），下一帧任务刷上来（≤100 ms）。
//     这个顺序比"等 50 ms 再一起亮"更好——背光先亮说明按键被响应了。
//   - 开机第一帧是特例：apm_screen_start 里**先画完再点背光**，因为那时没有"立刻"要求。
static void panel_on(void)
{
    if (s_panel && s_panel_asleep) {
        esp_err_t err = esp_lcd_panel_disp_sleep(s_panel, false);
        if (err != ESP_OK) ESP_LOGW(TAG, "ST7789 Sleep Out 失败: %s", esp_err_to_name(err));
        s_panel_asleep = false;
    }
    bl_set_duty_pct(APM_BL_ON_PCT);

    // 背光已经亮了，但屏幕上的内容可能还是熄屏前那一帧（灭屏期间的连接/标签变化
    // 只置了 dirty）。**叫醒屏幕任务立刻补画**，别让它等下一次 100 ms 轮询 ——
    // 否则用户点亮后先看到一帧旧内容再跳变，观感像"显示错了"。
    if (s_task && s_dirty) {
        xTaskNotifyGive(s_task);
    }
}

// ---------------------------------------------------------------------------
// 内部：刷新视图
// ---------------------------------------------------------------------------
// 把 s_state 翻译成渲染视图。标签文本先拷进 s_label_buf 再指向它，
// 因为 apm_screen_label() 返回的是状态结构体内部的指针，重画期间必须稳定。
static void refresh_view_locked(void)
{
    for (int i = 0; i < 3; i++) {
        const char *src = apm_screen_label(&s_state, (uint8_t)i);
        strncpy(s_label_buf[i], src, sizeof s_label_buf[i] - 1);
        s_label_buf[i][sizeof s_label_buf[i] - 1] = '\0';
        s_view.labels[i] = s_label_buf[i];
    }
    s_view.capture_btn = apm_screen_capture_btn(&s_state);
    // 底部提示只有一份：灭屏时屏幕看不见，做两份文案没有意义。
    s_view.hint = APM_TXT_HINT_ON;
}

// ---------------------------------------------------------------------------
// 屏幕任务
// ---------------------------------------------------------------------------
// 100 ms 一轮，只做两件事：
//   1. 内容需要重画时刷一遍（s_dirty）；
//   2. 低电量短闪的相位推进。
//
// **亮灭切换不在这里做**：它在 apm_screen_wake / off / on_key 里同步完成，因为
// "按键 → 屏幕亮"必须是立刻的，绕一趟任务队列会让人觉得按了没反应。
// 两处都做的话，一次切换会刷两遍屏（第一遍在 API 里、第二遍在任务里看到状态变了）。
//
// 不用事件队列：刷新是低频动作（只在电量/状态/标签变化时），
// 一个 10 Hz 轮询比给每个来源都接一条通知简单，且不会漏。
static void screen_task(void *arg)
{
    (void)arg;

    for (;;) {
        // 等一次通知或 100 ms。panel_on 在有未消费的 dirty 时会主动通知，
        // 这样"点亮后立刻补画"不用等下一次轮询。
        xTaskNotifyWait(0, 0, NULL, pdMS_TO_TICKS(100));

        if (xSemaphoreTake(s_mutex, pdMS_TO_TICKS(100)) == pdTRUE) {
            uint32_t now = now_ms();

            // 低电量短闪：灭屏时每 400 ms 亮 60 ms，共 3 次（1.2 s）。
            if (s_flash_until_ms) {
                if (now >= s_flash_until_ms || s_state.on) {
                    s_flash_until_ms = 0;
                    if (!s_state.on) bl_set_duty_pct(0);
                } else {
                    uint32_t phase = (now - (s_flash_until_ms - 1200)) % 400;
                    bl_set_duty_pct(phase < 60 ? APM_BL_FLASH_PCT : 0);
                }
            }

            // 内容重画。
            //
            // **灭屏时不能清掉 s_dirty**（2026-11-08 真机踩出来的）：真机上出现过
            // "屏幕已自动熄屏 → 用户在 PC 上连接 → 屏幕点亮后仍显示「未连接」+ 默认文案
            // 「快捷键」"。原因是那次连接与标签下发都发生在灭屏期间，而旧的代码把 dirty
            // 清掉却没画，数据永久丢了。
            //
            // 现在的规则：dirty 只表示"内容变了"，**只在真的画过之后才清**。
            // 灭屏时保留 dirty，等屏幕点亮后由这条分支补画（点亮由 panel_on 立即完成，
            // 见其注释：点亮要快、重画可以让任务慢一拍）。
            if (apm_screen_take_dirty(s_dirty, s_state.on)) {
                s_dirty = false;
                refresh_view_locked();
                redraw_locked();
            }
            xSemaphoreGive(s_mutex);
        }
        // 不需要再 vTaskDelay：循环开头的 xTaskNotifyWait 已经带 100 ms 超时。
    }
}

// ---------------------------------------------------------------------------
// 公开接口
// ---------------------------------------------------------------------------
bool apm_screen_ready(void) { return s_ready; }

esp_err_t apm_screen_start(void)
{
    if (s_ready) return ESP_OK;

    s_mutex = xSemaphoreCreateMutex();
    if (!s_mutex) {
        ESP_LOGE(TAG, "屏幕互斥锁创建失败");
        return ESP_ERR_NO_MEM;
    }

    // 二进制信号量的初始值是"已给出"：第一带开始前不需要谁先 give 一次。
    s_dma_done = xSemaphoreCreateBinary();
    if (!s_dma_done) {
        ESP_LOGE(TAG, "DMA 完成信号量创建失败");
        return ESP_ERR_NO_MEM;
    }
    xSemaphoreGive(s_dma_done);

    apm_screen_init(&s_state, now_ms());
    s_view.battery_pct = -1;   // 还没读到电量：显示「电量不可用」而不是 0%
    s_view.capture_btn = apm_screen_capture_btn(&s_state);
    refresh_view_locked();

    esp_err_t err = panel_init();
    if (err != ESP_OK) {
        // 屏幕失败不阻塞启动：设备仍然是可用的麦克风，只是看不到内容。
        ESP_LOGE(TAG, "屏幕初始化失败（%s），本次运行不显示内容", esp_err_to_name(err));
        return err;
    }
    err = bl_init();
    if (err != ESP_OK) {
        panel_release_failed();
        return err;
    }

    s_ready = true;
    if (xTaskCreate(screen_task, "apm_screen", 3072, NULL, 3, &s_task) != pdPASS) {
        ESP_LOGE(TAG, "屏幕任务创建失败");
        s_ready = false;
        return ESP_FAIL;
    }

    // 开机先画完再点背光：这样用户看到的第一帧就是完整内容，
    // 而不是"黑屏闪一下、字一块块冒出来"。之后由 30 s 自动熄屏接管。
    xSemaphoreTake(s_mutex, portMAX_DELAY);
    refresh_view_locked();
    redraw_locked();
    panel_on();
    xSemaphoreGive(s_mutex);

    ESP_LOGI(TAG, "屏幕就绪：%d s 无操作自动熄屏", APM_SCREEN_IDLE_OFF_MS / 1000);
    return ESP_OK;
}

bool apm_screen_on_key(uint8_t btn, bool is_long)
{
    if (!s_ready || !s_mutex) return false;
    if (xSemaphoreTake(s_mutex, pdMS_TO_TICKS(50)) != pdTRUE) return false;

    apm_screen_evt_t e = apm_screen_key_event(&s_state, btn, is_long, now_ms());
    bool changed = false;
    switch (e) {
    case APM_SCREEN_EVT_WAKE:
        panel_on();
        changed = true;
        break;
    case APM_SCREEN_EVT_TOGGLE:
        panel_off();
        s_flash_until_ms = 0;
        changed = true;
        break;
    default:
        break;
    }
    xSemaphoreGive(s_mutex);
    return changed;
}

void apm_screen_tick(void)
{
    if (!s_ready || !s_mutex) return;
    if (xSemaphoreTake(s_mutex, pdMS_TO_TICKS(50)) != pdTRUE) return;

    // 自动熄屏。`apm_screen_tick` 内部在采集中会自己刷新计时，这里只处理"该灭了"。
    if (apm_screen_auto_off_tick(&s_state, now_ms())) {
        panel_off();
        s_flash_until_ms = 0;
        ESP_LOGI(TAG, "无操作自动熄屏");
    }
    xSemaphoreGive(s_mutex);
}

bool apm_screen_wake(void)
{
    if (!s_ready || !s_mutex) return false;
    if (xSemaphoreTake(s_mutex, pdMS_TO_TICKS(50)) != pdTRUE) return false;
    bool was_off = !s_state.on;
    apm_screen_turn_on(&s_state, now_ms());
    if (was_off) panel_on();
    xSemaphoreGive(s_mutex);
    return was_off;
}

bool apm_screen_off(void)
{
    if (!s_ready || !s_mutex) return false;
    if (xSemaphoreTake(s_mutex, pdMS_TO_TICKS(50)) != pdTRUE) return false;
    bool ok = apm_screen_force_off(&s_state);
    if (ok) {
        panel_off();
        s_flash_until_ms = 0;
    }
    xSemaphoreGive(s_mutex);
    return ok;
}

void apm_screen_blink_low_battery(void)
{
    if (!s_ready || !s_mutex) return;
    // 亮屏时屏幕内容本身就在提示低电量，不做无意义的背光闪烁。
    if (s_state.on) return;
    s_flash_until_ms = now_ms() + 1200;
}

void apm_screen_set_battery(int pct, bool low)
{
    if (!s_ready || !s_mutex) return;
    if (xSemaphoreTake(s_mutex, pdMS_TO_TICKS(50)) != pdTRUE) return;
    if (s_view.battery_pct != pct || s_view.low_battery != low) {
        s_view.battery_pct = pct;
        s_view.low_battery = low;
        s_dirty = true;
    }
    xSemaphoreGive(s_mutex);
}

void apm_screen_set_capturing(bool capturing)
{
    if (!s_ready || !s_mutex) return;
    if (xSemaphoreTake(s_mutex, pdMS_TO_TICKS(50)) != pdTRUE) return;
    if (s_view.capturing != capturing) {
        // 只改**内容**（状态行显示「采集中」），不改亮灭。
        //
        // 2026-11-08 用户明确要求「开麦不需要点亮屏幕，省电更重要」：
        // 上一版在这里主动点亮，理由是"PC 远端开麦时没有按键，黑屏会让人不知道
        // 麦克风开了"。但那条理由换来的是"每次开麦都点亮一次背光"，
        // 而开麦/关麦在一次会话里会反复发生——对 500 mAh 电池不值。
        //
        // 现在的取舍：**屏幕亮不亮完全由用户手势决定**。若用户想知道是否在采集，
        // 长按 UP/DOWN 点亮看一眼即可（那时状态行显示「采集中」）。
        s_view.capturing = capturing;
        s_dirty = true;
    }
    xSemaphoreGive(s_mutex);
}

void apm_screen_set_connected(bool connected)
{
    if (!s_ready || !s_mutex) return;
    if (xSemaphoreTake(s_mutex, pdMS_TO_TICKS(50)) != pdTRUE) return;
    if (s_view.connected != connected) {
        s_view.connected = connected;
        s_dirty = true;
        // 灭屏期间收到的状态变化要留痕：真机上"连上了但屏幕显示未连接"就是
        // 这类变化发生在熄屏期间。日志里能看到"改了、但屏幕是灭的"，一眼能对上。
        if (!s_state.on) {
            ESP_LOGI(TAG, "连接状态 %s（屏幕灭着，等点亮后补画）",
                     connected ? "已连接" : "未连接");
        }
    }
    xSemaphoreGive(s_mutex);
}

bool apm_screen_set_labels(uint8_t capture_btn, const char *up, const char *down,
                           const char *ok)
{
    if (!s_ready || !s_mutex) return false;
    if (xSemaphoreTake(s_mutex, pdMS_TO_TICKS(50)) != pdTRUE) return false;

    bool clean = true;
    if (up) clean &= apm_screen_set_label(&s_state, 0, up, strlen(up));
    if (down) clean &= apm_screen_set_label(&s_state, 1, down, strlen(down));
    if (ok) clean &= apm_screen_set_label(&s_state, 2, ok, strlen(ok));
    apm_screen_set_capture_btn(&s_state, capture_btn);
    s_dirty = true;

    // 与 set_connected 同理：灭屏期间下发标签属于常态（PC 连上时屏幕可能已经熄了），
    // 这条日志用来确认"收到了、只是还没画"。
    if (!s_state.on) {
        ESP_LOGI(TAG, "收到按键标签（屏幕灭着，等点亮后补画）");
    }

    xSemaphoreGive(s_mutex);
    return clean;
}

void apm_screen_clear_labels(void)
{
    if (!s_ready || !s_mutex) return;
    if (xSemaphoreTake(s_mutex, pdMS_TO_TICKS(50)) != pdTRUE) return;
    apm_screen_reset_labels(&s_state);
    s_dirty = true;
    xSemaphoreGive(s_mutex);
}
