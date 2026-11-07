//! 子命令实现。原则：实测到的写数字，没测的写 null，不猜。
//!
//! 音频编排**不在这里**：采集、解码、丢块统计、声卡写入、指标聚合全在
//! `apm_app::Pipeline`（见 `docs/GUI.md` §3 的决定 D1）。本文件只做参数解析、
//! 循环驱动与打印。GUI 走的是同一条管线 —— 两边口径不会漂。

use anyhow::{anyhow, bail, Context, Result};
use apm_app::report;
use apm_app::{Pipeline, SessionOptions, SessionSummary, Telemetry};
use apm_link::Notification;
use apm_protocol::frame;
use apm_protocol::AUDIO_SAMPLE_RATE;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn session_id(prefix: &str) -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{prefix}-{ms}")
}

// ---------------------------------------------------------------------------
// analyze
// ---------------------------------------------------------------------------
pub fn analyze(path: &Path, as_json: bool) -> Result<()> {
    // 报告构造在 `apm_app::report` 里，与 GUI 的音质报告页共用同一份实现。
    let report = report::analyze_file(path).map_err(|e| anyhow!(e))?;

    if as_json {
        println!("{}", serde_json::to_string_pretty(&report.to_json())?);
        return Ok(());
    }

    println!("文件: {}", report.recording);
    println!(
        "格式: {} Hz / {} ch / {:.2} s（{} 采样）{}",
        report.sample_rate,
        report.channels,
        report.duration_s,
        report.samples,
        if report.downmixed {
            "，已下混为单声道"
        } else {
            ""
        }
    );
    if let Some(note) = report::rate_note(report.sample_rate) {
        println!("注意: {note}");
    }
    println!();
    println!("{}{}阈值", pad("指标", 14), pad("实测", 18));
    for check in &report.checks {
        let mark = if check.ok { "OK  " } else { "FAIL" };
        println!(
            "{}{}{} {}",
            pad(&check.name, 14),
            pad(&check.actual, 18),
            mark,
            check.expected
        );
    }
    if report.glitch_events > 0 {
        println!(
            "毛刺: {} 段满量程变号噪声，合计 {:.0} ms（采集端瞬态，非编解码问题）",
            report.glitch_events, report.glitch_ms
        );
    } else {
        println!("毛刺: 未检测到满量程变号噪声");
    }
    println!();
    println!("结论: {}", report.headline());
    println!("这只说明文件本身没有明显削波/直流偏置/电平异常；");
    println!("真机链路、虚拟声卡与会议软件端到端是否通过，必须另外测。");
    Ok(())
}

/// 终端列宽：CJK 字符占两格，直接 `{:<12}` 会让表格歪掉。
fn display_width(text: &str) -> usize {
    text.chars()
        .map(|c| match c as u32 {
            0x1100..=0x115F
            | 0x2E80..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE6F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6 => 2,
            _ => 1,
        })
        .sum()
}

fn pad(text: &str, width: usize) -> String {
    let w = display_width(text);
    format!("{text}{}", " ".repeat(width.saturating_sub(w)))
}

// ---------------------------------------------------------------------------
// selftest
// ---------------------------------------------------------------------------
fn find_vectors(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if !p.is_file() {
            bail!("指定的向量文件不存在: {}", p.display());
        }
        return Ok(p.to_path_buf());
    }

    let mut candidates = Vec::new();
    if let Ok(p) = std::env::var("APM_VECTORS") {
        candidates.push(PathBuf::from(p));
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("config/protocol-vectors.json"));
        candidates.push(cwd.join("../config/protocol-vectors.json"));
        candidates.push(cwd.join("../../config/protocol-vectors.json"));
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(Path::to_path_buf);
        while let Some(d) = dir {
            candidates.push(d.join("config/protocol-vectors.json"));
            dir = d.parent().map(Path::to_path_buf);
        }
    }

    candidates
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| anyhow!("找不到 config/protocol-vectors.json，用 --vectors 指定，或先跑 ./scripts/gen-protocol-vectors.sh"))
}

fn hex_decode(hex: &str) -> Vec<u8> {
    (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0))
        .collect()
}

pub fn selftest(explicit: Option<&Path>) -> Result<()> {
    let path = find_vectors(explicit)?;
    let text = std::fs::read_to_string(&path).with_context(|| format!("读 {}", path.display()))?;
    let v: Value = serde_json::from_str(&text).context("向量 JSON 解析失败")?;

    if v["schema"] != "ai-passport-mic-protocol-vectors-1" {
        bail!("向量文件 schema 不对: {}", v["schema"]);
    }

    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut check = |ok: bool, what: &str| {
        if ok {
            passed += 1;
            println!("  ok   {what}");
        } else {
            failed += 1;
            println!("  FAIL {what}");
        }
    };

    println!("向量文件: {}", path.display());

    println!("[crc]");
    for case in v["crc"].as_array().context("crc 不是数组")? {
        let data = case["data"].as_str().unwrap_or("");
        let want = case["crc"].as_u64().unwrap_or(0) as u16;
        let got = apm_protocol::crc::crc16_ccitt(data.as_bytes());
        check(got == want, &format!("CRC(\"{data}\") = 0x{got:04X}"));
    }

    println!("[frame]");
    for case in v["frames"].as_array().context("frames 不是数组")? {
        let name = case["name"].as_str().unwrap_or("?");
        let msg = case["msg"].as_u64().unwrap_or(0) as u8;
        let payload = hex_decode(case["payload_hex"].as_str().unwrap_or(""));
        let want = hex_decode(case["frame_hex"].as_str().unwrap_or(""));
        let got = frame::encode(msg, &payload).unwrap_or_default();
        check(got == want, &format!("{name} 帧编码与 C 一致"));

        let decoded = frame::decode(&want);
        let ok = matches!(&decoded, Ok((f, used)) if *used == want.len() && f.payload == payload.as_slice());
        check(ok, &format!("{name} 帧解码往返"));
    }

    println!("[adpcm]");
    let mut state = apm_protocol::adpcm::AdpcmState::default();
    for case in v["adpcm"].as_array().context("adpcm 不是数组")? {
        let index = case["index"].as_u64().unwrap_or(0);
        let pcm: Vec<i16> = case["pcm"]
            .as_array()
            .map(|a| a.iter().map(|x| x.as_i64().unwrap_or(0) as i16).collect())
            .unwrap_or_default();
        let want_block = hex_decode(case["block_hex"].as_str().unwrap_or(""));
        let got_block = apm_protocol::adpcm::encode_block(&mut state, &pcm);
        check(
            got_block == want_block,
            &format!("块 {index} 编码与 C 逐字节一致"),
        );

        let want_decoded: Vec<i16> = case["decoded"]
            .as_array()
            .map(|a| a.iter().map(|x| x.as_i64().unwrap_or(0) as i16).collect())
            .unwrap_or_default();
        let got_decoded =
            apm_protocol::adpcm::decode_block(&want_block, apm_protocol::AUDIO_BLOCK_SAMPLES);
        check(
            got_decoded == want_decoded,
            &format!("块 {index} 解码与 C 一致"),
        );
    }

    println!();
    println!("{passed} passed, {failed} failed");
    if failed > 0 {
        bail!("协议自检失败");
    }
    println!("协议层与固件 C 实现一致（{} 项）", passed);
    Ok(())
}

