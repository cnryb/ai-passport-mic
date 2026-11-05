//! 采集会话：解码后的 PCM 进声卡 / 落 WAV，并聚合指标。
//!
//! 为什么单独开一个**标准线程**而不是 tokio 任务：
//!
//! `cpal::Stream` 在 0.15 里是 `!Send`（`platform::NotSendSyncAcrossAllPlatforms`）。
//! 把它放进 `AppCore`（`Arc` 里、要被 tokio 派生）根本编译不过。所以声卡对象只能在
//! 创建它的线程里活一辈子：本模块用一条命名线程持有 `Sink` + WAV 写入器，
//! 主线程只通过有界 channel 喂 PCM、通过 `Meter` 读指标。
//!
//! 顺带的好处是磁盘 IO 与声卡写入都不占 tokio 的 worker，不会饿到 BLE 通知循环。
//!
//! 契约（`docs/GUI.md` §3 / §7）：
//! - channel 有界（32 块 = 640 ms）。满了就丢并计数，**绝不阻塞**采集线程。
//! - WAV 默认不落盘；落盘时也不参与音频路径（先 push 声卡，再写文件）。
//! - 收尾顺序：先停写声卡（`clear`）再返回 summary，避免尾巴上多出欠载。

use crate::state::{AppError, ErrorCode};
use apm_protocol::glitch;
use apm_protocol::AUDIO_SAMPLE_RATE;
use serde::Serialize;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Instant;

/// 音频块通道容量。32 块 ≈ 640 ms：真满了说明声卡线程已经卡死，
/// 这时候丢新的比丢旧的更安全（旧数据至少是连续的）。
const PCM_CHANNEL: usize = 32;
/// 峰值保持时长与释放速率（`docs/GUI.md` §7）。
const PEAK_HOLD_S: f64 = 1.5;
const PEAK_RELEASE_DB_PER_S: f32 = 12.0;
/// 全静音时把 RMS 夹到这个值，避免 -inf 进 JSON/界面。
const FLOOR_DBFS: f64 = -90.0;

// ---------------------------------------------------------------------------
// 指标
// ---------------------------------------------------------------------------

/// 一次会话的实时指标。`docs/GUI.md` §6 的 `telemetry` 载荷。
#[derive(Debug, Clone, Default, Serialize)]
pub struct Telemetry {
    /// 当前窗口 RMS；全静音为 `None`（不伪造成 -90）。
    pub rms_dbfs: Option<f64>,
    /// 峰值（带 1.5 s 保持 + 12 dB/s 释放）。
    pub peak_dbfs: Option<f64>,
    /// 声卡侧可用样本折合的毫秒数，**不是** channel 深度。
    pub buffer_ms: f32,
    /// 当前抖动缓冲目标（会自适应）。
    pub target_ms: u32,
    pub blocks: u64,
    /// 序号跳变推算出的缺失块数。
    pub lost_blocks: u32,
    /// 发生跳变的次数（一次跳变可能丢多块）。
    pub seq_jumps: u32,
    /// 设备 STATUS 上报的发送阶段丢弃，与 `lost_blocks` 分开显示。
    pub device_dropped: u16,
    pub underruns: u64,
    pub silent_frames: u64,
    pub lock_misses: u64,
    pub bytes_written: u64,
    pub elapsed_ms: u64,
    /// 长期漂移估计（ppm）。正值 = 设备时钟比声卡快。
    pub ppm: f64,
    pub glitch_events: u64,
    /// 因通道满而丢掉的块（不是设备丢的）。
    pub overflow_drops: u64,
    /// 长度不为 165 的块，通常意味着 MTU 不足被截断。
    pub bad_len_blocks: u32,
}

#[derive(Debug)]
struct MeterState {
    /// 当前窗口（5 Hz）的平方和与帧数，取过快照即清零。
    sum_sq: f64,
    frames: u64,
    /// 整段会话累计，用于"写入侧 RMS"（回环探针对比用）。
    total_sum_sq: f64,
    total_frames: u64,
    peak_hold_db: f32,
    peak_hold_at: f64,
    buffer_ms: f32,
    target_ms: u32,
    ppm: f64,
    underruns: u64,
    silent_frames: u64,
    lock_misses: u64,
    bytes_written: u64,
    overflow: u64,
    glitch_events: u64,
    glitch_muted: u64,
}

