// device/main/power_policy.c
// 省电决策的纯实现。**不依赖 ESP-IDF**，主机侧可直接编译测试。
// 语义与理由见 power_policy.h 与 docs/ARCHITECTURE.md 的「功耗与省电」节。
#include "power_policy.h"

// ---------------------------------------------------------------------------
// 连接参数档位
// ---------------------------------------------------------------------------
apm_conn_itvl_t apm_conn_itvl_for(bool capturing)
{
    if (capturing) {
        apm_conn_itvl_t r = { APM_CONN_ITVL_MIN_MS, APM_CONN_ITVL_MAX_MS };
        return r;
    }
    apm_conn_itvl_t r = { APM_CONN_ITVL_IDLE_MS, APM_CONN_ITVL_IDLE_MS };
    return r;
}

bool apm_conn_itvl_ok(bool capturing, uint16_t actual_ms)
{
    // 0 = 还没读到实际间隔，不该据此判"不合格"，否则每次刚连上都会白发一次请求。
    if (actual_ms == 0) return true;
    uint16_t limit = capturing ? APM_CONN_ITVL_OK_MS : APM_CONN_ITVL_IDLE_OK_MS;
    return actual_ms <= limit;
}

bool apm_link_slow_flag(bool capturing, uint16_t actual_ms, uint32_t ms_since_capture)
{
    // 空闲态永不置位：这是本函数存在的理由之一。若将来有人想"顺手统一判据"，
    // 后果是每一场空闲都被 PC 标成脏会话（link_slow 在 PC 侧是粘住的，洗不掉）。
    if (!capturing) return false;
    if (actual_ms == 0) return false;   // 还没读到，不下结论

    // 宽限期：刚进入采集时实际间隔还是空闲档的旧值（80 ms），要等主机接受新参数。
    // 这段时间报慢档会让整场会话被判脏 —— 真机踩过（2026-11-07，15 s 零丢块的健康录音
    // 被标成 software-fail）。见 APM_LINK_SLOW_GRACE_MS 的注释。
    if (ms_since_capture < APM_LINK_SLOW_GRACE_MS) return false;

    return actual_ms > APM_CONN_ITVL_OK_MS;
}

// ---------------------------------------------------------------------------
// 采集态
// ---------------------------------------------------------------------------
bool apm_capture_on(const apm_capture_t *c) { return c->forced || c->ptt_held; }

void apm_capture_ptt(apm_capture_t *c, bool held) { c->ptt_held = held; }

void apm_capture_start_cmd(apm_capture_t *c) { c->forced = true; }

void apm_capture_stop_cmd(apm_capture_t *c)
{
    // 只清 forced。把 ptt 一起清掉会让"用户按着开麦键 + PC 发了 STOP"变成
    // 键还按着却不采集，且抬键后也回不来（因为 ptt 已经被人代清）。
    c->forced = false;
}

void apm_capture_disconnect(apm_capture_t *c)
{
    c->forced = false;
    c->ptt_held = false;
}

// ---------------------------------------------------------------------------
// 发射功率
// ---------------------------------------------------------------------------
int8_t apm_tx_power_for_rssi(int8_t rssi)
{
    if (rssi >= -60) return APM_TX_NEAR_DBM;
    if (rssi >= -75) return APM_TX_MID_DBM;
    return APM_TX_FAR_DBM;
}

void apm_tx_init(apm_tx_state_t *st)
{
    if (!st) return;
    // 初值必须等于 sdkconfig 的开机默认（CONFIG_BT_CTRL_DFT_TX_POWER_LEVEL_P9）。
    // 写 0 会让第一轮评估误以为"现在是 0 dBm"，把一次并不存在的切换记进日志。
    st->dbm = APM_TX_INITIAL_DBM;
    st->pending_dbm = APM_TX_INITIAL_DBM;
    st->downgrade_pending = 0;
}

bool apm_tx_update(apm_tx_state_t *st, int8_t rssi)
{
    if (!st) return false;

    int8_t target = apm_tx_power_for_rssi(rssi);

    if (target > st->dbm) {
        // 升功率：立刻生效，不等确认。
        st->dbm = target;
        st->pending_dbm = target;
        st->downgrade_pending = 0;
        return true;
    }

    if (target < st->dbm) {
        // 降档：只累计**同一个目标档**的连续次数。
        //
        // 必须比对 pending_dbm，不能只数次数。否则 -59（想降到 -3）与 -61（想降到 +3）
        // 交替出现时，两次都会记成"想降"，凑够 2 次就莫名其妙降了一档 ——
        // 而这两个读数其实指向**不同**的目标，等于在边界上被抖动推着走。
        // 2026-11-07 的单测就是这么抓到它的。
        if (st->downgrade_pending > 0 && st->pending_dbm == target) {
            st->downgrade_pending++;
        } else {
            st->pending_dbm = target;
            st->downgrade_pending = 1;
        }

        if (st->downgrade_pending >= APM_TX_DOWNGRADE_STEPS) {
            st->dbm = target;
            st->pending_dbm = target;
            st->downgrade_pending = 0;
            return true;
        }
        return false;
    }

    // 已在目标档：清掉"想降"的计数，否则两次相隔很久的抖动会凑成一次降档。
    st->pending_dbm = st->dbm;
    st->downgrade_pending = 0;
    return false;
}
