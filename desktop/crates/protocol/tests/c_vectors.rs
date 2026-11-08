//! 跨端对拍：读 `config/protocol-vectors.json`（由 `device/test/gen_vectors.c` 从
//! **C 实现**导出），逐字节比对 Rust 实现。
//!
//! 这不是"自己写期望值自己验"，而是两端共用一个事实来源。C 侧改协议没重生成向量，
//! 或 Rust 侧算法跑偏，这里都会红。

use apm_protocol::adpcm::{self, AdpcmState};
use apm_protocol::crc::crc16_ccitt;
use apm_protocol::frame;
use serde_json::Value;
use std::path::PathBuf;

fn vectors() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../config/protocol-vectors.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "读不到 {}: {e}\n先跑 ./scripts/gen-protocol-vectors.sh",
            path.display()
        )
    });
    serde_json::from_str(&text).expect("向量 JSON 解析失败")
}

fn hex_decode(hex: &str) -> Vec<u8> {
    assert!(hex.len().is_multiple_of(2), "hex 长度必须是偶数: {hex}");
    (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).expect("非法 hex"))
        .collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn schema_is_expected() {
    let v = vectors();
    assert_eq!(v["schema"], "ai-passport-mic-protocol-vectors-1");
}

#[test]
fn crc_matches_c() {
    let v = vectors();
    let cases = v["crc"].as_array().expect("crc 必须是数组");
    assert!(!cases.is_empty());
    for case in cases {
        let data = case["data"].as_str().unwrap().as_bytes();
        let expected = case["crc"].as_u64().unwrap() as u16;
        assert_eq!(
            crc16_ccitt(data),
            expected,
            "CRC 不一致: {:?}",
            case["data"].as_str().unwrap()
        );
    }
}

#[test]
fn frames_match_c_byte_for_byte() {
    let v = vectors();
    let cases = v["frames"].as_array().expect("frames 必须是数组");
    assert!(cases.len() >= 4);

    for case in cases {
        let name = case["name"].as_str().unwrap();
        let msg = case["msg"].as_u64().unwrap() as u8;
        let payload = hex_decode(case["payload_hex"].as_str().unwrap());
        let expected = hex_decode(case["frame_hex"].as_str().unwrap());

        let encoded = frame::encode(msg, &payload).unwrap();
        assert_eq!(
            hex_encode(&encoded),
            hex_encode(&expected),
            "[{name}] 帧编码与 C 不一致"
        );

        let (decoded, used) = frame::decode(&expected).unwrap();
        assert_eq!(used, expected.len(), "[{name}] 消耗字节数不对");
        assert_eq!(decoded.msg, msg, "[{name}] msg 不对");
        assert_eq!(decoded.payload, payload.as_slice(), "[{name}] payload 不对");
    }
}

#[test]
fn adpcm_matches_c_byte_for_byte() {
    let v = vectors();
    let cases = v["adpcm"].as_array().expect("adpcm 必须是数组");
    assert!(cases.len() >= 2, "至少要有两块才能验证跨块状态继承");

    // C 侧是按顺序连续编码的，所以这里也按顺序复用同一个 state。
    let mut state = AdpcmState::default();

    for case in cases {
        let index = case["index"].as_u64().unwrap();
        let before = &case["state_before"];
        assert_eq!(
            state,
            AdpcmState {
                predictor: before["predictor"].as_i64().unwrap() as i16,
                index: before["index"].as_i64().unwrap() as i8,
            },
            "块 {index} 的起始状态与 C 不一致（上一块编码结果跑偏了）"
        );

        let pcm: Vec<i16> = case["pcm"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_i64().unwrap() as i16)
            .collect();
        assert_eq!(pcm.len(), 320);

        let block = adpcm::encode_block(&mut state, &pcm);
        let expected_block = hex_decode(case["block_hex"].as_str().unwrap());
        assert_eq!(
            hex_encode(&block),
            hex_encode(&expected_block),
            "块 {index} 的 ADPCM 编码与 C 不一致"
        );

        let after = &case["state_after"];
        assert_eq!(
            state,
            AdpcmState {
                predictor: after["predictor"].as_i64().unwrap() as i16,
                index: after["index"].as_i64().unwrap() as i8,
            },
            "块 {index} 编码后的状态与 C 不一致"
        );

        let expected_decoded: Vec<i16> = case["decoded"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_i64().unwrap() as i16)
            .collect();
        let decoded = adpcm::decode_block(&expected_block, 320);
        assert_eq!(decoded, expected_decoded, "块 {index} 的解码结果与 C 不一致");
    }
}

#[test]
fn block_header_exposes_seq_and_state() {
    let v = vectors();
    let case = &v["adpcm"][1];
    let block = hex_decode(case["block_hex"].as_str().unwrap());
    let (seq, state) = adpcm::block_header(&block).unwrap();
    assert_eq!(seq, 0, "生成器没填 seq，应为 0");
    assert_eq!(
        state.predictor,
        case["state_before"]["predictor"].as_i64().unwrap() as i16
    );
    assert_eq!(state.index, case["state_before"]["index"].as_i64().unwrap() as i8);
}

/// `SET_KEY_LABELS`：C 侧编出的载荷与整帧，Rust 侧必须逐字节复现，
/// 且能从 C 的字节里解析回同一份内容。
#[test]
fn key_labels_match_c_byte_for_byte() {
    use apm_protocol::device_labels::{KeyLabel, KeyLabels, ScreenKey};

    let v = vectors();
    let case = v["frames"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "key_labels")
        .expect("向量里必须有 key_labels 这一项（跑 ./scripts/gen-protocol-vectors.sh）");

    let expected_payload = hex_decode(case["payload_hex"].as_str().unwrap());
    let expected_frame = hex_decode(case["frame_hex"].as_str().unwrap());

    // 用与 C 生成器完全相同的输入构造，编码结果必须一致。
    let labels = KeyLabels {
        pc_pid: 0x5A,
        capture_key: Some(ScreenKey::Ok),
        items: vec![
            KeyLabel { key: ScreenKey::Up, text: "ctrl+ent".into() },
            KeyLabel { key: ScreenKey::Down, text: "esc*2".into() },
        ],
    };
    let payload = labels.encode().expect("encode");
    assert_eq!(
        hex_encode(&payload),
        hex_encode(&expected_payload),
        "SET_KEY_LABELS 载荷与 C 不一致"
    );
    assert_eq!(
        hex_encode(&labels.encode_frame().expect("encode_frame")),
        hex_encode(&expected_frame),
        "SET_KEY_LABELS 整帧与 C 不一致"
    );

    // 反向：从 C 的字节解析回来必须等价。
    let decoded = KeyLabels::decode(&expected_payload).expect("decode");
    assert_eq!(decoded, labels, "从 C 的载荷解析出的内容与 Rust 侧不一致");
}
