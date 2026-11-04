//! 把 BLE 解出来的 PCM 写进系统虚拟声卡。
//!
//! 这一层只做三件事：**抖动缓冲**、**欠载计数**、**断连停写**。
//! 它不负责重采样：设备侧是 16 kHz 单声道，声卡也必须能吃 16 kHz，否则直接报错，
//! 而不是偷偷改成 48 kHz 让人以为通了。
//!
//! 契约（见 `docs/ARCHITECTURE.md`）：
//! - 抖动缓冲目标 40–80 ms，超出上限丢最旧的整帧，不追着补。
//! - 断连时必须 [`Sink::clear`]：停止播放并丢掉旧缓冲，不能让会议软件继续收到
//!   上一秒的音频假装设备还在。
//! - 欠载（缓冲被读空）单独计数，不混进"正常"里。

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// 抖动缓冲目标下限 / 上限（毫秒），与架构文档的 40–80 ms 一致。
pub const TARGET_MIN_MS: u32 = 40;
pub const TARGET_MAX_MS: u32 = 80;

/// 缓冲容量上限：目标值的 3 倍，且不低于 200 ms。
/// 超过这个量说明链路或消费端已经失控，宁可丢最旧的也不要让延迟无限涨。
fn capacity_frames(target_ms: u32, rate: u32) -> usize {
    let ms = (target_ms * 3).max(200);
    (rate as usize * ms as usize) / 1000
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    /// 收到的帧（单声道采样）数
    pub frames_pushed: u64,
    /// 声卡实际取走的帧数
    pub frames_consumed: u64,
    /// 欠载次数（缓冲被读空）
    pub underruns: u64,
    /// 欠载时补进去的帧数（用上一个样本填充，避免爆音）
    pub frames_silenced: u64,
    /// 超出容量被丢弃的最旧帧数
    pub frames_dropped: u64,
    /// 推入样本的平方和，用于算写入侧 RMS（和回环侧对比）
    pub sum_sq: f64,
    /// 因欠载抬高目标缓冲的次数
    pub target_raises: u64,
    /// 因稳定收回目标缓冲的次数
    pub target_lowers: u64,
}

struct Shared {
    buf: VecDeque<i16>,
    rate: u32,
    /// 当前预填目标（毫秒）。会随欠载抬高、随稳定收回，范围 40–80 ms。
    target_ms: u32,
    target_frames: usize,
    capacity: usize,
    /// 已攒够 target_frames 才开始播放，避免一上来就欠载
    playing: bool,
    /// 连续无欠载的帧数，用于判断"稳了可以收回缓冲"
    stable_frames: u64,
    /// 最近一个输出样本，欠载时用它填充（0 会听到明显断续，保持电平更自然）
    last: i16,
    stats: Stats,
}

impl Shared {
    fn fill_ms(&self, rate: u32) -> f32 {
        self.buf.len() as f32 * 1000.0 / rate as f32
    }

    fn set_target_ms(&mut self, ms: u32) {
        self.target_ms = ms.clamp(TARGET_MIN_MS, TARGET_MAX_MS);
        self.target_frames = self.rate as usize * self.target_ms as usize / 1000;
    }

    /// 稳定判定窗口：连续这么多帧没有欠载才考虑收回缓冲（默认 5 秒）。
    fn stable_window(&self) -> u64 {
        self.rate as u64 * 5
    }

    /// 收回缓冲的前提：现存量要比目标还多出一截，否则会立刻又欠载。
    fn can_lower(&self) -> bool {
        self.target_ms > TARGET_MIN_MS
            && self.buf.len() > self.target_frames + (self.rate as usize / 50) // +20 ms
    }
}

pub struct Sink {
    _stream: cpal::Stream,
    shared: Arc<Mutex<Shared>>,
    /// 回调抢不到锁而整块出静音的次数（单独统计，不混进欠载）
    lock_misses: Arc<std::sync::atomic::AtomicU64>,
    rate: u32,
    channels: u16,
    device_name: String,
    target_ms: u32,
}

/// 列出所有输出设备名，供选错设备时提示用。
pub fn output_devices() -> Result<Vec<String>> {
    let host = cpal::default_host();
    let devices = host.output_devices().context("枚举输出设备失败")?;
    Ok(devices.filter_map(|d| d.name().ok()).collect())
}

