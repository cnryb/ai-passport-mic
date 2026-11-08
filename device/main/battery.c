// device/main/battery.c
// CW2017 电量计 + 低电量按键短闪指示。
//
// 硬件事实来自上游 FoloToy BSP（gitee.com/FoloToy/ai-passport bsp_pins.h，MIT）：
// 电量计与 ES8311 共用一条 I2C 总线（0x63）；GPIO21 背光是 LEDC PWM。
// 屏幕第一版不驱动，背光只当**被动查询式**低电量指示灯：平时恒灭（零功耗），
// SOC <= 20% 时按任意键，背光短闪 3 次。充电口旁的充电指示灯是充电 IC 硬件直驱
// （上游 BSP 能力契约无 LED 接口），MCU 不可控。
//
// 协议依据（CellWise **CW2017** datasheet；寄存器映射与 CW2015 **不同**）：
//   - 0x00 VERSION（本板读回 0xA0）
//   - 0x02/0x03 VCELL，14 位无符号，312.5 µV/LSB
//     （高字节 0x02 只有**低 6 位**是数据，bit7-6 保留：取值要 `& 0x3F`，不能右移）
//   - 0x04 = SOC 整数 %，0x05 = 1/256 % 小数（两者并列，不乘 100）
//   - 0x06 TEMP（只读；CW2015 这里是 RRT，**不要照 CW2015 写 0xAA**）
//   - 0x08 CONFIG，默认 0xF0 = **sleep**；唤醒写 0x30 清 sleep，再写 0x00 清 restart
//   - 0x0A INT_CONF，默认 0x40（CW2015 这里是 MODE，睡眠判断不能看这一位）
//
// 2026-11-06 修正（这才是"插着 USB 也永远 0%"的根因）：
//   上一版按 CW2015 的布局办事——写 0x06 触发 RRT、读 0x0A 判是否 0x30（睡眠）。
//   CW2017 的 0x0A 是 INT_CONF，默认就是 0x40，判定永远不成立，于是**从未唤醒**；
//   sleep 态下 VCELL / SOC 读出来恒为 0，界面就一直是"0%，请充电"。
//   现场 dump `A0 00 00 00 00 00 00 00 F0 00 40 14 AA 50 00 00` 里 0x08/0x0A/0x0B/0x0C/0x0D
//   全是上电默认值，正是"芯片一次都没被正确初始化过"的铁证。
//
// Unverified（真机未验）：唤醒后 CW2017 的实际读数、短闪观感。
#include "battery.h"

#include "audio.h"
#include "battery.h"
#include "board.h"
#include "link.h"
#include "screen.h"

#include "esp_log.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"

#include <string.h>

static const char *TAG = "apm_battery";

// CW2017 寄存器（CellWise CW2017 datasheet）。**不要把 CW2015 的布局搬过来**：
// 两代的 0x06 / 0x0A 含义完全不同（CW2015 = RRT / MODE，CW2017 = TEMP / INT_CONF）。
#define APM_CW2017_VERSION_REG  0x00
#define APM_CW2017_VCELL_REG    0x02
#define APM_CW2017_SOC_REG      0x04
#define APM_CW2017_TEMP_REG     0x06   // 只读
#define APM_CW2017_CONFIG_REG   0x08
#define APM_CW2017_INT_CONF_REG 0x0A   // 默认 0x40，**不是**睡眠标志
// 唤醒（datasheet "Power State"）：上电默认 sleep，两步退出——
// 先写 0x30 到 CONFIG 清 sleep 位，再写 0x00 清 restart 位。
#define APM_CW2017_CONFIG_WAKE_CLEAR_SLEEP   0x30
#define APM_CW2017_CONFIG_WAKE_CLEAR_RESTART 0x00
// VCELL 换算：CW2017 = 312.5 µV/LSB（CW2015 才是 305 µV，别混用）。
#define APM_CW2017_VCELL_UV_PER_LSB 3125   // 以 0.1 mV 为单位做整数运算

