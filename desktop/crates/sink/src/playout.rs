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

/// 抖动缓冲目标下限 / 上限（毫秒），与 `docs/ARCHITECTURE.md` 的 40–80 ms 一致。
pub const TARGET_MIN_MS: u32 = 40;
pub const TARGET_MAX_MS: u32 = 80;

/// 漂移控制器的时间常数（秒）。越小纠得越快、变调越明显。
pub const DRIFT_TAU_SEC: f64 = 2.0;
/// 单样本插删的速率上限（1% = 10000 ppm）。实测漂移约 4200 ppm，留出一倍余量。
pub const MAX_SLIP: f64 = 0.01;

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
    /// 超出容量被丢弃的最旧帧数
    pub frames_dropped: u64,
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
    stats: Stats,
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
    pub fn ppm(&self) -> f64 {
        let consumed = self.stats.frames_consumed;
        if consumed == 0 {
            return 0.0;
        }
        (self.stats.frames_skipped as f64 - self.stats.frames_inserted as f64) * 1e6
            / consumed as f64
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
        // 超容量：丢最旧的，保证延迟有上界
        while self.buf.len() > self.capacity {
            self.buf.pop_front();
            self.stats.frames_dropped += 1;
        }
        self.stats.frames_pushed += samples.len() as u64;
    }

    /// 断连时调用：停止播放并丢弃旧缓冲。
    pub fn clear(&mut self) {        self.buf.clear();
        self.playing = false;
        self.last = 0;
        self.slip = 0.0;
        self.stable_frames = 0;
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
        if !self.playing {
            if self.buf.len() >= self.target_frames {
                self.playing = true;
            } else {
                // 预填阶段不出声，但这不算欠载：还没有开始播
                out.fill(0);
                return;
            }
        }

        // 每轮回调更新一次漂移速率：err 为正（存量偏高）→ 多消费一点。
        let err = self.buf.len() as f64 - self.target_frames as f64;
        let slip_per_frame = (err / (self.rate as f64 * DRIFT_TAU_SEC)).clamp(-MAX_SLIP, MAX_SLIP);

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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 16_000;

    /// 以 `producer_ratio` 倍速生产，按 10 ms 回调消费；返回测量结果。
    /// 返回 (欠载次数（暖机后）, 丢帧, 最大存量 ms, ppm 估计)
    fn simulate(producer_ratio: f64, seconds: f64) -> (u64, u64, f32, f64) {
        let mut p = Playout::new(RATE, 80);
        let callback = 160usize; // 10 ms
        let callbacks = (seconds * 100.0) as usize;
        let mut produced = 0.0f64;
        let mut out = vec![0i16; callback];
        let warm = 5_000usize; // 前 5 s 算暖机，不计欠载
        let mut underruns_after_warm = 0u64;
        let mut max_fill = 0.0f32;

        for i in 0..callbacks {
            // 匀速生产：每个回调应到 callback × ratio 个样本（含小数累积）
            produced += callback as f64 * producer_ratio;
            let n = produced.floor();
            produced -= n;
            p.push(&vec![100i16; n as usize]);

            p.fill(&mut out);
            if i * callback >= warm && p.stats().underruns > underruns_after_warm {
                underruns_after_warm = p.stats().underruns;
            }
            max_fill = max_fill.max(p.fill_ms());
        }
        (underruns_after_warm, p.stats().frames_dropped, max_fill, p.ppm())
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

    /// 设备慢 4200 ppm（实测 20 s 差 84 ms）：必须靠补样本撑住，不能欠载。
    #[test]
    fn compensates_slow_producer() {
        let (underruns, _dropped, max_fill, ppm) = simulate(0.99581, 60.0);
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
        let mut p = Playout::new(RATE, 80);
        let callback = 160usize;
        let mut produced = 0.0f64;
        let mut out = vec![0i16; callback];
        let seconds = 60;
        for _ in 0..seconds * 100 {
            produced += callback as f64 * 1.005;
            let n = produced.floor();
            produced -= n;
            p.push(&vec![100i16; n as usize]);
            p.fill(&mut out);
        }
        let st = p.stats();
        assert!(st.frames_skipped > 0, "没有丢样本，说明没补偿");
        assert_eq!(st.frames_dropped, 0, "靠整帧丢弃兜底了，说明控制没起作用");
        assert!(
            (p.ppm() - 5000.0).abs() < 600.0,
            "ppm 估计 {} 与 5000 差太多",
            p.ppm()
        );
        assert!(p.fill_ms() < 200.0);
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