/// 声卡侧指标的一次快照。抽成结构体是为了让 `publish_sink` 的参数不失控，
/// 也避免调用方把 buffer_ms 与 target_ms 传反。
#[derive(Debug, Clone, Copy, Default)]
struct SinkSnapshot {
    buffer_ms: f32,
    target_ms: u32,
    ppm: f64,
    underruns: u64,
    silent_frames: u64,
    lock_misses: u64,
    bytes_written: u64,
}

/// 声卡线程与聚合线程共用的指标盒。锁只在微秒级持有，不跨任何 IO。
#[derive(Debug)]
pub struct Meter {
    inner: Mutex<MeterState>,
}

impl Meter {
    fn new(target_ms: u32) -> Self {
        Self {
            inner: Mutex::new(MeterState {
                sum_sq: 0.0,
                frames: 0,
                total_sum_sq: 0.0,
                total_frames: 0,
                peak_hold_db: FLOOR_DBFS as f32,
                peak_hold_at: 0.0,
                buffer_ms: 0.0,
                target_ms,
                ppm: 0.0,
                underruns: 0,
                silent_frames: 0,
                lock_misses: 0,
                bytes_written: 0,
                overflow: 0,
                glitch_events: 0,
                glitch_muted: 0,
            }),
        }
    }

    /// 采到一段 PCM：累加 RMS 窗口、更新峰值保持。
    fn observe(&self, pcm: &[i16], now_s: f64) {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        for &s in pcm {
            let x = s as f64;
            g.sum_sq += x * x;
            g.frames += 1;
            g.total_sum_sq += x * x;
            g.total_frames += 1;
        }
        if let Some(peak) = pcm.iter().map(|s| (*s as i32).abs()).max() {
            if peak > 0 {
                let db = 20.0 * (peak as f64 / 32768.0).log10();
                if db as f32 > g.peak_hold_db {
                    g.peak_hold_db = db as f32;
                    g.peak_hold_at = now_s;
                }
            }
        }
    }

    /// 取一次快照：算窗口 RMS、按规则释放峰值、重置窗口。
    fn sample(&self, now_s: f64) -> (Option<f64>, Option<f64>) {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        let rms = if g.frames == 0 {
            None
        } else {
            let mean = g.sum_sq / g.frames as f64;
            g.sum_sq = 0.0;
            g.frames = 0;
            if mean <= 0.0 {
                None
            } else {
                Some((20.0 * (mean.sqrt() / 32768.0).log10()).max(FLOOR_DBFS))
            }
        };

        // 保持 1.5 s，之后按 12 dB/s 掉，掉到地板就归 None。
        let elapsed = now_s - g.peak_hold_at;
        let peak = if elapsed <= PEAK_HOLD_S {
            Some(g.peak_hold_db as f64)
        } else {
            let db = g.peak_hold_db - PEAK_RELEASE_DB_PER_S * (elapsed - PEAK_HOLD_S) as f32;
            if db <= FLOOR_DBFS as f32 {
                None
            } else {
                Some(db as f64)
            }
        };
        (rms, peak)
    }

    fn note_overflow(&self) {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        g.overflow += 1;
    }

    /// 由声卡线程每 100 ms 刷新一次：这些数只能从 `Sink` 里读，读一次要抢它的锁，
    /// 所以不放到 5 Hz 的聚合里做。
    fn publish_sink(&self, snap: SinkSnapshot) {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        g.buffer_ms = snap.buffer_ms;
        g.target_ms = snap.target_ms;
        g.ppm = snap.ppm;
        g.underruns = snap.underruns;
        g.silent_frames = snap.silent_frames;
        g.lock_misses = snap.lock_misses;
        g.bytes_written = snap.bytes_written;
    }

    fn note_glitch(&self, events: u64, muted_samples: u64) {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        g.glitch_events = events;
        g.glitch_muted = muted_samples;
    }

