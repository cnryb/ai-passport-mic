// device/test/test_power_policy.c
// power_policy.c 的主机侧单测：不依赖 ESP-IDF，用系统 clang 直接编译运行。
//
//   ./scripts/test-protocol.sh      # 与协议单测一起跑
//
// 为什么要有这个文件：2026-11-07 那批省电改动（两档连接参数、LINK_SLOW 判据、
// 发射功率滞回、采集态状态机）上线时**一条测试都没有** —— 它们埋在 NimBLE 回调与
// FreeRTOS 任务里，主机侧编不起来，只能靠"编译通过 + 真机看一眼"。抽成纯函数后
// 这些判断才第一次可测。
#include "power_policy.h"

#include <stdio.h>

static int g_fail = 0;
static int g_pass = 0;

#define CHECK(cond, fmt, ...)                                                     \
    do {                                                                          \
        if (cond) {                                                               \
            g_pass++;                                                             \
            printf("  ok   " fmt "\n", ##__VA_ARGS__);                            \
        } else {                                                                  \
            g_fail++;                                                             \
            printf("  FAIL " fmt "  (%s:%d)\n", ##__VA_ARGS__, __FILE__, __LINE__); \
        }                                                                         \
    } while (0)

// ---------------------------------------------------------------------------
// 连接参数档位
// ---------------------------------------------------------------------------
static void test_conn_itvl_tiers(void)
{
    printf("[conn_itvl]\n");

    apm_conn_itvl_t cap = apm_conn_itvl_for(true);
    CHECK(cap.itvl_min_ms == 15 && cap.itvl_max_ms == 30,
          "采集态请求 15–30 ms（实际 %u–%u）", cap.itvl_min_ms, cap.itvl_max_ms);

    apm_conn_itvl_t idle = apm_conn_itvl_for(false);
    CHECK(idle.itvl_min_ms == 80 && idle.itvl_max_ms == 80,
          "空闲态请求 80 ms（实际 %u–%u）", idle.itvl_min_ms, idle.itvl_max_ms);

    // 空闲档必须严格大于采集档上限，否则"省电档"名不副实。
    CHECK(idle.itvl_max_ms > cap.itvl_max_ms,
          "空闲档 %u ms 严格大于采集档上限 %u ms",
          idle.itvl_max_ms, cap.itvl_max_ms);

    // 80 是延迟预算的上界约束：切回快速档要 1–2 个旧间隔，预算 250 ms。
    CHECK(idle.itvl_max_ms <= 100,
          "空闲档 %u ms 留够回切时间（≤100 ms 才能让 1–2 个旧间隔塞进 250 ms 预算）",
          idle.itvl_max_ms);
}

static void test_conn_itvl_ok(void)
{
    printf("[conn_itvl_ok]\n");

    // 采集态：40 ms 以内算合格，超了要重新请求。
    CHECK(apm_conn_itvl_ok(true, 30), "采集态 30 ms 合格");
    CHECK(apm_conn_itvl_ok(true, 40), "采集态 40 ms 正好在边界上，合格");
    CHECK(!apm_conn_itvl_ok(true, 41), "采集态 41 ms 不合格");
    CHECK(!apm_conn_itvl_ok(true, 500), "采集态 500 ms 不合格（KI-005 那种坏会话）");

    // 空闲态：容差 120 ms。**80 ms 必须合格**，否则空闲档每次心跳都在重发请求。
    CHECK(apm_conn_itvl_ok(false, 80), "空闲态 80 ms 合格（本次改动的核心目标值）");
    CHECK(apm_conn_itvl_ok(false, 120), "空闲态 120 ms 正好在边界上，合格");
    CHECK(!apm_conn_itvl_ok(false, 121), "空闲态 121 ms 不合格");
    CHECK(!apm_conn_itvl_ok(false, 500), "空闲态 500 ms 不合格（主机把间隔放太宽）");

    // 关键回归：80 ms 在空闲态合格，但在**采集态**不合格 —— 两档判据必须真的不同。
    CHECK(apm_conn_itvl_ok(false, 80) && !apm_conn_itvl_ok(true, 80),
          "同一个 80 ms：空闲态合格、采集态不合格（两档判据确实不同）");

    // 还没读到（0）不能判不合格，否则刚连上就白发一次请求。
    CHECK(apm_conn_itvl_ok(true, 0) && apm_conn_itvl_ok(false, 0),
          "实际间隔未知（0）时不判不合格");
}

// ---------------------------------------------------------------------------
// LINK_SLOW
// ---------------------------------------------------------------------------
static void test_link_slow(void)
{
    printf("[link_slow]\n");

    // 进入采集很久之后（超过宽限期）才按 40 ms 判，避免宽限期干扰其他断言。
    const uint32_t settled = APM_LINK_SLOW_GRACE_MS + 1000;

    // **最重要的一条**：空闲态无论多慢都不置位。
    // 若这条挂了，PC 端会把每一场空闲都标成脏会话，且 link_slow 是粘住的、洗不掉。
    CHECK(!apm_link_slow_flag(false, 80, settled), "空闲态 80 ms 不置 LINK_SLOW");
    CHECK(!apm_link_slow_flag(false, 200, settled), "空闲态 200 ms 也不置 LINK_SLOW");
    CHECK(!apm_link_slow_flag(false, 500, settled), "空闲态 500 ms 仍不置（只纠偏，不报慢档）");

    // 采集态按 40 ms 判（已过宽限期）。
    CHECK(!apm_link_slow_flag(true, 30, settled), "采集态 30 ms 不置位");
    CHECK(!apm_link_slow_flag(true, 40, settled), "采集态 40 ms 在边界上，不置位");
    CHECK(apm_link_slow_flag(true, 41, settled), "采集态 41 ms 置位");
    CHECK(apm_link_slow_flag(true, 500, settled), "采集态 500 ms 置位（KI-005 那种会话）");

    // 同一个 80 ms 在两种状态下结论相反 —— 这是判据"分档"的直接证据。
    CHECK(!apm_link_slow_flag(false, 80, settled) && apm_link_slow_flag(true, 80, settled),
          "同一个 80 ms：空闲不报慢档、采集报慢档");

    // 未知间隔不下结论。
    CHECK(!apm_link_slow_flag(true, 0, settled), "间隔未知（0）时不置位");
}

static void test_link_slow_grace(void)
{
    printf("[link_slow/grace]\n");

    // **真机回归（2026-11-07）**：刚进采集时实际间隔还是空闲档的 80 ms，
    // 宽限期内不能报慢档，否则整场健康录音会被 PC 判成 software-fail。
    // 现场证据：块率 50.0/s、序号跳变 0、长度异常 0，却被标成 software-fail。
    CHECK(!apm_link_slow_flag(true, 80, 0),
          "刚进采集读到 80 ms（旧档未生效）不置位");
    CHECK(!apm_link_slow_flag(true, 80, 500),
          "宽限期内 500 ms（0.5 s）读到 80 ms 不置位");
    CHECK(!apm_link_slow_flag(true, 80, APM_LINK_SLOW_GRACE_MS - 1),
          "宽限期最后一毫秒仍不置位");
    CHECK(!apm_link_slow_flag(true, 500, 1000),
          "宽限期内即使读到 500 ms 也不置位（宁可晚发现，也不要误伤开场）");

    // 过了宽限期就要如实报。
    CHECK(apm_link_slow_flag(true, 80, APM_LINK_SLOW_GRACE_MS),
          "宽限期一过，80 ms 立即置位");
    CHECK(apm_link_slow_flag(true, 500, APM_LINK_SLOW_GRACE_MS + 1),
          "宽限期之后 500 ms 置位");

    // 宽限期只对采集态有意义；空闲态本来就不报。
    CHECK(!apm_link_slow_flag(false, 500, 0), "空闲态宽限期内也不报");
}

// ---------------------------------------------------------------------------
// 采集态状态机
// ---------------------------------------------------------------------------
static void test_capture_state(void)
{
    printf("[capture]\n");

    apm_capture_t c = { false, false };
    CHECK(!apm_capture_on(&c), "初始不采集");

    apm_capture_ptt(&c, true);
    CHECK(apm_capture_on(&c), "PTT 按住 -> 采集");
    apm_capture_ptt(&c, false);
    CHECK(!apm_capture_on(&c), "PTT 抬起 -> 停止");

    apm_capture_start_cmd(&c);
    CHECK(apm_capture_on(&c), "CTRL START -> 采集");

    // 回归：CTRL STOP 不能顶掉 PTT（KI-005 语义）。
    apm_capture_stop_cmd(&c);
    CHECK(!apm_capture_on(&c), "CTRL STOP -> 停止");

    // 回归：START 置位期间，PTT 抬起不该把 forced 顶掉。
    apm_capture_t d = { false, false };
    apm_capture_start_cmd(&d);
    apm_capture_ptt(&d, true);
    apm_capture_ptt(&d, false);
    CHECK(apm_capture_on(&d),
          "PC 已 START 时 PTT 按下再抬起，仍在采集（抬手不能顶掉 forced）");

    // 回归：PTT 按住期间，CTRL STOP 不该把 ptt 顶掉。
    apm_capture_t e = { false, false };
    apm_capture_ptt(&e, true);
    apm_capture_stop_cmd(&e);
    CHECK(apm_capture_on(&e),
          "PTT 按住时收到 CTRL STOP，仍在采集（STOP 不能代清 ptt）");

    // 断链：两者全清，不等 STOP（KI-005 的解药）。
    apm_capture_t f = { false, false };
    apm_capture_start_cmd(&f);
    apm_capture_ptt(&f, true);
    apm_capture_disconnect(&f);
    CHECK(!apm_capture_on(&f), "断链清掉 forced 与 ptt 两者，不等 STOP");
    CHECK(!f.ptt_held && !f.forced, "断链后两个旗子都是 false");
}

// ---------------------------------------------------------------------------
// 发射功率
// ---------------------------------------------------------------------------
static void test_tx_power_mapping(void)
{
    printf("[tx_power/rssi]\n");

    CHECK(apm_tx_power_for_rssi(-40) == APM_TX_NEAR_DBM, "RSSI -40 -> -3 dBm");
    CHECK(apm_tx_power_for_rssi(-60) == APM_TX_NEAR_DBM, "RSSI -60（边界）-> -3 dBm");
    CHECK(apm_tx_power_for_rssi(-61) == APM_TX_MID_DBM, "RSSI -61 -> +3 dBm");
    CHECK(apm_tx_power_for_rssi(-75) == APM_TX_MID_DBM, "RSSI -75（边界）-> +3 dBm");
    CHECK(apm_tx_power_for_rssi(-76) == APM_TX_FAR_DBM, "RSSI -76 -> +9 dBm");
    CHECK(apm_tx_power_for_rssi(-95) == APM_TX_FAR_DBM, "RSSI -95 -> +9 dBm");

    // 三档必须递增，否则"升功率"的语义会反转。
    CHECK(APM_TX_NEAR_DBM < APM_TX_MID_DBM && APM_TX_MID_DBM < APM_TX_FAR_DBM,
          "三档严格递增：%d < %d < %d",
          APM_TX_NEAR_DBM, APM_TX_MID_DBM, APM_TX_FAR_DBM);
}

static void test_tx_power_init(void)
{
    printf("[tx_power/init]\n");

    apm_tx_state_t st;
    apm_tx_init(&st);
    // 初值必须等于 sdkconfig 的开机默认 +9 dBm，否则第一轮评估会凭空"切换"一次。
    CHECK(st.dbm == APM_TX_FAR_DBM, "初值等于开机默认 +9 dBm（实际 %d）", st.dbm);
    CHECK(st.downgrade_pending == 0, "初值没有挂着的降档请求");
}

static void test_tx_power_upgrade_is_immediate(void)
{
    printf("[tx_power/upgrade]\n");

    apm_tx_state_t st;
    apm_tx_init(&st);

    // 从 +9 进到近距离：**一次**就要降下来吗？不 —— 降档要连续 2 次。
    CHECK(!apm_tx_update(&st, -40), "第一次近距离读数不切换（降档需连续 2 次）");
    CHECK(st.dbm == APM_TX_FAR_DBM, "此时仍是 +9 dBm（实际 %d）", st.dbm);
    CHECK(apm_tx_update(&st, -40), "第二次近距离读数 -> 切换");
    CHECK(st.dbm == APM_TX_NEAR_DBM, "降到 -3 dBm（实际 %d）", st.dbm);

    // 升功率必须**立刻**生效：RSSI 一掉下去就升，不等确认。
    CHECK(apm_tx_update(&st, -80), "RSSI 掉到 -80 立刻升档");
    CHECK(st.dbm == APM_TX_FAR_DBM, "一次就回到 +9 dBm（实际 %d）", st.dbm);
}

static void test_tx_power_hysteresis(void)
{
    printf("[tx_power/hysteresis]\n");

    apm_tx_state_t st;
    apm_tx_init(&st);
    apm_tx_update(&st, -40);   // pending=1
    apm_tx_update(&st, -40);   // 生效 -> -3
    CHECK(st.dbm == APM_TX_NEAR_DBM, "已降到 -3 dBm");

    // 单次抖动不能降档，但**必须清零 pending**：
    // 否则两次相隔很久的抖动会凑成一次降档（这就是滞回的本意）。
    apm_tx_init(&st);                 // 回到 +9
    apm_tx_update(&st, -40);          // pending=1
    apm_tx_update(&st, -40);          // 生效
    CHECK(st.dbm == APM_TX_NEAR_DBM, "先降到 -3");

    apm_tx_update(&st, -40);
    CHECK(st.dbm == APM_TX_NEAR_DBM, "已经在目标档，不再变化");

    // 边界抖动：-59（近档）与 -61（中档）交替，不该产生任何切换。
    apm_tx_init(&st);
    bool switched = false;
    for (int i = 0; i < 10; i++) {
        switched |= apm_tx_update(&st, (i % 2 == 0) ? -59 : -61);
    }
    CHECK(!switched,
          "-59/-61 交替抖动 10 次，一次都没切换（降档需连续 2 次同一方向）");
    CHECK(st.dbm == APM_TX_FAR_DBM, "功率保持 +9 dBm 不变");
}

static void test_tx_power_clears_pending_on_target(void)
{
    printf("[tx_power/pending_reset]\n");

    apm_tx_state_t st;
    apm_tx_init(&st);

    // 场景：一次"想降"之后信号又变好（回到当前档），pending 必须清零。
    // 不清的话，下次再抖一下就会凑够 2 次，凭空降档。
    apm_tx_update(&st, -40);          // pending=1（想降到 -3）
    CHECK(st.downgrade_pending == 1, "挂起 1 次降档请求");
    apm_tx_update(&st, -90);          // 回到 +9 档
    CHECK(st.downgrade_pending == 0, "回到目标档时清零，不残留");

    // 之后单次抖动不该触发降档。
    CHECK(!apm_tx_update(&st, -40), "清零后单次抖动不降档");
    CHECK(st.dbm == APM_TX_FAR_DBM, "仍在 +9 dBm");
}

static void test_tx_power_null_safe(void)
{
    printf("[tx_power/null]\n");

    // 真实调用点会判空，但纯函数自己也不该崩：一次空指针崩溃在固件里是整机重启。
    CHECK(!apm_tx_update(NULL, -40), "apm_tx_update(NULL) 返回 false 不崩");
    apm_tx_init(NULL);   // 不崩即可
    CHECK(1, "apm_tx_init(NULL) 不崩");
}

// ---------------------------------------------------------------------------
int main(void)
{
    printf("== power_policy 单测 ==\n");
    test_conn_itvl_tiers();
    test_conn_itvl_ok();
    test_link_slow();
    test_link_slow_grace();
    test_capture_state();
    test_tx_power_mapping();
    test_tx_power_init();
    test_tx_power_upgrade_is_immediate();
    test_tx_power_hysteresis();
    test_tx_power_clears_pending_on_target();
    test_tx_power_null_safe();

    printf("\n%d passed, %d failed\n", g_pass, g_fail);
    return g_fail == 0 ? 0 : 1;
}
