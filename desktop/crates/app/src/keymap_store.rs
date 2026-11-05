//! 按键映射的读写与校验（G5）。
//!
//! 语义校验全部复用 `apm_protocol::keymap`：**只有一份** schema 实现，
//! 3 个事件（一个键一个动作）、键名白名单、PTT 开关约束都在那里。
//! 本模块只负责"文件在哪、坏文件怎么办、界面上怎么摆"。
//!
//! 一条硬规则：解析失败就整份拒绝，不写盘、不做部分生效。
//! 用户改坏一个键名时，宁可弹错误，也不能让 PTT 停下来不响应。

use crate::settings::{data_dir, keymap_path};
use apm_protocol::keymap::{self, Binding, KeyMap, SCHEMA};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 按键事件，顺序即界面展示顺序。
///
/// **直接取 `apm_protocol::keymap::EVENTS`，不再本地抄一份**——v1 时这里是 15 个硬编码
/// 字符串，协议层改事件表时差点只改一半。事件表只有一份，界面顺序就是那张表的顺序。
pub const EVENTS: [&str; 3] = keymap::EVENTS;

/// 采集状态钩子（开麦/关麦时注入的快捷键），跟在按键事件后面一起展示。
///
/// 它们不是按键事件：是"开始/停止采集"这两个时刻。放同一张表里编辑、
/// 存盘时分开（按键进 `bindings`，钩子进 `hooks`）。
pub const HOOKS: [&str; 2] = keymap::HOOK_EVENTS;

/// 界面上出现的全部行（按键 + 钩子），顺序即展示顺序。
pub fn all_events() -> [&'static str; 5] {
    [
        EVENTS[0], EVENTS[1], EVENTS[2], HOOKS[0], HOOKS[1],
    ]
}

pub const MODIFIERS: [&str; 4] = ["ctrl", "shift", "alt", "win"];

#[derive(Debug, Clone, Serialize)]
pub struct BindingView {
    pub event: String,
    /// 修饰键，按 `ctrl/shift/alt/win` 排序，空数组即无修饰。
    pub modifiers: Vec<String>,
    /// 目标键；空字符串表示"未绑定 = 不动作"。
    pub key: String,
    pub repeat: u8,
    /// 是 PTT 的开关动作（`ptt-toggle`）。
    pub is_ptt: bool,
    /// 不允许在界面上改（`ok` 必须是 `ptt-toggle`）。
    pub locked: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct KeymapView {
    pub path: String,
    pub schema: String,
    pub bindings: Vec<BindingView>,
    /// 读文件失败的原因（文件坏了要看得见），正常时为 null。
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BindingInput {
    pub event: String,
    #[serde(default)]
    pub modifiers: Vec<String>,
    /// 空字符串 = 解绑（不动作）。
    #[serde(default)]
    pub key: String,
    #[serde(default = "one")]
    pub repeat: u8,
}

fn one() -> u8 {
    1
}

fn view_of(map: &KeyMap, path: PathBuf, error: Option<String>) -> KeymapView {
    let bindings = all_events()
        .iter()
        .map(|event| {
            let binding: Option<&Binding> = map.resolve(event);
            BindingView {
                event: (*event).to_string(),
                modifiers: binding
                    .map(|b| b.modifiers.iter().map(|m| m.as_str().to_string()).collect())
                    .unwrap_or_default(),
                key: binding.map(|b| b.key.clone()).unwrap_or_default(),
                repeat: binding.map(|b| b.repeat).unwrap_or(1),
                is_ptt: binding.map(|b| b.is_ptt()).unwrap_or(false),
                locked: *event == "ok",
            }
        })
        .collect();
    KeymapView {
        path: path.display().to_string(),
        schema: SCHEMA.to_string(),
        bindings,
        error,
    }
}

/// 只锁 OK 键 PTT 开关的默认映射（与 `keymap::default_bindings` 一致）。
pub fn default_map() -> KeyMap {
    let minimal = format!("{{\"schema\":\"{SCHEMA}\",\"bindings\":{{}}}}");
    KeyMap::parse(&minimal).expect("默认映射必然合法")
}

pub fn load() -> (KeyMap, Option<String>) {
    let path = keymap_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        // 没文件不是错误：默认映射就是"只有 OK 键管 PTT"。
        return (default_map(), None);
    };
    match KeyMap::parse(&text) {
        Ok(map) => (map, None),
        Err(err) => (default_map(), Some(format!("{err}；已回退到默认映射"))),
    }
}

pub fn view() -> KeymapView {
    let (map, error) = load();
    view_of(&map, keymap_path(), error)
}

/// 当前生效的映射原文（用于"导出"）。
pub fn text() -> String {
    load().0.to_json()
}

fn write_map(dir: &std::path::Path, map: &KeyMap) -> Result<KeymapView, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("创建配置目录失败：{e}"))?;
    let path = dir.join("keymap.json");
    std::fs::write(&path, map.to_json()).map_err(|e| format!("写入映射失败：{e}"))?;
    Ok(view_of(map, path, None))
}

