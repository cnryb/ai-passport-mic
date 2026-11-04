//! 满量程毛刺抑制。
//!
//! ## 现象与结论
//!
//! 真机上偶发 0.2–1 s 的 ±32767 逐样本变号噪声，同一次录音里 `序号跳变=0`
//! （说明不是丢块），安静段也会突然出现，之后又自己恢复。
//!
//! 复现方式见 `tests/glitch_repro.rs`：把同样的波形喂给 ADPCM 编码 → 解码，
//! **编解码器本身不会产生这种输出**（削波瞬态最多污染一两块就恢复）。所以垃圾
//! 来自采集端（I2S / ES8311 的瞬态读出），被原样编码上了链路。治理放在两处：
//!
//! - 固件：编码前体检输入块，病态块不送出去（`device/main/app_main.c`）；
//! - PC 端：即使收到，也不能让会议软件听见 —— 就是这个模块。
//!
//! ## 判定口径
//!
//! 一个样本「满量程」= `|x| >= 32000`。病态段的定义是**连续 ≥ 8 个满量程样本
//! 且逐样本变号**。@16 kHz 下 8 个样本 = 0.5 ms，等价于 ≥ 8 kHz 的满量程方波：
//! 人声、室内噪声、麦克风削波（削波是平顶、同号连续）都做不出这个形状，所以
//! 误杀概率可以忽略。
//!
//! 处理方式是静音替换，而不是把样本钳到阈值：钳位仍然是刺耳的方波，静音才是
//! 用户能接受的结果。解除静音时接 64 个样本的淡入，避免「突然有声」的爆点。

/// `|x| >= SATURATION_LEVEL` 视作满量程。
pub const SATURATION_LEVEL: i16 = 32_000;
/// 连续多少个「满量程且变号」样本判定为病态（0.5 ms @ 16 kHz）。
pub const MIN_RUN: usize = 8;
/// 解除静音前要求的连续正常样本数（2 ms @ 16 kHz）。
pub const RELEASE_RUN: usize = 32;
/// 解除静音时的淡入长度。
pub const FADE_SAMPLES: usize = 64;

/// 毛刺抑制器。状态跨块，必须每块复用同一个实例。
#[derive(Debug, Clone)]
pub struct Suppressor {
    /// 当前「满量程且变号」的连续长度
    run: usize,
    /// 上一个满量程样本的符号（+1 / -1 / 0）
    prev_sign: i8,
    /// 是否正在静音
    muting: bool,
    /// 静音期间连续正常样本数
    calm: usize,
    /// 淡入剩余样本数
    fade_left: usize,
    /// 被静音替换的样本总数
    pub muted_samples: u64,
    /// 触发次数（一次持续静音算一次）
    pub events: u64,
}

impl Default for Suppressor {
    fn default() -> Self {
        Self::new()
    }
}

impl Suppressor {
    pub fn new() -> Self {
        Self {
            run: 0,
            prev_sign: 0,
            muting: false,
            calm: 0,
            fade_left: 0,
            muted_samples: 0,
            events: 0,
        }
    }

    /// 就地处理一块 PCM（单声道、16 bit）。返回是否改动过样本。
    pub fn process(&mut self, pcm: &mut [i16]) -> bool {
        let mut changed = false;
        for s in pcm.iter_mut() {
            let (sat, sign) = classify(*s);
            if sat {
                if self.prev_sign != 0 && sign != self.prev_sign {
                    self.run += 1;
                } else {
                    self.run = 1;
                }
                self.prev_sign = sign;
            } else {
                self.run = 0;
                self.prev_sign = 0;
            }

            if !self.muting {
                if self.run >= MIN_RUN {
                    self.muting = true;
                    self.events += 1;
                    self.calm = 0;
                }
            }

            if self.muting {
                // 静音期间：只有当样本既不饱和、也不是刚退出时才累计 calm
                if sat {
                    self.calm = 0;
                } else {
                    self.calm += 1;
                }
                if self.calm >= RELEASE_RUN {
                    self.muting = false;
                    self.fade_left = FADE_SAMPLES;
                }
                *s = 0;
                self.muted_samples += 1;
                changed = true;
                continue;
            }

            if sat {
                self.calm = 0;
            }

            if self.fade_left > 0 {
                let g = (FADE_SAMPLES - self.fade_left) as i32;
                *s = ((*s as i32) * g / FADE_SAMPLES as i32) as i16;
                self.fade_left -= 1;
                changed = true;
            }
        }
        changed
    }
}

/// `(是否满量程, 符号)`。
fn classify(x: i16) -> (bool, i8) {
    if x >= SATURATION_LEVEL {
        (true, 1)
    } else if x <= -SATURATION_LEVEL {
        (true, -1)
    } else {
        (false, 0)
    }
}

