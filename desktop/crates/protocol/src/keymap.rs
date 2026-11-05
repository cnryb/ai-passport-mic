//! 按键映射 schema（`ai-passport-mic-keymap-3`）。
//!
//! 字段与校验顺序以 `config/README.md` 为准：
//! schema 版本 → 事件名合法 → 键名/修饰键在支持列表内 → PTT 约束。
//! 任一步失败整份拒绝，不做"部分生效"。
//!
//! **版本演进（两步都往"更少更好记"走）：**
//!
//! | 版本 | 事件 | 说明 |
//! | --- | --- | --- |
//! | v1 | 15 个 | 3 键 × short/double/long/press/release |
//! | v2 | 6 个 | 去掉三种手势，只留 press/release |
//! | **v3（当前）** | **3 个** | **一个键一个动作**：`up` / `down` / `ok` |
//!
//! v3 的语义变化：按键不再是"按下做什么、抬起做什么"，而是**一次按下 = 一次动作**。
//! OK 键因此从"按住说话、松手停"变成"按一下开、再按一下关"（`ptt-toggle`）。
//! 三个键、三个动作，用户不用记任何手势。
//!
//! 固件的**事件上报协议不变**（仍然会发 0..=4 五种手势码），PC 端只消费
//! `PRESS`（按下那一瞬），其余忽略。这样不用改协议、不用重生成跨端向量。

use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;

pub const SCHEMA: &str = "ai-passport-mic-keymap-4";
/// 旧版本都能读，读取时按下表迁移到当前版本（见 [`KeyMap::parse`]）。
pub const SCHEMA_V3: &str = "ai-passport-mic-keymap-3";
pub const SCHEMA_V2: &str = "ai-passport-mic-keymap-2";
pub const SCHEMA_V1: &str = "ai-passport-mic-keymap-1";
pub const MAX_REPEAT: u8 = 4;
/// PTT 开关动作：按一次开，再按一次关。v3 只有一个动作，不再是 start/stop 一对。
pub const PTT_TOGGLE: &str = "ptt-toggle";
/// 旧版（v1/v2）的 PTT 动作名，只在迁移时出现，会统一改写成 [`PTT_TOGGLE`]。
pub const PTT_START_LEGACY: &str = "ptt-start";
pub const PTT_STOP_LEGACY: &str = "ptt-stop";

/// 全部合法事件，顺序即界面展示顺序。**这是唯一一份事件表**，
/// `apm-app` 与 GUI 都从这里取，别在别处再抄一份。
///
/// 没有点号：v3 里"哪个键"就是事件本身，不存在手势维度。
pub const EVENTS: [&str; 3] = ["up", "down", "ok"];

/// **采集状态钩子**：不是按键事件，而是"开始/停止采集"这两个时刻。
///
/// 用途：对着麦克风说话的同时触发一个快捷键（例如切到语音输入、或者在会议软件里
/// 按一下"开启麦克风"）。开麦时注入 `capture.start`，关麦时注入 `capture.stop`，
/// 都是**点按**（按下+弹起），不是按住不放。
pub const HOOK_EVENTS: [&str; 2] = ["capture.start", "capture.stop"];

/// 钩子里不允许出现 `ptt-toggle`：那会在开麦的瞬间又 toggle 一次，自己把自己关掉。
pub fn is_hook_event(name: &str) -> bool {
    HOOK_EVENTS.contains(&name)
}

/// v1 里存在、v2 起删除的手势（读 v1 文件时静默丢弃）。
const LEGACY_GESTURES: [&str; 3] = ["short", "double", "long"];

/// 读文件时用的旧 schema 版本。`None` = 当前版本，严格校验。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Legacy {
    None,
    /// v3：已经是 3 个事件，但没有 `hooks`（读进来就是当前形态，只是钩子为空）。
    V3,
    /// v2：`up.press` / `up.release` 两种；一个键两个状态。
    V2,
    /// v1：在 v2 的基础上还有 short/double/long。
    V1,
}


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
    /// 小写键名，或保留动作 [`PTT_TOGGLE`]。
    pub key: String,
    /// 目标键连发次数，1..=[`MAX_REPEAT`]。
    pub repeat: u8,
}

impl Binding {
    pub fn new(modifiers: Vec<Modifier>, key: impl Into<String>, repeat: u8) -> Self {
        Self { modifiers, key: key.into(), repeat }
    }

