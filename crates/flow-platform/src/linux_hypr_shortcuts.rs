//! Hyprland 的全局快捷键：把快捷键映射注册成合成器的快捷键，按下时交给流界。
//!
//! Wayland 的普通程序看不到全局键盘（见 `linux_wlr_input.rs`），Hyprland 为此提供了
//! `hyprland-global-shortcuts-v1`：程序注册一组快捷键，合成器在用户按下时通知它。协议只负责通知，
//! 触发键还要在合成器里绑上（`global, <appid>:<id>`），所以这里同时做两件事：
//!
//! - 在单独的线程里连着合成器，注册协议对象；按下时按 id 调用回调（界面上看到的是一次按键，
//!   不用先运行一个流界进程，和别的系统一样没有额外延迟）。
//! - 用 `hyprctl` 把每个触发键绑到 `global, <appid>:<id>`。Hyprland 0.57 起配置换成了 Lua，
//!   运行时绑定用 `hyprctl eval 'hl.bind(…)'`；更早的版本用 `hyprctl keyword bind …`，两种都支持。
//!   配置文件重载会清掉这些运行时绑定，所以盯着合成器的 `.socket2.sock`，收到 `configreloaded`
//!   就重新绑定。
//!
//! 触发键在合成器里按物理键位（`code:<xkb 键码>`）绑，和流界其他地方一致：字符仍由本机布局决定。
//! 协议文件来自 Hyprland（`protocols/hyprland-global-shortcuts-v1.xml`，BSD 三条款许可，版权见文件头）。

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use flow_core::hotkeys::KeyCombo;
use flow_core::keys::Modifiers;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};

use crate::keymap;
use crate::linux_wl;
use crate::{LocalShortcut, PlatformError};

mod protocol {
    //! `hyprland-global-shortcuts-v1` 的客户端绑定，由 `wayland-scanner` 从 `protocols/` 里的 XML 生成。
    //! 生成代码的警告不由我们控制。
    #![allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
    #![allow(non_upper_case_globals, non_snake_case, unused_imports)]
    #![allow(missing_docs, clippy::all)]

    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;

        wayland_scanner::generate_interfaces!("protocols/hyprland-global-shortcuts-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/hyprland-global-shortcuts-v1.xml");
}

use protocol::hyprland_global_shortcut_v1::{Event as ShortcutEvent, HyprlandGlobalShortcutV1};
use protocol::hyprland_global_shortcuts_manager_v1::HyprlandGlobalShortcutsManagerV1;

/// 注册到合成器的应用标识（和打包的应用标识一致；`global, <appid>:<id>` 靠它找到流界）。
const APP_ID: &str = "app.flowacross.desktop";

/// 按下的快捷键怎么通知引擎。
type Handler = Box<dyn FnMut(&str) + Send>;

static HANDLER: Mutex<Option<Handler>> = Mutex::new(None);

/// 引擎装一个回调：合成的快捷键被按下时调用它（参数是 [`LocalShortcut::id`]）。
pub(crate) fn set_handler(handler: Handler) {
    *HANDLER.lock().unwrap_or_else(|e| e.into_inner()) = Some(handler);
}

fn pressed(id: &str) {
    if let Some(handler) = HANDLER.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        handler(id);
    }
}

/// 和 Hyprland 的绑定怎么写。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindStyle {
    /// Hyprland 0.57 起（配置是 Lua）：`hyprctl eval 'hl.bind(…)'`。
    Lua,
    /// 更早的版本：`hyprctl keyword bind …`。
    Classic,
}

enum Request {
    /// 换成这一组快捷键。带 `Sender` 时，处理完（绑定也写好了）回一个信号。
    Set(Vec<LocalShortcut>, Option<Sender<()>>),
    /// Hyprland 重载了配置：运行时绑定被清掉，按当前设置重新绑。
    Rebind,
}

struct Service {
    tx: Sender<Request>,
}

static SERVICE: OnceLock<Result<Service, String>> = OnceLock::new();

/// 换一组要注册到 Hyprland 的快捷键，整体替换上一组。注册在后台线程上进行，不阻塞调用方。
pub(crate) fn set(shortcuts: Vec<LocalShortcut>) -> Result<(), PlatformError> {
    // 第一条映射出现之前不用连合成器；引擎每次同步都会调用这里。
    if shortcuts.is_empty() && SERVICE.get().is_none() {
        return Ok(());
    }
    let service = match SERVICE.get_or_init(|| start_service().map_err(|e| e.to_string())) {
        Ok(service) => service,
        Err(reason) => return Err(PlatformError::Os(reason.clone())),
    };
    service
        .tx
        .send(Request::Set(shortcuts, None))
        .map_err(|_| PlatformError::Os("Hyprland 快捷键线程已经停止".into()))
}

