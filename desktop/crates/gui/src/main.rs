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
            commands::disconnect,
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
            let smoke = std::env::args().any(|a| a == "--smoke-scan")
                || std::env::var("APM_GUI_SMOKE").as_deref() == Ok("scan");
            tauri::async_runtime::spawn(async move {
                if smoke {
                    run_smoke(boot_core).await;
                    return;
                }
                let _ = boot_core.detect_adapter().await;
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("启动失败：检查 Info.plist 与 tauri.conf.json");
}
