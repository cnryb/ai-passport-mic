//! `SET_KEY_LABELS`（`0x05`）：设备屏幕上的按键标签。
//!
//! 契约见 `docs/ARCHITECTURE.md` 的同名小节，固件侧实现在 `device/main/protocol.c`
//! 的 `apm_key_labels_decode` / `apm_key_labels_encode`。两端必须逐字节一致，
//! 由 `tests/c_vectors.rs` 用同一份向量钉住。
//!
//! 为什么这个职责在 PC 侧：**映射表在 PC**，固件看不到 UP 被映射成了 `ctrl+enter`。
//! 设备只负责显示文本，不做键名翻译——有第二份翻译表就必然会漂移。
//!
//! 载荷：
//! ```text
//! [0] pc_pid      u8
//! [1] capture_btn u8   0=up 1=down 2=ok 0xFF=未知
//! [2] count       u8   0..=3
//! [3..] 每条： [btn u8][text_len u8][text: UTF-8]
//! ```

use crate::frame::{self, FrameError};

/// 单条标签的字节上限，与固件的 `APM_KEY_LABEL_MAX_BYTES` 一致。
pub const MAX_LABEL_BYTES: usize = 24;
/// 最多三条（up / down / ok）。
pub const MAX_ITEMS: usize = 3;
/// `capture_btn` 用这个值表示"未知"。
pub const NO_CAPTURE: u8 = 0xFF;

/// 三个功能键。编号与协议 `apm_btn_t` 一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenKey {
    Up = 0,
    Down = 1,
    Ok = 2,
}

impl ScreenKey {
    pub fn from_index(i: u8) -> Option<Self> {
        match i {
            0 => Some(ScreenKey::Up),
            1 => Some(ScreenKey::Down),
            2 => Some(ScreenKey::Ok),
            _ => None,
        }
    }

    /// 键位映射里的事件名（`config/README.md` 的 `bindings` 键）。
    pub fn event_name(self) -> &'static str {
        match self {
            ScreenKey::Up => "up",
            ScreenKey::Down => "down",
            ScreenKey::Ok => "ok",
        }
    }

    /// 屏幕左侧那一列的键名（纯 ASCII，固件字库固定收录）。
    pub fn label(self) -> &'static str {
        match self {
            ScreenKey::Up => "UP",
            ScreenKey::Down => "DOWN",
            ScreenKey::Ok => "OK",
        }
    }

    pub fn all() -> [ScreenKey; 3] {
        [ScreenKey::Up, ScreenKey::Down, ScreenKey::Ok]
    }
}

/// 一条标签：键 + 文本（UTF-8 字节，不保证是完整字符边界的截断由调用方负责）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyLabel {
    pub key: ScreenKey,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeyLabels {
    pub pc_pid: u8,
    /// 本机开麦键。`None` = 未知（设备按默认 `ok` 处理）。
    pub capture_key: Option<ScreenKey>,
    /// 最多 [`MAX_ITEMS`] 条，同一个键只能出现一次。
    pub items: Vec<KeyLabel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelError {
    /// 载荷不足 / 声明长度超出实际 / 尾部有多余字节。
    Malformed(&'static str),
    /// 条目数超过 3。
    TooManyItems(usize),
    /// 同一个键出现两次。
    DuplicateKey(ScreenKey),
    /// 单条文本超过 [`MAX_LABEL_BYTES`]，或帧装不下。
    TooLong { key: ScreenKey, bytes: usize },
}

impl std::fmt::Display for LabelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LabelError::Malformed(w) => write!(f, "标签载荷格式错误：{w}"),
            LabelError::TooManyItems(n) => {
                write!(f, "标签条目 {n} 条超过上限 {MAX_ITEMS}")
            }
            LabelError::DuplicateKey(k) => write!(f, "键 {} 在标签里出现了两次", k.label()),
            LabelError::TooLong { key, bytes } => write!(
                f,
                "键 {} 的标签 {bytes} 字节超过上限 {MAX_LABEL_BYTES}",
                key.label()
            ),
        }
    }
}