    pub fn is_ptt(&self) -> bool {
        self.key == PTT_TOGGLE
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
    /// `ok` 被改成了非 `ptt-toggle`：麦克风就没有开关了。
    PttToggleMissing,
    /// 钩子里写了 `ptt-toggle`：开麦瞬间又 toggle，会自己关掉自己。
    HookCannotToggle { event: String },
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
                "未知事件名: {e}（合法值: up / down / ok；v3 起一个键只有一个动作，\n\
                 不再有 press/release/short/double/long）"
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
            KeyMapError::PttToggleMissing => write!(
                f,
                "ok 必须是 {PTT_TOGGLE}：它是麦克风唯一的开关，改掉就没法停采集"
            ),
            KeyMapError::HookCannotToggle { event } => write!(
                f,
                "{event} 不能绑 {PTT_TOGGLE}：采集钩子在开麦/关麦时触发，再 toggle 一次会自己把采集关掉"
            ),
        }
    }
}

impl std::error::Error for KeyMapError {}

/// 只检查形状，不检查语义 —— 语义在 [`parse`] 里按固定顺序查。
///
/// v3 的事件名就是按键本身，**没有点号**。`up.press` 这种是 v2 写法，出现在 v3 文件里
/// 一律按未知事件拒绝（迁移只在 schema 是旧版本时才走）。
pub fn is_supported_event(name: &str) -> bool {
    EVENTS.contains(&name)
}

/// 旧版事件名 → 按键名。`up.press` / `up.release` / `ok.short` 都归到 `up`、`ok`。
fn legacy_button(name: &str) -> Option<&str> {
    match name.split_once('.') {
        Some((button, _gesture)) if matches!(button, "up" | "down" | "ok") => Some(button),
        _ => None,
    }
}

/// 旧版的 PTT 动作名统一改写成 v3 的 toggle。
fn normalize_legacy_key(key: &str) -> &str {
    match key {
        PTT_START_LEGACY | PTT_STOP_LEGACY => PTT_TOGGLE,
        other => other,
    }
}

pub fn is_supported_key(key: &str) -> bool {
    if key == PTT_TOGGLE {
        return true;
    }
    // `lalt` / `ralt`：修饰键本身作为目标键（macOS 上即左/右 Option），
    // 用于"单按右 Alt"这类热键。左右语义只体现在目标键上，组合修饰键仍是 `alt`。
    const NAMED: [&str; 20] = [
        "enter", "esc", "space", "tab", "backspace", "delete", "up", "down", "left", "right",
        "home", "end", "pageup", "pagedown", "insert", "printscreen", "menu", "capslock",
        "lalt", "ralt",
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
    /// 按键事件 → 动作（`up` / `down` / `ok`）。
    bindings: BTreeMap<String, Binding>,
    /// 采集状态钩子 → 动作（`capture.start` / `capture.stop`）。可以为空。
    hooks: BTreeMap<String, Binding>,
}

/// 默认只锁 OK 键的 PTT 开关；UP/DOWN 未绑定 = 不动作。
fn default_bindings() -> BTreeMap<String, Binding> {
    let mut map = BTreeMap::new();
    map.insert("ok".to_string(), Binding::new(vec![], PTT_TOGGLE, 1));
    map
}

/// 解析单个动作对象。`legacy` 为真时把旧版的 `ptt-start` / `ptt-stop`
/// 改写成 v3 的 [`PTT_TOGGLE`]——v3 只有一个开关动作。
fn parse_action(
    event: &str,
    value: &Value,
    legacy: bool,
    allow_ptt: bool,
) -> Result<Binding, KeyMapError> {
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
                event: event.to_string(),
                modifier: raw.to_string(),
            })?;
            if !modifiers.contains(&m) {
                modifiers.push(m);
            }
        }
    }

    let raw_key = action
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| KeyMapError::UnknownKey {
            event: event.to_string(),
            key: String::new(),
        })?;
    let key = if legacy {
        normalize_legacy_key(raw_key)
    } else {
        raw_key
    };
    if !is_supported_key(key) {
        return Err(KeyMapError::UnknownKey {
            event: event.to_string(),
            key: key.to_string(),
        });
    }
    // 钩子里禁止 ptt-toggle：开麦的瞬间又 toggle 一次会把自己关掉。
    if !allow_ptt && key == PTT_TOGGLE {
        return Err(KeyMapError::HookCannotToggle { event: event.to_string() });
    }

    let repeat = match action.get("repeat") {
        None => 1,
        Some(v) => {
            let n = v.as_i64().ok_or(KeyMapError::RepeatOutOfRange {
                event: event.to_string(),
                value: -1,
            })?;
            if !(1..=MAX_REPEAT as i64).contains(&n) {
                return Err(KeyMapError::RepeatOutOfRange {
                    event: event.to_string(),
                    value: n,
                });
            }
            n as u8
        }
    };

    Ok(Binding {
        modifiers,
        key: key.to_string(),
        repeat,
    })
}