// ---------------------------------------------------------------------------
// 电池 profile（BATINFO，0x10–0x5F，80 字节）
//
// CW2017 **没有内置电池模型**。datasheet 的 "Battery Profile" 一节写明：profile
// 由整机厂按电芯标定后写入，用于 4.20/4.35/4.40 V 等不同电芯；缺失时 SOC 不会
// 报错，只会一直停在 0xFFxx（无效值）。本板实测 SOC = 0xFF00 / 0xFEE8，VCELL 正常
// （3.9 V），VERSION=0x0F、CONFIG=0x00（都在正常运行态）——就是 profile 没加载。
//
// 下面这份是 FoloToy AI Passport 上游 BSP 给**内置优特利（UTL）520 mAh** 电芯
// 标定的数据（components/bsp/src/bsp_battery.c，commit 7eeb76d）。
// **换电芯必须重新标定**：写错不会报错，只会安静地给出错误的百分比，比没有更危险。
#define APM_CW2017_SOC_ALERT_REG   0x0B
#define APM_CW2017_PROFILE_REG     0x10
#define APM_CW2017_PROFILE_LEN     80
// SOC_ALERT bit7 = UPDATE：写完 profile 置 1，芯片重启加载后自己清 0。
#define APM_CW2017_PROFILE_UPDATE  0x80

static const uint8_t s_profile[APM_CW2017_PROFILE_LEN] = {
    0x64, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xAD, 0xC7, 0xC8, 0xCA, 0xBD, 0xB1, 0xC1, 0x94,
    0x88, 0xD1, 0xBD, 0x97, 0x88, 0x66, 0x56, 0x4A,
    0x3F, 0x33, 0x26, 0x5C, 0x37, 0xD1, 0x27, 0xD8,
    0xCC, 0xB7, 0xCF, 0xB3, 0xB2, 0xAE, 0xA6, 0x9E,
    0x99, 0x97, 0x9B, 0x86, 0x47, 0x1E, 0x17, 0x26,
    0x49, 0x96, 0xD9, 0xE1, 0xDD, 0xDC, 0xD4, 0x59,
    0x00, 0x00, 0x90, 0x02, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x64, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x5C,
};

static i2c_master_dev_handle_t s_cw;
static bool     s_ready;
static uint8_t  s_soc;      // 最近一次成功读数
static bool     s_have_soc; // 是否读到过至少一次
static bool     s_low;
static uint8_t  s_last_raw[2];  // 最近一次读到的 0x04/0x05 原始字节，只给日志看

bool apm_battery_ready(void) { return s_have_soc; }
uint8_t apm_battery_soc(void) { return s_soc; }

static esp_err_t cw_read(uint8_t reg, uint8_t *buf, size_t len)
{
    return i2c_master_transmit_receive(s_cw, &reg, 1, buf, len, 50);
}

static esp_err_t cw_write(uint8_t reg, uint8_t val)
{
    uint8_t b[2] = { reg, val };
    return i2c_master_transmit(s_cw, b, 2, 50);
}

/// 切 CONFIG：datasheet 要求先写 0x30（复位态）再写目标值，两步之间留 20 ms。
static esp_err_t cw_set_mode(uint8_t mode)
{
    esp_err_t err = cw_write(APM_CW2017_CONFIG_REG, APM_CW2017_CONFIG_WAKE_CLEAR_SLEEP);
    if (err != ESP_OK) return err;
    vTaskDelay(pdMS_TO_TICKS(20));
    err = cw_write(APM_CW2017_CONFIG_REG, mode);
    if (err != ESP_OK) return err;
    vTaskDelay(pdMS_TO_TICKS(10));
    return ESP_OK;
}

/// 把芯片里的 BATINFO 与官方 profile 逐字节比对。
/// `read_ok=false` 表示 I2C 读失败——这时**不能**下"需要写入"的结论，
/// 也可能是总线问题，贸然写入会把一个可恢复的故障变成两份错误数据。
static bool profile_matches(bool *read_ok)
{
    *read_ok = true;
    uint8_t chunk[16];
    for (uint8_t base = 0; base < APM_CW2017_PROFILE_LEN; base += (uint8_t)sizeof chunk) {
        if (cw_read((uint8_t)(APM_CW2017_PROFILE_REG + base), chunk, sizeof chunk) != ESP_OK) {
            *read_ok = false;
            return false;
        }
        if (memcmp(chunk, &s_profile[base], sizeof chunk) != 0) return false;
    }
    return true;
}