impl std::error::Error for LabelError {}

impl KeyLabels {
    /// 组装载荷。返回的字节可直接交给 `frame::encode(msg::SET_KEY_LABELS, ...)`。
    pub fn encode(&self) -> Result<Vec<u8>, LabelError> {
        if self.items.len() > MAX_ITEMS {
            return Err(LabelError::TooManyItems(self.items.len()));
        }
        let mut seen = Vec::with_capacity(self.items.len());
        for it in &self.items {
            if seen.contains(&it.key) {
                return Err(LabelError::DuplicateKey(it.key));
            }
            seen.push(it.key);
            let bytes = it.text.len();
            if bytes > MAX_LABEL_BYTES {
                return Err(LabelError::TooLong { key: it.key, bytes });
            }
        }

        let mut out = Vec::with_capacity(3 + self.items.len() * 2 + MAX_LABEL_BYTES);
        out.push(self.pc_pid);
        out.push(match self.capture_key {
            Some(k) => k as u8,
            None => NO_CAPTURE,
        });
        out.push(self.items.len() as u8);
        for it in &self.items {
            let text = it.text.as_bytes();
            out.push(it.key as u8);
            out.push(text.len() as u8);
            out.extend_from_slice(text);
        }
        Ok(out)
    }

    /// 组装成完整控制帧。
    pub fn encode_frame(&self) -> Result<Vec<u8>, LabelError> {
        let payload = self.encode()?;
        frame::encode(frame::msg::SET_KEY_LABELS, &payload).map_err(|e| match e {
            FrameError::Len => LabelError::Malformed("载荷超过控制帧上限"),
            _ => LabelError::Malformed("帧编码失败"),
        })
    }

    /// 解析载荷。**任何结构不符都整条拒绝**，不做部分应用 ——
    /// 半份标签比没有标签更容易让人误判。
    pub fn decode(payload: &[u8]) -> Result<Self, LabelError> {
        if payload.len() < 3 {
            return Err(LabelError::Malformed("载荷不足 3 字节"));
        }
        let pc_pid = payload[0];
        let capture_key = ScreenKey::from_index(payload[1]);
        let count = payload[2] as usize;
        if count > MAX_ITEMS {
            return Err(LabelError::TooManyItems(count));
        }

        let mut items = Vec::with_capacity(count);
        let mut pos = 3usize;
        for _ in 0..count {
            if pos + 2 > payload.len() {
                return Err(LabelError::Malformed("条目头被截断"));
            }
            let key = ScreenKey::from_index(payload[pos])
                .ok_or(LabelError::Malformed("非法键号"))?;
            let tlen = payload[pos + 1] as usize;
            pos += 2;
            if pos + tlen > payload.len() {
                return Err(LabelError::Malformed("文本长度超出实际载荷"));
            }
            if tlen > MAX_LABEL_BYTES {
                return Err(LabelError::TooLong { key, bytes: tlen });
            }
            if items.iter().any(|i: &KeyLabel| i.key == key) {
                return Err(LabelError::DuplicateKey(key));
            }
            let text = String::from_utf8_lossy(&payload[pos..pos + tlen]).into_owned();
            items.push(KeyLabel { key, text });
            pos += tlen;
        }
        if pos != payload.len() {
            return Err(LabelError::Malformed("尾部有多余字节"));
        }
        Ok(KeyLabels { pc_pid, capture_key, items })
    }
}

