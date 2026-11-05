//! 音质报告：一次 WAV 的客观指标 + 阈值判定。
//!
//! 从 `crates/cli/src/cmd.rs` 搬过来，因为 GUI 的「音质报告」页要跟
//! `apmctl analyze --json` **逐字段一致** —— 两份实现迟早会漂。
//! 报告里必须永远区分「软件分析通过」与「真机链路通过」，不能合并成一句"音质合格"。

use apm_protocol::glitch::{self, Scan};
use apm_protocol::metrics::{self, Check, Metrics};
use apm_protocol::wav;
use apm_protocol::AUDIO_SAMPLE_RATE;
use serde::Serialize;
use serde_json::{json, Value};
use std::path::Path;

pub const SCHEMA: &str = "ai-passport-mic-quality-report-1";

/// 判定口径写死在报告里，前端只展示，不重新解释。
pub const VERDICT_PASS: &str = "software-pass-hardware-pending";
pub const VERDICT_FAIL: &str = "software-fail";

#[derive(Debug, Clone, Serialize)]
pub struct QualityReport {
    pub recording: String,
    pub sample_rate: u32,
    pub channels: u16,
    /// 多声道被下混过：直接对交错样本算 RMS 会把两个声道混成一个假信号。
    pub downmixed: bool,
    pub duration_s: f64,
    pub samples: usize,
    pub rms_dbfs: Option<f64>,
    pub peak_dbfs: Option<f64>,
    pub clipping_pct: f64,
    pub dc_offset_pct: f64,
    pub zero_crossing_pct: f64,
    pub glitch_events: u64,
    pub glitch_ms: f64,
    /// 每条阈值判定，GUI 表格直接渲染这个。
    pub checks: Vec<ReportCheck>,
    pub pass: bool,
    pub verdict: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportCheck {
    pub name: String,
    pub ok: bool,
    pub actual: String,
    pub expected: String,
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// 读 WAV 并算报告。失败原因原样返回（截断、非 PCM16、不是 RIFF），不猜。
pub fn analyze_file(path: &Path) -> Result<QualityReport, String> {
    let file = wav::read_wav(path).map_err(|e| format!("{}: {e}", path.display()))?;

    let (samples, downmixed) = if file.channels > 1 {
        let ch = file.channels as usize;
        let mono: Vec<i16> = file
            .samples
            .chunks_exact(ch)
            .map(|frame| (frame.iter().map(|&s| s as i32).sum::<i32>() / ch as i32) as i16)
            .collect();
        (mono, true)
    } else {
        (file.samples.clone(), false)
    };

    let m: Metrics = metrics::analyze(&samples, file.sample_rate);
    let checks: Vec<Check> = metrics::evaluate(&m);
    let mut pass = metrics::software_pass(&checks);
    // 满量程毛刺是链路/采集问题，不是文件格式问题：出现了就不能算通过。
    let g: Scan = glitch::scan(&samples);
    if g.events > 0 {
        pass = false;
    }

    Ok(QualityReport {
        recording: path.display().to_string(),
        sample_rate: file.sample_rate,
        channels: file.channels,
        downmixed,
        duration_s: m.duration_s,
        samples: m.samples,
        rms_dbfs: m.rms_dbfs,
        peak_dbfs: m.peak_dbfs,
        clipping_pct: m.clipping_pct,
        dc_offset_pct: m.dc_offset_pct,
        zero_crossing_pct: m.zero_crossing_pct,
        glitch_events: g.events,
        glitch_ms: g.duration_ms(file.sample_rate),
        checks: checks
            .into_iter()
            .map(|c| ReportCheck {
                name: c.name.to_string(),
                ok: c.ok,
                actual: c.actual,
                expected: c.expected.to_string(),
            })
            .collect(),
        pass,
        verdict: if pass { VERDICT_PASS } else { VERDICT_FAIL },
    })
}

impl QualityReport {
    /// 与 `apmctl analyze --json` 完全一致的形状（schema 不变，字段一个不少）。
    /// 未测的字段写 `null`，不猜 —— 这是 `docs/SOUND_QUALITY.md` 的硬要求。
    pub fn to_json(&self) -> Value {
        json!({
            "schema": SCHEMA,
            "firmware": Value::Null,
            "desktop": env!("CARGO_PKG_VERSION"),
            "host": format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
            "adapter": Value::Null,
            "virtual_device": Value::Null,
            "environment": Value::Null,
            "distance_cm": Value::Null,
            "angle_deg": Value::Null,
            "reference_source": Value::Null,
            "recording": self.recording,
            "metrics": {
                "duration_s": round2(self.duration_s),
                "rms_dbfs": self.rms_dbfs.map(round2),
                "peak_dbfs": self.peak_dbfs.map(round2),
                "clipping_pct": round3(self.clipping_pct),
                "dc_offset_pct": round3(self.dc_offset_pct),
                "zero_crossing_pct": round2(self.zero_crossing_pct),
                "glitch_events": self.glitch_events,
                "glitch_ms": round2(self.glitch_ms),
                // 下面三项只能从链路日志得到，WAV 里没有。
                "lost_chunks": Value::Null,
                "startup_latency_p95_ms": Value::Null,
                "steady_latency_p95_ms": Value::Null,
            },
            "subjective": Value::Null,
            "verdict": self.verdict,
        })
    }

    /// 给人看的一行结论，CLI 与 GUI 共用同一句，避免措辞漂移。
    pub fn headline(&self) -> &'static str {
        if self.pass {
            "软件分析通过（software-pass-hardware-pending）"
        } else {
            "软件分析不通过（software-fail）"
        }
    }
}

/// 报告里能直接看到的一条结论：采样率不是 16 kHz 时提醒对比口径。
pub fn rate_note(sample_rate: u32) -> Option<String> {
    if sample_rate == AUDIO_SAMPLE_RATE {
        None
    } else {
        Some(format!(
            "采样率不是 {AUDIO_SAMPLE_RATE} Hz。指标本身与采样率无关，但与固件链路对比时要留意。"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_sine(path: &Path, db: f64, seconds: f64) {
        let amp = 32768.0 * 10f64.powf(db / 20.0);
        let rate = AUDIO_SAMPLE_RATE;
        let n = (rate as f64 * seconds) as usize;
        let samples: Vec<i16> = (0..n)
            .map(|i| {
                let t = i as f64 / rate as f64;
                (amp * (2.0 * std::f64::consts::PI * 440.0 * t).sin()).round() as i16
            })
            .collect();
        wav::write_wav(path, rate, 1, &samples).unwrap();
    }

    #[test]
    fn sine_between_thresholds_passes_software_checks() {
        let dir = std::env::temp_dir().join("apm-app-report");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sine.wav");
        write_sine(&path, -20.0, 11.0);

        let r = analyze_file(&path).unwrap();
        assert!(r.pass, "checks={:?}", r.checks);
        assert_eq!(r.verdict, VERDICT_PASS);
        assert!(r.glitch_events == 0);
        let j = r.to_json();
        assert_eq!(j["schema"], SCHEMA);
        assert!(j["metrics"]["lost_chunks"].is_null(), "未测字段必须 null");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn silence_fails() {
        let dir = std::env::temp_dir().join("apm-app-report");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("silence.wav");
        wav::write_wav(&path, AUDIO_SAMPLE_RATE, 1, &vec![0i16; 160_000]).unwrap();

        let r = analyze_file(&path).unwrap();
        assert!(!r.pass);
        assert_eq!(r.verdict, VERDICT_FAIL);
        assert_eq!(r.rms_dbfs, None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_reports_path() {
        let err = analyze_file(Path::new("/nonexistent/x.wav")).unwrap_err();
        assert!(err.contains("/nonexistent/x.wav"), "{err}");
    }
}