/// 写入 profile 并让芯片重新加载。
///
/// 时序照 datasheet：BATINFO 只能在 sleep/复位态（CONFIG=0xF0）写；写完置
/// SOC_ALERT 的 UPDATE 位，再 0x30 → 0x00 重启，芯片才会重新加载 profile 并计算 SOC。
/// 任何一步失败都不置 UPDATE 位——宁可继续显示"电量不可用"，也不要让芯片带着
/// 半份 profile 去算出一个看似正常的错误百分比。
static esp_err_t profile_install(void)
{
    esp_err_t err = cw_set_mode(0xF0);
    if (err != ESP_OK) return err;

    uint8_t mode = 0;
    if (cw_read(APM_CW2017_CONFIG_REG, &mode, 1) != ESP_OK) return ESP_FAIL;
    if (mode != 0xF0) {
        ESP_LOGW(TAG, "写 profile 前 CONFIG=0x%02X（期望 0xF0），放弃写入", mode);
        return ESP_ERR_INVALID_STATE;
    }

    // 逐字节写。BATINFO 是芯片内部配置区，不赌个别批次对连续突发写的处理一致；
    // 80 次传输只在开机且检测到 profile 缺失时跑一次，耗时可接受。
    for (size_t i = 0; i < APM_CW2017_PROFILE_LEN; i++) {
        err = cw_write((uint8_t)(APM_CW2017_PROFILE_REG + i), s_profile[i]);
        if (err != ESP_OK) {
            ESP_LOGE(TAG, "写 profile 第 %u 字节失败：%s", (unsigned)i, esp_err_to_name(err));
            return err;
        }
    }

    bool read_ok = false;
    if (!profile_matches(&read_ok)) {
        ESP_LOGE(TAG, "profile 回读不一致（读取%s），不置 UPDATE 位",
                 read_ok ? "正常" : "失败");
        return ESP_ERR_INVALID_STATE;
    }

    uint8_t alert = 0;
    if (cw_read(APM_CW2017_SOC_ALERT_REG, &alert, 1) != ESP_OK) return ESP_FAIL;
    err = cw_write(APM_CW2017_SOC_ALERT_REG, (uint8_t)(alert | APM_CW2017_PROFILE_UPDATE));
    if (err != ESP_OK) return err;

    ESP_LOGI(TAG, "profile 已写入并回读校验通过，重启芯片加载");
    return cw_set_mode(APM_CW2017_CONFIG_WAKE_CLEAR_RESTART);
}

/// 打一份 BATINFO 快照。只在开机打一次：这是"芯片里到底有没有 profile"的唯一证据。
/// 十六进制全量输出，方便与官方 profile 逐字节比对。
static void dump_batinfo(const char *when)
{
    uint8_t chunk[16];
    bool all_zero = true;
    bool all_ff = true;
    bool ok = true;

    for (uint8_t base = 0; base < APM_CW2017_PROFILE_LEN; base += (uint8_t)sizeof chunk) {
        uint8_t reg = (uint8_t)(APM_CW2017_PROFILE_REG + base);
        if (cw_read(reg, chunk, sizeof chunk) != ESP_OK) {
            ESP_LOGW(TAG, "BATINFO %s：%02X 起读取失败", when, reg);
            ok = false;
            continue;
        }
        ESP_LOGI(TAG, "BATINFO %s %02X-%02X = %02X %02X %02X %02X %02X %02X %02X %02X"
                      " %02X %02X %02X %02X %02X %02X %02X %02X",
                 when, reg, (uint8_t)(reg + sizeof chunk - 1),
                 chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
                 chunk[8], chunk[9], chunk[10], chunk[11], chunk[12], chunk[13], chunk[14], chunk[15]);
        for (size_t i = 0; i < sizeof chunk; i++) {
            if (chunk[i] != 0x00) all_zero = false;
            if (chunk[i] != 0xFF) all_ff = false;
        }
    }

    if (!ok) {
        ESP_LOGW(TAG, "BATINFO %s：未读完整，profile 是否存在仍不确定", when);
    } else if (all_zero || all_ff) {
        ESP_LOGW(TAG, "BATINFO %s：%s（未写入 profile）", when, all_zero ? "全 0" : "全 FF");
    } else {
        ESP_LOGI(TAG, "BATINFO %s：有数据（需与电芯 profile 逐字节比对）", when);
    }
}

