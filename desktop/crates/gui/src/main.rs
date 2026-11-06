// 只在 Windows 上隐藏控制台窗口；macOS 上不需要该属性。
#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

mod commands;

use apm_app::{AppCore, LogLevel};
use std::sync::Arc;
use tauri::Emitter;

/// 命令层共享的状态。只持有内核的 Arc，不持有内核不该知道的东西，
/// 保证 `apm-app` 仍然与 GUI 解耦。
pub struct GuiState {
    pub core: Arc<AppCore>,
}

/// 无人值守冒烟：检测适配器 → 扫描 6 s → 记录结果 → 退出。
/// 退出码 0 表示至少发现一个带本项目服务 UUID 的设备。
async fn run_smoke(core: Arc<AppCore>) {
    let mut code = 1;
    if core.detect_adapter().await.is_ok() {
        let _ = core.start_scan(6_000).await;
        let devices = core.devices();
        let ours = devices.iter().filter(|d| d.has_service).count();
        core.record(
            LogLevel::Info,
            "smoke",
            format!("SMOKE_RESULT devices={} ours={ours}", devices.len()),
        );
        if ours > 0 {
            code = 0;
        }
    } else {
        core.record(LogLevel::Error, "smoke", "SMOKE_RESULT adapter=unavailable");
    }
    core.record(LogLevel::Info, "smoke", format!("SMOKE_EXIT {code}"));
    std::process::exit(code);
}

/// 无人值守冒烟：连接 → USB 检测（必然复位设备）→ 自动重连。
///
/// 验的是"点一次检测不会把 BLE 永久打断"这条回归：`firmware_probe` 命令里
/// 就是 `probe` 成功后接 `reconnect_after_reset`，这里走同一串调用，
/// 只是把结果写进诊断日志而不是画到界面上。
/// 只读版本，不写 flash——冒烟不该改设备状态。
async fn run_firmware_smoke(core: Arc<AppCore>) {
    let fail = |reason: &str| -> ! {
        core.record(LogLevel::Error, "smoke", format!("FW_SMOKE fail={reason}"));
        std::process::exit(1);
    };

    if core.detect_adapter().await.is_err() {
        fail("adapter=unavailable");
    }

    // 先把设备连上（用户平时就是这么连的）：优先自动连接，没记录就扫一遍。
    let _ = core.auto_connect().await;
    if core.snapshot().state.device().is_none() {
        let _ = core.start_scan(6_000).await;
        if let Some(id) = core
            .devices()
            .iter()
            .find(|d| d.has_service)
            .map(|d| d.id.clone())
        {
            let _ = core.connect(id).await;
        }
    }
    let Some(device_id) = core.snapshot().state.device().map(|d| d.id.clone()) else {
        fail("no_device_connected");
    };

    let Some(port) = apm_app::firmware::list_ports().into_iter().next() else {
        fail("no_usb_port");
    };

    let image_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../device/build/ai-passport-mic-merged.bin");
    let Ok(image) = apm_app::firmware::load_image(&image_path) else {
        fail("no_image");
    };

    core.record(LogLevel::Info, "smoke", format!("FW_SMOKE probe port={}", port.path));
    let probe = apm_app::firmware::probe(&port.path, &image);
    let probe_note = match &probe {
        Ok(r) => format!("{:?}/{}", r.state, r.chip),
        Err(e) => format!("err={e}"),
    };

    let reconnected = core.reconnect_after_reset(&device_id).await.is_ok()
        && core.snapshot().state.device().map(|d| d.id.clone()).as_deref()
            == Some(device_id.as_str());

    core.record(
        LogLevel::Info,
        "smoke",
        format!("FW_SMOKE probe={probe_note} reconnect={reconnected}"),
    );
    std::process::exit(if reconnected { 0 } else { 1 });
}