/// 撤销全部注册（退出时调用，等绑定真的删掉再返回）。没启动过、或注册失败时什么都不做。
pub(crate) fn clear() {
    let Some(Ok(service)) = SERVICE.get() else {
        return;
    };
    let (ack, done) = mpsc::channel();
    if service.tx.send(Request::Set(Vec::new(), Some(ack))).is_ok() {
        let _ = done.recv_timeout(Duration::from_secs(2));
    }
}

/// 连上合成器、绑定 `hyprland-global-shortcuts-v1`，起两个线程：一个收合成器的事件，
/// 一个处理设置变化（注册 / 撤销协议对象、改 Hyprland 的绑定）。
fn start_service() -> Result<Service, String> {
    let conn = linux_wl::connect().map_err(|e| e.to_string())?;
    let (globals, queue): (_, EventQueue<State>) =
        registry_queue_init(&conn).map_err(|e| format!("读取 Wayland 协议列表失败：{e}"))?;
    let qh = queue.handle();
    let manager: HyprlandGlobalShortcutsManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("Hyprland 没有提供全局快捷键协议：{e}"))?;
    let style = detect_style()?;
    let (tx, rx) = mpsc::channel();

    // Hyprland 重载配置时会把运行时加的绑定清掉，重新绑一次。
    if let Some(socket) = reload_socket() {
        let tx = tx.clone();
        let _ = thread::Builder::new()
            .name("flowacross-hypr-reload".into())
            .spawn(move || watch_reloads(socket, tx));
    }

    thread::Builder::new()
        .name("flowacross-hypr-shortcuts".into())
        .spawn(move || {
            let mut queue = queue;
            let mut state = State;
            loop {
                match queue.blocking_dispatch(&mut state) {
                    Ok(_) => {}
                    Err(e) => {
                        // 合成器退出（或连接断了）：等进程结束。重新连上要重新注册，
                        // 而 Hyprland 退出通常意味着整个会话要结束了。
                        eprintln!("Hyprland 快捷键连接断开：{e}");
                        break;
                    }
                }
            }
        })
        .map_err(|e| format!("启动 Hyprland 快捷键线程失败：{e}"))?;

    thread::Builder::new()
        .name("flowacross-hypr-bind".into())
        .spawn(move || command_loop(conn, manager, qh, style, rx))
        .map_err(|e| format!("启动 Hyprland 绑定线程失败：{e}"))?;

    Ok(Service { tx })
}

/// 收合成器的事件：只有 `pressed` 有用（`released` 也要读掉，不然队列会一直积着）。
struct State;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<HyprlandGlobalShortcutsManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &HyprlandGlobalShortcutsManagerV1,
        _: <HyprlandGlobalShortcutsManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<HyprlandGlobalShortcutV1, String> for State {
    fn event(
        _: &mut Self,
        _: &HyprlandGlobalShortcutV1,
        event: ShortcutEvent,
        id: &String,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, ShortcutEvent::Pressed { .. }) {
            pressed(id);
        }
    }
}

/// 现在注册到 Hyprland 里的东西：协议对象和绑定的触发键，撤销时按这个来。
struct Bindings {
    specs: Vec<LocalShortcut>,
    objects: HashMap<String, (LocalShortcut, HyprlandGlobalShortcutV1)>,
    /// 绑在 Hyprland 里的触发键（同一种写法，撤销时原样用）。
    keys: Vec<String>,
}

fn command_loop(
    conn: Connection,
    manager: HyprlandGlobalShortcutsManagerV1,
    qh: QueueHandle<State>,
    style: BindStyle,
    rx: Receiver<Request>,
) {
    let mut bindings = Bindings {
        specs: Vec::new(),
        objects: HashMap::new(),
        keys: Vec::new(),
    };
    while let Ok(command) = rx.recv() {
        let (specs, force, ack) = match command {
            Request::Set(specs, ack) => (specs, false, ack),
            // 重载后重新绑一次；协议对象还在，不用动。
            Request::Rebind => (bindings.specs.clone(), true, None),
        };
        apply(&conn, &manager, &qh, style, specs, force, &mut bindings);
        if let Some(ack) = ack {
            let _ = ack.send(());
        }
    }
}

