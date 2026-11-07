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

/// 复用缓冲池大小（块）。`push` 从池里借一个 `Vec` 拷进去，声卡线程处理完还回来；
/// 池空时才新分配。
///
/// **为什么要有这个（A6）**：以前每块都 `pcm.to_vec()`，50 块/秒 × 20 分钟 ≈
/// 6 万次小块分配，全部发生在 BLE 通知任务里。分配本身不慢，但它会在 tokio worker
/// 上引入不可预测的停顿，而这条线程的停顿会直接表现为"块到达间隔变长"
/// —— 正是 KI-006 要区分的那件事。池大小取 channel 容量的两倍，够覆盖最坏积压。
const PCM_POOL: usize = PCM_CHANNEL * 2;
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
    /// 实测存量中位数（ms，整场累计）。`None` = 还没跑够样本。
    ///
    /// **报告里的延迟应当引这个，而不是 `target_ms`。** 修剪守卫会把稳态存量
    /// 留在"目标 + 余量"附近，用目标值当延迟会系统性低估二三十毫秒。
    pub fill_p50_ms: Option<f64>,
    /// 实测存量 p95（ms）。延迟预算（`p95 ≤ 150 ms`）里"抖动缓冲"那一项应当用它。
    pub fill_p95_ms: Option<f64>,
    /// 当前**实际生效**的抖动缓冲目标（基准 + 自适应余量）。
    pub target_ms: u32,
    /// 用户选的基准目标。自适应不会改它（A3）。
    pub base_target_ms: u32,
    /// 自适应临时加上的余量（ms）。非 0 说明最近发生过欠载。
    pub boost_ms: u32,
    pub blocks: u64,
    /// 序号跳变推算出的缺失块数。
    pub lost_blocks: u32,
    /// 发生跳变的次数（一次跳变可能丢多块）。
    pub seq_jumps: u32,
    /// 设备 STATUS 上报的发送阶段丢弃，与 `lost_blocks` 分开显示。
    pub device_dropped: u16,
    pub underruns: u64,
    /// 其中**上游断供**造成的次数（A5）。与 `underruns` 分开：
    /// 断供型调缓冲目标没用，得查链路；消费型才是缓冲不够。
    pub underruns_underfed: u64,
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
    /// 设备报过 `LINK_SLOW`（连接间隔 > 40 ms）：这一场的欠载/漂移是链路假象，
    /// 不能当音质结论。见 `docs/KNOWN_ISSUES.md` KI-005。
    pub link_slow: bool,
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
    base_target_ms: u32,
    boost_ms: u32,
    ppm: f64,
    underruns: u64,
    underruns_underfed: u64,
    silent_frames: u64,
    lock_misses: u64,
    bytes_written: u64,
    overflow: u64,
    glitch_events: u64,
    glitch_muted: u64,
    fill_p50_ms: Option<f64>,
    fill_p95_ms: Option<f64>,
}

