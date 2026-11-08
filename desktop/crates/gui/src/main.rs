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

/// 无人值守**性能**基准：连上设备 → 开流 → 保持 N 秒 → 退出。
///
/// 为什么需要单独一个入口（2026-11-07）：G2 的 CPU/内存/帧率三项要在**推流稳态**
/// 下测才有效，而 `scan` 入口扫完就退、界面又必须人点「开始」。
/// 结果就是 §14.10 那种"人盯活动监视器抄数字"的测法——不可复现，也没法进 CI。
///
/// 这个入口让 `scripts/test-gui-perf.sh` 能确定性地把应用放进稳态：
/// 电平表在画、音频在写声卡、5 Hz 遥测在跑，与用户实际使用时的负载一致。
///
/// 只读设备，不改任何状态；结束时不动设备（STOP 由会话收尾负责）。
async fn run_perf(core: Arc<AppCore>, seconds: u64) {
    let fail = |reason: &str| -> ! {
        core.record(LogLevel::Error, "perf", format!("PERF_EXIT 1 reason={reason}"));
        std::process::exit(1);
    };

    if core.detect_adapter().await.is_err() {
        fail("adapter=unavailable");
    }

    // 自动连接优先；没记录就扫一遍再连。
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
    if core.snapshot().state.device().is_none() {
        fail("no_device");
    }
    core.record(LogLevel::Info, "perf", "PERF_READY connected");

    // 声卡：用配置里的；没配就挑第一个原生支持 16 kHz 的（优先 BlackHole）。
    if core.snapshot().settings.sink.is_none() {
        let Ok(sinks) = core.list_sinks() else {
            fail("sink_enum_failed");
        };
        let pick = sinks
            .iter()
            .find(|s| s.name.to_lowercase().contains("blackhole") && s.supports_16k)
            .or_else(|| sinks.iter().find(|s| s.supports_16k));
        match pick {
            Some(s) => {
                let _ = core.set_sink(s.name.clone());
            }
            None => fail("no_16k_sink"),
        }
    }

    match core.start_stream(false).await {
        Ok(id) => core.record(
            LogLevel::Info,
            "perf",
            format!("PERF_STREAMING session={id} seconds={seconds}"),
        ),
        Err(e) => fail(&format!("start_stream={:?}", e.code)),
    }

    // 保持稳态。这段时间里外部脚本在采 CPU/内存。
    tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;

    let summary = core.stop_stream().await;
    match summary {
        Ok(s) => core.record(
            LogLevel::Info,
            "perf",
            format!(
                "PERF_RESULT blocks={} underruns={} silent={} link_slow={}",
                s.blocks, s.underruns, s.silent_frames, s.link_slow
            ),
        ),
        Err(e) => core.record(
            LogLevel::Warn,
            "perf",
            format!("PERF_STOP_ERR {:?}", e.code),
        ),
    }
    core.record(LogLevel::Info, "perf", "PERF_EXIT 0");
    std::process::exit(0);
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

/// 无人值守验收入口：读 `<数据目录>/smoke`，内容为 `scan` / `firmware` / `perf:<秒>`。
///
/// **只保留哨兵文件这一条路**，因为另外两条在本机都被实测证伪（`docs/GUI_IMPLEMENTATION_LOG.md` §14.5）：
/// - 命令行 `--smoke-*`：经 LaunchServices（`open --args`）启动时 argv 里根本没有参数，
///   三种写法都试过；双击启动同理；
/// - 环境变量 `APM_GUI_SMOKE`：`open` 不继承 shell 环境，`launchctl setenv` 被系统拒绝；
/// - 直接跑 `Contents/MacOS/apm-gui --smoke-scan`：被 TCC 以"缺蓝牙用途声明" abort。
///
/// 留着前两条只是把"试过不行"记在代码里，实际永远走不到。真要再加一条路，先有证据再说。
///
/// 读到就删：一次性，不会下次启动又跑一遍。
fn smoke_mode() -> Option<String> {
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
            commands::check_sink,
            commands::open_sound_settings,
            commands::open_virtual_audio_download,
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
            commands::report_fps,
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
            // `<数据目录>/smoke` 里写 `scan` 是无人值守验收开关：走**同一套**检测 + 扫描
            // 代码路径，把结果写进诊断日志后退出。G0 的"扫到设备"靠它拿到可复现的证据，
            // 而不是靠人眼看界面。为什么用哨兵文件而不是命令行参数/环境变量：
            // 见 `smoke_mode()` 的注释（三条路里另外两条都被实测证伪）。
            let boot_core = Arc::clone(&core);
            let smoke = smoke_mode();
            let smoke_scan = smoke.as_deref() == Some("scan");
            let smoke_firmware = smoke.as_deref() == Some("firmware");
            // 写 `perf:<秒>`：连上设备、开流、保持 N 秒，供外部脚本采 CPU/内存。
            // 见 `scripts/test-gui-perf.sh`。
            let smoke_perf = smoke
                .as_deref()
                .and_then(|m| m.strip_prefix("perf"))
                .map(|rest| {
                    rest.trim_start_matches(':')
                        .trim()
                        .parse::<u64>()
                        .unwrap_or(60)
                });
            tauri::async_runtime::spawn(async move {
                if smoke_scan {
                    run_smoke(boot_core).await;
                    return;
                }
                if smoke_firmware {
                    run_firmware_smoke(boot_core).await;
                    return;
                }
                if let Some(seconds) = smoke_perf {
                    run_perf(boot_core, seconds).await;
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
