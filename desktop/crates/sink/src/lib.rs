//! 把 BLE 解出来的 PCM 写进系统虚拟声卡。
//!
//! 这一层只做四件事：**抖动缓冲**、**时钟漂移补偿**、**欠载计数**、**断连停写**。
//! 它不负责重采样：设备侧是 16 kHz 单声道，声卡也必须能吃 16 kHz，否则直接报错，
//! 而不是偷偷改成 48 kHz 让人以为通了。漂移补偿走单样本插删（见 [`playout`]），
//! 不是周期性丢整帧。
//!
//! 契约（见 `docs/ARCHITECTURE.md`）：
//! - 抖动缓冲目标 40–80 ms，超出上限丢最旧的整帧，不追着补。
//! - 设备与声卡各有一颗晶振，长期漂移由单样本插删吸收，不靠欠载兜底。
//! - 断连时必须 [`Sink::clear`]：停止播放并丢掉旧缓冲，不能让会议软件继续收到
//!   上一秒的音频假装设备还在。
//! - 欠载（缓冲被读空）单独计数，不混进"正常"里。

mod playout;

pub use playout::{
    UNDERRUN_LOG,
    Playout, Stats, DRIFT_TAU_SEC, GAP_BUCKETS_MS, MAX_SLIP, TARGET_MAX_MS, TARGET_MIN_MS,
};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub struct Sink {
    _stream: cpal::Stream,
    playout: Arc<Mutex<Playout>>,
    lock_misses: Arc<AtomicU64>,
    rate: u32,
    channels: u16,
    device_name: String,
    target_ms: u32,
}

/// 列出所有输出设备名，供选错设备时提示用。
pub fn output_devices() -> Result<Vec<String>> {
    let host = cpal::default_host();
    let devices = host.output_devices().context("枚举输出设备失败")?;
    Ok(devices
        .filter_map(|d| d.description().ok().map(|desc| desc.name().to_string()))
        .collect())
}

/// 一个输出设备的能力摘要，供 GUI 的声卡下拉框判断"这个能不能用"。
///
/// `uid` 用 cpal 0.18 的 `Device::id()`（macOS 上是 CoreAudio 的 UID 字符串），取不到时才
/// 回退设备名。留这个字段是为了以后上 WASAPI 时不用改前端契约
/// （`docs/GUI.md` §8 提到 Windows 要按 UID 匹配）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkInfo {
    pub name: String,
    pub uid: String,
    /// 报告的最大声道数；读不到配置时为 0。
    pub channels: u16,
    pub min_rate: u32,
    pub max_rate: u32,
    /// 是否原生支持 16 kHz。不支持时界面要提前拦住，而不是等 `open` 才报错。
    pub supports_16k: bool,
}

