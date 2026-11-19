// 快捷键的显示与录制：浏览器按键事件 ↔ USB HID 键码（flow-core keys::KeyCode / hotkeys::KeyCombo）。
//
// 录制用 `KeyboardEvent.code`（物理键位），不用 `key`（打出来的字符）：流界在网络上传的也是物理键位，
// 换了键盘布局、开着输入法都不影响。

import type { KeyCombo } from "./types";
import { isMac } from "./ui";

/** 修饰键位掩码，与 flow-core keys::Modifiers 一致。 */
export const MOD = { SHIFT: 1, CTRL: 2, ALT: 4, META: 8 } as const;

/** `KeyboardEvent.code` → HID 键码（Usage Page 0x07）。修饰键不在表里：它们不能当主键。 */
const CODE_TO_HID: Record<string, number> = {
  Enter: 0x28,
  Escape: 0x29,
  Backspace: 0x2a,
  Tab: 0x2b,
  Space: 0x2c,
  Minus: 0x2d,
  Equal: 0x2e,
  BracketLeft: 0x2f,
  BracketRight: 0x30,
  Backslash: 0x31,
  Semicolon: 0x33,
  Quote: 0x34,
  Backquote: 0x35,
  Comma: 0x36,
  Period: 0x37,
  Slash: 0x38,
  CapsLock: 0x39,
  PrintScreen: 0x46,
  ScrollLock: 0x47,
  Pause: 0x48,
  Insert: 0x49,
  Home: 0x4a,
  PageUp: 0x4b,
  Delete: 0x4c,
  End: 0x4d,
  PageDown: 0x4e,
  ArrowRight: 0x4f,
  ArrowLeft: 0x50,
  ArrowDown: 0x51,
  ArrowUp: 0x52,
  NumLock: 0x53,
  NumpadDivide: 0x54,
  NumpadMultiply: 0x55,
  NumpadSubtract: 0x56,
  NumpadAdd: 0x57,
  NumpadEnter: 0x58,
  Numpad0: 0x62,
  NumpadDecimal: 0x63,
  NumpadEqual: 0x67,
  IntlBackslash: 0x64,
  ContextMenu: 0x65,
};
// A–Z：0x04 起。
for (let i = 0; i < 26; i++) CODE_TO_HID[`Key${String.fromCharCode(65 + i)}`] = 0x04 + i;
// 1–9：0x1E 起，0 在最后。
for (let i = 1; i <= 9; i++) CODE_TO_HID[`Digit${i}`] = 0x1d + i;
CODE_TO_HID.Digit0 = 0x27;
// F1–F12：0x3A 起；F13–F20：0x68 起。F21–F24 各系统的键码表（flow-platform keymap）里没有，录了也不会触发。
for (let i = 1; i <= 12; i++) CODE_TO_HID[`F${i}`] = 0x39 + i;
for (let i = 13; i <= 20; i++) CODE_TO_HID[`F${i}`] = 0x68 + (i - 13);
// 小键盘 1–9：0x59 起。
for (let i = 1; i <= 9; i++) CODE_TO_HID[`Numpad${i}`] = 0x58 + i;