esp_err_t apm_battery_init(void)
{
    if (s_ready) return ESP_OK;

    i2c_master_bus_handle_t bus = apm_audio_i2c_bus();
    if (!bus) {
        return ESP_ERR_INVALID_STATE;
    }

    i2c_device_config_t dev_cfg = {
        .dev_addr_length = I2C_ADDR_BIT_LEN_7,
        .device_address  = APM_I2C_CW2017_ADDR,
        .scl_speed_hz    = 400000,
    };
    esp_err_t err = i2c_master_bus_add_device(bus, &dev_cfg, &s_cw);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "CW2017 设备句柄创建失败: %s", esp_err_to_name(err));
        return err;
    }

    // 先确认芯片在。sleep 态下 I2C 仍然应答，所以这一步只验通路，不验工作状态。
    //
    // 2026-11-06 真机（13:49 / 14:03 两次）：**单字节读 VERSION 不可靠**——同一块板
    // 单字节读稳定返回 0x0F，而 2 字节读和 16 字节连读都返回正确的 0xA0。
    // 所以这里按 2 字节读，只取第一个字节；这不是芯片问题，是 1 字节读的时序问题。
    uint8_t ver[2] = { 0 };
    for (int i = 0; i < 3; i++) {
        err = cw_read(APM_CW2017_VERSION_REG, ver, 2);
        ESP_LOGI(TAG, "VERSION 第 %d 次：%s，值 0x%02X",
                 i + 1, err == ESP_OK ? "OK" : esp_err_to_name(err), ver[0]);
        if (err == ESP_OK) break;
        vTaskDelay(pdMS_TO_TICKS(20));
    }
    if (err != ESP_OK) {
        // 无应答多半是板上真没焊这颗芯片：不卡启动，电量只是不可用。
        ESP_LOGW(TAG, "CW2017 无应答（%s），电量功能不可用", esp_err_to_name(err));
        s_cw = NULL;
        return err;
    }

    // 电量无效的根因在 profile，不在电压。先留一份修复前快照，再与官方 profile
    // 比对；不一致才写。顺序有讲究：BATINFO 只能在 CONFIG=0xF0（sleep）时写，
    // 所以"写 profile"必须排在"唤醒"之前，否则等于先把自己叫醒再去睡。
    bool woke = false;
    bool profile_written = false;
    bool read_ok = false;
    if (profile_matches(&read_ok)) {
        ESP_LOGI(TAG, "BATINFO 与官方 UTL 520mAh profile 一致，无需写入");
    } else if (!read_ok) {
        // 读失败不等于缺失：可能是总线抖动。不写，避免把可恢复的故障变成两份错误数据。
        ESP_LOGW(TAG, "BATINFO 读取失败，无法判断 profile 是否缺失；本次不写入");
    } else {
        // 先留证据再覆盖：这是判断"到底有没有 profile"的唯一现场。
        dump_batinfo("修复前");
        ESP_LOGW(TAG, "BATINFO 与官方 profile 不一致：按缺失处理，开始写入");
        err = profile_install();
        if (err == ESP_OK) {
            woke = true;  // profile_install 末尾已经 0x30 → 0x00 重启过
            profile_written = true;
        } else {
            ESP_LOGE(TAG, "profile 写入失败（%s），电量仍会不可用", esp_err_to_name(err));
        }
    }

    // 唤醒：CW2017 上电默认 sleep（CONFIG=0xF0），sleep 下 VCELL / SOC 恒为 0。
    // 已经由 profile_install 重启过的不再复位一次——每次重启都会让 FastCali
    // 从头估算，多按一次只会把首次有效读数往后推。
    if (!woke) {
        err = cw_set_mode(APM_CW2017_CONFIG_WAKE_CLEAR_RESTART);
        if (err != ESP_OK) {
            // 写失败不返回错误：后面读寄存器会自己暴露问题，交给日志。
            ESP_LOGW(TAG, "CW2017 唤醒写入失败（%s），读数可能仍为 0", esp_err_to_name(err));
        }
    }

    s_ready = true;
    ESP_LOGI(TAG, "CW2017 就绪 VERSION=0x%02X I2C 0x%02X，低电量阈值 %d%%",
             ver[0], APM_I2C_CW2017_ADDR, APM_BATTERY_LOW_SOC_PCT);

    // FastCali 首次估算 + 首轮 ADC 转换都要时间。实测（14:03）：唤醒后 200 ms 读到的
    // VCELL 仍是 0x0000，到 ~400 ms 才出现 0x336B。取 600 ms 留足余量，
    // 免得开机那条"VCELL 仍为 0"的 warn 变成误报。
    vTaskDelay(pdMS_TO_TICKS(600));

    // 诊断：0x00–0x0F 全 dump，按 CW2017 的映射读这一行：
    //   0x00 VERSION / 0x02-03 VCELL / 0x04-05 SOC / 0x06 TEMP /
    //   0x08 CONFIG / 0x0A INT_CONF / 0x0B SOC_ALERT / 0x0C TEMP_MAX / 0x0D TEMP_MIN
    // 唤醒成功后 0x08 应从默认 0xF0 变成 0x00，VCELL 也应非 0——两处都能一眼验证。
    uint8_t regs[16] = { 0 };
    if (cw_read(0x00, regs, sizeof regs) == ESP_OK) {
        ESP_LOGI(TAG, "reg00-0F = %02X %02X %02X %02X %02X %02X %02X %02X %02X %02X %02X %02X %02X %02X %02X %02X",
                 regs[0], regs[1], regs[2], regs[3], regs[4], regs[5], regs[6], regs[7],
                 regs[8], regs[9], regs[10], regs[11], regs[12], regs[13], regs[14], regs[15]);

        uint32_t raw14 = (((uint32_t)regs[2] & 0x3F) << 8) | regs[3];
        ESP_LOGI(TAG, "唤醒后 VCELL raw=%lu ≈%lumV，SOC=%u%%",
                 (unsigned long)raw14,
                 (unsigned long)(raw14 * APM_CW2017_VCELL_UV_PER_LSB / 10000),
                 regs[4]);
        if (raw14 == 0) {
            ESP_LOGW(TAG, "唤醒后 VCELL 仍为 0：要么芯片没退出 sleep（CONFIG 看 0x08），"
                          "要么电芯端确实没有电压（电池包未接 / 保护板锁死），需万用表定案");
        }
    } else {
        ESP_LOGW(TAG, "寄存器 dump 失败，芯片可能只在部分地址应答");
    }

    // profile 是这次改写的对象：写完再看一眼，确认重启加载后内容仍在。
    if (profile_written) {
        dump_batinfo("修复后");
    }

    // 诊断：同一批寄存器用 4 种读法各读一遍，定位"哪种读法可信"。
    // 现场已知：VERSION 单字节读回 0x0F，而同一次 16 字节连读的 0x00 是 0xA0 ——
    // 说明单字节读/连续读的结果不一致，SOC 读到 0xFE/0xFF 可能也是读法问题，不是真值。
    // 这 3 轮只用于本次定位，确认稳定后应删掉。
    for (int i = 0; i < 3; i++) {
        uint8_t ver1[1] = { 0 };
        uint8_t vc2[2] = { 0 };
        uint8_t so2[2] = { 0 };
        uint8_t so1[2] = { 0 };
        esp_err_t e1 = cw_read(0x00, ver1, 1);
        esp_err_t e2 = cw_read(APM_CW2017_VCELL_REG, vc2, 2);
        esp_err_t e3 = cw_read(APM_CW2017_SOC_REG, so2, 2);
        esp_err_t e4a = cw_read(APM_CW2017_SOC_REG, &so1[0], 1);
        esp_err_t e4b = cw_read(APM_CW2017_SOC_REG + 1, &so1[1], 1);
        ESP_LOGI(TAG, "读法 %d/3：VER[1B]=%s%02X | VCELL[2B]=%s%02X%02X | SOC[2B]=%s%02X%02X | SOC[1B×2]=%s%02X/%02X",
                 i + 1,
                 e1 == ESP_OK ? "" : "ERR ", ver1[0],
                 e2 == ESP_OK ? "" : "ERR ", vc2[0], vc2[1],
                 e3 == ESP_OK ? "" : "ERR ", so2[0], so2[1],
                 (e4a == ESP_OK && e4b == ESP_OK) ? "" : "ERR ", so1[0], so1[1]);
        vTaskDelay(pdMS_TO_TICKS(200));
    }

    return ESP_OK;
}

