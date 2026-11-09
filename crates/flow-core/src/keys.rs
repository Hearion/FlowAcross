//! 键盘与鼠标按钮：平台无关的键码、修饰键、按键状态与路由。
//!
//! 网络上传 USB HID 键码（Usage Page 0x07），平台层负责与 macOS 虚拟键码 / Windows 扫描码互转。

use std::collections::BTreeSet;
use std::ops::{BitOr, BitOrAssign};

use serde::{Deserialize, Serialize};

/// USB HID 键码（Keyboard/Keypad Page 0x07）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct KeyCode(pub u16);

impl KeyCode {
    pub const A: KeyCode = KeyCode(0x04);
    pub const C: KeyCode = KeyCode(0x06);
    pub const V: KeyCode = KeyCode(0x19);
    pub const ENTER: KeyCode = KeyCode(0x28);
    pub const ESCAPE: KeyCode = KeyCode(0x29);
    pub const BACKSPACE: KeyCode = KeyCode(0x2A);
    pub const TAB: KeyCode = KeyCode(0x2B);
    pub const SPACE: KeyCode = KeyCode(0x2C);
    pub const CAPS_LOCK: KeyCode = KeyCode(0x39);
    pub const RIGHT: KeyCode = KeyCode(0x4F);
    pub const LEFT: KeyCode = KeyCode(0x50);
    pub const DOWN: KeyCode = KeyCode(0x51);
    pub const UP: KeyCode = KeyCode(0x52);

    pub const LEFT_CTRL: KeyCode = KeyCode(0xE0);
    pub const LEFT_SHIFT: KeyCode = KeyCode(0xE1);
    pub const LEFT_ALT: KeyCode = KeyCode(0xE2);
    /// macOS ⌘ / Windows 徽标键。
    pub const LEFT_META: KeyCode = KeyCode(0xE3);
    pub const RIGHT_CTRL: KeyCode = KeyCode(0xE4);
    pub const RIGHT_SHIFT: KeyCode = KeyCode(0xE5);
    pub const RIGHT_ALT: KeyCode = KeyCode(0xE6);
    pub const RIGHT_META: KeyCode = KeyCode(0xE7);

    /// 如果是修饰键，返回对应的修饰键位。
    pub fn modifier(self) -> Option<Modifiers> {
        match self {
            Self::LEFT_SHIFT | Self::RIGHT_SHIFT => Some(Modifiers::SHIFT),
            Self::LEFT_CTRL | Self::RIGHT_CTRL => Some(Modifiers::CTRL),
            Self::LEFT_ALT | Self::RIGHT_ALT => Some(Modifiers::ALT),
            Self::LEFT_META | Self::RIGHT_META => Some(Modifiers::META),
            _ => None,
        }
    }

    /// Mac ⌘ ↔ Windows Ctrl 互换（左右分别互换）。用于 Mac 控制 Windows 时让 ⌘C 变成 Ctrl+C。
    pub fn swap_meta_ctrl(self) -> KeyCode {
        match self {
            Self::LEFT_META => Self::LEFT_CTRL,
            Self::LEFT_CTRL => Self::LEFT_META,
            Self::RIGHT_META => Self::RIGHT_CTRL,
            Self::RIGHT_CTRL => Self::RIGHT_META,
            other => other,
        }
    }
}

/// Mac 上的一个修饰键。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MacModifier {
    /// ⌃ Control
    Control,
    /// ⌥ Option
    Option,
    /// ⌘ Command
    Command,
}

impl MacModifier {
    fn key(self, right: bool) -> KeyCode {
        match (self, right) {
            (Self::Control, false) => KeyCode::LEFT_CTRL,
            (Self::Control, true) => KeyCode::RIGHT_CTRL,
            (Self::Option, false) => KeyCode::LEFT_ALT,
            (Self::Option, true) => KeyCode::RIGHT_ALT,
            (Self::Command, false) => KeyCode::LEFT_META,
            (Self::Command, true) => KeyCode::RIGHT_META,
        }
    }
}

/// PC（Windows）键盘控制 Mac 时，Ctrl、徽标键（Win）、Alt 分别当作 Mac 的哪个键。左右键各自对应。
///
/// 可以把两个键对应到同一个 Mac 键；此时同时按住再松开其中一个，Mac 会认为它已经松开。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WinToMacKeys {
    pub ctrl: MacModifier,
    pub win: MacModifier,
    pub alt: MacModifier,
}

