//! AI Passport Mic 的无 UI 应用内核。
//!
//! CLI 与 GUI 都只调这里，不允许各自再写一套编排（`docs/GUI.md` §3 的决定 D1）。
//! 本 crate **不得**依赖 tauri 或任何 GUI 框架。

pub mod core;
pub mod events;
pub mod firmware;
pub mod inject;
pub mod keymap_store;
pub mod log;
pub mod pipeline;
pub mod report;
pub mod session;
pub mod settings;
pub mod state;

pub use core::{AppCore, AppSnapshot, SessionRecord, SinkView};
pub use events::{AppEvent, LogEntry, LogLevel};
pub use firmware::{FirmwareImage, FlashReport, ImageInfo, PortView, ProbeResult};
pub use pipeline::Pipeline;
pub use report::{QualityReport, ReportCheck};
pub use session::{SessionOptions, SessionSummary, Telemetry};
pub use settings::Settings;
pub use state::{AppError, AppState, DeviceView, ErrorCode};
