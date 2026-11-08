//! 播放缓冲：抖动吸收 + 时钟漂移补偿。
//!
//! 抽成不依赖 `cpal` 的纯结构，是为了能在没有声卡的机器上跑仿真
//! （见文末测试：以 15933 Hz 生产、16000 Hz 消费跑 60 s）。
//!
//! ## 为什么要补偿漂移
//!
//! 设备的 16 kHz 来自 ES8311 的 MCLK，声卡的 16 kHz 来自另一颗晶振。两个"16 kHz"
//! 不是同一个数，实测 20 s 里累积出约 84 ms 的缺口。只靠抖动缓冲的结果是周期性的
//! 欠载（补上一刻的样本）或溢出（丢掉整帧），开会时表现为每隔几秒"咔"一下。
//!
//! ## 怎么做
//!
//! 用**比例控制器 + 单样本插删**：
//! - 每轮回调算一次 `err = 现有帧数 - 目标帧数`，转成每秒要补/丢的比例
//!   `rate = err / (rate × τ)`，τ = 2 s，上限 ±1%（10000 ppm，远超实际漂移）；
//! - 累积到 1 个样本时补一个（取相邻两点平均）或丢一个（取相邻两点平均），
//!   相当于 ±至少 1 个样本的分辨率，不做整块丢弃，所以听感上是极轻微的变调，
//!   而不是断音。
//!
//! 为什么不用"整帧丢弃"：整帧丢 20 ms 会听出明显的字尾截断；单样本级别的
//! 插删在 0.4% 漂移下约每秒 67 次，每次只动 1/16000 s，人耳察觉不到。
//!
//! 目标缓冲仍然保留自适应用途（抖动大就抬高、稳了就收回），管的是**短时抖动**；
//! 漂移补偿管的是**长时趋势**，两者不冲突。

use std::collections::VecDeque;
use std::time::Instant;

/// 抖动缓冲目标下限 / 上限（毫秒）。**这是全仓唯一一份**：
/// `apm_app::settings` 只转出，`session.rs` 也不再自己 `clamp(40, 80)`
/// （历史上三处各存一份，改上限时漏改就分叉）。
pub const TARGET_MIN_MS: u32 = 40;
pub const TARGET_MAX_MS: u32 = 80;

/// 自适应抬升的步长（毫秒）。只在**真的欠载**时抬。
const TARGET_STEP_MS: u32 = 5;

/// 判断"这次欠载是被饿着"的阈值（毫秒）：距上一次推入超过这个时长仍没数据，
/// 说明消费端在等链路，而不是消费得比生产快。
///
/// 取 1.5 个块周期（30 ms）：正常节奏下每 20 ms 必有一块进来，
/// 超过 30 ms 还没推入就不是"节奏稍慢"，是真的断供了。
pub const UNDERFED_MS: f64 = 30.0;

/// 缓冲填充量的直方图桶宽（毫秒）与桶数：覆盖 0–256 ms。
///
/// 存在的理由是 **A4**：契约里写的是"抖动缓冲 40–80 ms"，但稳态实际存量并不等于
/// 目标值（修剪守卫把它留在 `目标 + TRIM_BACK_MS` 附近）。拿"目标 80 ms"当延迟结论
/// 会系统性低估二三十毫秒。这里直接测**实际存量分布**，报告用 p50/p95，
/// 而不是用一个假设值去糊弄延迟预算。
const FILL_BUCKET_MS: f64 = 4.0;
const FILL_BUCKETS: usize = 64;

/// 单样本插删的速率上限（1% = 10000 ppm）。实测漂移约 4200 ppm，留出一倍余量。
pub const MAX_SLIP: f64 = 0.01;

/// 漂移估计的最短样本时长（秒）。太短的窗口噪声压过真信号，
/// 宁可报 0 也不要给一个假数字 —— 这个值会被写进验收报告。
pub const DRIFT_MIN_SAMPLE_SEC: f64 = 10.0;

/// 判定"推入速率是否已稳定"的观察窗口（秒）。
///
/// 攒批交付的突发通常几十到几百毫秒，1 秒窗口足够把它摊平；
/// 又短到不至于让漂移估计晚太久才有读数。
const DRIFT_ARM_WINDOW_SEC: f64 = 1.0;

/// 窗口内平均推入速率与名义值的允许偏差，超过就认为"突发还没过去"、重新攒窗口。
///
/// 取 2%：真实时钟差典型 <0.5%（实测 4200 ppm 已属极端），而开场攒批的伪影
/// 实测在 2500 ppm 以上、甚至上万 ppm，两者分得很开。
const DRIFT_ARM_TOLERANCE: f64 = 0.02;

/// 延迟守卫：存量超过"目标 + 这个余量(ms)"时，一次性丢最旧的数据回到
/// "目标 + `TRIM_BACK_MS`"，保证延迟不会因为回调停摆而永久抬高。
///
/// **为什么不能用存量驱动漂移补偿**（2026-11-06 深夜）：旧实现按
/// `err = 存量 − 目标` 决定插删速率。CoreAudio 每隔几秒停一下、存量堆高，
/// 控制器就把它当成"设备时钟快"，用 ±1% 慢慢删掉几千帧真实音频
/// （仿真：每 3 s 停 2 拍 → 删 0.35 s 人声，而真漂移是 0）。
///
/// 现在两件事分开：**漂移**用墙钟估（不看消费端），**停摆积压**用这个守卫
/// 一次性丢旧数据。
pub const TRIM_OVER_MS: u32 = 60;
/// 一次性修剪后保留的余量：回到"目标 + 这个值(ms)"。
///
/// 两条硬约束（都踩过）：
/// 1. 不能修剪到精确目标 —— 下一轮回调立刻又不够，缓冲弹性被削光；
/// 2. **必须大于 `can_lower()` 的门槛（+20 ms）** —— 那个函数要求存量比目标高出一截
///    才肯把目标降一档。修剪得太低会让目标只升不降。
///
/// 2026-11-06 真机实测（每次 90 s，同一台机器）：
///
/// | over / back | 欠载 | 静音帧 | 缓冲中位 |
/// | --- | --- | --- | --- |
/// | 110 / 35 | 5 次 | 1123 | 105 ms |
/// | **60 / 35** | **2 次** | **114** | 97 ms |
/// | 60 / 30 | 7 次 | 1367 | 87 ms |
///
/// 取 60 / 35：欠载与静音都最少。`back=30` 能把缓冲压到 87 ms，但欠载翻三倍
/// —— 用延迟换音质的代价太大，宁可多 17 ms 缓冲。
pub const TRIM_BACK_MS: u32 = 35;