/// 从界面提交的一组绑定生成完整映射。
///
/// - 空 `key` 的事件不写进 JSON（schema 里"不出现"才表示不动作）；
/// - `ok` 被清空或改成别的键时，强制填回 `ptt-toggle`，
///   否则麦克风没有开关（`config/README.md` 的 PTT 约束）。
pub fn save_bindings_at(
    dir: &std::path::Path,
    bindings: &[BindingInput],
) -> Result<KeymapView, String> {
    let mut root_bindings = serde_json::Map::new();
    let mut root_hooks = serde_json::Map::new();
    for input in bindings {
        // 钩子与按键是两类：按键进 `bindings`，采集钩子进 `hooks`。
        let is_hook = keymap::is_hook_event(&input.event);
        if !is_hook && !keymap::is_supported_event(&input.event) {
            return Err(format!("未知事件名：{}", input.event));
        }
        if input.key.trim().is_empty() {
            continue;
        }
        let mut action = serde_json::Map::new();
        let mods: Vec<serde_json::Value> = input
            .modifiers
            .iter()
            .map(|m| serde_json::Value::String(m.clone()))
            .collect();
        action.insert("modifiers".into(), serde_json::Value::Array(mods));
        action.insert("key".into(), serde_json::Value::String(input.key.trim().to_string()));
        if input.repeat != 1 {
            action.insert("repeat".into(), serde_json::Value::from(input.repeat));
        }
        if is_hook {
            root_hooks.insert(input.event.clone(), serde_json::Value::Object(action));
        } else {
            root_bindings.insert(input.event.clone(), serde_json::Value::Object(action));
        }
    }

    // PTT 开关兜底：无论界面怎么传，都保证它在。
    root_bindings.insert(
        "ok".into(),
        serde_json::json!({ "modifiers": [], "key": keymap::PTT_TOGGLE }),
    );

    let mut root = serde_json::Map::new();
    root.insert("schema".into(), serde_json::json!(SCHEMA));
    root.insert("bindings".into(), serde_json::Value::Object(root_bindings));
    if !root_hooks.is_empty() {
        root.insert("hooks".into(), serde_json::Value::Object(root_hooks));
    }
    let text = serde_json::to_string(&serde_json::Value::Object(root))
        .map_err(|e| format!("序列化失败：{e}"))?;

    match KeyMap::parse(&text) {
        Ok(map) => write_map(dir, &map),
        Err(err) => Err(err.to_string()),
    }
}

pub fn save_bindings(bindings: &[BindingInput]) -> Result<KeymapView, String> {
    save_bindings_at(&data_dir(), bindings)
}

