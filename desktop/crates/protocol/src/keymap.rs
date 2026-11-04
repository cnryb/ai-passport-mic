//! 按键映射 schema（`ai-passport-mic-keymap-1`）。
//!
//! 字段与校验顺序以 `config/README.md` 为准：
//! schema 版本 → 事件名合法 → 键名/修饰键在支持列表内 → PTT 约束。
//! 任一步失败整份拒绝，不做"部分生效"。
//!
//! 事件名描述手势（`double` = 双击了一次），`repeat` 描述动作（把目标键连发几次）。

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::fmt;

pub const SCHEMA: &str = "ai-passport-mic-keymap-1";
pub const MAX_REPEAT: u8 = 4;
pub const PTT_START: &str = "ptt-start";
pub const PTT_STOP: &str = "ptt-stop";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Modifier {
    Ctrl,
    Shift,
    Alt,
    Win,
}

impl Modifier {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "ctrl" => Some(Modifier::Ctrl),
            "shift" => Some(Modifier::Shift),
            "alt" => Some(Modifier::Alt),
            "win" => Some(Modifier::Win),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Modifier::Ctrl => "ctrl",
            Modifier::Shift => "shift",
            Modifier::Alt => "alt",
            Modifier::Win => "win",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub modifiers: Vec<Modifier>,
    /// 小写键名，或保留动作 [`PTT_START`] / [`PTT_STOP`]。
    pub key: String,
    /// 目标键连发次数，1..=[`MAX_REPEAT`]。
    pub repeat: u8,
}

impl Binding {
    pub fn new(modifiers: Vec<Modifier>, key: impl Into<String>, repeat: u8) -> Self {
        Self { modifiers, key: key.into(), repeat }
    }

    pub fn is_ptt(&self) -> bool {
        self.key == PTT_START || self.key == PTT_STOP
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyMapError {
    Json(String),
    Schema { actual: String },
    NotObject(&'static str),
    UnknownEvent(String),
    UnknownModifier { event: String, modifier: String },
    UnknownKey { event: String, key: String },
    RepeatOutOfRange { event: String, value: i64 },
    /// `ok.release` 被改成了非 `ptt-stop`：麦克风会停不下来。
    PttStopMissing,
}

impl fmt::Display for KeyMapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyMapError::Json(e) => write!(f, "JSON 解析失败: {e}"),
            KeyMapError::Schema { actual } => write!(
                f,
                "schema 版本不支持: {actual}（期望 {SCHEMA}）；版本号变了就要写迁移说明"
            ),
            KeyMapError::NotObject(what) => write!(f, "{what} 必须是 JSON 对象"),
            KeyMapError::UnknownEvent(e) => write!(
                f,
                "未知事件名: {e}（合法值: up/down/ok × short/double/long/press/release）"
            ),
            KeyMapError::UnknownModifier { event, modifier } => write!(
                f,
                "{event} 的修饰键 {modifier} 不在支持列表（ctrl/shift/alt/win）"
            ),
            KeyMapError::UnknownKey { event, key } => {
                write!(f, "{event} 的键名 {key} 不在支持列表")
            }
            KeyMapError::RepeatOutOfRange { event, value } => write!(
                f,
                "{event} 的 repeat={value} 超出范围 1..={MAX_REPEAT}"
            ),
            KeyMapError::PttStopMissing => write!(
                f,
                "ok.release 必须是 {PTT_STOP}：它是 PTT 的停止通道，改掉会让麦克风停不下来"
            ),
        }
    }
}

impl std::error::Error for KeyMapError {}

/// 只检查形状，不检查语义 —— 语义在 [`parse`] 里按固定顺序查。
pub fn is_supported_event(name: &str) -> bool {
    let mut parts = name.split('.');
    let (Some(button), Some(gesture)) = (parts.next(), parts.next()) else {
        return false;
    };
    if parts.next().is_some() {
        return false; // 多于一个点：up.short.x 这种一律拒绝
    }
    let button_ok = matches!(button, "up" | "down" | "ok");
    let gesture_ok = matches!(gesture, "short" | "double" | "long" | "press" | "release");
    button_ok && gesture_ok
}

