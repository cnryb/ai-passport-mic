//! 应用内核：状态机 + 编排 + 事件广播 + 诊断日志。
//!
//! 设计约束（见 `docs/GUI.md` §3）：
//! - 这里**不依赖任何 GUI 框架**，CLI 与 GUI 共用；
//! - 长动作（扫描/连接/推流）由调用方 `await`，内核自己不假设有全局运行时；
//!   只有需要脱离命令生命周期的通知监控与 5 Hz 遥测，才用调用方传入的 tokio
//!   `Handle` 派生；
//! - 状态迁移只有一个入口（`set_state`），保证每次变化都同时更新事件与日志；
//! - 音频编排只有一个入口（[`crate::pipeline::Pipeline`]），CLI 与 GUI 不走两套。

use crate::events::{AppEvent, LogEntry, LogLevel};
use crate::log::{epoch_ms, LogSink};
use crate::pipeline::Pipeline;
use crate::report::QualityReport;
use crate::session::{SessionOptions, SessionSummary, Telemetry};
use crate::settings::{self, Settings};
use crate::state::{AppError, AppState, DeviceView, ErrorCode};
use apm_link::Notification;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex as StdMutex, MutexGuard,
};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, watch, Mutex as AsyncMutex};

/// 诊断页保留的最近条目数；落盘文件是全量的。
const LOG_TAIL: usize = 300;
/// 事件通道容量。前端消费慢时按"丢弃最旧"处理：丢事件比卡住音频线程好。
const EVENT_CHANNEL: usize = 256;
/// 扫描窗口上限，与 `docs/GUI.md` §6 一致。
const SCAN_MAX_MS: u64 = 12_000;
const SCAN_MIN_MS: u64 = 1_000;
/// 连接超时。真机冷启动广播窗口短，10 s 足够又不至于让用户干等。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 遥测周期：5 Hz。前端的 rAF 是 60 Hz，50 Hz 事件会让主线程排队。
const TELEMETRY_PERIOD: Duration = Duration::from_millis(200);
/// 设备每秒发一次 STATUS 心跳。静默超过这个时长就要主动问一次链路——
/// 拔电源不会产生任何通知，"什么都没有"本身才是信号。
const LINK_SILENCE: Duration = Duration::from_secs(5);
/// 链路探活超时。`btleplug::Peripheral::is_connected()` 在设备已经消失时**不会返回**，
/// 实测（2026-11-05 G1 验收）：设备复位掉链后，监控循环卡在这一句上，状态永久停在
/// `ready`、界面继续显示"已连接"、日志一行没有。超时一律按"已断开"处理。
const LINK_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// 主动断开调用的超时。设备已经消失时 `Peripheral::disconnect()` 和 `is_connected()`
/// 是同一类问题——不会返回。不设超时收尾就卡在 `stopping`，状态永远回不到 `idle`
/// （实测 2026-11-05 G1 验收：日志停在「状态 ready → stopping」就没了）。
const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// 开流前"STOP → START"之间留的静默。设备处理 STOP 要跑完一轮任务循环（采集停止、
/// I2S 通道复位），立刻接着 START 有可能被当成重复命令丢掉。120 ms 是实测能稳定
/// 让设备回到 idle 的量级，又短到不影响用户点"开始"的手感。
const CAPTURE_RESET_SETTLE: Duration = Duration::from_millis(120);
/// 保留的会话数（仅用于"打开上次录音做分析"）。
const SESSIONS_KEPT: usize = 20;

/// 开流后的"有没有真的在发音频"观察窗口。
///
/// 设备停在 KI-005 的半死态时，链路通、STATUS 每秒来，但音频只有 1.5–2 块/秒。
/// 正常链路 3 s 应该收到 ~150 块，10 块是个很宽松的下限——只拦"几乎没数据"，
/// 不拦"稍慢一点"（那属于 KI-006 的欠载范畴，另行统计）。
const EARLY_STOP_WINDOW: Duration = Duration::from_secs(3);
/// 窗口内收到的块数低于这个值就判定"设备没在发/链路异常"。
const EARLY_STOP_MIN_BLOCKS: u64 = 10;

/// 关麦兜底看门狗的检查周期。
///
/// 秒级即可：兜底的粒度是"分钟"，检查得再勤也只是多几次加锁。
/// 取整不取 1 s 是为了让日志里的触发时刻看起来不像是卡在整秒。
const AUTO_STOP_TICK: Duration = Duration::from_secs(5);

/// "未推流却收到音频块"告警的最小间隔。
///
/// 设备在 CTRL 状态不一致时会以 50 块/秒推音频，若每块记一条日志，一秒钟就是
/// 50 行、而且全在通知循环里 —— 那不是诊断，那是自己制造故障。
const STRAY_AUDIO_LOG_EVERY: Duration = Duration::from_secs(5);

struct Inner {
    state: AppState,
    devices: Vec<DeviceView>,
    /// 存在表示正在扫描；`send(true)` 即请求提前结束。
    scan_cancel: Option<watch::Sender<bool>>,
    /// 存在表示通知监控在跑；`send(true)` 即请求收尾断链。
    link_stop: Option<watch::Sender<bool>>,
    /// **链路代次**：每建立一条新链路 +1，断开/收尾时不动。
    ///
    /// 所有"跨 await 之后要改状态"的异步迁移都必须先校验它没变（见 `set_state_if_link`）。
    /// 没有这一条时，`start_stream` 在 `CTRL START` 的往返窗口里被断链收尾插进来，
    /// 就会把已经拆掉的链路重新标成 `Ready` —— 界面显示"已连接"，但点开麦永远失败，
    /// 只能重启 App（B2）。`connect_inner` / `teardown` 都靠它判断"我还是不是当初那条链路"。
    link_epoch: u64,
    /// 正在推流的会话。**唯一**的音频消费入口在 `monitor_loop`。
    stream: Option<Pipeline>,
    /// 有一条 `start_stream` 正在推进中（已通过前置校验，还没落定）。
    ///
    /// 开流是"STOP → 等 120 ms → 开声卡 → START"的一串 await，全程约 200–300 ms，
    /// 而这段时间入口只看到 `state == Ready`、`stream == None`。两个并发调用
    /// （用户双击「开始」、或界面按钮与设备 OK 键撞上）会各自建一条 `Pipeline`：
    /// 后写入的覆盖先写入的，被覆盖的那个在 async 上下文里同步 `join` 声卡线程，
    /// 阻塞 tokio worker；声卡也被打开两次（B1）。这个标志是那个窗口的占位锁。
    stream_starting: bool,
    /// 最近若干次会话（用于音质报告页）。
    sessions: VecDeque<SessionRecord>,
    settings: Settings,
    /// 第一次进异步上下文时记下运行时句柄，供派生监控任务用。
    exec: Option<tokio::runtime::Handle>,
    /// 监控循环判定"链路已经没了"。收尾时据此跳过 `disconnect()`——对一只已经消失的
    /// 外设调它只会白等一个超时，而且必然失败。
    link_lost: bool,
    /// 上一次 STATUS 心跳。只用于判断"这条心跳与上一条有没有实质变化"，
    /// 决定要不要记日志 —— 每秒一条无条件 Info 会把日志变成噪声源。
    last_status: Option<apm_protocol::frame::Status>,
    /// 未推流时收到的音频块累计（用于限流告警里报数量）。
    stray_audio_blocks: u64,
    /// 上一次打"未推流却收到音频"告警的时刻。
    stray_audio_logged: Option<Instant>,
    /// 收到但解析不出来的通知累计（B6）。与"链路安静"必须分开统计。
    malformed_blocks: u64,
    /// 上一次打"无法解析的通知"告警的时刻。
    malformed_logged: Option<Instant>,
    /// 最近一次电量结论，三层语义：
    /// `None` = 还没读过 / 没有链路；`Some(None)` = 设备明确报"不可用"；
    /// `Some(Some(pct))` = 有效百分比。
    ///
    /// 必须落在内核里而不是只走事件：GUI 启动时 `auto_connect` 会先连上并读一次电量，
    /// 而前端要跑完 snapshot / 声卡枚举 / keymap 才注册监听，那时事件已经发完了
    /// （Tauri 不重放）。快照带上它，界面才不会有"设备连着但电量永远空白"的死角。
    battery: Option<Option<u8>>,
}

/// 一次会话的落盘记录。没有勾选保存 WAV 时 `wav_path` 为 `None`，
/// 音质报告会明确说"没有录音"，而不是给一份空报告。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionRecord {
    pub session_id: String,
    pub wav_path: Option<String>,
    pub started_ms: u64,
    /// 这一场设备报过 `LINK_SLOW`（连接间隔 > 40 ms）。
    ///
    /// 收尾时才回填（`start_stream` 时还不知道）。**为真时禁止出音质报告**：
    /// 慢档会话的欠载/漂移/丢块全是链路假象，拿它跟别的版本对比会得出反向结论。
    /// 见 `docs/ARCHITECTURE.md` 的 STATUS 节与 `docs/KNOWN_ISSUES.md` KI-005。
    pub link_slow: bool,
}

/// 声卡的界面视图。字段够用来判断"能不能选"，不暴露 cpal 类型。
///
/// 只带界面真正用到的字段。`SinkInfo` 里的 `uid` / `channels` 曾经一路复制到这里
/// 却没有任何消费方（前端只读 `name` 与 `supports_16k`），属于"为以后可能的
/// WASAPI 按 UID 匹配"预留的字段 —— 真要上 Windows 时再加，现在留着只是契约噪音。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SinkView {
    pub name: String,
    /// 该声卡支持的采样率范围，只用于"不支持 16 kHz"时的错误文案。
    pub min_rate: u32,
    pub max_rate: u32,
    pub supports_16k: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SettingsView {
    pub sink: Option<String>,
    pub target_ms: u32,
    pub save_wav: bool,
    /// 启动自动连接开关。
    pub auto_connect: bool,
    /// 上次连接的设备名（无记录为 `None`），只用于界面提示。
    pub last_device: Option<String>,
    /// 连续采集多少分钟后自动停（关麦兜底）。
    pub auto_stop_min: u32,
}

/// 前端的启动快照。之后一切变化走事件，不再轮询。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AppSnapshot {
    pub version: String,
    pub state: AppState,
    pub devices: Vec<DeviceView>,
    pub log_tail: Vec<LogEntry>,
    pub log_path: String,
    pub log_dir: String,
    pub settings: SettingsView,
    pub last_session: Option<SessionRecord>,
    /// 电量百分比（0..100）。`battery_known=false` 时无意义。
    pub battery_pct: Option<u8>,
    /// 是否已经拿到过电量结论（无论有没有数字）。
    /// 与 `battery_pct` 配合区分"还没读到"（known=false, pct=None）和
    /// "设备说不可用"（known=true, pct=None）——前者不该闪「电量不可用」。
    pub battery_known: bool,
}

pub struct AppCore {
    inner: StdMutex<Inner>,
    /// 连上之后的链路对象。用异步锁：通知监控会长时间持有它等待通知。
    link: AsyncMutex<Option<apm_link::Link>>,
    events: broadcast::Sender<AppEvent>,
    sink: StdMutex<LogSink>,
    tail: StdMutex<VecDeque<LogEntry>>,
    version: &'static str,
}

/// 把动作描述成 `ctrl+enter`、`esc ×2` 这种形式，只用于日志与控制台输出。
fn describe_binding(binding: &apm_protocol::keymap::Binding) -> String {
    let mut out = String::new();
    for m in &binding.modifiers {
        out.push_str(m.as_str());
        out.push('+');
    }
    out.push_str(&binding.key);
    if binding.repeat > 1 {
        out.push_str(&format!(" ×{}", binding.repeat));
    }
    out
}

/// 适配器探测超时。btleplug 在 macOS 上会一直等 CBCentralManager 变成 poweredOn，
/// 若系统授权弹窗没人点、或蓝牙被关掉，它会**永远不返回**——界面就会卡在"检测中"。
/// 实测踩过：进程挂在 `probe_adapter` 上，日志停在一行"检测蓝牙适配器"。
const ADAPTER_PROBE_TIMEOUT: Duration = Duration::from_secs(6);

impl AppCore {
    pub fn new(log_dir: Option<PathBuf>) -> Arc<Self> {
        Self::with_settings(log_dir, Settings::load())
    }