esp_err_t apm_battery_read(uint8_t *soc)
{
    if (!s_ready) return ESP_ERR_INVALID_STATE;
    if (!soc) return ESP_ERR_INVALID_ARG;

    // 一次连读 0x02–0x05：VCELL（2 字节）+ SOC（2 字节）。只读 SOC 也能工作，
    // 但 SOC=0 时没法和"测不到电芯"区分，而这两种在界面上是两回事：
    // 前者该提示"请充电"，后者该显示"电量不可用"。
    uint8_t buf[4];
    esp_err_t err = cw_read(APM_CW2017_VCELL_REG, buf, 4);
    if (err != ESP_OK) return err;
    s_last_raw[0] = buf[2];
    s_last_raw[1] = buf[3];

    // 0x04 = 整数 %，0x05 = 1/256 % 的小数部分 —— **两者是并列关系**，
    // 合起来就是 buf[2] + buf[3]/256，不存在任何乘法。
    //
    // 2026-11-06 修：上一版写成 buf[0]*100 + buf[1]*100/256，整整放大 100 倍，
    // 于是任何 >= 1% 的真实电量都撞上 100% 上限、一路显示"满电"，
    // 直到真实电量掉到 1% 以下才露出 0.97%→0% 的假读数。
    // 现场表现就是"一直 100%，然后突然没电关机"。别再乘 100。
    if (buf[2] > 100) {
        // 超出合法范围说明这一帧不可信。**不能返回 0**：0 在协议里是"真的没电"，
        // 会让界面提示用户去充电。宁可报错，让调用方维持上一次的值。
        return ESP_ERR_INVALID_RESPONSE;
    }

    // VCELL 与 SOC 同时为 0 = 电芯端测不到电压（芯片还在 sleep / 电池包未接 /
    // 保护板锁死）。这种情况**不能当 0% 上报**，否则界面会提示"请充电"，
    // 而真实情况是"根本测不到"——按不可用处理，上层维持上一次的值或显示不可用。
    // VCELL[13:8] 在 0x02 的**低 6 位**（bit7-6 保留），VCELL[7:0] 在 0x03。
    // 2026-11-06 修：上一版写 `>> 2`，等于又除以 4，把 4.11 V 的满电电池读成 1.03 V，
    // 差点判成"深度过放、要换电池"。这里只能屏蔽高 2 位，**不能右移**。
    uint32_t vcell_raw = (((uint32_t)buf[0] & 0x3F) << 8) | buf[1];
    if (vcell_raw == 0 && buf[2] == 0) {
        return ESP_ERR_INVALID_RESPONSE;
    }

    uint32_t pct = buf[2] + ((buf[3] >= 128) ? 1u : 0u);  // 小数部分四舍五入到整数
    if (pct > 100) pct = 100;
    *soc = (uint8_t)pct;
    return ESP_OK;
}

