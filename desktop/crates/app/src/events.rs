//! 跨 IPC 的事件与日志条目。
//!
//! 事件是"边沿"：状态变化、设备列表变化、产生日志时发一次。
//! 逐块音频**不进**事件通道（160 B × 50/s 喂 webview 没有意义，且必然引入抖动）：
//! `telemetry` 按 5 Hz 合批，前端只拿数字（`docs/GUI.md` §3 的 D2 / D3）。

use crate::session::{SessionSummary, Telemetry};
use crate::state::{AppState, DeviceView, ErrorCode};
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
    /// 5 Hz。前端拿到就覆盖，不做累加 —— 否则丢一个事件就永久偏了。
    Telemetry {
        telemetry: Telemetry,
    },
    SessionDone {
        /// Boxed：`SessionSummary` 有 30+ 字段（~400 字节），直接内联会让
        /// `AppEvent` 的每个变体都按最大值占位。这是低频事件，一次堆分配无成本。
        summary: Box<SessionSummary>,
    },
    /// 设备电量百分比（0..100）。只在数值变化或连接后首次读到时发一次，不是心跳。
    ///
    /// `pct = None` 表示**电量计不可用**，不是"还没读到"：设备没有 Battery Service、
    /// 读特征值失败、或设备侧明确回报"电量未知"（CW2017 的 SOC 寄存器返回 0xFF 时
    /// 固件不上报数字）。前端据此显示「电量不可用」，既不显示 0%、也不静默隐藏——
    /// 静默隐藏会让人以为这个功能没做（见 docs/KNOWN_ISSUES.md KI-007）。
    Battery {
        pct: Option<u8>,
    },
    /// 推流被内核主动中止（不是用户点的停止）。
    ///
    /// 目前只有一种来源：开流后 `EARLY_STOP_WINDOW` 内收到的块数过少，判定
    /// "设备没开始发送/链路异常"（KI-005 的 PC 侧护栏）。
    ///
    /// **为什么不走 `state-changed` 的 error 状态**：早停之后状态会回到 `ready`，
    /// 用户应该能立刻重试；落到 `error` 会让「开始」按钮一直禁用，还得手动断开重连。
    /// 但错误又不能吞掉——否则用户只会看到"点了开始又自己停了"。所以用独立边沿事件，
    /// 前端弹一次横幅，状态机保持可用。
    StreamAborted {
        code: ErrorCode,
        detail: String,
    },
}