/// 缓冲容量上限：目标值的 3 倍，且不低于 200 ms。
/// 超过这个量说明链路或消费端已经失控，宁可丢最旧的也不要让延迟无限涨。
fn capacity_frames(target_ms: u32, rate: u32) -> usize {
    let ms = (target_ms * 3).max(200);
    (rate as usize * ms as usize) / 1000
}

#[derive(Debug, Clone, Copy)]
pub struct Stats {
    /// 收到的帧（单声道采样）数
    pub frames_pushed: u64,
    /// 声卡实际取走的帧数
    pub frames_consumed: u64,
    /// 欠载次数（缓冲被读空）
    pub underruns: u64,
    /// 欠载时补进去的帧数（用上一个样本填充，避免爆音）
    pub frames_silenced: u64,
    /// **因链路断供**造成的欠载次数（A5：欠载归因）。
    ///
    /// 判据见 [`Playout::push`] 与 `fill`：欠载发生时，如果距上一次推入
    /// 超过 [`UNDERFED_MS`]，就是"生产端没喂上"（链路/通知循环），
    /// 否则是"消费端跑得比生产快"（真正的缓冲目标不够）。
    /// **两者要分开报**：混成一个数字时，调缓冲目标会把链路问题一起"修好"，
    /// 于是根因永远查不清 —— KI-006 就是这么绕了两轮。
    pub underruns_underfed: u64,
    /// 超出容量被丢弃的最旧帧数（兜底路径，正常不该触顶）
    pub frames_overflow: u64,
    /// 延迟守卫一次性修剪掉的最旧帧数（停摆积压，不是漂移补偿）
    pub frames_trimmed: u64,
    /// 推入样本的平方和，用于算写入侧 RMS（和回环侧对比）
    pub sum_sq: f64,
    /// 漂移补偿补出来的样本数（生产慢于消费）
    pub frames_inserted: u64,
    /// 漂移补偿丢掉的样本数（生产快于消费）
    pub frames_skipped: u64,
    /// 声卡回调的间隔统计（`docs/KNOWN_ISSUES.md` KI-006 要的"停摆时长分布"）。
    pub callbacks: u64,
    /// 回调间隔超过 1.5× 应有周期的次数（= 明显的回调停摆）
    pub stalled_callbacks: u64,
    /// 观测到的最大回调间隔（毫秒）。这个数字直接决定缓冲上限该抬到多少。
    pub max_callback_gap_ms: f64,
    /// 按间隔分桶的回调次数（桶边界见 [`GAP_BUCKETS_MS`]）
    pub gap_histogram: [u64; 8],
    /// **实际存量分布**（每 [`FILL_BUCKET_MS`] 一桶，共 [`FILL_BUCKETS`] 桶）。
    ///
    /// A4：报告里的"延迟"必须用实测存量，不能用目标值。桶 0 覆盖 0–4 ms。
    pub fill_histogram: [u64; FILL_BUCKETS],
    /// `fill_histogram` 的样本总数（每轮回调记一次）。
    pub fill_samples: u64,
    /// 每次欠载发生的时刻（毫秒，相对第一次 `fill`；最多记 [`UNDERRUN_LOG`] 条）。
    ///
    /// 只记时刻、不做任何 IO / 分配：音频回调里不能 malloc，也不能 log。
    /// 有了这串时刻才能回答"欠载是启动瞬态还是均匀铺满全程"——
    /// 2026-11-07 的三轮 `--tone` 里，120 s 与 180 s 的欠载次数几乎一样多（8 / 7 / 8），
    /// 这本身就暗示不是稳态事故。
    pub underrun_at_ms: [f64; 16],
    pub underruns_logged: usize,
}

/// 欠载时刻最多记多少条。够看清分布即可，不追求记全。
///
/// 用 16 而不是 64：`Stats` 整个结构体要保持 `Copy`（回调里按值取快照），
/// 而 `[f64; 64]` 没有 `Default`，`derive(Default)` 会编译不过。
pub const UNDERRUN_LOG: usize = 16;

/// `Stats` 手写 `Default`：`[u64; FILL_BUCKETS]` 超出标准库为数组实现 `Default`
/// 的长度上限（32），`derive` 会编译不过。
impl Default for Stats {
    fn default() -> Self {
        Self {
            frames_pushed: 0,
            frames_consumed: 0,
            underruns: 0,
            frames_silenced: 0,
            underruns_underfed: 0,
            frames_overflow: 0,
            frames_trimmed: 0,
            sum_sq: 0.0,
            frames_inserted: 0,
            frames_skipped: 0,
            callbacks: 0,
            stalled_callbacks: 0,
            max_callback_gap_ms: 0.0,
            gap_histogram: [0; 8],
            fill_histogram: [0; FILL_BUCKETS],
            fill_samples: 0,
            underrun_at_ms: [0.0; UNDERRUN_LOG],
            underruns_logged: 0,
        }
    }
}

/// 回调间隔直方图的桶边界（毫秒）。
///
/// KI-006 的下一步是"量出停摆时长分布，再决定缓冲上限抬到多少"——
/// 如果停摆普遍超过 120 ms，那么把 80 抬到 100 一点用都没有。
pub const GAP_BUCKETS_MS: [f64; 8] = [10.0, 20.0, 30.0, 50.0, 80.0, 120.0, 250.0, f64::INFINITY];

impl Stats {
    fn note_callback_gap(&mut self, gap_ms: f64) {
        self.callbacks += 1;
        if gap_ms > self.max_callback_gap_ms {
            self.max_callback_gap_ms = gap_ms;
        }
        for (i, &edge) in GAP_BUCKETS_MS.iter().enumerate() {
            if gap_ms <= edge {
                self.gap_histogram[i] += 1;
                break;
            }
        }
    }