/// 导入用户粘贴/选择的 JSON 文本。校验与上面同一条路径。
pub fn save_text_at(dir: &std::path::Path, text: &str) -> Result<KeymapView, String> {
    match KeyMap::parse(text) {
        Ok(map) => write_map(dir, &map),
        Err(err) => Err(err.to_string()),
    }
}

pub fn save_text(text: &str) -> Result<KeymapView, String> {
    save_text_at(&data_dir(), text)
}

/// 恢复默认：写回只含 PTT 两端的映射。
pub fn reset_at(dir: &std::path::Path) -> Result<KeymapView, String> {
    write_map(dir, &default_map())
}

pub fn reset() -> Result<KeymapView, String> {
    reset_at(&data_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_lists_all_events_even_when_unbound() {
        let v = view_of(&default_map(), PathBuf::from("/tmp/x.json"), None);
        // 按键 3 个 + 钩子 2 个，全部展示；未绑定显示为空。
        assert_eq!(v.bindings.len(), all_events().len());
        assert_eq!(v.bindings.len(), 5);
        let up = v.bindings.iter().find(|b| b.event == "up").unwrap();
        assert!(up.key.is_empty(), "未绑定要显示为空，不能显示成 esc");
        let ok = v.bindings.iter().find(|b| b.event == "ok").unwrap();
        assert!(ok.locked);
        assert!(ok.is_ptt);
        assert_eq!(ok.key, keymap::PTT_TOGGLE);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("apm-keymap-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn save_rejects_unknown_key() {
        let err = save_bindings_at(
            &temp_dir("unknown-key"),
            &[BindingInput {
                event: "up".into(),
                modifiers: vec![],
                key: "hyper".into(),
                repeat: 1,
            }],
        )
        .unwrap_err();
        assert!(err.contains("hyper"), "{err}");
    }

    #[test]
    fn save_rejects_unknown_modifier() {
        let err = save_bindings_at(
            &temp_dir("unknown-mod"),
            &[BindingInput {
                event: "up".into(),
                modifiers: vec!["meta".into()],
                key: "a".into(),
                repeat: 1,
            }],
        )
        .unwrap_err();
        assert!(err.contains("meta"), "{err}");
    }

    #[test]
    fn empty_key_means_unbound_not_esc() {
        let dir = temp_dir("unbound");
        let view = save_bindings_at(
            &dir,
            &[BindingInput {
                event: "up".into(),
                modifiers: vec![],
                key: "".into(),
                repeat: 1,
            }],
        )
        .expect("解绑不该报错");
        let up = view.bindings.iter().find(|b| b.event == "up").unwrap();
        assert!(up.key.is_empty(), "空 key 必须表示不动作");
        // 落盘文本里不该出现 key: ""
        let text = std::fs::read_to_string(dir.join("keymap.json")).unwrap();
        assert!(!text.contains("\"key\": \"\""), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ok_is_forced_back_to_ptt_toggle() {
        let dir = temp_dir("ptt");
        let view = save_bindings_at(
            &dir,
            &[BindingInput {
                event: "ok".into(),
                modifiers: vec![],
                key: "esc".into(),
                repeat: 1,
            }],
        )
        .unwrap();
        let ok = view.bindings.iter().find(|b| b.event == "ok").unwrap();
        assert_eq!(ok.key, keymap::PTT_TOGGLE, "PTT 开关被改掉了");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn roundtrip_through_file_keeps_bindings() {
        let dir = temp_dir("roundtrip");
        save_bindings_at(
            &dir,
            &[BindingInput {
                event: "up".into(),
                modifiers: vec!["ctrl".into()],
                key: "enter".into(),
                repeat: 2,
            }],
        )
        .unwrap();
        let text = std::fs::read_to_string(dir.join("keymap.json")).unwrap();
        let (map, err) = (KeyMap::parse(&text).unwrap(), None::<String>);
        assert!(err.is_none());
        let b = map.resolve("up").unwrap();
        assert_eq!(b.key, "enter");
        assert_eq!(b.repeat, 2);
        assert_eq!(b.modifiers.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
