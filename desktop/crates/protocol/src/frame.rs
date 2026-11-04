//! 控制帧：`[0x56 0x4D][version][type][len u16-LE][payload][CRC16 u16-LE]`。
//!
//! CRC 覆盖 `version` 到 payload 末尾，**不含**两字节 magic。

use crate::crc::crc16_ccitt;
use std::fmt;

pub const MAGIC: [u8; 2] = [0x56, 0x4D];
pub const VERSION: u8 = 1;
/// magic(2) + version(1) + type(1) + len(2)
pub const PREFIX_LEN: usize = 6;
pub const CRC_LEN: usize = 2;
pub const MAX_PAYLOAD: usize = 64;
pub const MIN_LEN: usize = PREFIX_LEN + CRC_LEN;
pub const MAX_LEN: usize = MAX_PAYLOAD + MIN_LEN;

/// 帧类型。`0x01-0x7F` 是 PC → 设备，`0x81-0xFF` 是设备 → PC。
pub mod msg {
    pub const START: u8 = 0x01;
    pub const STOP: u8 = 0x02;
    pub const SET_GAIN: u8 = 0x03;
    pub const STATUS: u8 = 0x81;
    pub const EVENT: u8 = 0x82;
    pub const PONG: u8 = 0x83;

    pub fn name(t: u8) -> &'static str {
        match t {
            START => "START",
            STOP => "STOP",
            SET_GAIN => "SET_GAIN",
            STATUS => "STATUS",
            EVENT => "EVENT",
            PONG => "PONG",
            _ => "UNKNOWN",
        }
    }
}

/// STATUS.payload[0]
pub mod state {
    pub const IDLE: u8 = 0;
    pub const READY: u8 = 1;
    pub const CAPTURING: u8 = 2;
    pub const ERROR: u8 = 3;

    pub fn name(s: u8) -> &'static str {
        match s {
            IDLE => "idle",
            READY => "ready",
            CAPTURING => "capturing",
            ERROR => "error",
            _ => "unknown",
        }
    }
}

/// STATUS.payload[1] 的位。
pub const FLAG_BLE_CONNECTED: u8 = 0x01;
pub const FLAG_AUDIO_SUBSCRIBED: u8 = 0x02;
pub const FLAG_PTT_HELD: u8 = 0x04;
pub const FLAG_AUDIO_ERROR: u8 = 0x08;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    Short,
    Magic,
    Version(u8),
    Len,
    Crc,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Short => write!(f, "帧长度不足"),
            FrameError::Magic => write!(f, "magic 不匹配（期望 56 4D）"),
            FrameError::Version(v) => write!(f, "协议版本不支持: {v}（期望 {VERSION}）"),
            FrameError::Len => write!(f, "声明长度越界或缓冲区不足"),
            FrameError::Crc => write!(f, "CRC 校验失败"),
        }
    }
}

impl std::error::Error for FrameError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame<'a> {
    pub version: u8,
    pub msg: u8,
    pub payload: &'a [u8],
}

/// 编码一帧。`payload` 超过 [`MAX_PAYLOAD`] 返回 [`FrameError::Len`]。
pub fn encode(msg: u8, payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    if payload.len() > MAX_PAYLOAD {
        return Err(FrameError::Len);
    }
    let mut out = Vec::with_capacity(MIN_LEN + payload.len());
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.push(msg);
    out.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    out.extend_from_slice(payload);

    let crc = crc16_ccitt(&out[2..]);
    out.extend_from_slice(&crc.to_le_bytes());
    Ok(out)
}

/// 解码一帧，返回 `(帧, 消耗字节数)`。
///
/// 允许 `buf` 后面还有别的数据（BLE 通知里一般不会），所以返回消耗长度。
pub fn decode(buf: &[u8]) -> Result<(Frame<'_>, usize), FrameError> {
    if buf.len() < MIN_LEN {
        return Err(FrameError::Short);
    }
    if buf[0] != MAGIC[0] || buf[1] != MAGIC[1] {
        return Err(FrameError::Magic);
    }
    if buf[2] != VERSION {
        return Err(FrameError::Version(buf[2]));
    }
    let payload_len = u16::from_le_bytes([buf[4], buf[5]]) as usize;
    if payload_len > MAX_PAYLOAD {
        return Err(FrameError::Len);
    }
    let total = MIN_LEN + payload_len;
    if buf.len() < total {
        return Err(FrameError::Len);
    }
    let crc_calc = crc16_ccitt(&buf[2..PREFIX_LEN + payload_len]);
    let crc_wire = u16::from_le_bytes([buf[PREFIX_LEN + payload_len], buf[PREFIX_LEN + payload_len + 1]]);
    if crc_calc != crc_wire {
        return Err(FrameError::Crc);
    }
    Ok((
        Frame {
            version: buf[2],
            msg: buf[3],
            payload: &buf[PREFIX_LEN..PREFIX_LEN + payload_len],
        },
        total,
    ))
}

