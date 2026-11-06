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
    atomic::{AtomicBool, Ordering},
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

struct Inner {
    state: AppState,
    devices: Vec<DeviceView>,
    /// 存在表示正在扫描；`send(true)` 即请求提前结束。
    scan_cancel: Option<watch::Sender<bool>>,
    /// 存在表示通知监控在跑；`send(true)` 即请求收尾断链。
    link_stop: Option<watch::Sender<bool>>,
    /// 正在推流的会话。**唯一**的音频消费入口在 `monitor_loop`。
    stream: Option<Pipeline>,
    /// 最近若干次会话（用于音质报告页）。
    sessions: VecDeque<SessionRecord>,
    settings: Settings,
    /// 第一次进异步上下文时记下运行时句柄，供派生监控任务用。
    exec: Option<tokio::runtime::Handle>,
    /// 监控循环判定"链路已经没了"。收尾时据此跳过 `disconnect()`——对一只已经消失的
    /// 外设调它只会白等一个超时，而且必然失败。
    link_lost: bool,
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
}

/// 声卡的界面视图。字段够用来判断"能不能选"，不暴露 cpal 类型。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SinkView {
    pub name: String,
    pub uid: String,
    pub channels: u16,
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
    /// PTT 开关防抖：DOUBLE 手势会连发两次 PRESS（固件协议如此），而开/关麦本身
    /// 有几百毫秒的状态切换窗口。这期间再来的 PRESS 直接忽略——既避免了"开了又关"
    /// 的双击副作用，也堵住了并发开关两条会话的竞态。
    ptt_switching: AtomicBool,
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
        let (events, _) = broadcast::channel(EVENT_CHANNEL);
        let core = Arc::new(Self {
            inner: StdMutex::new(Inner {
                // 初始为 Idle：真正的适配器状态由 `detect_adapter` 覆盖。
                // 先假设可用，避免界面在首帧闪一下"没有蓝牙"。
                state: AppState::Idle,
                devices: Vec::new(),
                scan_cancel: None,
                link_stop: None,
                stream: None,
                sessions: VecDeque::new(),
                settings: Settings::load(),
                exec: None,
                link_lost: false,
                battery: None,
            }),
            link: AsyncMutex::new(None),
            events,
            sink: StdMutex::new(LogSink::new(log_dir)),
            tail: StdMutex::new(VecDeque::new()),
            version: env!("CARGO_PKG_VERSION"),
            ptt_switching: AtomicBool::new(false),
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
        }
    }

    /// 启动自动连接开关。立即落盘：用户勾完就关 App 也得记住。
    pub fn set_auto_connect(&self, enabled: bool) -> Result<(), AppError> {
        let mut inner = self.lock_inner();
        inner.settings.auto_connect = enabled;
        if let Err(err) = inner.settings.save() {
            self.record(LogLevel::Warn, "ble", format!("保存配置失败：{err}"));
        }
        Ok(())
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
        self.lock_inner().battery = None;
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
        let device = {
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
            if let Err(err) = inner.settings.save() {
                self.record(LogLevel::Warn, "ble", format!("保存配置失败：{err}"));
            }
            view
        };

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
        // 新链路：把"已知丢失"的标记清掉，否则第一次收尾就会跳过一次正常的断开调用。
        self.lock_inner().link_lost = false;
        let (stop_tx, stop_rx) = watch::channel(false);
        self.lock_inner().link_stop = Some(stop_tx);
        self.set_state(AppState::Ready {
            device: device.clone(),
        });
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
            match (&inner.settings.auto_connect, inner.settings.last_device_id.clone()) {
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
        let list = apm_sink::list_sinks().map_err(|e| {
            AppError::new(ErrorCode::SinkMissing, format!("枚举输出设备失败：{e}"))
        })?;
        Ok(list
            .into_iter()
            .map(|s| SinkView {
                name: s.name,
                uid: s.uid,
                channels: s.channels,
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
        {
            let mut inner = self.lock_inner();
            inner.settings.sink = Some(name.clone());
            if let Err(err) = inner.settings.save() {
                self.record(LogLevel::Warn, "sink", format!("保存配置失败：{err}"));
            }
        }
        self.record(LogLevel::Info, "sink", format!("输出声卡改为 {name}"));
        Ok(())
    }

    /// 是否默认保存会话 WAV。默认关（`docs/GUI.md` §12 第 3 条）。
    pub fn set_save_wav(&self, save: bool) -> Result<(), AppError> {
        let mut inner = self.lock_inner();
        inner.settings.save_wav = save;
        if let Err(err) = inner.settings.save() {
            self.record(LogLevel::Warn, "app", format!("保存配置失败：{err}"));
        }
        Ok(())
    }

    /// 缓冲目标（40–80 ms）。运行中改了取下一批回调生效，不需要重启推流。
    pub fn set_buffer_target(&self, ms: u32) -> Result<(), AppError> {
        let ms = settings::clamp_target(ms);
        let stream = {
            let mut inner = self.lock_inner();
            inner.settings.target_ms = ms;
            if let Err(err) = inner.settings.save() {
                self.record(LogLevel::Warn, "sink", format!("保存配置失败：{err}"));
            }
            inner.stream.as_ref().map(|_| ())
        };
        if stream.is_some() {
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
    pub async fn start_stream(self: &Arc<Self>, save_wav: bool) -> Result<String, AppError> {
        let device = match self.state() {
            AppState::Ready { device } => device,
            other => {
                return Err(AppError::busy(format!(
                    "当前状态 {} 不能开始推流（需要先连上设备）",
                    other.kind()
                )))
            }
        };
        if self.lock_inner().stream.is_some() {
            return Err(AppError::busy("已经在推流"));
        }

        let (sink_name, target_ms) = {
            let inner = self.lock_inner();
            (inner.settings.sink.clone(), inner.settings.target_ms)
        };
        let sink_name = sink_name.ok_or_else(|| {
            AppError::new(
                ErrorCode::SinkMissing,
                "还没有选择输出声卡：先在下拉框里选一个（macOS 通常装 BlackHole 2ch）",
            )
        })?;

        let started_ms = epoch_ms();
        let session_id = format!("s{started_ms}");
        let wav_path = save_wav.then(|| settings::sessions_dir().join(format!("{session_id}.wav")));

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

        self.lock_inner().stream = Some(pipeline);

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
            self.set_state(AppState::Ready { device });
            return Err(err);
        }

        {
            let mut inner = self.lock_inner();
            inner.sessions.push_back(SessionRecord {
                session_id: session_id.clone(),
                wav_path: wav_path.as_ref().map(|p| p.display().to_string()),
                started_ms,
            });
            while inner.sessions.len() > SESSIONS_KEPT {
                inner.sessions.pop_front();
            }
        }

        self.set_state(AppState::Streaming {
            device,
            session_id: session_id.clone(),
        });
        // 采集真的起来了才触发钩子：失败路径已经 return，不会误报"已开麦"。
        self.fire_hook("capture.start");
        self.remember_exec();
        self.spawn_telemetry();
        Ok(session_id)
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
    fn inject_in_background(self: &Arc<Self>, what: String, binding: apm_protocol::keymap::Binding) {
        let core = Arc::clone(self);
        let label = describe_binding(&binding);
        let spawned = std::thread::Builder::new()
            .name("apm-inject".into())
            .spawn(move || match crate::inject::tap(&binding) {
                Ok(()) => core.record(LogLevel::Info, "inject", format!("{what} 已注入 {label}")),
                Err(err) => core.record(
                    LogLevel::Warn,
                    "inject",
                    format!("{what} 注入失败：{err}"),
                ),
            });
        if let Err(err) = spawned {
            self.record(LogLevel::Warn, "inject", format!("起注入线程失败：{err}"));
        }
    }

    /// 设备按键 PRESS → 动作（M3 直连）。
    ///
    /// 动作按映射表走（每次现读，与 `fire_hook` 同一规则）：
    /// - `ok`（保留动作 `ptt-toggle`）：没在推流就开麦，在推流就停麦——**开关语义**；
    /// - `up` / `down`：配了普通键就点按注入，未绑定 = 不动作。
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
            return; // up / down 未绑定 = 不动作（ok 恒有默认 ptt-toggle）
        };
        if binding.is_ptt() {
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    let core = Arc::clone(self);
                    handle.spawn(async move { core.toggle_ptt().await });
                }
                Err(_) => {
                    self.record(LogLevel::Warn, "button", "没有 tokio 运行时，PTT 开关无法执行")
                }
            }
        } else {
            self.inject_in_background(format!("按键 {name}"), binding);
        }
    }

    /// PTT 开关的唯一执行点：设备 OK 键走这里，语义与界面按钮完全一致
    /// （同一条 `start_stream` / `stop_stream`，钩子照常触发）。
    ///
    /// 防抖：切换进行中（`ptt_switching`）再来的 PRESS 直接忽略。开麦有 120 ms
    /// 的 STOP→START 静默窗口，关麦要收声卡线程——DOUBLE 的两次 PRESS 相隔约
    /// 180 ms，落在窗口内就被当作一次动作，这是对"开了又关"副作用的修复而不是新行为。
    async fn toggle_ptt(self: &Arc<Self>) {
        if self
            .ptt_switching
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            self.record(
                LogLevel::Debug,
                "button",
                "PTT 切换进行中，忽略本次按键（防抖）",
            );
            return;
        }
        let streaming = self.lock_inner().stream.is_some();
        let action = if streaming { "关麦" } else { "开麦" };
        let result = if streaming {
            self.stop_stream().await.map(|_| ())
        } else {
            let save_wav = self.lock_inner().settings.save_wav;
            self.start_stream(save_wav).await.map(|_| ())
        };
        self.ptt_switching.store(false, Ordering::SeqCst);
        match result {
            Ok(()) => self.record(LogLevel::Info, "button", format!("PTT {action}完成")),
            Err(err) => {
                if err.code == ErrorCode::Busy {
                    // busy 是状态机在挡（比如界面正在收尾），记 debug 留证据即可。
                    self.record(LogLevel::Debug, "button", format!("PTT 暂不能切换：{}", err.detail));
                } else {
                    self.record(LogLevel::Warn, "button", format!("PTT 切换失败：{}", err.detail));
                }
            }
        }
    }

    /// 停止推流。收尾顺序：**先停写声卡 → 再 CTRL STOP**（断链由 disconnect 负责）。
    pub async fn stop_stream(self: &Arc<Self>) -> Result<SessionSummary, AppError> {
        if self.lock_inner().stream.is_none() {
            return Err(AppError::busy("当前没有在推流"));
        }
        let device = self.state().device().cloned();
        self.set_state(AppState::Stopping);

        let summary = match self.take_and_finish_stream("手动停止").await {
            Some(summary) => summary,
            None => {
                self.set_state(match device {
                    Some(device) => AppState::Ready { device },
                    None => AppState::Idle,
                });
                return Err(AppError::internal("会话收尾失败，详见诊断日志"));
            }
        };

        let link = self.link.lock().await.clone();
        if let Some(link) = link {
            if let Err(err) = link.stop_capture().await {
                self.record(LogLevel::Warn, "sink", format!("发送 CTRL STOP 失败：{err}"));
            }
        }

        match device {
            Some(device) => self.set_state(AppState::Ready { device }),
            None => self.set_state(AppState::Idle),
        }
        self.fire_hook("capture.stop");
        Ok(summary)
    }

    /// 对某次会话的 WAV 出音质报告。没有录音时明确报错，不给空报告。
    pub fn analyze_session(&self, session_id: &str) -> Result<QualityReport, AppError> {
        let path = {
            let inner = self.lock_inner();
            inner
                .sessions
                .iter()
                .find(|s| s.session_id == session_id)
                .and_then(|s| s.wav_path.clone())
        };
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
                "推流: 块={} 丢块={} 跳变={} 设备丢弃={} 缓冲={:.0} ms 欠载={} 静音帧={} 锁定失败={} 漂移={:+.0} ppm 毛刺={}\n",
                t.blocks,
                t.lost_blocks,
                t.seq_jumps,
                t.device_dropped,
                t.buffer_ms,
                t.underruns,
                t.silent_frames,
                t.lock_misses,
                t.ppm,
                t.glitch_events
            ));
        } else {
            out.push_str("推流: 未推流\n");
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

    fn tail_snapshot(&self) -> Vec<LogEntry> {
        let tail = self.tail.lock().unwrap_or_else(|e| e.into_inner());
        tail.iter().cloned().collect()
    }

    /// 记录一条日志：进内存环形缓冲、写文件、发事件。三处都做，缺一处就会
    /// 出现"界面上看不到、事后也查不到"。
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
        if previous != next {
            self.record(
                LogLevel::Info,
                "app",
                format!("状态 {} → {}", previous.kind(), next.kind()),
            );
        }
        self.emit(AppEvent::StateChanged { state: next });
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
                self.emit(AppEvent::SessionDone {
                    summary: summary.clone(),
                });
                Some(summary)
            }
            Err(err) => {
                self.record(
                    LogLevel::Error,
                    "sink",
                    format!("会话收尾任务失败：{err}"),
                );
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
                        return None;
                    }
                }
                Err(err) => {
                    self.record(LogLevel::Error, "ble", format!("链路中断：{err}"));
                    self.lock_inner().link_lost = true;
                    // 链路断了要区分"设备没了"和"蓝牙被关了"：
                    // 后者要落到 no_adapter，界面才会给"打开蓝牙"的指引。
                    let probe = tokio::time::timeout(
                        ADAPTER_PROBE_TIMEOUT,
                        apm_link::probe_adapter(),
                    )
                    .await;
                    return match probe {
                        Ok(Ok(())) => None,
                        _ => Some(AppState::NoAdapter),
                    };
                }
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
                self.record(
                    LogLevel::Info,
                    "status",
                    format!(
                        "state={} flags={:#04x} sent={} dropped={}",
                        status.state, status.flags, status.sent, status.dropped
                    ),
                );
            }
            Notification::Event(event) => {
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
                            p.on_audio(&block);
                            true
                        }
                        None => false,
                    }
                };
                if !streamed {
                    // 没在推流时收到音频说明 CTRL 状态不一致，值得记一条。
                    self.record(
                        LogLevel::Warn,
                        "audio",
                        format!("收到音频块 {} 字节但当前未推流", block.len()),
                    );
                }
            }
            Notification::Pong(seq) => self
                .record(LogLevel::Debug, "ping", format!("PONG seq={seq}")),
            Notification::Battery(pct) => {
                // 设备只在数值变化时通知（1 s 采样），不需要去抖。
                // 同样先落内核再发事件：晚注册监听的前端靠快照补。
                self.lock_inner().battery = Some(Some(pct));
                self.emit(AppEvent::Battery { pct: Some(pct) });
                self.record(
                    LogLevel::Debug,
                    "ble",
                    format!("电量变化：{pct}%"),
                );
            }
        }
    }

    /// 收尾：先停声卡 → 再退订断链 → 最后落状态。
    async fn teardown(self: &Arc<Self>, next: Option<AppState>) {
        self.set_state(AppState::Stopping);
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
        AppCore::new(Some(std::env::temp_dir().join(dir)))
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
    }

    #[test]
    fn analyze_unknown_session_errors() {
        let core = quiet_core("apm-app-test-analyze");
        let err = core.analyze_session("nope").unwrap_err();
        assert_eq!(err.code, ErrorCode::Internal);
        assert!(err.detail.contains("没有录音"), "{}", err.detail);
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
        let path = core.log_path();
        let text = std::fs::read_to_string(&path).expect("日志文件应存在");
        assert!(text.contains("\"message\":\"落盘检查\""), "{text}");
        assert!(text.lines().all(|l| l.starts_with('{')), "{text}");
    }
}