    /// 组装快照。计数器（块数/丢块/跳变）由调用方填，它们不是声卡侧的数据。
    pub fn snapshot(&self, now_s: f64) -> (Option<f64>, Option<f64>, Telemetry) {
        let (rms, peak) = self.sample(now_s);
        let g = match self.inner.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        let t = Telemetry {
            rms_dbfs: rms,
            peak_dbfs: peak,
            buffer_ms: g.buffer_ms,
            target_ms: g.target_ms,
            underruns: g.underruns,
            silent_frames: g.silent_frames,
            lock_misses: g.lock_misses,
            bytes_written: g.bytes_written,
            ppm: g.ppm,
            glitch_events: g.glitch_events,
            overflow_drops: g.overflow,
            ..Telemetry::default()
        };
        (rms, peak, t)
    }

    /// 整段会话的写入侧 RMS（dBFS）。没有样本时 `None`。
    /// 回环验证要用它跟"从声卡输入端回读到的电平"比，不能用单个窗口的 RMS。
    fn written_rms(&self) -> Option<f64> {
        let g = match self.inner.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        if g.total_frames == 0 {
            return None;
        }
        let mean = g.total_sum_sq / g.total_frames as f64;
        if mean <= 0.0 {
            Some(-120.0)
        } else {
            Some((20.0 * (mean.sqrt() / 32768.0).log10()).max(-120.0))
        }
    }

    fn counters(&self) -> (u64, u64) {
        let g = match self.inner.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        (g.glitch_events, g.glitch_muted)
    }
}

// ---------------------------------------------------------------------------
// 会话
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SessionOptions {
    pub session_id: String,
    /// 目标声卡名（子串匹配）。`None` = 只落 WAV，不推声卡。
    pub sink: Option<String>,
    /// 抖动缓冲目标（40–80 ms），运行期可由 [`SessionHandle::retarget`] 改。
    pub target_ms: u32,
    /// 会话 WAV 路径。`None` = 不落盘（默认，见 `docs/GUI.md` §12 第 3 条）。
    pub wav_path: Option<PathBuf>,
}

/// 声卡打开成功后回给调用方的信息（也用于日志）。
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub sink_device: Option<String>,
    pub rate: u32,
    pub channels: u16,
    pub target_ms: u32,
    pub wav_path: Option<String>,
}

/// 一次会话的收尾结果。块数/丢块由 [`crate::pipeline::Pipeline`] 补齐。
#[derive(Debug, Clone, Default, Serialize)]
pub struct SessionSummary {
    pub session_id: String,
    /// 收到的音频块数（不含长度异常被拒的）。
    pub blocks: u64,
    /// 推入声卡/写进 WAV 的采样帧数。
    pub frames: u64,
    pub duration_s: f64,
    pub sink_device: Option<String>,
    pub wav_path: Option<String>,
    pub bytes_written: u64,
    pub underruns: u64,
    pub silent_frames: u64,
    pub frames_dropped: u64,
    pub lock_misses: u64,
    pub ppm: f64,
    pub frames_inserted: u64,
    pub frames_skipped: u64,
    pub target_start_ms: u32,
    pub target_end_ms: u32,
    pub glitch_events: u64,
    pub glitch_muted_ms: f64,
    pub lost_blocks: u32,
    pub seq_jumps: u32,
    pub device_dropped: u16,
    pub bad_len_blocks: u32,
    pub overflow_drops: u64,
}

enum Msg {
    Pcm(Vec<i16>),
    Retarget(u32),
    Stop,
}

/// 主线程握着的句柄。`Drop` 时若还没 `finish`，会自动发 stop 并等线程收尾，
/// 避免 GUI 退出时留下一条还在往声卡写静音的线程。
pub struct SessionHandle {
    tx: SyncSender<Msg>,
    meter: Arc<Meter>,
    join: Option<JoinHandle<WorkerOutcome>>,
    info: SessionInfo,
    started: Instant,
    /// 已确认结束（`finish` 调用过）；`Drop` 里不再重复 join。
    finished: Mutex<bool>,
}

