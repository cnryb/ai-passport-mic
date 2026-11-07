//! Tauri 命令层：只做"参数搬运 + 调内核"，不放任何业务分支。
//!
//! 规则（`docs/GUI.md` §3 的 D4）：
//! - 每个命令都薄到可以一眼看完；
//! - 长动作靠事件回报进度，命令本身 await 到结束即可；
//! - 错误统一是 `{code, detail}`，前端按 code 查指引。

use crate::GuiState;
use apm_app::keymap_store::{self, BindingInput, KeymapView};
use apm_app::{
    firmware, AppError, AppSnapshot, FlashReport, ImageInfo, PortView, ProbeResult, QualityReport,
    SessionSummary, SinkView,
};
use apm_app::state::ErrorCode;
use apm_app::LogLevel;
use serde::Serialize;
use std::path::PathBuf;
use tauri::{AppHandle, Emitter, Manager, State};

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

/// 启动自动连接开关（立即落盘）。
#[tauri::command]
pub fn set_auto_connect(state: State<'_, GuiState>, enabled: bool) -> Result<(), AppError> {
    state.core.set_auto_connect(enabled)
}

/// 异步命令只要带 `State<'_, _>` 就必须返回 `Result`（Tauri 的借用检查约束），
/// 所以这里即使不失败也包一层 `Ok(())`。
#[tauri::command]
pub async fn disconnect(state: State<'_, GuiState>) -> Result<(), AppError> {
    state.core.disconnect().await;
    Ok(())
}

// ---- G2 / G3：声卡与推流 ----

#[tauri::command]
pub fn list_sinks(state: State<'_, GuiState>) -> Result<Vec<SinkView>, AppError> {
    state.core.list_sinks()
}

#[tauri::command]
pub fn set_sink(state: State<'_, GuiState>, name: String) -> Result<(), AppError> {
    state.core.set_sink(name)
}

#[tauri::command]
pub fn set_save_wav(state: State<'_, GuiState>, save: bool) -> Result<(), AppError> {
    state.core.set_save_wav(save)
}

#[tauri::command]
pub fn set_buffer_target(state: State<'_, GuiState>, ms: u32) -> Result<(), AppError> {
    state.core.set_buffer_target(ms)
}

/// 关麦兜底时长（分钟）。运行中改也生效（看门狗每轮现读配置）。
#[tauri::command]
pub fn set_auto_stop_min(state: State<'_, GuiState>, minutes: u32) -> Result<(), AppError> {
    state.core.set_auto_stop_min(minutes)
}

#[tauri::command]
pub async fn start_stream(state: State<'_, GuiState>, save_wav: bool) -> Result<String, AppError> {
    state.core.start_stream(save_wav).await
}

#[tauri::command]
pub async fn stop_stream(state: State<'_, GuiState>) -> Result<SessionSummary, AppError> {
    state.core.stop_stream().await
}

// ---- G4：音质报告 ----

#[tauri::command]
pub fn analyze_session(
    state: State<'_, GuiState>,
    session_id: String,
) -> Result<QualityReport, AppError> {
    state.core.analyze_session(&session_id)
}

#[tauri::command]
pub fn analyze_session_json(
    state: State<'_, GuiState>,
    session_id: String,
) -> Result<String, AppError> {
    state.core.analyze_session_json(&session_id)
}

/// 在访达里选中一次会话的 WAV。路径由后端给出，前端不自己拼目录。
#[tauri::command]
pub fn reveal_path(state: State<'_, GuiState>, path: String) -> Result<(), AppError> {
    let _ = state;
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("-R")
            .arg(&path)
            .spawn()
            .map(|_| ())
            .map_err(|e| AppError::internal(format!("在访达中显示失败：{e}")))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        Err(AppError::internal("首发只支持 macOS"))
    }
}

// ---- G5：按键映射 ----
//
// 校验全在 `apm_app::keymap_store`（复用 `apm_protocol::keymap`），
// 命令层不重复判断"这个键名合不合法"。

#[tauri::command]
pub fn keymap_view() -> KeymapView {
    keymap_store::view()
}