// ---------------------------------------------------------------------------
// firmware：列串口 / 读版本 / 烧写
//
// 逻辑一条都不在这里——全在 `apm_app::firmware`。CLI 只负责选口、选镜像、打印。
// 界面上点「检测」走的也是同一条路径，所以这里的报错就是用户看到的报错。
// ---------------------------------------------------------------------------

/// 默认随包镜像：仓库里的构建产物。安装包里的是 `Resources/firmware/`，那是 GUI 的事。
fn default_image_path() -> Result<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../device/build/ai-passport-mic-merged.bin");
    if p.is_file() {
        return Ok(p);
    }
    Err(anyhow!(
        "找不到合并镜像 {}，先跑 ./scripts/build-firmware.sh，或用 --image 指定",
        p.display()
    ))
}

/// 选串口：显式给了就用；没给就要求候选唯一，避免"猜一个口把别人的板子写了"。
fn pick_port(explicit: Option<&str>) -> Result<String> {
    let ports = apm_app::firmware::list_ports();
    if let Some(p) = explicit {
        return Ok(p.to_string());
    }
    match ports.len() {
        0 => Err(anyhow!(
            "没找到 VID:PID = {:04X}:{:04X} 的串口。用数据线直连电脑（不经无源 HUB），或先跑 `apmctl firmware ports` 看看",
            apm_app::firmware::TARGET_VID,
            apm_app::firmware::TARGET_PID
        )),
        1 => Ok(ports[0].path.clone()),
        _ => {
            let list: Vec<String> = ports.iter().map(|p| p.path.clone()).collect();
            Err(anyhow!(
                "有 {} 个候选串口 {}，用 --port 指定其中一个",
                ports.len(),
                list.join(", ")
            ))
        }
    }
}

pub fn firmware_ports() -> Result<()> {
    let ports = apm_app::firmware::list_ports();
    if ports.is_empty() {
        println!(
            "没有 VID:PID = {:04X}:{:04X} 的串口。",
            apm_app::firmware::TARGET_VID,
            apm_app::firmware::TARGET_PID
        );
        println!("设备关机时插 USB 只会给充电 IC 上电，C3 不枚举——先按电源键 0.5 s 开机。");
        return Ok(());
    }
    println!("{} 个候选串口（只读操作，没碰设备）：", ports.len());
    for p in &ports {
        println!(
            "  {}\tserial={}",
            p.path,
            p.serial_number.as_deref().unwrap_or("—")
        );
    }
    Ok(())
}

pub fn firmware_probe(port: Option<&str>, image: Option<&Path>) -> Result<()> {
    let port = pick_port(port)?;
    let image_path = match image {
        Some(p) => p.to_path_buf(),
        None => default_image_path()?,
    };
    let image = apm_app::firmware::load_image(&image_path)?;

    println!("串口: {port}");
    println!(
        "镜像: {}（版本 {}，{} 字节）",
        image_path.display(),
        image.info.version,
        image.info.bytes
    );
    println!("检测会让设备复位进下载模式，正在推流会断……");

    let r = apm_app::firmware::probe(&port, &image)?;
    println!("芯片: {}", r.chip);
    println!(
        "设备版本: {}",
        r.current_version.as_deref().unwrap_or("（读不到 / 空片）")
    );
    println!("镜像版本: {}", r.image_version);
    println!(
        "状态: {:?}{}",
        r.state,
        if r.first_flash { "（首烧）" } else { "" }
    );
    Ok(())
}

pub fn firmware_flash(port: Option<&str>, image: Option<&Path>) -> Result<()> {
    let port = pick_port(port)?;
    let image_path = match image {
        Some(p) => p.to_path_buf(),
        None => default_image_path()?,
    };
    let image = apm_app::firmware::load_image(&image_path)?;

    println!("串口: {port}");
    println!(
        "镜像: {}（版本 {}）",
        image_path.display(),
        image.info.version
    );
    println!("烧写会让设备复位；只写 bootloader / 分区表 / app，不动 NVS。");

    let mut last = 255u8;
    let report = apm_app::firmware::flash(&port, &image, |pct| {
        if pct != last {
            last = pct;
            print!("\r进度 {pct:3}%");
            use std::io::Write;
            std::io::stdout().flush().ok();
        }
    })?;
    println!();
    println!(
        "完成：{} → {}，{} 字节，{:.1} s，校验 {}",
        report.from_version.as_deref().unwrap_or("空片"),
        report.to_version,
        report.bytes_written,
        report.elapsed_ms as f64 / 1000.0,
        if report.verified {
            "通过"
        } else {
            "未通过"
        }
    );
    println!("app 分区 MD5 {}", report.md5);
    Ok(())
}

// ---------------------------------------------------------------------------
// scan / status / record
// ---------------------------------------------------------------------------
pub async fn scan(timeout_s: u64) -> Result<()> {
    println!("扫描 {} 秒（蓝牙 LE）...", timeout_s);
    let devices = apm_link::scan(Duration::from_secs(timeout_s))
        .await
        .context("扫描失败")?;

    if devices.is_empty() {
        println!("没发现设备。确认设备已上电且在广播；本项目的设备名是 \"AI Passport Mic\"。");
        return Ok(());
    }

    println!();
    println!(
        "{}{}{}本项目服务",
        pad("ID", 38),
        pad("名称", 20),
        pad("RSSI", 10)
    );
    for d in devices {
        println!(
            "{}{}{}{}",
            pad(&d.id, 38),
            pad(
                if d.name.is_empty() {
                    "(无名)"
                } else {
                    &d.name
                },
                20
            ),
            pad(
                &d.rssi
                    .map(|r| format!("{r} dBm"))
                    .unwrap_or_else(|| "-".into()),
                10
            ),
            if d.has_service { "yes" } else { "no" }
        );
    }
    Ok(())
}

