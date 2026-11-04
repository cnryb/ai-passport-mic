//! AI Passport Mic 的 BLE 链路。
//!
//! 只做第一版需要的事：按设备名或服务 UUID 找设备、连接、订阅三个 notify 特征值、
//! 把通知解析成协议类型。**不做**烧录、不做 HID、不碰系统蓝牙配对设置。
//!
//! 已知限制：btleplug 不暴露协商后的 MTU，所以 PC 端无法直接断言"能单包发 165 字节"。
//! 设备侧会检查并在 STATUS / 日志里报告；PC 侧只能从实际收到的块长度反推。

pub mod device;

pub use device::{connect, probe_adapter, scan, scan_stream, DeviceInfo, Link, Notification};

/// 固件里音频块长度（165）—— 收满这个长度才算一个完整块。
pub const AUDIO_BLOCK_LEN: usize = apm_protocol::AUDIO_BLOCK_LEN;