/// STATUS 的 8 字节载荷。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Status {
    pub state: u8,
    pub flags: u8,
    pub dropped: u16,
    pub sent: u32,
}

impl Status {
    pub fn encode(&self) -> [u8; 8] {
        let mut out = [0u8; 8];
        out[0] = self.state;
        out[1] = self.flags;
        out[2..4].copy_from_slice(&self.dropped.to_le_bytes());
        out[4..8].copy_from_slice(&self.sent.to_le_bytes());
        out
    }

    pub fn decode(payload: &[u8]) -> Option<Self> {
        if payload.len() < 8 {
            return None;
        }
        Some(Status {
            state: payload[0],
            flags: payload[1],
            dropped: u16::from_le_bytes([payload[2], payload[3]]),
            sent: u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]),
        })
    }

    pub fn ble_connected(&self) -> bool {
        self.flags & FLAG_BLE_CONNECTED != 0
    }
    pub fn audio_subscribed(&self) -> bool {
        self.flags & FLAG_AUDIO_SUBSCRIBED != 0
    }
    pub fn ptt_held(&self) -> bool {
        self.flags & FLAG_PTT_HELD != 0
    }
    pub fn audio_error(&self) -> bool {
        self.flags & FLAG_AUDIO_ERROR != 0
    }
}

/// EVENT 的 3 字节载荷。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    pub button: u8,
    pub event: u8,
    pub flags: u8,
}

impl Event {
    pub fn encode(&self) -> [u8; 3] {
        [self.button, self.event, self.flags]
    }

    pub fn decode(payload: &[u8]) -> Option<Self> {
        if payload.len() < 3 {
            return None;
        }
        Some(Event {
            button: payload[0],
            event: payload[1],
            flags: payload[2],
        })
    }

    pub fn button_name(&self) -> &'static str {
        match self.button {
            0 => "up",
            1 => "down",
            2 => "ok",
            _ => "unknown",
        }
    }

    /// 与 `config/README.md` 的事件名对应：click → short。
    pub fn event_name(&self) -> &'static str {
        match self.event {
            0 => "press",
            1 => "short",
            2 => "double",
            3 => "long",
            4 => "release",
            _ => "unknown",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_empty_payload() {
        let bytes = encode(msg::START, &[]).unwrap();
        assert_eq!(bytes.len(), MIN_LEN);
        let (frame, used) = decode(&bytes).unwrap();
        assert_eq!(used, bytes.len());
        assert_eq!(frame.msg, msg::START);
        assert!(frame.payload.is_empty());
    }

    #[test]
    fn roundtrip_status() {
        let status = Status {
            state: state::CAPTURING,
            flags: FLAG_BLE_CONNECTED | FLAG_AUDIO_SUBSCRIBED,
            dropped: 7,
            sent: 123_456,
        };
        let bytes = encode(msg::STATUS, &status.encode()).unwrap();
        let (frame, _) = decode(&bytes).unwrap();
        let decoded = Status::decode(frame.payload).unwrap();
        assert_eq!(decoded, status);
    }

    #[test]
    fn rejects_bad_crc() {
        let mut bytes = encode(msg::START, &[]).unwrap();
        bytes[3] ^= 0xFF; // 改 type，不动 CRC
        assert_eq!(decode(&bytes), Err(FrameError::Crc));
    }

    #[test]
    fn rejects_bad_magic_and_version() {
        let mut bytes = encode(msg::START, &[]).unwrap();
        bytes[1] = 0;
        assert_eq!(decode(&bytes), Err(FrameError::Magic));

        let mut bytes = encode(msg::START, &[]).unwrap();
        bytes[2] = 2;
        assert_eq!(decode(&bytes), Err(FrameError::Version(2)));
    }

    #[test]
    fn rejects_truncated_and_oversized() {
        let bytes = encode(msg::START, &[]).unwrap();
        assert_eq!(decode(&bytes[..3]), Err(FrameError::Short));

        let long = vec![0u8; MAX_PAYLOAD + 1];
        assert_eq!(encode(msg::START, &long), Err(FrameError::Len));
    }
}
