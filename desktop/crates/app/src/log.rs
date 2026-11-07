//! 诊断日志落盘（JSON Lines）。
//!
//! 为什么不用 `tracing`：G0 只需要"出问题能拿到证据"，一行一个 JSON 足够被
//! `jq` 和肉眼同时消费。等 G2 要做采样级别的排查再换，不提前引框架。
//!
//! ## 为什么写盘必须离开调用方线程（2026-11-07）
//!
//! 曾经 `record()` 是**同步**写一个裸 `File`（无缓冲），而调用它的
//! `AppCore::handle_notification` 与音频块解码**在同一个 tokio 任务里**。设备每秒发一条
//! STATUS 心跳，于是每秒至少一次 `write(2)` 压在音频消费路径上。APFS 的长尾
//! （元数据更新、快照、Spotlight）足以做出几十毫秒的停顿 —— 而这正好落在
//! `Pipeline` 的"块到达间隔"计时窗口里。
//!
//! 后果不只是卡顿，而是**把仪器修坏了**：`note_arrival` 记到的"到达间隔"会包含我们
//! 自己写日志花掉的时间，于是"链路抖动"这个根因结论无法与"PC 端自己停顿"区分开
//! （见 `docs/KNOWN_ISSUES.md` KI-006 的 2026-11-07 复核）。
//!
//! 现在的口径：**`record()` 只做一次内存入队，绝不碰磁盘**。落盘由一条命名写线程
//! （`apm-log`）完成，队列有界（[`LOG_QUEUE`]），满了就丢并计数 ——
//! 与 `session.rs` 的 PCM channel 同一优先级取向：丢日志比卡音频线程重要。

use crate::events::LogEntry;
use std::fs::{create_dir_all, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

/// 待落盘队列容量（行）。日志量在 1–5 行/秒量级，4096 行足够扛住磁盘短暂卡顿；
/// 真的顶满说明磁盘已经长期写不动，这时候丢日志比让调用方阻塞正确得多。
const LOG_QUEUE: usize = 4096;

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

/// 写线程收到的消息。
enum Msg {
    Entry(LogEntry),
    /// 屏障：写线程处理到这里时回 ack。调用方据此确认"此前的日志都已落盘"。
    Barrier(SyncSender<()>),
    Stop,
}

/// 日志落盘。`write()` 永不阻塞在磁盘上。
pub struct LogSink {
    dir: PathBuf,
    inner: Inner,
}

enum Inner {
    /// 正常路径：一条命名写线程独占文件。
    Thread {
        tx: SyncSender<Msg>,
        dropped: Arc<AtomicU64>,
        join: Option<JoinHandle<()>>,
    },
    /// 写线程起不来（极罕见）时的降级：退回同步写。宁可慢，也不能没有日志。
    Direct(FileWriter),
}

impl LogSink {
    pub fn new(dir: Option<PathBuf>) -> Self {
        let dir = dir.unwrap_or_else(default_log_dir);
        match Self::spawn(dir.clone()) {
            Some(inner) => Self { dir, inner },
            None => Self {
                dir: dir.clone(),
                inner: Inner::Direct(FileWriter::new(dir)),
            },
        }
    }

    fn spawn(dir: PathBuf) -> Option<Inner> {
        let (tx, rx) = sync_channel::<Msg>(LOG_QUEUE);
        let dropped = Arc::new(AtomicU64::new(0));
        let worker_dir = dir.clone();
        let join = thread::Builder::new()
            .name("apm-log".into())
            .spawn(move || writer_loop(&worker_dir, rx))
            .ok()?;
        Some(Inner::Thread {
            tx,
            dropped,
            join: Some(join),
        })
    }

    /// 当前（今天）的日志文件路径。只是拼路径，不保证文件已存在。
    pub fn path(&self) -> PathBuf {
        let day = (epoch_ms() / 86_400_000) as i64;
        self.dir.join(format!("app-{}.jsonl", day_stamp(day)))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 入队一行日志。**不碰磁盘**：正常路径只有一次 `try_send`。
    pub fn write(&mut self, entry: &LogEntry) {
        match &mut self.inner {
            Inner::Thread { tx, dropped, .. } => match tx.try_send(Msg::Entry(entry.clone())) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                    dropped.fetch_add(1, Ordering::Relaxed);
                }
            },
            Inner::Direct(w) => w.write(entry),
        }
    }

    /// 等队列排空并落盘。测试与"退出前确保写完"用；正常路径不需要调。
    pub fn flush(&mut self) {
        match &mut self.inner {
            Inner::Thread { tx, .. } => {
                let (ack_tx, ack_rx) = sync_channel(1);
                // 这里用阻塞 send：队列满只说明磁盘卡了很久，等一次比丢掉屏障更合理。
                // 写线程已经退出时 send 会立刻 Err，不会挂住。
                if tx.send(Msg::Barrier(ack_tx)).is_ok() {
                    let _ = ack_rx.recv();
                }
            }
            Inner::Direct(w) => w.flush(),
        }
    }

    /// 因队列满而丢弃的行数。诊断包里用它说明"日志可能不全"。
    pub fn dropped_lines(&self) -> u64 {
        match &self.inner {
            Inner::Thread { dropped, .. } => dropped.load(Ordering::Relaxed),
            Inner::Direct(_) => 0,
        }
    }
}

