//! 虚拟声卡「一键验证」：往它写一段已知电平的正弦，再从它的输入端回读。
//!
//! 存在的理由：程序自称"我写了 N 帧"只是自说自话。用户真正关心的是
//! **会议软件把麦克风选成这块声卡之后，能不能听到声音**。中间隔着三层
//! （写入 → 声卡直通 → 输入端回读），任何一层断了都表现为"对方听不到"，
//! 而三层里只有最后一层是用户能感知的。所以这里一次性把三层都量出来：
//!
//! ```text
//!   written_rms  → 推入的样本电平（应当等于请求的 dBFS）
//!   underruns    → 声卡有没有被喂空（> 0 说明喂入端/声卡有问题）
//!   loopback_rms → 从输入端回读到的电平（这才是"能不能被听到"）
//! ```
//!
//! 判据刻意保守：回读到的电平比写入低 12 dB 以上就算失败。BlackHole 这类
//! 直通设备是 0 dB 的，差 12 dB 只能是中间被静音、被系统音量压掉，
//! 或者根本没连到同一块设备上。
//!
//! **已知边界**：`loopback_rms` 拿到 `None` 或 -120 dBFS 有两种截然不同的原因——
//! 声卡没直通，或者本进程没有麦克风权限。本模块无法区分（读输入设备本身就需要权限），
//! 所以报告里必须把两种可能都写出来，不能替用户下结论。

use crate::tone::{rms_dbfs, ToneSource};
use crate::{Probe, Sink};
use anyhow::{Context, Result};

/// 判定结果。只区分"能自动判断"的三种，其余一律交给人看数字。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SinkCheckVerdict {
    /// 回读电平与写入电平接近，声卡直通正常。
    Pass,
    /// 回读几乎静音：声卡没直通，或本进程没有麦克风权限。
    LoopbackSilent,
    /// 输入端一个样本都没读到：麦克风权限被拒的可能性最大。
    LoopbackNoSamples,
    /// 写入端自身有问题（欠载 / 喂入不足），此时回读结论不可采信。
    SinkStalled,
}

/// 一次验证的完整结果。字段都用原始量，判定另给一个 `verdict`，
/// 让人可以不同意判定而自己看数字。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SinkCheckReport {
    /// 实际打开的声卡名（可能比用户给的子串长）。
    pub device_name: String,
    /// 请求的电平（dBFS）。
    pub requested_dbfs: f64,
    /// 时长（秒）。
    pub seconds: u64,
    /// 推入的帧数。
    pub pushed_frames: u64,
    /// 推入样本自身的 RMS。与 `loopback_rms_dbfs` 同一口径，可直接相减。
    pub written_rms_dbfs: Option<f64>,
    /// 从输入端回读到的秒数。0 表示读不到输入。
    pub loopback_seconds: f64,
    /// 回读电平。
    pub loopback_rms_dbfs: Option<f64>,
    /// 回读峰值电平。
    pub loopback_peak_dbfs: Option<f64>,
    /// 运行期欠载次数（收尾 drain 造成的不算，见 [`crate::Sink::clear`]）。
    pub underruns: u64,
    pub verdict: SinkCheckVerdict,
}

/// 判定阈值：回读比写入低多少 dB 就认为不通。
///
/// 取 12 dB 而不是 3 dB，是因为系统音量滑杆、某些声卡的软件混音会带来几 dB 偏差，
/// 而真正的"没直通"是 60 dB 以上的差距，中间没有模糊地带。
const LOOPBACK_TOLERANCE_DB: f64 = 12.0;

/// 跑一次完整验证。`seconds` 建议 2–3 s：太短 RMS 不稳，太长用户等不及。
pub fn check(name_substr: &str, seconds: u64) -> Result<SinkCheckReport> {
    let seconds = seconds.clamp(1, 10);
    let rate = 16_000u32;
    let target_ms = 60;
    let dbfs = -20.0;

    let mut sink = Sink::open(name_substr, rate, target_ms)
        .with_context(|| format!("打开输出设备 \"{name_substr}\" 失败"))?;
    let device_name = sink.device_name().to_string();

    // 探针必须在**写入之前**开：先开输入，再开始推，才不会漏掉开头的样本。
    // 探针失败不算致命错误（可能是权限），但要如实带出去。
    let probe = Probe::open(&device_name, rate).ok();

    // 440 Hz 是听感上"标准测试音"的频率；这里不依赖整周期，因为电平按 RMS 定义，
    // 单块与整段的 RMS 差异实测 < 0.01 dB（`tone` 模块的单测钉住了这一点）。
    let mut tone = ToneSource::new(rate, 440.0, dbfs);
    let block = 320usize; // 20 ms
    let blocks = seconds as usize * 50;
    let mut written = Vec::with_capacity(blocks * block);
    let started = std::time::Instant::now();

    for i in 0..blocks {
        let pcm = tone.block(block);
        sink.push(&pcm);
        written.extend_from_slice(&pcm);

        // 按绝对时刻配速。写成"每块 sleep 20 ms"会累加生成开销，
        // 越跑越落后，最后报出一堆假欠载（`apmctl sink --tone` 踩过这个坑）。
        let deadline = std::time::Duration::from_micros((i as u64 + 1) * 20_000);
        let elapsed = started.elapsed();
        if deadline > elapsed {
            std::thread::sleep(deadline - elapsed);
        }
    }

    // 欠载统计要在收尾等待之前取（见 `Sink::clear` 的注释）。
    let stats = sink.stats();
    // 再等一会儿让最后 60 ms 缓冲真正吐出去，回读才读得到尾巴。
    std::thread::sleep(std::time::Duration::from_millis(120));
    sink.clear();

    let written_rms = rms_dbfs(&written);
    let (loopback_seconds, loopback_rms, loopback_peak, loopback_frames) = match &probe {
        Some(p) => (p.seconds(), p.rms_dbfs(), p.peak_dbfs(), p.frames()),
        None => (0.0, None, None, 0),
    };

    let verdict = if stats.underruns > 0 && written_rms.unwrap_or(-120.0) > -60.0 {
        // 有欠载：写入端自己都不稳，回读结论不可采信。**不要**在这里下"声卡坏了"的结论。
        SinkCheckVerdict::SinkStalled
    } else if loopback_frames == 0 {
        SinkCheckVerdict::LoopbackNoSamples
    } else {
        match (written_rms, loopback_rms) {
            (Some(w), Some(l)) if l > w - LOOPBACK_TOLERANCE_DB => SinkCheckVerdict::Pass,
            _ => SinkCheckVerdict::LoopbackSilent,
        }
    };

    Ok(SinkCheckReport {
        device_name,
        requested_dbfs: dbfs,
        seconds,
        pushed_frames: stats.frames_pushed,
        written_rms_dbfs: written_rms,
        loopback_seconds,
        loopback_rms_dbfs: loopback_rms,
        loopback_peak_dbfs: loopback_peak,
        underruns: stats.underruns,
        verdict,
    })
}

