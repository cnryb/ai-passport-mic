//! AI Passport Mic 跨端协议（PC 侧）。
//!
//! 契约来源：`docs/ARCHITECTURE.md` 的「协议细节（实现契约）」。
//! 固件侧实现在 `device/main/protocol.c`，两端必须逐字节一致 ——
//! `tests/c_vectors.rs` 读的是 C 侧生成的同一份向量文件，不是另写一套期望值。
//!
//! 这个 crate 刻意不依赖任何 BLE 或音频库：协议与指标要能在没有硬件的机器上测。

pub mod adpcm;
pub mod crc;
pub mod device_labels;
pub mod frame;
pub mod glitch;
pub mod keymap;
pub mod metrics;
pub mod wav;

/// 16 kHz 单声道 16-bit：20 ms = 320 采样。
pub const AUDIO_SAMPLE_RATE: u32 = 16_000;
pub const AUDIO_BLOCK_SAMPLES: usize = 320;
pub const AUDIO_BLOCK_HEADER_LEN: usize = 5;
pub const AUDIO_BLOCK_DATA_LEN: usize = 160;
/// 5 字节块头 + 160 字节 nibble = 165。
pub const AUDIO_BLOCK_LEN: usize = AUDIO_BLOCK_HEADER_LEN + AUDIO_BLOCK_DATA_LEN;

/// 单包发完一个音频块所需的 MTU（ATT 头 3 字节）。
pub const AUDIO_REQUIRED_MTU: u16 = AUDIO_BLOCK_LEN as u16 + 3;

/// BLE 服务与特征值 UUID（文本序；`docs/ARCHITECTURE.md` 的 BLE 服务一节）。
pub const SERVICE_UUID: &str = "a7c52e00-31d4-4d2a-8f6b-5e9c1b3f7a00";
pub const CTRL_UUID: &str = "a7c52e00-31d4-4d2a-8f6b-5e9c1b3f7a01";
pub const PROFILE_UUID: &str = "a7c52e00-31d4-4d2a-8f6b-5e9c1b3f7a02";
pub const STATUS_UUID: &str = "a7c52e00-31d4-4d2a-8f6b-5e9c1b3f7a03";
pub const EVENT_UUID: &str = "a7c52e00-31d4-4d2a-8f6b-5e9c1b3f7a04";
pub const AUDIO_UUID: &str = "a7c52e00-31d4-4d2a-8f6b-5e9c1b3f7a05";

/// 标准 Battery Service（`docs/ARCHITECTURE.md` BLE 服务一节）。
/// 固件不存在该服务时（旧固件）电量功能静默降级，不影响其余链路。
pub const BATTERY_SERVICE_UUID: &str = "0000180f-0000-1000-8000-00805f9b34fb";
pub const BATTERY_LEVEL_UUID: &str = "00002a19-0000-1000-8000-00805f9b34fb";