// ---------------------------------------------------------------------------
// 从 keymap 生成标签
// ---------------------------------------------------------------------------
/// 把一条映射动作翻成屏幕上的一句话。
///
/// 规则（与 `docs/ARCHITECTURE.md` 的 `SET_KEY_LABELS` 节一致）：
/// - `ptt-toggle` → 「开麦 关麦」（这是开麦键）
/// - 其余动作 → **键名照抄 keymap 原文**（`enter` / `ctrl+enter` / `f5` / `lalt`），
///   不做中文翻译。有第二份键名翻译表就必然会与 `config/README.md` 漂移。
/// - `repeat > 1` 追加 `xN`（ASCII 的 `x`，不是 `×`，两者都在字库里，但 `x` 更省宽）。
///
/// 超过 [`MAX_LABEL_BYTES`] 时**在字符边界上截断**：切出半个汉字的话，
/// 设备端会把它当成非法 UTF-8 换成 `?`，屏幕上就是一句读不懂的话。
pub fn describe_binding_for_screen(binding: &crate::keymap::Binding) -> String {
    let mut text = if binding.is_ptt() {
        "开麦 关麦".to_string()
    } else {
        let mut s = String::new();
        for m in &binding.modifiers {
            s.push_str(m.as_str());
            s.push('+');
        }
        s.push_str(&binding.key);
        s
    };
    if binding.repeat > 1 {
        text.push_str(&format!(" x{}", binding.repeat));
    }
    truncate_chars(&text, MAX_LABEL_BYTES)
}

