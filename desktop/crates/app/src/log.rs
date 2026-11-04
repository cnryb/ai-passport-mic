//! 诊断日志落盘（JSON Lines）。
//!
//! 为什么不用 `tracing`：G0 只需要"出问题能拿到证据"，一行一个 JSON 足够被
//! `jq` 和肉眼同时消费。等 G2 要做采样级别的排查再换，不提前引框架。

use crate::events::LogEntry;
use std::fs::{create_dir_all, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// epoch 毫秒。`SystemTime` 可能回拨，所以它只用于展示与文件名，不用于计时。
pub fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `~/Library/Logs/AI Passport Mic`
pub fn default_log_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Path::new(&home)
        .join("Library")
        .join("Logs")
        .join("AI Passport Mic")
}

pub struct LogSink {
    dir: PathBuf,
    /// 当前写入的文件对应的"天"（自 epoch 起的天数），用来判断是否需要滚动。
    day: i64,
    file: Option<std::fs::File>,
    /// 打不开文件时只提示一次，避免每行日志都刷 stderr。
    warned: bool,
}

impl LogSink {
    pub fn new(dir: Option<PathBuf>) -> Self {
        let dir = dir.unwrap_or_else(default_log_dir);
        Self {
            dir,
            day: -1,
            file: None,
            warned: false,
        }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(format!("app-{}.jsonl", day_stamp(self.day_or_today())))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn day_or_today(&self) -> i64 {
        if self.day >= 0 {
            self.day
        } else {
            (epoch_ms() / 86_400_000) as i64
        }
    }

    pub fn write(&mut self, entry: &LogEntry) {
        let day = (entry.ts_ms / 86_400_000) as i64;
        if self.file.is_none() || day != self.day {
            if create_dir_all(&self.dir).is_err() {
                if !self.warned {
                    eprintln!("诊断日志目录不可写: {}", self.dir.display());
                    self.warned = true;
                }
                return;
            }
            let path = self.dir.join(format!("app-{}.jsonl", day_stamp(day)));
            match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(file) => {
                    self.file = Some(file);
                    self.day = day;
                }
                Err(err) => {
                    if !self.warned {
                        eprintln!("打开诊断日志失败: {} ({err})", path.display());
                        self.warned = true;
                    }
                    return;
                }
            }
        }
        if let Some(file) = self.file.as_mut() {
            if let Ok(line) = serde_json::to_string(entry) {
                // 写日志失败不能影响音频与 UI：吞掉错误，只关掉文件下次重开。
                if writeln!(file, "{line}").is_err() {
                    self.file = None;
                }
            }
        }
    }
}

/// 自 epoch 起的天数 → `YYYYMMDD`。
fn day_stamp(day: i64) -> String {
    let (y, m, d) = ymd_from_days(day);
    format!("{y:04}{m:02}{d:02}")
}

/// 天数 → 公历年月日（Howard Hinnant 的 civil_from_days，无时区、无依赖）。
fn ymd_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_origin_is_1970_01_01() {
        assert_eq!(ymd_from_days(0), (1970, 1, 1));
        assert_eq!(day_stamp(0), "19700101");
    }

    #[test]
    fn known_dates() {
        // 2026-11-04 是自 epoch 起第 20761 天。
        assert_eq!(ymd_from_days(20_761), (2026, 11, 4));
        // 闰日要落在正确月份。
        assert_eq!(ymd_from_days(19_782), (2024, 2, 29));
    }
}