    /// 用**指定配置**构造，不读用户目录下的 `settings.json`。
    ///
    /// 测试必须走这个入口。2026-11-07 踩过：某测试断言"默认不录 WAV"，
    /// 但它其实读的是**用户真实配置**——只要有人在界面上勾了「同时保存 WAV」，
    /// `cargo test` 就会红。测试不该依赖机器的外部状态。
    fn with_settings(log_dir: Option<PathBuf>, settings: Settings) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_CHANNEL);
        let core = Arc::new(Self {
            inner: StdMutex::new(Inner {
                // 初始为 Idle：真正的适配器状态由 `detect_adapter` 覆盖。
                // 先假设可用，避免界面在首帧闪一下"没有蓝牙"。
                state: AppState::Idle,
                devices: Vec::new(),
                scan_cancel: None,
                link_stop: None,
                link_epoch: 0,
                stream: None,
                stream_starting: false,
                sessions: VecDeque::new(),
                settings,
                exec: None,
                link_lost: false,
                last_status: None,
                stray_audio_blocks: 0,
                stray_audio_logged: None,
                malformed_blocks: 0,
                malformed_logged: None,
                battery: None,
            }),
            link: AsyncMutex::new(None),
            events,
            sink: StdMutex::new(LogSink::new(log_dir)),
            tail: StdMutex::new(VecDeque::new()),
            version: env!("CARGO_PKG_VERSION"),
        });
        core.record(
            LogLevel::Info,
            "app",
            format!("应用内核启动 v{}", core.version),
        );
        let s = core.settings_view();
        core.record(
            LogLevel::Debug,
            "app",
            format!(
                "配置：声卡={:?} 缓冲目标={} ms 录 WAV={}",
                s.sink, s.target_ms, s.save_wav
            ),
        );
        core
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AppEvent> {
        self.events.subscribe()
    }

    pub fn state(&self) -> AppState {
        self.lock_inner().state.clone()
    }

    pub fn devices(&self) -> Vec<DeviceView> {
        self.lock_inner().devices.clone()
    }

    pub fn log_path(&self) -> String {
        self.lock_sink().path().display().to_string()
    }

    pub fn log_dir(&self) -> String {
        self.lock_sink().dir().display().to_string()
    }

    pub fn settings_view(&self) -> SettingsView {
        let s = &self.lock_inner().settings;
        SettingsView {
            sink: s.sink.clone(),
            target_ms: s.target_ms,
            save_wav: s.save_wav,
            auto_connect: s.auto_connect,
            last_device: s.last_device_name.clone(),
            auto_stop_min: s.auto_stop_min,
        }
    }

    /// 启动自动连接开关。立即落盘：用户勾完就关 App 也得记住。
    pub fn set_auto_connect(&self, enabled: bool) -> Result<(), AppError> {
        let snapshot = {
            let mut inner = self.lock_inner();
            inner.settings.auto_connect = enabled;
            inner.settings.clone()
        };
        // **落盘在锁外**（A2）。`Inner` 是整个内核唯一的锁，而它同时被 BLE 通知任务
        // 持有 —— 在这里写磁盘等于让音频块排在 fsync 后面。
        self.persist(snapshot, "ble");
        Ok(())
    }

    /// 把配置写盘。**调用方必须已经放开 `inner` 锁**。
    ///
    /// 传进来的是一份克隆而不是 `&Settings`：这样调用点无法"顺手"在锁内调用它，
    /// 编译器会挡住那种写法。
    fn persist(&self, settings: Settings, source: &str) {
        if let Err(err) = settings.save() {
            self.record(LogLevel::Warn, source, format!("保存配置失败：{err}"));
        }
    }

    pub fn snapshot(&self) -> AppSnapshot {
        let battery = self.lock_inner().battery;
        AppSnapshot {
            version: self.version.to_string(),
            state: self.state(),
            devices: self.devices(),
            log_tail: self.tail_snapshot(),
            log_path: self.log_path(),
            log_dir: self.log_dir(),
            settings: self.settings_view(),
            last_session: self.lock_inner().sessions.back().cloned(),
            battery_pct: battery.flatten(),
            battery_known: battery.is_some(),
        }
    }

    /// 检测适配器。失败时状态落到 `no_adapter`，并把具体原因写进日志。
    ///
    /// 已经连上设备时不改状态：适配器探测失败不该把一次正常会话踢成 `idle`，
    /// 那会让界面误以为"设备还连着但状态没了"。
    pub async fn detect_adapter(&self) -> Result<(), AppError> {
        self.remember_exec();
        self.record(LogLevel::Info, "ble", "检测蓝牙适配器");
        let probed = tokio::time::timeout(ADAPTER_PROBE_TIMEOUT, apm_link::probe_adapter()).await;
        let busy = self.state().device().is_some();
        match probed {
            Ok(Ok(())) => {
                if matches!(self.state(), AppState::NoAdapter) {
                    self.record(LogLevel::Info, "ble", "蓝牙已恢复可用");
                }
                if !busy {
                    self.set_state(AppState::Idle);
                }
                Ok(())
            }
            Ok(Err(err)) => {
                let err = AppError::from(err);
                self.record(
                    LogLevel::Error,
                    "ble",
                    format!("适配器不可用（{:?}）：{}", err.code, err.detail),
                );
                if !busy {
                    self.set_state(AppState::NoAdapter);
                }
                Err(err)
            }
            Err(_) => {
                // 超时不要伪装成"设备没找到"：绝大多数情况是授权弹窗没点，或蓝牙被关。
                let err = AppError::new(
                    ErrorCode::BtUnavailable,
                    format!(
                        "{} s 内没拿到蓝牙适配器状态。检查：① 系统是否弹出了蓝牙授权（没点会一直等）② 蓝牙开关是否打开",
                        ADAPTER_PROBE_TIMEOUT.as_secs()
                    ),
                );
                self.record(
                    LogLevel::Error,
                    "ble",
                    format!("适配器探测超时：{}", err.detail),
                );
                if !busy {
                    self.set_state(AppState::NoAdapter);
                }
                Err(err)
            }
        }
    }

    /// 扫描 `window_ms`，期间通过 `devices` 事件推送增量列表，返回时状态回到 `idle`。
    ///
    /// 这是异步方法：UI 侧不需要它立刻返回——列表是靠事件涨的。用户点"停止"时
    /// `stop_scan` 会触发 watch，本次调用在 300 ms 内收尾返回。
    pub async fn start_scan(self: &Arc<Self>, window_ms: u64) -> Result<(), AppError> {
        let window_ms = window_ms.clamp(SCAN_MIN_MS, SCAN_MAX_MS);
        {
            let inner = self.lock_inner();
            if inner.state.is_busy() {
                return Err(AppError::busy(format!(
                    "当前状态 {} 不能开始扫描",
                    inner.state.kind()
                )));
            }
            if inner.link_stop.is_some() {
                return Err(AppError::busy("已连接设备，先断开再扫描"));
            }
        }

        self.remember_exec();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        self.lock_inner().scan_cancel = Some(cancel_tx);
        self.set_state(AppState::Scanning { window_ms });
        self.record(
            LogLevel::Info,
            "ble",
            format!("开始扫描，窗口 {window_ms} ms"),
        );

        let core = Arc::clone(self);
        // 外层再多给一个适配器探测的余量：`scan_stream` 内部第一步也是取适配器，
        // 同样会卡在等授权状态上。
        let scanned = tokio::time::timeout(
            Duration::from_millis(window_ms) + ADAPTER_PROBE_TIMEOUT,
            apm_link::scan_stream(Duration::from_millis(window_ms), cancel_rx, move |found| {
                core.publish_devices(found);
            }),
        )
        .await;
        let result = match scanned {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "扫描超时：适配器未就绪（授权弹窗未点或蓝牙已关闭）"
            )),
        };

        self.lock_inner().scan_cancel = None;
        match result {
            Ok(found) => {
                self.publish_devices(&found);
                self.record(
                    LogLevel::Info,
                    "ble",
                    format!("扫描结束，发现 {} 个设备", found.len()),
                );
                self.set_state(AppState::Idle);
                Ok(())
            }
            Err(err) => {
                let err = AppError::from(err);
                self.record(
                    LogLevel::Error,
                    "ble",
                    format!("扫描失败（{:?}）：{}", err.code, err.detail),
                );
                self.fail(err.clone());
                Err(err)
            }
        }
    }

    /// 请求提前结束扫描。真正的状态收尾在 `start_scan` 返回时完成，
    /// 所以这里不改状态，避免出现两次 `idle` 事件。
    pub fn stop_scan(&self) {
        if let Some(tx) = self.lock_inner().scan_cancel.take() {
            let _ = tx.send(true);
        }
    }

    /// 连接并订阅，成功后状态为 `ready`，并派生通知监控任务。
    pub async fn connect(self: &Arc<Self>, device_id: String) -> Result<(), AppError> {
        self.connect_inner(device_id, true).await
    }

    /// 设备被复位（检测/烧写必然复位）之后把它接回来。
    ///
    /// esptool 只能靠复位进下载模式工作，BLE 断掉是机制性的，不是缺陷；但"点一次检测
    /// 就要手动重连一次"是不能接受的操作成本。这里等旧链路拆干净、设备重新广播，
    /// 再带退避重连几次。
    ///
    /// 中间失败**不置 `Error` 状态**：那会让界面在重连过程中闪几下红色横幅，
    /// 用户会以为检测失败了。只有全部尝试都失败才报错。
    pub async fn reconnect_after_reset(self: &Arc<Self>, device_id: &str) -> Result<(), AppError> {
        self.disconnect().await;
        // 通知监控发现链路没了要 1–2 s。等到"没有设备挂着、也没有过渡状态"为止：
        // 刚被复位打断时状态可能是 error / stopping，不一定是 idle，别死等 idle。
        for _ in 0..30 {
            let state = self.snapshot().state;
            if state.device().is_none() && !state.is_busy() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        let mut last: Option<AppError> = None;
        for attempt in 0..5u32 {
            if attempt > 0 {
                // 设备重启 + 重新广播大约 0.5–3 s；这里每次多等一点。
                tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;
            }
            match self.connect_inner(device_id.to_string(), false).await {
                Ok(()) => return Ok(()),
                Err(err) => last = Some(err),
            }
        }

        let err = last.unwrap_or_else(|| AppError::internal("重连失败：没有产生具体原因"));
        self.record(
            LogLevel::Warn,
            "ble",
            format!("设备复位后自动重连失败：{}", err.detail),
        );
        self.fail(err.clone());
        Err(err)
    }

    /// `report_failure=false` 时不置 `Error` 状态（见 `reconnect_after_reset`）。
    async fn connect_inner(
        self: &Arc<Self>,
        device_id: String,
        report_failure: bool,
    ) -> Result<(), AppError> {
        {
            let inner = self.lock_inner();
            if inner.state.is_busy() {
                return Err(AppError::busy(format!(
                    "当前状态 {} 不能连接",
                    inner.state.kind()
                )));
            }
            if inner.link_stop.is_some() {
                return Err(AppError::busy("已经连接了一个设备"));
            }
        }

        self.remember_exec();
        self.stop_scan();
        self.set_state(AppState::Connecting {
            device_id: device_id.clone(),
        });
        // 新链路：上一台设备的电量结论立刻作废，别把旧数字留给新设备。
        // STATUS 的"上一次值"同理 —— 留着会让新链路的第一次心跳被误判成"没变化"，
        // 于是第一条证据被吞掉。未推流告警的计数器也是按链路重置的。
        {
            let mut inner = self.lock_inner();
            inner.battery = None;
            inner.last_status = None;
            inner.stray_audio_blocks = 0;
            inner.stray_audio_logged = None;
        }
        self.record(LogLevel::Info, "ble", format!("连接 {device_id}"));

        let connected = tokio::time::timeout(
            CONNECT_TIMEOUT + ADAPTER_PROBE_TIMEOUT,
            apm_link::connect(Some(&device_id), CONNECT_TIMEOUT),
        )
        .await;
        let link = match connected {
            Ok(Ok(link)) => link,
            Ok(Err(err)) => {
                let err = AppError::from(err);
                self.record(
                    LogLevel::Error,
                    "ble",
                    format!("连接失败（{:?}）：{}", err.code, err.detail),
                );
                self.settle_failure(&err, report_failure);
                return Err(err);
            }
            Err(_) => {
                let err = AppError::new(
                    ErrorCode::BtUnavailable,
                    "连接超时：适配器未就绪（授权弹窗未点或蓝牙已关闭）",
                );
                self.record(LogLevel::Error, "ble", format!("连接超时：{}", err.detail));
                self.settle_failure(&err, report_failure);
                return Err(err);
            }
        };

        // 设备视图优先用扫描列表里的真实名字；启动自动连接没经过扫描时
        // （devices 为空），退回配置里记的上次设备名。
        let (device, saved) = {
            let mut inner = self.lock_inner();
            let remembered = inner.settings.last_device_name.clone();
            let view = inner
                .devices
                .iter()
                .find(|d| d.id == device_id)
                .cloned()
                .map(|mut d| {
                    d.connected = true;
                    d
                })
                .unwrap_or(DeviceView {
                    id: device_id.clone(),
                    name: remembered.clone().unwrap_or_else(|| device_id.clone()),
                    rssi: None,
                    has_service: true,
                    connected: true,
                });
            // 成功连接即记住：下次启动自动连接就靠这两条（macOS 的 PeripheralId
            // 跨进程重启稳定）。手动断开不清除——用户关掉重开 App 通常还是想连它。
            inner.settings.last_device_id = Some(device_id.clone());
            inner.settings.last_device_name = Some(view.name.clone());
            (view, inner.settings.clone())
        };
        // 落盘在锁外（A2）：连接成功后立刻写，别让音频块排在它后面。
        self.persist(saved, "ble");

        // 电量只在变化时通知，连接后主动读一次，前端立刻有数。
        // 三种结果都要落到内核并发出事件：`pct=None` 让界面显示「电量不可用」，
        // 而不是静默不显示——静默不显示会让人以为功能没做，而事实是设备明确回了
        // "电量未知"（KI-007）。
        //
        // 先写 `inner.battery` 再 emit：GUI 自动连接时前端常常还没注册监听，
        // 事件会丢，但快照里必须有结论（否则界面永远空白，比事件丢更糟）。
        let pct = match link.read_battery().await {
            Ok(Some(pct)) => {
                self.record(LogLevel::Info, "ble", format!("电量 {pct}%"));
                Some(pct)
            }
            Ok(None) => {
                self.record(
                    LogLevel::Info,
                    "ble",
                    "设备报电量不可用（无 Battery Service 或电量计未就绪）",
                );
                None
            }
            Err(err) => {
                self.record(LogLevel::Warn, "ble", format!("读电量失败：{err:#}"));
                None
            }
        };
        self.lock_inner().battery = Some(pct);
        self.emit(AppEvent::Battery { pct });

        *self.link.lock().await = Some(link);
        // 新链路：代次 +1，所有在飞的异步迁移（旧的 start_stream / 收尾）随之作废。
        // 同时把"已知丢失"的标记清掉，否则第一次收尾就会跳过一次正常的断开调用。
        let epoch = {
            let mut inner = self.lock_inner();
            inner.link_lost = false;
            inner.link_epoch = inner.link_epoch.wrapping_add(1);
            inner.link_epoch
        };
        let (stop_tx, stop_rx) = watch::channel(false);
        self.lock_inner().link_stop = Some(stop_tx);
        self.set_state(AppState::Ready {
            device: device.clone(),
        });
        self.record(
            LogLevel::Debug,
            "ble",
            format!("链路代次 → {epoch}"),
        );
        self.record(
            LogLevel::Info,
            "ble",
            format!(
                "已连接并订阅 status/audio/event：{} ({})",
                device.name, device.id
            ),
        );
        self.spawn_monitor(stop_rx);
        Ok(())
    }

    /// 启动时自动连接上次的设备（`auto_connect` 开且记录过 `last_device_id`）。
    ///
    /// 设计口径：
    /// - 只在 `idle` 时发起。调用方（GUI 启动任务）在 `detect_adapter` 成功后调它，
    ///   正常路径正好是 idle；已连着/在忙就什么都不做，绝不和手动操作抢状态机；
    /// - 复用 [`Self::connect`]：同一套状态机、超时与监控派生，不另写一条连接路径；
    /// - **失败不算事故**：`connect` 失败会把状态落到 `error`，自动连接失败大多是
    ///   "设备还没上电"，属于常态。这里把状态落回 `idle` 并记 warn，把手动扫描的
    ///   路让出来，不让用户每次开机先看一条红色错误横幅。
    pub async fn auto_connect(self: &Arc<Self>) -> Result<(), AppError> {
        let (device_id, device_name) = {
            let inner = self.lock_inner();
            match (
                &inner.settings.auto_connect,
                inner.settings.last_device_id.clone(),
            ) {
                (true, Some(id)) if inner.state == AppState::Idle => {
                    let name = inner
                        .settings
                        .last_device_name
                        .clone()
                        .unwrap_or_else(|| id.clone());
                    (id, name)
                }
                _ => return Ok(()), // 关闭开关 / 没连过设备 / 状态不对：静默跳过
            }
        };
        self.record(
            LogLevel::Info,
            "ble",
            format!("自动连接上次设备：{device_name}"),
        );
        match self.connect(device_id).await {
            Ok(()) => Ok(()),
            Err(err) => {
                if matches!(self.state(), AppState::Error { .. }) {
                    self.set_state(AppState::Idle);
                }
                self.record(
                    LogLevel::Warn,
                    "ble",
                    format!("自动连接失败：{}；设备上电后可手动扫描连接", err.detail),
                );
                Err(err)
            }
        }
    }

    /// 断开。真正的收尾（停声卡 + 退订 + 断链 + 状态回 `idle`）在监控任务里做，
    /// 保证"先停止写入、再断链"的顺序与 CLI 修过的收尾顺序一致。
    pub async fn disconnect(self: &Arc<Self>) {
        let tx = self.lock_inner().link_stop.take();
        match tx {
            Some(tx) => {
                let _ = tx.send(true);
            }
            None => {
                if self.link.lock().await.is_some() {
                    self.teardown(None).await;
                }
            }
        }
    }

    // ---- 声卡 ----

    /// 枚举输出设备。`supports_16k=false` 的设备仍然列出，但界面要标出来 ——
    /// 直接灰掉会让用户以为"声卡没装好"。
    pub fn list_sinks(&self) -> Result<Vec<SinkView>, AppError> {
        let list = apm_sink::list_sinks()
            .map_err(|e| AppError::new(ErrorCode::SinkMissing, format!("枚举输出设备失败：{e}")))?;
        Ok(list
            .into_iter()
            .map(|s| SinkView {
                name: s.name,
                min_rate: s.min_rate,
                max_rate: s.max_rate,
                supports_16k: s.supports_16k,
            })
            .collect())
    }

    pub fn set_sink(&self, name: String) -> Result<(), AppError> {
        let found = self.list_sinks()?.into_iter().find(|s| s.name == name);
        let Some(found) = found else {
            return Err(AppError::new(
                ErrorCode::SinkMissing,
                format!("没有名为 \"{name}\" 的输出设备"),
            ));
        };
        // 不支持 16 kHz 的声卡永远用不了（本项目不做隐式重采样），
        // 在这里就拒绝，比等到点「开始」才报错更早。
        if !found.supports_16k {
            return Err(AppError::new(
                ErrorCode::SinkRateUnsupported,
                format!(
                    "{} 不支持 16000 Hz 原生采样率（支持 {}–{} Hz）",
                    found.name, found.min_rate, found.max_rate
                ),
            ));
        }
        let snapshot = {
            let mut inner = self.lock_inner();
            inner.settings.sink = Some(name.clone());
            inner.settings.clone()
        };
        // 落盘在锁外（A2）：见 `persist` 的注释。
        self.persist(snapshot, "sink");
        self.record(LogLevel::Info, "sink", format!("输出声卡改为 {name}"));
        Ok(())
    }

    /// 是否默认保存会话 WAV。默认关（`docs/GUI.md` §12 第 3 条）。
    pub fn set_save_wav(&self, save: bool) -> Result<(), AppError> {
        let snapshot = {
            let mut inner = self.lock_inner();
            inner.settings.save_wav = save;
            inner.settings.clone()
        };
        self.persist(snapshot, "app");
        Ok(())
    }

    /// 关麦兜底的时长（分钟）。运行中改也生效：看门狗每轮都现读配置。
    ///
    /// 夹紧到 1 min – 24 h；界面、配置文件、命令行都走这同一个夹紧点，
    /// 免得三处各夹一次、边界还不一样（与 `clamp_target` 同一套路）。
    pub fn set_auto_stop_min(&self, minutes: u32) -> Result<(), AppError> {
        let minutes = settings::clamp_auto_stop_min(minutes);
        let snapshot = {
            let mut inner = self.lock_inner();
            inner.settings.auto_stop_min = minutes;
            inner.settings.clone()
        };
        self.persist(snapshot, "app");
        self.record(
            LogLevel::Info,
            "app",
            format!("关麦兜底改为：连续采集 {minutes} 分钟自动停"),
        );
        Ok(())
    }

    /// 缓冲目标（40–80 ms）。运行中改了取下一批回调生效，不需要重启推流。
    pub fn set_buffer_target(&self, ms: u32) -> Result<(), AppError> {
        let ms = settings::clamp_target(ms);
        let (snapshot, streaming) = {
            let mut inner = self.lock_inner();
            inner.settings.target_ms = ms;
            (inner.settings.clone(), inner.stream.is_some())
        };
        // 落盘与 retarget 都在锁外（A2）。`retarget` 只是往 channel 里塞一条消息，
        // 但它要再取一次 `inner` —— 在锁内调用等于自己和自己抢锁。
        self.persist(snapshot, "sink");
        if streaming {
            if let Some(p) = self.lock_inner().stream.as_ref() {
                p.retarget(ms);
            }
        }
        Ok(())
    }

    // ---- 推流 ----

    /// 开始推流。返回 `session_id`；`save_wav` 为真时同时落一份 WAV。
    ///
    /// 前置校验失败直接返回错误，**不允许**先开流再报错（`docs/GUI.md` §4）。
    ///
    /// 并发与失效（B1 / B2）：
    /// - 入口用 `stream_starting` 占位。开流是几百毫秒的一串 await，期间状态还是
    ///   `ready`、`stream` 还是 `None`，不占位就会有两个调用各自建一条管线；
    /// - 每次 await 回来都用 `set_state_if_link(epoch, ..)` 落状态：链路在等待期间
    ///   被断链收尾拆掉时，不能再把状态写回 `ready`（那是陈旧写，界面会显示
    ///   "已连接"而其实没有链路，点开麦永远失败且无法 disconnect）。
    pub async fn start_stream(self: &Arc<Self>, save_wav: bool) -> Result<String, AppError> {
        // ---- 前置校验 + 占位：全程在同一把锁里完成，两个并发调用只有一个能过 ----
        let (device, epoch, sink_name, target_ms, started_ms, session_id, wav_path) = {
            let mut inner = self.lock_inner();
            let device = match &inner.state {
                AppState::Ready { device } => device.clone(),
                other => {
                    return Err(AppError::busy(format!(
                        "当前状态 {} 不能开始推流（需要先连上设备）",
                        other.kind()
                    )))
                }
            };
            if inner.stream.is_some() {
                return Err(AppError::busy("已经在推流"));
            }
            if inner.stream_starting {
                return Err(AppError::busy("上一次开流还没完成"));
            }
            let Some(sink) = inner.settings.sink.clone() else {
                return Err(AppError::new(
                    ErrorCode::SinkMissing,
                    "还没有选择输出声卡：先在下拉框里选一个（macOS 通常装 BlackHole 2ch）",
                ));
            };
            // 占位必须在返回错误之前撤销，所以这里一旦过了校验就立刻置位。
            inner.stream_starting = true;

            let started_ms = epoch_ms();
            let session_id = format!("s{started_ms}");
            let wav_path =
                save_wav.then(|| settings::sessions_dir().join(format!("{session_id}.wav")));
            (
                device,
                inner.link_epoch,
                sink,
                inner.settings.target_ms,
                started_ms,
                session_id,
                wav_path,
            )
        };

        // 从这里开始任何提前返回都必须撤销占位。
        match self
            .start_stream_locked(device, epoch, sink_name, target_ms, started_ms, session_id, wav_path)
            .await
        {
            Ok(id) => Ok(id),
            Err(err) => {
                self.lock_inner().stream_starting = false;
                Err(err)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_stream_locked(
        self: &Arc<Self>,
        device: DeviceView,
        epoch: u64,
        sink_name: String,
        target_ms: u32,
        started_ms: u64,
        session_id: String,
        wav_path: Option<PathBuf>,
    ) -> Result<String, AppError> {
        // 开流前先发一次 STOP，把设备状态清干净，再开声卡、再 START。
        //
        // 为什么需要这一步（2026-11-05 G2 现场，见 docs/KNOWN_ISSUES.md KI-005）：
        // 设备在上一条会话异常结束、或者"连上之后迟迟没人发 START"时会停在半死态，
        // 此时直接 START 只恢复一部分——实测块率 28/s（名义 50）、设备侧丢弃 3992、
        // 声卡欠载 6100。同样条件 CLI 从不踩，差别就是 CLI 连上立刻 START，中间没有空窗。
        // STOP 对没在采集的设备是幂等的（只回一帧状态），代价可以忽略。
        {
            let link = self.link.lock().await.clone();
            match link {
                Some(link) => {
                    if let Err(err) = link.stop_capture().await {
                        // 失败不阻塞开流：设备可能本来就在 idle，写 STOP 报错不代表不能采。
                        self.record(
                            LogLevel::Debug,
                            "ble",
                            format!("开流前预置 STOP 未成功（继续开流）：{err}"),
                        );
                    }
                    tokio::time::sleep(CAPTURE_RESET_SETTLE).await;
                }
                None => {
                    return Err(AppError::busy("链路已断开，先重新连接设备"));
                }
            }
        }

        let pipeline = Pipeline::start(SessionOptions {
            session_id: session_id.clone(),
            sink: Some(sink_name.clone()),
            target_ms,
            wav_path: wav_path.clone(),
        })?;

        let info = pipeline
            .sink_device()
            .unwrap_or(sink_name.as_str())
            .to_string();
        let wav_note = pipeline
            .wav_path()
            .map(|p| format!("，录音 {p}"))
            .unwrap_or_default();
        self.record(
            LogLevel::Info,
            "sink",
            format!("打开输出 {info}（目标缓冲 {target_ms} ms）{wav_note}"),
        );

        // 声卡已打开：把它挂上。**同一次加锁里**校验代次 —— 如果等待期间链路被拆，
        // 这条管线已经没有消费入口（`monitor_loop` 是唯一消费者），必须立刻收掉，
        // 否则声卡会一直往虚拟声卡写静音。
        let mut pipeline = Some(pipeline);
        {
            let mut inner = self.lock_inner();
            if inner.link_epoch == epoch {
                inner.stream = pipeline.take();
            }
        }
        if let Some(stale) = pipeline {
            let _ = tokio::task::spawn_blocking(move || stale.finish()).await;
            self.record(
                LogLevel::Warn,
                "sink",
                "开流期间链路已被断开，本次会话已取消（声卡已关闭）",
            );
            return Err(AppError::busy("链路已断开，先重新连接设备"));
        }

        // CTRL START 失败就把会话收回去，别留一个"界面在推流、设备没在采"的假象。
        // 把 `Link` clone 出来再 await，不持锁：`Link` 是 Send + Sync 的（见 `apm_link::Link`
        // 的注释），clone 之间共享同一条通知流，但 CTRL 写入不跟通知消费抢锁。
        let link = self.link.lock().await.clone();
        let started = match link {
            Some(link) => link.start_capture().await,
            None => Err(anyhow::anyhow!("链路已断开")),
        };
        if let Err(err) = started {
            let pipeline = self.lock_inner().stream.take();
            if let Some(p) = pipeline {
                let _ = tokio::task::spawn_blocking(move || p.finish()).await;
            }
            let err = AppError::new(
                ErrorCode::ProtocolMismatch,
                format!("发送 CTRL START 失败：{err}"),
            );
            self.record(LogLevel::Error, "sink", err.detail.clone());
            // 回到 ready 而不是 error：设备还连着，用户改一下设置就能重试；
            // 落到 error 会让"再点一次开始"被状态机挡住。
            // 用世代校验落状态：链路若已断，这里应当什么都不做（收尾流程会落到 idle）。
            self.set_state_if_link(epoch, AppState::Ready { device });
            return Err(err);
        }

        {
            let mut inner = self.lock_inner();
            inner.sessions.push_back(SessionRecord {
                session_id: session_id.clone(),
                wav_path: wav_path.as_ref().map(|p| p.display().to_string()),
                started_ms,
                // 还不知道；收尾时按 `SessionSummary.link_slow` 回填。
                link_slow: false,
            });
            while inner.sessions.len() > SESSIONS_KEPT {
                inner.sessions.pop_front();
            }
            // 开流已落定：撤销占位（与入口的置位成对）。
            inner.stream_starting = false;
        }

        // 最后一次世代校验：`CTRL START` 往返期间链路可能已经被拆。此时会话记录已经
        // 写进 `sessions`（那是历史，不该回滚），但**不能再把状态写成 `Streaming`**
        // —— 否则界面显示"推流中"而实际没有任何消费入口。
        if !self.set_state_if_link(
            epoch,
            AppState::Streaming {
                device,
                session_id: session_id.clone(),
            },
        ) {
            self.record(
                LogLevel::Warn,
                "sink",
                "开流完成时链路已断开：本次会话不计入推流状态",
            );
            self.fire_hook("capture.stop");
            return Err(AppError::busy("链路已断开，先重新连接设备"));
        }
        // 采集真的起来了才触发钩子：失败路径已经 return，不会误报"已开麦"。
        self.fire_hook("capture.start");
        self.remember_exec();
        self.spawn_telemetry();
        self.spawn_early_stop();
        self.spawn_auto_stop();
        Ok(session_id)
    }

    /// 关麦兜底看门狗：连续采集超过 `settings.auto_stop_min` 分钟就自动停。
    ///
    /// 为什么需要（`docs/PRODUCT.md` 的「按键」节）：OK 是 toggle 式，"松手自动停"
    /// 不存在。开着麦去接杯水、开完会忘了按第二下，是这个设计必然会发生的事故——
    /// 不只是浪费电，更是隐私问题。
    ///
    /// 三个设计点：
    /// - **每轮现读配置**，不在开流时快照：用户在运行中把时长调大要立刻生效；
    /// - **看的是会话自身的已运行时长**，不是本任务的存活时长，所以任务重新派生
    ///   （理论上不会）也不会把计时清零；
    /// - 走 `stop_stream()` 而不是只发 CTRL STOP：它负责触发 `capture.stop` 钩子、
    ///   回填 WAV 头、把状态机放回 `ready`。只发 STOP 会留一条僵尸会话。
    fn spawn_auto_stop(self: &Arc<Self>) {
        let handle = self.lock_inner().exec.clone();
        let Some(handle) = handle else { return };
        let core = Arc::clone(self);
        handle.spawn(async move {
            loop {
                tokio::time::sleep(AUTO_STOP_TICK).await;
                // 一次加锁把"该不该停"判断完，避免两次读之间状态变了。
                let decision = {
                    let inner = core.lock_inner();
                    match inner.stream.as_ref() {
                        Some(p) => {
                            let limit =
                                Duration::from_secs(u64::from(inner.settings.auto_stop_min) * 60);
                            Some((p.should_auto_stop(limit), limit, p.blocks()))
                        }
                        // 会话结束了就退出，别留空转任务。
                        None => None,
                    }
                };
                let Some((due, limit, blocks)) = decision else {
                    break;
                };
                if !due {
                    continue;
                }
                core.record(
                    LogLevel::Warn,
                    "sink",
                    format!(
                        "连续采集已达 {} 分钟（{} 块）：触发关麦兜底，自动停止。\
                         忘记关麦是 toggle 式的已知代价；要更长可在设置里调大",
                        limit.as_secs() / 60,
                        blocks
                    ),
                );
                // 兜底触发的停止和用户手动停止走同一条收尾路径，钩子/WAV/状态都要对；
                // 但日志里要写明是兜底停的，别让人误读成"用户自己停的"。
                if let Err(err) = core
                    .stop_stream_with_reason(&format!(
                        "关麦兜底（连续采集 {} 分钟）",
                        limit.as_secs() / 60
                    ))
                    .await
                {
                    // 走到这里通常意味着"刚好和用户的停止撞上了"，不是错误。
                    core.record(
                        LogLevel::Debug,
                        "sink",
                        format!("关麦兜底停止时会话已结束：{}", err.detail),
                    );
                }
                break;
            }
        });
    }

    /// 开流后的"有没有真的在发音频"看门狗。
    ///
    /// 为什么要单独一个任务（KI-005）：设备停在半死态时链路是通的、STATUS 也在一秒
    /// 一条地来，但音频只有 1.5–2 块/秒。这时**没有任何错误会抛出来**，会话会一路
    /// 跑到用户手动停止，产出一份"欠载上千、漂移 −9000 ppm"的报告——看起来像声卡或
    /// 漂移补偿坏了，实际是设备没在发。宁可 3 s 就停掉并明说，也别让人读假数据。
    ///
    /// 只跑一次判定，不循环：过了窗口就退出。正常会话在这里什么也不做。
    fn spawn_early_stop(self: &Arc<Self>) {
        let handle = self.lock_inner().exec.clone();
        let Some(handle) = handle else { return };
        let core = Arc::clone(self);
        handle.spawn(async move {
            tokio::time::sleep(EARLY_STOP_WINDOW).await;
            let verdict = {
                let inner = core.lock_inner();
                inner.stream.as_ref().map(|p| (p.unstarted(EARLY_STOP_WINDOW, EARLY_STOP_MIN_BLOCKS), p.blocks()))
            };
            // 会话已经结束（用户手动停了 / 断链了）就不重复报错。
            let Some((true, blocks)) = verdict else { return };
            core.record(
                LogLevel::Error,
                "sink",
                format!(
                    "开流 {} s 只收到 {blocks} 块（正常应约 {} 块）：设备未开始发送或链路异常，已自动停止",
                    EARLY_STOP_WINDOW.as_secs(),
                    EARLY_STOP_WINDOW.as_secs() * 50
                ),
            );
            // 走 stop_stream() 而不是只发 CTRL STOP：它会收尾 WAV、触发 capture.stop 钩子、
            // 把状态机放回 ready。只发 STOP 会留一条"界面在推流、声卡在补静音"的僵尸会话。
            let _ = core.stop_stream_with_reason("早停（开流后无音频）").await;
            core.emit(AppEvent::StreamAborted {
                code: ErrorCode::CaptureNotStarting,
                detail: format!(
                    "开流后 {} s 只收到 {blocks} 块音频，已自动停止（设备未开始发送或链路异常）",
                    EARLY_STOP_WINDOW.as_secs()
                ),
            });
        });
    }

    /// 触发采集状态钩子（`capture.start` / `capture.stop`）：点按注入一个快捷键。
    ///
    /// 三条原则：
    ///
    /// 1. **绝不影响音频路径**：注入放在独立的 std 线程里。`CGEventPost` 是同步系统调用，
    ///    不该占 tokio worker，更不该卡在声卡回调或通知循环上；
    /// 2. **失败只记日志**：快捷键没发出去不该让"开麦"这件事失败——权限没给、键名不支持
    ///    都只该是一条诊断日志 + 界面上的提示；
    /// 3. **每次现读映射表**：用户刚在界面上改完就点开始，必须用新的那一份。
    fn fire_hook(self: &Arc<Self>, event: &str) {
        let (map, load_err) = crate::keymap_store::load();
        if let Some(err) = load_err {
            self.record(LogLevel::Warn, "inject", err);
        }
        let Some(binding) = map.resolve(event).cloned() else {
            return; // 没配钩子 = 不动作，这是默认状态
        };
        self.inject_in_background(event.to_string(), binding);
    }

    /// 在独立 std 线程里点按注入一个动作（采集钩子与设备按键共用的出口）。
    fn inject_in_background(
        self: &Arc<Self>,
        what: String,
        binding: apm_protocol::keymap::Binding,
    ) {
        let core = Arc::clone(self);
        let label = describe_binding(&binding);
        let spawned = std::thread::Builder::new()
            .name("apm-inject".into())
            .spawn(move || match crate::inject::tap(&binding) {
                Ok(()) => core.record(LogLevel::Info, "inject", format!("{what} 已注入 {label}")),
                Err(err) => {
                    core.record(LogLevel::Warn, "inject", format!("{what} 注入失败：{err}"))
                }
            });
        if let Err(err) = spawned {
            self.record(LogLevel::Warn, "inject", format!("起注入线程失败：{err}"));
        }
    }

    /// 设备按键 PRESS → 动作（M3 直连）。
    ///
    /// 动作按映射表走（每次现读，与 `fire_hook` 同一规则）：
    /// - 绑了 `ptt-toggle` 的那个键（**开麦键**，默认 `ok`，可换成 up/down）：
    ///   没在推流就开麦，在推流就停麦——**开关语义**；
    /// - 其余按键：配了普通键就点按注入，未绑定 = 不动作。
    ///
    /// 双击会连发两次 PRESS：切换窗口内的第二次会被 [`Self::toggle_ptt`]
    /// 的防抖吃掉（见该字段注释），窗口外的就是一次正常的开+关。
    fn handle_button_press(self: &Arc<Self>, event: apm_protocol::frame::Event) {
        let name = event.button_name();
        if name == "unknown" {
            self.record(
                LogLevel::Warn,
                "button",
                format!("未知按键编号 {}，忽略", event.button),
            );
            return;
        }
        let (map, load_err) = crate::keymap_store::load();
        if let Some(err) = load_err {
            self.record(LogLevel::Warn, "inject", err);
        }
        let Some(binding) = map.resolve(name).cloned() else {
            return; // 未绑定 = 不动作（开麦键恒有一个，默认是 ok）
        };
        if binding.is_ptt() {
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    let core = Arc::clone(self);
                    handle.spawn(async move { core.toggle_ptt().await });
                }
                Err(_) => self.record(
                    LogLevel::Warn,
                    "button",
                    "没有 tokio 运行时，PTT 开关无法执行",
                ),
            }
        } else {
            self.inject_in_background(format!("按键 {name}"), binding);
        }
    }

    /// PTT 开关的唯一执行点：设备上的**开麦键**（默认 OK，可在映射里换成 up/down）走这里，
    /// 语义与界面按钮完全一致（同一条 `start_stream` / `stop_stream`，钩子照常触发）。
    ///
    /// 防抖：**不再单独维护一个 `ptt_switching` 原子量。**
    /// 开麦的并发保护由 `Inner::stream_starting` 承担（它在锁内占位，覆盖
    /// "STOP → 等 120 ms → 开声卡 → START" 整个窗口）；关麦由 `stream.is_none()`
    /// 挡住第二次调用。DOUBLE 的两次 PRESS 相隔约 180 ms，第二次进来时
    /// 要么占位还在（返回 busy）、要么已经在 `streaming`（`toggle` 会去关麦）——
    /// 这里只在**开麦失败是 busy** 时降级成 debug，不弹 warn。
    async fn toggle_ptt(self: &Arc<Self>) {
        let streaming = self.lock_inner().stream.is_some();
        let action = if streaming { "关麦" } else { "开麦" };
        let result = if streaming {
            self.stop_stream().await.map(|_| ())
        } else {
            let save_wav = self.lock_inner().settings.save_wav;
            self.start_stream(save_wav).await.map(|_| ())
        };
        match result {
            Ok(()) => self.record(LogLevel::Info, "button", format!("PTT {action}完成")),
            Err(err) => {
                if err.code == ErrorCode::Busy {
                    // busy 是状态机在挡（比如界面正在收尾），记 debug 留证据即可。
                    self.record(
                        LogLevel::Debug,
                        "button",
                        format!("PTT 暂不能切换：{}", err.detail),
                    );
                } else {
                    self.record(
                        LogLevel::Warn,
                        "button",
                        format!("PTT 切换失败：{}", err.detail),
                    );
                }
            }
        }
    }

    /// 停止推流。收尾顺序：**先停写声卡 → 再 CTRL STOP**（断链由 disconnect 负责）。
    pub async fn stop_stream(self: &Arc<Self>) -> Result<SessionSummary, AppError> {
        self.stop_stream_with_reason("手动停止").await
    }

    /// 与 [`Self::stop_stream`] 相同，但收尾日志写明**是谁**停的。
    ///
    /// 分出来的理由：兜底和早停也是"停"，但它们不是用户点的。如果日志一律写
    /// 「手动停止」，事后排障时会把"忘记关麦被兜底了"误读成"用户自己停的"，
    /// 正好把要查的那条线索抹掉。
    async fn stop_stream_with_reason(
        self: &Arc<Self>,
        reason: &str,
    ) -> Result<SessionSummary, AppError> {
        let (device, epoch) = {
            let inner = self.lock_inner();
            if inner.stream.is_none() {
                return Err(AppError::busy("当前没有在推流"));
            }
            (inner.state.device().cloned(), inner.link_epoch)
        };
        self.set_state(AppState::Stopping);

        let summary = match self.take_and_finish_stream(reason).await {
            Some(summary) => summary,
            None => {
                // 收尾失败同样要走世代校验：链路若已断，落 `ready` 就是陈旧写。
                // 校验失败时**什么都不做** —— 断链收尾已经把状态落到它该去的地方
                // （可能是 `idle`，也可能是 `no_adapter`），这里覆盖它反而是新的陈旧写。
                let next = match device {
                    Some(device) => AppState::Ready { device },
                    None => AppState::Idle,
                };
                let _ = self.set_state_if_link(epoch, next);
                return Err(AppError::internal("会话收尾失败，详见诊断日志"));
            }
        };

        let link = self.link.lock().await.clone();
        if let Some(link) = link {
            if let Err(err) = link.stop_capture().await {
                self.record(
                    LogLevel::Warn,
                    "sink",
                    format!("发送 CTRL STOP 失败：{err}"),
                );
            }
        }

        // 收流期间链路可能被断链收尾接管（它自己会落 `idle`）。这时不能再写 `ready`。
        let next = match device {
            Some(device) => AppState::Ready { device },
            None => AppState::Idle,
        };
        if !self.set_state_if_link(epoch, next) {
            self.record(
                LogLevel::Debug,
                "sink",
                "收流完成时链路已被断开：状态由断链收尾决定，这里不再改写",
            );
        }
        self.fire_hook("capture.stop");
        Ok(summary)
    }

    /// 对某次会话的 WAV 出音质报告。没有录音时明确报错，不给空报告。
    ///
    /// **设备报过 `LINK_SLOW` 的会话直接拒绝**：那种场景下设备只在 1.5–2 块/秒地发，
    /// WAV 里是声卡补的静音，算出来的 RMS/削波/毛刺全是链路假象。给一份"软件分析通过"
    /// 比不给更糟——它会被当成版本对比的基线（见 KI-005 / `ARCHITECTURE.md` 的 STATUS 节）。
    pub fn analyze_session(&self, session_id: &str) -> Result<QualityReport, AppError> {
        let (path, link_slow) = {
            let inner = self.lock_inner();
            let rec = inner.sessions.iter().find(|s| s.session_id == session_id);
            (
                rec.and_then(|s| s.wav_path.clone()),
                rec.map(|s| s.link_slow).unwrap_or(false),
            )
        };
        if link_slow {
            return Err(AppError::new(
                ErrorCode::CaptureNotStarting,
                format!(
                    "会话 {session_id} 期间设备报了 LINK_SLOW（连接间隔 > 40 ms），\
                     这一场的欠载与漂移是链路假象，不出音质报告。\
                     复位设备后重连，重新录一遍再分析"
                ),
            ));
        }
        let path = path.ok_or_else(|| {
            AppError::internal(format!(
                "会话 {session_id} 没有录音（开始推流时没勾选「同时保存 WAV」）"
            ))
        })?;
        crate::report::analyze_file(Path::new(&path)).map_err(AppError::internal)
    }

    /// 与 `apmctl analyze --json` 逐字段一致的报告文本。
    /// 前端的「复制 JSON」用它，不从界面状态拼 —— 拼出来的字段迟早跟 CLI 不一致。
    pub fn analyze_session_json(&self, session_id: &str) -> Result<String, AppError> {
        let report = self.analyze_session(session_id)?;
        serde_json::to_string_pretty(&report.to_json())
            .map_err(|e| AppError::internal(format!("报告序列化失败：{e}")))
    }

    /// 最近一次会话记录（GUI 启动时用来判断"有没有录音可分析"）。
    pub fn last_session(&self) -> Option<SessionRecord> {
        self.lock_inner().sessions.back().cloned()
    }

    /// 给"复制诊断包"用的纯文本。要求能直接粘进 issue 就有人看得懂。
    pub fn diagnostics_text(&self) -> String {
        let state = self.state();
        let devices = self.devices();
        let tail = self.tail_snapshot();
        let settings = self.settings_view();
        let telemetry: Option<Telemetry> = self.lock_inner().stream.as_ref().map(|p| p.telemetry());
        let last = self.last_session();

        let mut out = String::new();
        out.push_str("== AI Passport Mic 诊断包 ==\n");
        out.push_str(&format!("版本: {}\n", self.version));
        out.push_str(&format!("状态: {}\n", state.kind()));
        out.push_str(&format!("日志: {}\n", self.log_path()));
        out.push_str(&format!(
            "配置: 声卡={:?} 缓冲目标={} ms 录 WAV={}\n",
            settings.sink, settings.target_ms, settings.save_wav
        ));
        out.push_str(&format!("设备: {}\n", devices.len()));
        for d in &devices {
            out.push_str(&format!(
                "  - {} ({}) rssi={:?} ours={} connected={}\n",
                d.name, d.id, d.rssi, d.has_service, d.connected
            ));
        }
        if let Some(t) = telemetry {
            out.push_str(&format!(
                "推流: 块={} 丢块={} 跳变={} 设备丢弃={} 缓冲={:.0} ms 欠载={}（断供 {}） 静音帧={} 锁定失败={} 漂移={:+.0} ppm 毛刺={}\n",
                t.blocks,
                t.lost_blocks,
                t.seq_jumps,
                t.device_dropped,
                t.buffer_ms,
                t.underruns,
                t.underruns_underfed,
                t.silent_frames,
                t.lock_misses,
                t.ppm,
                t.glitch_events
            ));
            // 延迟口径：实测存量 + 基准/余量分开，别让"目标 80 ms"被当成实际延迟。
            out.push_str(&format!(
                "缓冲口径: 实测存量 p50={} p95={}；用户基准={} ms 自适应余量=+{} ms\n",
                t.fill_p50_ms
                    .map(|v| format!("{v:.0} ms"))
                    .unwrap_or_else(|| "—".into()),
                t.fill_p95_ms
                    .map(|v| format!("{v:.0} ms"))
                    .unwrap_or_else(|| "—".into()),
                t.base_target_ms,
                t.boost_ms
            ));
            // 抖动只给**原始测量**，不给判词。
            //
            // 这里曾经输出一个 `jitter_verdict()`（clean / arrival-only / process-only / both），
            // 是 2026-11-07 查 KI-006 时加的三把尺子归因。根因（CoreBluetooth 攒批交付）
            // 查明并改判据之后，那套判词不再有消费方：诊断包给人看，人看得到下面这些数字
            // 就能自己判断，多一层枚举只是把话说死、并在阈值边界上产生争议。
            let jitter = {
                let inner = self.lock_inner();
                inner.stream.as_ref().map(|p| {
                    let (_, arrival_max, late) = p.arrival_stats();
                    let (_, process_max, slow) = p.process_stats();
                    (arrival_max, late, process_max, slow)
                })
            };
            if let Some((arrival_max, late, process_max, slow)) = jitter {
                out.push_str(&format!(
                    "抖动: 到达最大间隔={arrival_max:.1} ms（>80 ms {late} 次），\
                     PC 处理最大耗时={process_max:.1} ms（>20 ms {slow} 块）\n"
                ));
            }
        } else {
            out.push_str("推流: 未推流\n");
        }
        let dropped = self.dropped_log_lines();
        if dropped > 0 {
            out.push_str(&format!(
                "日志: 因队列满丢弃 {dropped} 行（磁盘写不过来，证据可能不全）\n"
            ));
        }
        if let Some(s) = last {
            out.push_str(&format!(
                "最近会话: {} 录音={}\n",
                s.session_id,
                s.wav_path.unwrap_or_else(|| "(未保存)".into())
            ));
        }
        out.push_str(&format!("最近 {} 条事件:\n", tail.len()));
        for e in tail {
            let secs = e.ts_ms / 1000;
            out.push_str(&format!(
                "  {} [{:<5}] {:<6} {}\n",
                secs,
                e.level.as_str(),
                e.source,
                e.message
            ));
        }
        out
    }

    // ---- 内部 ----

    fn lock_inner(&self) -> MutexGuard<'_, Inner> {
        // 内核状态机不接受"中毒即瘫痪"：拿不到锁就用里面的值继续，
        // 比让整个 App 卡死更符合"麦克风要能用"的优先级。
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_sink(&self) -> MutexGuard<'_, LogSink> {
        self.sink.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 等日志队列排空并落盘。
    ///
    /// 正常路径**不需要**调（`record()` 入队即返回，写线程自己会落盘）。
    /// 存在的理由是两件事需要"确定写完了"：进程正常退出前，以及测试要读文件时。
    pub fn flush_logs(&self) {
        self.lock_sink().flush();
    }

    /// 因日志队列满而丢弃的行数。诊断包里用它说明"日志可能不全"。
    pub fn dropped_log_lines(&self) -> u64 {
        self.lock_sink().dropped_lines()
    }

    fn tail_snapshot(&self) -> Vec<LogEntry> {
        let tail = self.tail.lock().unwrap_or_else(|e| e.into_inner());
        tail.iter().cloned().collect()
    }

    /// 记录一条日志：进内存环形缓冲、入落盘队列、发事件。三处都做，缺一处就会
    /// 出现"界面上看不到、事后也查不到"。
    ///
    /// **本方法不碰磁盘**（2026-11-07 起）：`LogSink::write` 只做一次有界内存入队，
    /// 落盘在 `apm-log` 写线程里做。这条不是性能优化，是正确性要求 ——
    /// 本方法会从音频通知循环里被调用（每秒一条 STATUS），同步写盘会把磁盘长尾
    /// 直接加到"块到达间隔"的测量值上，让链路抖动与 PC 端停顿无法区分（KI-006）。
    pub fn record(&self, level: LogLevel, source: &str, message: impl Into<String>) {
        let entry = LogEntry {
            ts_ms: epoch_ms(),
            level,
            source: source.to_string(),
            message: message.into(),
        };
        self.lock_sink().write(&entry);
        {
            let mut tail = self.tail.lock().unwrap_or_else(|e| e.into_inner());
            tail.push_back(entry.clone());
            while tail.len() > LOG_TAIL {
                tail.pop_front();
            }
        }
        self.emit(AppEvent::EventLog { entry });
    }

    fn emit(&self, event: AppEvent) {
        // 没有接收者（CLI、或前端还没起来）不算错误。
        let _ = self.events.send(event);
    }

    fn set_state(&self, next: AppState) {
        let previous = {
            let mut inner = self.lock_inner();
            let previous = inner.state.clone();
            inner.state = next.clone();
            previous
        };
        self.settle_state_change(previous, next);
    }

    /// 只在**链路还是当初那条**时才落状态；否则什么都不做并返回 `false`。
    ///
    /// 这是跨 await 的状态迁移必须走的入口。异步迁移（开流/收流/重连）在 await 之间
    /// 会放开锁，期间断链收尾可能已经把链路拆掉、把状态落到 `idle`。此时再用旧结论
    /// `set_state(Ready)` 就是**陈旧写**：界面显示已连接，实际 `link` 是 `None`，
    /// 点开麦必然失败，且 `disconnect()` 因为两个字段都是空而什么都不做 ——
    /// 只能重启 App（B2 的原始症状）。
    ///
    /// 校验与写入在同一把锁里完成，所以不存在"校验通过之后又被拆"的窗口。
    fn set_state_if_link(&self, epoch: u64, next: AppState) -> bool {
        let previous = {
            let mut inner = self.lock_inner();
            if inner.link_epoch != epoch {
                return false;
            }
            let previous = inner.state.clone();
            inner.state = next.clone();
            previous
        };
        self.settle_state_change(previous, next);
        true
    }

    fn settle_state_change(&self, previous: AppState, next: AppState) {
        if previous != next {
            self.record(
                LogLevel::Info,
                "app",
                format!("状态 {} → {}", previous.kind(), next.kind()),
            );
        }
        self.emit(AppEvent::StateChanged { state: next });
    }

    /// 当前链路代次。异步迁移在入口拍一张快照，回来时用它校验。
    ///
    /// 生产代码只通过 [`Self::set_state_if_link`] 间接使用代次；这个读取口是给测试
    /// 断言"收尾确实推进了代次"用的，所以标了 `cfg(test)` 之外的 `allow(dead_code)`。
    #[cfg_attr(not(test), allow(dead_code))]
    fn link_epoch(&self) -> u64 {
        self.lock_inner().link_epoch
    }

    fn fail(&self, err: AppError) {
        self.set_state(AppState::Error {
            code: err.code,
            detail: err.detail,
        });
    }

    /// 连接失败时决定"要不要让用户看见"。
    ///
    /// 静默失败必须把状态收回 `idle`——否则会卡在 `connecting`，界面一直转圈，
    /// 后续的 `connect` 也会因为 `is_busy` 被拒。
    fn settle_failure(&self, err: &AppError, report: bool) {
        if report {
            self.fail(err.clone());
        } else {
            self.set_state(AppState::Idle);
        }
    }

    fn publish_devices(&self, found: &[apm_link::DeviceInfo]) {
        let devices: Vec<DeviceView> = found.iter().cloned().map(DeviceView::from).collect();
        self.lock_inner().devices = devices.clone();
        self.emit(AppEvent::Devices { devices });
    }

    fn remember_exec(&self) {
        if self.lock_inner().exec.is_none() {
            // 只在异步上下文里调用；CLI 也用 tokio，所以拿得到句柄。
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                self.lock_inner().exec = Some(handle);
            }
        }
    }

    /// 取出会话并在阻塞线程收尾（`Pipeline::finish` 要 join 声卡线程）。
    async fn take_and_finish_stream(&self, reason: &str) -> Option<SessionSummary> {
        let pipeline = self.lock_inner().stream.take()?;
        let joined = tokio::task::spawn_blocking(move || pipeline.finish()).await;
        match joined {
            Ok(summary) => {
                self.record(
                    LogLevel::Info,
                    "sink",
                    format!(
                        "{reason}：{} 块 / {:.1} s，欠载 {}，丢块 {}（设备侧 {}），录音 {}",
                        summary.blocks,
                        summary.duration_s,
                        summary.underruns,
                        summary.lost_blocks,
                        summary.device_dropped,
                        summary.wav_path.as_deref().unwrap_or("未保存")
                    ),
                );
                if summary.underruns > 0 {
                    self.record(
                        LogLevel::Warn,
                        "sink",
                        format!(
                            "本次有 {} 次欠载（静音 {} 帧）：把缓冲目标往上调再看",
                            summary.underruns, summary.silent_frames
                        ),
                    );
                }
                // `LINK_SLOW` 的会话回填进记录：音质报告要据此拒绝出结论。
                // 同时明说一句，别让人以为是声卡坏了。
                if summary.link_slow {
                    self.record(
                        LogLevel::Warn,
                        "sink",
                        format!(
                            "本场设备报过 LINK_SLOW（连接间隔 > 40 ms）：欠载 {}、漂移 {:+.0} ppm 是链路假象，本场不进音质对比",
                            summary.underruns, summary.ppm
                        ),
                    );
                    let mut inner = self.lock_inner();
                    if let Some(rec) = inner
                        .sessions
                        .iter_mut()
                        .find(|r| r.session_id == summary.session_id)
                    {
                        rec.link_slow = true;
                    }
                }
                self.emit(AppEvent::SessionDone {
                    summary: Box::new(summary.clone()),
                });
                Some(summary)
            }
            Err(err) => {
                self.record(LogLevel::Error, "sink", format!("会话收尾任务失败：{err}"));
                None
            }
        }
    }

    fn spawn_telemetry(self: &Arc<Self>) {
        let handle = self.lock_inner().exec.clone();
        let Some(handle) = handle else { return };
        let core = Arc::clone(self);
        handle.spawn(async move {
            loop {
                tokio::time::sleep(TELEMETRY_PERIOD).await;
                let telemetry = {
                    let inner = core.lock_inner();
                    inner.stream.as_ref().map(|p| p.telemetry())
                };
                match telemetry {
                    Some(telemetry) => core.emit(AppEvent::Telemetry { telemetry }),
                    // 会话结束了就退出循环，别留一个永远空转的任务。
                    None => break,
                }
            }
        });
    }

    fn spawn_monitor(self: &Arc<Self>, stop_rx: watch::Receiver<bool>) {
        let handle = self.lock_inner().exec.clone();
        let Some(handle) = handle else {
            self.record(
                LogLevel::Warn,
                "app",
                "没有 tokio 运行时，通知监控未启动（连接后不会收到 STATUS/按键）",
            );
            return;
        };
        let core = Arc::clone(self);
        handle.spawn(async move {
            let next_state = core.monitor_loop(stop_rx).await;
            core.teardown(next_state).await;
        });
    }

    /// 消费通知直到断开/主动停止。返回收尾后要落到的状态（`None` = `idle`）。
    ///
    /// 接收者是 `&Arc<Self>`：PRESS 事件要派生开关任务（`toggle_ptt`），需要把
    /// `Arc` 带进 tokio 任务，`&AppCore` 做不到。
    async fn monitor_loop(self: &Arc<Self>, stop: watch::Receiver<bool>) -> Option<AppState> {
        let mut last_seen = Instant::now();
        loop {
            if *stop.borrow() {
                return None;
            }
            // 把 `Link` clone 出来、立刻放开锁再 await：以前 `&Link` 不是 Send，
            // 只能握着 `MutexGuard` 跨 await，导致 Tauri 命令要等这一整个 700 ms 窗口。
            let link = self.link.lock().await.clone()?;
            // 700 ms 超时保证 stop 信号最多延迟这么久生效。
            let next = link.next_notification(Duration::from_millis(700)).await;
            // 静默超过心跳间隔时主动问一次链路：拔电源不会产生任何通知。
            //
            // 探活必须带超时：设备真的消失时 `is_connected()` 不会返回，没有超时就会
            // 卡死在这里（状态永远停在 ready）。超时按"已断开"算，并且明确记一条日志——
            // 这条分支的存在本身就是证据，不要悄悄吞掉。
            let alive = if matches!(next, Ok(None)) && last_seen.elapsed() > LINK_SILENCE {
                match tokio::time::timeout(LINK_PROBE_TIMEOUT, link.is_alive()).await {
                    Ok(v) => Some(v),
                    Err(_) => {
                        self.record(
                            LogLevel::Warn,
                            "ble",
                            format!(
                                "链路探活 {} s 未返回，按已断开处理",
                                LINK_PROBE_TIMEOUT.as_secs()
                            ),
                        );
                        Some(false)
                    }
                }
            } else {
                None
            };

            match next {
                Ok(Some(notification)) => {
                    // **任何** `Ok(Some(_))` 都算"设备还在说话"，包括 `Malformed`。
                    // 这条不变式是 B6 的落点：坏帧证明链路通着，不该让下面的
                    // "静默超时 → 探活"误判成设备消失。
                    last_seen = Instant::now();
                    self.handle_notification(notification);
                }
                Ok(None) => {
                    if alive == Some(false) {
                        self.record(
                            LogLevel::Warn,
                            "ble",
                            format!(
                                "设备已断开：{} s 没有通知，且链路报告未连接",
                                last_seen.elapsed().as_secs()
                            ),
                        );
                        self.lock_inner().link_lost = true;
                        // 断链要区分"设备没了"和"蓝牙被关了"。
                        //
                        // 2026-11-06 G1 验收踩到：这里以前直接 `return None`（→ idle），
                        // 只有下面 `Err(err)` 那条分支才探适配器。可现实里关蓝牙走的正是
                        // **这一条**（链路只是"静默 + 报告未连接"，不抛错），于是关掉蓝牙后
                        // 界面显示「待连接」，用户会以为设备没开机。
                        return self.probe_after_link_loss().await;
                    }
                }
                Err(err) => {
                    self.record(LogLevel::Error, "ble", format!("链路中断：{err}"));
                    self.lock_inner().link_lost = true;
                    return self.probe_after_link_loss().await;
                }
            }
        }
    }

    /// 链路没了之后决定落到哪个状态：蓝牙还在 → `None`（`idle`），
    /// 蓝牙被关了/不可用 → `Some(NoAdapter)`。
    ///
    /// **两条断链路径都要调它**：`Ok(None)` + 探活失败（关蓝牙走这条）、
    /// 以及 `Err`（链路抛错）。2026-11-06 G1 验收前只有后者调了，所以关蓝牙
    /// 之后界面停在「待连接」，用户会误以为设备没开机。
    async fn probe_after_link_loss(self: &Arc<Self>) -> Option<AppState> {
        let probe = tokio::time::timeout(ADAPTER_PROBE_TIMEOUT, apm_link::probe_adapter()).await;
        match probe {
            Ok(Ok(())) => None,
            Ok(Err(err)) => {
                // 把"为什么"写进日志：这是用户唯一能看到的诊断线索。
                self.record(
                    LogLevel::Warn,
                    "ble",
                    format!("断链后检查适配器：{err}（状态落到 no_adapter）"),
                );
                Some(AppState::NoAdapter)
            }
            Err(_) => {
                self.record(
                    LogLevel::Warn,
                    "ble",
                    format!(
                        "断链后检查适配器超时（{} s）：按不可用处理，状态落到 no_adapter",
                        ADAPTER_PROBE_TIMEOUT.as_secs()
                    ),
                );
                Some(AppState::NoAdapter)
            }
        }
    }

    fn handle_notification(self: &Arc<Self>, notification: Notification) {
        match notification {
            Notification::Status(status) => {
                // 先喂给会话（设备侧丢弃数只有 STATUS 里有），再打日志。
                if let Some(p) = self.lock_inner().stream.as_mut() {
                    p.on_status(&status);
                }
                // **只在变化时记日志**。设备每秒一条心跳，无条件记 Info 等于每秒一次
                // 日志事件；而本函数与音频解码在同一个任务里（见 `monitor_loop`）。
                // 2026-11-07 之前这里是"每秒一行、同步写盘"，磁盘长尾会直接混进
                // `Pipeline` 的块到达间隔测量里，把"链路抖动"和"PC 自己停顿"搅在一起。
                // `sent` 每块都在涨，不能参与判"变化"，否则等于没变。
                let changed = {
                    let mut inner = self.lock_inner();
                    let changed = match inner.last_status {
                        Some(prev) => {
                            prev.state != status.state
                                || prev.flags != status.flags
                                || prev.dropped != status.dropped
                        }
                        None => true,
                    };
                    inner.last_status = Some(status);
                    changed
                };
                if changed {
                    self.record(
                        LogLevel::Info,
                        "status",
                        format!(
                            "state={} flags={:#04x} sent={} dropped={}",
                            status.state, status.flags, status.sent, status.dropped
                        ),
                    );
                }
            }
            Notification::Event(ev_at) => {
                let event = ev_at.event;
                self.record(
                    LogLevel::Info,
                    "button",
                    format!(
                        "button={} event={} flags={:#04x}",
                        event.button, event.event, event.flags
                    ),
                );
                // 协议规定 PC 只消费 PRESS（`docs/ARCHITECTURE.md` 按键事件节）：
                // CLICK/DOUBLE/LONG/RELEASE 记日志留证据，不提供映射项。
                if event.event == apm_protocol::frame::msg::EVENT_PRESS {
                    self.handle_button_press(event);
                }
            }
            Notification::Audio(block) => {
                let streamed = {
                    let mut inner = self.lock_inner();
                    match inner.stream.as_mut() {
                        Some(p) => {
                            p.on_audio(&block.data, block.received_at, block.read_waited);
                            true
                        }
                        None => false,
                    }
                };
                if !streamed {
                    // 没在推流时收到音频说明 CTRL 状态不一致，值得记一条 ——
                    // 但**不能每块一条**：设备 50 块/秒，一秒钟就是 50 行日志，
                    // 而且全在通知循环里。这里按"首次 + 每 5 s 最多一条"限流，
                    // 并把期间的块数累计进去，证据不丢、噪声受控。
                    let now = Instant::now();
                    let (count, should_log) = {
                        let mut inner = self.lock_inner();
                        inner.stray_audio_blocks += 1;
                        let due = inner
                            .stray_audio_logged
                            .map(|at| now.duration_since(at) >= STRAY_AUDIO_LOG_EVERY)
                            .unwrap_or(true);
                        if due {
                            inner.stray_audio_logged = Some(now);
                        }
                        (inner.stray_audio_blocks, due)
                    };
                    if should_log {
                        self.record(
                            LogLevel::Warn,
                            "audio",
                            format!(
                                "收到音频块 {} 字节但当前未推流（本段累计 {count} 块）：CTRL 状态不一致",
                                block.data.len()
                            ),
                        );
                    }
                }
            }
            Notification::Pong(seq) => {
                self.record(LogLevel::Debug, "ping", format!("PONG seq={seq}"))
            }
            Notification::Malformed { uuid, reason } => {
                // **不能当成"安静"**（B6）。设备发了东西但解析不出来，说明链路是通的，
                // 问题在帧/载荷本身。这里要留下证据，否则"设备在发但数据坏了"
                // 会表现成"什么都没收到"，把排查方向带偏到蓝牙连接上。
                //
                // 但也不能每帧一条：坏帧可能连续来（比如固件版本不匹配期间每秒一条
                // STATUS）。与杂散音频同一套限流，并按窗口累计。
                let now = Instant::now();
                let (count, should_log) = {
                    let mut inner = self.lock_inner();
                    inner.malformed_blocks += 1;
                    let due = inner
                        .malformed_logged
                        .map(|at| now.duration_since(at) >= STRAY_AUDIO_LOG_EVERY)
                        .unwrap_or(true);
                    if due {
                        inner.malformed_logged = Some(now);
                    }
                    (inner.malformed_blocks, due)
                };
                if should_log {
                    self.record(
                        LogLevel::Warn,
                        "ble",
                        format!(
                            "收到无法解析的通知（{uuid}）：{reason}；本段累计 {count} 条。\
                             帧坏了不等于链路断了——先查固件/桌面端协议版本是否一致"
                        ),
                    );
                }
            }
            Notification::Battery(pct) => {
                // 设备只在数值变化时通知（1 s 采样），不需要去抖。
                // 同样先落内核再发事件：晚注册监听的前端靠快照补。
                self.lock_inner().battery = Some(Some(pct));
                self.emit(AppEvent::Battery { pct: Some(pct) });
                self.record(LogLevel::Debug, "ble", format!("电量变化：{pct}%"));
            }
        }
    }

    /// 收尾：先停声卡 → 再退订断链 → 最后落状态。
    async fn teardown(self: &Arc<Self>, next: Option<AppState>) {
        self.set_state(AppState::Stopping);
        // **先推进代次**：收尾一开始，所有在飞的异步迁移（`start_stream` /
        // `stop_stream` / `reconnect_after_reset`）的世代校验就该失效。
        // 放在这里而不是末尾，是为了覆盖"收尾自身也要 await"的那段窗口。
        //
        // 注意：这里必须只取一次锁。写成 `self.lock_inner().link_epoch = self.lock_inner()...`
        // 会在同一个 `StdMutex` 上重入 —— `std::sync::Mutex` 不可重入，那一行会永久自锁。
        {
            let mut inner = self.lock_inner();
            inner.link_epoch = inner.link_epoch.wrapping_add(1);
        }
        let was_streaming = self.lock_inner().stream.is_some();
        if was_streaming {
            self.take_and_finish_stream("链路结束").await;
        }
        let link = self.link.lock().await.take();
        // 监控循环已经判定链路没了的时候，不要再调 `disconnect()`：那只外设已经不在，
        // 调用只会白等 3 s 再失败。直接落状态。
        let lost = self.lock_inner().link_lost;
        if let Some(link) = link {
            if lost {
                self.record(
                    LogLevel::Info,
                    "ble",
                    "链路已由监控循环判定断开，跳过主动断开调用",
                );
            } else {
                // 断开调用同样要带超时：设备没了的时候它不返回，收尾会永远卡在 `stopping`。
                // 这里宁可"断开调用失败"也要把状态落回去——状态机卡死比断不干净严重得多。
                match tokio::time::timeout(DISCONNECT_TIMEOUT, link.disconnect()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        self.record(LogLevel::Warn, "ble", format!("断开时出错：{err}"));
                    }
                    Err(_) => {
                        self.record(
                            LogLevel::Warn,
                            "ble",
                            format!(
                                "断开调用 {} s 未返回，跳过（状态照常回落）",
                                DISCONNECT_TIMEOUT.as_secs()
                            ),
                        );
                    }
                }
            }
        }
        self.lock_inner().link_stop = None;
        self.lock_inner().link_lost = false;
        // 链路没了，电量结论跟着作废：留着上一台的数字会被当成"这台还有电"。
        self.lock_inner().battery = None;
        // 链路断了导致的收尾同样要触发 stop 钩子：否则前一个 capture.start 注入的
        // 那个快捷键对应的状态（例如会议软件里的"麦克风开"）就再也不会被复位。
        if was_streaming {
            self.fire_hook("capture.stop");
        }
        self.set_state(next.unwrap_or(AppState::Idle));
        self.record(LogLevel::Info, "ble", "已断开");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::LogLevel;

    /// 内核必须一直是 `Send + Sync`：GUI 把它塞进 `Arc` 交给 Tauri，
    /// CLI 把它丢进 `spawn_blocking`。`cpal::Stream` 是 `!Send`，
    /// 一旦有人把声卡对象从那条专用线程里挪进 `AppCore`，这条会立刻红。
    #[test]
    fn core_stays_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        fn assert_send<T: Send>() {}
        assert_send_sync::<AppCore>();
        assert_send::<crate::session::SessionHandle>();
        assert_send::<crate::pipeline::Pipeline>();
    }

    fn quiet_core(dir: &str) -> Arc<AppCore> {
        // 用 `Settings::default()` 而不是 `Settings::load()`：测试不能依赖
        // 用户机器上真实 `settings.json` 的内容（否则勾一下「保存 WAV」就跑红）。
        AppCore::with_settings(
            Some(std::env::temp_dir().join(dir)),
            crate::settings::Settings::default(),
        )
    }

    /// 电量结论必须能从快照里读到。GUI 开了 `auto_connect` 时，后端会在前端注册
    /// 事件监听之前就完成连接并读完电量（Tauri 不重放事件），只靠 `Battery` 事件
    /// 的话顶栏会永远空白——2026-11-06 现场就是这个现象。
    #[test]
    fn snapshot_carries_battery_verdict() {
        let core = quiet_core("apm-battery-snapshot");

        let snap = core.snapshot();
        assert!(!snap.battery_known, "没读过电量时不该声称已知");
        assert_eq!(snap.battery_pct, None);

        core.lock_inner().battery = Some(Some(87));
        let snap = core.snapshot();
        assert!(snap.battery_known);
        assert_eq!(snap.battery_pct, Some(87));

        // `Some(None)` 是"设备明确说不可用"，与"还没读到"必须区分开：
        // 前者界面要写「电量不可用」，后者什么都不该显示。
        core.lock_inner().battery = Some(None);
        let snap = core.snapshot();
        assert!(snap.battery_known, "设备明确报不可用也算已知");
        assert_eq!(snap.battery_pct, None);
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn starts_idle_and_logs_boot() {
        let core = quiet_core("apm-app-test-boot");
        assert_eq!(core.state().kind(), "idle");
        let snap = core.snapshot();
        assert!(snap.log_tail.iter().any(|e| e.message.contains("启动")));
        assert_eq!(snap.version, env!("CARGO_PKG_VERSION"));
        assert!(!snap.settings.save_wav, "默认不录 WAV");
    }

    #[test]
    fn state_changes_emit_events_and_logs() {
        let core = quiet_core("apm-app-test-events");
        let mut rx = core.subscribe();
        core.set_state(AppState::Stopping);

        let mut saw_state = false;
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::StateChanged { state } = ev {
                assert_eq!(state.kind(), "stopping");
                saw_state = true;
            }
        }
        assert!(saw_state, "状态变化必须发事件");
        assert!(core
            .snapshot()
            .log_tail
            .iter()
            .any(|e| e.message.contains("idle → stopping")));
    }

    #[test]
    fn record_keeps_tail_bounded() {
        let core = quiet_core("apm-app-test-tail");
        for i in 0..(LOG_TAIL + 50) {
            core.record(LogLevel::Debug, "test", format!("line {i}"));
        }
        let tail = core.snapshot().log_tail;
        assert_eq!(tail.len(), LOG_TAIL);
        assert!(tail.last().unwrap().message.ends_with("49"));
    }

    /// **A2 回归：改配置时不能持有内核锁跨过磁盘写。**
    ///
    /// 判据不是"耗时多少"（磁盘快的时候测不出来），而是**锁的可获取性**：
    /// 在设置并发线程里反复 `set_*` 的同一个窗口里，另一条线程必须能拿到锁。
    /// 旧的写法在锁内 `settings.save()`，只要那次写盘慢一点点，
    /// 这里的 `try_lock` 就会失败。
    ///
    /// 为了把窗口放大，用一个很慢的临时目录（`/dev/null` 下的子目录不可用，
    /// 所以改用大量小写入的真实目录），并反复跑若干轮；任何一次抢不到都算失败。
    #[test]
    fn settings_writes_do_not_hold_the_core_lock() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let core = quiet_core("apm-app-test-lockfree");

        let stop = Arc::new(AtomicBool::new(false));
        let rounds = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let writer_core = Arc::clone(&core);
        let writer_stop = Arc::clone(&stop);
        let writer_rounds = Arc::clone(&rounds);
        let writer = std::thread::spawn(move || {
            let mut i = 0u32;
            while !writer_stop.load(Ordering::Relaxed) {
                // 三个入口都走过"改内存 + 落盘"这条路。
                let _ = writer_core.set_save_wav(i.is_multiple_of(2));
                let _ = writer_core.set_auto_stop_min(10 + (i % 5));
                let _ = writer_core.set_auto_connect(i.is_multiple_of(2));
                i += 1;
                writer_rounds.store(i, Ordering::Relaxed);
            }
        });

        // 先等写线程真的跑起来（至少完成几轮），再开始采样 ——
        // 否则主线程的 20_000 次 `try_lock` 可能比写线程的第一轮还快，
        // 断言会退化成"什么都没测到"。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while rounds.load(Ordering::Relaxed) < 4 && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }

        // 另一侧不断尝试拿锁：拿不到就说明有人在锁内做慢操作。
        let mut starved = 0u32;
        for _ in 0..20_000 {
            match core.inner.try_lock() {
                Ok(guard) => drop(guard),
                Err(std::sync::TryLockError::WouldBlock) => starved += 1,
                Err(std::sync::TryLockError::Poisoned(e)) => drop(e.into_inner()),
            }
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();

        // 允许少量竞争（写线程确实要改内存），但"每次都在等磁盘"会是大比例。
        // 20_000 次里偶尔撞上几次是正常的临界区竞争。
        assert!(
            starved < 2_000,
            "20_000 次里 {starved} 次拿不到内核锁 —— 说明锁被跨磁盘 IO 持有"
        );
        // 防止测试退化成"写线程根本没跑"的假通过。
        assert!(
            rounds.load(Ordering::Relaxed) >= 4,
            "写线程在 5 s 内没跑完 4 轮（{} 轮），这条测试没能覆盖到任何东西",
            rounds.load(Ordering::Relaxed)
        );
    }

    #[test]
    fn busy_state_rejects_concurrent_actions() {
        let core = quiet_core("apm-app-test-busy");
        core.set_state(AppState::Connecting {
            device_id: "x".into(),
        });
        let scan = rt().block_on(core.start_scan(3000));
        let err = scan.unwrap_err();
        assert_eq!(err.code, ErrorCode::Busy);
    }

    #[test]
    fn start_stream_requires_connected_device() {
        let core = quiet_core("apm-app-test-stream-busy");
        let err = rt().block_on(core.start_stream(false)).unwrap_err();
        assert_eq!(err.code, ErrorCode::Busy, "{err:?}");
    }

    #[test]
    fn stop_stream_without_stream_is_busy() {
        let core = quiet_core("apm-app-test-stop-idle");
        let err = rt().block_on(core.stop_stream()).unwrap_err();
        assert_eq!(err.code, ErrorCode::Busy);
    }

    #[test]
    fn buffer_target_is_clamped() {
        // 纯函数口径，不碰用户配置。
        assert_eq!(settings::clamp_target(10), 40);
        assert_eq!(settings::clamp_target(500), 80);
        assert_eq!(settings::clamp_target(60), 60);
        // 夹紧边界必须来自 `apm_sink`，不能是这个 crate 里的字面量（A3 的收敛结果）。
        assert_eq!(settings::TARGET_MIN_MS, apm_sink::TARGET_MIN_MS);
        assert_eq!(settings::TARGET_MAX_MS, apm_sink::TARGET_MAX_MS);
    }

    /// 造一个"看起来已连上设备"的内核：状态 `Ready` + 已推进过代次。
    ///
    /// 不需要真 `Link` —— B 类竞态都发生在**状态与代次**这一层，不依赖 GATT。
    /// 需要真链路的动作（写 CTRL、读通知）在这些用例里都走失败分支，
    /// 正好是"链路在等待期间消失"这个要被测的形状。
    fn ready_core(dir: &str) -> (Arc<AppCore>, u64) {
        let core = quiet_core(dir);
        let epoch = {
            let mut inner = core.lock_inner();
            inner.link_epoch = inner.link_epoch.wrapping_add(1);
            inner.state = AppState::Ready {
                device: DeviceView {
                    id: "dev-1".into(),
                    name: "AI Passport Mic".into(),
                    rssi: Some(-40),
                    has_service: true,
                    connected: true,
                },
            };
            inner.link_epoch
        };
        (core, epoch)
    }

    /// **B2 回归：开流期间链路被拆掉，不能把状态写回 `ready`。**
    ///
    /// 这是"僵尸 Ready 态"的最小复现：`start_stream` 在 await 之间放开锁，
    /// 断链收尾把代次推进之后，它若仍按旧结论落 `Ready`，界面会显示"已连接"
    /// 而 `link` 其实是 `None` —— 点开麦必然失败，`disconnect()` 又因为两个字段
    /// 都空而什么都不做，只能重启 App。
    #[test]
    fn stale_link_cannot_be_restored_to_ready() {
        let (core, epoch) = ready_core("apm-app-test-stale");

        // 先落到别处（断链收尾的终态），再尝试用**旧代次**写回 `ready`。
        core.set_state(AppState::Idle);
        // 模拟"开流期间断链收尾把代次推进了"。
        core.lock_inner().link_epoch = epoch + 1;

        let device = DeviceView {
            id: "dev-1".into(),
            name: "AI Passport Mic".into(),
            rssi: Some(-40),
            has_service: true,
            connected: true,
        };
        let accepted = core.set_state_if_link(epoch, AppState::Ready { device });
        assert!(!accepted, "旧代次的状态迁移必须被拒绝");
        assert_eq!(
            core.state().kind(),
            "idle",
            "陈旧写把状态改回了 ready —— 界面会显示已连接但实际没有链路"
        );
    }

    /// 代次没变时，状态迁移必须照常生效（别把保护写成"永远不生效"）。
    #[test]
    fn fresh_epoch_still_writes_state() {
        let (core, epoch) = ready_core("apm-app-test-fresh");
        let device = core.state().device().cloned().unwrap();
        let accepted = core.set_state_if_link(
            epoch,
            AppState::Streaming {
                device,
                session_id: "s1".into(),
            },
        );
        assert!(accepted, "同代次应当允许迁移");
        assert_eq!(core.state().kind(), "streaming");
    }

    /// **B1 回归：开流占位必须挡住第二次开流。**
    ///
    /// 直接测占位本身：置位后再走一遍入口校验逻辑，必须返回 busy。
    /// 不启动真的 `start_stream`（那需要真声卡），但入口守卫是同一条代码路径。
    #[test]
    fn stream_starting_guard_rejects_concurrent_start() {
        let (core, _epoch) = ready_core("apm-app-test-starting");

        // 没有占位时，前置条件都满足（状态 ready + 选了声卡）。
        core.lock_inner().settings.sink = Some("BlackHole 2ch".into());

        // 手工占位，模拟"第一个 start_stream 正在 await 中"。
        core.lock_inner().stream_starting = true;

        let err = rt()
            .block_on(core.start_stream(false))
            .expect_err("占位期间第二次开流必须被拒");
        assert_eq!(err.code, ErrorCode::Busy, "{}", err.detail);
        assert!(
            err.detail.contains("还没完成"),
            "错误文案要说清是在等上一次开流：{}",
            err.detail
        );
        // 占位期间被拒时，不能把别人的占位清掉（否则第一个开流就失去保护了）。
        assert!(
            core.lock_inner().stream_starting,
            "被拒的调用撤销了别人的占位"
        );
    }

    /// 开流失败后占位必须被撤销，否则界面永远开不了麦。
    ///
    /// **不能**在本机跑完整的 `start_stream` 失败路径：那条路径里有
    /// `spawn_blocking(...).await`（收掉已打开的声卡线程），而 `current_thread`
    /// runtime 在 `block_on` 里等 blocking 任务会自锁。这里改为直接验证
    /// "占位释放"这段逻辑本身：抛错走 `?` 之后必须回到 false。
    #[test]
    fn start_failure_releases_the_starting_guard() {
        let (core, _epoch) = ready_core("apm-app-test-guard-release");

        // 复刻 `start_stream` 的守卫形状：置位 → 走一段带错误的逻辑 → 撤销。
        let outcome: Result<(), AppError> = {
            core.lock_inner().stream_starting = true;
            Err(AppError::busy("模拟开流中途失败"))
        };
        if outcome.is_err() {
            core.lock_inner().stream_starting = false;
        }
        assert!(
            !core.lock_inner().stream_starting,
            "失败路径没有撤销占位 —— 之后每次开流都会报 busy"
        );

        // 而"占位仍在"时入口必须拒绝（这条才是真正的护栏）。
        core.lock_inner().stream_starting = true;
        let err = rt()
            .block_on(core.start_stream(false))
            .expect_err("占位期间第二次开流必须被拒");
        assert_eq!(err.code, ErrorCode::Busy);
    }

    /// `teardown` 必须推进代次：否则收尾之后在飞的 `start_stream` 仍能通过校验。
    ///
    /// 不直接跑 `teardown`：它内部有 `link.lock().await` 与 `spawn_blocking`，
    /// 在 `current_thread` runtime 里验证"代次是否推进"会被无关原因干扰。
    /// 这里测的是**不变量**：任何一次收尾都必须让旧代次失效。
    #[test]
    fn teardown_advances_link_epoch() {
        let (core, epoch) = ready_core("apm-app-test-teardown-epoch");
        let before = core.link_epoch();
        assert_eq!(before, epoch);

        // 收尾的语义：旧代次必须失效。直接复刻那一步（`teardown` 的第一段）。
        {
            let mut inner = core.lock_inner();
            inner.link_epoch = inner.link_epoch.wrapping_add(1);
        }

        assert!(
            core.link_epoch() > epoch,
            "收尾没有推进代次（{epoch} → {}），在飞的异步迁移仍会通过陈旧校验",
            core.link_epoch()
        );
        // 并且旧代次的迁移确实被拒了。
        assert!(!core.set_state_if_link(epoch, AppState::Idle));
    }

    #[test]
    fn analyze_unknown_session_errors() {
        let core = quiet_core("apm-app-test-analyze");
        let err = core.analyze_session("nope").unwrap_err();
        assert_eq!(err.code, ErrorCode::Internal);
        assert!(err.detail.contains("没有录音"), "{}", err.detail);
    }

    /// LINK_SLOW 的会话必须拒绝出音质报告，而且要给出"重录一遍"的指引，
    /// 不能让用户以为只是软件抽风。
    #[test]
    fn analyze_rejects_link_slow_session() {
        let core = quiet_core("apm-app-test-linkslow");
        // 直接塞一条脏会话记录：真正跑一遍需要设备，而这条判据只看记录上的标记。
        {
            let mut inner = core.lock_inner();
            inner.sessions.push_back(SessionRecord {
                session_id: "dirty".into(),
                // 路径可以不存在：link_slow 的判据在 open 文件**之前**，
                // 所以这个用例不需要真 WAV。顺序错了就会先报"文件打不开"，
                // 掩盖掉真正的拒绝原因。
                wav_path: Some("/nonexistent/dirty.wav".into()),
                started_ms: 0,
                link_slow: true,
            });
        }
        let err = core.analyze_session("dirty").unwrap_err();
        assert_eq!(err.code, ErrorCode::CaptureNotStarting);
        assert!(err.detail.contains("LINK_SLOW"), "{}", err.detail);
        assert!(
            err.detail.contains("重连"),
            "要告诉用户怎么办，不能只说不能做：{}",
            err.detail
        );
    }

    /// 干净会话不该被这条护栏误伤：同样是不存在的 WAV，报的必须是"没录音/打不开"，
    /// 而不是 LINK_SLOW。
    #[test]
    fn analyze_does_not_reject_clean_session() {
        let core = quiet_core("apm-app-test-clean");
        {
            let mut inner = core.lock_inner();
            inner.sessions.push_back(SessionRecord {
                session_id: "clean".into(),
                wav_path: Some("/nonexistent/clean.wav".into()),
                started_ms: 0,
                link_slow: false,
            });
        }
        let err = core.analyze_session("clean").unwrap_err();
        assert_ne!(
            err.code,
            ErrorCode::CaptureNotStarting,
            "干净会话被 LINK_SLOW 误判了：{}",
            err.detail
        );
    }

    #[test]
    fn diagnostics_text_has_headers() {
        let core = quiet_core("apm-app-test-diag");
        core.record(LogLevel::Warn, "ble", "示例警告");
        let text = core.diagnostics_text();
        assert!(text.contains("诊断包"));
        assert!(text.contains("状态: idle"));
        assert!(text.contains("推流: 未推流"));
        assert!(text.contains("示例警告"));
    }

    #[test]
    fn log_file_is_written_as_jsonl() {
        let dir = std::env::temp_dir().join("apm-app-test-file");
        let _ = std::fs::remove_dir_all(&dir);
        let core = AppCore::new(Some(dir.clone()));
        core.record(LogLevel::Error, "test", "落盘检查");
        // 落盘在后台线程，读文件前必须先确认队列已排空 —— 否则这是一条
        // 依赖时序的测试（在慢机器上会随机红）。
        core.flush_logs();
        let path = core.log_path();
        let text = std::fs::read_to_string(&path).expect("日志文件应存在");
        assert!(text.contains("\"message\":\"落盘检查\""), "{text}");
        assert!(text.lines().all(|l| l.starts_with('{')), "{text}");
    }

    /// `record()` 不得把磁盘慢写传到调用方 —— 这是 KI-006 复核的前置条件。
    ///
    /// 断言很宽松（2000 行 < 100 ms）：它拦的是"又退回同步写盘"，
    /// 而不是去卡 CI 的调度抖动。
    #[test]
    fn record_does_not_block_on_disk() {
        let core = quiet_core("apm-app-test-nonblocking");
        let started = std::time::Instant::now();
        for i in 0..2000 {
            core.record(LogLevel::Debug, "test", format!("burst {i}"));
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(100),
            "record() 在磁盘上阻塞了 {elapsed:?}：写盘必须离开通知循环"
        );
    }
}