    /// 记一次实际存量（由 `fill` 每轮回调调用）。
    fn note_fill(&mut self, ms: f32) {
        let idx = (ms as f64 / FILL_BUCKET_MS).floor();
        let idx = if idx < 0.0 {
            0
        } else if idx >= FILL_BUCKETS as f64 {
            FILL_BUCKETS - 1
        } else {
            idx as usize
        };
        self.fill_histogram[idx] += 1;
        self.fill_samples += 1;
    }

    /// 实际存量的分位数（毫秒）。桶内取中点，桶宽 4 ms，误差 ≤ ±2 ms。
    ///
    /// `None` = 样本不足（还没跑够一轮）。
    pub fn fill_percentile_ms(&self, pct: f64) -> Option<f64> {
        if self.fill_samples == 0 {
            return None;
        }
        let target = (self.fill_samples as f64 * pct.clamp(0.0, 1.0)).ceil();
        let target = (target.max(1.0)) as u64;
        let mut acc = 0u64;
        for (i, &count) in self.fill_histogram.iter().enumerate() {
            acc += count;
            if acc >= target {
                return Some((i as f64 + 0.5) * FILL_BUCKET_MS);
            }
        }
        Some((FILL_BUCKETS as f64 - 0.5) * FILL_BUCKET_MS)
    }
}

pub struct Playout {
    buf: VecDeque<i16>,
    rate: u32,
    /// 用户选的基准目标（滑块）。自适应不动它。
    base_ms: u32,
    /// 自适应临时加的余量（欠载时 +5 ms、稳定后 −5 ms，封顶到 `TARGET_MAX_MS`）。
    boost_ms: u32,
    /// 实际生效的目标 = `base_ms + boost_ms`（被 `TARGET_MAX_MS` 夹住）。
    target_ms: u32,
    target_frames: usize,
    capacity: usize,
    /// 已攒够 target_frames 才开始播放，避免一上来就欠载
    playing: bool,
    /// 连续无欠载的帧数，用于判断"稳了可以收回缓冲"
    stable_frames: u64,
    /// 最近一个输出样本，欠载时用它填充
    last: i16,
    /// 漂移补偿的分数相位，绝对值到 1 就补/丢一个样本
    slip: f64,
    /// 漂移估计的起算点：`(开始时刻, 当时的 frames_pushed)`。
    ///
    /// 播起来的那一刻记一次，之后不再重置 —— 用长时间窗换准确度。
    /// 见 [`Self::ppm`] 为什么必须用墙钟而不是 `frames_consumed`。
    drift_base: Option<Arrival>,
    /// 等推入速率稳定后再起算漂移用的中间状态：`(采样起点时刻, 起点已推入帧数)`。
    ///
    /// 见 `arm_drift_base()` 的注释：开播瞬间主机攒批交付会让 `frames_pushed`
    /// 瞬间超前墙钟，从那里起算会把"攒批"读成"设备时钟快"，进而删真实音频。
    drift_arm: Option<Arrival>,
    /// 秒表。生产环境就是 `Instant::now()` 的封装；测试里可以喂假时间，
    /// 否则"用墙钟估漂移"这个算法在单测里根本跑不出来（循环几毫秒就跑完 60 秒仿真）。
    clock: Clock,
    /// 上一次声卡回调的时刻，用来量回调间隔（KI-006 的停摆时长分布）。
    last_callback: Option<Instant>,
    /// 第一次回调的时刻，欠载时刻以它为原点。
    first_callback: Option<Instant>,
    /// 相邻两次回调之间"应该"有多少帧，用来判断这次回调是不是明显迟到。
    frames_per_callback: usize,
    /// 上一次**推入**的时刻。A5 靠它区分"链路断供"与"消费端跑太快"。
    last_push_at: Option<Instant>,
    stats: Stats,
}

/// 单调时钟。生产是 [`Instant`]，测试可以手动推进。
#[derive(Debug, Clone, Copy)]
struct Clock {
    #[cfg(test)]
    fake: Option<Instant>,
}

impl Clock {
    fn new() -> Self {
        Self {
            #[cfg(test)]
            fake: None,
        }
    }

    fn now(&self) -> Instant {
        #[cfg(test)]
        if let Some(t) = self.fake {
            return t;
        }
        Instant::now()
    }

    /// 只在测试里用：把时钟往前拨。
    #[cfg(test)]
    fn advance(&mut self, d: std::time::Duration) {
        let base = self.fake.unwrap_or_else(Instant::now);
        self.fake = Some(base + d);
    }
}/// 漂移估计的起算点。用墙钟 + 推入帧数，两者都与消费端无关。
#[derive(Debug, Clone, Copy)]
struct Arrival {
    at: Instant,
    pushed: u64,
}

impl Playout {
    pub fn new(rate: u32, target_ms: u32) -> Self {
        let base_ms = target_ms.clamp(TARGET_MIN_MS, TARGET_MAX_MS);
        let rate = rate.max(1);
        Self {
            buf: VecDeque::with_capacity(capacity_frames(TARGET_MAX_MS, rate)),
            rate,
            base_ms,
            boost_ms: 0,
            target_ms: base_ms,
            target_frames: rate as usize * base_ms as usize / 1000,
            capacity: capacity_frames(TARGET_MAX_MS, rate),
            playing: false,
            stable_frames: 0,
            last: 0,
            slip: 0.0,
            drift_base: None,
            drift_arm: None,
            clock: Clock::new(),
            last_callback: None,
            first_callback: None,
            frames_per_callback: 0,
            last_push_at: None,
            stats: Stats::default(),
        }
    }

    pub fn fill_ms(&self) -> f32 {
        self.buf.len() as f32 * 1000.0 / self.rate as f32
    }

