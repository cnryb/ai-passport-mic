//! 最小 WAV 读写：只支持 PCM 16-bit。拒绝而不是"猜"其他格式。
//!
//! 自己写而不引依赖，是因为这里只服务 `apmctl record` / `apmctl analyze`，
//! 需要的是可控的报错文案，不是格式覆盖度。

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wav {
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: Vec<i16>,
}

#[derive(Debug)]
pub enum WavError {
    Io(io::Error),
    NotRiff,
    NotWave,
    /// 只做 PCM 16-bit；其他编码明确拒绝，不猜。
    UnsupportedFormat { format: u16, bits: u16 },
    MissingChunk(&'static str),
    Truncated,
}

impl std::fmt::Display for WavError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WavError::Io(e) => write!(f, "读写失败: {e}"),
            WavError::NotRiff => write!(f, "不是 RIFF 文件"),
            WavError::NotWave => write!(f, "RIFF 里不是 WAVE"),
            WavError::UnsupportedFormat { format, bits } => write!(
                f,
                "只支持 PCM 16-bit，实际 format={format} bits={bits}（先用 ffmpeg 转成 pcm_s16le）"
            ),
            WavError::MissingChunk(name) => write!(f, "缺少 {name} chunk"),
            WavError::Truncated => write!(f, "文件被截断"),
        }
    }
}

impl std::error::Error for WavError {}

impl From<io::Error> for WavError {
    fn from(e: io::Error) -> Self {
        WavError::Io(e)
    }
}

fn read_u16(buf: &[u8], at: usize) -> Result<u16, WavError> {
    buf.get(at..at + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or(WavError::Truncated)
}

fn read_u32(buf: &[u8], at: usize) -> Result<u32, WavError> {
    buf.get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or(WavError::Truncated)
}

pub fn read_wav(path: &Path) -> Result<Wav, WavError> {
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;

    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" {
        return Err(WavError::NotRiff);
    }
    if &bytes[8..12] != b"WAVE" {
        return Err(WavError::NotWave);
    }

    let mut pos = 12usize;
    let mut sample_rate = 0u32;
    let mut channels = 0u16;
    let mut bits = 0u16;
    let mut format = 0u16;
    let mut data: Option<&[u8]> = None;

    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = read_u32(&bytes, pos + 4)? as usize;
        let body_start = pos + 8;
        let body_end = body_start.checked_add(size).ok_or(WavError::Truncated)?;
        if body_end > bytes.len() {
            return Err(WavError::Truncated);
        }
        let body = &bytes[body_start..body_end];

        match id {
            b"fmt " => {
                format = read_u16(body, 0)?;
                channels = read_u16(body, 2)?;
                sample_rate = read_u32(body, 4)?;
                bits = read_u16(body, 14)?;
            }
            b"data" => data = Some(body),
            _ => {}
        }
        // chunk 按偶数字节对齐
        pos = body_end + (size & 1);
    }

    if channels == 0 || sample_rate == 0 {
        return Err(WavError::MissingChunk("fmt "));
    }
    if format != 1 || bits != 16 {
        return Err(WavError::UnsupportedFormat { format, bits });
    }
    let data = data.ok_or(WavError::MissingChunk("data"))?;

    let samples = data
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();

    Ok(Wav { sample_rate, channels, samples })
}

pub fn write_wav(
    path: &Path,
    sample_rate: u32,
    channels: u16,
    samples: &[i16],
) -> Result<(), WavError> {
    let data_len = (samples.len() * 2) as u32;
    let byte_rate = sample_rate * channels as u32 * 2;
    let block_align = channels * 2;

    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes()); // bits
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }

    let mut file = File::create(path)?;
    file.write_all(&out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_roundtrip() {
        let dir = std::env::temp_dir().join("apm-wav-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("roundtrip.wav");

        let samples: Vec<i16> = (0..320).map(|i| (i * 100 - 16_000) as i16).collect();
        write_wav(&path, 16_000, 1, &samples).unwrap();

        let wav = read_wav(&path).unwrap();
        assert_eq!(wav.sample_rate, 16_000);
        assert_eq!(wav.channels, 1);
        assert_eq!(wav.samples, samples);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_non_riff() {
        let dir = std::env::temp_dir().join("apm-wav-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("not-riff.wav");
        std::fs::write(&path, b"hello world, not a wav").unwrap();
        assert!(matches!(read_wav(&path), Err(WavError::NotRiff)));
        let _ = std::fs::remove_file(&path);
    }
}
