//! 系统级按键注入（M3）。
//!
//! 只做一件事：把一个 [`Binding`] 变成"真的敲了一下这个键"。分两类动作：
//!
//! - 普通键（`enter` / `esc` / `a` / `f5` …）：按下 + 弹起，发到系统，落到当前前台应用；
//! - 保留动作 `ptt-toggle`：**不在这里处理**，它由 `core` 改采集状态，返回
//!   [`InjectError::Reserved`] 让调用方自己走。
//!
//! 两条硬规则（`config/README.md`）：
//!
//! 1. **不管成功还是异常，按下过的修饰键必须释放。** 这里用"一次性发 complete 事件"
//!    的做法：按下 + 弹起成对出现。修饰键作为目标键（`lalt` / `ralt`）时 keyDown
//!    带上自己的修饰键 flag、keyUp 清掉——不存在"按下去没松开"的状态。
//! 2. 注入失败要能被看见。macOS 上最常见的原因是**没给辅助功能权限**，
//!    系统不会报错，只是把事件丢掉——所以发送前先探一次权限，没权限直接报错并给出指引。
//!
//! macOS 之外暂时没有实现：Windows 需要 SendInput，第一版只在 macOS 发布。

use apm_protocol::keymap::{Binding, Modifier, PTT_TOGGLE};

/// 注入失败的原因。都带人话，直接进诊断日志。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InjectError {
    /// 这个键名在系统层面没有对应键（例如 macOS 没有 PrintScreen）。
    UnsupportedKey(String),
    /// 保留动作（`ptt-toggle`）不归注入层管。
    Reserved,
    /// 没有辅助功能权限。
    NoPermission,
    /// 系统调用失败。
    System(String),
    /// 当前平台没实现。
    UnsupportedPlatform,
}

impl std::fmt::Display for InjectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InjectError::UnsupportedKey(k) => write!(
                f,
                "键名 {k} 在本系统上没有对应按键（macOS 没有 PrintScreen / Menu 这类键）"
            ),
            InjectError::Reserved => write!(f, "保留动作，不走注入层"),
            InjectError::NoPermission => write!(
                f,
                "没有“辅助功能”权限：系统设置 → 隐私与安全性 → 辅助功能，把本 App 打开后重试"
            ),
            InjectError::System(e) => write!(f, "系统调用失败：{e}"),
            InjectError::UnsupportedPlatform => write!(f, "按键注入目前只实现了 macOS"),
        }
    }
}