/// 把 Hyprland 里的绑定和协议注册换成 `specs`。任何一步失败都只记日志：下次设置变化时会再试。
/// `force` 为真时内容没变也重新绑（Hyprland 重载配置后运行时绑定没了）。
fn apply(
    conn: &Connection,
    manager: &HyprlandGlobalShortcutsManagerV1,
    qh: &QueueHandle<State>,
    style: BindStyle,
    specs: Vec<LocalShortcut>,
    force: bool,
    bindings: &mut Bindings,
) {
    let specs = normalize(specs);
    // 引擎每次同步都会调用这里，内容没变就别再调 hyprctl。
    if !force && specs == bindings.specs {
        return;
    }
    // 内容变了的（触发键、说明、目标）先撤销，再按新的注册。
    let stale: Vec<String> = bindings
        .objects
        .iter()
        .filter(|(id, (old, _))| specs.iter().find(|s| &s.id == *id) != Some(old))
        .map(|(id, _)| id.clone())
        .collect();
    for id in stale {
        if let Some((_, shortcut)) = bindings.objects.remove(&id) {
            shortcut.destroy();
        }
    }
    for spec in &specs {
        if bindings.objects.contains_key(&spec.id) {
            continue;
        }
        let trigger = key_string(style, &spec.combo).unwrap_or_default();
        let shortcut = manager.register_shortcut(
            spec.id.clone(),
            APP_ID.to_owned(),
            spec.description.clone(),
            trigger,
            qh,
            spec.id.clone(),
        );
        bindings
            .objects
            .insert(spec.id.clone(), (spec.clone(), shortcut));
    }
    if let Err(e) = conn.flush() {
        eprintln!("注册 Hyprland 快捷键失败：{e}");
    }
    bindings.keys = apply_binds(style, &bindings.keys, &specs);
    bindings.specs = specs;
}

/// 去掉重复和绑不了的：同一 id 只留第一个，键码表里查不到的组合（[`keymap::to_linux`] 为空）
/// 跳过，不带修饰键又不是功能键的也不绑（那等于把整个按键从所有程序手里抢走）。
fn normalize(specs: Vec<LocalShortcut>) -> Vec<LocalShortcut> {
    let mut seen = HashSet::new();
    specs
        .into_iter()
        .filter(|s| !s.id.is_empty() && s.combo.is_trigger() && key_number(s.combo).is_some())
        .filter(|s| seen.insert(s.id.clone()))
        .collect()
}

/// 触发键在 Hyprland 里的写法：
/// - Lua（0.57 起）：`CTRL + ALT + code:39`；
/// - 更早的版本：`CTRL ALT, code:39`。
///
/// 用键码而不是键名：`code:` 是 xkb 键码（evdev 加 8），和本机键盘布局无关，流界各处的键码表正好能换。
fn key_string(style: BindStyle, combo: &KeyCombo) -> Option<String> {
    let code = key_number(*combo)?;
    let mut mods = Vec::new();
    for (mask, name) in [
        (Modifiers::CTRL, "CTRL"),
        (Modifiers::ALT, "ALT"),
        (Modifiers::SHIFT, "SHIFT"),
        (Modifiers::META, "SUPER"),
    ] {
        if combo.mods.contains(mask) {
            mods.push(name);
        }
    }
    let key = format!("code:{code}");
    Some(match style {
        BindStyle::Lua => {
            let mut parts: Vec<String> = mods.iter().map(|s| (*s).to_owned()).collect();
            parts.push(key);
            parts.join(" + ")
        }
        BindStyle::Classic if mods.is_empty() => key,
        BindStyle::Classic => format!("{}, {key}", mods.join(" ")),
    })
}

/// HID 键码 → xkb 键码（evdev 加 8）；键码表里没有的键返回 `None`。
fn key_number(combo: KeyCombo) -> Option<u16> {
    keymap::to_linux(combo.key)?.checked_add(8)
}