/// 声卡侧指标的一次快照。抽成结构体是为了让 `publish_sink` 的参数不失控，
/// 也避免调用方把 buffer_ms 与 target_ms 传反。
#[derive(Debug, Clone, Copy, Default)]
struct SinkSnapshot {
    buffer_ms: f32,
    target_ms: u32,
    /// 用户选的基准目标（自适应不会改它）。
    base_target_ms: u32,
    /// 自适应临时余量。
    boost_ms: u32,
    ppm: f64,
    underruns: u64,
    /// 其中"上游断供"造成的次数（A5）。
    underruns_underfed: u64,
    silent_frames: u64,
    lock_misses: u64,
    bytes_written: u64,
    /// 实测存量分位数（A4）。
    fill_p50_ms: Option<f64>,
    fill_p95_ms: Option<f64>,
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
                base_target_ms: target_ms,
                boost_ms: 0,
                ppm: 0.0,
                underruns: 0,
                underruns_underfed: 0,
                silent_frames: 0,
                lock_misses: 0,
                bytes_written: 0,
                overflow: 0,
                glitch_events: 0,
                glitch_muted: 0,
                fill_p50_ms: None,
                fill_p95_ms: None,
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
        g.base_target_ms = snap.base_target_ms;
        g.boost_ms = snap.boost_ms;
        g.ppm = snap.ppm;
        g.underruns = snap.underruns;
        g.underruns_underfed = snap.underruns_underfed;
        g.silent_frames = snap.silent_frames;
        g.lock_misses = snap.lock_misses;
        g.bytes_written = snap.bytes_written;
        g.fill_p50_ms = snap.fill_p50_ms;
        g.fill_p95_ms = snap.fill_p95_ms;
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
            fill_p50_ms: g.fill_p50_ms,
            fill_p95_ms: g.fill_p95_ms,
            target_ms: g.target_ms,
            base_target_ms: g.base_target_ms,
            boost_ms: g.boost_ms,
            underruns: g.underruns,
            underruns_underfed: g.underruns_underfed,
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
    /// 其中**上游断供**（距上次推入 > 30 ms）造成的次数（A5）。
    ///
    /// `underruns - underruns_underfed` = 消费端跑得比生产快的次数 ——
    /// 只有这一部分能靠"把缓冲目标调大"改善。
    pub underruns_underfed: u64,
    pub silent_frames: u64,
    pub frames_dropped: u64,
    /// 延迟守卫一次性修剪掉的旧帧数（回调停摆造成的积压，不是链路问题）
    pub frames_trimmed: u64,
    pub lock_misses: u64,
    pub ppm: f64,
    pub frames_inserted: u64,
    pub frames_skipped: u64,
    /// 会话开始时**用户选的**缓冲基准（ms）。自适应不改它。
    pub target_start_ms: u32,
    /// 会话结束时用户选的基准（运行中滑块改过就会变）。
    pub target_end_ms: u32,
    /// 会话期间自适应临时加过的最大余量（ms）。0 = 全程没被抬过。
    pub target_boost_max_ms: u32,
    /// 实测存量中位数（ms，整场累计）。报告里的"抖动缓冲延迟"用它。
    pub buffer_p50_ms: Option<f64>,
    /// 实测存量 p95（ms）。延迟预算里"抖动缓冲"那一项用它。
    pub buffer_p95_ms: Option<f64>,
    pub glitch_events: u64,
    pub glitch_muted_ms: f64,
    pub lost_blocks: u32,
    pub seq_jumps: u32,
    pub device_dropped: u16,
    pub bad_len_blocks: u32,
    pub overflow_drops: u64,
    /// 设备报过 `LINK_SLOW`（连接间隔 > 40 ms）。这一场的数字不能进音质对比。
    pub link_slow: bool,
    /// 本场最大块到达间隔（ms，链路层打点，不含我们自己的处理时间）。
    pub max_arrival_gap_ms: f64,
    /// 本场到达间隔超过 80 ms 缓冲上限的次数。
    pub late_arrivals: u64,
    /// 本场单块处理耗时最大值（ms）：链路交给我们 → 解码完成推入声卡。
    pub max_process_gap_ms: f64,
    /// 本场处理耗时超过 20 ms（超过一个块的时长）的块数。
    pub slow_process_blocks: u64,
    /// 本场最长一次到达间隔期间，读取实际等了多久（ms）。
    ///
    /// `max_arrival_gap_ms - 这一项` = 我们在那一刻自己花掉的时间。
    pub gap_read_wait_ms: f64,
    /// 本场读取等待的最大值（ms）：反映"我们问链路问得够不够勤"。
    pub max_read_wait_ms: f64,
    /// 通知交付模式：`even`（逐块）/ `clumped`（攒批）/ `unclear` / `insufficient`。
    ///
    /// 这是 G2 判据（方案 D 阈值）的直接依据：攒批交付下"偶尔一次长间隔"是机制固有形状。
    pub delivery_pattern: String,
    /// 相邻间隔 <5 ms 的比例（攒批的直接证据）。
    pub delivery_clumped_ratio: f64,
    /// 间隔落在 15–25 ms 的比例（逐块交付的证据）。
    pub delivery_even_ratio: f64,
}