impl KeyMap {
    /// 解析并校验。失败时整份拒绝，调用方不要拿半个 map 去用。
    ///
    /// 旧版本（v2 的 6 事件、v1 的 15 事件）会在这里迁移到 v3 的 3 事件：
    /// 一个键只留一个动作，`press` 优先于 `release`；`ptt-start`/`ptt-stop` 归成 `ptt-toggle`。
    /// **迁移必须在事件名校验之前发生**，否则老文件会直接报"未知事件名"。
    pub fn parse(json_text: &str) -> Result<Self, KeyMapError> {
        let root: Value =
            serde_json::from_str(json_text).map_err(|e| KeyMapError::Json(e.to_string()))?;
        let obj = root.as_object().ok_or(KeyMapError::NotObject("根节点"))?;

        // 1. schema。旧版本允许读，走向下迁移；未知版本整份拒绝。
        let actual = obj.get("schema").and_then(Value::as_str).unwrap_or("");
        let legacy = match actual {
            SCHEMA => Legacy::None,
            SCHEMA_V3 => Legacy::V3,
            SCHEMA_V2 => Legacy::V2,
            SCHEMA_V1 => Legacy::V1,
            _ => return Err(KeyMapError::Schema { actual: actual.to_string() }),
        };

        // 2. 事件名
        let bindings_value = obj
            .get("bindings")
            .ok_or(KeyMapError::NotObject("bindings"))?;
        let bindings_obj = bindings_value
            .as_object()
            .ok_or(KeyMapError::NotObject("bindings 字段"))?;

        let mut parsed: BTreeMap<String, Binding> = BTreeMap::new();
        match legacy {
            Legacy::None | Legacy::V3 => {
                for (event, value) in bindings_obj {
                    if !is_supported_event(event) {
                        return Err(KeyMapError::UnknownEvent(event.clone()));
                    }
                    parsed.insert(event.clone(), parse_action(event, value, false, true)?);
                }
            }
            from @ (Legacy::V1 | Legacy::V2) => {
                // 旧版一个键可能有两个状态（按下/抬起），v3 只能留一个：
                // 按下优先——它是"用户想做事"的那一刻；只有抬起绑定的事件才退而取抬起。
                let mut press: BTreeMap<&str, Binding> = BTreeMap::new();
                let mut release: BTreeMap<&str, Binding> = BTreeMap::new();
                for (event, value) in bindings_obj {
                    let Some(button) = legacy_button(event) else {
                        return Err(KeyMapError::UnknownEvent(event.clone()));
                    };
                    let gesture = event.split_once('.').map(|(_, g)| g).unwrap_or("");
                    if from == Legacy::V1 && LEGACY_GESTURES.contains(&gesture) {
                        continue; // v1 的短按/双击/长按：直接丢，不报错
                    }
                    if gesture != "press" && gesture != "release" {
                        return Err(KeyMapError::UnknownEvent(event.clone()));
                    }
                    let binding = parse_action(event, value, true, true)?;
                    if gesture == "press" {
                        press.insert(button, binding);
                    } else {
                        release.insert(button, binding);
                    }
                }
                for (button, binding) in press {
                    parsed.insert(button.to_string(), binding);
                }
                for (button, binding) in release {
                    parsed.entry(button.to_string()).or_insert(binding);
                }
            }
        }

        // 未出现的事件用默认值。
        for (event, binding) in default_bindings() {
            parsed.entry(event).or_insert(binding);
        }

        // 4. PTT 约束：停止通道必须在。
        match parsed.get("ok") {
            Some(b) if b.key == PTT_TOGGLE => {}
            _ => return Err(KeyMapError::PttToggleMissing),
        }

        // 5. 采集状态钩子（可选）。键名必须落在 HOOK_EVENTS 里，动作不允许是 ptt-toggle。
        let mut hooks: BTreeMap<String, Binding> = BTreeMap::new();
        if let Some(hooks_value) = obj.get("hooks") {
            let hooks_obj = hooks_value
                .as_object()
                .ok_or(KeyMapError::NotObject("hooks 字段"))?;
            for (event, value) in hooks_obj {
                if !is_hook_event(event) {
                    return Err(KeyMapError::UnknownEvent(event.clone()));
                }
                hooks.insert(event.clone(), parse_action(event, value, false, false)?);
            }
        }

        Ok(KeyMap { bindings: parsed, hooks })
    }