// ---------------------------------------------------------------------------
// 背光：**所有权已经交给 screen.c**（它要同时管屏幕亮灭与低电量提示）。
// 这里只保留"低电量时按任意键申请一次短闪"的入口。
// ---------------------------------------------------------------------------
void apm_battery_on_button(void)
{
    // 低电量 + 有读数才闪。屏幕层会自己判断"亮屏时不闪"（亮屏时屏幕上就写着电量，
    // 再闪背光是多余的）。
    if (s_low && s_have_soc) {
        apm_screen_blink_low_battery();
    }
}

// ---------------------------------------------------------------------------
// 电量任务：1 s 采样 SOC（芯片内部也是 1 s 刷新），变化时 BLE 上报。
//
// **背光不在这里管了**：屏幕模块接管背光之后，"低电量提示"变成屏幕内容的一部分
// （亮屏时直接写着电量，灭屏时按任意键让背光短闪一次）。
// 这里只负责"读得到、读不到怎么办"与"变化时上报"。
// I2C 偶发失败按"维持上次状态"处理，不抖动。
// ---------------------------------------------------------------------------
/// 读一次电芯电压原始值（14 位）。失败返回 false。
static bool vcell_raw(uint32_t *raw14)
{
    uint8_t v[2];
    if (cw_read(APM_CW2017_VCELL_REG, v, 2) != ESP_OK) return false;
    *raw14 = (((uint32_t)v[0] & 0x3F) << 8) | v[1];
    return true;
}