#[derive(Debug, Default)]
struct WorkerOutcome {
    frames: u64,
    bytes_written: u64,
    underruns: u64,
    silent_frames: u64,
    frames_dropped: u64,
    lock_misses: u64,
    ppm: f64,
    frames_inserted: u64,
    frames_skipped: u64,
    target_start_ms: u32,
    target_end_ms: u32,
    glitch_events: u64,
    glitch_muted: u64,
}

impl SessionHandle {
    pub fn info(&self) -> &SessionInfo {
        &self.info
    }

    pub fn elapsed_s(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    /// 喂一块**已解码**的 PCM。满了就丢并计数，永不阻塞。
    pub fn push(&self, pcm: &[i16]) {
        match self.tx.try_send(Msg::Pcm(pcm.to_vec())) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => self.meter.note_overflow(),
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// 运行期改缓冲目标，取下一批回调生效。
    pub fn retarget(&self, ms: u32) {
        let _ = self.tx.try_send(Msg::Retarget(ms));
    }

    pub fn meter(&self) -> &Arc<Meter> {
        &self.meter
    }

    pub fn counters(&self) -> (u64, u64) {
        self.meter.counters()
    }

    /// 写入侧 RMS（dBFS），用于回环电平对比。
    pub fn written_rms(&self) -> Option<f64> {
        self.meter.written_rms()
    }

    /// 收尾并返回统计。会阻塞到声卡线程退出（正常 < 50 ms）。
    pub fn finish(mut self) -> SessionSummary {
        let outcome = self.stop_worker();
        let (glitch_events, glitch_muted) = outcome
            .as_ref()
            .map(|o| (o.glitch_events, o.glitch_muted))
            .unwrap_or((0, 0));
        let o = outcome.unwrap_or_default();
        SessionSummary {
            session_id: self.info.session_id.clone(),
            frames: o.frames,
            duration_s: if self.info.rate == 0 {
                0.0
            } else {
                o.frames as f64 / self.info.rate as f64
            },
            sink_device: self.info.sink_device.clone(),
            wav_path: self.info.wav_path.clone(),
            bytes_written: o.bytes_written,
            underruns: o.underruns,
            silent_frames: o.silent_frames,
            frames_dropped: o.frames_dropped,
            lock_misses: o.lock_misses,
            ppm: o.ppm,
            frames_inserted: o.frames_inserted,
            frames_skipped: o.frames_skipped,
            target_start_ms: o.target_start_ms,
            target_end_ms: o.target_end_ms,
            glitch_events,
            glitch_muted_ms: if self.info.rate == 0 {
                0.0
            } else {
                glitch_muted as f64 * 1000.0 / self.info.rate as f64
            },
            ..SessionSummary::default()
        }
    }

    fn stop_worker(&mut self) -> Option<WorkerOutcome> {
        if *self.finished.lock().unwrap_or_else(|e| e.into_inner()) {
            return None;
        }
        *self.finished.lock().unwrap_or_else(|e| e.into_inner()) = true;
        let _ = self.tx.send(Msg::Stop);
        self.join.take().and_then(|h| h.join().ok())
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        // finish 没被调用（异常路径）时也要把线程收干净，否则声卡会继续被写静音。
        let _ = self.stop_worker();
    }
}

/// 开一条会话。**返回成功即代表声卡已经打开**（错误在开线程阶段同步回传），
/// 所以界面可以在 start_stream 的返回值上直接报 `sink_missing`。
pub fn start(opts: SessionOptions) -> Result<SessionHandle, AppError> {
    let target_ms = opts.target_ms.clamp(40, 80);
    let meter = Arc::new(Meter::new(target_ms));
    let (tx, rx) = sync_channel::<Msg>(PCM_CHANNEL);
    let (ready_tx, ready_rx) = sync_channel::<Result<SessionInfo, AppError>>(1);

    let meter_thread = Arc::clone(&meter);
    let opts_thread = opts.clone();
    let join = thread::Builder::new()
        .name("apm-sink".into())
        .spawn(move || worker(rx, meter_thread, opts_thread, ready_tx))
        .map_err(|e| AppError::internal(format!("创建声卡线程失败：{e}")))?;

    match ready_rx.recv() {
        Ok(Ok(info)) => Ok(SessionHandle {
            tx,
            meter,
            join: Some(join),
            info,
            started: Instant::now(),
            finished: Mutex::new(false),
        }),
        Ok(Err(err)) => {
            let _ = join.join();
            Err(err)
        }
        Err(_) => {
            let _ = join.join();
            Err(AppError::internal("声卡线程启动后没有回报状态"))
        }
    }
}

/// 声卡线程主体。返回时保证：声卡已 `clear`、WAV 头已回填。
fn worker(
    rx: Receiver<Msg>,
    meter: Arc<Meter>,
    opts: SessionOptions,
    ready: SyncSender<Result<SessionInfo, AppError>>,
) -> WorkerOutcome {
    let mut sink = match &opts.sink {
        Some(name) => match apm_sink::Sink::open(name, AUDIO_SAMPLE_RATE, opts.target_ms) {
            Ok(s) => Some(s),
            Err(err) => {
                let _ = ready.send(Err(classify_sink_error(&err.to_string())));
                return WorkerOutcome::default();
            }
        },
        None => None,
    };

    let mut wav = match opts.wav_path.as_deref().map(WavWriter::create).transpose() {
        Ok(w) => w,
        Err(err) => {
            let _ = ready.send(Err(retarget_wav_error(&opts, &err)));
            return WorkerOutcome::default();
        }
    };

    let info = SessionInfo {
        session_id: opts.session_id.clone(),
        sink_device: sink.as_ref().map(|s| s.device_name().to_string()),
        rate: sink.as_ref().map(|s| s.rate()).unwrap_or(AUDIO_SAMPLE_RATE),
        channels: sink.as_ref().map(|s| s.channels()).unwrap_or(1),
        target_ms: sink.as_ref().map(|s| s.target_ms()).unwrap_or(opts.target_ms),
        wav_path: opts.wav_path.as_ref().map(|p| p.display().to_string()),
    };
    let target_start_ms = info.target_ms;
    let _ = ready.send(Ok(info.clone()));

    let mut suppressor = glitch::Suppressor::new();
    let mut frames = 0u64;
    let mut ticks = 0u32;

    loop {
        match rx.recv() {
            Ok(Msg::Pcm(mut pcm)) => {
                // 先压毛刺再进声卡/文件：垃圾数据不该被会议软件听到，也不该进音质报告。
                suppressor.process(&mut pcm);
                meter.observe(&pcm, 0.0);
                if let Some(w) = wav.as_mut() {
                    if let Err(err) = w.write(&pcm) {
                        // 磁盘问题只报错，不阻塞音频（`docs/GUI.md` §11）。
                        eprintln!("WAV 写入失败（继续推声卡）：{err}");
                        wav = None;
                    }
                }
                if let Some(s) = sink.as_mut() {
                    s.push(&pcm);
                }
                frames += pcm.len() as u64;
                ticks += 1;
                if ticks.is_multiple_of(5) {
                    meter.note_glitch(suppressor.events, suppressor.muted_samples);
                    if let Some(s) = sink.as_ref() {
                        let st = s.stats();
                        meter.publish_sink(SinkSnapshot {
                            buffer_ms: s.fill_ms(),
                            target_ms: s.target_ms(),
                            ppm: s.ppm(),
                            underruns: st.underruns,
                            silent_frames: st.frames_silenced,
                            lock_misses: s.lock_misses(),
                            bytes_written: frames * 2,
                        });
                    }
                }
            }
            Ok(Msg::Retarget(ms)) => {
                if let Some(s) = sink.as_mut() {
                    s.retarget(ms);
                }
            }
            Ok(Msg::Stop) | Err(_) => break,
        }
    }

    if let Some(w) = wav.as_mut() {
        if let Err(err) = w.finalize() {
            eprintln!("WAV 收尾失败：{err}");
        }
    }

    let mut outcome = WorkerOutcome {
        frames,
        glitch_events: suppressor.events,
        glitch_muted: suppressor.muted_samples,
        target_start_ms,
        ..WorkerOutcome::default()
    };
    if let Some(mut s) = sink {
        // 先停写再读统计：clear 之后声卡不再消费，统计才是最终值。
        s.clear();
        let st = s.stats();
        outcome.underruns = st.underruns;
        outcome.silent_frames = st.frames_silenced;
        outcome.frames_dropped = st.frames_dropped;
        outcome.lock_misses = s.lock_misses();
        outcome.ppm = s.ppm();
        outcome.frames_inserted = st.frames_inserted;
        outcome.frames_skipped = st.frames_skipped;
        outcome.target_end_ms = s.target_ms();
    } else {
        outcome.target_end_ms = target_start_ms;
    }
    if let Some(w) = wav {
        outcome.bytes_written = w.bytes_written();
    }
    outcome
}

/// 声卡错误 → 固定错误码。靠固定句式匹配，与本 crate 其它分类一致。
pub fn classify_sink_error(text: &str) -> AppError {
    // 采样率不支持与"设备不见了"要分开：前者换声卡就能解决，后者要去查 BlackHole。
    // 其余失败（被独占、格式不支持）统一归 sink_missing —— 用户要做的事是一样的。
    if text.contains("不支持") && text.contains("采样率") {
        AppError::new(ErrorCode::SinkRateUnsupported, text.to_string())
    } else {
        AppError::new(ErrorCode::SinkMissing, text.to_string())
    }
}

fn retarget_wav_error(opts: &SessionOptions, err: &std::io::Error) -> AppError {
    AppError::internal(format!(
        "创建会话文件失败（{:?}）：{err}",
        opts.wav_path.as_deref().unwrap_or(Path::new(""))
    ))
}

// ---------------------------------------------------------------------------
// WAV 增量写
// ---------------------------------------------------------------------------

/// 边录边写：先占 44 字节头，收尾时回填长度。
///
/// 不用 `apm_protocol::wav::write_wav`（那需要整段样本在内存里），因为一次会话
/// 可能是几十分钟：16 kHz × 2 B/s ≈ 32 KB/s，10 分钟就 19 MB，攒在内存里没有意义。
struct WavWriter {
    file: BufWriter<File>,
    data_bytes: u64,
    path: PathBuf,
}

impl WavWriter {
    fn create(path: &Path) -> std::io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = File::create(path)?;
        let mut w = Self {
            file: BufWriter::new(file),
            data_bytes: 0,
            path: path.to_path_buf(),
        };
        w.write_header()?;
        Ok(w)
    }

    fn write_header(&mut self) -> std::io::Result<()> {
        let data_len = self.data_bytes as u32;
        let rate = AUDIO_SAMPLE_RATE;
        let channels = 1u16;
        let mut head = Vec::with_capacity(44);
        head.extend_from_slice(b"RIFF");
        head.extend_from_slice(&(36 + data_len).to_le_bytes());
        head.extend_from_slice(b"WAVE");
        head.extend_from_slice(b"fmt ");
        head.extend_from_slice(&16u32.to_le_bytes());
        head.extend_from_slice(&1u16.to_le_bytes()); // PCM
        head.extend_from_slice(&channels.to_le_bytes());
        head.extend_from_slice(&rate.to_le_bytes());
        head.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
        head.extend_from_slice(&(channels * 2).to_le_bytes());
        head.extend_from_slice(&16u16.to_le_bytes());
        head.extend_from_slice(b"data");
        head.extend_from_slice(&data_len.to_le_bytes());
        self.file.write_all(&head)
    }

    fn write(&mut self, pcm: &[i16]) -> std::io::Result<()> {
        for s in pcm {
            self.file.write_all(&s.to_le_bytes())?;
        }
        self.data_bytes += (pcm.len() * 2) as u64;
        Ok(())
    }

    fn finalize(&mut self) -> std::io::Result<()> {
        self.file.flush()?;
        let mut file = self.file.get_ref();
        file.seek(SeekFrom::Start(0))?;
        let data_len = self.data_bytes as u32;
        let rate = AUDIO_SAMPLE_RATE;
        let mut head = Vec::with_capacity(44);
        head.extend_from_slice(b"RIFF");
        head.extend_from_slice(&(36 + data_len).to_le_bytes());
        head.extend_from_slice(b"WAVE");
        head.extend_from_slice(b"fmt ");
        head.extend_from_slice(&16u32.to_le_bytes());
        head.extend_from_slice(&1u16.to_le_bytes());
        head.extend_from_slice(&1u16.to_le_bytes());
        head.extend_from_slice(&rate.to_le_bytes());
        head.extend_from_slice(&(rate * 2).to_le_bytes());
        head.extend_from_slice(&2u16.to_le_bytes());
        head.extend_from_slice(&16u16.to_le_bytes());
        head.extend_from_slice(b"data");
        head.extend_from_slice(&data_len.to_le_bytes());
        file.write_all(&head)?;
        file.flush()?;
        Ok(())
    }

    fn bytes_written(&self) -> u64 {
        self.data_bytes
    }
}

impl Drop for WavWriter {
    fn drop(&mut self) {
        // 异常路径也要让文件可读：finalize 幂等（只是重写头）。
        if let Err(err) = self.finalize() {
            eprintln!("WAV 收尾失败（{:?}）：{err}", self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_tracks_rms_and_releases_peak() {
        let m = Meter::new(80);
        let pcm = vec![16_000i16; 3200]; // 100 ms
        m.observe(&pcm, 0.0);
        let (rms, peak, t) = m.snapshot(0.1);
        assert!(rms.unwrap() > -10.0 && rms.unwrap() < 0.0, "{rms:?}");
        assert!(peak.unwrap() > -10.0, "{peak:?}");
        assert_eq!(t.target_ms, 80);

        // 保持 1.5 s 后开始按 12 dB/s 释放；10 s 后应归零。
        m.observe(&vec![0i16; 320], 1.6);
        let (_rms2, peak2, _) = m.snapshot(10.0);
        assert!(peak2.is_none(), "峰值没有释放：{peak2:?}");
    }

    /// 有声卡时跑一遍真实写入路径。
    ///
    /// **默认 `#[ignore]`**：这条只有在装了 BlackHole 之类虚拟声卡的机器上才有意义，
    /// 以前写成"没有声卡就 return"，`cargo test` 会显示 ok —— 假通过，比没有测试更坏。
    /// 现在默认不跑（`cargo test` 会报 1 ignored），要用
    /// `./scripts/test-sink.sh`（或 `cargo test -p apm-app session_with_real_sink -- --ignored`）显式跑。
    ///
    /// 这条测的是"声卡线程 + cpal + clear 收尾"这段最容易被重构弄坏的地方：
    /// 它编译不过和它跑得通是两件事。
    #[test]
    #[ignore = "需要虚拟声卡；用 ./scripts/test-sink.sh 跑"]
    fn session_with_real_sink() {
        let sinks = apm_sink::list_sinks().expect("枚举输出设备失败");
        let name = sinks
            .iter()
            .find(|s| s.name.to_lowercase().contains("blackhole") && s.supports_16k)
            .map(|s| s.name.clone())
            .expect("没有找到支持 16 kHz 的 BlackHole 声卡");

        let h = start(SessionOptions {
            session_id: "sink-smoke".into(),
            sink: Some(name.clone()),
            target_ms: 80,
            wav_path: None,
        })
        .expect("打开声卡失败（被独占或权限不足）");
        assert_eq!(h.info().sink_device.as_deref(), Some(name.as_str()));
        assert_eq!(h.info().rate, AUDIO_SAMPLE_RATE);

        // 推 0.5 s 静音：只要不 panic、收尾能拿到统计即可。
        for _ in 0..25 {
            h.push(&vec![0i16; apm_protocol::AUDIO_BLOCK_SAMPLES]);
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
        let summary = h.finish();
        assert_eq!(summary.frames, 25 * apm_protocol::AUDIO_BLOCK_SAMPLES as u64);
        assert!(summary.target_start_ms >= 40);
        assert_eq!(summary.wav_path, None);
    }

    /// 会话结束后能再开一条：声卡线程必须真的回收了。
    /// 线程没 join 干净的话，第二次 `start` 会拿到被独占的设备或者直接卡住。
    #[test]
    fn session_can_restart_after_finish() {
        let dir = std::env::temp_dir().join("apm-app-restart");
        std::fs::create_dir_all(&dir).unwrap();

        for round in 0..2 {
            let path = dir.join(format!("r{round}.wav"));
            let _ = std::fs::remove_file(&path);
            let h = start(SessionOptions {
                session_id: format!("r{round}"),
                sink: None,
                target_ms: 80,
                wav_path: Some(path.clone()),
            })
            .unwrap();
            for _ in 0..3 {
                h.push(&vec![200i16; apm_protocol::AUDIO_BLOCK_SAMPLES]);
            }
            let s = h.finish();
            assert_eq!(s.frames, 3 * apm_protocol::AUDIO_BLOCK_SAMPLES as u64, "第 {round} 轮");
            let _ = std::fs::remove_file(&path);
        }
    }

    /// 异常路径：句柄没 `finish` 就被丢弃，`Drop` 仍要把线程 join 掉并回填 WAV 头。
    /// 文件能解析、样本数对得上，就说明 worker 真的退出了（而不是文件停在一半）。
    #[test]
    fn drop_without_finish_still_closes_wav() {
        let dir = std::env::temp_dir().join("apm-app-drop");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dropped.wav");
        let _ = std::fs::remove_file(&path);

        {
            let h = start(SessionOptions {
                session_id: "drop".into(),
                sink: None,
                target_ms: 80,
                wav_path: Some(path.clone()),
            })
            .unwrap();
            h.push(&[1i16, 2, 3, 4]);
            // 不调用 finish，直接 drop。
        }

        let wav = apm_protocol::wav::read_wav(&path).expect("Drop 之后 WAV 头没有被回填");
        assert_eq!(wav.samples, vec![1, 2, 3, 4]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn silence_reports_none_not_floor() {
        let m = Meter::new(80);
        m.observe(&vec![0i16; 320], 0.0);
        let (rms, _peak, _) = m.snapshot(0.2);
        assert!(rms.is_none(), "全静音必须报 None，不能伪造 -90 dBFS");
    }

    #[test]
    fn wav_writer_patches_header_on_finalize() {
        let dir = std::env::temp_dir().join("apm-app-wav");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.wav");
        let _ = std::fs::remove_file(&path);

        {
            let mut w = WavWriter::create(&path).unwrap();
            w.write(&[1, 2, 3, 4]).unwrap();
            w.write(&[5, 6]).unwrap();
            w.finalize().unwrap();
        }

        let wav = apm_protocol::wav::read_wav(&path).unwrap();
        assert_eq!(wav.sample_rate, AUDIO_SAMPLE_RATE);
        assert_eq!(wav.channels, 1);
        assert_eq!(wav.samples, vec![1, 2, 3, 4, 5, 6]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sink_errors_map_to_fixed_codes() {
        let missing = classify_sink_error("没有名字包含 \"BlackHole\" 的输出设备");
        assert_eq!(missing.code, ErrorCode::SinkMissing);
        let rate = classify_sink_error("输出设备 \"x\" 不支持 16000 Hz 原生采样率；本项目不隐式重采样");
        assert_eq!(rate.code, ErrorCode::SinkRateUnsupported);
    }

    /// 没有声卡也能跑的端到端：只落 WAV，检查收尾统计。
    #[test]
    fn session_without_sink_writes_frames() {
        let dir = std::env::temp_dir().join("apm-app-session");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.wav");
        let _ = std::fs::remove_file(&path);

        let h = start(SessionOptions {
            session_id: "t1".into(),
            sink: None,
            target_ms: 80,
            wav_path: Some(path.clone()),
        })
        .unwrap();
        for _ in 0..5 {
            h.push(&vec![100i16; apm_protocol::AUDIO_BLOCK_SAMPLES]);
        }
        let summary = h.finish();
        assert_eq!(summary.blocks, 0, "块数由 Pipeline 填，这里应为 0");
        assert_eq!(summary.frames, 5 * apm_protocol::AUDIO_BLOCK_SAMPLES as u64);
        assert!((summary.duration_s - 0.1).abs() < 0.001, "{}", summary.duration_s);
        assert!(std::path::Path::new(summary.wav_path.as_ref().unwrap()).exists());
        let _ = std::fs::remove_file(&path);
    }
}