    /// 查动作。按键事件与采集钩子统一在这里查，调用方不用关心存在哪张表。
    pub fn resolve(&self, event: &str) -> Option<&Binding> {
        self.bindings.get(event).or_else(|| self.hooks.get(event))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Binding)> {
        self.bindings.iter()
    }

    /// 只看采集钩子（`capture.start` / `capture.stop`），按固定顺序。
    pub fn hooks(&self) -> Vec<(&str, &Binding)> {
        HOOK_EVENTS
            .iter()
            .filter_map(|e| self.hooks.get(*e).map(|b| (*e, b)))
            .collect()
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
        let mut root = Map::new();
        root.insert("schema".to_string(), Value::String(SCHEMA.to_string()));
        root.insert("bindings".to_string(), Value::Object(bindings));
        if !self.hooks.is_empty() {
            let mut hooks = Map::new();
            for (event, binding) in &self.hooks {
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
                hooks.insert(event.clone(), Value::Object(action));
            }
            root.insert("hooks".to_string(), Value::Object(hooks));
        }
        let root = Value::Object(root);
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
        assert_eq!(map.resolve("up").unwrap().key, "enter");
        assert_eq!(map.resolve("up").unwrap().modifiers, vec![Modifier::Ctrl]);
        assert_eq!(map.resolve("down").unwrap().repeat, 2);
        assert_eq!(map.resolve("ok").unwrap().key, PTT_TOGGLE);
        assert!(map.resolve("ok").unwrap().is_ptt());
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
        let minimal = r#"{"schema":"ai-passport-mic-keymap-3","bindings":{}}"#;
        let map = KeyMap::parse(minimal).unwrap();
        assert_eq!(map.resolve("ok").unwrap().key, PTT_TOGGLE);
        assert!(map.resolve("up").is_none(), "UP 默认不动作");
        assert!(map.resolve("down").is_none());
    }

    #[test]
    fn rejects_wrong_schema() {
        let bad = r#"{"schema":"ai-passport-mic-keymap-0","bindings":{}}"#;
        assert!(matches!(KeyMap::parse(bad), Err(KeyMapError::Schema { .. })));
    }

