//! 跨 IPC 的事件与日志条目。
//!
//! 事件是"边沿"：只在状态变化、设备列表变化、产生日志时发一次。逐块的音频与
//! 指标不走这里（G2 的 `telemetry` 按 5 Hz 合批，见 `docs/GUI.md` §3）。

use crate::state::{AppState, DeviceView};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// 一条诊断日志。`ts_ms` 是 **epoch 毫秒**（写文件用），渲染时由前端本地化。
/// 事件顺序与超时判断不要依赖它，用单调时钟（`Instant`）。
#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub ts_ms: u64,
    pub level: LogLevel,
    /// 来源：app / ble / status / button / sink
    pub source: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum AppEvent {
    StateChanged {
        state: AppState,
    },
    Devices {
        devices: Vec<DeviceView>,
    },
    EventLog {
        entry: LogEntry,
    },
}