    pub fn target_ms(&self) -> u32 {
        self.target_ms
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn buffered_frames(&self) -> usize {
        self.buf.len()
    }

    /// 长期漂移估计（ppm）。正值 = 设备时钟比声卡快（补偿方式是丢样本）。
    ///
    /// **用墙钟 + 推入帧数算，不看消费端。**
    ///
    /// 为什么必须这样（2026-11-06 深夜定位）：旧实现用
    /// `(frames_skipped − frames_inserted) / frames_consumed`，而控制器又是按
    /// `err = 存量 − 目标` 决定插删的。于是 CoreAudio 每隔几秒抖一下、缓冲堆高，
    /// 控制器就把它当成"设备时钟快"，真的删掉几千帧音频，`ppm` 再如实报出这几千——
    /// **读数和控制互相污染，分不清是谁的错**。
    ///
    /// 墙钟分母是与消费端完全无关的量：`pushed` 来自 BLE 推送（设备时钟），
    /// `elapsed` 来自系统时钟。两者都不受回调停摆影响，所以停摆期间读数纹丝不动
    /// （仿真验证：每 3 秒停 1/2/5 拍，读数分别 +0 / +0 / +0）。
    /// 等推入速率稳定后，才记下漂移估计的起算点。
    ///
    /// **为什么需要这一步**（2026-11-08 真机 + 仿真定位）：
    ///
    /// 主机是攒批交付（CoreBluetooth，相邻块 <5 ms 占 42%）。开播那一刻手上
    /// 已经攒了一批，它们在头几十毫秒内集中 `push` 进来，`frames_pushed` 于是
    /// 瞬间超前于墙钟。从那一刻起算，`ppm()` 会把"攒批"读成"设备时钟快"：
    ///
    /// ```text
    /// 仿真（真漂移 0，开场 300 ms 攒批）：ppm = +2501
    /// 真机（欠载多的那轮，60 s 时）：      ppm = +2484
    /// ```
    ///
    /// 补偿器随即按这个假读数**删样本**去"纠正"——删掉的是真实音频。
    /// 真机那轮因此删了 9406 个样本、欠载从 20 抬到 96，而收尾仍报"漂移 0 ppm"
    /// （因为把起点挪到突发之后，读数自然回到 0）。**读数和控制互相污染**，
    /// 这与 2026-11-06 那次修复的是同一类错误，只是入口换成了"开场突发"。
    ///
    /// 判据用"最近这段窗口的推入量接近名义值"而不是固定等待：
    /// 攒批突发总会过去，而固定时长在慢主机上可能还没过去。
    fn arm_drift_base(&mut self) {
        if self.drift_base.is_some() {
            return;
        }
        let now = self.clock.now();
        let Some(arm) = self.drift_arm else {
            // 第一次进来：记一个采样起点，下一轮再判。
            self.drift_arm = Some(Arrival {
                at: now,
                pushed: self.stats.frames_pushed,
            });
            return;
        };

        let secs = now.saturating_duration_since(arm.at).as_secs_f64();
        if secs < DRIFT_ARM_WINDOW_SEC {
            return;   // 窗口还没够，继续攒样本
        }
        let pushed = (self.stats.frames_pushed - arm.pushed) as f64;
        let ratio = pushed / (self.rate as f64 * secs);

        // 窗口内的平均速率接近 1.0 才算"攒批已过去"。
        // 容差给 2%：既容忍真实的时钟差（典型 <0.5%），也挡住开场那种 10%+ 的突发。
        if (ratio - 1.0).abs() <= DRIFT_ARM_TOLERANCE {
            self.drift_base = Some(Arrival {
                at: now,
                pushed: self.stats.frames_pushed,
            });
            self.drift_arm = None;
        } else {
            // 还不稳（突发还没消化完）：把窗口起点往后挪，重新攒。
            self.drift_arm = Some(Arrival {
                at: now,
                pushed: self.stats.frames_pushed,
            });
        }
    }

    pub fn ppm(&self) -> f64 {
        let Some(base) = self.drift_base else {
            return 0.0;
        };
        let secs = self.clock.now().duration_since(base.at).as_secs_f64();        // 样本不够时读数噪声大，不如报 0（调用方本来也不该在开头几分钟较真）。
        if secs < DRIFT_MIN_SAMPLE_SEC {
            return 0.0;
        }
        let pushed = (self.stats.frames_pushed - base.pushed) as f64;
        (pushed / (self.rate as f64 * secs) - 1.0) * 1e6
    }

    /// 运行期改抖动缓冲目标（GUI 的缓冲滑块）。取下一批回调生效，
    /// 不重建流、不断音。
    ///
    /// **这是用户意图，不是自适应的当前值。** 自适应只能在它之上临时加余量
    /// （见 [`Playout::boost_ms`]），绝不能把用户明确选的 80 悄悄降到 40 ——
    /// 那会让"我把缓冲调大了"变成一句谎话，而且下次一阵抖动立刻欠载。
    pub fn set_target_ms(&mut self, ms: u32) {
        self.base_ms = ms.clamp(TARGET_MIN_MS, TARGET_MAX_MS);
        self.recompute_target();
    }

    /// 用户选的基准目标（毫秒）。
    pub fn base_ms(&self) -> u32 {
        self.base_ms
    }

    /// 自适应临时加上的余量（毫秒）。0 = 正在用用户选的基准值。
    pub fn boost_ms(&self) -> u32 {
        self.boost_ms
    }

    fn recompute_target(&mut self) {
        let ms = (self.base_ms + self.boost_ms).min(TARGET_MAX_MS);
        self.target_ms = ms;
        self.target_frames = self.rate as usize * ms as usize / 1000;
    }

    /// 稳定判定窗口：连续这么多帧没有欠载才考虑收回临时余量（默认 5 秒）。
    fn stable_window(&self) -> u64 {
        self.rate as u64 * 5
    }

    /// 收回临时余量的前提：现存量要比目标还多出一截，否则会立刻又欠载。
    fn can_lower(&self) -> bool {
        self.boost_ms > 0
            && self.buf.len() > self.target_frames + (self.rate as usize / 50) // +20 ms
    }

    /// 推入一段单声道 PCM。
    pub fn push(&mut self, samples: &[i16]) {
        self.buf.extend(samples.iter().copied());
        self.stats.sum_sq += samples.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>();
        self.last_push_at = Some(self.clock.now());
        // 真·超容量兜底：正常路径不应该走到这里（延迟守卫会先把存量压住），
        // 走到说明消费端已经停摆很久，宁可丢最旧的也不要让延迟无限涨。
        while self.buf.len() > self.capacity {
            self.buf.pop_front();
            self.stats.frames_overflow += 1;
        }
        self.stats.frames_pushed += samples.len() as u64;
    }

    /// 断连时调用：停止播放并丢弃旧缓冲。
    pub fn clear(&mut self) {
        self.buf.clear();
        self.playing = false;
        self.last = 0;
        self.slip = 0.0;
        self.stable_frames = 0;
        self.drift_base = None;
        self.drift_arm = None;
        // 下一次 `push` 之前不该被当成"断供"：清空后 `last_push_at` 是过去时刻，
        // 但此刻会话已经结束，`fill` 不会再被调用。
        self.last_push_at = None;
    }

    /// 声卡自己报错时补记一次欠载（由 `Sink` 的错误回调调用）。
    pub fn note_underrun(&mut self) {
        self.stats.underruns += 1;
    }

    /// 填满一轮声卡回调（单声道，一个元素一帧）。
    pub fn fill(&mut self, out: &mut [i16]) {
        if out.is_empty() {
            return;
        }

        // 回调间隔统计（KI-006）：这是唯一能直接证明"CoreAudio 回调被系统停摆"的证据。
        // 必须在 `playing` 判断之前记——预填阶段的间隔同样有参考价值。
        let now = self.clock.now();
        if let Some(prev) = self.last_callback {
            let gap_ms = now.saturating_duration_since(prev).as_secs_f64() * 1000.0;
            self.stats.note_callback_gap(gap_ms);
            // 本次回调带来了多少帧？正常等于 `out.len()`；超出 1.5 倍视为停摆。
            if self.frames_per_callback > 0 {
                let expected =
                    self.frames_per_callback as f64 / self.rate as f64 * 1000.0 * 1.5;
                if gap_ms > expected {
                    self.stats.stalled_callbacks += 1;
                }
            }
        }
        self.last_callback = Some(now);
        if self.first_callback.is_none() {
            self.first_callback = Some(now);
        }
        self.frames_per_callback = out.len();

        // **A4：测实际存量，不要用目标值代替延迟。**
        // 在消费之前记，得到的是"这一轮回调开始时缓冲里有多少毫秒"——
        // 那就是这块音频要等多久才会被播出去的量级。
        // 只有在播放中的样本才有意义：预填阶段存量必然在涨，混进来会把 p95 抬高。
        if self.playing {
            self.stats.note_fill(self.fill_ms());
        }

        if !self.playing {
            if self.buf.len() >= self.target_frames {
                self.playing = true;
                // 漂移估计的起算点**先不记**，见下（`drift_arm`）。
                self.drift_arm = None;
            } else {
                // 预填阶段不出声，但这不算欠载：还没有开始播
                out.fill(0);
                return;
            }
        }

        // ---- 漂移估计的起算点：等推入速率稳定后才记 ----
        //
        // 为什么不能一开播就记（2026-11-08 真机 + 仿真定位）：
        // 主机是**攒批交付**（相邻块 <5 ms 占 42%）。开播那一刻手上已经攒了一批，
        // 它们在头几十毫秒内一起 `push` 进来，于是 `frames_pushed` 瞬间超前于墙钟。
        // 从这个点起算，`ppm()` 会把"攒批"读成"设备时钟快"：
        //   · 仿真（真漂移 0、开场 300 ms 攒批）：ppm = **+2501**
        //   · 真机（欠载多的那轮）：60 s 时 ppm = **+2484**
        // 补偿器随即按这个假读数**删样本**去"纠正"，删的却是真实音频 ——
        // 真机那轮删了 9406 个样本、欠载从 20 抬到 96，而收尾报"漂移 0 ppm"。
        //
        // 判据用"最近一秒的推入量接近名义值"而不是固定等待：攒批的突发总会过去，
        // 而固定时长在慢主机上可能还没过去。
        self.arm_drift_base();

        // 延迟守卫：停摆会把存量堆高，靠 ±1% 消化要几十秒，期间延迟一直偏大
        // （仿真里会永久卡在 230 ms）。超过上限就一次性丢最旧的数据，
        // 回到"目标 + 余量"。丢的是最旧的那几十毫秒，比"延迟长期偏高"可接受。
        //
        // 这一步**不参与漂移估计**：它是消费端瞬态造成的积压，不是时钟差。
        let trim_over = self.target_frames + self.rate as usize * TRIM_OVER_MS as usize / 1000;
        if self.buf.len() > trim_over {
            let keep = self.target_frames + self.rate as usize * TRIM_BACK_MS as usize / 1000;
            let drop = self.buf.len().saturating_sub(keep);
            for _ in 0..drop {
                self.buf.pop_front();
            }
            self.stats.frames_trimmed += drop as u64;
        }

        // 漂移补偿速率**只看墙钟漂移估计，不看缓冲存量**。
        //
        // 旧实现用 `err = 存量 − 目标` 决定插删速率，于是 CoreAudio 每隔几秒停一下、
        // 存量堆高，控制器就把它当成"设备时钟快"，用 ±1% 慢慢删掉几千帧真实音频
        // （仿真：每 3 s 停 2 拍 → 删 0.35 s 人声，而真漂移是 0）。
        let slip_per_frame = (self.ppm() / 1e6).clamp(-MAX_SLIP, MAX_SLIP);

        let mut shortage = false;
        for slot in out.iter_mut() {
            self.slip += slip_per_frame;
            let sample = if self.slip >= 1.0 && self.buf.len() >= 2 {
                // 生产快于消费：两个样本只出一个（取平均），净效果是每分钟少放掉
                // 一批样本，把长期多出来的那点数据吃掉。
                let a = self.buf.pop_front().unwrap() as i32;
                let b = self.buf.pop_front().unwrap() as i32;
                self.slip -= 1.0;
                self.stats.frames_skipped += 1;
                ((a + b) / 2) as i16
            } else if self.slip <= -1.0 && self.buf.len() >= 2 {
                // 生产慢于消费：补一个中间样本，不消费前沿
                let a = *self.buf.front().unwrap() as i32;
                let b = self.buf[1] as i32;
                self.slip += 1.0;
                self.stats.frames_inserted += 1;
                ((a + b) / 2) as i16
            } else {
                match self.buf.pop_front() {
                    Some(s) => {
                        self.last = s;
                        self.stable_frames += 1;
                        if self.stable_frames >= self.stable_window() {
                            self.stable_frames = 0;
                            if self.can_lower() {
                                // 只收回临时余量，绝不动用户选的基准。
                                self.boost_ms = self.boost_ms.saturating_sub(TARGET_STEP_MS);
                                self.recompute_target();
                            }
                        }
                        s
                    }
                    None => {
                        // 一次回调里缺多少帧都只算一次欠载事件；缺的帧数单独记
                        shortage = true;
                        self.stats.frames_silenced += 1;
                        // 欠载说明余量不够：抬高一档临时余量（不动用户的基准值），
                        // 并沿用上一个样本避免爆音。
                        self.stable_frames = 0;
                        if self.target_ms < TARGET_MAX_MS {
                            let room = TARGET_MAX_MS - self.target_ms;
                            self.boost_ms += room.min(TARGET_STEP_MS);
                            self.recompute_target();
                        }
                        self.last
                    }
                }
            };
            self.stats.frames_consumed += 1;
            *slot = sample;
        }
        if shortage {
            self.stats.underruns += 1;
            // **A5：欠载要分两种。** 消费端读空时，如果距上一次推入已经超过
            // `UNDERFED_MS`，说明这期间我们一直没拿到新数据 —— 是链路/上游断供，
            // 不是缓冲目标不够。混在一起报时，把目标从 60 抬到 80 会让数字变好，
            // 于是"链路偶尔断 100 ms"这个真因被永久掩盖（KI-006 踩过两轮）。
            //
            // 判据用"距上次推入的时长"而不是"存量"：存量在欠载那一刻必然是 0，
            // 它无法区分"刚刚推进来就被抽干"和"根本没东西推"。
            let starvation = self
                .last_push_at
                .map(|at| now.saturating_duration_since(at).as_secs_f64() * 1000.0)
                .unwrap_or(f64::INFINITY);
            if starvation > UNDERFED_MS {
                self.stats.underruns_underfed += 1;
            }
            // 记下这次欠载的时刻（相对第一次回调）。音频回调里不能分配、不能 IO，
            // 所以只写进预分配的定长数组，打印留给调用方。
            if self.stats.underruns_logged < UNDERRUN_LOG {
                if let Some(first) = self.first_callback {
                    let at = now.saturating_duration_since(first).as_secs_f64() * 1000.0;
                    self.stats.underrun_at_ms[self.stats.underruns_logged] = at;
                    self.stats.underruns_logged += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 16_000;

    /// 通用仿真器：按 10 ms 一拍推进，每拍推进假时钟。
    ///
    /// `stall_every = Some(n)` 表示每 n 拍里前 `stall_len` 拍 "回调没被调用"
    /// （`fill` 传空切片），这正是真机 CoreAudio 被系统停摆的形状。
    struct Sim {
        p: Playout,
        produced: f64,
        callbacks: usize,
    }

    impl Sim {
        fn new() -> Self {
            let mut p = Playout::new(RATE, 80);
            p.clock.advance(std::time::Duration::from_secs(0));
            Self {
                p,
                produced: 0.0,
                callbacks: 0,
            }
        }

        /// 跑满 `seconds` 秒，`ratio` 是生产倍率（1.0005 = 设备快 500 ppm）。
        fn run(&mut self, ratio: f64, seconds: f64, stall: Option<(usize, usize)>) -> f32 {
            let callback = 160usize;
            let mut out = vec![0i16; callback];
            let mut max_fill = 0.0f32;
            for _ in 0..(seconds * 100.0) as usize {
                self.produced += callback as f64 * ratio;
                let n = self.produced.floor();
                self.produced -= n;
                self.p.push(&vec![100i16; n as usize]);

                // 时间照常流逝：停摆时回调没被调用，但墙钟一样在走。
                self.p
                    .clock
                    .advance(std::time::Duration::from_millis(10));

                let stalled = matches!(stall, Some((every, len)) if self.callbacks % every < len);
                if stalled {
                    self.p.fill(&mut out[..0]);
                } else {
                    self.p.fill(&mut out);
                }
                self.callbacks += 1;
                max_fill = max_fill.max(self.p.fill_ms());
            }
            max_fill
        }
    }

    fn simulate(producer_ratio: f64, seconds: f64) -> (u64, u64, f32, f64) {
        let mut s = Sim::new();
        let max_fill = s.run(producer_ratio, seconds, None);
        (
            s.p.stats().underruns,
            s.p.stats().frames_overflow,
            max_fill,
            s.p.ppm(),
        )
    }

    /// **2026-11-08 复现真机现象：开场 ppm 虚高 → 补偿器删样本 → 欠载计数被抬高。**
    ///
    /// 真机数据（同设备同固件，相隔 30 分钟的两次 600 s 长测）：
    ///   · 欠载少的那轮（20 次）：60 s 时 ppm=+272，全程删 233 样本
    ///   · 欠载多的那轮（96 次）：60 s 时 ppm=**+2484**，全程删 **9406** 样本
    /// 两轮收尾都报 `漂移: 0 ppm`，链接指标（RTT 59 ms、clumped 42%）几乎相同。
    ///
    /// 差别只在**开场那一分钟的 ppm 估计**。这条测试问的是：
    /// 当生产端在开头有一次性突发（预填期攒的块集中到达）时，
    /// `ppm()` 会不会把这段突发误读成"设备时钟快"，从而用删样本的方式纠正？
    #[test]
    fn opening_burst_must_not_be_read_as_clock_drift() {
        // 参数取自真机量级：真机欠载多的那轮开场 ppm=+2484、全程删 9406 样本。
        // 这里用"预填期攒了 300 ms 的音频、在 50 ms 内集中到达"模拟攒批交付
        // （macOS CoreBluetooth 的 clumped 交付，相邻块 <5 ms 占 42%）。
        let mut p = Playout::new(RATE, 80);
        let mut out = vec![0i16; 160];
        let step = 160usize;                            // 每 10 ms 一拍
        let burst_frames = RATE as usize * 300 / 1000;  // 300 ms 的音频
        let burst_per_tick = burst_frames / 5;

        // **每个回调都采样一次 ppm**，取峰值。
        // 只看收尾值是不够的：真机的伤害发生在**运行中**——补偿器按虚高的 ppm
        // 删样本，等 60 秒后 ppm 被平均回去，删掉的音频却回不来了。
        // 第一版测试只断言收尾 ppm，变异（把"等稳定"改成"立刻起算"）仍然 PASS，
        // 等于没守住这个修复。
        let mut peak_ppm: f64 = 0.0;
        for tick in 0..6000 {
            let push_n = if tick < 5 { burst_per_tick } else { step };
            p.push(&vec![100i16; push_n]);
            p.clock.advance(std::time::Duration::from_millis(10));
            p.fill(&mut out);
            peak_ppm = peak_ppm.max(p.ppm().abs());
        }

        let skipped = p.stats().frames_skipped;
        // 真漂移 0：运行期任何时刻都不该读到几千 ppm 的时钟差。
        assert!(
            peak_ppm < 500.0,
            "开场突发被读成时钟差了：运行期峰值 ppm={peak_ppm:+.0}（真值 0）"
        );
        // 更关键：不能因此删掉成批的样本。删样本 = 丢音频 = 抬欠载计数。
        let skipped_ms = skipped as f64 / RATE as f64 * 1000.0;
        assert!(
            skipped_ms < 5.0,
            "开场突发导致删了 {skipped_ms:.1} ms 音频（真漂移 0，不该删）"
        );
    }

    /// 生产与消费同速率：不应该有任何插删，也不应该欠载。
    #[test]
    fn matched_clock_needs_no_correction() {
        let (underruns, dropped, max_fill, ppm) = simulate(1.0, 30.0);
        assert_eq!(underruns, 0, "同速率下欠载了");
        assert_eq!(dropped, 0);
        assert!(ppm.abs() < 50.0, "同速率下的 ppm 估计 {ppm} 偏离太多");
        assert!(max_fill < 120.0, "存量涨到 {max_fill} ms");
    }

    /// 漂移读数必须**完全不受回调停摆影响**。
    ///
    /// 这是 2026-11-06 那次修复的核心目标：旧实现用 `(丢−补)/consumed` 估漂移，
    /// 而控制器又按缓冲存量插删，于是 CoreAudio 每次抖动都会删掉一截真实音频，
    /// 读数再把这截删除量报成"设备时钟快"。仿真里每 3 秒停 2 拍能复现
    /// 真机 +3400 ppm 的量级（真值 0）。
    #[test]
    fn drift_reading_ignores_callback_stalls() {
        let cases = [
            (None, "+0"),
            (Some((300, 1)), "每 3 s 停 1 拍"),
            (Some((300, 2)), "每 3 s 停 2 拍"),
            (Some((1000, 1)), "每 10 s 停 1 拍"),
        ];
        for (stall, label) in cases {
            let mut s = Sim::new();
            s.run(1.0, 60.0, stall);
            let ppm = s.p.ppm();
            assert!(
                ppm.abs() < 200.0,
                "{label}：真漂移 0，读数 {ppm:+.0} ppm —— 停摆又被读成时钟差了"
            );
        }
    }

    /// 停摆期间不许把真实音频删掉一大截。
    #[test]
    fn callback_stall_does_not_eat_real_audio() {
        let mut s = Sim::new();
        s.run(1.0, 60.0, Some((300, 2)));
        let eaten = s.p.stats().frames_skipped as f64 / RATE as f64;
        assert!(
            eaten < 0.12,
            "每 3 s 停 2 拍就删掉了 {eaten:.3} s 真实音频（旧实现是 0.35 s）"
        );
    }

    /// 延迟守卫与自适应目标不能互相打架。
    ///
    /// `can_lower()` 要求"存量 > 目标 + 20 ms"才肯收回临时余量。修剪保留的余量若小于
    /// 这个门槛，临时余量就永远收不回来，延迟长期偏高
    /// （2026-11-06 真机实测：中位 117 ms、13 个整秒 >150 ms）。
    ///
    /// 用编译期断言而不是运行时 `assert!`：这几个都是常量，改坏了应该直接编不过，
    /// 而不是等到有人跑测试才发现。
    const _: () = {
        const CAN_LOWER_GATE_MS: u32 = 20;
        assert!(
            TRIM_BACK_MS > CAN_LOWER_GATE_MS,
            "TRIM_BACK_MS 必须大于 can_lower 门槛，否则临时余量收不回来"
        );
        assert!(
            TRIM_OVER_MS > TRIM_BACK_MS,
            "修剪上限必须大于保留余量，否则守卫自相矛盾"
        );
        // 抬升步长必须能整除可用上限，否则 `boost` 会在 75/80 之间来回跳一格。
        assert!(
            TARGET_MAX_MS.is_multiple_of(TARGET_STEP_MS)
                && TARGET_MIN_MS.is_multiple_of(TARGET_STEP_MS),
            "STEP 必须能整除上下限，否则自适应会出现非整档的目标"
        );
    };

    /// 设备慢 4200 ppm（实测 20 s 差 84 ms）：必须靠补样本撑住，不能欠载。
    #[test]
    fn compensates_slow_producer() {        let (underruns, _dropped, max_fill, ppm) = simulate(0.99581, 60.0);
        assert_eq!(underruns, 0, "补偿后仍然欠载");
        assert!(
            (ppm + 4200.0).abs() < 600.0,
            "ppm 估计 {ppm} 与 4200 差太多"
        );
        assert!(max_fill < 120.0, "存量涨到 {max_fill} ms");
    }

    /// 设备快 5000 ppm：必须靠丢样本收住，不能让延迟无限涨。
    #[test]
    fn compensates_fast_producer() {
        let mut s = Sim::new();
        s.run(1.005, 60.0, None);
        let st = s.p.stats();
        assert!(st.frames_skipped > 0, "没有丢样本，说明没补偿");
        assert_eq!(st.frames_overflow, 0, "靠整帧丢弃兜底了，说明控制没起作用");
        let ppm = s.p.ppm();
        assert!((ppm - 5000.0).abs() < 600.0, "ppm 估计 {ppm} 与 5000 差太多");
        assert!(s.p.fill_ms() < 200.0);
    }

    /// 断连后必须停播、清空，不能把上一秒的声音留着继续播。
    #[test]
    fn clear_stops_playback() {
        let mut p = Playout::new(RATE, 80);
        p.push(&vec![1000i16; RATE as usize / 10]);
        let mut out = vec![0i16; 160];
        p.fill(&mut out);
        assert!(out.iter().any(|&v| v != 0), "暖机后应该出声");
        p.clear();
        out.fill(1);
        p.fill(&mut out);
        assert!(out.iter().all(|&v| v == 0), "clear 之后还在出声");
    }

    /// **A3 回归：自适应不许把用户选的基准值降下来。**
    ///
    /// 旧实现里 `set_target_ms(target - 5)` 直接改 `target_ms`，于是用户把滑块拉到
    /// 80 ms、跑稳 5 s 之后目标会一路降到 40 ms —— 界面上还显示 80。
    /// 现在的口径：`base_ms` 只由用户改，自适应只动 `boost_ms`。
    #[test]
    fn adaptive_never_lowers_the_user_set_base() {
        let mut p = Playout::new(RATE, 80);
        assert_eq!(p.base_ms(), 80);
        assert_eq!(p.boost_ms(), 0);

        // 跑一段完全稳态的仿真：不断喂、不断取，永远不会欠载。
        let mut out = vec![0i16; 160];
        for _ in 0..(30 * 100) {
            p.clock.advance(std::time::Duration::from_millis(10));
            p.push(&vec![100i16; 160]);
            p.fill(&mut out);
        }

        assert_eq!(p.base_ms(), 80, "用户选的基准被偷偷改了");
        assert_eq!(p.boost_ms(), 0, "稳态下不该有临时余量");
        assert_eq!(p.target_ms(), 80, "实际生效目标应当仍是 80");

        // 用户主动调小才允许变：这是唯一能改 base 的入口。
        p.set_target_ms(60);
        assert_eq!(p.base_ms(), 60);
        assert_eq!(p.target_ms(), 60);
    }

    /// 自适应抬升只加在 `boost` 上，且到上限就停。
    #[test]
    fn adaptive_boost_stops_at_target_max() {
        let mut p = Playout::new(RATE, 80);
        let mut out = vec![0i16; 160];
        // 先推满，让 playing = true。
        p.push(&vec![100i16; RATE as usize / 10]);
        p.clock.advance(std::time::Duration::from_millis(10));
        p.fill(&mut out);
        // 之后一直不喂，欠载 → 抬 boost。基准 80 已在上限，boost 没有可用空间。
        for _ in 0..50 {
            p.clock.advance(std::time::Duration::from_millis(10));
            p.fill(&mut out);
        }
        assert!(p.stats().underruns > 0, "不喂数据必须欠载");
        assert_eq!(p.base_ms(), 80, "基准不该被改");
        assert_eq!(p.target_ms(), TARGET_MAX_MS, "目标不该超过上限");
        assert_eq!(p.boost_ms(), 0, "基准已到上限时没有可加的余量");
    }

    /// **A5：欠载必须区分"链路断供"与"消费太快"。**
    ///
    /// 两种情形都在同一个 `fill` 里读到空缓冲，但归因不同：
    /// - 距上次推入 > `UNDERFED_MS` → 上游没喂上（`underruns_underfed`）；
    /// - 距上次推入很近 → 我们消费得比生产快（只计 `underruns`）。
    ///
    /// 混成一个数字是 KI-006 绕两轮的根源：抬缓冲目标会让前者也"变好"，
    /// 于是链路问题被当成缓冲不够。
    #[test]
    fn underruns_are_attributed_to_starvation_or_consumption() {
        let mut out = vec![0i16; 160];

        // 情形 A：先喂够让它播起来，然后彻底断供 → 断供型。
        let mut starved = Playout::new(RATE, 40);
        // 目标 40 ms = 640 帧；先给 50 ms，确保预填完成、`playing = true`。
        starved.push(&vec![100i16; RATE as usize * 50 / 1000]);
        for _ in 0..40 {
            starved.clock.advance(std::time::Duration::from_millis(20));
            starved.fill(&mut out);
        }
        let a = starved.stats();
        assert!(a.underruns > 0, "断供必须欠载");
        assert_eq!(
            a.underruns_underfed, a.underruns,
            "一直没喂数据时，欠载应当全部归到断供（实得 {}/{}）",
            a.underruns_underfed, a.underruns
        );

        // 情形 B：每拍都喂，但喂得比取得少（消费快于生产）→ 消费型。
        let mut fast = Playout::new(RATE, 40);
        // 先攒够预填量，让 playing = true。
        fast.push(&vec![100i16; RATE as usize * 40 / 1000 + 160]);
        fast.clock.advance(std::time::Duration::from_millis(1));
        fast.fill(&mut out);
        for _ in 0..200 {
            // 每 10 ms 只喂 100 帧（相当于 10 ms 需要 160 帧）→ 必然逐渐抽干。
            fast.clock.advance(std::time::Duration::from_millis(10));
            fast.push(&[100i16; 100]);
            fast.fill(&mut out);
        }
        let b = fast.stats();
        assert!(b.underruns > 0, "喂得比取得少，迟早欠载");
        assert!(
            b.underruns_underfed < b.underruns,
            "持续喂数据时不该全部归成断供（断供 {}/总 {}）",
            b.underruns_underfed,
            b.underruns
        );
    }

    /// **A4：报告延迟要用实测存量，不能拿目标值糊弄。**
    ///
    /// 目标 80 ms 时，修剪守卫会把稳态存量留在"目标 + 余量"附近，
    /// 实际 p50 明显高于 80。这条测试钉住"我们确实在测存量分布"，
    /// 而不是只回一个常数。
    #[test]
    fn fill_percentiles_reflect_measured_buffer() {
        let mut s = Sim::new();
        s.run(1.0, 30.0, None);
        let st = s.p.stats();

        let p50 = st.fill_percentile_ms(0.5).expect("跑够 30 s 应当有样本");
        let p95 = st.fill_percentile_ms(0.95).expect("跑够 30 s 应当有样本");

        // 目标 80 ms；实测应当落在同一量级，且 p95 ≥ p50。
        assert!(p50 > 40.0, "存量中位数只有 {p50} ms，说明没在测");
        assert!(p95 >= p50, "p95 {p95} 竟然小于 p50 {p50}");
        assert!(
            p50 < 200.0 && p95 < 260.0,
            "存量 {p50}/{p95} ms 超出桶范围，直方图可能需要加宽"
        );
        // 桶宽 4 ms，样本足够时误差应当很小。
        assert!(st.fill_samples > 1000, "样本太少：{}", st.fill_samples);
    }
}