/// 在 Hyprland 里换掉绑定：先删掉上一次的（以及这次要用的，清掉上次崩溃留下的），再按现在的绑。
/// 返回这次之后应该绑定着的触发键，供下一次撤销、以及退出时清理。
///
/// Hyprland 的 `unbind` 要求修饰键、键名、键码都一样才删：这里一律写 `code:<键码>`，所以只会删掉
/// 同样用 `code:` 写的绑定（我们自己的，或用户在配置里恰好用同一个键码写的），用户按键名写的
/// 绑定（`CTRL ALT, S`）不受影响——那种情况下按一次，两边的动作都会执行。
fn apply_binds(style: BindStyle, old: &[String], specs: &[LocalShortcut]) -> Vec<String> {
    // 能绑的（触发键在键码表里查得到），按设置里的顺序。
    let keys: Vec<(String, &LocalShortcut)> = specs
        .iter()
        .filter_map(|s| Some((key_string(style, &s.combo)?, s)))
        .collect();
    let mut unbind = old.to_vec();
    for (key, _) in &keys {
        if !unbind.contains(key) {
            unbind.push(key.clone());
        }
    }
    match style {
        BindStyle::Lua => {
            let mut script = String::new();
            for key in &unbind {
                script.push_str(&format!("hl.unbind({})\n", lua_string(key)));
            }
            for (key, spec) in &keys {
                script.push_str(&format!(
                    "hl.bind({}, hl.dsp.global({}), {{description = {}}})\n",
                    lua_string(key),
                    lua_string(&format!("{APP_ID}:{}", spec.id)),
                    lua_string(&spec.description)
                ));
            }
            if !script.is_empty() {
                run_hyprctl(&["eval", &script]);
            }
        }
        BindStyle::Classic => {
            for key in &unbind {
                run_hyprctl(&["keyword", "unbind", key]);
            }
            for (key, spec) in &keys {
                run_hyprctl(&[
                    "keyword",
                    "bind",
                    &format!("{key}, global, {APP_ID}:{}", spec.id),
                ]);
            }
        }
    }
    keys.into_iter().map(|(key, _)| key).collect()
}

/// Lua 字符串字面量。说明文字是我们自己生成的，但仍然照规矩转义（将来可能带上设备名）。
fn lua_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\{:03}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 是 Lua 配置（0.57 起）还是老的 hyprlang 配置。老版本没有 `eval`，会回 "unknown request"。
fn detect_style() -> Result<BindStyle, String> {
    match hyprctl(&["eval", "return 1"]) {
        Ok(out) if !out.contains("unknown request") => Ok(BindStyle::Lua),
        Ok(_) => Ok(BindStyle::Classic),
        Err(e) => Err(format!(
            "找不到可用的 hyprctl（Hyprland 的命令行工具）：{e}"
        )),
    }
}

fn run_hyprctl(args: &[&str]) {
    if let Err(e) = hyprctl(args) {
        eprintln!("hyprctl {} 失败：{e}", args.first().copied().unwrap_or(""));
    }
}

/// 运行 `hyprctl`，返回标准输出。失败时把错误输出（或系统错误）带回来。
fn hyprctl(args: &[&str]) -> Result<String, String> {
    let out = Command::new("hyprctl")
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let reason = stderr.trim();
        return Err(match reason.is_empty() {
            true => format!("退出码 {}", out.status.code().unwrap_or(-1)),
            false => reason.to_owned(),
        });
    }
    if stdout.starts_with("unknown request") {
        // `keyword` 的未知请求也是退出码 0，由调用方看内容。
        return Ok(stdout);
    }
    Ok(stdout)
}

/// Hyprland 事件套接字（`.socket2.sock`）的位置：新版本在 `$XDG_RUNTIME_DIR/hypr/<签名>/`，
/// 更早的在 `/tmp/hypr/<签名>/`。
fn reload_socket() -> Option<PathBuf> {
    let sig = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    let newer = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .map(|d| d.join("hypr").join(&sig).join(".socket2.sock"))
        .filter(|p| p.exists());
    newer.or_else(|| {
        let older = PathBuf::from("/tmp/hypr").join(&sig).join(".socket2.sock");
        older.exists().then_some(older)
    })
}