impl Sink {
    /// 按设备名子串打开输出，固定 `rate` 采样率。
    ///
    /// 设备不支持这个采样率时直接失败并列出可选设备 —— 不做隐式重采样。
    pub fn open(name_substr: &str, rate: u32, target_ms: u32) -> Result<Self> {
        let target_ms = target_ms.clamp(TARGET_MIN_MS, TARGET_MAX_MS);
        let host = cpal::default_host();

        let mut matched: Option<(cpal::Device, String)> = None;
        for device in host.output_devices().context("枚举输出设备失败")? {
            let name = device.name().unwrap_or_default();
            if name.to_lowercase().contains(&name_substr.to_lowercase()) {
                matched = Some((device, name));
                break;
            }
        }
        let (device, device_name) = matched.ok_or_else(|| {
            let all = output_devices().unwrap_or_default();
            anyhow!(
                "没有名字包含 \"{name_substr}\" 的输出设备。\n现有输出设备: {}",
                if all.is_empty() {
                    "(无)".to_string()
                } else {
                    all.join(" / ")
                }
            )
        })?;

        // 只接受原生支持目标采样率的配置；找不到就报错，不重采样。
        let mut chosen: Option<cpal::SupportedStreamConfigRange> = None;
        for range in device.supported_output_configs().context("读取声卡配置失败")? {
            let (lo, hi) = (range.min_sample_rate().0, range.max_sample_rate().0);
            if rate >= lo && rate <= hi {
                // 优先多声道（虚拟声卡常见 2ch），单声道也能用
                let better = match &chosen {
                    None => true,
                    Some(cur) => range.channels() > cur.channels(),
                };
                if better {
                    chosen = Some(range);
                }
            }
        }
        let range = chosen.ok_or_else(|| {
            anyhow!(
                "输出设备 \"{device_name}\" 不支持 {rate} Hz 原生采样率；本项目不隐式重采样，\
                 请在声卡设置里把采样率改成 {rate} Hz 后重试"
            )
        })?;
        let supported = range.with_sample_rate(cpal::SampleRate(rate));
        let sample_format = supported.sample_format();
        let config = supported.config();
        let channels = config.channels;
        if channels == 0 {
            return Err(anyhow!("输出设备 \"{device_name}\" 报告 0 声道"));
        }

        let lock_misses = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let shared = Arc::new(Mutex::new(Shared {
            // 容量按「目标上限」的 3 倍预留，这样目标抬高时不用重新分配
            buf: VecDeque::with_capacity(capacity_frames(TARGET_MAX_MS, rate)),
            rate,
            target_ms,
            target_frames: rate as usize * target_ms as usize / 1000,
            capacity: capacity_frames(TARGET_MAX_MS, rate),
            playing: false,
            stable_frames: 0,
            last: 0,
            stats: Stats::default(),
        }));

        let stream = match sample_format {
            cpal::SampleFormat::F32 => {
                build::<f32>(&device, &config, shared.clone(), lock_misses.clone())?
            }
            cpal::SampleFormat::I16 => {
                build::<i16>(&device, &config, shared.clone(), lock_misses.clone())?
            }
            other => {
                return Err(anyhow!(
                    "声卡采样格式 {other:?} 暂不支持（只实现 f32 / i16）"
                ))
            }
        };
        stream.play().context("启动输出流失败")?;

        Ok(Self {
            _stream: stream,
            shared,
            lock_misses,
            rate,
            channels,
            device_name,
            target_ms,
        })
    }

    /// 推入一段单声道 PCM。
    pub fn push(&mut self, samples: &[i16]) {
        let mut sh = self.shared.lock().expect("sink 缓冲锁中毒");
        for &s in samples {
            sh.buf.push_back(s);
        }
        let sq: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
        sh.stats.sum_sq += sq;
        // 超容量：丢最旧的，保证延迟有上界
        while sh.buf.len() > sh.capacity {
            sh.buf.pop_front();
            sh.stats.frames_dropped += 1;
        }
        sh.stats.frames_pushed += samples.len() as u64;
    }

    /// 断连时调用：停止播放并丢弃旧缓冲。
    ///
    /// 不这么做的话，声卡会继续把上一段声音播完 / 循环补静音，
    /// 会议软件会以为设备还在线 —— 这正是架构文档明令禁止的。
    pub fn clear(&mut self) {
        let mut sh = self.shared.lock().expect("sink 缓冲锁中毒");
        sh.buf.clear();
        sh.playing = false;
        sh.last = 0;
    }

    pub fn stats(&self) -> Stats {
        self.shared.lock().expect("sink 缓冲锁中毒").stats
    }