/// 无人值守验收开关：`--smoke-scan` / `--smoke-firmware`，或
/// `<数据目录>/smoke` 文件里写 `scan` / `firmware`。
///
/// **为什么还要文件这一条**：`open ... --args --smoke-scan` 在本机试过三种写法，
/// 进程 argv 里都没有参数（见 `docs/GUI.md` §14.5）；直接跑 `Contents/MacOS/apm-gui`
/// 又会被 TCC 以"缺蓝牙用途声明"abort。哨兵文件是这台机器上唯一验证可行的入口。
/// 读到就删：一次性，不会下次启动又跑一遍。
fn smoke_mode() -> Option<String> {
    if let Some(mode) = std::env::args().find_map(|a| a.strip_prefix("--smoke-").map(str::to_string))
    {
        return Some(mode);
    }
    if let Ok(mode) = std::env::var("APM_GUI_SMOKE") {
        return Some(mode);
    }

    let path = apm_app::settings::data_dir().join("smoke");
    let mode = std::fs::read_to_string(&path).ok()?.trim().to_string();
    let _ = std::fs::remove_file(&path);
    (!mode.is_empty()).then_some(mode)
}

fn main() {
    let core = AppCore::new(None);

    tauri::Builder::default()
        .manage(GuiState {
            core: Arc::clone(&core),
        })
        .invoke_handler(tauri::generate_handler![
            commands::snapshot,
            commands::detect_adapter,
            commands::scan_start,
            commands::scan_stop,
            commands::connect,
            commands::set_auto_connect,
            commands::disconnect,
            commands::list_sinks,
            commands::set_sink,
            commands::set_save_wav,
            commands::set_buffer_target,
            commands::set_auto_stop_min,
            commands::start_stream,
            commands::stop_stream,
            commands::analyze_session,
            commands::analyze_session_json,
            commands::reveal_path,
            commands::keymap_view,
            commands::keymap_save,
            commands::keymap_import,
            commands::keymap_reset,
            commands::keymap_text,
            commands::inject_permission,
            commands::open_accessibility_settings,
            commands::firmware_status,
            commands::firmware_probe,
            commands::firmware_flash,
            commands::diagnostics_text,
            commands::log_path,
            commands::reveal_log,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();

            // 事件桥：内核广播 → 前端。前端消费不过来时丢最旧事件并记一条 warn，
            // 不允许反压到蓝牙线程。
            let event_core = Arc::clone(&core);
            let mut rx = event_core.subscribe();
            tauri::async_runtime::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(event) => {
                            let _ = handle.emit("app-event", &event);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            event_core.record(
                                LogLevel::Warn,
                                "app",
                                format!("前端消费滞后，丢弃 {skipped} 个事件"),
                            );
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });

            // 启动即检测适配器：失败会落到 no_adapter 并给出原因，不静默。
            //
            // `APM_GUI_SMOKE=scan` 或 `--smoke-scan` 是无人值守验收开关：走**同一套**
            // 检测 + 扫描代码路径，把结果写进诊断日志后退出。G0 的"扫到设备"就是靠它
            // 拿到可复现的证据，而不是靠人眼看界面。
            //
            // 用命令行参数而不是只认环境变量，是因为 bundle 必须由 LaunchServices
            // （`open -a ... --args`）启动才会以 App 自己的身份申请蓝牙权限；
            // 而 `open` 不方便传环境变量。
            let boot_core = Arc::clone(&core);
            let smoke = smoke_mode();
            let smoke_scan = smoke.as_deref() == Some("scan");
            let smoke_firmware = smoke.as_deref() == Some("firmware");
            tauri::async_runtime::spawn(async move {
                if smoke_scan {
                    run_smoke(boot_core).await;
                    return;
                }
                if smoke_firmware {
                    run_firmware_smoke(boot_core).await;
                    return;
                }
                // 适配器就绪后自动连接上次的设备。失败（设备没上电、不在广播）
                // 只落 warn 日志并回到 idle，不弹错误横幅——设备端是常态。
                if boot_core.detect_adapter().await.is_ok() {
                    let _ = boot_core.auto_connect().await;
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("启动失败：检查 Info.plist 与 tauri.conf.json");
}