enum Msg {
    /// PCM 块 + 它来自哪个池槽位（`None` = 池空时新分配的，还不属于池）。
    Pcm { pcm: Vec<i16>, slot: Option<usize> },
    Retarget(u32),
    Stop,
}

/// 复用缓冲池。声卡线程用完把槽位还回来，生产者下次直接复用。
///
/// `SessionHandle::push` 借一个、`worker` 处理完还回来。池空时退回新分配
/// （与旧行为一致），所以它只影响"常见路径少一次 malloc/free"，不影响正确性。
struct PcmPool {
    /// 空闲槽位下标。
    free: Vec<usize>,
    /// 槽位本体。`take` 会把 `Vec` 取走（`mem::take`），`give_back` 再放回。
    slots: Vec<Vec<i16>>,
    /// 每个槽位当前是否被借走。用于防重复归还，O(1) 判断。
    taken: Vec<bool>,
}

impl PcmPool {
    fn new() -> Self {
        Self {
            // 先全部登记为空闲；槽位本体第一次借用时才扩容，
            // 免得开流瞬间就分配 64 个块。
            free: (0..PCM_POOL).rev().collect(),
            slots: vec![Vec::new(); PCM_POOL],
            taken: vec![false; PCM_POOL],
        }
    }

    /// 借一个槽位：返回 `(slot_index, buffer)`。池空时返回 `None`。
    fn take(&mut self) -> Option<(usize, Vec<i16>)> {
        let idx = self.free.pop()?;
        self.taken[idx] = true;
        Some((idx, std::mem::take(&mut self.slots[idx])))
    }

    /// 还回槽位。`idx` 必须来自 [`Self::take`]；越界或重复归还直接忽略，
    /// 宁可少一个缓冲也不要在音频路径上 panic。
    fn give_back(&mut self, idx: usize, mut buf: Vec<i16>) {
        if idx >= self.slots.len() || !self.taken[idx] {
            return;
        }
        // 保留容量、不保留数据：下次 `clear()` + `extend_from_slice` 复用同一块内存。
        buf.clear();
        self.slots[idx] = buf;
        self.taken[idx] = false;
        self.free.push(idx);
    }
}

/// 主线程握着的句柄。`Drop` 时若还没 `finish`，会自动发 stop 并等线程收尾，
/// 避免 GUI 退出时留下一条还在往声卡写静音的线程。
pub struct SessionHandle {
    tx: SyncSender<Msg>,
    /// 复用缓冲池：生产者借、声卡线程还。锁只包住一次 `Vec` 移动（百纳秒级），
    /// 不跨任何 IO，也不跨 `sink.push`。
    pool: Arc<Mutex<PcmPool>>,
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
    underruns_underfed: u64,
    silent_frames: u64,
    frames_dropped: u64,
    frames_trimmed: u64,
    lock_misses: u64,
    ppm: f64,
    frames_inserted: u64,
    frames_skipped: u64,
    target_start_ms: u32,
    target_end_ms: u32,
    target_boost_max_ms: u32,
    buffer_p50_ms: Option<f64>,
    buffer_p95_ms: Option<f64>,
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
    ///
    /// 走缓冲池：常见路径上不再每块 `to_vec()`（见 [`PCM_POOL`]）。
    /// 池空时退回新分配，所以行为与旧实现一致，只是少了分配噪声。
    pub fn push(&self, pcm: &[i16]) {
        let (slot, mut buf) = match self.pool.lock() {
            Ok(mut pool) => match pool.take() {
                Some((slot, buf)) => (Some(slot), buf),
                None => (None, Vec::with_capacity(pcm.len())),
            },
            Err(e) => {
                let mut pool = e.into_inner();
                match pool.take() {
                    Some((slot, buf)) => (Some(slot), buf),
                    None => (None, Vec::with_capacity(pcm.len())),
                }
            }
        };
        buf.clear();
        buf.extend_from_slice(pcm);

        match self.tx.try_send(Msg::Pcm { pcm: buf, slot }) {
            Ok(()) => {}
            Err(TrySendError::Full(msg)) => {
                self.meter.note_overflow();
                // 没送出去就把槽位还回池子，否则池会一路漏空、退回每块分配。
                if let Msg::Pcm { pcm, slot } = msg {
                    self.give_back(slot, pcm);
                }
            }
            Err(TrySendError::Disconnected(msg)) => {
                if let Msg::Pcm { pcm, slot } = msg {
                    self.give_back(slot, pcm);
                }
            }
        }
    }

