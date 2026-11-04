//! IMA-ADPCM（DVI4）编解码。
//!
//! 与 `device/main/protocol.c` 逐字节一致：整数运算顺序、clamp 位置、nibble 打包顺序
//! 都相同。改这里必须同步改固件，否则 `tests/c_vectors.rs` 会挂 —— 这正是它存在的意义。
//!
//! 块头 5 字节：`seq u16-LE`、`predictor i16-LE`、`index u8`。
//! 块头的 predictor/index 是**本块编码开始前**的状态：编码器跨块继承，解码器每块
//! 由块头重置，所以丢一块不会污染后续音频。

use crate::AUDIO_BLOCK_HEADER_LEN;

const STEP_TABLE: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408,
    449, 494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066,
    2272, 2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630,
    9493, 10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794,
    32767,
];

const INDEX_TABLE: [i8; 8] = [-1, -1, -1, -1, 2, 4, 6, 8];

const MAX_INDEX: i32 = 88;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AdpcmState {
    pub predictor: i16,
    pub index: i8,
}

impl AdpcmState {
    pub fn new(predictor: i16, index: i8) -> Self {
        Self { predictor, index: index.clamp(0, MAX_INDEX as i8) }
    }
}

fn clamp_predictor(value: i32) -> i16 {
    value.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

fn clamp_index(value: i32) -> i8 {
    value.clamp(0, MAX_INDEX) as i8
}

fn encode_nibble(state: &mut AdpcmState, sample: i16) -> u8 {
    let step = STEP_TABLE[state.index as usize];

    let mut diff = sample as i32 - state.predictor as i32;
    let sign = diff < 0;
    if sign {
        diff = -diff;
    }

    let mut vpdiff = step >> 3;
    let mut delta = 0u8;
    if diff >= step {
        delta |= 4;
        diff -= step;
        vpdiff += step;
    }
    let step = step >> 1;
    if diff >= step {
        delta |= 2;
        diff -= step;
        vpdiff += step;
    }
    let step = step >> 1;
    if diff >= step {
        delta |= 1;
        vpdiff += step;
    }

    let predictor = state.predictor as i32 + if sign { -vpdiff } else { vpdiff };
    state.predictor = clamp_predictor(predictor);
    state.index = clamp_index(state.index as i32 + INDEX_TABLE[delta as usize] as i32);

    if sign {
        delta | 8
    } else {
        delta
    }
}

fn decode_nibble(state: &mut AdpcmState, nibble: u8) -> i16 {
    let step = STEP_TABLE[state.index as usize];

    let mut vpdiff = step >> 3;
    if nibble & 4 != 0 {
        vpdiff += step;
    }
    if nibble & 2 != 0 {
        vpdiff += step >> 1;
    }
    if nibble & 1 != 0 {
        vpdiff += step >> 2;
    }

    let predictor = state.predictor as i32 + if nibble & 8 != 0 { -vpdiff } else { vpdiff };
    state.predictor = clamp_predictor(predictor);
    state.index = clamp_index(state.index as i32 + INDEX_TABLE[(nibble & 7) as usize] as i32);

    state.predictor
}

/// 编码一块。返回 `5 + pcm.len()/2` 字节，`seq` 位留 0 由调用方填。
///
/// `pcm` 长度必须是偶数 —— 一个字节装两个 nibble。
pub fn encode_block(state: &mut AdpcmState, pcm: &[i16]) -> Vec<u8> {
    assert!(pcm.len().is_multiple_of(2), "PCM 采样数必须是偶数，才能按 nibble 打包");

    let mut out = vec![0u8; AUDIO_BLOCK_HEADER_LEN + pcm.len() / 2];
    // 块头写的是【编码前】的状态。
    out[2..4].copy_from_slice(&state.predictor.to_le_bytes());
    out[4] = state.index as u8;

    for (i, pair) in pcm.chunks_exact(2).enumerate() {
        let lo = encode_nibble(state, pair[0]);
        let hi = encode_nibble(state, pair[1]);
        // 低 nibble 放偶数号采样，高 nibble 放奇数号采样。
        out[AUDIO_BLOCK_HEADER_LEN + i] = (lo & 0x0F) | ((hi & 0x0F) << 4);
    }
    out
}

/// 解码一块到 `out`（长度即采样数）。块头重置解码状态。
pub fn decode_into(block: &[u8], out: &mut [i16]) {
    assert!(block.len() >= AUDIO_BLOCK_HEADER_LEN, "块长不足 5 字节");
    assert!(
        block.len() >= AUDIO_BLOCK_HEADER_LEN + out.len() / 2,
        "块数据不足以解出 {} 个采样",
        out.len()
    );

    let predictor = i16::from_le_bytes([block[2], block[3]]);
    let index = block[4] as i8;
    let mut state = AdpcmState { predictor, index: index.clamp(0, MAX_INDEX as i8) };

    for (i, chunk) in out.chunks_exact_mut(2).enumerate() {
        let byte = block[AUDIO_BLOCK_HEADER_LEN + i];
        chunk[0] = decode_nibble(&mut state, byte & 0x0F);
        chunk[1] = decode_nibble(&mut state, (byte >> 4) & 0x0F);
    }
}

/// 解码一块，返回 `samples` 个采样。
pub fn decode_block(block: &[u8], samples: usize) -> Vec<i16> {
    let mut out = vec![0i16; samples];
    decode_into(block, &mut out);
    out
}

/// 从块头读出 `(seq, 编码前状态)`。链路层用它检测丢块并决定是否重置解码器。
pub fn block_header(block: &[u8]) -> Option<(u16, AdpcmState)> {
    if block.len() < AUDIO_BLOCK_HEADER_LEN {
        return None;
    }
    let seq = u16::from_le_bytes([block[0], block[1]]);
    let predictor = i16::from_le_bytes([block[2], block[3]]);
    let index = block[4] as i8;
    Some((seq, AdpcmState { predictor, index }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AUDIO_BLOCK_LEN;
    use crate::AUDIO_BLOCK_SAMPLES;

    fn sine_block(freq: f64, amp: f64, offset: usize) -> Vec<i16> {
        (0..AUDIO_BLOCK_SAMPLES)
            .map(|i| {
                let t = (offset + i) as f64 / 16_000.0;
                (amp * 32767.0 * (2.0 * std::f64::consts::PI * freq * t).sin()).round() as i16
            })
            .collect()
    }

    fn snr_db(reference: &[i16], decoded: &[i16]) -> f64 {
        let mut signal = 0.0;
        let mut error = 0.0;
        for (r, d) in reference.iter().zip(decoded) {
            let s = *r as f64;
            let e = *d as f64 - s;
            signal += s * s;
            error += e * e;
        }
        if error == 0.0 {
            return 99.0;
        }
        10.0 * (signal / error).log10()
    }

    #[test]
    fn block_length_matches_contract() {
        let mut state = AdpcmState::default();
        let block = encode_block(&mut state, &sine_block(1000.0, 0.4, 0));
        assert_eq!(block.len(), AUDIO_BLOCK_LEN);
        assert_eq!(AUDIO_BLOCK_LEN, 165);
    }

    #[test]
    fn steady_state_snr_above_threshold() {
        let mut state = AdpcmState::default();
        let mut worst = 99.0f64;
        for k in 0..4 {
            let pcm = sine_block(1000.0, 0.4, k * AUDIO_BLOCK_SAMPLES);
            let block = encode_block(&mut state, &pcm);
            let decoded = decode_block(&block, AUDIO_BLOCK_SAMPLES);
            let snr = snr_db(&pcm, &decoded);
            if k >= 2 {
                worst = worst.min(snr);
            }
        }
        assert!(worst >= 18.0, "稳态 SNR 只有 {worst:.2} dB");
    }

    #[test]
    fn silence_converges() {
        let mut state = AdpcmState::default();
        let silent = vec![0i16; AUDIO_BLOCK_SAMPLES];
        let block = encode_block(&mut state, &silent);
        let decoded = decode_block(&block, AUDIO_BLOCK_SAMPLES);
        let max_abs = decoded.iter().map(|v| v.unsigned_abs()).max().unwrap();
        assert!(max_abs <= 8, "静音块解码出现 {max_abs} 的残留");
    }

    #[test]
    fn lost_block_does_not_poison_decoder() {
        let mut state = AdpcmState::default();
        let mut blocks = Vec::new();
        for k in 0..3 {
            let pcm = sine_block(800.0, 0.3, k * AUDIO_BLOCK_SAMPLES);
            blocks.push(encode_block(&mut state, &pcm));
        }

        let mut in_order = vec![0i16; AUDIO_BLOCK_SAMPLES];
        for block in &blocks {
            decode_into(block, &mut in_order);
        }
        // 前两块"丢了"：只解第三块。
        let only_third = decode_block(&blocks[2], AUDIO_BLOCK_SAMPLES);
        assert_eq!(in_order, only_third, "块头没有自同步，丢块会污染后续音频");
    }
}
