//! 自定义快捷键与快捷键映射（纯逻辑）。
//!
//! 快捷键都是"手边这台电脑"的：只有物理键盘所在的电脑看得到按键，所以在哪台电脑上按，
//! 就用哪台电脑的设置。三类动作：
//!
//! - 切到左 / 右相邻的电脑（默认 Ctrl+Alt+←/→，可以改、可以关）；
//! - 切到指定的电脑（包括回到本机）；
//! - 快捷键映射：按下一组键，让指定的电脑收到另一组键（类似 Synergy 的 hotkey），
//!   不需要先把键鼠切过去。
//!
//! 修饰键按"恰好相等"匹配：Ctrl+Alt+S 和 Ctrl+Alt+Shift+S 是两个不同的快捷键。

use serde::{Deserialize, Serialize};

use crate::keys::{KeyCode, Modifiers};
use crate::layout::DeviceId;

/// 每一类快捷键最多保留这么多条，配置文件被手动改坏时不至于拖慢每次按键。
pub const MAX_HOTKEYS: usize = 32;

/// 一组按键：若干修饰键加一个主键。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyCombo {
    pub mods: Modifiers,
    pub key: KeyCode,
}

/// F1–F12。
const F1_F12: std::ops::RangeInclusive<u16> = 0x3A..=0x45;
/// Print Screen、Scroll Lock、Pause。
const SYS_KEYS: std::ops::RangeInclusive<u16> = 0x46..=0x48;
/// F13–F20。HID 里还有 F21–F24（0x70–0x73），但各系统的键码表（`flow-platform::keymap`）只到 F20
/// （Mac 的键盘没有更多），录成快捷键也永远不会触发。
const F13_F20: std::ops::RangeInclusive<u16> = 0x68..=0x6F;

impl KeyCombo {
    pub const fn new(mods: Modifiers, key: KeyCode) -> Self {
        Self { mods, key }
    }

    /// 主键是一个真实的、不是修饰键的按键：HID 键盘页里有定义的范围（0x04–0xA4），
    /// 不含后面的保留区和修饰键。
    fn has_real_key(&self) -> bool {
        (0x04..=0xA4).contains(&self.key.0) && self.mods.0 & !0x0F == 0
    }

    /// 能不能当触发键：要带 Ctrl、Alt 或 ⌘ / Win 之一，否则平时打字就会触发；
    /// 功能键（F1–F20、Print Screen、Scroll Lock、Pause）可以单独用。
    pub fn is_trigger(&self) -> bool {
        if !self.has_real_key() {
            return false;
        }
        let strong = Modifiers::CTRL.0 | Modifiers::ALT.0 | Modifiers::META.0;
        self.mods.0 & strong != 0
            || F1_F12.contains(&self.key.0)
            || SYS_KEYS.contains(&self.key.0)
            || F13_F20.contains(&self.key.0)
    }

    /// 能不能发给别的电脑：有一个真实的主键就行，修饰键可有可无（比如只发一个 S）。
    pub fn is_sendable(&self) -> bool {
        self.has_real_key()
    }

    /// 发给对方时的按键顺序：修饰键（用左边的那个）在前，主键在后。对方依次按下、反序抬起。
    pub fn keys(&self) -> Vec<KeyCode> {
        let mut out = Vec::with_capacity(5);
        for (m, code) in [
            (Modifiers::CTRL, KeyCode::LEFT_CTRL),
            (Modifiers::ALT, KeyCode::LEFT_ALT),
            (Modifiers::SHIFT, KeyCode::LEFT_SHIFT),
            (Modifiers::META, KeyCode::LEFT_META),
        ] {
            if self.mods.contains(m) {
                out.push(code);
            }
        }
        out.push(self.key);
        out
    }
}

/// 被控制方执行一次快捷键映射时依次注入的按键：(键码, 是否按下)。
///
/// `held`：正在控制本机的设备手上还按着的修饰键（触发映射用的 Ctrl、Alt 多半已经发过来了）。
/// 直接按 `keys` 会和它们组合在一起，所以先把它们抬起，`keys` 依次按下、反序抬起，
/// 最后再把 `held` 按回去——对方松手时发来的抬起才对得上。
pub fn tap_sequence(held: &[KeyCode], keys: &[KeyCode]) -> Vec<(KeyCode, bool)> {
    let mut out = Vec::with_capacity(2 * (held.len() + keys.len()));
    out.extend(held.iter().map(|&k| (k, false)));
    out.extend(keys.iter().map(|&k| (k, true)));
    out.extend(keys.iter().rev().map(|&k| (k, false)));
    out.extend(held.iter().map(|&k| (k, true)));
    out
}

