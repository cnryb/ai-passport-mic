// device/main/power_policy.h
// 省电相关的**纯决策逻辑**：采集态状态机、连接参数档位、LINK_SLOW 判据、发射功率滞回。
//
// 为什么单独抽出来：这些判断原先散在 link.c（NimBLE 回调语境）与 app_main.c（FreeRTOS
// 任务语境）里，两边都拉不起主机侧编译，于是"改了省电逻辑但没有任何测试能跑"——
// 2026-11-07 那批改动就是这个状态。抽成不依赖 ESP-IDF 的纯函数之后，
// `device/test/test_power_policy.c` 可以用系统 clang 直接跑，见 ./scripts/test-protocol.sh。
//
// **本文件不得包含任何 ESP-IDF 头文件。** 一旦引入，主机侧就编不过，测试随之作废。
#pragma once

#include <stdbool.h>
#include <stdint.h>

// ---------------------------------------------------------------------------
// 连接参数档位（跨端契约，见 docs/ARCHITECTURE.md 的「连接参数」节）
//
// 采集中必须 15–30 ms 才推得动 50 块/秒；空闲时放宽到 80 ms 省射频唤醒。
// 80 而不是 200：从空闲档切回快速档要等主机回 CONN_UPDATE，实际落在 1–2 个旧间隔之后，
// 80 ms 时是 60–160 ms，塞得进「OK 按下到首块音频 p95 ≤ 250 ms」；200 ms 会把开场
// 拖成可听停顿（已被明确否掉）。
// ---------------------------------------------------------------------------
#define APM_CONN_ITVL_MIN_MS      15
#define APM_CONN_ITVL_MAX_MS      30
#define APM_CONN_ITVL_IDLE_MS     80

// 实际间隔的容差：超过就要重新请求。
//   采集态 40 ms —— 再慢就撑不住 165 B × 50 块/秒（干净会话实测落在 30 ms）；
//   空闲态 120 ms —— 只要求"别被主机放到 500 ms 那种量级"，不按采集态判。
#define APM_CONN_ITVL_OK_MS       40
#define APM_CONN_ITVL_IDLE_OK_MS  120

typedef struct {
    uint16_t itvl_min_ms;
    uint16_t itvl_max_ms;
} apm_conn_itvl_t;

/// 当前档位应该请求的连接间隔范围。
apm_conn_itvl_t apm_conn_itvl_for(bool capturing);

/// 实际间隔是否符合当前档位（false = 该重新请求了）。
bool apm_conn_itvl_ok(bool capturing, uint16_t actual_ms);

/// STATUS 的 bit4 `LINK_SLOW` 是否该置位。
///
/// **只在采集态判**。空闲档本来就是 80 ms，按采集态的 40 ms 判据会把每一场空闲都
/// 报成慢档，PC 端就会把每一次正常会话都标脏（`Pipeline::link_slow` 是粘住的）。
///
/// `ms_since_capture` = 进入采集态至今的毫秒数，用于**宽限期**：
/// 从空闲档（80 ms）切到快速档要等主机回 CONN_UPDATE，实际落在 1–2 个旧间隔之后，
/// 也就是 80–160 ms 甚至更久。这期间 STATUS 心跳若正好读到旧的 80 ms，就会置位，
/// 而 PC 侧是整场粘住的 —— 一份 15 秒、块率 50.0/s、零丢块的健康录音会被判成
/// `software-fail`。**2026-11-07 真机就是这么踩到的。**
///
/// 所以进入采集后的 APM_LINK_SLOW_GRACE_MS 内一律不报。代价是真正的慢档会话
/// 晚 2 s 才被发现（那时块率已经掉到 1.5–2 块/秒，早 2 s 晚 2 s 不影响结论）。
bool apm_link_slow_flag(bool capturing, uint16_t actual_ms, uint32_t ms_since_capture);

/// 进入采集后多久才开始判 `LINK_SLOW`。取 2 s：心跳 1 s 一次，留够两个周期的余量。
#define APM_LINK_SLOW_GRACE_MS 2000

// ---------------------------------------------------------------------------
// 采集态状态机
//
// 语义来自 KI-005 的教训，改动前先读 docs/KNOWN_ISSUES.md：
//   - PTT 按住 或 PC 下发 START，任一成立就采集；
//   - CTRL STOP 只清 forced，**不能**把 PTT 一起清掉（否则用户按着开麦键、
//     PC 那边点了停止，键还按着却突然不采集）；
//   - PTT 抬起只清 ptt，**不能**顶掉 forced（否则一次抬手就把 PC 的 START 顶没）；
//   - 断链两者全清，不等 STOP —— 这就是 KI-005 的解药。
// ---------------------------------------------------------------------------
typedef struct {
    bool ptt_held;
    bool forced;
} apm_capture_t;

/// 是否在采集。等价于 `forced || ptt_held`。
bool apm_capture_on(const apm_capture_t *c);

/// PTT 按下 / 抬起。
void apm_capture_ptt(apm_capture_t *c, bool held);

/// CTRL START：置 forced，不动 ptt。
void apm_capture_start_cmd(apm_capture_t *c);

/// CTRL STOP：清 forced，不动 ptt。
void apm_capture_stop_cmd(apm_capture_t *c);

/// 断链：两者全清。
void apm_capture_disconnect(apm_capture_t *c);

// ---------------------------------------------------------------------------
// 发射功率
//
// 三档按 RSSI 分。注意这不是省电主力：连接间隔 30 ms、每周期 1–2 个 165 B 包，
// 发射占空比只有百分之几，降功率对平均电流的影响是 mA 量级；它的价值是缩短不必要的射程。
// ---------------------------------------------------------------------------
#define APM_TX_NEAR_DBM   (-3)   // RSSI >= -60：桌面近距离
#define APM_TX_MID_DBM    ( 3)   // RSSI >= -75：同房间
#define APM_TX_FAR_DBM    ( 9)   // 更差：原来的默认值（＝ sdkconfig 开机默认）
#define APM_TX_INITIAL_DBM APM_TX_FAR_DBM

// 降档需要连续几次评估都指向更低档。RSSI 在边界上的秒级抖动会让档位来回跳，
// 而每次跳都要重配射频；多等一轮的代价只是几秒额外功耗。
#define APM_TX_DOWNGRADE_STEPS 2

/// RSSI 落到哪一档（无滞回，只是映射）。
int8_t apm_tx_power_for_rssi(int8_t rssi);

typedef struct {
    int8_t  dbm;                // 当前功率
    int8_t  pending_dbm;        // 正在"想降到"的目标档；无挂起时等于 dbm
    uint8_t downgrade_pending;  // 连续指向 pending_dbm 的次数
} apm_tx_state_t;

/// 初始化到开机默认档。
void apm_tx_init(apm_tx_state_t *st);

/// 按 RSSI 决定要不要换档。返回 true 表示 `st->dbm` 变了，调用方需要真的重配射频。
///
/// 方向不对称：**升功率立刻生效**（宁可多费一点电，也不要因为反应慢而断链）；
/// **降功率要连续 APM_TX_DOWNGRADE_STEPS 次**才生效。
bool apm_tx_update(apm_tx_state_t *st, int8_t rssi);