impl Default for WinToMacKeys {
    fn default() -> Self {
        Self::WINDOWS
    }
}

impl WinToMacKeys {
    /// Windows 习惯（默认）：Ctrl → ⌘、Win → ⌃，Ctrl+C 在 Mac 上就是复制。
    pub const WINDOWS: Self = Self {
        ctrl: MacModifier::Command,
        win: MacModifier::Control,
        alt: MacModifier::Option,
    };
    /// 按 Mac 键盘的位置：空格旁边的 Alt → ⌘、Win → ⌥、Ctrl → ⌃。
    pub const MAC_POSITION: Self = Self {
        ctrl: MacModifier::Control,
        win: MacModifier::Option,
        alt: MacModifier::Command,
    };
    /// 原样直通：和把 PC 键盘直接插在 Mac 上一样，Win → ⌘、Alt → ⌥。
    pub const DIRECT: Self = Self {
        ctrl: MacModifier::Control,
        win: MacModifier::Command,
        alt: MacModifier::Option,
    };

    /// 把 PC 键盘的键码换算成 Mac 上要注入的键码；不是 Ctrl/Win/Alt 的键原样返回。
    pub fn apply(self, code: KeyCode) -> KeyCode {
        let (m, right) = match code {
            KeyCode::LEFT_CTRL => (self.ctrl, false),
            KeyCode::RIGHT_CTRL => (self.ctrl, true),
            KeyCode::LEFT_META => (self.win, false),
            KeyCode::RIGHT_META => (self.win, true),
            KeyCode::LEFT_ALT => (self.alt, false),
            KeyCode::RIGHT_ALT => (self.alt, true),
            other => return other,
        };
        m.key(right)
    }
}

/// 修饰键集合（位掩码）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Modifiers(pub u8);

impl Modifiers {
    pub const NONE: Modifiers = Modifiers(0);
    pub const SHIFT: Modifiers = Modifiers(1);
    pub const CTRL: Modifiers = Modifiers(1 << 1);
    pub const ALT: Modifiers = Modifiers(1 << 2);
    pub const META: Modifiers = Modifiers(1 << 3);

    pub fn contains(self, other: Modifiers) -> bool {
        self.0 & other.0 == other.0
    }
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl BitOr for Modifiers {
    type Output = Modifiers;
    fn bitor(self, rhs: Self) -> Self {
        Modifiers(self.0 | rhs.0)
    }
}

impl BitOrAssign for Modifiers {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// 有"按下/抬起"状态的输入事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputEvent {
    Key {
        code: KeyCode,
        down: bool,
        repeat: bool,
    },
    Button {
        button: MouseButton,
        down: bool,
    },
}

/// 记录"已按下、尚未抬起"的按键和鼠标按钮。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PressedState {
    keys: BTreeSet<KeyCode>,
    buttons: BTreeSet<MouseButton>,
}

impl PressedState {
    /// 返回状态是否发生变化。
    pub fn on_key(&mut self, code: KeyCode, down: bool) -> bool {
        if down {
            self.keys.insert(code)
        } else {
            self.keys.remove(&code)
        }
    }

    pub fn on_button(&mut self, button: MouseButton, down: bool) -> bool {
        if down {
            self.buttons.insert(button)
        } else {
            self.buttons.remove(&button)
        }
    }

    pub fn has_key(&self, code: KeyCode) -> bool {
        self.keys.contains(&code)
    }

    pub fn has_button(&self, button: MouseButton) -> bool {
        self.buttons.contains(&button)
    }

    pub fn any_button(&self) -> bool {
        !self.buttons.is_empty()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty()
    }

    pub fn modifiers(&self) -> Modifiers {
        self.keys
            .iter()
            .filter_map(|k| k.modifier())
            .fold(Modifiers::NONE, |acc, m| acc | m)
    }

    /// 清空状态并返回需要发送的"抬起"事件：先鼠标按钮（结束拖拽），再普通键，最后修饰键，
    /// 避免释放过程中意外组合出快捷键。
    pub fn release_all(&mut self) -> Vec<InputEvent> {
        let mut out: Vec<InputEvent> = self
            .buttons
            .iter()
            .map(|&button| InputEvent::Button {
                button,
                down: false,
            })
            .collect();
        let (mods, normal): (Vec<KeyCode>, Vec<KeyCode>) =
            self.keys.iter().partition(|k| k.modifier().is_some());
        out.extend(normal.into_iter().chain(mods).map(|code| InputEvent::Key {
            code,
            down: false,
            repeat: false,
        }));
        self.keys.clear();
        self.buttons.clear();
        out
    }
}