    fn give_back(&self, slot: Option<usize>, buf: Vec<i16>) {
        let Some(idx) = slot else { return };
        match self.pool.lock() {
            Ok(mut pool) => pool.give_back(idx, buf),
            Err(e) => e.into_inner().give_back(idx, buf),
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
            underruns_underfed: o.underruns_underfed,
            silent_frames: o.silent_frames,
            frames_dropped: o.frames_dropped,
            frames_trimmed: o.frames_trimmed,
            lock_misses: o.lock_misses,
            ppm: o.ppm,
            frames_inserted: o.frames_inserted,
            frames_skipped: o.frames_skipped,
            target_start_ms: o.target_start_ms,
            target_end_ms: o.target_end_ms,
            target_boost_max_ms: o.target_boost_max_ms,
            buffer_p50_ms: o.buffer_p50_ms,
            buffer_p95_ms: o.buffer_p95_ms,
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
    // 唯一一份夹紧：`apm_sink` 的常量就是契约，不在这里再写一遍字面量。
    let target_ms = opts
        .target_ms
        .clamp(apm_sink::TARGET_MIN_MS, apm_sink::TARGET_MAX_MS);
    let meter = Arc::new(Meter::new(target_ms));
    let pool = Arc::new(Mutex::new(PcmPool::new()));
    let (tx, rx) = sync_channel::<Msg>(PCM_CHANNEL);
    let (ready_tx, ready_rx) = sync_channel::<Result<SessionInfo, AppError>>(1);

    let meter_thread = Arc::clone(&meter);
    let pool_thread = Arc::clone(&pool);
    let opts_thread = opts.clone();
    let join = thread::Builder::new()
        .name("apm-sink".into())
        .spawn(move || worker(rx, meter_thread, pool_thread, opts_thread, ready_tx))
        .map_err(|e| AppError::internal(format!("创建声卡线程失败：{e}")))?;

    match ready_rx.recv() {
        Ok(Ok(info)) => Ok(SessionHandle {
            tx,
            pool,
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
    pool: Arc<Mutex<PcmPool>>,
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
        // `SessionInfo::target_ms` 对外表达的是**用户选的基准**，不是自适应当前值。
        // 报告里写"目标 80 ms"必须对应滑块上的那个数，否则界面与报告会互相矛盾。
        target_ms: sink
            .as_ref()
            .map(|s| s.base_target_ms())
            .unwrap_or(opts.target_ms),
        wav_path: opts.wav_path.as_ref().map(|p| p.display().to_string()),
    };
    let target_start_ms = info.target_ms;
    let _ = ready.send(Ok(info.clone()));

    let mut suppressor = glitch::Suppressor::new();
    let mut frames = 0u64;
    let mut ticks = 0u32;
    // 会话期间自适应临时余量的峰值（ms）。
    let mut boost_max = 0u32;

    loop {
        match rx.recv() {
            Ok(Msg::Pcm { mut pcm, slot }) => {
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
                // 用完立刻还回缓冲池：`sink.push` 已经把样本拷进 `Playout` 的
                // `VecDeque`，这里之后不会再读这块内存。
                if let Some(idx) = slot {
                    match pool.lock() {
                        Ok(mut p) => p.give_back(idx, pcm),
                        Err(e) => e.into_inner().give_back(idx, pcm),
                    }
                }
                ticks += 1;
                if ticks.is_multiple_of(5) {
                    meter.note_glitch(suppressor.events, suppressor.muted_samples);
                    if let Some(s) = sink.as_ref() {
                        let st = s.stats();
                        let boost = s.boost_ms();
                        boost_max = boost_max.max(boost);
                        meter.publish_sink(SinkSnapshot {
                            buffer_ms: s.fill_ms(),
                            target_ms: s.target_ms(),
                            base_target_ms: s.base_target_ms(),
                            boost_ms: boost,
                            ppm: s.ppm(),
                            underruns: st.underruns,
                            underruns_underfed: st.underruns_underfed,
                            silent_frames: st.frames_silenced,
                            lock_misses: s.lock_misses(),
                            bytes_written: frames * 2,
                            fill_p50_ms: st.fill_percentile_ms(0.5),
                            fill_p95_ms: st.fill_percentile_ms(0.95),
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
        target_boost_max_ms: boost_max,
        ..WorkerOutcome::default()
    };
    if let Some(mut s) = sink {
        // 先停写再读统计：clear 之后声卡不再消费，统计才是最终值。
        s.clear();
        let st = s.stats();
        outcome.underruns = st.underruns;
        outcome.underruns_underfed = st.underruns_underfed;
        outcome.silent_frames = st.frames_silenced;
        // 报告里的"超容量丢弃"只算真·容量兜底；延迟守卫修剪的旧数据另记，
        // 两者混在一起会让人以为链路出了问题。
        outcome.frames_dropped = st.frames_overflow;
        outcome.frames_trimmed = st.frames_trimmed;
        outcome.lock_misses = s.lock_misses();
        outcome.ppm = s.ppm();
        outcome.frames_inserted = st.frames_inserted;
        outcome.frames_skipped = st.frames_skipped;
        // 收尾报告：基准是用户的意图，自适应余量单独报峰值。
        outcome.target_end_ms = s.base_target_ms();
        outcome.target_boost_max_ms = boost_max.max(s.boost_ms());
        // 实测存量分位数（A4）：报告里的延迟用它，不用目标值。
        outcome.buffer_p50_ms = st.fill_percentile_ms(0.5);
        outcome.buffer_p95_ms = st.fill_percentile_ms(0.95);
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

/// 44 字节 PCM16 单声道 WAV 头。**开头写一次、收尾回填一次**，两处必须完全一致，
/// 所以只留这一个拼装点：以前 `write_header` 与 `finalize` 各拼一遍，
/// 字段写法还不一样（一个是变量 `channels`，一个是硬编码 `1u16`）——
/// 改一处忘另一处就是一个能录不能放的文件。
fn wav_header(data_len: u32) -> Vec<u8> {
    const CHANNELS: u16 = 1;
    const BITS: u16 = 16;
    const PCM: u16 = 1;
    let rate = AUDIO_SAMPLE_RATE;
    let block_align = CHANNELS * BITS / 8;

    let mut head = Vec::with_capacity(44);
    head.extend_from_slice(b"RIFF");
    head.extend_from_slice(&(36 + data_len).to_le_bytes());
    head.extend_from_slice(b"WAVE");
    head.extend_from_slice(b"fmt ");
    head.extend_from_slice(&16u32.to_le_bytes()); // fmt 块长度
    head.extend_from_slice(&PCM.to_le_bytes());
    head.extend_from_slice(&CHANNELS.to_le_bytes());
    head.extend_from_slice(&rate.to_le_bytes());
    head.extend_from_slice(&(rate * block_align as u32).to_le_bytes()); // 字节率
    head.extend_from_slice(&block_align.to_le_bytes());
    head.extend_from_slice(&BITS.to_le_bytes());
    head.extend_from_slice(b"data");
    head.extend_from_slice(&data_len.to_le_bytes());
    head
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
        let head = wav_header(self.data_bytes as u32);
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
        let head = wav_header(self.data_bytes as u32);
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
        assert_eq!(t.base_target_ms, 80, "基准目标应当就是构造时给的值");
        assert_eq!(t.boost_ms, 0, "刚开会话不该有自适应余量");

        // 保持 1.5 s 后开始按 12 dB/s 释放；10 s 后应归零。
        m.observe(&vec![0i16; 320], 1.6);
        let (_rms2, peak2, _) = m.snapshot(10.0);
        assert!(peak2.is_none(), "峰值没有释放：{peak2:?}");
    }

    /// **A6：缓冲池必须真的被复用，且送不出去的槽位要还回来。**
    ///
    /// 判据用"池里空闲槽位数"而不是分配次数（分配次数测不到）。
    /// 池大小固定，所以只要 push 之后不泄漏、worker 归还，长期稳态应当是满的。
    #[test]
    fn pcm_pool_recycles_and_does_not_leak_on_full_channel() {
        let mut pool = PcmPool::new();
        assert_eq!(pool.free.len(), PCM_POOL);

        // 借空整个池：必须借得到，且拿到的都是空 Vec。
        let mut held = Vec::new();
        for _ in 0..PCM_POOL {
            let (idx, buf) = pool.take().expect("池没满时应当借得到");
            assert!(buf.is_empty(), "借出的缓冲必须是空的");
            held.push(idx);
        }
        assert!(pool.take().is_none(), "借空之后应当返回 None（退回新分配）");

        // 全部归还后应当恢复原样。
        for idx in held {
            pool.give_back(idx, Vec::with_capacity(4));
        }
        assert_eq!(pool.free.len(), PCM_POOL, "归还后池没有恢复");

        // 重复归还同一个槽位不能把空闲表撑大（那会让两个生产者拿到同一块内存）。
        let (idx, buf) = pool.take().unwrap();
        pool.give_back(idx, buf.clone());
        pool.give_back(idx, buf);
        assert_eq!(pool.free.len(), PCM_POOL, "重复归还污染了空闲表");
    }

    /// 真正的 `SessionHandle::push` 路径：channel 满时槽位要还回去，不能一路漏空。
    #[test]
    fn push_returns_pool_slot_when_channel_is_full() {
        let dir = std::env::temp_dir().join("apm-app-pool-full");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pool.wav");
        let _ = std::fs::remove_file(&path);

        let h = start(SessionOptions {
            session_id: "pool".into(),
            sink: None,
            target_ms: 80,
            wav_path: Some(path.clone()),
        })
        .unwrap();

        let block = vec![100i16; apm_protocol::AUDIO_BLOCK_SAMPLES];
        // 灌远超 channel 容量的块：前 PCM_CHANNEL 个能进，其余全部 Full。
        for _ in 0..(PCM_CHANNEL * 4) {
            h.push(&block);
        }
        // 池不泄漏的直接证据：所有槽位都能借到（说明被 Full 拒掉的那些都还回来了）。
        {
            let mut pool = h.pool.lock().unwrap_or_else(|e| e.into_inner());
            let mut taken = Vec::new();
            while let Some((idx, _)) = pool.take() {
                taken.push(idx);
            }
            assert!(
                !taken.is_empty(),
                "池被漏空了 —— channel 满时槽位没有归还"
            );
            for idx in taken {
                pool.give_back(idx, Vec::new());
            }
        }
        // 溢出应当被计数，而不是被静默吞掉。
        assert!(
            h.meter().snapshot(1.0).2.overflow_drops > 0,
            "channel 满丢块没有计数"
        );

        let _ = h.finish();
        let _ = std::fs::remove_file(&path);
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
        assert_eq!(
            summary.frames,
            25 * apm_protocol::AUDIO_BLOCK_SAMPLES as u64
        );
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
            assert_eq!(
                s.frames,
                3 * apm_protocol::AUDIO_BLOCK_SAMPLES as u64,
                "第 {round} 轮"
            );
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
        let rate =
            classify_sink_error("输出设备 \"x\" 不支持 16000 Hz 原生采样率；本项目不隐式重采样");
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
        assert!(
            (summary.duration_s - 0.1).abs() < 0.001,
            "{}",
            summary.duration_s
        );
        assert!(std::path::Path::new(summary.wav_path.as_ref().unwrap()).exists());
        let _ = std::fs::remove_file(&path);
    }
}