pub fn is_supported_key(key: &str) -> bool {
    if key == PTT_START || key == PTT_STOP {
        return true;
    }
    const NAMED: [&str; 18] = [
        "enter", "esc", "space", "tab", "backspace", "delete", "up", "down", "left", "right",
        "home", "end", "pageup", "pagedown", "insert", "printscreen", "menu", "capslock",
    ];
    if NAMED.contains(&key) {
        return true;
    }
    if let Some(rest) = key.strip_prefix('f') {
        if let Ok(n) = rest.parse::<u8>() {
            return (1..=12).contains(&n) && !rest.starts_with('0');
        }
        return false;
    }
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => c.is_ascii_lowercase() || c.is_ascii_digit(),
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeyMap {
    bindings: BTreeMap<String, Binding>,
}

/// 默认只锁 PTT 的两个端点；其余事件未绑定 = 不动作。
fn default_bindings() -> BTreeMap<String, Binding> {
    let mut map = BTreeMap::new();
    map.insert("ok.press".to_string(), Binding::new(vec![], PTT_START, 1));
    map.insert("ok.release".to_string(), Binding::new(vec![], PTT_STOP, 1));
    map
}

impl KeyMap {
    /// 解析并校验。失败时整份拒绝，调用方不要拿半个 map 去用。
    pub fn parse(json_text: &str) -> Result<Self, KeyMapError> {
        let root: Value =
            serde_json::from_str(json_text).map_err(|e| KeyMapError::Json(e.to_string()))?;
        let obj = root.as_object().ok_or(KeyMapError::NotObject("根节点"))?;

        // 1. schema
        let actual = obj.get("schema").and_then(Value::as_str).unwrap_or("");
        if actual != SCHEMA {
            return Err(KeyMapError::Schema { actual: actual.to_string() });
        }

        // 2. 事件名
        let bindings_value = obj
            .get("bindings")
            .ok_or(KeyMapError::NotObject("bindings"))?;
        let bindings_obj = bindings_value
            .as_object()
            .ok_or(KeyMapError::NotObject("bindings 字段"))?;

        let mut parsed: BTreeMap<String, Binding> = BTreeMap::new();
        for (event, value) in bindings_obj {
            if !is_supported_event(event) {
                return Err(KeyMapError::UnknownEvent(event.clone()));
            }

            // 3. 修饰键 / 键名
            let action = value
                .as_object()
                .ok_or(KeyMapError::NotObject("bindings.* 的值"))?;

            let mut modifiers = Vec::new();
            if let Some(list) = action.get("modifiers") {
                let arr = list
                    .as_array()
                    .ok_or(KeyMapError::NotObject("modifiers"))?;
                for item in arr {
                    let raw = item.as_str().unwrap_or("");
                    let m = Modifier::parse(raw).ok_or_else(|| KeyMapError::UnknownModifier {
                        event: event.clone(),
                        modifier: raw.to_string(),
                    })?;
                    if !modifiers.contains(&m) {
                        modifiers.push(m);
                    }
                }
            }

            let key = action
                .get("key")
                .and_then(Value::as_str)
                .ok_or_else(|| KeyMapError::UnknownKey {
                    event: event.clone(),
                    key: String::new(),
                })?;
            if !is_supported_key(key) {
                return Err(KeyMapError::UnknownKey {
                    event: event.clone(),
                    key: key.to_string(),
                });
            }

            let repeat = match action.get("repeat") {
                None => 1,
                Some(v) => {
                    let n = v.as_i64().ok_or(KeyMapError::RepeatOutOfRange {
                        event: event.clone(),
                        value: -1,
                    })?;
                    if !(1..=MAX_REPEAT as i64).contains(&n) {
                        return Err(KeyMapError::RepeatOutOfRange {
                            event: event.clone(),
                            value: n,
                        });
                    }
                    n as u8
                }
            };

            parsed.insert(
                event.clone(),
                Binding { modifiers, key: key.to_string(), repeat },
            );
        }

        // 未出现的事件用默认值。
        for (event, binding) in default_bindings() {
            parsed.entry(event).or_insert(binding);
        }

        // 4. PTT 约束：停止通道必须在。
        match parsed.get("ok.release") {
            Some(b) if b.key == PTT_STOP => {}
            _ => return Err(KeyMapError::PttStopMissing),
        }

        Ok(KeyMap { bindings: parsed })
    }

    pub fn resolve(&self, event: &str) -> Option<&Binding> {
        self.bindings.get(event)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Binding)> {
        self.bindings.iter()
    }

    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    /// 导出当前生效的绑定（含默认值），不写注释与未知字段，保证 round-trip 稳定。
    pub fn to_json(&self) -> String {
        let mut bindings = Map::new();
        for (event, binding) in &self.bindings {
            let modifiers: Vec<Value> = binding
                .modifiers
                .iter()
                .map(|m| Value::String(m.as_str().to_string()))
                .collect();
            let mut action = Map::new();
            action.insert("modifiers".to_string(), Value::Array(modifiers));
            action.insert("key".to_string(), Value::String(binding.key.clone()));
            if binding.repeat != 1 {
                action.insert("repeat".to_string(), Value::from(binding.repeat));
            }
            bindings.insert(event.clone(), Value::Object(action));
        }
        let root = json!({
            "schema": SCHEMA,
            "bindings": Value::Object(bindings),
        });
        serde_json::to_string_pretty(&root).expect("序列化不会失败")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../../../../config/button-map.example.json");

    #[test]
    fn example_file_parses() {
        let map = KeyMap::parse(EXAMPLE).unwrap();
        assert_eq!(map.resolve("up.short").unwrap().key, "enter");
        assert_eq!(map.resolve("up.short").unwrap().modifiers, vec![Modifier::Ctrl]);
        assert_eq!(map.resolve("down.double").unwrap().repeat, 2);
        assert_eq!(map.resolve("ok.press").unwrap().key, PTT_START);
        assert_eq!(map.resolve("ok.release").unwrap().key, PTT_STOP);
    }

    #[test]
    fn roundtrip_is_stable() {
        let map = KeyMap::parse(EXAMPLE).unwrap();
        let once = map.to_json();
        let twice = KeyMap::parse(&once).unwrap().to_json();
        assert_eq!(once, twice, "导出再导入后文本必须稳定");
    }

    #[test]
    fn defaults_fill_missing_ptt() {
        let minimal = r#"{"schema":"ai-passport-mic-keymap-1","bindings":{}}"#;
        let map = KeyMap::parse(minimal).unwrap();
        assert_eq!(map.resolve("ok.press").unwrap().key, PTT_START);
        assert_eq!(map.resolve("ok.release").unwrap().key, PTT_STOP);
    }

    #[test]
    fn rejects_wrong_schema() {
        let bad = r#"{"schema":"ai-passport-mic-keymap-2","bindings":{}}"#;
        assert!(matches!(KeyMap::parse(bad), Err(KeyMapError::Schema { .. })));
    }

    #[test]
    fn rejects_unknown_event() {
        let bad = r#"{"schema":"ai-passport-mic-keymap-1","bindings":{"up.triple":{"key":"esc"}}}"#;
        assert!(matches!(KeyMap::parse(bad), Err(KeyMapError::UnknownEvent(_))));
    }

    #[test]
    fn rejects_unknown_key_and_modifier() {
        let bad_key = r#"{"schema":"ai-passport-mic-keymap-1","bindings":{"up.short":{"key":"hyper"}}}"#;
        assert!(matches!(KeyMap::parse(bad_key), Err(KeyMapError::UnknownKey { .. })));

        let bad_mod =
            r#"{"schema":"ai-passport-mic-keymap-1","bindings":{"up.short":{"modifiers":["meta"],"key":"a"}}}"#;
        assert!(matches!(
            KeyMap::parse(bad_mod),
            Err(KeyMapError::UnknownModifier { .. })
        ));
    }

    #[test]
    fn rejects_repeat_out_of_range() {
        let bad =
            r#"{"schema":"ai-passport-mic-keymap-1","bindings":{"down.double":{"key":"esc","repeat":5}}}"#;
        assert!(matches!(
            KeyMap::parse(bad),
            Err(KeyMapError::RepeatOutOfRange { .. })
        ));
    }

    #[test]
    fn rejects_rewritten_ptt_stop() {
        let bad = r#"{"schema":"ai-passport-mic-keymap-1","bindings":{"ok.release":{"key":"esc"}}}"#;
        assert_eq!(KeyMap::parse(bad), Err(KeyMapError::PttStopMissing));
    }

    #[test]
    fn rejects_invalid_json() {
        assert!(matches!(KeyMap::parse("{"), Err(KeyMapError::Json(_))));
    }
}