impl SinkCheckReport {
    /// 给用户看的判定文案。**必须**如实说明"静音"的两种可能原因，
    /// 不能替用户断言"声卡没装好"——也可能是没给麦克风权限。
    pub fn verdict_text(&self) -> String {
        match self.verdict {
            SinkCheckVerdict::Pass => format!(
                "声卡直通正常：写入 {:.1} dBFS，回读 {:.1} dBFS",
                self.written_rms_dbfs.unwrap_or(-120.0),
                self.loopback_rms_dbfs.unwrap_or(-120.0)
            ),
            SinkCheckVerdict::LoopbackSilent => format!(
                "回读几乎静音（{}）：要么这块声卡没有直通，要么本 App 没有麦克风权限",
                self.loopback_rms_dbfs
                    .map(|v| format!("{v:.1} dBFS"))
                    .unwrap_or_else(|| "无读数".into())
            ),
            SinkCheckVerdict::LoopbackNoSamples => "输入端一个样本都没读到：最可能是麦克风权限被拒绝".into(),
            SinkCheckVerdict::SinkStalled => format!(
                "写入端自身有 {} 次欠载，回读结论不可采信（先解决喂入问题再验）",
                self.underruns
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 判定是纯函数，不需要真设备就能测边界。这里只测"输入不完整时不能误判成 Pass"。
    fn report(written: Option<f64>, loopback: Option<f64>, frames: u64, underruns: u64) -> SinkCheckReport {
        let verdict = if underruns > 0 && written.unwrap_or(-120.0) > -60.0 {
            SinkCheckVerdict::SinkStalled
        } else if frames == 0 {
            SinkCheckVerdict::LoopbackNoSamples
        } else {
            match (written, loopback) {
                (Some(w), Some(l)) if l > w - LOOPBACK_TOLERANCE_DB => SinkCheckVerdict::Pass,
                _ => SinkCheckVerdict::LoopbackSilent,
            }
        };
        SinkCheckReport {
            device_name: "test".into(),
            requested_dbfs: -20.0,
            seconds: 2,
            pushed_frames: 32_000,
            written_rms_dbfs: written,
            loopback_seconds: if frames == 0 { 0.0 } else { 2.0 },
            loopback_rms_dbfs: loopback,
            loopback_peak_dbfs: loopback,
            underruns,
            verdict,
        }
    }

    #[test]
    fn pass_when_levels_match() {
        assert_eq!(report(Some(-20.0), Some(-20.5), 32_000, 0).verdict, SinkCheckVerdict::Pass);
    }

    #[test]
    fn silent_loopback_fails() {
        assert_eq!(
            report(Some(-20.0), Some(-120.0), 32_000, 0).verdict,
            SinkCheckVerdict::LoopbackSilent
        );
    }

    #[test]
    fn no_samples_is_its_own_verdict() {
        // 权限被拒与"直通断了"要做的事完全不同，不能混成一个。
        assert_eq!(
            report(Some(-20.0), None, 0, 0).verdict,
            SinkCheckVerdict::LoopbackNoSamples
        );
    }

    #[test]
    fn underruns_invalidate_the_loopback_conclusion() {
        assert_eq!(
            report(Some(-20.0), Some(-21.0), 32_000, 7).verdict,
            SinkCheckVerdict::SinkStalled
        );
    }

    #[test]
    fn tolerance_accepts_a_few_db_of_system_volume() {
        // 系统音量压掉 6 dB 仍然算通；压掉 20 dB 就不算。
        assert_eq!(report(Some(-20.0), Some(-26.0), 32_000, 0).verdict, SinkCheckVerdict::Pass);
        assert_eq!(
            report(Some(-20.0), Some(-40.0), 32_000, 0).verdict,
            SinkCheckVerdict::LoopbackSilent
        );
    }

    #[test]
    fn verdict_text_never_claims_a_single_cause_for_silence() {
        let r = report(Some(-20.0), Some(-120.0), 32_000, 0);
        let text = r.verdict_text();
        assert!(text.contains("直通"), "{text}");
        assert!(text.contains("麦克风权限"), "{text}");
    }
}
