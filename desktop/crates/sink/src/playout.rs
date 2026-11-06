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

/// 抖动缓冲目标下限 / 上限（毫秒），与 `docs/ARCHITECTURE.md` 的 40–80 ms 一致。
pub const TARGET_MIN_MS: u32 = 40;
pub const TARGET_MAX_MS: u32 = 80;

/// 漂移控制器的时间常数（秒）。越小纠得越快、变调越明显。
pub const DRIFT_TAU_SEC: f64 = 2.0;
/// 单样本插删的速率上限（1% = 10000 ppm）。实测漂移约 4200 ppm，留出一倍余量。
pub const MAX_SLIP: f64 = 0.01;

/// 漂移估计的最短样本时长（秒）。太短的窗口噪声压过真信号，
/// 宁可报 0 也不要给一个假数字 —— 这个值会被写进验收报告。
pub const DRIFT_MIN_SAMPLE_SEC: f64 = 10.0;

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

#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    /// 收到的帧（单声道采样）数
    pub frames_pushed: u64,
    /// 声卡实际取走的帧数
    pub frames_consumed: u64,
    /// 欠载次数（缓冲被读空）
    pub underruns: u64,
    /// 欠载时补进去的帧数（用上一个样本填充，避免爆音）
    pub frames_silenced: u64,
    /// 超出容量被丢弃的最旧帧数（兜底路径，正常不该触顶）
    pub frames_overflow: u64,
    /// 延迟守卫一次性修剪掉的最旧帧数（停摆积压，不是漂移补偿）
    pub frames_trimmed: u64,
    /// 推入样本的平方和，用于算写入侧 RMS（和回环侧对比）
    pub sum_sq: f64,
    /// 因欠载抬高目标缓冲的次数
    pub target_raises: u64,
    /// 因稳定收回目标缓冲的次数
    pub target_lowers: u64,
    /// 漂移补偿补出来的样本数（生产慢于消费）
    pub frames_inserted: u64,
    /// 漂移补偿丢掉的样本数（生产快于消费）
    pub frames_skipped: u64,
    /// 延迟守卫一次性修剪掉的最旧帧数（停摆积压，不是漂移补偿）
    pub trim_events: u64,
    /// 声卡回调的间隔统计（`docs/KNOWN_ISSUES.md` KI-006 要的"停摆时长分布"）。
    pub callbacks: u64,
    /// 回调间隔超过 1.5× 应有周期的次数（= 明显的回调停摆）
    pub stalled_callbacks: u64,
    /// 观测到的最大回调间隔（毫秒）。这个数字直接决定缓冲上限该抬到多少。
    pub max_callback_gap_ms: f64,
    /// 按间隔分桶的回调次数（桶边界见 [`GAP_BUCKETS_MS`]）
    pub gap_histogram: [u64; 8],
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
}

pub struct Playout {
    buf: VecDeque<i16>,
    rate: u32,
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
    /// 秒表。生产环境就是 `Instant::now()` 的封装；测试里可以喂假时间，
    /// 否则"用墙钟估漂移"这个算法在单测里根本跑不出来（循环几毫秒就跑完 60 秒仿真）。
    clock: Clock,
    /// 上一次声卡回调的时刻，用来量回调间隔（KI-006 的停摆时长分布）。
    last_callback: Option<Instant>,
    /// 第一次回调的时刻，欠载时刻以它为原点。
    first_callback: Option<Instant>,
    /// 相邻两次回调之间"应该"有多少帧，用来判断这次回调是不是明显迟到。
    frames_per_callback: usize,
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
        let target_ms = target_ms.clamp(TARGET_MIN_MS, TARGET_MAX_MS);
        let rate = rate.max(1);
        Self {
            buf: VecDeque::with_capacity(capacity_frames(TARGET_MAX_MS, rate)),
            rate,
            target_ms,
            target_frames: rate as usize * target_ms as usize / 1000,
            capacity: capacity_frames(TARGET_MAX_MS, rate),
            playing: false,
            stable_frames: 0,
            last: 0,
            slip: 0.0,
            drift_base: None,
            clock: Clock::new(),
            last_callback: None,
            first_callback: None,
            frames_per_callback: 0,
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
    pub fn set_target_ms(&mut self, ms: u32) {
        self.target_ms = ms.clamp(TARGET_MIN_MS, TARGET_MAX_MS);
        self.target_frames = self.rate as usize * self.target_ms as usize / 1000;
    }

    /// 稳定判定窗口：连续这么多帧没有欠载才考虑收回缓冲（默认 5 秒）。
    fn stable_window(&self) -> u64 {
        self.rate as u64 * 5
    }

    /// 收回缓冲的前提：现存量要比目标还多出一截，否则会立刻又欠载。
    fn can_lower(&self) -> bool {
        self.target_ms > TARGET_MIN_MS
            && self.buf.len() > self.target_frames + (self.rate as usize / 50) // +20 ms
    }

    /// 推入一段单声道 PCM。
    pub fn push(&mut self, samples: &[i16]) {
        self.buf.extend(samples.iter().copied());
        self.stats.sum_sq += samples.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>();
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

        if !self.playing {
            if self.buf.len() >= self.target_frames {
                self.playing = true;
                // 播起来的那一刻记下漂移估计的起算点（墙钟 + 已推入帧数）。
                self.drift_base = Some(Arrival {
                    at: self.clock.now(),
                    pushed: self.stats.frames_pushed,
                });
            } else {
                // 预填阶段不出声，但这不算欠载：还没有开始播
                out.fill(0);
                return;
            }
        }

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
            self.stats.trim_events += 1;
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
                                let next = self.target_ms.saturating_sub(5);
                                self.set_target_ms(next);
                                self.stats.target_lowers += 1;
                            }
                        }
                        s
                    }
                    None => {
                        // 一次回调里缺多少帧都只算一次欠载事件；缺的帧数单独记
                        shortage = true;
                        self.stats.frames_silenced += 1;
                        // 欠载说明余量不够：抬高一档，并沿用上一个样本避免爆音
                        self.stable_frames = 0;
                        if self.target_ms < TARGET_MAX_MS {
                            let next = (self.target_ms + 5).min(TARGET_MAX_MS);
                            self.set_target_ms(next);
                            self.stats.target_raises += 1;
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
            // 记下这次欠载的时刻（相对第一次回调）。音频回调里不能分配、不能 IO，
            // 所以只写进预分配的定长数组，打印留给调用方。
            if self.stats.underruns_logged < UNDERRUN_LOG {
                if let Some(first) = self.first_callback {
                    let at = self
                        .clock
                        .now()
                        .saturating_duration_since(first)
                        .as_secs_f64()
                        * 1000.0;
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
    /// `can_lower()` 要求"存量 > 目标 + 20 ms"才肯把目标降一档。修剪保留的余量若小于
    /// 这个门槛，缓冲就永远回不到"够低"，目标只升不降、延迟长期偏高
    /// （2026-11-06 真机实测：中位 117 ms、13 个整秒 >150 ms）。
    ///
    /// 用编译期断言而不是运行时 `assert!`：这几个都是常量，改坏了应该直接编不过，
    /// 而不是等到有人跑测试才发现。
    const _: () = {
        const CAN_LOWER_GATE_MS: u32 = 20;
        assert!(
            TRIM_BACK_MS > CAN_LOWER_GATE_MS,
            "TRIM_BACK_MS 必须大于 can_lower 门槛，否则目标卡在 80 ms 降不下来"
        );
        assert!(
            TRIM_OVER_MS > TRIM_BACK_MS,
            "修剪上限必须大于保留余量，否则守卫自相矛盾"
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
}