/// 按 UTF-8 字符边界截断到 `max_bytes` 以内。
pub fn truncate_chars(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// 从整份映射生成三条屏幕标签。
///
/// 未绑定的键给「无」——屏幕上留白会让人以为"这一行坏了"。
/// `capture_key` 取 `ptt_event()`；解析通过时它必然恰好有一个。
pub fn labels_from_keymap(map: &crate::keymap::KeyMap, pc_pid: u8) -> KeyLabels {
    let capture_event = map.ptt_event();
    let capture_key = capture_event.and_then(|e| match e {
        "up" => Some(ScreenKey::Up),
        "down" => Some(ScreenKey::Down),
        "ok" => Some(ScreenKey::Ok),
        _ => None,
    });

    let items = ScreenKey::all()
        .into_iter()
        .map(|key| {
            let text = match map.resolve(key.event_name()) {
                Some(b) => describe_binding_for_screen(b),
                None => "无".to_string(),
            };
            KeyLabel { key, text }
        })
        .collect();

    KeyLabels { pc_pid, capture_key, items }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{Binding, KeyMap, Modifier};

    #[test]
    fn roundtrip() {
        let labels = KeyLabels {
            pc_pid: 0x5A,
            capture_key: Some(ScreenKey::Ok),
            items: vec![
                KeyLabel { key: ScreenKey::Up, text: "ctrl+enter".into() },
                KeyLabel { key: ScreenKey::Down, text: "esc x2".into() },
                KeyLabel { key: ScreenKey::Ok, text: "开麦 关麦".into() },
            ],
        };
        let payload = labels.encode().expect("encode");
        assert_eq!(KeyLabels::decode(&payload).expect("decode"), labels);
    }

    #[test]
    fn empty_is_valid() {
        // count=0 是合法的：表示"清空成设备默认文案"。
        let labels = KeyLabels { pc_pid: 1, capture_key: None, items: vec![] };
        let payload = labels.encode().expect("encode");
        assert_eq!(payload, vec![1, NO_CAPTURE, 0]);
        assert_eq!(KeyLabels::decode(&payload).expect("decode").items.len(), 0);
    }

    #[test]
    fn rejects_malformed() {
        assert!(KeyLabels::decode(&[0, 0]).is_err(), "不足 3 字节");
        assert!(KeyLabels::decode(&[0, 0, 4]).is_err(), "count 超限");
        assert!(KeyLabels::decode(&[0, 0, 1, 9, 0]).is_err(), "非法键号");
        assert!(KeyLabels::decode(&[0, 0, 1, 0, 5, b'a']).is_err(), "文本长度超出载荷");
        assert!(KeyLabels::decode(&[0, 0, 0, 0xFF]).is_err(), "尾部多余字节");
        assert!(
            KeyLabels::decode(&[0, 0, 2, 0, 1, b'a', 0, 1, b'b']).is_err(),
            "重复键号"
        );
    }

    #[test]
    fn rejects_too_long_text() {
        let long = "x".repeat(MAX_LABEL_BYTES + 1);
        let labels = KeyLabels {
            pc_pid: 0,
            capture_key: None,
            items: vec![KeyLabel { key: ScreenKey::Up, text: long }],
        };
        assert!(labels.encode().is_err(), "编码拒绝超长文本");

        // 解码侧同样拒绝。
        let mut payload = vec![0, NO_CAPTURE, 1, 0, (MAX_LABEL_BYTES + 1) as u8];
        payload.extend(std::iter::repeat_n(b'x', MAX_LABEL_BYTES + 1));
        assert!(KeyLabels::decode(&payload).is_err(), "解码拒绝超长文本");
    }

    #[test]
    fn describe_ptt_binding() {
        let b = Binding::new(vec![], "ptt-toggle", 1);
        assert_eq!(describe_binding_for_screen(&b), "开麦 关麦");
    }

    #[test]
    fn describe_modifier_binding() {
        let b = Binding::new(vec![Modifier::Ctrl], "enter", 1);
        assert_eq!(describe_binding_for_screen(&b), "ctrl+enter");
    }

    #[test]
    fn describe_repeat() {
        let b = Binding::new(vec![], "esc", 2);
        assert_eq!(describe_binding_for_screen(&b), "esc x2");
    }

    #[test]
    fn describe_truncates_on_char_boundary() {
        // 24 字节 = 8 个汉字。9 个汉字必须截到 8 个，且不留下半个字符。
        let b = Binding::new(vec![], "开麦关麦开麦关麦开麦", 1);
        let out = describe_binding_for_screen(&b);
        assert!(out.len() <= MAX_LABEL_BYTES, "长度 {} 超过上限", out.len());
        assert!(out.is_char_boundary(out.len()), "截断落在字符边界上");
        assert_eq!(out.chars().count(), 8);
    }

    #[test]
    fn labels_from_example_keymap() {
        let raw = include_str!("../../../../config/button-map.example.json");
        let map = KeyMap::parse(raw).expect("样例映射必须能解析");
        let labels = labels_from_keymap(&map, 7);
        assert_eq!(labels.pc_pid, 7);
        assert_eq!(labels.capture_key, Some(ScreenKey::Ok), "样例里 ok 是开麦键");
        assert_eq!(labels.items.len(), 3);
        assert_eq!(labels.items[0].text, "ctrl+enter");
        assert_eq!(labels.items[1].text, "esc x2", "repeat=2 要体现在屏幕上");
        assert_eq!(labels.items[2].text, "开麦 关麦");
        // 生成的标签必须能编码（不超长、不重复）。
        labels.encode().expect("样例映射生成的标签必须可编码");
    }

    #[test]
    fn missing_binding_shows_placeholder() {
        let map = KeyMap::parse(
            r#"{"schema":"ai-passport-mic-keymap-4","bindings":{"ok":{"key":"ptt-toggle"}}}"#,
        ).expect("parse");
        let labels = labels_from_keymap(&map, 0);
        // UP / DOWN 未绑定 → 「无」，不是空串（空串在屏幕上就是一行空白，像坏了）。
        assert_eq!(labels.items[0].text, "无");
        assert_eq!(labels.items[1].text, "无");
        assert_eq!(labels.items[2].text, "开麦 关麦");
    }

    #[test]
    fn capture_key_follows_remap() {
        let map = KeyMap::parse(
            r#"{"schema":"ai-passport-mic-keymap-4","bindings":{"up":{"key":"ptt-toggle"},"ok":{"key":"enter"}}}"#,
        )
            .expect("parse");
        let labels = labels_from_keymap(&map, 0);
        assert_eq!(labels.capture_key, Some(ScreenKey::Up), "开麦键换成 up 后要跟着变");
        assert_eq!(labels.items[0].text, "开麦 关麦");
        assert_eq!(labels.items[2].text, "enter");
    }
}