/// 只读扫描：给 `analyze` 的报告用。不改数据，复用同一套判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Scan {
    /// 病态段数
    pub events: u64,
    /// 落在病态段里的样本数（不含淡入 / 静音替换的部分）
    pub samples: u64,
}

impl Scan {
    pub fn duration_ms(&self, sample_rate: u32) -> f64 {
        if sample_rate == 0 {
            return 0.0;
        }
        self.samples as f64 * 1000.0 / sample_rate as f64
    }
}

pub fn scan(samples: &[i16]) -> Scan {
    let mut out = Scan::default();
    let mut run = 0usize;
    let mut prev_sign = 0i8;
    let mut in_event = false;
    for &x in samples {
        let (sat, sign) = classify(x);
        if sat {
            if prev_sign != 0 && sign != prev_sign {
                run += 1;
            } else {
                run = 1;
            }
            prev_sign = sign;
        } else {
            run = 0;
            prev_sign = 0;
        }
        if run >= MIN_RUN {
            if !in_event {
                in_event = true;
                out.events += 1;
            }
            out.samples += 1;
        } else if !sat {
            in_event = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn speech_like(n: usize, amp: f64) -> Vec<i16> {
        (0..n)
            .map(|i| {
                let t = i as f64 / 16_000.0;
                (amp * (2.0 * std::f64::consts::PI * 220.0 * t).sin()) as i16
            })
            .collect()
    }

    /// 病态段必须被静音，且不能把两侧的真实音频一起吃掉。
    #[test]
    fn suppresses_alternating_full_scale() {
        let mut pcm = speech_like(16_000, 300.0);
        let head = pcm.clone();
        for i in 0..12_800 {
            pcm.push(if i % 2 == 0 { 32_767 } else { -32_768 });
        }
        let tail_start = pcm.len();
        pcm.extend(speech_like(4_000, 300.0));

        let mut sup = Suppressor::new();
        sup.process(&mut pcm);

        assert_eq!(sup.events, 1, "应识别为一次持续毛刺");
        // 前 1 s 的真实音频保持原样
        assert_eq!(&pcm[..16_000], &head[..], "毛刺之前的音频被改了");
        // 病态段被压掉
        let worst = pcm[16_100..tail_start].iter().map(|v| (*v as i32).abs()).max().unwrap();
        assert!(worst < 1_000, "病态段没被静音，最大 {worst}");
        // 之后恢复出真实音频（淡入结束后电平应回到 300 量级）
        let after = &pcm[tail_start + FADE_SAMPLES + 100..];
        let peak = after.iter().map(|v| (*v as i32).abs()).max().unwrap();
        assert!(peak > 200, "解除静音后没恢复声音，峰值 {peak}");
    }

    /// 真实削波是平顶、同号连续，不能误杀。
    #[test]
    fn keeps_clipped_speech() {
        let mut pcm = vec![0i16; 4_000];
        for (i, v) in pcm.iter_mut().enumerate() {
            *v = if (i / 400) % 2 == 0 { 32_767 } else { -32_768 };
        }
        let orig = pcm.clone();
        let mut sup = Suppressor::new();
        sup.process(&mut pcm);
        assert_eq!(pcm, orig, "平顶削波被误判成毛刺");
        assert_eq!(sup.events, 0);
    }

    /// 低电平噪声与正常语音不受影响。
    #[test]
    fn keeps_normal_audio() {
        let mut pcm = speech_like(8_000, 8_000.0);
        let orig = pcm.clone();
        let mut sup = Suppressor::new();
        assert!(!sup.process(&mut pcm));
        assert_eq!(pcm, orig);
        assert_eq!(sup.muted_samples, 0);
    }

    /// 状态必须跨块延续：病态段切成 320 样本的块也不能漏。
    #[test]
    fn state_carries_across_blocks() {
        let mut sup = Suppressor::new();
        let mut muted_total = 0u64;
        for b in 0..40 {
            let mut block: Vec<i16> = (0..320)
                .map(|i| if (b * 320 + i) % 2 == 0 { 32_767 } else { -32_768 })
                .collect();
            sup.process(&mut block);
            muted_total += block.iter().filter(|v| **v == 0).count() as u64;
        }
        assert_eq!(sup.events, 1, "跨块被算成了多次事件");
        assert!(muted_total > 12_000, "跨块时漏掉了 {muted_total}");
    }

    /// 扫描口径与抑制器一致，且不改数据。
    #[test]
    fn scan_matches_suppressor() {
        let mut pcm = vec![0i16; 1_000];
        for (i, v) in pcm.iter_mut().enumerate().take(500).skip(100) {
            *v = if i % 2 == 0 { 32_767 } else { -32_768 };
        }
        let s = scan(&pcm);
        assert_eq!(s.events, 1);
        assert!(s.samples >= 390);
        assert!(s.duration_ms(16_000) > 20.0);
    }
}
