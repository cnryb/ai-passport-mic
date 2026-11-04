//! 应用内核：状态机 + 编排 + 事件广播 + 诊断日志。
//!
//! 设计约束（见 `docs/GUI.md` §3）：
//! - 这里**不依赖任何 GUI 框架**，CLI 与 GUI 共用；
//! - 长动作（扫描/连接）由调用方 `await`，内核自己不假设有全局运行时；
//!   只有需要脱离命令生命周期的通知监控，才用调用方传入的 tokio `Handle` 派生。
//! - 状态迁移只有一个入口（`set_state`），保证每次变化都同时更新事件与日志。

use crate::events::{AppEvent, LogEntry, LogLevel};
use crate::log::{epoch_ms, LogSink};
use crate::state::{AppError, AppState, DeviceView};
use apm_link::Notification;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};
use std::time::Duration;
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

struct Inner {
    state: AppState,
    devices: Vec<DeviceView>,
    /// 存在表示正在扫描；`send(true)` 即请求提前结束。
    scan_cancel: Option<watch::Sender<bool>>,
    /// 存在表示通知监控在跑；`send(true)` 即请求收尾断连。
    link_stop: Option<watch::Sender<bool>>,
    /// 第一次进异步上下文时记下运行时句柄，供派生监控任务用。
    exec: Option<tokio::runtime::Handle>,
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
                exec: None,
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

    pub fn snapshot(&self) -> AppSnapshot {
        AppSnapshot {
            version: self.version.to_string(),
            state: self.state(),
            devices: self.devices(),
            log_tail: self.tail_snapshot(),
            log_path: self.log_path(),
            log_dir: self.log_dir(),
        }
    }

    /// 检测适配器。失败时状态落到 `no_adapter`，并把具体原因写进日志。
    pub async fn detect_adapter(&self) -> Result<(), AppError> {
        self.remember_exec();
        self.record(LogLevel::Info, "ble", "检测蓝牙适配器");
        let probed = tokio::time::timeout(ADAPTER_PROBE_TIMEOUT, apm_link::probe_adapter()).await;
        match probed {
            Ok(Ok(())) => {
                if matches!(self.state(), AppState::NoAdapter) {
                    self.record(LogLevel::Info, "ble", "蓝牙已恢复可用");
                }
                self.set_state(AppState::Idle);
                Ok(())
            }
            Ok(Err(err)) => {
                let err = AppError::from(err);
                self.record(
                    LogLevel::Error,
                    "ble",
                    format!("适配器不可用（{:?}）：{}", err.code, err.detail),
                );
                self.set_state(AppState::NoAdapter);
                Err(err)
            }
            Err(_) => {
                // 超时不要伪装成"设备没找到"：绝大多数情况是授权弹窗没点，或蓝牙被关。
                let err = AppError::new(
                    crate::state::ErrorCode::BtUnavailable,
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
                self.set_state(AppState::NoAdapter);
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
                self.fail(err.clone());
                return Err(err);
            }
            Err(_) => {
                let err = AppError::new(
                    crate::state::ErrorCode::BtUnavailable,
                    "连接超时：适配器未就绪（授权弹窗未点或蓝牙已关闭）",
                );
                self.record(LogLevel::Error, "ble", format!("连接超时：{}", err.detail));
                self.fail(err.clone());
                return Err(err);
            }
        };

        let device = self
            .lock_inner()
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
                name: device_id.clone(),
                rssi: None,
                has_service: true,
                connected: true,
            });

        *self.link.lock().await = Some(link);
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

    /// 断开。真正的收尾（退订 + 断链 + 状态回 `idle`）在监控任务里做，
    /// 保证"先停止写入、再断链"的顺序与 CLI 修过的收尾顺序一致。
    pub async fn disconnect(self: &Arc<Self>) {
        let tx = self.lock_inner().link_stop.take();
        match tx {
            Some(tx) => {
                let _ = tx.send(true);
            }
            None => {
                if self.link.lock().await.is_some() {
                    self.teardown().await;
                }
            }
        }
    }