/// 按下这组键，把键鼠切到这台电脑（可以是本机：回到本机）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceHotkey {
    pub device: DeviceId,
    pub combo: KeyCombo,
}

/// 快捷键映射：按下 `trigger`，让 `device` 收到 `send`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyMapping {
    pub trigger: KeyCombo,
    pub send: KeyCombo,
    pub device: DeviceId,
}

/// 这台电脑上的全部快捷键设置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    /// 切到左边相邻的电脑；`None` 表示不用。
    pub switch_left: Option<KeyCombo>,
    /// 切到右边相邻的电脑；`None` 表示不用。
    pub switch_right: Option<KeyCombo>,
    /// 切到指定的电脑。
    pub devices: Vec<DeviceHotkey>,
    /// 快捷键映射。
    pub mappings: Vec<KeyMapping>,
}

impl Default for Hotkeys {
    /// 默认只有 Ctrl+Alt+←/→，和以前写死的一样。
    fn default() -> Self {
        let mods = Modifiers::CTRL | Modifiers::ALT;
        Self {
            switch_left: Some(KeyCombo::new(mods, KeyCode::LEFT)),
            switch_right: Some(KeyCombo::new(mods, KeyCode::RIGHT)),
            devices: Vec::new(),
            mappings: Vec::new(),
        }
    }
}

/// 一个快捷键触发后要做的事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyAction {
    SwitchLeft,
    SwitchRight,
    SwitchTo(DeviceId),
    Send { device: DeviceId, keys: KeyCombo },
}

impl Hotkeys {
    /// 按住 `mods` 时按下 `key` 触发哪个动作。同一组键出现多次时，按"左右切换、切到指定电脑、
    /// 映射"的顺序取第一个（[`Hotkeys::sanitized`] 之后不会有重复）。
    pub fn action(&self, mods: Modifiers, key: KeyCode) -> Option<HotkeyAction> {
        let hit = |c: &KeyCombo| c.mods == mods && c.key == key;
        if self.switch_left.as_ref().is_some_and(hit) {
            return Some(HotkeyAction::SwitchLeft);
        }
        if self.switch_right.as_ref().is_some_and(hit) {
            return Some(HotkeyAction::SwitchRight);
        }
        if let Some(d) = self.devices.iter().find(|d| hit(&d.combo)) {
            return Some(HotkeyAction::SwitchTo(d.device.clone()));
        }
        self.mappings
            .iter()
            .find(|m| hit(&m.trigger))
            .map(|m| HotkeyAction::Send {
                device: m.device.clone(),
                keys: m.send,
            })
    }

    /// 整理界面或配置文件给的设置：去掉不能当触发键的、不能发送的、和前面重复的，每类最多
    /// [`MAX_HOTKEYS`] 条，每台电脑只留一个"切到它"的快捷键。
    pub fn sanitized(mut self) -> Self {
        let mut used: Vec<KeyCombo> = Vec::new();
        let mut take = |c: &KeyCombo| {
            if !c.is_trigger() || used.contains(c) {
                return false;
            }
            used.push(*c);
            true
        };
        self.switch_left = self.switch_left.filter(&mut take);
        self.switch_right = self.switch_right.filter(&mut take);
        // 满了就不再收：被丢掉的条目不能还占着它的触发键（先收再截断的话就会）。
        let mut seen: Vec<DeviceId> = Vec::new();
        self.devices.retain(|d| {
            if seen.len() >= MAX_HOTKEYS || seen.contains(&d.device) || !take(&d.combo) {
                return false;
            }
            seen.push(d.device.clone());
            true
        });
        let mut kept = 0;
        self.mappings.retain(|m| {
            if kept >= MAX_HOTKEYS || !m.send.is_sendable() || !take(&m.trigger) {
                return false;
            }
            kept += 1;
            true
        });
        self
    }