pub async fn status(device: Option<&str>, timeout_s: u64) -> Result<()> {
    let link = apm_link::connect(device, Duration::from_secs(15))
        .await
        .map_err(|e| anyhow!("{e}\n提示: {}", apm_link::device::hint_for_error(&e)))?;

    println!("已连接。观察 {} 秒（Ctrl-C 退出）。", timeout_s);
    match link.read_status().await {
        Ok(s) => println!(
            "当前: state={} flags=0x{:02X} dropped={} sent={}",
            frame::state::name(s.state),
            s.flags,
            s.dropped,
            s.sent
        ),
        Err(e) => println!("读 STATUS 失败: {e}"),
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_s);
    let mut last: Option<(u8, u16, u32)> = None;

    while tokio::time::Instant::now() < deadline {
        match link.next_notification(Duration::from_millis(500)).await {
            Ok(Some(Notification::Status(s))) => {
                let snapshot = (s.state, s.dropped, s.sent);
                if last == Some(snapshot) {
                    continue; // 心跳没变化就不刷屏
                }
                last = Some(snapshot);
                println!(
                    "state={:<9} ble={} audio={} ptt={} err={}  dropped={:<5} sent={}",
                    frame::state::name(s.state),
                    yes_no(s.ble_connected()),
                    yes_no(s.audio_subscribed()),
                    yes_no(s.ptt_held()),
                    yes_no(s.audio_error()),
                    s.dropped,
                    s.sent
                );
            }
            Ok(Some(Notification::Event(ev))) => {
                println!(
                    "按键: {}.{}（flags=0x{:02X}）",
                    ev.button_name(),
                    ev.event_name(),
                    ev.flags
                );
            }
            Ok(Some(Notification::Audio(block))) => {
                println!("收到音频块 {} 字节（未订阅录音，忽略）", block.data.len());
            }
            Ok(Some(Notification::Pong(seq))) => {
                println!("PONG seq={seq}（本命令不测延迟，用 `apmctl ping`）");
            }
            Ok(Some(Notification::Battery(pct))) => {
                println!("电量 {pct}%");
            }
            Ok(Some(Notification::Malformed { uuid, reason })) => {
                // 坏帧不是"安静"：设备发了东西，链路是通的。如实打出来，
                // 别让它看起来像"什么都没收到"（B6）。
                println!("无法解析的通知（{uuid}）：{reason}");
            }
            Ok(None) => {}
            Err(e) => {
                println!("通知流出错: {e}");
                break;
            }
        }
    }

    link.disconnect().await.ok();
    Ok(())
}

/// 读一次电量。连接后主动读（固件只在 SOC 变化时通知，光等通知拿不到初值）。
pub async fn battery(device: Option<&str>) -> Result<()> {
    let link = apm_link::connect(device, Duration::from_secs(15))
        .await
        .map_err(|e| anyhow!("{e}\n提示: {}", apm_link::device::hint_for_error(&e)))?;

    match link.read_battery().await {
        Ok(Some(pct)) => println!("电量 {pct}%"),
        Ok(None) => {
            println!("设备没有 Battery Service（旧固件），电量不可用");
        }
        Err(e) => println!("读电量失败: {e:#}"),
    }

    link.disconnect().await.ok();
    Ok(())
}
///
/// 原因见 `docs/KNOWN_ISSUES.md` **KI-005 的第二种触发方式**：设备在上一条会话异常结束、
/// 或者"连上之后迟迟没人发 START"时会停在半死态，此时直接 START 只恢复一部分
/// （实测块率 28/s、设备侧丢弃 3992）。
///
/// CLI 是连上立刻 START，本来踩不到这个坑；这里补上是为了跟 `apm-app` 的
/// `start_stream` 保持同一套语义，别让两条路径各长一份行为。
async fn reset_capture_state(link: &apm_link::Link) {
    // STOP 对没在采集的设备是幂等的，失败也不阻塞。
    link.stop_capture().await.ok();
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
}

fn yes_no(v: bool) -> &'static str {
    if v {
        "是"
    } else {
        "否"
    }
}