impl Drop for LogSink {
    fn drop(&mut self) {
        if let Inner::Thread { tx, dropped, join } = &mut self.inner {
            let n = dropped.swap(0, Ordering::Relaxed);
            if n > 0 {
                eprintln!("诊断日志队列曾满，丢弃 {n} 行（磁盘写不过来）");
            }
            let _ = tx.send(Msg::Stop);
            if let Some(h) = join.take() {
                let _ = h.join();
            }
        }
    }
}

/// 写线程主体：独占文件句柄，按天滚动。
fn writer_loop(dir: &Path, rx: Receiver<Msg>) {
    let mut w = FileWriter::new(dir.to_path_buf());
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Entry(entry) => w.write(&entry),
            Msg::Barrier(ack) => {
                w.flush();
                let _ = ack.send(());
            }
            Msg::Stop => break,
        }
    }
    // 退出前把最后几行刷出去：进程正常关闭时不该丢尾巴。
    w.flush();
}

/// 真正的文件写入者。只在写线程里跑（或降级路径的调用方线程里）。
struct FileWriter {
    dir: PathBuf,
    /// 当前写入的文件对应的"天"（自 epoch 起的天数），用来判断是否需要滚动。
    day: i64,
    file: Option<File>,
    /// 打不开文件时只提示一次，避免每行日志都刷 stderr。
    warned: bool,
}

impl FileWriter {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            day: -1,
            file: None,
            warned: false,
        }
    }

    fn write(&mut self, entry: &LogEntry) {
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
                // 写日志失败不能影响任何调用方：关掉文件，下一行再重开。
                if writeln!(file, "{line}").is_err() {
                    self.file = None;
                }
            }
        }
    }

    fn flush(&mut self) {
        if let Some(file) = self.file.as_mut() {
            let _ = file.flush();
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
    use crate::events::LogLevel;

    fn entry(msg: &str) -> LogEntry {
        LogEntry {
            ts_ms: epoch_ms(),
            level: LogLevel::Info,
            source: "test".into(),
            message: msg.into(),
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("apm-log-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

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

    /// 落盘是异步的，但 `flush()` 之后必须一行不少、顺序不变。
    #[test]
    fn writes_are_ordered_and_flushable() {
        let dir = temp_dir("order");
        let mut sink = LogSink::new(Some(dir.clone()));
        for i in 0..100 {
            sink.write(&entry(&format!("line {i}")));
        }
        sink.flush();
        let text = std::fs::read_to_string(sink.path()).expect("日志文件应存在");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 100, "行数不对：{}", lines.len());
        assert!(lines[0].contains("line 0"), "{}", lines[0]);
        assert!(lines[99].contains("line 99"), "{}", lines[99]);
        assert!(lines.iter().all(|l| l.starts_with('{')), "不是 JSONL");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 关键契约：`write()` 不得阻塞在磁盘上。
    ///
    /// 断言方式：把队列灌到远超容量的行数，`write()` 的累计耗时仍然必须是毫秒级。
    /// 同步写 8000 行 JSON 到磁盘要几百毫秒，这里给 50 ms 的宽松上限 —— 它拦住的是
    /// "又退回去同步写"，不是去卡 CI 的抖动。
    #[test]
    fn write_does_not_block_on_disk() {
        let dir = temp_dir("nonblocking");
        let mut sink = LogSink::new(Some(dir.clone()));
        let started = std::time::Instant::now();
        for i in 0..(LOG_QUEUE * 2) {
            sink.write(&entry(&format!("burst {i}")));
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "record() 在磁盘上阻塞了 {elapsed:?}：写盘必须离开调用方线程"
        );
        // 队列满时丢弃要计数，不能静默。
        assert!(sink.dropped_lines() > 0, "队列满了却没计数");
        sink.flush();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 没 `flush` 就 drop，也要把已经入队的行写完（进程正常退出的形状）。
    #[test]
    fn drop_flushes_pending_lines() {
        let dir = temp_dir("drop");
        let path;
        {
            let mut sink = LogSink::new(Some(dir.clone()));
            path = sink.path();
            for i in 0..20 {
                sink.write(&entry(&format!("tail {i}")));
            }
        }
        let text = std::fs::read_to_string(&path).expect("drop 之后日志应已落盘");
        assert_eq!(text.lines().count(), 20);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