    /// 给"复制诊断包"用的纯文本。要求能直接粘进 issue 就有人看得懂。
    pub fn diagnostics_text(&self) -> String {
        let state = self.state();
        let devices = self.devices();
        let tail = self.tail_snapshot();
        let mut out = String::new();
        out.push_str("== AI Passport Mic 诊断包 ==\n");
        out.push_str(&format!("版本: {}\n", self.version));
        out.push_str(&format!("状态: {}\n", state.kind()));
        out.push_str(&format!("日志: {}\n", self.log_path()));
        out.push_str(&format!("设备: {}\n", devices.len()));
        for d in &devices {
            out.push_str(&format!(
                "  - {} ({}) rssi={:?} ours={} connected={}\n",
                d.name, d.id, d.rssi, d.has_service, d.connected
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
        handle.spawn(async move { core.monitor_loop(stop_rx).await });
    }

    async fn monitor_loop(self: Arc<Self>, stop: watch::Receiver<bool>) {
        loop {
            if *stop.borrow() {
                break;
            }
            let mut guard = self.link.lock().await;
            let Some(link) = guard.as_mut() else {
                break;
            };
            // 700 ms 超时保证 stop 信号最多延迟这么久生效。
            let next = link.next_notification(Duration::from_millis(700)).await;
            drop(guard);

            match next {
                Ok(Some(Notification::Status(status))) => self.record(
                    LogLevel::Info,
                    "status",
                    format!(
                        "state={} flags={:#04x} sent={} dropped={}",
                        status.state, status.flags, status.sent, status.dropped
                    ),
                ),
                Ok(Some(Notification::Event(event))) => self.record(
                    LogLevel::Info,
                    "button",
                    format!(
                        "button={} event={} flags={:#04x}",
                        event.button, event.event, event.flags
                    ),
                ),
                // G0 不推流，设备也不该发音频。真发来了说明 CTRL 状态不一致，
                // 记一条 warn 而不是静默丢弃——G2 才处理音频。
                Ok(Some(Notification::Audio(block))) => self.record(
                    LogLevel::Debug,
                    "audio",
                    format!("收到音频块 {} 字节但未在推流（G2 前正常）", block.len()),
                ),
                Ok(None) => {}
                Err(err) => {
                    self.record(LogLevel::Error, "ble", format!("链路中断：{err}"));
                    break;
                }
            }
        }
        self.teardown().await;
    }

    async fn teardown(&self) {
        self.set_state(AppState::Stopping);
        let link = self.link.lock().await.take();
        if let Some(link) = link {
            if let Err(err) = link.disconnect().await {
                self.record(LogLevel::Warn, "ble", format!("断开时出错：{err}"));
            }
        }
        self.lock_inner().link_stop = None;
        self.set_state(AppState::Idle);
        self.record(LogLevel::Info, "ble", "已断开");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::LogLevel;

    fn quiet_core(dir: &str) -> Arc<AppCore> {
        AppCore::new(Some(std::env::temp_dir().join(dir)))
    }

    #[test]
    fn starts_idle_and_logs_boot() {
        let core = quiet_core("apm-app-test-boot");
        assert_eq!(core.state().kind(), "idle");
        let snap = core.snapshot();
        assert!(snap.log_tail.iter().any(|e| e.message.contains("启动")));
        assert_eq!(snap.version, env!("CARGO_PKG_VERSION"));
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
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let scan = rt.block_on(core.start_scan(3000));
        let err = scan.unwrap_err();
        assert_eq!(err.code, crate::state::ErrorCode::Busy);
    }

    #[test]
    fn diagnostics_text_has_headers() {
        let core = quiet_core("apm-app-test-diag");
        core.record(LogLevel::Warn, "ble", "示例警告");
        let text = core.diagnostics_text();
        assert!(text.contains("诊断包"));
        assert!(text.contains("状态: idle"));
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