/// 读 Hyprland 的事件流：配置文件重载后让绑定线程重新绑定。
fn watch_reloads(path: PathBuf, tx: Sender<Request>) {
    let Ok(stream) = UnixStream::connect(&path) else {
        return;
    };
    for line in BufReader::new(stream).lines().map_while(Result::ok) {
        if line.starts_with("configreloaded") && tx.send(Request::Rebind).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flow_core::keys::KeyCode;

    fn ctrl_alt(key: KeyCode) -> KeyCombo {
        KeyCombo::new(Modifiers::CTRL | Modifiers::ALT, key)
    }

    #[test]
    fn trigger_keys_use_physical_keycodes() {
        // S：HID 0x16 → evdev 31 → xkb 39。
        let s = KeyCombo::new(Modifiers::CTRL | Modifiers::ALT, KeyCode(0x16));
        assert_eq!(
            key_string(BindStyle::Lua, &s).unwrap(),
            "CTRL + ALT + code:39"
        );
        assert_eq!(
            key_string(BindStyle::Classic, &s).unwrap(),
            "CTRL ALT, code:39"
        );
        // 修饰键的顺序固定；⌘ / Win 在 Hyprland 里是 SUPER。
        let meta_shift = KeyCombo::new(Modifiers::META | Modifiers::SHIFT, KeyCode::A);
        assert_eq!(
            key_string(BindStyle::Lua, &meta_shift).unwrap(),
            "SHIFT + SUPER + code:38"
        );
        assert_eq!(
            key_string(BindStyle::Classic, &meta_shift).unwrap(),
            "SHIFT SUPER, code:38"
        );
        // 功能键可以不带修饰键；← 是 xkb 113。
        let f5 = KeyCombo::new(Modifiers::NONE, KeyCode(0x3E));
        assert_eq!(key_string(BindStyle::Lua, &f5).unwrap(), "code:71");
        let left = KeyCombo::new(Modifiers::NONE, KeyCode::LEFT);
        assert_eq!(key_string(BindStyle::Classic, &left).unwrap(), "code:113");
        // 多媒体键（借用键盘页的 0xE8–0xEB）也能绑。
        let play = KeyCombo::new(Modifiers::NONE, KeyCode::PLAY_PAUSE);
        assert_eq!(key_string(BindStyle::Lua, &play).unwrap(), "code:172");
    }

    #[test]
    fn unknown_keycodes_are_skipped() {
        // 键码表里没有的键（这个值只在保留区里）绑不了。
        let odd = KeyCombo::new(Modifiers::CTRL, KeyCode(0xA0));
        assert_eq!(key_number(odd), None);
        assert_eq!(key_string(BindStyle::Lua, &odd), None);
        assert_eq!(key_string(BindStyle::Classic, &odd), None);
        let specs = normalize(vec![
            LocalShortcut {
                id: "a".into(),
                combo: odd,
                description: "x".into(),
            },
            LocalShortcut {
                id: "".into(),
                combo: ctrl_alt(KeyCode(0x16)),
                description: "x".into(),
            },
            LocalShortcut {
                id: "b".into(),
                combo: ctrl_alt(KeyCode(0x16)),
                description: "x".into(),
            },
            LocalShortcut {
                id: "b".into(),
                combo: ctrl_alt(KeyCode(0x17)),
                description: "y".into(),
            },
        ]);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].id, "b");
        assert_eq!(specs[0].combo.key, KeyCode(0x16));
    }

    #[test]
    fn plain_keys_are_not_taken_from_other_programs() {
        // 没带修饰键、又不是功能键的按键不能注册成全局快捷键：那会把整个按键抢走。
        let specs = normalize(vec![LocalShortcut {
            id: "mapping:0".into(),
            combo: KeyCombo::new(Modifiers::NONE, KeyCode(0x16)),
            description: "x".into(),
        }]);
        assert!(specs.is_empty());
        // 功能键可以单独用。
        let f = normalize(vec![LocalShortcut {
            id: "mapping:0".into(),
            combo: KeyCombo::new(Modifiers::NONE, KeyCode(0x3E)),
            description: "x".into(),
        }]);
        assert_eq!(f.len(), 1);
    }

    #[test]
    fn lua_strings_are_escaped() {
        assert_eq!(
            lua_string("CTRL + ALT + code:39"),
            "\"CTRL + ALT + code:39\""
        );
        assert_eq!(lua_string("流界：快捷键映射 1"), "\"流界：快捷键映射 1\"");
        assert_eq!(lua_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(lua_string("a\nb"), "\"a\\nb\"");
    }

    #[test]
    fn normalize_keeps_the_first_of_each_id_order() {
        let specs = normalize(vec![
            LocalShortcut {
                id: "mapping:0".into(),
                combo: ctrl_alt(KeyCode(0x16)),
                description: "第一条".into(),
            },
            LocalShortcut {
                id: "mapping:1".into(),
                combo: ctrl_alt(KeyCode(0x17)),
                description: "第二条".into(),
            },
        ]);
        assert_eq!(
            specs.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["mapping:0", "mapping:1"]
        );
    }
}