/// 一个物理输入事件该交给谁。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// 放行给本机系统。
    Local,
    /// 拦截并发送给正在控制的远端设备。
    Remote,
    /// 拦截并丢弃（例如切到远端前就按住的键，其连发事件不应该继续在本机打字）。
    Drop,
}

/// 按键路由：在"本机"和"远端"之间切换时，保证每个按键的抬起事件都送到当初收到按下的那一边。
///
/// 典型场景：按住 Shift 跨屏（防误触设置），到了远端才松开 Shift。
/// 这个抬起必须交给本机，否则本机会一直认为 Shift 被按着。
#[derive(Debug, Default)]
pub struct InputRouter {
    local: PressedState,
    remote: PressedState,
    remote_active: bool,
}

impl InputRouter {
    pub fn is_remote(&self) -> bool {
        self.remote_active
    }

    /// 当前物理上按住的修饰键（本机 + 远端）。
    pub fn held_modifiers(&self) -> Modifiers {
        self.local.modifiers() | self.remote.modifiers()
    }

    /// 当前是否有鼠标按钮被按住（v1 不支持跨设备拖拽，按住时禁止切换）。
    pub fn any_button_held(&self) -> bool {
        self.local.any_button() || self.remote.any_button()
    }

    pub fn route_key(&mut self, code: KeyCode, down: bool, repeat: bool) -> Route {
        if down && repeat {
            return if self.remote.has_key(code) {
                Route::Remote
            } else if self.local.has_key(code) {
                if self.remote_active {
                    Route::Drop
                } else {
                    Route::Local
                }
            } else if self.remote_active {
                Route::Remote
            } else {
                Route::Local
            };
        }
        if down {
            if self.remote_active {
                self.remote.on_key(code, true);
                Route::Remote
            } else {
                self.local.on_key(code, true);
                Route::Local
            }
        } else if self.remote.on_key(code, false) {
            Route::Remote
        } else if self.local.on_key(code, false) {
            Route::Local
        } else if self.remote_active {
            Route::Remote
        } else {
            Route::Local
        }
    }

    pub fn route_button(&mut self, button: MouseButton, down: bool) -> Route {
        if down {
            if self.remote_active {
                self.remote.on_button(button, true);
                Route::Remote
            } else {
                self.local.on_button(button, true);
                Route::Local
            }
        } else if self.remote.on_button(button, false) {
            Route::Remote
        } else if self.local.on_button(button, false) {
            Route::Local
        } else if self.remote_active {
            Route::Remote
        } else {
            Route::Local
        }
    }

    /// 开始控制远端。本机已按住的键保持记录，它们的抬起仍交给本机。
    pub fn enter_remote(&mut self) {
        self.remote_active = true;
    }

