//! 子命令实现。原则：实测到的写数字，没测的写 null，不猜。

use anyhow::{anyhow, bail, Context, Result};
use apm_link::{Notification, AUDIO_BLOCK_LEN};
use apm_protocol::adpcm;
use apm_protocol::frame;
use apm_protocol::metrics;
use apm_protocol::wav;
use apm_protocol::{AUDIO_BLOCK_SAMPLES, AUDIO_SAMPLE_RATE};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

// ---------------------------------------------------------------------------
// analyze
// ---------------------------------------------------------------------------
pub fn analyze(path: &Path, as_json: bool) -> Result<()> {
    let file = wav::read_wav(path).map_err(|e| anyhow!("{path:?}: {e}"))?;

    // 多声道先下混：直接对交错样本算 RMS 会把两个声道混成一个假信号。
    let (samples, downmixed) = if file.channels > 1 {
        let ch = file.channels as usize;
        let mono: Vec<i16> = file
            .samples
            .chunks_exact(ch)
            .map(|frame| (frame.iter().map(|&s| s as i32).sum::<i32>() / ch as i32) as i16)
            .collect();
        (mono, true)
    } else {
        (file.samples.clone(), false)
    };

    let m = metrics::analyze(&samples, file.sample_rate);
    let checks = metrics::evaluate(&m);
    let pass = metrics::software_pass(&checks);

    if as_json {
        let report = json!({
            "schema": "ai-passport-mic-quality-report-1",
            "firmware": Value::Null,
            "desktop": env!("CARGO_PKG_VERSION"),
            "host": format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
            "adapter": Value::Null,
            "virtual_device": Value::Null,
            "environment": Value::Null,
            "distance_cm": Value::Null,
            "angle_deg": Value::Null,
            "reference_source": Value::Null,
            "recording": path.display().to_string(),
            "metrics": {
                "duration_s": round2(m.duration_s),
                "rms_dbfs": m.rms_dbfs.map(round2),
                "peak_dbfs": m.peak_dbfs.map(round2),
                "clipping_pct": round3(m.clipping_pct),
                "dc_offset_pct": round3(m.dc_offset_pct),
                "zero_crossing_pct": round2(m.zero_crossing_pct),
                // 下面三项只能从链路日志得到，WAV 里没有。
                "lost_chunks": Value::Null,
                "startup_latency_p95_ms": Value::Null,
                "steady_latency_p95_ms": Value::Null,
            },
            "subjective": Value::Null,
            "verdict": if pass { "software-pass-hardware-pending" } else { "software-fail" },
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!("文件: {}", path.display());
    println!(
        "格式: {} Hz / {} ch / {:.2} s（{} 采样）{}",
        file.sample_rate,
        file.channels,
        m.duration_s,
        m.samples,
        if downmixed { "，已下混为单声道" } else { "" }
    );
    if file.sample_rate != AUDIO_SAMPLE_RATE {
        println!(
            "注意: 采样率不是 {} Hz。指标本身与采样率无关，但与固件链路对比时要留意。",
            AUDIO_SAMPLE_RATE
        );
    }
    println!();
    println!("{}{}阈值", pad("指标", 14), pad("实测", 18));
    for check in &checks {
        let mark = if check.ok { "OK  " } else { "FAIL" };
        println!(
            "{}{}{} {}",
            pad(check.name, 14),
            pad(&check.actual, 18),
            mark,
            check.expected
        );
    }
    println!();
    println!(
        "结论: {}",
        if pass {
            "软件分析通过（software-pass-hardware-pending）"
        } else {
            "软件分析不通过（software-fail）"
        }
    );
    println!("这只说明文件本身没有明显削波/直流偏置/电平异常；");
    println!("真机链路、虚拟声卡与会议软件端到端是否通过，必须另外测。");
    Ok(())
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}
fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// 终端列宽：CJK 字符占两格，直接 `{:<12}` 会让表格歪掉。
fn display_width(text: &str) -> usize {
    text.chars()
        .map(|c| match c as u32 {
            0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF
            | 0xFE30..=0xFE6F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 => 2,
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

fn hex_eq(a: &[u8], b: &[u8]) -> bool {
    a == b
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
        check(hex_eq(&got, &want), &format!("{name} 帧编码与 C 一致"));

        let decoded = frame::decode(&want);
        let ok = matches!(&decoded, Ok((f, used)) if *used == want.len() && f.payload == payload.as_slice());
        check(ok, &format!("{name} 帧解码往返"));
    }

    println!("[adpcm]");
    let mut state = adpcm::AdpcmState::default();
    for case in v["adpcm"].as_array().context("adpcm 不是数组")? {
        let index = case["index"].as_u64().unwrap_or(0);
        let pcm: Vec<i16> = case["pcm"]
            .as_array()
            .map(|a| a.iter().map(|x| x.as_i64().unwrap_or(0) as i16).collect())
            .unwrap_or_default();
        let want_block = hex_decode(case["block_hex"].as_str().unwrap_or(""));
        let got_block = adpcm::encode_block(&mut state, &pcm);
        check(
            hex_eq(&got_block, &want_block),
            &format!("块 {index} 编码与 C 逐字节一致"),
        );

        let want_decoded: Vec<i16> = case["decoded"]
            .as_array()
            .map(|a| a.iter().map(|x| x.as_i64().unwrap_or(0) as i16).collect())
            .unwrap_or_default();
        let got_decoded = adpcm::decode_block(&want_block, AUDIO_BLOCK_SAMPLES);
        check(got_decoded == want_decoded, &format!("块 {index} 解码与 C 一致"));
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
            pad(if d.name.is_empty() { "(无名)" } else { &d.name }, 20),
            pad(&d.rssi.map(|r| format!("{r} dBm")).unwrap_or_else(|| "-".into()), 10),
            if d.has_service { "yes" } else { "no" }
        );
    }
    Ok(())
}

pub async fn status(device: Option<&str>, timeout_s: u64) -> Result<()> {
    let mut link = apm_link::connect(device, Duration::from_secs(15))
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
                println!("收到音频块 {} 字节（未订阅录音，忽略）", block.len());
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

fn yes_no(v: bool) -> &'static str {
    if v {
        "是"
    } else {
        "否"
    }
}

pub async fn record(path: &Path, seconds: u64, device: Option<&str>) -> Result<()> {
    let mut link = apm_link::connect(device, Duration::from_secs(15))
        .await
        .map_err(|e| anyhow!("{e}\n提示: {}", apm_link::device::hint_for_error(&e)))?;

    println!("已连接，请求开始采集（{} 秒）...", seconds);
    link.start_capture().await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let mut samples: Vec<i16> = Vec::new();
    let mut blocks = 0u32;
    let mut lost = 0u32;
    let mut bad_len = 0u32;
    let mut last_seq: Option<u16> = None;
    // 设备侧在发送阶段丢的块 PC 端看不见（序号根本没发出来），只能靠 STATUS 上报
    let mut dev_dropped: u16 = 0;

    while tokio::time::Instant::now() < deadline {
        match link.next_notification(Duration::from_millis(500)).await {
            Ok(Some(Notification::Status(s))) => {
                dev_dropped = s.dropped;
            }
            Ok(Some(Notification::Audio(block))) => {
                if block.len() != AUDIO_BLOCK_LEN {
                    // 长度不对通常意味着 MTU 不足导致截断，必须报出来而不是丢掉装没事。
                    if bad_len == 0 {
                        println!(
                            "警告: 音频块长度 {} != {}，可能 MTU 不足被截断",
                            block.len(),
                            AUDIO_BLOCK_LEN
                        );
                    }
                    bad_len += 1;
                    continue;
                }
                if let Some((seq, _)) = adpcm::block_header(&block) {
                    if let Some(prev) = last_seq {
                        let gap = seq.wrapping_sub(prev.wrapping_add(1));
                        if gap != 0 {
                            lost += gap as u32;
                        }
                    }
                    last_seq = Some(seq);
                }
                let mut pcm = vec![0i16; AUDIO_BLOCK_SAMPLES];
                adpcm::decode_into(&block, &mut pcm);
                samples.extend_from_slice(&pcm);
                blocks += 1;
            }
            Ok(Some(_)) => {}
            Ok(None) => {}
            Err(e) => {
                println!("通知流出错: {e}");
                break;
            }
        }
    }

    link.stop_capture().await.ok();
    link.disconnect().await.ok();

    if samples.is_empty() {
        bail!("一个音频块都没收到。检查设备是否真的在采集（OK 键是否被按住过、STATUS 是否 CAPTURING）");
    }

    wav::write_wav(path, AUDIO_SAMPLE_RATE, 1, &samples).map_err(|e| anyhow!("写 WAV 失败: {e}"))?;

    let m = metrics::analyze(&samples, AUDIO_SAMPLE_RATE);
    println!();
    println!("已写入 {}", path.display());
    println!(
        "块: {} 个（{:.2} s），序号跳变丢失: {}，长度异常: {}",
        blocks,
        m.duration_s,
        lost,
        bad_len
    );
    if dev_dropped > 0 {
        println!(
            "设备侧丢弃: {} 块（发送阶段丢的，序号没发出来，PC 端统计不到）",
            dev_dropped
        );
    }
    println!(
        "RMS: {}  峰值: {}  削波: {:.3}%  直流: {:.3}%",
        m.rms_dbfs.map(|v| format!("{v:.2} dBFS")).unwrap_or_else(|| "静音".into()),
        m.peak_dbfs.map(|v| format!("{v:.2} dBFS")).unwrap_or_else(|| "静音".into()),
        m.clipping_pct,
        m.dc_offset_pct
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
/// 与 `record` 的共同点：丢块、长度异常、欠载都单独报数，不用"能听到"掩盖问题。
pub async fn sink(
    to: &str,
    seconds: u64,
    target_ms: u32,
    verify: bool,
    tone: bool,
    device: Option<&str>,
) -> Result<()> {
    // 先开声卡：设备名写错时不该先去连 BLE，也不该让用户以为是设备的问题。
    let mut out = apm_sink::Sink::open(to, AUDIO_SAMPLE_RATE as u32, target_ms)
        .context("打开虚拟声卡失败")?;
    let target_start = out.target_ms();
    println!(
        "输出: {}（{} Hz / {} ch），抖动缓冲目标 {} ms",
        out.device_name(),
        out.rate(),
        out.channels(),
        out.target_ms()
    );

    if tone {
        return sink_tone(&mut out, seconds, verify);
    }

    let mut link = apm_link::connect(device, Duration::from_secs(15))
        .await
        .map_err(|e| anyhow!("{e}\n提示: {}", apm_link::device::hint_for_error(&e)))?;
    println!("已连接设备，开始采集（{} 秒）。会议软件里把麦克风选成该设备即可。", seconds);
    link.start_capture().await?;

    // 回环探针放在采集开始之后起，避免把连接期的静音算进 RMS。
    let probe = if verify {
        match apm_sink::Probe::open(to, AUDIO_SAMPLE_RATE as u32) {
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
    let mut blocks = 0u32;
    let mut lost = 0u32;
    let mut bad_len = 0u32;
    let mut last_seq: Option<u16> = None;
    let mut next_report = tokio::time::Instant::now() + Duration::from_secs(1);
    let mut elapsed_s = 0u64;
    let mut dev_dropped: u16 = 0;

    while tokio::time::Instant::now() < deadline {
        match link.next_notification(Duration::from_millis(500)).await {
            Ok(Some(Notification::Audio(block))) => {
                if block.len() != AUDIO_BLOCK_LEN {
                    if bad_len == 0 {
                        println!(
                            "警告: 音频块长度 {} != {}，可能 MTU 不足被截断",
                            block.len(),
                            AUDIO_BLOCK_LEN
                        );
                    }
                    bad_len += 1;
                    continue;
                }
                if let Some((seq, _)) = adpcm::block_header(&block) {
                    if let Some(prev) = last_seq {
                        let gap = seq.wrapping_sub(prev.wrapping_add(1));
                        if gap != 0 {
                            lost += gap as u32;
                        }
                    }
                    last_seq = Some(seq);
                }
                let mut pcm = vec![0i16; AUDIO_BLOCK_SAMPLES];
                adpcm::decode_into(&block, &mut pcm);
                out.push(&pcm);
                blocks += 1;
            }
            Ok(Some(Notification::Status(s))) => {
                dev_dropped = s.dropped;
            }
            Ok(Some(_)) => {}
            Ok(None) => {}
            Err(e) => {
                println!("通知流出错: {e}");
                break;
            }
        }

        if tokio::time::Instant::now() >= next_report {
            next_report += Duration::from_secs(1);
            elapsed_s += 1;
            let st = out.stats();
            println!(
                "  [{:>3}s] 块={} 缓冲={:.0} ms 已播={:.1} s 欠载={} 静音帧={} 丢弃={}",
                elapsed_s,
                blocks,
                out.fill_ms(),
                st.frames_consumed as f64 / AUDIO_SAMPLE_RATE as f64,
                st.underruns,
                st.frames_silenced,
                st.frames_dropped
            );
        }
    }

    // 先停写再断链：STOP 有一次 BLE 往返，期间声卡还在消费缓冲，
    // 拖到最后才 clear 会白掉一段静音（实测尾巴上多出 3 次欠载）。
    out.clear();
    link.stop_capture().await.ok();
    link.disconnect().await.ok();
    let st = out.stats();

    println!();
    println!(
        "块: {} 个，序号跳变丢失: {}，长度异常: {}，设备侧丢弃: {}",
        blocks, lost, bad_len, dev_dropped
    );
    println!(
        "声卡: 已播 {:.2} s，欠载 {} 次（静音 {} 帧 ≈ {:.0} ms），超容量丢弃 {} 帧",
        st.frames_consumed as f64 / AUDIO_SAMPLE_RATE as f64,
        st.underruns,
        st.frames_silenced,
        st.frames_silenced as f64 * 1000.0 / AUDIO_SAMPLE_RATE as f64,
        st.frames_dropped
    );
    println!(
        "缓冲: 目标 {} → {} ms（欠载抬高 {} 次 / 稳定收回 {} 次），回调抢锁失败 {} 次",
        target_start,
        out.target_ms(),
        st.target_raises,
        st.target_lowers,
        out.lock_misses()
    );
    if st.underruns > 0 {
        println!("提示: 有欠载说明缓冲不够或链路抖动；把 --target-ms 往上调（上限 80）再看。");
    }
    if let Some(p) = probe {
        let written = out.rms_dbfs();
        let looped = p.rms_dbfs();
        println!();
        println!(
            "回环: 输入端 {:.2} s，RMS {}，峰值 {}",
            p.seconds(),
            looped.map(|v| format!("{v:.2} dBFS")).unwrap_or_else(|| "无样本".into()),
            p.peak_dbfs().map(|v| format!("{v:.2} dBFS")).unwrap_or_else(|| "-".into())
        );
        println!(
            "写入: RMS {}",
            written.map(|v| format!("{v:.2} dBFS")).unwrap_or_else(|| "无样本".into())
        );
        match (written, looped) {
            (Some(w), Some(l)) => {
                let diff = (w - l).abs();
                if diff <= 3.0 {
                    println!("判定: 回环与写入电平差 {diff:.1} dB（≤3 dB）→ 音频确实进了声卡");
                } else {
                    println!("判定: 电平差 {diff:.1} dB，超过 3 dB —— 声卡可能没在直通，或采样率不一致");
                }
            }
            _ => println!("判定: 样本不足，无法比较（检查权限或把 --seconds 调大）"),
        }
    }
    println!("这只证明 PCM 已经推进声卡；会议软件里能不能选中它，要另外确认。");
    Ok(())
}

/// 只验证"声卡这一段"：不连 BLE，直接推 440 Hz 正弦，再从输入端回读。
///
/// 存在的理由：先把权限、采样率、直通三件事从 BLE 链路里剥离出来，
/// 否则一旦回环是静音，分不清是蓝牙没声还是声卡没通。
fn sink_tone(out: &mut apm_sink::Sink, seconds: u64, verify: bool) -> Result<()> {
    use std::f64::consts::TAU;

    let rate = AUDIO_SAMPLE_RATE as f64;
    let block = AUDIO_BLOCK_SAMPLES;
    let amp = 32768.0 * 10f64.powf(-20.0 / 20.0); // -20 dBFS

    let probe = if verify {
        match apm_sink::Probe::open(out.device_name(), AUDIO_SAMPLE_RATE as u32) {
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
    for i in 0..(seconds * 50) {
        let mut pcm = Vec::with_capacity(block);
        for _ in 0..block {
            pcm.push((amp * phase.sin()) as i16);
            phase = (phase + TAU * 440.0 / rate) % TAU;
        }
        out.push(&pcm);
        pushed += block as u64;
        if i % 5 == 4 {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    std::thread::sleep(Duration::from_millis(300));
    out.clear();

    let st = out.stats();
    println!(
        "写入 {} 帧，声卡取走 {} 帧，欠载 {} 次",
        pushed, st.frames_consumed, st.underruns
    );
    if let Some(p) = probe {
        println!(
            "回环: 输入端 {:.2} s，RMS {}，峰值 {}",
            p.seconds(),
            p.rms_dbfs().map(|v| format!("{v:.2} dBFS")).unwrap_or_else(|| "无样本".into()),
            p.peak_dbfs().map(|v| format!("{v:.2} dBFS")).unwrap_or_else(|| "-".into())
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
    let p = apm_sink::Probe::open(name, AUDIO_SAMPLE_RATE as u32)
        .context("打开输入设备失败")?;
    println!("监听 \"{name}\" {} 秒...", seconds);
    std::thread::sleep(Duration::from_secs(seconds));
    println!(
        "样本 {:.2} s，RMS {}，峰值 {}",
        p.seconds(),
        p.rms_dbfs().map(|v| format!("{v:.2} dBFS")).unwrap_or_else(|| "无样本".into()),
        p.peak_dbfs().map(|v| format!("{v:.2} dBFS")).unwrap_or_else(|| "-".into())
    );
    Ok(())
}