    /// v1（15 事件）→ v3：三种手势丢弃，按下优先，PTT 两端归成 toggle。
    #[test]
    fn migrates_v1_by_dropping_gestures() {
        let v1 = r#"{"schema":"ai-passport-mic-keymap-1","bindings":{
            "up.short":{"modifiers":["ctrl"],"key":"enter"},
            "up.double":{"key":"esc","repeat":2},
            "down.long":{"key":"f5"},
            "down.press":{"key":"space"},
            "ok.press":{"key":"ptt-start"},
            "ok.release":{"key":"ptt-stop"}
        }}"#;
        let map = KeyMap::parse(v1).expect("v1 必须还能读");
        assert!(map.resolve("up.short").is_none(), "v1 的手势事件应被丢弃");
        assert!(map.resolve("down.long").is_none());
        assert_eq!(map.resolve("down").unwrap().key, "space");
        assert_eq!(map.resolve("ok").unwrap().key, PTT_TOGGLE);
        assert!(map.to_json().contains("ai-passport-mic-keymap-4"), "迁移后导出必须是当前版本");
    }

    /// v2（6 事件）→ v3：一个键两个状态时**按下优先**，release 只在缺 press 时补位。
    #[test]
    fn migrates_v2_press_wins_over_release() {
        let v2 = r#"{"schema":"ai-passport-mic-keymap-2","bindings":{
            "up.press":{"modifiers":["ctrl"],"key":"enter"},
            "up.release":{"key":"esc"},
            "down.release":{"key":"f5"},
            "ok.press":{"key":"ptt-start"},
            "ok.release":{"key":"ptt-stop"}
        }}"#;
        let map = KeyMap::parse(v2).expect("v2 必须还能读");
        assert_eq!(map.resolve("up").unwrap().key, "enter", "按下优先，抬起丢弃");
        assert_eq!(map.resolve("up").unwrap().modifiers, vec![Modifier::Ctrl]);
        assert_eq!(map.resolve("down").unwrap().key, "f5", "只有抬起时退而取抬起");
        assert_eq!(map.resolve("ok").unwrap().key, PTT_TOGGLE);
        assert!(map.to_json().contains("ai-passport-mic-keymap-4"), "迁移后导出必须是当前版本");
    }

    /// 钩子里写 ptt-toggle 必须被拒：开麦瞬间又 toggle，会自己关掉自己。
    #[test]
    fn rejects_ptt_toggle_in_hooks() {
        let bad = r#"{"schema":"ai-passport-mic-keymap-4","bindings":{"ok":{"key":"ptt-toggle"}},"hooks":{
            "capture.start":{"key":"ptt-toggle"}
        }}"#;
        assert!(matches!(KeyMap::parse(bad), Err(KeyMapError::HookCannotToggle { .. })));
    }

    /// 带钩子的文件 round-trip 后钩子不能丢。
    #[test]
    fn hooks_survive_roundtrip() {
        let text = r#"{"schema":"ai-passport-mic-keymap-4","bindings":{"ok":{"key":"ptt-toggle"}},"hooks":{
            "capture.start":{"modifiers":["win"],"key":"f5"},
            "capture.stop":{"modifiers":["win"],"key":"f6"}
        }}"#;
        let map = KeyMap::parse(text).unwrap();
        assert_eq!(map.hooks().len(), 2);
        let again = KeyMap::parse(&map.to_json()).unwrap();
        assert_eq!(again.hooks().len(), 2, "钩子在 round-trip 后丢了");
    }

    #[test]
    fn rejects_unknown_event() {
        let bad = r#"{"schema":"ai-passport-mic-keymap-3","bindings":{"left":{"key":"esc"}}}"#;
        assert!(matches!(KeyMap::parse(bad), Err(KeyMapError::UnknownEvent(_))));
        // v3 里出现旧写法必须报错，不能被悄悄忽略
        let old = r#"{"schema":"ai-passport-mic-keymap-3","bindings":{"up.press":{"key":"esc"}}}"#;
        assert!(matches!(KeyMap::parse(old), Err(KeyMapError::UnknownEvent(_))));
    }

    #[test]
    fn rejects_unknown_key_and_modifier() {
        let bad_key = r#"{"schema":"ai-passport-mic-keymap-3","bindings":{"up":{"key":"hyper"}}}"#;
        assert!(matches!(KeyMap::parse(bad_key), Err(KeyMapError::UnknownKey { .. })));

        let bad_mod =
            r#"{"schema":"ai-passport-mic-keymap-3","bindings":{"up":{"modifiers":["meta"],"key":"a"}}}"#;
        assert!(matches!(
            KeyMap::parse(bad_mod),
            Err(KeyMapError::UnknownModifier { .. })
        ));
    }

    /// v3 只有 `ptt-toggle` 一个保留动作：旧名直接写进 v3 文件应被拒。
    #[test]
    fn rejects_legacy_ptt_action_in_v3() {
        let bad = r#"{"schema":"ai-passport-mic-keymap-3","bindings":{"ok":{"key":"ptt-start"}}}"#;
        assert!(matches!(KeyMap::parse(bad), Err(KeyMapError::UnknownKey { .. })));
    }

    #[test]
    fn rejects_repeat_out_of_range() {
        let bad = r#"{"schema":"ai-passport-mic-keymap-3","bindings":{"down":{"key":"esc","repeat":5}}}"#;
        assert!(matches!(
            KeyMap::parse(bad),
            Err(KeyMapError::RepeatOutOfRange { .. })
        ));
    }

    #[test]
    fn rejects_rewritten_ptt_toggle() {
        let bad = r#"{"schema":"ai-passport-mic-keymap-3","bindings":{"ok":{"key":"esc"}}}"#;
        assert_eq!(KeyMap::parse(bad), Err(KeyMapError::PttToggleMissing));
    }

    #[test]
    fn rejects_invalid_json() {
        assert!(matches!(KeyMap::parse("{"), Err(KeyMapError::Json(_))));
    }
}