#[tauri::command]
pub fn keymap_save(bindings: Vec<BindingInput>) -> Result<KeymapView, AppError> {
    keymap_store::save_bindings(&bindings).map_err(AppError::keymap_invalid)
}

#[tauri::command]
pub fn keymap_import(text: String) -> Result<KeymapView, AppError> {
    keymap_store::save_text(&text).map_err(AppError::keymap_invalid)
}

#[tauri::command]
pub fn keymap_reset() -> Result<KeymapView, AppError> {
    keymap_store::reset().map_err(AppError::keymap_invalid)
}

#[tauri::command]
pub fn keymap_text() -> String {
    keymap_store::text()
}

// ---- M3：注入权限引导 ----
//
// 没有辅助功能权限时 CGEventPost 静默丢弃（`apm_app::inject` 的探针已经把这层
// 拦在"发出错误"而不是"按了没反应"），但用户仍得自己摸到系统设置那一页。
// 这两个命令只做引导：查状态 + 跳面板，权限本身应用内无法替代勾选。

/// 当前 App 有没有辅助功能权限（注入快捷键的前提）。
#[tauri::command]
pub fn inject_permission() -> bool {
    apm_app::inject::available()
}

/// 打开系统设置的"辅助功能"面板，把用户送到能勾选的那一页。
///
/// 关键顺序：**先请求权限、再开面板**。纯探测（`AXIsProcessTrusted`）不会让本 App
/// 出现在辅助功能列表里；带 `kAXTrustedCheckOptionPrompt` 的请求会弹系统授权框，
/// 并把本 App 以未勾选状态登记进列表——用户接下来只需拨开关，不用手动添加。
#[tauri::command]
pub fn open_accessibility_settings() -> Result<(), AppError> {
    #[cfg(target_os = "macos")]
    {
        // 弹授权框（若已授权则无副作用），列表里由此出现本 App。
        let _ = apm_app::inject::request_permission();
        std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
            .spawn()
            .map(|_| ())
            .map_err(|e| AppError::internal(format!("打开系统设置失败：{e}")))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(AppError::internal("首发只支持 macOS"))
    }
}

// ---- M6：固件烧写 ----
//
// 逻辑全在 `apm_app::firmware`（无 UI，CLI 也能用）。这里只做三件事：
// 找镜像路径、把阻塞的串口操作塞进 `spawn_blocking`、把进度转成事件。
//
// **不提供后台轮询**：probe 会复位设备进下载模式，正推流时被偷偷复位比"没检测到"
// 更难解释。只有用户明确点"检测"或"开始烧写"时才碰设备。

/// 开箱页首屏数据：随包镜像 + 候选串口。不碰设备，可以随便调。
#[derive(Debug, Serialize)]
pub struct FirmwareStatus {
    pub image: Option<ImageInfo>,
    /// 镜像缺失/损坏的原因。有值时不显示"可烧写"——别拿坏包去写设备。
    pub image_error: Option<String>,
    pub ports: Vec<PortView>,
}

/// 随包镜像在哪。
///
/// 打包后读 `Resources/firmware/...`；开发态回退到仓库构建产物，
/// 省掉"改一次固件就要重打包一次"的循环。两条路径都不存在才算错。
fn firmware_path(app: &AppHandle) -> Result<PathBuf, AppError> {
    let bundled = app
        .path()
        .resolve(
            "firmware/ai-passport-mic-merged.bin",
            tauri::path::BaseDirectory::Resource,
        )
        .ok()
        .filter(|p| p.exists());
    if let Some(p) = bundled {
        return Ok(p);
    }

    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../device/build/ai-passport-mic-merged.bin");
    if dev.exists() {
        return Ok(dev);
    }

    Err(AppError::new(
        ErrorCode::FirmwareImageInvalid,
        "找不到随包固件镜像 firmware/ai-passport-mic-merged.bin。安装包不完整，或本地还没跑过 ./scripts/build-firmware.sh",
    ))
}