    /// 设备被移除后，把指向它的快捷键一起删掉。返回是否有改动。
    pub fn forget_device(&mut self, device: &DeviceId) -> bool {
        let before = self.devices.len() + self.mappings.len();
        self.devices.retain(|d| &d.device != device);
        self.mappings.retain(|m| &m.device != device);
        before != self.devices.len() + self.mappings.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTRL_ALT: Modifiers = Modifiers(Modifiers::CTRL.0 | Modifiers::ALT.0);
    const S: KeyCode = KeyCode(0x16);
    const F5: KeyCode = KeyCode(0x3E);

    fn dev(s: &str) -> DeviceId {
        DeviceId(s.into())
    }

    #[test]
    fn defaults_are_the_old_fixed_hotkeys() {
        let h = Hotkeys::default();
        assert_eq!(
            h.action(CTRL_ALT, KeyCode::LEFT),
            Some(HotkeyAction::SwitchLeft)
        );
        assert_eq!(
            h.action(CTRL_ALT, KeyCode::RIGHT),
            Some(HotkeyAction::SwitchRight)
        );
        // 多按一个 Shift 就不是同一个快捷键了。
        assert_eq!(h.action(CTRL_ALT | Modifiers::SHIFT, KeyCode::LEFT), None);
        assert_eq!(h.action(Modifiers::CTRL, KeyCode::LEFT), None);
        // 旧配置文件里没有这一项：用默认值。
        let old: Hotkeys = serde_json::from_str("{}").unwrap();
        assert_eq!(old, h);
    }

    #[test]
    fn triggers_need_a_strong_modifier_unless_function_key() {
        assert!(KeyCombo::new(CTRL_ALT, S).is_trigger());
        assert!(KeyCombo::new(Modifiers::META, S).is_trigger());
        // 只有 Shift 或什么都不带：平时打字就会按到。
        assert!(!KeyCombo::new(Modifiers::SHIFT, S).is_trigger());
        assert!(!KeyCombo::new(Modifiers::NONE, S).is_trigger());
        // 功能键可以单独用。
        assert!(KeyCombo::new(Modifiers::NONE, F5).is_trigger());
        assert!(KeyCombo::new(Modifiers::NONE, KeyCode(0x47)).is_trigger());
        assert!(KeyCombo::new(Modifiers::NONE, KeyCode(0x68)).is_trigger());
        // F21–F24 各系统的键码表里没有，单独按不算；HID 保留区的键码不是按键。
        assert!(!KeyCombo::new(Modifiers::NONE, KeyCode(0x70)).is_trigger());
        assert!(!KeyCombo::new(CTRL_ALT, KeyCode(0xA5)).is_trigger());
        assert!(!KeyCombo::new(CTRL_ALT, KeyCode(0xDF)).is_trigger());
        // 主键不能是修饰键、保留键码或不存在的键。
        assert!(!KeyCombo::new(CTRL_ALT, KeyCode::LEFT_SHIFT).is_trigger());
        assert!(!KeyCombo::new(CTRL_ALT, KeyCode::CAPS_STATE_ON).is_trigger());
        assert!(!KeyCombo::new(CTRL_ALT, KeyCode(0)).is_trigger());
        // 发送的键不要求修饰键。
        assert!(KeyCombo::new(Modifiers::NONE, S).is_sendable());
        assert!(!KeyCombo::new(Modifiers::NONE, KeyCode::LEFT_CTRL).is_sendable());
        assert!(!KeyCombo::new(Modifiers(0x80), S).is_sendable());
    }

    #[test]
    fn keys_put_modifiers_first() {
        let c = KeyCombo::new(Modifiers::CTRL | Modifiers::SHIFT, S);
        assert_eq!(c.keys(), vec![KeyCode::LEFT_CTRL, KeyCode::LEFT_SHIFT, S]);
        assert_eq!(KeyCombo::new(Modifiers::NONE, S).keys(), vec![S]);
    }

    #[test]
    fn tap_lifts_held_modifiers_around_the_keys() {
        let (ctrl, alt, shift) = (KeyCode::LEFT_CTRL, KeyCode::LEFT_ALT, KeyCode::LEFT_SHIFT);
        // 对方按着 Ctrl+Alt 触发映射，要本机按 Shift+S。
        assert_eq!(
            tap_sequence(&[ctrl, alt], &[shift, S]),
            vec![
                (ctrl, false),
                (alt, false),
                (shift, true),
                (S, true),
                (S, false),
                (shift, false),
                (ctrl, true),
                (alt, true),
            ]
        );
        // 没有人按着修饰键：只是按一下、抬起。
        assert_eq!(tap_sequence(&[], &[S]), vec![(S, true), (S, false)]);
    }

    #[test]
    fn device_and_mapping_hotkeys_resolve() {
        let h = Hotkeys {
            devices: vec![DeviceHotkey {
                device: dev("pc"),
                combo: KeyCombo::new(CTRL_ALT, KeyCode(0x1E)),
            }],
            mappings: vec![KeyMapping {
                trigger: KeyCombo::new(CTRL_ALT, S),
                send: KeyCombo::new(Modifiers::NONE, S),
                device: dev("pc"),
            }],
            ..Default::default()
        };
        assert_eq!(
            h.action(CTRL_ALT, KeyCode(0x1E)),
            Some(HotkeyAction::SwitchTo(dev("pc")))
        );
        assert_eq!(
            h.action(CTRL_ALT, S),
            Some(HotkeyAction::Send {
                device: dev("pc"),
                keys: KeyCombo::new(Modifiers::NONE, S),
            })
        );
        assert_eq!(h.action(CTRL_ALT, F5), None);
    }

    #[test]
    fn sanitize_drops_invalid_and_duplicate_entries() {
        let dup = KeyCombo::new(CTRL_ALT, KeyCode::LEFT);
        let h = Hotkeys {
            switch_left: Some(dup),
            // 不带强修饰键：去掉。
            switch_right: Some(KeyCombo::new(Modifiers::SHIFT, KeyCode::RIGHT)),
            devices: vec![
                // 和「切到左边」重复：去掉。
                DeviceHotkey {
                    device: dev("a"),
                    combo: dup,
                },
                DeviceHotkey {
                    device: dev("b"),
                    combo: KeyCombo::new(CTRL_ALT, KeyCode(0x1E)),
                },
                // 同一台电脑的第二个：去掉。
                DeviceHotkey {
                    device: dev("b"),
                    combo: KeyCombo::new(CTRL_ALT, KeyCode(0x1F)),
                },
            ],
            mappings: vec![
                KeyMapping {
                    trigger: KeyCombo::new(CTRL_ALT, S),
                    send: KeyCombo::new(Modifiers::NONE, S),
                    device: dev("b"),
                },
                // 发送的不是真实按键：去掉。
                KeyMapping {
                    trigger: KeyCombo::new(CTRL_ALT, F5),
                    send: KeyCombo::new(Modifiers::NONE, KeyCode::LEFT_ALT),
                    device: dev("b"),
                },
                // 触发键和上一条映射重复：去掉。
                KeyMapping {
                    trigger: KeyCombo::new(CTRL_ALT, S),
                    send: KeyCombo::new(Modifiers::NONE, F5),
                    device: dev("b"),
                },
            ],
        }
        .sanitized();
        assert_eq!(h.switch_left, Some(dup));
        assert_eq!(h.switch_right, None);
        assert_eq!(h.devices.len(), 1);
        assert_eq!(h.devices[0].device, dev("b"));
        assert_eq!(h.mappings.len(), 1);
        assert_eq!(h.mappings[0].send, KeyCombo::new(Modifiers::NONE, S));
        // 整理过的设置再整理一次不变。
        assert_eq!(h.clone().sanitized(), h);
    }

    #[test]
    fn entries_dropped_for_being_over_the_limit_do_not_reserve_their_trigger() {
        // 40 个设备快捷键（超出上限 8 个），最后一个的触发键和唯一一条映射相同。
        let devices: Vec<DeviceHotkey> = (0..40u16)
            .map(|i| DeviceHotkey {
                device: dev(&format!("d{i}")),
                combo: KeyCombo::new(CTRL_ALT, KeyCode(0x04 + i)),
            })
            .collect();
        let last = devices[39].combo;
        let h = Hotkeys {
            devices,
            mappings: vec![KeyMapping {
                trigger: last,
                send: KeyCombo::new(Modifiers::NONE, S),
                device: dev("d0"),
            }],
            ..Default::default()
        }
        .sanitized();
        assert_eq!(h.devices.len(), MAX_HOTKEYS);
        // 第 40 个设备快捷键被丢掉了，它的触发键可以给映射用。
        assert_eq!(h.mappings.len(), 1);
    }

    #[test]
    fn forgetting_a_device_removes_its_hotkeys() {
        let mut h = Hotkeys {
            devices: vec![DeviceHotkey {
                device: dev("a"),
                combo: KeyCombo::new(CTRL_ALT, KeyCode(0x1E)),
            }],
            mappings: vec![KeyMapping {
                trigger: KeyCombo::new(CTRL_ALT, S),
                send: KeyCombo::new(Modifiers::NONE, S),
                device: dev("b"),
            }],
            ..Default::default()
        };
        assert!(!h.forget_device(&dev("c")));
        assert!(h.forget_device(&dev("a")));
        assert!(h.devices.is_empty() && h.mappings.len() == 1);
        assert!(h.forget_device(&dev("b")));
        assert!(h.mappings.is_empty());
    }

    #[test]
    fn json_shape_is_stable() {
        let h = Hotkeys {
            switch_right: None,
            ..Default::default()
        };
        let v = serde_json::to_value(&h).unwrap();
        // 修饰键是位掩码（Ctrl=2、Alt=4），键码是 HID 用法号（← 是 0x50）。界面按这个格式读写。
        assert_eq!(v["switch_left"]["mods"], 6);
        assert_eq!(v["switch_left"]["key"], 0x50);
        assert!(v["switch_right"].is_null());
        assert_eq!(serde_json::from_value::<Hotkeys>(v).unwrap(), h);
    }
}