    /// 结束控制远端（正常切回、断网、超时都走这里）。返回需要发给远端的释放事件。
    pub fn leave_remote(&mut self) -> Vec<InputEvent> {
        self.remote_active = false;
        self.remote.release_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifiers_from_pressed_keys() {
        let mut s = PressedState::default();
        s.on_key(KeyCode::LEFT_SHIFT, true);
        s.on_key(KeyCode::RIGHT_META, true);
        s.on_key(KeyCode::A, true);
        assert_eq!(s.modifiers(), Modifiers::SHIFT | Modifiers::META);
        assert!(s.modifiers().contains(Modifiers::SHIFT));
        assert!(!s.modifiers().contains(Modifiers::CTRL));
    }

    #[test]
    fn release_all_orders_buttons_keys_then_modifiers() {
        let mut s = PressedState::default();
        s.on_key(KeyCode::LEFT_CTRL, true);
        s.on_key(KeyCode::C, true);
        s.on_button(MouseButton::Left, true);
        let ev = s.release_all();
        assert_eq!(
            ev,
            vec![
                InputEvent::Button {
                    button: MouseButton::Left,
                    down: false
                },
                InputEvent::Key {
                    code: KeyCode::C,
                    down: false,
                    repeat: false
                },
                InputEvent::Key {
                    code: KeyCode::LEFT_CTRL,
                    down: false,
                    repeat: false
                },
            ]
        );
        assert!(s.is_empty());
    }

    #[test]
    fn modifier_held_while_crossing_is_released_locally() {
        let mut r = InputRouter::default();
        assert_eq!(r.route_key(KeyCode::LEFT_SHIFT, true, false), Route::Local);
        r.enter_remote();
        // 在远端打字
        assert_eq!(r.route_key(KeyCode::A, true, false), Route::Remote);
        assert_eq!(r.route_key(KeyCode::A, false, false), Route::Remote);
        // 松开跨屏前按下的 Shift → 交给本机
        assert_eq!(r.route_key(KeyCode::LEFT_SHIFT, false, false), Route::Local);
        assert_eq!(r.held_modifiers(), Modifiers::NONE);
    }

    #[test]
    fn local_autorepeat_is_dropped_while_remote() {
        let mut r = InputRouter::default();
        r.route_key(KeyCode::SPACE, true, false);
        r.enter_remote();
        assert_eq!(r.route_key(KeyCode::SPACE, true, true), Route::Drop);
        assert_eq!(r.route_key(KeyCode::SPACE, false, false), Route::Local);
    }

    #[test]
    fn remote_autorepeat_is_forwarded() {
        let mut r = InputRouter::default();
        r.enter_remote();
        r.route_key(KeyCode::BACKSPACE, true, false);
        assert_eq!(r.route_key(KeyCode::BACKSPACE, true, true), Route::Remote);
    }

    #[test]
    fn leave_remote_releases_everything_on_remote() {
        let mut r = InputRouter::default();
        r.enter_remote();
        r.route_key(KeyCode::LEFT_META, true, false);
        r.route_button(MouseButton::Left, true);
        assert!(r.any_button_held());
        let released = r.leave_remote();
        assert_eq!(released.len(), 2);
        assert!(!r.any_button_held());
        // 之后到来的物理抬起事件交给本机（本机没见过按下，抬起无副作用）
        assert_eq!(r.route_key(KeyCode::LEFT_META, false, false), Route::Local);
    }

    #[test]
    fn win_to_mac_presets() {
        let all = [
            KeyCode::LEFT_CTRL,
            KeyCode::RIGHT_CTRL,
            KeyCode::LEFT_META,
            KeyCode::RIGHT_META,
            KeyCode::LEFT_ALT,
            KeyCode::RIGHT_ALT,
            KeyCode::A,
            KeyCode::LEFT_SHIFT,
        ];
        // Windows 习惯与原来的 ⌘/Ctrl 互换完全一致，老用户升级后手感不变。
        assert_eq!(WinToMacKeys::default(), WinToMacKeys::WINDOWS);
        for k in all {
            assert_eq!(WinToMacKeys::WINDOWS.apply(k), k.swap_meta_ctrl());
            assert_eq!(WinToMacKeys::DIRECT.apply(k), k);
        }
        // 按 Mac 位置：空格旁的 Alt 是 ⌘，左右分别对应。
        let m = WinToMacKeys::MAC_POSITION;
        assert_eq!(m.apply(KeyCode::LEFT_ALT), KeyCode::LEFT_META);
        assert_eq!(m.apply(KeyCode::RIGHT_ALT), KeyCode::RIGHT_META);
        assert_eq!(m.apply(KeyCode::LEFT_META), KeyCode::LEFT_ALT);
        assert_eq!(m.apply(KeyCode::LEFT_CTRL), KeyCode::LEFT_CTRL);
        assert_eq!(m.apply(KeyCode::C), KeyCode::C);
    }

    #[test]
    fn win_to_mac_custom() {
        let custom = WinToMacKeys {
            ctrl: MacModifier::Command,
            win: MacModifier::Option,
            alt: MacModifier::Command,
        };
        assert_eq!(custom.apply(KeyCode::RIGHT_CTRL), KeyCode::RIGHT_META);
        assert_eq!(custom.apply(KeyCode::LEFT_ALT), KeyCode::LEFT_META);
        assert_eq!(custom.apply(KeyCode::RIGHT_META), KeyCode::RIGHT_ALT);
        let bytes = postcard::to_allocvec(&custom).unwrap();
        assert_eq!(
            postcard::from_bytes::<WinToMacKeys>(&bytes).unwrap(),
            custom
        );
    }

    #[test]
    fn swap_meta_ctrl_is_symmetric() {
        for k in [KeyCode::LEFT_META, KeyCode::RIGHT_CTRL, KeyCode::A] {
            assert_eq!(k.swap_meta_ctrl().swap_meta_ctrl(), k);
        }
        assert_eq!(KeyCode::LEFT_META.swap_meta_ctrl(), KeyCode::LEFT_CTRL);
    }
}
