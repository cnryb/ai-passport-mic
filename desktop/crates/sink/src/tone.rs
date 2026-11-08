//! 正弦测试音发生器：把"声卡这一段通不通"从 BLE 链路里剥离出来单独验。
//!
//! 为什么单独成模块：`apmctl sink --tone` 和 GUI 的「验证声卡」跑的是同一件事
//! （往虚拟声卡推一段已知电平的正弦，再从它的输入端回读），两份各写一遍迟早会
//! 在频率、电平、块长上分叉，然后把"两边结论不同"误读成"其中一个通路有问题"。
//!
//! 电平口径：`dbfs` 是**满量程正弦的 RMS**。传 `-20.0` 时峰值约 -17 dBFS，
//! 既不会削波，又远高于虚拟声卡 16 bit 的噪声底，回读时容易判定。

//! 电平口径：`dbfs` 指**这段正弦的 RMS 电平**，不是峰值幅度。
//! 两者的差是固定的 3.01 dB（1/√2），很容易搞混 —— 2026-11-08 就踩过：
//! 按"幅度 = -20 dBFS"生成，回读却报 -23 dBFS，看起来像是链路丢了 3 dB。
//! 因为判据是拿 **RMS 对 RMS** 比较，参数就统一按 RMS 定义，幅度在内部换算。

/// 生成块长固定的正弦 PCM（单声道 `i16`）。
///
/// 相位在多次调用之间连续，所以分块推入拼起来是一段无缝的正弦；
/// 每块重新起相位会在块边界产生咔哒声，回读 RMS 不受影响但听感会以为是故障。
pub struct ToneSource {
    phase: f64,
    /// 每个样本的相位增量（弧度）
    step: f64,
    amp: f64,
    rate: u32,
}

impl ToneSource {
    /// `dbfs` 是 **RMS 电平**。内部换算成峰值幅度（乘 √2），
    /// 这样 [`rms_dbfs`] 量出来就是 `dbfs`，判据可以直接相减。
    pub fn new(rate: u32, freq_hz: f64, dbfs: f64) -> Self {
        let rms = 32768.0 * 10f64.powf(dbfs / 20.0);
        let amp = (rms * std::f64::consts::SQRT_2).min(32767.0);
        Self {
            phase: 0.0,
            step: std::f64::consts::TAU * freq_hz / rate as f64,
            amp,
            rate,
        }
    }

    /// 峰值幅度（相对满量程的线性值）。削波检查用。
    pub fn peak_amplitude(&self) -> f64 {
        self.amp
    }

    /// 填满 `out`，相位连续推进。
    pub fn fill(&mut self, out: &mut [i16]) {
        for slot in out.iter_mut() {
            *slot = (self.amp * self.phase.sin()) as i16;
            self.phase += self.step;
            if self.phase >= std::f64::consts::TAU {
                self.phase -= std::f64::consts::TAU;
            }
        }
    }

    /// 新分配一块长度为 `n` 的 PCM。
    pub fn block(&mut self, n: usize) -> Vec<i16> {
        let mut v = vec![0i16; n];
        self.fill(&mut v);
        v
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }
}

/// 一段 PCM 的 RMS（dBFS，相对 `i16` 满量程）。空输入返回 `None`。
///
/// 与 `Probe::rms_dbfs` 同一口径，两个数可以直接相减：
/// 写入 -20 dBFS、回读也应该是 -20 dBFS 附近，差得多说明中间被改了电平
/// （系统音量、声卡混音、被别的 App 抢走）。
pub fn rms_dbfs(samples: &[i16]) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let sum_sq: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    let mean = sum_sq / samples.len() as f64;
    if mean <= 0.0 {
        return Some(-120.0);
    }
    Some(20.0 * (mean.sqrt() / 32768.0).log10())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_matches_requested_level() {
        // -20 dBFS 的正弦，RMS 应当就是 -20 dBFS（±0.1）。参数按 RMS 定义，
        // 所以这里量的就是参数本身 —— 幅度/RMS 的 3 dB 换算已经在构造函数里做完。
        let mut t = ToneSource::new(16_000, 440.0, -20.0);
        let pcm = t.block(16_000);
        let db = rms_dbfs(&pcm).expect("非空");
        assert!((db + 20.0).abs() < 0.1, "期望 -20 dBFS，实得 {db:.3}");
    }

    #[test]
    fn block_size_does_not_bias_rms() {
        // 单块 320 样本的 RMS 与整段一致（±0.1）：分块推入本身不引入电平偏差。
        // 这条曾经救过一次误判 —— 当时以为 -23 dBFS 是"块内不足整周期"造成的，
        // 实际是**幅度/RMS 差 3 dB**（见 `ToneSource::new`）。别再重复那个猜测。
        let mut a = ToneSource::new(16_000, 440.0, -20.0);
        let block = rms_dbfs(&a.block(320)).expect("非空");
        let mut b = ToneSource::new(16_000, 440.0, -20.0);
        let whole = rms_dbfs(&b.block(16_000)).expect("非空");
        assert!((block - whole).abs() < 0.1, "单块 {block:.3} vs 整段 {whole:.3}");
    }

    #[test]
    fn tone_does_not_clip() {
        // -6 dBFS RMS 对应峰值约 -3 dBFS，远不到满量程。
        let mut t = ToneSource::new(16_000, 400.0, -6.0);
        let pcm = t.block(16_000);
        let peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        assert!(peak < 32767, "-6 dBFS RMS 不应削波，实得峰值 {peak}");
        assert!(t.peak_amplitude() < 32768.0);
    }

    #[test]
    fn loud_source_is_clamped_instead_of_wrapping() {
        // 0 dBFS RMS 的正弦需要峰值 +3 dBFS，物理上做不到。
        // 这里选择"夹到满量程"而不是让 i16 溢出回绕（回绕是刺耳的爆音）。
        let mut t = ToneSource::new(16_000, 400.0, 0.0);
        let pcm = t.block(1600);
        let peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        assert!(peak <= 32767, "峰值 {peak} 超过满量程");
        assert!(pcm.iter().all(|s| *s > -32768), "出现了回绕产生的极小值");
    }

    #[test]
    fn phase_is_continuous_across_blocks() {
        // 分块生成与一次生成必须逐样本相同：否则块边界会有咔哒声。
        let mut a = ToneSource::new(16_000, 440.0, -20.0);
        let mut b = ToneSource::new(16_000, 440.0, -20.0);
        let joined: Vec<i16> = (0..5).flat_map(|_| a.block(320)).collect();
        let one_shot = b.block(1600);
        assert_eq!(joined, one_shot);
    }

    #[test]
    fn silent_input_has_floor_level() {
        assert_eq!(rms_dbfs(&[]), None);
        assert_eq!(rms_dbfs(&[0; 100]), Some(-120.0));
    }
}