/// 录音到 WAV。采集/解码/丢块/毛刺抑制全在 `apm_app::Pipeline`，
/// 这里只负责驱动通知循环与打印。
pub async fn record(path: &Path, seconds: u64, device: Option<&str>) -> Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).ok();
        }
    }

    let mut pipeline = Pipeline::start(SessionOptions {
        session_id: session_id("cli-record"),
        sink: None,
        target_ms: apm_app::settings::DEFAULT_TARGET_MS,
        wav_path: Some(path.to_path_buf()),
    })
    .map_err(|e| anyhow!("创建会话失败：{e}"))?;

    let link = apm_link::connect(device, Duration::from_secs(15))
        .await
        .map_err(|e| anyhow!("{e}\n提示: {}", apm_link::device::hint_for_error(&e)))?;

    println!("已连接，请求开始采集（{} 秒）...", seconds);
    reset_capture_state(&link).await;
    link.start_capture().await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    while tokio::time::Instant::now() < deadline {
        match link.next_notification(Duration::from_millis(500)).await {
            Ok(Some(notification)) => {
                pipeline.on_notification(&notification);
            }
            Ok(None) => {}
            Err(e) => {
                println!("通知流出错: {e}");
                break;
            }
        }
    }

    // 先停采集再收尾会话：不然收尾期间还在往文件里塞数据。
    link.stop_capture().await.ok();
    link.disconnect().await.ok();
    let summary = pipeline.finish();

    if summary.blocks == 0 {
        bail!("一个音频块都没收到。检查设备是否真的在采集（OK 键是否被按住过、STATUS 是否 CAPTURING）");
    }

    let report = report::analyze_file(path).map_err(|e| anyhow!(e))?;
    println!();
    println!("已写入 {}", path.display());
    println!(
        "块: {} 个（{:.2} s），序号跳变丢失: {}（跳变 {} 次），长度异常: {}",
        summary.blocks,
        summary.duration_s,
        summary.lost_blocks,
        summary.seq_jumps,
        summary.bad_len_blocks
    );
    if summary.device_dropped > 0 {
        println!(
            "设备侧丢弃: {} 块（发送阶段丢的，序号没发出来，PC 端统计不到）",
            summary.device_dropped
        );
    }
    // 慢档会话的数据不能当音质结论用（KI-005 / KI-006 的口径陷阱）。
    // GUI 侧的 `analyze_session` 已经硬拒，CLI 这里至少要说出来，
    // 否则"录了一份能分析的 WAV"会被当成可用基线。
    if summary.link_slow {
        println!(
            "⚠ 本场设备报过 LINK_SLOW（连接间隔 > 40 ms）：欠载与丢块是链路假象，\
             这份 WAV **不能**进音质对比"
        );
    }
    if summary.glitch_events > 0 {
        println!(
            "采集端毛刺: {} 段，已静音 ≈{:.0} ms",
            summary.glitch_events, summary.glitch_muted_ms
        );
    }
    println!(
        "RMS: {}  峰值: {}  削波: {:.3}%  直流: {:.3}%",
        report
            .rms_dbfs
            .map(|v| format!("{v:.2} dBFS"))
            .unwrap_or_else(|| "静音".into()),
        report
            .peak_dbfs
            .map(|v| format!("{v:.2} dBFS"))
            .unwrap_or_else(|| "静音".into()),
        report.clipping_pct,
        report.dc_offset_pct
    );
    println!("下一步: apmctl analyze {} --json", path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// sink
// ---------------------------------------------------------------------------

/// 把实时音频写进系统虚拟声卡。
///
/// 与 `record` 的区别：不落盘，直接进声卡，所以能真正被会议软件选中。
/// 两者共用同一条 `Pipeline`：丢块、长度异常、欠载、漂移都按同一口径计数。
pub async fn sink(
    to: &str,
    seconds: u64,
    target_ms: u32,
    verify: bool,
    tone: bool,
    rtt: bool,
    device: Option<&str>,
) -> Result<()> {
    if tone {
        // 只验声卡这一段，不碰 BLE：先把权限/采样率/直通从链路里剥离出来。
        let mut out =
            apm_sink::Sink::open(to, AUDIO_SAMPLE_RATE, target_ms).context("打开虚拟声卡失败")?;
        return sink_tone(&mut out, seconds, verify);
    }

    let mut pipeline = Pipeline::start(SessionOptions {
        session_id: session_id("cli-sink"),
        sink: Some(to.to_string()),
        target_ms,
        wav_path: None,
    })
    .map_err(|e| anyhow!("{}", e.detail))?;
    println!(
        "输出: {}（{} Hz / {} ch），抖动缓冲目标 {} ms",
        pipeline.sink_device().unwrap_or(to),
        pipeline.rate(),
        pipeline.channels(),
        target_ms
    );

    let link = apm_link::connect(device, Duration::from_secs(15))
        .await
        .map_err(|e| anyhow!("{e}\n提示: {}", apm_link::device::hint_for_error(&e)))?;
    println!(
        "已连接设备，开始采集（{} 秒）。会议软件里把麦克风选成该设备即可。",
        seconds
    );
    reset_capture_state(&link).await;
    link.start_capture().await?;

    // 回环探针放在采集开始之后起，避免把连接期的静音算进 RMS。
    let probe = if verify {
        match apm_sink::Probe::open(to, AUDIO_SAMPLE_RATE) {
            Ok(p) => {
                println!("回环探针已开启：从 {} 的输入端回读电平", to);
                Some(p)
            }
            Err(e) => {
                println!("回环探针开启失败（不影响输出）: {e}");
                None
            }
        }
    } else {
        None
    };

    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let mut next_report = tokio::time::Instant::now() + Duration::from_secs(1);
    let mut elapsed_s = 0u64;

    // `--rtt`：在同一条连接上定期发 PING，用来把"停顿"归因到链路还是音频通知队列。
    //
    // 为什么不直接复用 `link.ping()`：那个函数会**自己消费通知流**（它内部循环
    // next_notification 直到拿到匹配的 PONG），并行跑就会把音频块吃掉。
    // 设备只支持 1 条 BLE 连接（`CONFIG_BT_NIMBLE_MAX_CONNECTIONS=1`），
    // 也开不了第二个进程。所以只能在这里自己发、由本循环统一收 PONG。
    let mut next_ping = tokio::time::Instant::now();
    let mut ping_seq = 0u32;
    // 在途 PING：(seq, 发出时刻, 相对会话起点的 ms)
    let mut inflight: Option<(u32, std::time::Instant, f64)> = None;
    let session_start = std::time::Instant::now();
    let mut rtt_timeouts = 0u32;
    if rtt {
        println!("链路 RTT 采样已开启（每 500 ms 一次）：用来判断停顿归链路还是归音频通知队列");
    }

    while tokio::time::Instant::now() < deadline {
        // 到点就发一个新 PING（同一时刻只保留一个在途，避免自己制造拥塞）。
        if rtt && inflight.is_none() && tokio::time::Instant::now() >= next_ping {
            ping_seq = ping_seq.wrapping_add(1).max(1);
            match link.send_ctrl(apm_protocol::frame::msg::PING, &ping_seq.to_le_bytes()).await {
                Ok(()) => {
                    let at = session_start.elapsed().as_secs_f64() * 1000.0;
                    inflight = Some((ping_seq, std::time::Instant::now(), at));
                }
                Err(e) => println!("PING 发送失败（继续录音）：{e}"),
            }
            next_ping += Duration::from_millis(500);
        }

        match link.next_notification(Duration::from_millis(500)).await {
            Ok(Some(notification)) => {
                // PONG 只喂给 RTT 记录器，不进 Pipeline（它不是音频，也不该影响统计）。
                match &notification {
                    Notification::Pong(got) => {
                        if let Some((seq, sent_at, at_ms)) = inflight {
                            if *got == seq {
                                pipeline.note_rtt(
                                    session_start + Duration::from_millis(at_ms as u64),
                                    sent_at.elapsed(),
                                );
                                inflight = None;
                            }
                        }
                    }
                    _ => {
                        pipeline.on_notification(&notification);
                    }
                }
            }
            Ok(None) => {
                // 在途 PING 超时：记一次但不清空 inflight（等下一个 500 ms 窗口重发）。
                if let Some((_, sent_at, _)) = inflight {
                    if sent_at.elapsed() > Duration::from_millis(1500) {
                        rtt_timeouts += 1;
                        inflight = None;
                    }
                }
            }
            Err(e) => {
                println!("通知流出错: {e}");
                break;
            }
        }

        if tokio::time::Instant::now() >= next_report {
            next_report += Duration::from_secs(1);
            elapsed_s += 1;
            let t: Telemetry = pipeline.telemetry();
            println!(
                "  [{:>3}s] 块={} 缓冲={:.0} ms 欠载={} 静音帧={} 溢出={} 漂移={:+.0} ppm",
                elapsed_s,
                t.blocks,
                t.buffer_ms,
                t.underruns,
                t.silent_frames,
                t.overflow_drops,
                t.ppm
            );
        }
    }

    // 先停写再断链：STOP 有一次 BLE 往返，期间声卡还在消费缓冲，
    // 拖到最后才 clear 会白掉一段静音（实测尾巴上多出 3 次欠载）。
    let written = pipeline.written_rms();
    // 这几样都要在 `finish()` 之前取：它消费掉 pipeline。
    let (arrival_hist, arrival_max_ms, late_arrivals) = {
        let (h, m, l) = pipeline.arrival_stats();
        (*h, m, l)
    };
    let (process_hist, process_max_ms, slow_process) = {
        let (h, m, s) = pipeline.process_stats();
        (*h, m, s)
    };
    let (read_hist, read_max_ms, slow_reads) = {
        let (h, m, s) = pipeline.read_wait_stats();
        (*h, m, s)
    };
    let gap_read_wait = pipeline.gap_read_wait_ms();
    let rtt_count = pipeline.rtt_samples().len();
    let rtt_raw: Vec<(f64, f64)> = pipeline.rtt_samples().to_vec();
    let (fine_hist, fine_overflow, _) = {
        let (h, o, c) = pipeline.fine_gaps();
        (*h, o, c)
    };
    let summary = pipeline.finish();
    link.stop_capture().await.ok();
    link.disconnect().await.ok();

    print_summary(&summary);

    // 抖动诊断：**只给原始测量，不给判词**。
    //
    // 这里曾经有 `抖动归因` / `尖峰时间形状` / `停顿归属` 三条判词（clean / arrival-only /
    // periodic / audio-queue-only …），是 2026-11-07 查 KI-006 时加的。那套判词最终
    // 互相否决过（"尖峰看着有节律" vs "RTT 已证明链路通畅"），根因查明是 CoreBluetooth
    // 攒批交付之后就不再需要：交付模式 + 下面这几个直方图，看报告的人自己就能判。
    //
    // 三把尺子的含义（不要合并成一个数）：
    // - 链路到达间隔：链路层打点，不含我们的处理时间；
    // - PC 处理耗时：received_at → 解码完成，量我们自己花了多久；
    // - 读取等待：问下一条实际等了多久，能分开"数据晚到"与"我们问晚了"。
    let edges = apm_app::pipeline::ARRIVAL_BUCKETS_MS;
    let hist_line = |hist: &[u64; 8]| -> String {
        let mut line = String::new();
        for (i, &count) in hist.iter().enumerate() {
            if count == 0 {
                continue;
            }
            let lo = if i == 0 { 0.0 } else { edges[i - 1] };
            let hi = edges[i];
            if hi.is_infinite() {
                line.push_str(&format!(">{lo:.0}ms:{count}  "));
            } else {
                line.push_str(&format!("{lo:.0}-{hi:.0}ms:{count}  "));
            }
        }
        line
    };
    println!();
    if arrival_hist.iter().sum::<u64>() > 0 {
        println!(
            "链路到达间隔: 最大 {arrival_max_ms:.1} ms，超过 80 ms 缓冲上限 {late_arrivals} 次"
        );
        println!("  分布: {}", hist_line(&arrival_hist));
    }
    if process_hist.iter().sum::<u64>() > 0 {
        println!(
            "PC 处理耗时: 最大 {process_max_ms:.1} ms，超过 20 ms（一个块时长）{slow_process} 块"
        );
        println!("  分布: {}", hist_line(&process_hist));
    }
    if read_hist.iter().sum::<u64>() > 0 {
        println!(
            "读取等待: 最大 {read_max_ms:.1} ms，超过 80 ms {slow_reads} 次\
             （长 = 数据确实没到；短 = 是我们问晚了）"
        );
        println!("  分布: {}", hist_line(&read_hist));
    }
    // 把最长那次间隔拆成"等链路"与"我们自己"两段。这是唯一需要的算术：
    // 到达间隔 = 读取等待 + 我们自己花掉的。
    if summary.max_arrival_gap_ms > 0.0 {
        let local_ms = (summary.max_arrival_gap_ms - gap_read_wait).max(0.0);
        println!(
            "最长间隔拆解: {:.1} ms = 等链路 {:.1} ms + 我们自己 {:.1} ms",
            summary.max_arrival_gap_ms, gap_read_wait, local_ms
        );
    }

    // 交付模式：G2 判据（方案 D）的依据，必须留着。
    println!();
    println!(
        "通知交付模式: {}（相邻 <5 ms {:.0}%、15–25 ms {:.0}%）",
        summary.delivery_pattern,
        summary.delivery_clumped_ratio * 100.0,
        summary.delivery_even_ratio * 100.0
    );
    println!("  {}", delivery_explain(&summary.delivery_pattern));
    if fine_hist.iter().sum::<u64>() + fine_overflow > 0 {
        let mut line = String::new();
        let mut lo = 0.0;
        for (i, &c) in fine_hist.iter().enumerate() {
            if c == 0 {
                lo = apm_app::pipeline::FINE_BUCKETS_MS[i];
                continue;
            }
            line.push_str(&format!("{lo:.0}-{:.0}ms:{c}  ", apm_app::pipeline::FINE_BUCKETS_MS[i]));
            lo = apm_app::pipeline::FINE_BUCKETS_MS[i];
        }
        if fine_overflow > 0 {
            line.push_str(&format!(">40ms:{fine_overflow}  "));
        }
        println!("  细粒度分布: {line}");
    }
    if rtt_count > 0 {
        // RTT 只打原始统计，不打"停顿归属"的判词。
        //
        // 判词（link-wide / audio-queue-only）曾经用来自动否决"尖峰有节律 → 改连接参数"
        // 那条建议。现在建议本身没了，判词也就没有意义；而 RTT 与到达尖峰的对应关系
        // 人看一眼两张序列就能判断，不需要中间那层枚举。
        let raw: Vec<String> = rtt_raw
            .iter()
            .take(6)
            .map(|(at, rtt)| format!("{:.0}ms→{:.2}ms", at, rtt))
            .collect();
        println!("链路 RTT 原始采样（前 6 个）: {}", raw.join("  "));
        let mut sorted: Vec<f64> = rtt_raw.iter().map(|(_, r)| *r).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = sorted.len();
        let median = if n % 2 == 1 {
            sorted[n / 2]
        } else {
            (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
        };
        let p95 = sorted[((n as f64 * 0.95).ceil() as usize).saturating_sub(1).min(n - 1)];
        println!(
            "链路 RTT: {} 个采样，中位 {:.1} ms，p95 {:.1} ms{}",
            rtt_count,
            median,
            p95,
            if rtt_timeouts > 0 {
                format!("；另有 {rtt_timeouts} 次 PING 超时")
            } else {
                String::new()
            }
        );
    }

    // 欠载与到达间隔的关系：迟到次数 ≠ 欠载次数。存量接近目标时，80–100 ms 的间隔
    // 大多被存量吸收，只有更大的尖峰几乎必然欠载。
    if summary.underruns > 0 && late_arrivals > 0 {
        println!(
            "提示: {late_arrivals} 次到达间隔 > 80 ms（最大 {:.0} ms，其中 {gap_read_wait:.0} ms \
             在等链路）、欠载 {} 次",
            summary.max_arrival_gap_ms, summary.underruns
        );
    }

    if let Some(p) = probe {
        let looped = p.rms_dbfs();
        println!();
        println!(
            "回环: 输入端 {:.2} s，RMS {}，峰值 {}",
            p.seconds(),
            looped
                .map(|v| format!("{v:.2} dBFS"))
                .unwrap_or_else(|| "无样本".into()),
            p.peak_dbfs()
                .map(|v| format!("{v:.2} dBFS"))
                .unwrap_or_else(|| "-".into())
        );
        println!(
            "写入: RMS {}",
            written
                .map(|v| format!("{v:.2} dBFS"))
                .unwrap_or_else(|| "无样本".into())
        );
        match (written, looped) {
            (Some(w), Some(l)) => {
                let diff = (w - l).abs();
                if diff <= 3.0 {
                    println!("判定: 回环与写入电平差 {diff:.1} dB（≤3 dB）→ 音频确实进了声卡");
                } else {
                    println!(
                        "判定: 电平差 {diff:.1} dB，超过 3 dB —— 声卡可能没在直通，或采样率不一致"
                    );
                }
            }
            _ => println!("判定: 样本不足，无法比较（检查权限或把 --seconds 调大）"),
        }
    }
    println!("这只证明 PCM 已经推进声卡；会议软件里能不能选中它，要另外确认。");
    Ok(())
}

fn print_summary(summary: &SessionSummary) {
    println!();
    println!(
        "块: {} 个，序号跳变丢失: {}（跳变 {} 次），长度异常: {}，设备侧丢弃: {}",
        summary.blocks,
        summary.lost_blocks,
        summary.seq_jumps,
        summary.bad_len_blocks,
        summary.device_dropped
    );
    println!(
        "声卡: 已播 {:.2} s，欠载 {} 次（其中断供 {} 次；静音 {} 帧 ≈ {:.0} ms），超容量丢弃 {} 帧，停摆修剪 {} 帧",
        summary.duration_s,
        summary.underruns,
        summary.underruns_underfed,
        summary.silent_frames,
        summary.silent_frames as f64 * 1000.0 / AUDIO_SAMPLE_RATE as f64,
        summary.frames_dropped,
        summary.frames_trimmed
    );
    println!(
        "漂移: {:.0} ppm（墙钟法，与消费端无关；补偿补 {} / 丢 {} 样本）",
        summary.ppm, summary.frames_inserted, summary.frames_skipped
    );
    if summary.glitch_events > 0 {
        println!(
            "采集端毛刺: {} 段，已静音 ≈{:.0} ms —— 已压掉，会议里不会听到",
            summary.glitch_events, summary.glitch_muted_ms
        );
    }
    // **延迟用实测存量，不用目标值**（A4）。目标值只是用户意图，
    // 修剪守卫会把稳态存量留在"目标 + 余量"附近，拿目标当延迟会系统性低估。
    match (summary.buffer_p50_ms, summary.buffer_p95_ms) {
        (Some(p50), Some(p95)) => println!(
            "缓冲: 实测存量 p50 {p50:.0} ms / p95 {p95:.0} ms（用户基准 {} → {} ms，自适应最多 +{} ms）",
            summary.target_start_ms, summary.target_end_ms, summary.target_boost_max_ms
        ),
        _ => println!(
            "缓冲: 无实测存量（没开声卡或会话太短）；用户基准 {} → {} ms，自适应最多 +{} ms",
            summary.target_start_ms, summary.target_end_ms, summary.target_boost_max_ms
        ),
    }
    println!(
        "      目标 {} ms / 回调抢锁失败 {} 次 / 通道满丢块 {}",
        summary.target_end_ms, summary.lock_misses, summary.overflow_drops
    );
    if summary.underruns > 0 {
        let consumed_side = summary.underruns.saturating_sub(summary.underruns_underfed);
        // **按归因给建议**（A5）。以前无论哪种欠载都提示"把缓冲调大"，
        // 而链路断供型欠载调缓冲是无效的 —— 那会把真因掩盖成"调大了就好一点"。
        if consumed_side > 0 {
            if summary.target_end_ms < apm_sink::TARGET_MAX_MS {
                println!(
                    "提示: {consumed_side} 次欠载是**消费端**造成的（缓冲被读空而数据刚到）——\
                     把 --target-ms 往上调（上限 {}）再看。",
                    apm_sink::TARGET_MAX_MS
                );
            } else {
                println!(
                    "提示: {consumed_side} 次欠载是消费端造成，但目标已在上限 {} ms，没有余量了。",
                    apm_sink::TARGET_MAX_MS
                );
            }
        }
        if summary.underruns_underfed > 0 {
            println!(
                "提示: {} 次欠载是**上游断供**（距上次推入 > {} ms）——调缓冲目标无效，\
                 要查链路到达（见下方到达间隔统计）与 PC 端自身停顿。",
                summary.underruns_underfed,
                apm_sink::UNDERFED_MS
            );
        }
    }
}

/// 只验证"声卡这一段"：不连 BLE，直接推 440 Hz 正弦，再从输入端回读。
///
/// 存在的理由：先把权限、采样率、直通三件事从 BLE 链路里剥离出来，
/// 否则一旦回环是静音，分不清是蓝牙没声还是声卡没通。
fn sink_tone(out: &mut apm_sink::Sink, seconds: u64, verify: bool) -> Result<()> {
    use std::f64::consts::TAU;

    let rate = AUDIO_SAMPLE_RATE as f64;
    let block = apm_protocol::AUDIO_BLOCK_SAMPLES;
    let amp = 32768.0 * 10f64.powf(-20.0 / 20.0); // -20 dBFS

    let probe = if verify {
        match apm_sink::Probe::open(out.device_name(), AUDIO_SAMPLE_RATE) {
            Ok(p) => Some(p),
            Err(e) => {
                println!("回环探针开启失败: {e}");
                None
            }
        }
    } else {
        None
    };

    println!("推 440 Hz 正弦（-20 dBFS）{} 秒...", seconds);
    let mut phase = 0.0f64;
    let mut pushed = 0u64;

    // 按**绝对时刻**配速，不要用"每 5 块 sleep 100 ms"。
    //
    // 后者看着像实时（50 块/秒 = 320×50 = 16000 样本/秒），实际不是：一秒里睡满
    // 1000 ms 之后还要**再加**生成 50 块的计算时间，于是每秒都落后一点、越落越多。
    // 实测 180 s 的 `--tone`：推入 180.000 s、声卡取走 192.160 s、欠载 605 次
    // （补静音 12.100 s，正好等于落后量）。
    //
    // 这个函数的存在理由就是"把声卡从 BLE 链路里剥离出来单独验"，它要是自己喂不上，
    // 就会报出几百次假欠载，把排查方向指向声卡。必须按实时速率精确配速。
    let started = std::time::Instant::now();
    let blocks = seconds * 50;
    // 记录配速本身的抖动：如果"相邻两次推入"的间隔偶尔超过缓冲目标，
    // 欠载就是喂入端造成的，而不是声卡。tone 模式存在的意义就是分清这两件事。
    let mut max_gap = Duration::ZERO;
    let mut late_pushes = 0u64;
    let mut last_push = Duration::ZERO;
    for i in 0..blocks {
        let mut pcm = Vec::with_capacity(block);
        for _ in 0..block {
            pcm.push((amp * phase.sin()) as i16);
            phase = (phase + TAU * 440.0 / rate) % TAU;
        }
        out.push(&pcm);
        pushed += block as u64;

        // 第 i 块应当在 (i+1) × 20 ms 之前喂进去；落后就立刻补，不落后才睡。
        let deadline = Duration::from_micros((i + 1) * 20_000);
        let elapsed = started.elapsed();
        if deadline > elapsed {
            std::thread::sleep(deadline - elapsed);
        }
        let now = started.elapsed();
        let gap = now.saturating_sub(last_push);
        if gap > max_gap {
            max_gap = gap;
        }
        // 超过 40 ms（两个块周期）就算这次喂入迟到了
        if gap > Duration::from_millis(40) {
            late_pushes += 1;
        }
        last_push = now;
    }
    // 先量喂入耗时，**再**做末尾的收尾等待：后者不属于喂入配速，
    // 把它算进 wall 会让实时率看起来永远是 0.998×（300 ms / 180 s），
    // 从而把"配速没问题"误读成"发生器在拖后腿"。
    let wall = started.elapsed().as_secs_f64();

    // 关键：欠载统计要在**收尾等待之前**取。
    //
    // 喂入一停，缓冲里剩下的 80 ms 会被声卡按正常节奏抽干，抽干之后必然欠载——
    // 那是"我们不再喂了"，不是"声卡停摆"。2026-11-07 的 `--tone` 就栽在这上面：
    // 120 s 的跑法报 8 次欠载，欠载时刻是 120032/120064/…/120256 ms，
    // 间隔 32 ms、共 256 ms，正好等于这句 `sleep(300ms)` 的长度。
    let st = out.stats();
    std::thread::sleep(Duration::from_millis(300));
    out.clear();

    let st_drained = out.stats();
    println!(
        "写入 {} 帧（{:.2} s），声卡取走 {} 帧，运行期欠载 {} 次",
        pushed,
        pushed as f64 / rate,
        st.frames_consumed,
        st.underruns
    );
    if st_drained.underruns > st.underruns {
        println!(
            "收尾阶段另有 {} 次欠载（喂入一停缓冲必然被抽干，属正常收尾，不计入运行期）",
            st_drained.underruns - st.underruns
        );
    }
    // 喂入速率与实时是否吻合，直接决定欠载数字有没有意义。不报这个数，
    // 用户会把"发生器没喂够"当成"声卡不行"——2026-11-07 就踩过。
    let expected = pushed as f64 / rate;
    if wall > 0.0 {
        println!(
            "喂入速率: {:.2} s 的内容用了 {:.2} s 推完（实时率 {:.3}×；1.000× 才说明是实时喂入）",
            expected,
            wall,
            expected / wall
        );
        if expected / wall < 0.98 {
            println!(
                "警告: 喂入慢于实时 {:.1}%，本次的欠载**不能**当作声卡的结论",
                (1.0 - expected / wall) * 100.0
            );
        }
    }
    println!(
        "喂入间隔: 最大 {:.1} ms，超过 40 ms 的迟到 {late_pushes} 次（目标缓冲 {} ms）",
        max_gap.as_secs_f64() * 1000.0,
        out.target_ms()
    );

    // 声卡回调的间隔分布 —— KI-006 的下一步要的原始数据。
    // 没有这个直方图，"抬缓冲上限"只能靠猜。
    if st.callbacks > 0 {
        println!(
            "声卡回调: {} 次，最大间隔 {:.1} ms，明显迟到（>1.5× 周期）{} 次",
            st.callbacks, st.max_callback_gap_ms, st.stalled_callbacks
        );
        let edges = apm_sink::GAP_BUCKETS_MS;
        let mut line = String::new();
        for (i, &count) in st.gap_histogram.iter().enumerate() {
            if count == 0 {
                continue;
            }
            let lo = if i == 0 { 0.0 } else { edges[i - 1] };
            let hi = edges[i];
            if hi.is_infinite() {
                line.push_str(&format!(">{lo:.0}ms:{count}  "));
            } else {
                line.push_str(&format!("{lo:.0}-{hi:.0}ms:{count}  "));
            }
        }
        println!("回调间隔分布: {line}");
    }
    if st.underruns_logged > 0 {
        let mut times = String::new();
        for i in 0..st.underruns_logged {
            times.push_str(&format!("{:.0} ", st.underrun_at_ms[i]));
        }
        println!("欠载时刻(ms，相对首次回调): {times}");
    }
    if max_gap > Duration::from_millis(out.target_ms() as u64) {
        println!("结论: 有 {late_pushes} 次喂入间隔超过缓冲目标 —— 欠载由**喂入端**造成，不是声卡");
    } else if st.underruns > 0 {
        // 关键判据：欠载都发生在喂入结束之后，就是 `clear()` 收尾造成的，
        // 不是运行期的真欠载。2026-11-07 的 `--tone` 全靠这一条才没被误读。
        let tail = st.underrun_at_ms[0] >= expected * 1000.0 - 200.0;
        if tail {
            println!(
                "结论: {} 次欠载全部发生在**喂入结束后的收尾阶段**（{:.0} ms 之后），\
                 属于 clear() 造成的测量伪影，不记为运行期欠载",
                st.underruns, st.underrun_at_ms[0]
            );
        } else {
            println!(
                "结论: 喂入间隔始终小于缓冲目标，但仍有 {} 次欠载 —— 欠载由**声卡回调停摆**造成",
                st.underruns
            );
        }
    } else {
        println!("结论: 0 欠载，声卡这一段是干净的");
    }
    if let Some(p) = probe {
        println!(
            "回环: 输入端 {:.2} s，RMS {}，峰值 {}",
            p.seconds(),
            p.rms_dbfs()
                .map(|v| format!("{v:.2} dBFS"))
                .unwrap_or_else(|| "无样本".into()),
            p.peak_dbfs()
                .map(|v| format!("{v:.2} dBFS"))
                .unwrap_or_else(|| "-".into())
        );
        match p.rms_dbfs() {
            Some(v) if v > -60.0 => println!("判定: 声卡直通正常（能读到 -20 dBFS 附近的信号）"),
            Some(v) => println!("判定: 回环几乎静音（{v:.2} dBFS）—— 权限被拒或声卡没直通"),
            None => println!("判定: 输入端没有样本 —— 麦克风权限被拒的可能性最大"),
        }
    }
    Ok(())
}

/// 只读一个输入设备的电平。
///
/// 排查用：同样一份代码去读内置麦克风，如果内置有正常电平、虚拟声卡没有，
/// 说明权限没问题，问题在虚拟声卡的直通；两边都静音则说明是权限被拒。
pub fn probe(name: &str, seconds: u64) -> Result<()> {
    let p = apm_sink::Probe::open(name, AUDIO_SAMPLE_RATE).context("打开输入设备失败")?;
    println!("监听 \"{name}\" {} 秒...", seconds);
    std::thread::sleep(Duration::from_secs(seconds));
    println!(
        "样本 {:.2} s，RMS {}，峰值 {}",
        p.seconds(),
        p.rms_dbfs()
            .map(|v| format!("{v:.2} dBFS"))
            .unwrap_or_else(|| "无样本".into()),
        p.peak_dbfs()
            .map(|v| format!("{v:.2} dBFS"))
            .unwrap_or_else(|| "-".into())
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// ping
// ---------------------------------------------------------------------------

/// 链路往返时延：PING 带递增 seq，设备原样回 PONG。
///
/// 只测链路，**不**声称端到端：真正的话筒延迟还要过 codec 采集、20 ms 分块和
/// 抖动缓冲，那需要声学回环（见 docs/KNOWN_ISSUES.md 的 KI-003）。
pub async fn ping(
    device: Option<&str>,
    count: u32,
    interval_ms: u64,
    timeout_ms: u64,
    as_json: bool,
) -> Result<()> {
    if count == 0 {
        bail!("--count 至少为 1");
    }
    if timeout_ms == 0 {
        bail!("--timeout-ms 至少为 1");
    }

    let link = apm_link::connect(device, Duration::from_secs(15))
        .await
        .map_err(|e| anyhow!("{e}\n提示: {}", apm_link::device::hint_for_error(&e)))?;

    if !as_json {
        println!("已连接。PING {count} 次（间隔 {interval_ms} ms，单次超时 {timeout_ms} ms）。");
        println!("口径: 这是链路往返（含连接间隔与设备任务调度），音频单程 ≈ RTT/2；");
        println!("      不等于端到端声学延迟 —— 后者还要过 codec 采集、20 ms 分块与抖动缓冲。");
        println!();
    }

    let mut samples: Vec<f64> = Vec::new();
    let mut timeouts = 0u32;
    let mut send_error: Option<String> = None;

    for i in 0..count {
        let seq = i + 1;
        match link.ping(seq, Duration::from_millis(timeout_ms)).await {
            Ok(Some(rtt)) => {
                let ms = rtt.as_secs_f64() * 1000.0;
                samples.push(ms);
                if !as_json {
                    println!("  #{seq:<3} {ms:8.1} ms");
                }
            }
            Ok(None) => {
                timeouts += 1;
                if !as_json {
                    println!("  #{seq:<3}   超时（> {timeout_ms} ms）");
                }
            }
            Err(e) => {
                // 链路断了不是"超时一次"，直接停手并如实报告，别把断线混进延迟统计。
                send_error = Some(format!("{e:#}"));
                break;
            }
        }
        if i + 1 < count && interval_ms > 0 {
            tokio::time::sleep(Duration::from_millis(interval_ms)).await;
        }
    }

    link.disconnect().await.ok();

    if let Some(err) = send_error {
        bail!("链路中断：{err}（已收到 {} 次回包）", samples.len());
    }
    if samples.is_empty() {
        bail!("{count} 次 PING 全部超时。可能是链路不通，或设备还在跑未实现 PONG 的旧固件");
    }

    let stats = PingStats::from_samples(&samples).expect("samples 已确认非空");
    let n = stats.n;
    let (min, median, p95, max, mean, jitter, one_way) = (
        stats.min,
        stats.median,
        stats.p95,
        stats.max,
        stats.mean,
        stats.jitter,
        stats.one_way_estimate,
    );

    if as_json {
        let report = json!({
            "schema": "ai-passport-mic-ping-report-1",
            "sent": count,
            "received": n,
            "timeouts": timeouts,
            "rtt_ms": {
                "min": round2(min),
                "median": round2(median),
                "p95": round2(p95),
                "max": round2(max),
                "mean": round2(mean),
            },
            "jitter_ms": round2(jitter),
            "one_way_estimate_ms": round2(one_way),
            "samples_ms": samples.iter().map(|v| round2(*v)).collect::<Vec<_>>(),
            "note": "链路往返，不含 codec 采集、20 ms 分块与抖动缓冲；one_way_estimate 只是 RTT/2",
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!();
    println!(
        "往返: 最小 {:.1} / 中位 {:.1} / p95 {:.1} / 最大 {:.1} ms（发出 {}，收到 {}，超时 {}）",
        min, median, p95, max, count, n, timeouts
    );
    println!("抖动: 相邻两次 RTT 的平均绝对差 {:.1} ms", jitter);
    println!(
        "音频单程估计: ≈ {:.1} ms（中位 RTT/2，仅链路，不含 codec 与缓冲）",
        one_way
    );
    if n < count as usize {
        println!(
            "注意: 有 {} 次没回包，延迟统计只覆盖回包的那几次",
            count as usize - n
        );
    }
    Ok(())
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// 延迟统计。抽成不依赖蓝牙的纯结构，是为了能单测 —— 现场只有一台设备，
/// 统计口径写错了没人能发现。
#[derive(Debug, Clone, Copy, PartialEq)]
struct PingStats {
    n: usize,
    min: f64,
    median: f64,
    p95: f64,
    max: f64,
    mean: f64,
    /// 相邻两次 RTT 的平均绝对差。不用标准差：这里要回答的是"会不会忽然抖一下"。
    jitter: f64,
    /// 中位 RTT 的一半。**只是链路单程估计**，不含 codec 采集与抖动缓冲。
    one_way_estimate: f64,
}

impl PingStats {
    fn from_samples(samples: &[f64]) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }
        let mut sorted = samples.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = sorted.len();

        let median = if n % 2 == 1 {
            sorted[n / 2]
        } else {
            (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
        };
        // 最近秩法：p95 取"至少 95% 的样本不超过它"的实测值，不插值 ——
        // 报告里每个数都应该是真的测到过的数。
        let p95 = sorted[((n as f64 * 0.95).ceil() as usize).clamp(1, n) - 1];
        let mean = samples.iter().sum::<f64>() / n as f64;
        let jitter = if n > 1 {
            samples.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f64>() / (n - 1) as f64
        } else {
            0.0
        };

        Some(Self {
            n,
            min: sorted[0],
            median,
            p95,
            max: sorted[n - 1],
            mean,
            jitter,
            one_way_estimate: median / 2.0,
        })
    }
}

/// 交付模式的说明文案。
///
/// 单独抽出来是因为 `SessionSummary` 只带字符串（跨 IPC 友好），
/// 而解释要引用 `apm_app` 里的枚举判词，避免两处措辞漂移。
fn delivery_explain(pattern: &str) -> &'static str {
    use apm_app::pipeline::DeliveryKind;
    match pattern {
        "even" => DeliveryKind::Even.explain(),
        "clumped" => DeliveryKind::Clumped.explain(),
        "unclear" => DeliveryKind::Unclear.explain(),
        _ => DeliveryKind::Insufficient.explain(),
    }
}

#[cfg(test)]
mod tests {
    use super::PingStats;

    #[test]
    fn ping_stats_rejects_empty() {
        assert!(PingStats::from_samples(&[]).is_none());
    }

    #[test]
    fn ping_stats_single_sample_has_zero_jitter() {
        let s = PingStats::from_samples(&[12.0]).unwrap();
        assert_eq!(s.n, 1);
        assert_eq!(s.min, 12.0);
        assert_eq!(s.median, 12.0);
        assert_eq!(s.p95, 12.0);
        assert_eq!(s.max, 12.0);
        assert_eq!(s.jitter, 0.0);
        assert_eq!(s.one_way_estimate, 6.0);
    }

    #[test]
    fn ping_stats_uses_nearest_rank_p95() {
        // 1..=20：20 个样本的 p95 是第 19 个值（19.0），不是插值出来的 19.05。
        let v: Vec<f64> = (1..=20).map(|x| x as f64).collect();
        let s = PingStats::from_samples(&v).unwrap();
        assert_eq!(s.min, 1.0);
        assert_eq!(s.max, 20.0);
        assert_eq!(s.median, 10.5);
        assert_eq!(s.p95, 19.0);
        assert_eq!(s.mean, 10.5);
        assert_eq!(s.jitter, 1.0);
        assert_eq!(s.one_way_estimate, 5.25);
    }

    #[test]
    fn ping_stats_jitter_tracks_order_not_just_spread() {
        // 同样一组数，顺序不同抖动不同 —— 抖动必须按原始发送顺序算。
        let steady = PingStats::from_samples(&[10.0, 10.0, 10.0]).unwrap();
        let spiky = PingStats::from_samples(&[10.0, 40.0, 10.0]).unwrap();
        assert_eq!(steady.jitter, 0.0);
        assert_eq!(spiky.jitter, 30.0);
        assert_eq!(steady.median, spiky.median);
    }
}
