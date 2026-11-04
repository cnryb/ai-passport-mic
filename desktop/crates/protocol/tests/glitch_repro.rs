//! 满量程毛刺的复现与归因。
//!
//! 目的不是"证明修好了"，而是钉住一条结论：**ADPCM 编解码本身不会产生 0.2–1 s
//! 的 ±32767 逐样本变号**。所以真机上那个现象来自采集端（I2S / ES8311 瞬态读出）
//! 被原样编码上链路，而不是协议层算错。
//!
//! 结论变了就要改 `docs/SOUND_QUALITY.md` 的判断，不要只改这个文件。

use apm_protocol::adpcm::{self, AdpcmState};
use apm_protocol::glitch::{self, Suppressor};
use apm_protocol::AUDIO_BLOCK_SAMPLES;

fn tone(n: usize, amp: f64) -> Vec<i16> {
    (0..n)
        .map(|i| {
            let t = i as f64 / 16_000.0;
            (amp * (2.0 * std::f64::consts::PI * 300.0 * t).sin()) as i16
        })
        .collect()
}

/// 用「编码 → 解码」走一遍，返回每块里满量程样本数。
fn roundtrip(pcm: &[i16]) -> Vec<usize> {
    let mut state = AdpcmState::default();
    let mut out = Vec::new();
    for block in pcm.chunks_exact(AUDIO_BLOCK_SAMPLES) {
        let wire = adpcm::encode_block(&mut state, block);
        let mut decoded = vec![0i16; AUDIO_BLOCK_SAMPLES];
        adpcm::decode_into(&wire, &mut decoded);
        out.push(decoded.iter().filter(|v| (**v as i32).abs() >= 32_000).count());
    }
    out
}

/// 30 ms 的满量程削波瞬态（麦克风 pop / 增益切换都会这样）之后接低电平底噪：
/// 编码器最多污染相邻一两块就恢复，不会持续 0.8 s。
#[test]
fn codec_recovers_right_after_clipping_transient() {
    let mut pcm = tone(16_000, 200.0); // 1 s 底噪
    for i in 0..(16 * 30) {
        pcm.push(if i % 2 == 0 { 32_767 } else { -32_768 });
    }
    pcm.extend(tone(32_000, 200.0)); // 之后 2 s

    let bad: Vec<usize> = roundtrip(&pcm)
        .into_iter()
        .enumerate()
        .filter(|(_, n)| *n > 32) // 一块里超过 2 ms 满量程才算病态
        .map(|(i, _)| i)
        .collect();

    assert!(
        bad.len() <= 3,
        "编解码自身产生了持续毛刺（{} 块），归因结论要重写：{bad:?}",
        bad.len()
    );
}

/// 即使真收到这样一段垃圾（模拟采集端故障），抑制器也要把它从听感上抹掉。
#[test]
fn suppressor_cleans_observed_pattern() {
    let mut pcm = tone(16_000, 300.0);
    for i in 0..12_800 {
        // 0.8 s，与真机观察到的时长一致
        pcm.push(if i % 2 == 0 { 32_767 } else { -32_768 });
    }
    pcm.extend(tone(16_000, 300.0));

    let mut sup = Suppressor::new();
    sup.process(&mut pcm);

    assert_eq!(sup.events, 1);
    // 判定需要攒够 MIN_RUN 个样本才触发，所以开头最多漏 MIN_RUN-1 个（0.4 ms，
    // 听不出来）。除此之外不允许再有满量程样本。
    let residual = pcm.iter().filter(|v| (**v as i32).abs() >= 32_000).count();
    assert!(
        residual < glitch::MIN_RUN,
        "抑制后仍有 {residual} 个满量程样本（应 < {}）",
        glitch::MIN_RUN
    );
    assert!(
        (sup.muted_samples as i64 - 12_800).abs() < 200,
        "静音长度偏离病态段：{}",
        sup.muted_samples
    );
}

/// 扫描口径：同一个文件既能被抑制，也能在报告里被量化。
#[test]
fn scan_quantifies_the_same_region() {
    let mut pcm = vec![0i16; 800];
    for i in 0..800 {
        pcm.push(if i % 2 == 0 { 32_767 } else { -32_768 });
    }
    pcm.extend(tone(800, 200.0));

    let s = glitch::scan(&pcm);
    assert_eq!(s.events, 1);
    assert!(s.duration_ms(16_000) > 40.0, "量出来的时长 {} ms", s.duration_ms(16_000));
}