/// 敲一下（按下 + 弹起）。`repeat > 1` 时连发多次。
pub fn tap(binding: &Binding) -> Result<(), InjectError> {
    if binding.key == PTT_TOGGLE {
        return Err(InjectError::Reserved);
    }
    imp::tap(binding)
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

    // AXIsProcessTrusted 在 ApplicationServices 里，返回 Boolean（u8）。
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> u8;
        fn AXIsProcessTrustedWithOptions(options: *const std::ffi::c_void) -> u8;
    }

    /// 有没有辅助功能权限。没有的话 CGEventPost 会**静默丢弃**事件，
    /// 不探一次的话用户只会看到"按了没反应"。
    pub fn has_permission() -> bool {
        unsafe { AXIsProcessTrusted() != 0 }
    }

    /// 请求辅助功能权限（带系统弹窗）。
    ///
    /// 纯 `AXIsProcessTrusted()` 探测**不会让本 App 出现在系统设置的辅助功能列表里**——
    /// 列表项只有在 App 第一次带着 `kAXTrustedCheckOptionPrompt` 请求时才被登记。
    /// 没有这一步，用户点开设置面板看到的列表是空的，只能手动添加
    /// （debug 包的路径埋在 target 深处，实际不可操作）。
    ///
    /// 返回值与探测相同；弹窗是异步的，用户拒绝/没点时返回 false，横幅轮询会继续复查。
    pub fn request_permission() -> bool {
        use core_foundation::base::TCFType;
        use core_foundation::boolean::CFBoolean;
        use core_foundation::dictionary::CFDictionary;
        use core_foundation::string::CFString;

        let key = CFString::new("AXTrustedCheckOptionPrompt");
        let options = CFDictionary::from_CFType_pairs(&[(key, CFBoolean::true_value())]);
        unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef() as *const _) != 0 }
    }

    fn flags_of(modifiers: &[Modifier]) -> CGEventFlags {
        let mut flags = CGEventFlags::empty();
        for m in modifiers {
            flags |= match m {
                Modifier::Ctrl => CGEventFlags::CGEventFlagControl,
                Modifier::Shift => CGEventFlags::CGEventFlagShift,
                Modifier::Alt => CGEventFlags::CGEventFlagAlternate,
                // macOS 上 win = Command
                Modifier::Win => CGEventFlags::CGEventFlagCommand,
            };
        }
        flags
    }

    /// 键名 → macOS 虚拟键码（Carbon HIToolbox 的那套 ANSI 码）。
    ///
    /// 只覆盖 `keymap::is_supported_key` 白名单里**本系统真实存在**的键。
    /// `printscreen` / `menu` / `insert` 在 macOS 上没有对应键，返回 `None`，
    /// 让用户看到明确的"这个键做不到"，而不是悄悄什么都不发。
    pub fn keycode_for(key: &str) -> Option<u16> {
        let code = match key {
            // 字母
            "a" => 0, "s" => 1, "d" => 2, "f" => 3, "h" => 4, "g" => 5, "z" => 6,
            "x" => 7, "c" => 8, "v" => 9, "b" => 11, "q" => 12, "w" => 13, "e" => 14,
            "r" => 15, "y" => 16, "t" => 17, "o" => 31, "u" => 32, "i" => 34, "p" => 35,
            "l" => 37, "j" => 38, "k" => 40, "n" => 45, "m" => 46,
            // 数字
            "1" => 18, "2" => 19, "3" => 20, "4" => 21, "6" => 22, "5" => 23,
            "9" => 25, "7" => 26, "8" => 28, "0" => 29,
            // 编辑键
            "enter" => 36, "tab" => 48, "space" => 49, "backspace" => 51, "esc" => 53,
            "delete" => 117, // forward delete
            "capslock" => 57,
            // 修饰键本身作为目标键：macOS 上 Alt = Option。
            // kVK_Option = 58，kVK_RightOption = 61（Carbon HIToolbox Events.h）。
            "lalt" => 58, "ralt" => 61,
            // 方向 / 翻页
            "left" => 123, "right" => 124, "down" => 125, "up" => 126,
            "home" => 115, "end" => 119, "pageup" => 116, "pagedown" => 121,
            // 功能键
            "f1" => 122, "f2" => 120, "f3" => 99, "f4" => 118, "f5" => 96,
            "f6" => 97, "f7" => 98, "f8" => 100, "f9" => 101, "f10" => 109,
            "f11" => 103, "f12" => 111,
            // 明确没有的键：printscreen / menu / insert
            _ => return None,
        };
        Some(code)
    }

    /// 修饰键作为目标键时，keyDown 事件必须带上它自己的修饰键 flag。
    ///
    /// macOS 上修饰键不是普通按键：应用识别"单按右 Option"靠的是 `flagsChanged`
    /// 事件里的修饰键标志位。如果 keyDown 只带键码不带 flag，收方看到的是
    /// "keycode 61 但 flags 没变化"——等于什么都没发生，事件被静默忽略。
    fn own_flag(key: &str) -> CGEventFlags {
        match key {
            "lalt" | "ralt" => CGEventFlags::CGEventFlagAlternate,
            "capslock" => CGEventFlags::CGEventFlagAlphaShift,
            _ => CGEventFlags::empty(),
        }
    }

    pub fn tap(binding: &Binding) -> Result<(), InjectError> {
        // 先查键码再查权限：键名不支持是"配置错误"，应当先报出来；
        // 否则没权限时所有键都会报成 NoPermission，用户永远看不到"这个键做不到"。
        let code = keycode_for(&binding.key)
            .ok_or_else(|| InjectError::UnsupportedKey(binding.key.clone()))?;
        if !has_permission() {
            return Err(InjectError::NoPermission);
        }

        // core-graphics 0.25 这里返回 `Result<_, ()>`，没有错误详情可用。
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| InjectError::System("创建 CGEventSource 失败".into()))?;
        let base = flags_of(&binding.modifiers);
        // keyDown：目标键自身的修饰键 flag 必须在（见 own_flag 的注释）。
        let down_flags = base | own_flag(&binding.key);
        // keyUp：修饰键本身弹起，恢复到 binding 修饰键描述的状态。
        let up_flags = base;

        for _ in 0..binding.repeat.clamp(1, 4) {
            let down = CGEvent::new_keyboard_event(source.clone(), code, true)
                .map_err(|_| InjectError::System("构造 keyDown 事件失败".into()))?;
            down.set_flags(down_flags);
            down.post(CGEventTapLocation::HID);

            // 真实敲键按下到弹起至少几十毫秒；零间隔连发可能被有事件合并逻辑的
            // 应用当成无效事件。20 ms 在人手做不到但远小于任何热键超时的区间。
            std::thread::sleep(std::time::Duration::from_millis(20));

            let up = CGEvent::new_keyboard_event(source.clone(), code, false)
                .map_err(|_| InjectError::System("构造 keyUp 事件失败".into()))?;
            up.set_flags(up_flags);
            up.post(CGEventTapLocation::HID);
        }
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::*;

    pub fn has_permission() -> bool {
        true
    }

    pub fn request_permission() -> bool {
        true
    }

    pub fn tap(_binding: &Binding) -> Result<(), InjectError> {
        Err(InjectError::UnsupportedPlatform)
    }
}