    /// 写入侧 RMS（dBFS），用来和回环探针的读数对比。
    pub fn rms_dbfs(&self) -> Option<f64> {
        let sh = self.shared.lock().ok()?;
        if sh.stats.frames_pushed == 0 {
            return None;
        }
        let mean = sh.stats.sum_sq / sh.stats.frames_pushed as f64;
        if mean <= 0.0 {
            return Some(-120.0);
        }
        Some(20.0 * (mean.sqrt() / 32768.0).log10())
    }

    /// 回调抢不到锁而整块出静音的次数
    pub fn lock_misses(&self) -> u64 {
        self.lock_misses.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn fill_ms(&self) -> f32 {
        let sh = self.shared.lock().expect("sink 缓冲锁中毒");
        sh.fill_ms(self.rate)
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// 当前抖动缓冲目标（会自适应变化）
    pub fn target_ms(&self) -> u32 {
        self.shared
            .lock()
            .map(|s| s.target_ms)
            .unwrap_or(self.target_ms)
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    shared: Arc<Mutex<Shared>>,
    lock_misses: Arc<std::sync::atomic::AtomicU64>,
) -> Result<cpal::Stream>
where
    T: cpal::Sample + cpal::SizedSample + cpal::FromSample<i16> + 'static,
{
    let channels = config.channels as usize;
    let err_shared = shared.clone();
    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [T], _info: &cpal::OutputCallbackInfo| {
                let mut sh = match err_shared.try_lock() {
                    Ok(g) => g,
                    // 主线程正在 push：这一轮先出静音，不能在这里等锁，否则会卡住音频线程。
                    // 但这同样是「本该有声却出了静音」，必须计入欠载，否则统计会骗人。
                    Err(_) => {
                        lock_misses.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        for v in data.iter_mut() {
                            *v = T::from_sample(0i16);
                        }
                        return;
                    }
                };

                let frames = data.len() / channels;

                // 预填：缓冲没攒够目标值之前不出声，避免一开始就欠载
                if !sh.playing {
                    if sh.buf.len() >= sh.target_frames {
                        sh.playing = true;
                    } else {
                        for v in data.iter_mut() {
                            *v = T::from_sample(0i16);
                        }
                        return;
                    }
                }

                let mut shortage = false;
                for f in 0..frames {
                    let sample = match sh.buf.pop_front() {
                        Some(s) => {
                            sh.last = s;
                            // 稳定计数：连续足够久没欠载，就把缓冲收回一档（带滞回，见 can_lower）
                            sh.stable_frames += 1;
                            if sh.stable_frames >= sh.stable_window() {
                                sh.stable_frames = 0;
                                if sh.can_lower() {
                                    let next = sh.target_ms.saturating_sub(5);
                                    sh.set_target_ms(next);
                                    sh.stats.target_lowers += 1;
                                }
                            }
                            s
                        }
                        None => {
                            // 一次回调里缺多少帧都只算一次欠载事件；缺的帧数单独记
                            shortage = true;
                            sh.stats.frames_silenced += 1;
                            // 欠载说明余量不够：抬高一档（上限 80 ms），并用上一个样本填充避免爆音
                            sh.stable_frames = 0;
                            if sh.target_ms < TARGET_MAX_MS {
                                let next = (sh.target_ms + 5).min(TARGET_MAX_MS);
                                sh.set_target_ms(next);
                                sh.stats.target_raises += 1;
                            }
                            sh.last
                        }
                    };
                    sh.stats.frames_consumed += 1;
                    for c in 0..channels {
                        data[f * channels + c] = T::from_sample(sample);
                    }
                }
                if shortage {
                    sh.stats.underruns += 1;
                }
            },
            move |err| {
                eprintln!("声卡输出错误: {err}");
                if let Ok(mut sh) = shared.lock() {
                    sh.stats.underruns += 1;
                }
            },
            None,
        )
        .context("创建输出流失败")?;
    Ok(stream)
}

// ---------------------------------------------------------------------------
// 回环探针：验证音频真的进了虚拟声卡
// ---------------------------------------------------------------------------

/// 在同一个设备上开输入流，统计实际收到的电平。
///
/// 存在的理由：程序自己报"我写了 N 帧"只是自说自话，虚拟声卡到底有没有把这些
/// 样本环回出来，得从它的输入端再量一次。BlackHole 这类设备是 2 进 2 出直通，
/// 所以写入的电平应该能在输入端原样看到。
pub struct Probe {
    _stream: cpal::Stream,
    shared: Arc<Mutex<ProbeShared>>,
    rate: u32,
    channels: u16,
}

struct ProbeShared {
    sum_sq: f64,
    frames: u64,
    peak: i32,
}

impl Probe {
    pub fn open(name_substr: &str, rate: u32) -> Result<Self> {
        let host = cpal::default_host();
        let mut matched: Option<(cpal::Device, String)> = None;
        for device in host.input_devices().context("枚举输入设备失败")? {
            let name = device.name().unwrap_or_default();
            if name.to_lowercase().contains(&name_substr.to_lowercase()) {
                matched = Some((device, name));
                break;
            }
        }
        let (device, device_name) = matched
            .ok_or_else(|| anyhow!("没有名字包含 \"{name_substr}\" 的输入设备（虚拟声卡要 2 进 2 出）"))?;

        let mut chosen: Option<cpal::SupportedStreamConfigRange> = None;
        for range in device.supported_input_configs().context("读取输入配置失败")? {
            let (lo, hi) = (range.min_sample_rate().0, range.max_sample_rate().0);
            if rate >= lo && rate <= hi {
                let better = match &chosen {
                    None => true,
                    Some(cur) => range.channels() > cur.channels(),
                };
                if better {
                    chosen = Some(range);
                }
            }
        }
        // 诊断用的探针允许回落到设备默认采样率：RMS 与采样率无关，
        // 硬要求 16 kHz 反而会让内置麦克风这类设备直接测不了。
        let supported = match chosen {
            Some(range) => range.with_sample_rate(cpal::SampleRate(rate)),
            None => device
                .default_input_config()
                .with_context(|| format!("输入侧 \"{device_name}\" 既不支持 {rate} Hz，也读不到默认配置"))?,
        };
        let sample_format = supported.sample_format();
        let config = supported.config();
        let channels = config.channels;
        let actual_rate = config.sample_rate.0;

        let shared = Arc::new(Mutex::new(ProbeShared { sum_sq: 0.0, frames: 0, peak: 0 }));
        let stream = match sample_format {
            cpal::SampleFormat::F32 => build_probe::<f32>(&device, &config, shared.clone())?,
            cpal::SampleFormat::I16 => build_probe::<i16>(&device, &config, shared.clone())?,
            other => return Err(anyhow!("输入采样格式 {other:?} 暂不支持")),
        };
        stream.play().context("启动输入流失败")?;

        Ok(Self { _stream: stream, shared, rate: actual_rate, channels })
    }

    /// 从开始到现在累计的 RMS（dBFS）。没有样本时为 None。
    pub fn rms_dbfs(&self) -> Option<f64> {
        let sh = self.shared.lock().ok()?;
        if sh.frames == 0 {
            return None;
        }
        let mean = sh.sum_sq / sh.frames as f64;
        if mean <= 0.0 {
            return Some(-120.0);
        }
        Some(20.0 * (mean.sqrt() / 32768.0).log10())
    }

    pub fn peak_dbfs(&self) -> Option<f64> {
        let sh = self.shared.lock().ok()?;
        if sh.frames == 0 {
            return None;
        }
        let p = sh.peak.max(1) as f64;
        Some(20.0 * (p / 32768.0).log10())
    }

    pub fn frames(&self) -> u64 {
        self.shared.lock().map(|s| s.frames).unwrap_or(0)
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn seconds(&self) -> f64 {
        self.frames() as f64 / self.rate as f64 / self.channels.max(1) as f64
    }
}

fn build_probe<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    shared: Arc<Mutex<ProbeShared>>,
) -> Result<cpal::Stream>
where
    T: cpal::Sample + cpal::SizedSample + 'static,
    f32: cpal::FromSample<T>,
{
    let stream = device
        .build_input_stream(
            config,
            move |data: &[T], _info: &cpal::InputCallbackInfo| {
                let Ok(mut sh) = shared.try_lock() else { return };
                for &v in data {
                    let f: f32 = v.to_sample::<f32>();
                    let s = (f * 32767.0) as i32;
                    sh.sum_sq += (s as f64) * (s as f64);
                    sh.frames += 1;
                    if s.abs() > sh.peak {
                        sh.peak = s.abs();
                    }
                }
            },
            move |err| eprintln!("输入流错误: {err}"),
            None,
        )
        .context("创建输入流失败")?;
    Ok(stream)
}
