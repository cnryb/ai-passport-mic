//! 应用状态与错误码。
//!
//! 前端**不允许自造状态**：界面显示的一切都来自这里的 `AppState`，错误一律带
//! `ErrorCode`，前端按 code 查修复指引，不显示裸字符串。

use serde::{Deserialize, Serialize};
use std::fmt;

/// 设备在界面上的展示信息。与 `apm_link::DeviceInfo` 分开，是为了让 IPC 载荷
/// 只包含界面需要的字段（链路内部字段变了不会波及前端）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceView {
    pub id: String,
    pub name: String,
    pub rssi: Option<i16>,
    /// 广播或 GATT 里带本项目的服务 UUID。
    pub has_service: bool,
    pub connected: bool,
}

impl From<apm_link::DeviceInfo> for DeviceView {
    fn from(info: apm_link::DeviceInfo) -> Self {
        Self {
            id: info.id,
            name: info.name,
            rssi: info.rssi,
            has_service: info.has_service,
            connected: info.connected,
        }
    }
}

/// 界面能看到的唯一状态枚举。序列化形状：`{"kind":"ready","device":{...}}`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AppState {
    /// 没有可用适配器，或系统未授权蓝牙。
    NoAdapter,
    /// 就绪，未扫未连。
    Idle,
    Scanning {
        window_ms: u64,
    },
    Connecting {
        device_id: String,
    },
    Ready {
        device: DeviceView,
    },
    /// G2 之前不会出现，先占位以保证前端状态机一次写对。
    Streaming {
        device: DeviceView,
        session_id: String,
    },
    Stopping,
    Error {
        code: ErrorCode,
        detail: String,
    },
}

impl AppState {
    /// 稳定的字符串标识，用于日志与样式类名（不要用 Debug 输出，格式会变）。
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NoAdapter => "no_adapter",
            Self::Idle => "idle",
            Self::Scanning { .. } => "scanning",
            Self::Connecting { .. } => "connecting",
            Self::Ready { .. } => "ready",
            Self::Streaming { .. } => "streaming",
            Self::Stopping => "stopping",
            Self::Error { .. } => "error",
        }
    }

    /// 正在做阻塞性动作，界面要禁用互斥按钮。
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::Scanning { .. } | Self::Connecting { .. } | Self::Stopping
        )
    }

    pub fn device(&self) -> Option<&DeviceView> {
        match self {
            Self::Ready { device } | Self::Streaming { device, .. } => Some(device),
            _ => None,
        }
    }
}

/// 固定错误码。加一个码就要同步改三处：本枚举、前端 `ERROR_HINTS`、`docs/GUI.md` §4。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BtUnauthorized,
    BtUnavailable,
    DeviceNotFound,
    ConnectTimeout,
    SubscribeFailed,
    SinkMissing,
    SinkRateUnsupported,
    ProtocolMismatch,
    /// 按键映射里出现了不支持的键名/修饰键/事件名，或 PTT 停止通道缺失。
    KeymapInvalid,
    Busy,
    Internal,
}

impl ErrorCode {
    /// 给用户看的下一步动作（不是给开发者看的堆栈）。
    pub fn hint(&self) -> &'static str {
        match self {
            Self::BtUnauthorized => "系统设置 → 隐私与安全性 → 蓝牙，允许本 App 后重新检测",
            Self::BtUnavailable => "确认系统蓝牙已打开；虚拟机与远程会话通常没有可用适配器",
            Self::DeviceNotFound => "确认设备已上电并在广播（开机后 3 秒内开始广播）",
            Self::ConnectTimeout => "设备可能已被其他主机占用，靠近一点再试",
            Self::SubscribeFailed => "重连一次；连续失败请重启设备",
            Self::SinkMissing => "选定的虚拟声卡不见了，点「重新枚举」或检查 BlackHole 是否已加载",
            Self::SinkRateUnsupported => "该声卡不支持 16 kHz，请换一个原生支持 16 kHz 的输出",
            Self::ProtocolMismatch => "固件与桌面端版本不一致，重新烧录同一版本",
            Self::KeymapInvalid => "键名或修饰键不在支持列表内；PTT 的停止通道不能删（见 config/README.md）",
            Self::Busy => "当前状态下不能这么做，先停止正在进行的操作",
            Self::Internal => "查看诊断日志；这属于未预期错误",
        }
    }
}

/// 跨 IPC 的错误载荷：永远是 `{code, detail}`，前端不需要解析文本。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppError {
    pub code: ErrorCode,
    pub detail: String,
}

