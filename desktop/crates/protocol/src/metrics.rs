//! 音质客观指标。口径来自 `docs/SOUND_QUALITY.md` 的「客观指标与初始阈值」。
//!
//! 这里算的只是**软件分析**：它说明文件本身没有明显削波 / 直流偏置 / 电平异常，
//! 不代表真机链路、虚拟声卡和会议软件端到端通过。报告里必须分开写。

#[derive(Debug, Clone, PartialEq)]
pub struct Metrics {
    pub samples: usize,
    pub duration_s: f64,
    /// RMS（dBFS）。全静音时为 `None`，不要写成 -inf 混进 JSON。
    pub rms_dbfs: Option<f64>,
    /// 峰值（dBFS）。全静音时为 `None`。
    pub peak_dbfs: Option<f64>,
    /// `abs(x) >= 32760` 的样本比例（%）。
    pub clipping_pct: f64,
    /// `abs(mean(x)) / 32768`（%）。
    pub dc_offset_pct: f64,
    /// 相邻样本符号变化比例（%）。只记录，没有合格线。
    pub zero_crossing_pct: f64,
}

/// 一条阈值判定。
#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub actual: String,
    pub expected: &'static str,
}

const FULL_SCALE: f64 = 32768.0;
const CLIPPING_LEVEL: i32 = 32760;

pub fn analyze(samples: &[i16], sample_rate: u32) -> Metrics {
    let n = samples.len();
    if n == 0 || sample_rate == 0 {
        return Metrics {
            samples: n,
            duration_s: 0.0,
            rms_dbfs: None,
            peak_dbfs: None,
            clipping_pct: 0.0,
            dc_offset_pct: 0.0,
            zero_crossing_pct: 0.0,
        };
    }

    let mut sum_sq = 0.0f64;
    let mut sum = 0.0f64;
    let mut peak = 0i32;
    let mut clipped = 0usize;
    let mut crossings = 0usize;

    for (i, sample) in samples.iter().enumerate() {
        let x = *sample as i32;
        let xf = x as f64;
        sum_sq += xf * xf;
        sum += xf;

        let abs = x.abs();
        if abs > peak {
            peak = abs;
        }
        if abs >= CLIPPING_LEVEL {
            clipped += 1;
        }
        if i > 0 && ((samples[i - 1] < 0) != (*sample < 0)) {
            crossings += 1;
        }
    }

    let rms = (sum_sq / n as f64).sqrt();
    let rms_dbfs = if rms > 0.0 {
        Some(20.0 * (rms / FULL_SCALE).log10())
    } else {
        None
    };
    let peak_dbfs = if peak > 0 {
        Some(20.0 * (peak as f64 / FULL_SCALE).log10())
    } else {
        None
    };

    Metrics {
        samples: n,
        duration_s: n as f64 / sample_rate as f64,
        rms_dbfs,
        peak_dbfs,
        clipping_pct: clipped as f64 * 100.0 / n as f64,
        dc_offset_pct: (sum / n as f64).abs() / FULL_SCALE * 100.0,
        zero_crossing_pct: if n > 1 {
            crossings as f64 * 100.0 / (n - 1) as f64
        } else {
            0.0
        },
    }
}

/// 按 `docs/SOUND_QUALITY.md` 的初始阈值逐项判定。任一不通过就是软件分析不通过。
pub fn evaluate(metrics: &Metrics) -> Vec<Check> {
    let rms_ok = matches!(metrics.rms_dbfs, Some(v) if (-30.0..=-12.0).contains(&v));
    let peak_ok = matches!(metrics.peak_dbfs, Some(v) if v <= -1.0);

    vec![
        Check {
            name: "时长",
            ok: metrics.duration_s >= 10.0,
            actual: format!("{:.2} s", metrics.duration_s),
            expected: ">= 10 s（太短不用于结论）",
        },
        Check {
            name: "RMS",
            ok: rms_ok,
            actual: match metrics.rms_dbfs {
                Some(v) => format!("{v:.2} dBFS"),
                None => "全静音".to_string(),
            },
            expected: "-30 ~ -12 dBFS",
        },
        Check {
            name: "峰值",
            ok: peak_ok,
            actual: match metrics.peak_dbfs {
                Some(v) => format!("{v:.2} dBFS"),
                None => "全静音".to_string(),
            },
            expected: "<= -1 dBFS",
        },
        Check {
            name: "削波比例",
            ok: metrics.clipping_pct < 0.1,
            actual: format!("{:.3} %", metrics.clipping_pct),
            expected: "< 0.1 %",
        },
        Check {
            name: "直流偏置",
            ok: metrics.dc_offset_pct < 1.0,
            actual: format!("{:.3} %", metrics.dc_offset_pct),
            expected: "< 1 %",
        },
        Check {
            name: "过零率",
            ok: true, // 没有合格线，只记录
            actual: format!("{:.2} %", metrics.zero_crossing_pct),
            expected: "仅记录",
        },
    ]
}

pub fn software_pass(checks: &[Check]) -> bool {
    checks.iter().all(|c| c.ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_has_no_level() {
        let m = analyze(&vec![0i16; 16_000], 16_000);
        assert_eq!(m.rms_dbfs, None);
        assert_eq!(m.peak_dbfs, None);
        assert_eq!(m.duration_s, 1.0);
        assert_eq!(m.clipping_pct, 0.0);
        assert!(!software_pass(&evaluate(&m)), "静音不能判为通过");
    }

    #[test]
    fn full_scale_square_clips() {
        let mut samples = vec![0i16; 1000];
        for (i, s) in samples.iter_mut().enumerate() {
            *s = if i < 500 { 32767 } else { -32768 };
        }
        let m = analyze(&samples, 16_000);
        assert!(m.clipping_pct > 99.0);
        assert_eq!(m.peak_dbfs.unwrap(), 0.0);
        assert!(!software_pass(&evaluate(&m)));
    }

    #[test]
    fn dc_offset_detected() {
        let samples = vec![1638i16; 16_000]; // 5% 满量程
        let m = analyze(&samples, 16_000);
        assert!((m.dc_offset_pct - 5.0).abs() < 0.01);
        assert_eq!(m.zero_crossing_pct, 0.0);
    }

    #[test]
    fn sine_at_minus_18_dbfs_passes_level_checks() {
        let amp = 32768.0 * 10f64.powf(-18.0 / 20.0);
        let samples: Vec<i16> = (0..16_000)
            .map(|i| {
                let t = i as f64 / 16_000.0;
                (amp * (2.0 * std::f64::consts::PI * 440.0 * t).sin()).round() as i16
            })
            .collect();
        let m = analyze(&samples, 16_000);
        assert!((m.rms_dbfs.unwrap() + 21.0).abs() < 0.5, "RMS = {:?}", m.rms_dbfs);
        assert!(m.clipping_pct < 0.1);
        assert!(m.dc_offset_pct < 1.0);
        // 440 Hz / 16 kHz：每周期约 36.4 个采样，每周期 2 次过零 → 约 5.5%
        assert!(m.zero_crossing_pct > 4.0 && m.zero_crossing_pct < 7.0);
    }
}