#[tauri::command]
pub fn firmware_status(app: AppHandle) -> FirmwareStatus {
    let ports = firmware::list_ports();
    match firmware_path(&app).and_then(|p| firmware::inspect_image(&p).map_err(AppError::from)) {
        Ok(image) => FirmwareStatus {
            image: Some(image),
            image_error: None,
            ports,
        },
        Err(e) => FirmwareStatus {
            image: None,
            image_error: Some(e.detail),
            ports,
        },
    }
}

/// 读设备当前版本。**会让设备复位**，界面上要提前说清楚。
#[tauri::command]
pub async fn firmware_probe(
    state: State<'_, GuiState>,
    app: AppHandle,
    port: String,
) -> Result<ProbeResult, AppError> {
    let was_connected = state.core.snapshot().state.device().map(|d| d.id.clone());
    let path = firmware_path(&app)?;
    let joined = tauri::async_runtime::spawn_blocking(move || -> Result<ProbeResult, AppError> {
        let image = firmware::load_image(&path)?;
        firmware::probe(&port, &image).map_err(AppError::from)
    })
    .await
    .map_err(|e| AppError::internal(format!("检测任务异常退出：{e}")))?;
    let result = joined?;

    // 检测本身就把设备复位了，BLE 会断。用户不该为此手动重连一次。
    // 重连失败不影响检测结果——检测结果已经拿到了。
    if let Some(id) = was_connected {
        let _ = state.core.reconnect_after_reset(&id).await;
    }
    Ok(result)
}

/// 烧写并校验。进度走 `firmware-progress`（0–100），返回值 `verified` 必为 true，
/// 否则是 `Err`——不允许把"写失败"渲染成"完成，但有点小问题"。
#[tauri::command]
pub async fn firmware_flash(
    state: State<'_, GuiState>,
    app: AppHandle,
    port: String,
) -> Result<FlashReport, AppError> {
    let was_connected = state.core.snapshot().state.device().map(|d| d.id.clone());
    let path = firmware_path(&app)?;
    let handle = app.clone();

    let joined = tauri::async_runtime::spawn_blocking(move || -> Result<FlashReport, AppError> {
        let image = firmware::load_image(&path)?;
        firmware::flash(&port, &image, |percent| {
            let _ = handle.emit("firmware-progress", percent);
        })
        .map_err(AppError::from)
    })
    .await
    .map_err(|e| AppError::internal(format!("烧写任务异常退出：{e}")))?;
    let report = joined?;

    // 烧完设备已经在跑新固件，把它接回 BLE，省掉"再去实时页点一次连接"。
    if let Some(id) = was_connected {
        let _ = state.core.reconnect_after_reset(&id).await;
    }
    Ok(report)
}

// ---- 诊断 ----

/// 生成可直接粘贴的诊断包。写剪贴板交给前端（`navigator.clipboard` 不可用时
/// 回退到 textarea + execCommand），这样后端不需要引额外插件。
#[tauri::command]
pub fn diagnostics_text(state: State<'_, GuiState>) -> String {
    state.core.diagnostics_text()
}

/// 前端上报一段实测帧率（5 s 窗口），写进诊断日志。
///
/// 存在的理由：G2 的"前端 ≥ 30 fps"此前一直是"未测（没有采集手段）"。
/// WebView 内拿不到可无人值守读取的帧计数口，所以在真实渲染循环里数帧、
/// 每 5 s 上报一次，由 `scripts/test-gui-perf.sh` 汇总。
///
/// 级别用 `Debug`：它在推流期间每 5 s 一条，不该污染界面的诊断页（那里显示
/// warn/error 为主），但必须落盘可查。异常帧率（< 30）单独提成 `Warn` 留证据。
#[tauri::command]
pub fn report_fps(state: State<'_, GuiState>, fps: f64, window_ms: u32) {
    if !fps.is_finite() || fps <= 0.0 {
        return;
    }
    let level = if fps < 30.0 {
        LogLevel::Warn
    } else {
        LogLevel::Debug
    };
    state.core.record(
        level,
        "fps",
        format!("FPS {fps:.1} over {window_ms}ms"),
    );
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