impl AppError {
    pub fn new(code: ErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, detail)
    }

    pub fn busy(detail: impl Into<String>) -> Self {
        Self::new(ErrorCode::Busy, detail)
    }

    pub fn keymap_invalid(detail: impl Into<String>) -> Self {
        Self::new(ErrorCode::KeymapInvalid, detail)
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{:?}] {}", self.code, self.detail)
    }
}

impl std::error::Error for AppError {}

/// 把链路层的 `anyhow` 错误映射成固定错误码。
///
/// 靠消息文本匹配确实不优雅，但 `apm_link` 的错误都是本项目自己写的固定句式，
/// 改动会同时改这里；比给链路层加错误枚举的改动面小得多。匹配不到就归 `Internal`，
/// 不要猜成"设备没找到"——那会让用户去查一个不存在的硬件问题。
pub fn classify(err: &anyhow::Error) -> AppError {
    let text = format!("{err:#}");
    let lower = text.to_lowercase();

    // 顺序有讲究："没有可用的蓝牙适配器（…检查蓝牙权限）" 同时含有"权限"字样，
    // 必须先判适配器，否则会被误报成未授权。
    let code = if text.contains("没有可用的蓝牙适配器") || lower.contains("adapter") {
        ErrorCode::BtUnavailable
    } else if lower.contains("permission")
        || lower.contains("denied")
        || lower.contains("not authorized")
        || text.contains("权限")
    {
        ErrorCode::BtUnauthorized
    } else if text.contains("没找到") {
        ErrorCode::DeviceNotFound
    } else if text.contains("订阅") {
        ErrorCode::SubscribeFailed
    } else if text.contains("超时") || lower.contains("timeout") {
        ErrorCode::ConnectTimeout
    } else {
        ErrorCode::Internal
    };

    AppError::new(code, text)
}

impl From<anyhow::Error> for AppError {
    fn from(err: anyhow::Error) -> Self {
        classify(&err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn state_kind_is_stable() {
        assert_eq!(AppState::Idle.kind(), "idle");
        assert_eq!(AppState::Scanning { window_ms: 6000 }.kind(), "scanning");
        assert!(AppState::Connecting {
            device_id: "x".into()
        }
        .is_busy());
        assert!(!AppState::Idle.is_busy());
    }

    #[test]
    fn state_serializes_with_kind_tag() {
        let json = serde_json::to_string(&AppState::Scanning { window_ms: 6000 }).unwrap();
        assert_eq!(json, r#"{"kind":"scanning","window_ms":6000}"#);

        let err = AppState::Error {
            code: ErrorCode::BtUnauthorized,
            detail: "TCC".into(),
        };
        let json = serde_json::to_string(&err).unwrap();
        assert!(json.contains(r#""code":"bt_unauthorized""#), "{json}");
    }

    #[test]
    fn classify_maps_known_messages() {
        let cases = [
            (
                "初始化蓝牙管理器失败: permission denied",
                ErrorCode::BtUnauthorized,
            ),
            ("没有可用的蓝牙适配器（macOS 上检查系统设置里的蓝牙权限）", ErrorCode::BtUnavailable),
            ("没找到匹配 \"AI Passport\" 的设备", ErrorCode::DeviceNotFound),
            ("订阅 0000-… 失败", ErrorCode::SubscribeFailed),
            ("连接超时", ErrorCode::ConnectTimeout),
        ];
        for (text, want) in cases {
            let got = classify(&anyhow!("{text}"));
            assert_eq!(got.code, want, "{text}");
        }
    }

    #[test]
    fn unknown_errors_stay_internal() {
        let got = classify(&anyhow!("unexpected"));
        assert_eq!(got.code, ErrorCode::Internal);
        assert_eq!(got.detail, "unexpected");
    }

    #[test]
    fn error_code_hints_are_not_empty() {
        for code in [
            ErrorCode::BtUnauthorized,
            ErrorCode::BtUnavailable,
            ErrorCode::DeviceNotFound,
            ErrorCode::ConnectTimeout,
            ErrorCode::SubscribeFailed,
            ErrorCode::SinkMissing,
            ErrorCode::SinkRateUnsupported,
            ErrorCode::ProtocolMismatch,
            ErrorCode::KeymapInvalid,
            ErrorCode::Busy,
            ErrorCode::Internal,
        ] {
            assert!(!code.hint().is_empty(), "{code:?}");
        }
    }
}