/// 当前平台能不能注入（权限 + 实现都存在）。
pub fn available() -> bool {
    imp::has_permission()
}

/// 请求辅助功能权限（macOS：弹系统授权框，并把本 App 登记进辅助功能列表）。
/// 其他平台没有这一步，恒返回 true。
pub fn request_permission() -> bool {
    imp::request_permission()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(key: &str, mods: Vec<Modifier>, repeat: u8) -> Binding {
        Binding::new(mods, key, repeat)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn alt_side_keycodes_are_pinned() {
        // 键码是跨软件契约（别的软件按"右 Option = 61"识别热键），钉死防手滑改错。
        assert_eq!(imp::keycode_for("lalt"), Some(58)); // kVK_Option
        assert_eq!(imp::keycode_for("ralt"), Some(61)); // kVK_RightOption
    }

    /// 真机验收用：向前台应用真的发一次右 Alt 点按。默认 `#[ignore]`，手动跑：
    ///
    /// ```text
    /// cargo test -p apm-app -- --ignored real_tap --nocapture
    /// ```
    ///
    /// 事件归属**运行测试的宿主**（终端需要辅助功能权限；打包后的 .app 归 .app 自己）。
    /// 发出后靠目标软件的反应肉眼确认——右 Alt 单独点按在文本编辑器里通常没有可见效果。
    #[test]
    #[ignore = "真机验收：会向前台应用发真实按键，需要宿主有辅助功能权限"]
    fn real_tap_ralt_to_frontmost_app() {
        let result = tap(&binding("ralt", vec![], 1));
        println!("real ralt tap: {result:?}");
        assert!(result.is_ok(), "注入失败：{result:?}");
    }

    #[test]
    fn ptt_toggle_is_reserved_not_injected() {
        assert_eq!(
            tap(&binding(PTT_TOGGLE, vec![], 1)),
            Err(InjectError::Reserved),
            "ptt-toggle 必须交给 core 处理，不能当按键发出去"
        );
    }

    #[test]
    fn rejects_key_names_the_os_does_not_have() {
        // 这条在 macOS 上才有意义：白名单里有、系统上没这个键。
        #[cfg(target_os = "macos")]
        {
            for key in ["printscreen", "menu", "insert"] {
                assert_eq!(
                    tap(&binding(key, vec![], 1)),
                    Err(InjectError::UnsupportedKey(key.to_string())),
                    "{key} 不该被静默忽略"
                );
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keycode_table_covers_the_whitelist() {
        use apm_protocol::keymap::is_supported_key;
        // 逐个白名单键：要么有键码，要么是已知"本系统没有"的那三个。
        let known_missing = ["printscreen", "menu", "insert"];
        let samples = [
            "enter", "esc", "space", "tab", "backspace", "delete", "up", "down", "left",
            "right", "home", "end", "pageup", "pagedown", "capslock", "lalt", "ralt",
            "a", "z", "0", "9", "f1", "f12", "printscreen", "menu", "insert",
        ];
        for key in samples {
            assert!(is_supported_key(key), "{key} 应该在建键白名单里");
            match imp::keycode_for(key) {
                Some(code) => assert!(code < 128, "{key} 的键码 {code} 不像合法 ANSI 码"),
                None => assert!(
                    known_missing.contains(&key),
                    "{key} 在白名单里却没有键码，也不在已知缺失列表"
                ),
            }
        }
    }
}