static void battery_task(void *arg)
{
    (void)arg;
    uint8_t last_notified = 0xFF;  // 0xFF 不可能来自合法读数，保证首读必通知
    bool first = true;
    // 时间基准用毫秒，不用"循环次数"：拿 tick 计数算节流会让阈值跟着周期漂。
    uint64_t boot_us = esp_timer_get_time();
    uint64_t last_read_us = 0;
    uint64_t last_warn_us = 0;
    bool warned = false;

    // 1 s 醒一次就够（芯片内部也是 1 s 刷新）。
    const uint32_t APM_GUARD_MS = 5000;    // 开机宽限 5 s（FastCali 首次估算要 2–3 s）
    const uint32_t APM_WARN_MS  = 60000;   // "电量不可用"每 60 s 一条
    const uint32_t APM_READ_MS  = 1000;    // SOC 采样周期

    for (;;) {
        // 采样周期恒 1 s。按键唤醒用不上了（背光归屏幕层），但保留 notify 位以待
        // 将来"立刻重读一次"这类需求，代价只有一次空唤醒。
        xTaskNotifyWait(0, 0, NULL, pdMS_TO_TICKS(APM_READ_MS));

        uint64_t now_us = esp_timer_get_time();
        uint32_t since_boot_ms = (uint32_t)((now_us - boot_us) / 1000);
        uint32_t since_read_ms = (uint32_t)((now_us - last_read_us) / 1000);

        if (since_read_ms >= APM_READ_MS) {
            last_read_us = now_us;
            uint8_t soc = 0;
            esp_err_t err = apm_battery_read(&soc);
            if (err == ESP_OK) {
                s_soc = soc;
                s_have_soc = true;
                bool low = soc <= APM_BATTERY_LOW_SOC_PCT;
                if (low != s_low) {
                    s_low = low;
                    ESP_LOGI(TAG, "电量 %u%%%s", soc,
                             low ? "（低电量：屏幕上会高亮显示）" : "");
                }
                if (first) {
                    // 首读打一条 INFO：现场判断"到底读到了什么"时，这是唯一能带回来的证据。
                    // 2026-11-06 就是靠这条（0x04=0 0x05=0 且 VCELL 也是 0）确认
                    // "不是算错，是电芯端真的测不到电压"。
                    //
                    // 一次读 0x00–0x07（按 CW2017 映射）：VERSION / 保留 / VCELL 高 /
                    // VCELL 低 / SOC_H / SOC_L / TEMP / 保留。
                    // VERSION 有正常值 = I2C 通路和芯片都在；VCELL = 0 = 电芯端测不到电压
                    //（芯片还在 sleep、没接电池、保护板锁死，或电芯已放废），SOC 再准也没意义。
                    // VCELL 按 14 位、312.5 µV/LSB 折算（CW2017 datasheet）。这个电压
                    // 只当诊断线索，不进协议、不进界面——界面只认 SOC。
                    first = false;
                    uint8_t dump[8] = { 0 };
                    if (cw_read(0x00, dump, sizeof dump) == ESP_OK) {
                        uint32_t raw14 = (((uint32_t)dump[2] & 0x3F) << 8) | dump[3];
                        ESP_LOGI(TAG,
                                 "首次读数 %u%%；reg00-07 = %02X %02X %02X %02X %02X %02X %02X %02X；"
                                 "VCELL raw=%lu ≈%lumV，TEMP=%u",
                                 soc, dump[0], dump[1], dump[2], dump[3],
                                 dump[4], dump[5], dump[6], dump[7],
                                 (unsigned long)raw14,
                                 (unsigned long)(raw14 * APM_CW2017_VCELL_UV_PER_LSB / 10000),
                                 dump[6]);
                    } else {
                        ESP_LOGI(TAG, "首次读数 %u%%，reg00-07 读不出来", soc);
                    }
                }
                if (soc != last_notified) {
                    // 只在读到合法值后才调：缓存（读特征值回的是它）不会被污染。
                    apm_link_notify_battery(soc);
                    last_notified = soc;
                }
            } else if (err != ESP_ERR_INVALID_STATE) {
                // 保持上一次的值，不把"读不到"当"没电"上报。
                // 每 60 s 一条：既能看到"一直没恢复"，又不会把串口刷满。
                // 只打一次是不行的——那会让"后来 I2C 全挂了"看起来像"一直静默正常"。
                //
                // 开机宽限 5 s：FastCali 的首次估算要 2–3 s（实测写入 profile 后
                // 2.6 s 才出 77%）。这段时间报"从未读到过有效电量"是误报，
                // 会被当成故障写进排查记录——KI-007 就是这么绕远的。
                uint32_t since_warn_ms = warned
                    ? (uint32_t)((now_us - last_warn_us) / 1000)
                    : since_boot_ms;
                if (since_boot_ms >= APM_GUARD_MS && since_warn_ms >= APM_WARN_MS) {
                    warned = true;
                    last_warn_us = now_us;
                    uint32_t raw = 0;
                    bool vcell_ok = vcell_raw(&raw);
                    uint32_t mv = vcell_ok ? raw * APM_CW2017_VCELL_UV_PER_LSB / 10000 : 0;
                    // 只有"值不合法"这一种走 INVALID_RESPONSE：芯片能读，但 SOC > 100
                    // （现场是 0xFF / 0xFE）。这是"电量未知"，不是"没电"——不能提示充电。
                    const char *batt_state =
                        (vcell_ok && mv < APM_BATTERY_UNDERVOLT_MV)
                            ? "（低于锂电欠压 2.5V：电池未接或已过放）"
                            : "";
                    if (s_have_soc) {
                        ESP_LOGW(TAG, "电量不可用（%s）：VCELL=%lumV%s，维持 %u%%",
                                 esp_err_to_name(err), (unsigned long)mv, batt_state, s_soc);
                    } else {
                        ESP_LOGW(TAG, "电量不可用（%s）：VCELL=%lumV%s，从未读到过有效电量",
                                 esp_err_to_name(err), (unsigned long)mv, batt_state);
                    }
                }
            }
        }
    }
}

esp_err_t apm_battery_start(void)
{
    esp_err_t err = apm_battery_init();
    if (err != ESP_OK) return err;

    if (xTaskCreate(battery_task, "apm_batt", 3072, NULL, 4, NULL) != pdPASS) {
        ESP_LOGE(TAG, "电量任务创建失败");
        return ESP_FAIL;
    }
    return ESP_OK;
}
