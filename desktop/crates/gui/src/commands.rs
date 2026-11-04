//! Tauri 命令层：只做"参数搬运 + 调内核"，不放任何业务分支。
//!
//! 规则（`docs/GUI.md` §3 的 D4）：
//! - 每个命令都薄到可以一眼看完；
//! - 长动作靠事件回报进度，命令本身 await 到结束即可；
//! - 错误统一是 `{code, detail}`，前端按 code 查指引。

use crate::GuiState;
use apm_app::{AppError, AppSnapshot};
use tauri::State;

#[tauri::command]
pub fn snapshot(state: State<'_, GuiState>) -> AppSnapshot {
    state.core.snapshot()
}

#[tauri::command]
pub async fn detect_adapter(state: State<'_, GuiState>) -> Result<(), AppError> {
    state.core.detect_adapter().await
}

/// 扫描窗口默认 6 s（`docs/GUI.md` §6）。返回时列表已经通过 `devices` 事件推完。
#[tauri::command]
pub async fn scan_start(state: State<'_, GuiState>, window_ms: Option<u64>) -> Result<(), AppError> {
    state.core.start_scan(window_ms.unwrap_or(6_000)).await
}

#[tauri::command]
pub fn scan_stop(state: State<'_, GuiState>) {
    state.core.stop_scan();
}

#[tauri::command]
pub async fn connect(state: State<'_, GuiState>, device_id: String) -> Result<(), AppError> {
    state.core.connect(device_id).await
}

/// 异步命令只要带 `State<'_, _>` 就必须返回 `Result`（Tauri 的借用检查约束），
/// 所以这里即使不失败也包一层 `Ok(())`。
#[tauri::command]
pub async fn disconnect(state: State<'_, GuiState>) -> Result<(), AppError> {
    state.core.disconnect().await;
    Ok(())
}

/// 生成可直接粘贴的诊断包。写剪贴板交给前端（`navigator.clipboard` 不可用时
/// 回退到 textarea + execCommand），这样后端不需要引额外插件。
#[tauri::command]
pub fn diagnostics_text(state: State<'_, GuiState>) -> String {
    state.core.diagnostics_text()
}

#[tauri::command]
pub fn log_path(state: State<'_, GuiState>) -> String {
    state.core.log_path()
}

/// 在访达里打开日志目录。仅 macOS 首发，其他平台明确报错而不是静默失败。
#[tauri::command]
pub fn reveal_log(state: State<'_, GuiState>) -> Result<(), AppError> {
    let dir = state.core.log_dir();
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&dir)
            .spawn()
            .map(|_| ())
            .map_err(|e| AppError::internal(format!("打开日志目录失败：{e}")))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = dir;
        Err(AppError::internal("首发只支持 macOS"))
    }
}