/// 枚举输出设备。读不到配置的设备（如被独占）跳过而不是让整次枚举失败。
///
/// `uid` 用 cpal 0.18 的 `Device::id()`（macOS 上是 CoreAudio 的 UID 字符串），
/// 而不是设备名 —— 同名设备（两块同型号 USB 声卡）以前会被认成一个。
pub fn list_sinks() -> Result<Vec<SinkInfo>> {
    let host = cpal::default_host();
    let devices = host.output_devices().context("枚举输出设备失败")?;
    let mut out = Vec::new();
    for device in devices {
        let Ok(desc) = device.description() else { continue };
        let name = desc.name().to_string();
        let mut channels = 0u16;
        let mut min_rate = u32::MAX;
        let mut max_rate = 0u32;
        let mut supports_16k = false;
        if let Ok(configs) = device.supported_output_configs() {
            for range in configs {
                channels = channels.max(range.channels());
                min_rate = min_rate.min(range.min_sample_rate());
                max_rate = max_rate.max(range.max_sample_rate());
                if (range.min_sample_rate()..=range.max_sample_rate()).contains(&16_000) {
                    supports_16k = true;
                }
            }
        }
        // 拿不到 id 时退回设备名：宁可 UID 不唯一，也不能让这个设备从列表里消失。
        let uid = device
            .id()
            .map(|id| id.to_string())
            .unwrap_or_else(|_| name.clone());
        out.push(SinkInfo {
            uid,
            name,
            channels,
            min_rate: if min_rate == u32::MAX { 0 } else { min_rate },
            max_rate,
            supports_16k,
        });
    }
    out.sort_by_key(|s| s.name.to_lowercase());
    Ok(out)
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
            let name = device
                .description()
                .map(|d| d.name().to_string())
                .unwrap_or_default();
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
            let (lo, hi) = (range.min_sample_rate(), range.max_sample_rate());
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
        let supported = range.with_sample_rate(rate);
        let sample_format = supported.sample_format();
        let config = supported.config();
        let channels = config.channels;
        if channels == 0 {
            return Err(anyhow!("输出设备 \"{device_name}\" 报告 0 声道"));
        }

        let lock_misses = Arc::new(AtomicU64::new(0));
        let playout = Arc::new(Mutex::new(Playout::new(rate, target_ms)));

        let stream = match sample_format {
            cpal::SampleFormat::F32 => {
                build::<f32>(&device, &config, playout.clone(), lock_misses.clone())?
            }
            cpal::SampleFormat::I16 => {
                build::<i16>(&device, &config, playout.clone(), lock_misses.clone())?
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
            playout,
            lock_misses,
            rate,
            channels,
            device_name,
            target_ms,
        })
    }

    /// 推入一段单声道 PCM。
    pub fn push(&mut self, samples: &[i16]) {
        self.playout
            .lock()
            .expect("sink 缓冲锁中毒")
            .push(samples);
    }

    /// 断连时调用：停止播放并丢弃旧缓冲。
    ///
    /// 不这么做的话，声卡会继续把上一段声音播完 / 循环补静音，
    /// 会议软件会以为设备还在线 —— 这正是架构文档明令禁止的。
    pub fn clear(&mut self) {
        self.playout.lock().expect("sink 缓冲锁中毒").clear();
    }

    pub fn stats(&self) -> Stats {
        self.playout.lock().expect("sink 缓冲锁中毒").stats()
    }

    /// 长期漂移估计（ppm）。正值 = 设备时钟比声卡快。
    pub fn ppm(&self) -> f64 {
        self.playout.lock().map(|p| p.ppm()).unwrap_or(0.0)
    }

    /// 写入侧 RMS（dBFS），用来和回环探针的读数对比。
    pub fn rms_dbfs(&self) -> Option<f64> {
        let st = self.playout.lock().ok()?.stats();
        if st.frames_pushed == 0 {
            return None;
        }
        let mean = st.sum_sq / st.frames_pushed as f64;
        if mean <= 0.0 {
            return Some(-120.0);
        }
        Some(20.0 * (mean.sqrt() / 32768.0).log10())
    }

    /// 回调抢不到锁而整块出静音的次数
    pub fn lock_misses(&self) -> u64 {
        self.lock_misses.load(Ordering::Relaxed)
    }

    pub fn fill_ms(&self) -> f32 {
        self.playout
            .lock()
            .map(|p| p.fill_ms())
            .unwrap_or(0.0)
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
        self.playout
            .lock()
            .map(|p| p.target_ms())
            .unwrap_or(self.target_ms)
    }

    /// 运行期改缓冲目标（40–80 ms）。超出范围夹住，不报错 ——
    /// 滑块本来就只会给合法值，这里夹一下是防止未来别的调用方写坏。
    pub fn retarget(&mut self, ms: u32) {
        if let Ok(mut p) = self.playout.lock() {
            p.set_target_ms(ms);
        }
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    playout: Arc<Mutex<Playout>>,
    lock_misses: Arc<AtomicU64>,
) -> Result<cpal::Stream>
where
    T: cpal::Sample + cpal::SizedSample + cpal::FromSample<i16> + 'static,
{
    let channels = config.channels as usize;
    let mut scratch: Vec<i16> = Vec::new();
    let err_playout = playout.clone();
    // cpal 0.18 起 `build_output_stream` 按值收 `StreamConfig`（它是 `Copy`）。
    let config = *config;
    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [T], _info: &cpal::OutputCallbackInfo| {
                let mut pl = match playout.try_lock() {
                    Ok(g) => g,
                    // 主线程正在 push：这一轮先出静音，不能在这里等锁，否则会卡住音频线程。
                    // 但这同样是「本该有声却出了静音」，必须计入，否则统计会骗人。
                    Err(_) => {
                        lock_misses.fetch_add(1, Ordering::Relaxed);
                        for v in data.iter_mut() {
                            *v = T::from_sample(0i16);
                        }
                        return;
                    }
                };

                let frames = data.len() / channels;
                if scratch.len() != frames {
                    scratch.resize(frames, 0);
                }
                pl.fill(&mut scratch);
                for f in 0..frames {
                    let s = scratch[f];
                    for c in 0..channels {
                        data[f * channels + c] = T::from_sample(s);
                    }
                }
            },
            move |err| {
                eprintln!("声卡输出错误: {err}");
                if let Ok(mut pl) = err_playout.lock() {
                    pl.note_underrun();
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
            let name = device
                .description()
                .map(|d| d.name().to_string())
                .unwrap_or_default();
            if name.to_lowercase().contains(&name_substr.to_lowercase()) {
                matched = Some((device, name));
                break;
            }
        }
        let (device, device_name) = matched
            .ok_or_else(|| anyhow!("没有名字包含 \"{name_substr}\" 的输入设备（虚拟声卡要 2 进 2 出）"))?;

        let mut chosen: Option<cpal::SupportedStreamConfigRange> = None;
        for range in device.supported_input_configs().context("读取输入配置失败")? {
            let (lo, hi) = (range.min_sample_rate(), range.max_sample_rate());
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
            Some(range) => range.with_sample_rate(rate),
            None => device
                .default_input_config()
                .with_context(|| format!("输入侧 \"{device_name}\" 既不支持 {rate} Hz，也读不到默认配置"))?,
        };
        let sample_format = supported.sample_format();
        let config = supported.config();
        let channels = config.channels;
        let actual_rate = config.sample_rate;

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
    let config = *config;
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

#[cfg(test)]
mod tests {
    /// `cpal::Stream` 必须是 `Send` —— 这是 KI-004 能否关闭的判据。
    ///
    /// 0.15 里它是 `!Send`，所以声卡对象**只能**在创建它的线程里活一辈子，
    /// `crates/app/src/session.rs` 的"声卡独占一条命名线程"是被迫的设计。
    /// 0.17 起 macOS 的 `Stream` 拿了 `Send`，这里把它钉住：
    /// 将来谁把 cpal 降回旧版本，这条会在编译期就炸，而不是等到有人想跨线程用才发现。
    #[test]
    fn cpal_stream_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<cpal::Stream>();
    }
}
