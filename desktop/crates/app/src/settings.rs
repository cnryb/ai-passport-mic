//! 本机配置与数据目录。
//!
//! 三条约定（`docs/GUI.md` §12）：
//! - 声卡选择与缓冲目标要持久化，下次启动恢复；
//! - 会话 WAV **默认不落盘**，勾选状态本身也持久化；
//! - 不写注册表、不写系统目录：所有东西都在 App 自己的 Application Support 下。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// bundle identifier 固定（`docs/GUI.md` §8 硬约束 1）：目录名跟着它走，
/// 改名字等于把用户已有配置扔掉。
const APP_DIR: &str = "com.cnryb.ai-passport-mic";

/// 抖动缓冲默认 80 ms：实测 60 时会摆到 28–60，80 时稳在 76–104（`cli/main.rs` 的注释）。
pub const DEFAULT_TARGET_MS: u32 = 80;
/// 缓冲目标下限 / 上限（`docs/ARCHITECTURE.md` 的 40–80 ms）。
pub const TARGET_MIN_MS: u32 = 40;
pub const TARGET_MAX_MS: u32 = 80;

/// 缓冲目标的唯一夹紧点。界面滑块、配置文件、命令行都走这里，
/// 免得三处各夹一次、边界还不一样。
pub fn clamp_target(ms: u32) -> u32 {
    ms.clamp(TARGET_MIN_MS, TARGET_MAX_MS)
}

pub fn data_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join("Library/Application Support")
                .join(APP_DIR);
        }
    }
    // 非 macOS 的退回路径：XDG_DATA_HOME 优先，其次 ~/.local/share。
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".local/share"))
            .unwrap_or_else(std::env::temp_dir),
    };
    base.join(APP_DIR)
}

pub fn sessions_dir() -> PathBuf {
    data_dir().join("sessions")
}

pub fn keymap_path() -> PathBuf {
    data_dir().join("keymap.json")
}

pub fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// 上次选中的输出设备名。`None` = 还没选过。
    #[serde(default)]
    pub sink: Option<String>,
    #[serde(default = "default_target_ms")]
    pub target_ms: u32,
    /// 会话 WAV 是否默认落盘。默认 false（隐私口径）。
    #[serde(default)]
    pub save_wav: bool,
    /// 上次成功连接的设备 ID（macOS 上是 CoreBluetooth identifier，
    /// 跨进程重启稳定），启动自动连接用。`None` = 还没连过任何设备。
    #[serde(default)]
    pub last_device_id: Option<String>,
    /// 上次连接的设备名，只用于界面展示与自动连接失败时的日志。
    #[serde(default)]
    pub last_device_name: Option<String>,
    /// 启动后是否自动连接上次的设备。默认开——这是"打开就能用"的预期。
    #[serde(default = "default_true")]
    pub auto_connect: bool,
}

fn default_target_ms() -> u32 {
    DEFAULT_TARGET_MS
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            sink: None,
            target_ms: DEFAULT_TARGET_MS,
            save_wav: false,
            last_device_id: None,
            last_device_name: None,
            auto_connect: true,
        }
    }
}

impl Settings {
    /// 读配置。文件不存在或坏了都回默认值 —— 配置读不出来不该让麦克风用不了。
    pub fn load() -> Self {
        let path = settings_path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let dir = data_dir();
        std::fs::create_dir_all(&dir)?;
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(settings_path(), text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_live_under_app_support() {
        assert!(data_dir().ends_with(APP_DIR));
        assert!(keymap_path().ends_with("keymap.json"));
        assert!(sessions_dir().ends_with("sessions"));
    }

    #[test]
    fn defaults_are_private_by_design() {
        let s = Settings::default();
        assert!(!s.save_wav, "会话 WAV 默认必须不落盘");
        assert_eq!(s.target_ms, 80);
    }

    #[test]
    fn settings_roundtrip() {
        let s = Settings {
            sink: Some("BlackHole 2ch".into()),
            target_ms: 60,
            save_wav: true,
            last_device_id: Some("ABCD-1234".into()),
            last_device_name: Some("AI Passport".into()),
            auto_connect: false,
        };
        let text = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&text).unwrap();
        assert_eq!(back.sink.as_deref(), Some("BlackHole 2ch"));
        assert_eq!(back.target_ms, 60);
        assert!(back.save_wav);
        assert_eq!(back.last_device_id.as_deref(), Some("ABCD-1234"));
        assert_eq!(back.last_device_name.as_deref(), Some("AI Passport"));
        assert!(!back.auto_connect);
    }

    #[test]
    fn old_settings_without_device_fields_default_to_auto_connect() {
        // 旧版 settings.json 没有 last_device_* / auto_connect 字段：
        // 反序列化必须不炸，且自动连接默认是开的。
        let back: Settings = serde_json::from_str("{\"target_ms\":60}").unwrap();
        assert!(back.auto_connect);
        assert!(back.last_device_id.is_none());
    }

    #[test]
    fn broken_json_falls_back_to_default() {
        // 只验证反序列化行为，不真的写用户目录。
        let back: Settings = serde_json::from_str("{\"target_ms\":\"x\"}").unwrap_or_default();
        assert_eq!(back.target_ms, DEFAULT_TARGET_MS);
    }
}