/** HID 键码 → 显示的名字。 */
const HID_LABEL: Record<number, string> = {
  0x28: "回车",
  0x29: "Esc",
  0x2a: "退格",
  0x2b: "Tab",
  0x2c: "空格",
  0x2d: "-",
  0x2e: "=",
  0x2f: "[",
  0x30: "]",
  0x31: "\\",
  0x33: ";",
  0x34: "'",
  0x35: "`",
  0x36: ",",
  0x37: ".",
  0x38: "/",
  0x39: "Caps Lock",
  0x46: "Print Screen",
  0x47: "Scroll Lock",
  0x48: "Pause",
  0x49: "Insert",
  0x4a: "Home",
  0x4b: "Page Up",
  0x4c: "Delete",
  0x4d: "End",
  0x4e: "Page Down",
  0x4f: "→",
  0x50: "←",
  0x51: "↓",
  0x52: "↑",
  0x53: "Num Lock",
  0x54: "小键盘 /",
  0x55: "小键盘 *",
  0x56: "小键盘 -",
  0x57: "小键盘 +",
  0x58: "小键盘回车",
  0x62: "小键盘 0",
  0x63: "小键盘 .",
  0x67: "小键盘 =",
  0x64: "ISO \\",
  0x65: "菜单键",
};
for (let i = 0; i < 26; i++) HID_LABEL[0x04 + i] = String.fromCharCode(65 + i);
for (let i = 1; i <= 9; i++) HID_LABEL[0x1d + i] = String(i);
HID_LABEL[0x27] = "0";
for (let i = 1; i <= 12; i++) HID_LABEL[0x39 + i] = `F${i}`;
for (let i = 13; i <= 20; i++) HID_LABEL[0x68 + (i - 13)] = `F${i}`;
for (let i = 1; i <= 9; i++) HID_LABEL[0x58 + i] = `小键盘 ${i}`;

/** 主键的名字；表里没有的键显示成键码，至少能认出来是哪一条。 */
export function keyLabel(hid: number): string {
  return HID_LABEL[hid] ?? `键 0x${hid.toString(16).toUpperCase()}`;
}

/**
 * 修饰键的名字，按 `mac` 所指的系统习惯：Mac 上是 ⌃⌥⇧⌘，其他系统是 Ctrl、Alt、Shift、Win。
 * 触发键按手边这台电脑的习惯显示；发给别的电脑的键按那台电脑的系统显示。
 */
export function modLabels(mods: number, mac: boolean = isMac): string[] {
  const out: string[] = [];
  if (mods & MOD.CTRL) out.push(mac ? "⌃" : "Ctrl");
  if (mods & MOD.ALT) out.push(mac ? "⌥" : "Alt");
  if (mods & MOD.SHIFT) out.push(mac ? "⇧" : "Shift");
  if (mods & MOD.META) out.push(mac ? "⌘" : "Win");
  return out;
}

/** 一组按键里每个键帽上的字，修饰键在前。 */
export function comboLabels(c: KeyCombo, mac: boolean = isMac): string[] {
  return [...modLabels(c.mods, mac), keyLabel(c.key)];
}

export function sameCombo(a: KeyCombo | null, b: KeyCombo | null): boolean {
  return !!a && !!b && a.mods === b.mods && a.key === b.key;
}

/** 按下的是不是修饰键本身（录制时等它后面的主键）。 */
export function isModifierCode(code: string): boolean {
  return /^(Control|Alt|Shift|Meta|OS)(Left|Right)$/.test(code);
}

/** 从按键事件读出一组按键；按下的不是能录的键（表里没有）时返回 null。 */
export function comboFromEvent(e: { code: string; ctrlKey: boolean; altKey: boolean; shiftKey: boolean; metaKey: boolean }): KeyCombo | null {
  const key = CODE_TO_HID[e.code];
  if (key === undefined) return null;
  const mods = (e.ctrlKey ? MOD.CTRL : 0) | (e.altKey ? MOD.ALT : 0) | (e.shiftKey ? MOD.SHIFT : 0) | (e.metaKey ? MOD.META : 0);
  return { mods, key };
}

const isFunctionKey = (hid: number) => (hid >= 0x3a && hid <= 0x48) || (hid >= 0x68 && hid <= 0x6f);

/**
 * 能不能当触发键（与 flow-core hotkeys::KeyCombo::is_trigger 一致）：要带 Ctrl、Alt 或 ⌘ / Win 之一，
 * 否则平时打字就会触发；功能键可以单独用。
 */
export function isTrigger(c: KeyCombo): boolean {
  return (c.mods & (MOD.CTRL | MOD.ALT | MOD.META)) !== 0 || isFunctionKey(c.key);
}

/** 为什么这组键不能当触发键（给用户看的话）。 */
export const TRIGGER_RULE = `要带上 ${isMac ? "⌃、⌥ 或 ⌘" : "Ctrl、Alt 或 Win"} 中的至少一个（F1–F20 可以单独用）`;
