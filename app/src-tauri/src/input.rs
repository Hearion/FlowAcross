//! 跨屏控制器：根据捕获到的本机输入决定"留在本机"还是"发给远端"，并负责切换。
//!
//! 捕获回调在系统钩子线程上同步调用 [`Controller::handle`]，所以这里只做内存计算和
//! 投递到通道（不做 IO、不拿引擎状态锁）。引擎在状态变化时调用 [`Controller::configure`]，
//! 并定时调用 [`Controller::tick`]（心跳、贴边停留计时）。
//!
//! 失败路径（连接断开、对方被暂停、显示器消失）一律先释放远端按键、恢复本机光标，再通知界面。

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use flow_core::Point;
use flow_core::cursor::VirtualCursor;
use flow_core::hotkeys::{HotkeyAction, Hotkeys, KeyCombo};
use flow_core::keys::{InputEvent, InputRouter, KeyCode, Modifiers, Route};
use flow_core::layout::{Crossing, DeviceId, DisplayId, EdgeMapping, Layout};
use flow_core::protocol::Message;
use flow_core::switching::{Decision, EdgeSwitcher, HeldInput, SwitchPolicy};
use flow_platform::{CapturedEvent, Verdict};
use tokio::sync::mpsc;
use tracing::info;

pub type Output = mpsc::UnboundedSender<Message>;

/// 光标锁定 / 恢复（测试里替换成记录调用的闭包）。
pub type CursorControl = Box<dyn Fn(bool, Option<(f64, f64)>) + Send>;

const HEARTBEAT: Duration = Duration::from_millis(500);
/// 对方支持心跳回执时，这么久没收到回执就认为网络断了，键鼠回到本机（约 3 个心跳周期）。
const ACK_TIMEOUT: Duration = Duration::from_millis(1500);
/// 别的设备的键鼠正在操作本机时，最近这么久内有它的输入进来，本机就不从屏幕边切出去。
///
/// 同一时刻只能有一个控制方，谁的键鼠在动谁就是源。对方的光标刚进来时正贴着通往它的那条边，
/// 这时本机只要冒出一点朝那条边的位移（鼠标传感器的抖动、别的软件合成的移动）就会被当成
/// "本机的人把键鼠推回去了"，反过来接管对方；对方的人还在推，马上又切回来——两台电脑在 0.1 秒里
/// 互相接管好几次，一只鼠标同时带着两边的光标跑（用户反馈，日志里能看到连续的"本机被接管"）。
/// 对方停手这么久之后，本机的人照常可以把键鼠推过去。
const TAKEOVER_GUARD: Duration = Duration::from_millis(300);

/// 控制器通知引擎的事件。
#[derive(Debug, Clone, PartialEq)]
pub enum ControlEvent {
    /// 键鼠所在的设备变了。
    Changed,
    /// 正在控制的设备不可用了（断线、被暂停、被移除），键鼠已回到本机。
    Lost(DeviceId),
    /// 键鼠从远端穿过屏幕边回到了本机的这个位置（布局坐标）。跟在 `Changed` 后面发，
    /// 只用于在这条边上闪一道浮光；快捷键、断线等其他方式回来时不发。
    Returned(Point),
    /// 引擎标记过"键鼠被别的设备带走了"（[`Controller::set_away`]）之后，本机第一次有键鼠输入。
    LocalInput,
}

/// 引擎提供给控制器的配置快照。
#[derive(Default)]
pub struct ControllerConfig {
    /// 只包含本机和当前可控制的设备的显示器。
    pub layout: Layout,
    /// 本机显示器在布局空间中的偏移：布局坐标 = 本机系统坐标 + 偏移。
    pub local_offset: (f64, f64),
    /// 可控制的设备及其消息通道。
    pub outputs: HashMap<DeviceId, Output>,
    pub policy: SwitchPolicy,
    pub mapping: EdgeMapping,
    /// 控制远端时滚轮速度的百分比（见 `Settings::wheel_speed`）；0 当作 100。
    pub wheel_percent: u32,
    /// 共享未暂停。
    pub enabled: bool,
    /// 会回复心跳回执的设备（版本不低于 `SECURE_SINCE`）。只对它们做快速断线检测。
    pub acks: HashSet<DeviceId>,
    /// 这台电脑上的快捷键（左右切换、切到指定电脑、快捷键映射）。
    pub hotkeys: Hotkeys,
    /// 认得 `Message::KeyStroke` 的设备（版本不低于 `REMOTE_ACTIONS_SINCE`），快捷键映射只发给它们。
    pub remote_keys: HashSet<DeviceId>,
}

struct Active {
    device: DeviceId,
    tx: Output,
    cursor: VirtualCursor,
    session: u32,
    last_beat: Instant,
    /// 最近一次收到对方心跳回执（或开始控制）的时间。
    last_ack: Instant,
    /// 这次控制转发的按键数（写进日志，便于排查"键盘没过去"）。
    keys_sent: u32,
    /// 这次控制收到的本机鼠标移动事件数和开始时间（写进日志，便于排查卡顿：看鼠标回报率）。
    moves: u32,
    started: Instant,
}

pub struct Controller {
    local: DeviceId,
    cfg: ControllerConfig,
    switcher: EdgeSwitcher,
    router: InputRouter,
    active: Option<Active>,
    next_session: u32,
    heartbeat_seq: u32,
    epoch: Instant,
    events: mpsc::UnboundedSender<ControlEvent>,
    cursor: CursorControl,
    /// 触发了快捷键的方向键：它们的连发和抬起都拦截掉。
    swallow: HashSet<KeyCode>,
    /// 最近一次本机光标位置（布局坐标），用快捷键切回本机时放回这里。
    last_local: Option<Point>,
    return_to: Option<Point>,
    ack_timeout: Duration,
    /// 引擎认为键鼠被别的设备带走了；本机一有输入就通知它（只通知一次）。
    away: bool,
    /// 平台层最近一次上报的本机大写锁定状态（见 `CapturedEvent::CapsLock`）。
    caps: Option<bool>,
    /// 直接查询本机大写锁定状态的办法（macOS、Windows 有；Linux 靠上面的事件）。
    caps_probe: Option<CapsProbe>,
    /// 按滚轮速度换算后不足一个单位的余量（横向、纵向，单位是 1/100 个滚动单位）。
    wheel_rest: (i32, i32),
    /// 暂时不响应快捷键：设置页正在录制新的快捷键，按下的键要原样交给界面。
    hotkeys_suspended: bool,
    /// 物理上正按着的修饰键。单独记一份，不从按键路由里算：键鼠从一台远端切走时，路由会把
    /// 在那台远端按下的键全部"释放"并忘掉，而手其实还按着——不松开 Ctrl+Alt 接着按第二个
    /// 快捷键时就对不上了，主键还会被当成普通键打到新的那台电脑上。
    mods_down: HashSet<KeyCode>,
    /// 最近一次有别的设备的键鼠输入注入到本机的时间（见 [`TAKEOVER_GUARD`]）；对方离开后清掉。
    remote_input_at: Option<Instant>,
    takeover_guard: Duration,
}

/// 查询本机大写锁定是否打开；查不到时返回 `None`。
pub type CapsProbe = Box<dyn Fn() -> Option<bool> + Send>;

/// 光标移到距离边缘不足 1 个逻辑点、且还在朝边缘移动时，视为"推出边缘"。
/// 系统会把光标限制在屏幕内，贴边后继续推可能没有位移，所以到达最后一列像素就算数。
fn nudge(rect: flow_core::Rect, p: Point, dx: f64, dy: f64) -> (f64, f64) {
    let mut d = (dx, dy);
    if dx > 0.0 && p.x + dx >= rect.right() - 1.0 {
        d.0 = dx.max(rect.right() - p.x + 0.01);
    }
    if dx < 0.0 && p.x + dx <= rect.left() + 1.0 {
        d.0 = dx.min(rect.left() - p.x - 0.01);
    }
    if dy > 0.0 && p.y + dy >= rect.bottom() - 1.0 {
        d.1 = dy.max(rect.bottom() - p.y + 0.01);
    }
    if dy < 0.0 && p.y + dy <= rect.top() + 1.0 {
        d.1 = dy.min(rect.top() - p.y - 0.01);
    }
    d
}

/// 按滚轮速度（百分比，0 当作 100）换算一次滚动量。除不尽的部分留在 `rest` 里攒到下一次，
/// 这样慢速滚动（触控板一次只有几个单位）乘上小于 1 的倍率也不会一直是 0。
fn scale_wheel(delta: i32, percent: u32, rest: &mut i32) -> i32 {
    let percent = if percent == 0 { 100 } else { percent as i64 };
    let total = i64::from(delta) * percent + i64::from(*rest);
    *rest = (total % 100) as i32;
    (total / 100).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// 告诉对方本机大写锁定的状态。借用按键消息和保留键码（见 `KeyCode::CAPS_STATE_ON`），
/// 只发"抬起"，不认识它的旧版本会直接忽略。
fn caps_message(on: bool) -> Message {
    Message::Key {
        code: KeyCode::caps_state(on),
        down: false,
        repeat: false,
    }
}

fn input_message(ev: InputEvent) -> Message {
    match ev {
        InputEvent::Key { code, down, repeat } => Message::Key { code, down, repeat },
        InputEvent::Button { button, down } => Message::Button { button, down },
    }
}

impl Controller {
    pub fn new(
        local: DeviceId,
        events: mpsc::UnboundedSender<ControlEvent>,
        cursor: CursorControl,
    ) -> Self {
        Self {
            local,
            cfg: ControllerConfig::default(),
            switcher: EdgeSwitcher::default(),
            router: InputRouter::default(),
            active: None,
            next_session: 0,
            heartbeat_seq: 0,
            epoch: Instant::now(),
            events,
            cursor,
            swallow: HashSet::new(),
            last_local: None,
            return_to: None,
            ack_timeout: ACK_TIMEOUT,
            away: false,
            caps: None,
            caps_probe: None,
            wheel_rest: (0, 0),
            hotkeys_suspended: false,
            mods_down: HashSet::new(),
            remote_input_at: None,
            takeover_guard: TAKEOVER_GUARD,
        }
    }

    /// 别的设备的键鼠刚刚操作了本机（进入、移动、点击、滚动、按键；心跳这类通知不算）。
    /// 之后一小段时间内本机不从屏幕边切出去，见 [`TAKEOVER_GUARD`]。
    pub fn note_remote_input(&mut self) {
        self.remote_input_at = Some(Instant::now());
    }

    /// 控制本机的设备离开了：本机的键鼠马上可以照常切出去。
    pub fn remote_input_ended(&mut self) {
        self.remote_input_at = None;
    }

    /// 别的设备的键鼠正在操作本机（刚刚还有输入进来）。
    fn remote_input_active(&self) -> bool {
        self.remote_input_at
            .is_some_and(|t| t.elapsed() < self.takeover_guard)
    }

    /// 让控制器在开始控制远端时查一下本机的大写锁定状态并告诉对方。
    pub fn set_caps_probe(&mut self, probe: CapsProbe) {
        self.caps_probe = Some(probe);
    }

    /// 引擎推断键鼠被别的设备带走了（它刚结束对本机的控制）。之后本机一有键鼠输入，
    /// 就发一次 [`ControlEvent::LocalInput`]。
    pub fn set_away(&mut self, away: bool) {
        self.away = away;
    }

    /// 设置页录制快捷键期间暂停响应快捷键（否则刚按下的组合键会直接触发切换）。
    pub fn set_hotkeys_suspended(&mut self, suspended: bool) {
        self.hotkeys_suspended = suspended;
    }

    /// 正在控制的远端设备；`None` 表示键鼠在本机。
    pub fn active_device(&self) -> Option<&DeviceId> {
        self.active.as_ref().map(|a| &a.device)
    }

    /// 收到 `from` 的心跳回执。
    pub fn on_ack(&mut self, from: &DeviceId) {
        if let Some(a) = self.active.as_mut()
            && &a.device == from
        {
            a.last_ack = Instant::now();
        }
    }

    pub fn configure(&mut self, cfg: ControllerConfig) {
        self.switcher.policy = cfg.policy.clone();
        self.cfg = cfg;
        if !self.cfg.enabled || self.cfg.policy.locked {
            // 暂停或锁定前开始的贴边停留计时不能在之后到期触发切换。
            self.switcher.reset();
        }
        let Some(a) = &self.active else {
            return;
        };
        // 会话被重复连接替换后，发送通道换成了新的；继续用旧通道的话，本机输入全被拦截却到不了对方。
        let reachable = self
            .cfg
            .outputs
            .get(&a.device)
            .is_some_and(|tx| tx.same_channel(&a.tx));
        if !reachable || !self.cfg.enabled || self.cfg.policy.locked {
            let device = a.device.clone();
            self.leave_to_local(None);
            if !reachable {
                let _ = self.events.send(ControlEvent::Lost(device));
            }
        }
    }

    /// 立即回到本机（退出、出错时调用）。
    pub fn force_local(&mut self) {
        self.leave_to_local(None);
    }

    /// 本机被其他设备接管（对方开始控制本机）：结束控制远端、解锁本机光标，但不挪动光标，
    /// 光标由接管方随后注入的位置决定。返回是否确实结束了一次控制。
    ///
    /// 典型场景：Win 控制着 Mac 时，用户直接拿起 Mac 的鼠标推回 Win。Mac 会开始控制 Win，
    /// Win 必须同时退出"控制 Mac"，否则两边都以为自己是控制方：Win 的光标仍被锁住、
    /// 本机键鼠仍被拦截转发，表现为流转异常且切不回来。
    pub fn yield_control(&mut self) -> bool {
        let Some(old) = self.active.take() else {
            return false;
        };
        self.release_remote(&old);
        self.switcher.reset();
        (self.cursor)(false, None);
        info!("本机被其他设备接管，结束控制远端");
        let _ = self.events.send(ControlEvent::Changed);
        true
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// 物理上正按着的修饰键。
    fn held_modifiers(&self) -> Modifiers {
        self.mods_down
            .iter()
            .filter_map(|k| k.modifier())
            .fold(Modifiers::NONE, |acc, m| acc | m)
    }

    fn held(&self) -> HeldInput {
        HeldInput {
            modifiers: self.held_modifiers(),
            buttons: self.router.any_button_held(),
        }
    }

    fn send(&self, msg: Message) {
        if let Some(a) = &self.active {
            let _ = a.tx.send(msg);
        }
    }

    /// 处理一个本机捕获到的事件，返回是否放行给本机系统。
    pub fn handle(&mut self, ev: CapturedEvent) -> Verdict {
        // 能走到这里的都是本机的真实输入（流界自己注入的在捕获层就跳过了）。
        if self.away {
            self.away = false;
            let _ = self.events.send(ControlEvent::LocalInput);
        }
        match ev {
            CapturedEvent::MouseMove { x, y, dx, dy } => {
                let was_remote = self.active.is_some();
                if was_remote {
                    self.remote_move(dx, dy);
                } else {
                    self.local_move(x, y, dx, dy);
                }
                // 控制远端期间的移动一律拦截，包括触发切回本机的这一次：
                // 它的坐标是光标锁定时的位置（Windows 上是离开处往屏幕内收一点）加上位移，
                // 放行会把刚放到边缘的光标又拉回去。
                if was_remote || self.active.is_some() {
                    Verdict::Suppress
                } else {
                    Verdict::Pass
                }
            }
            CapturedEvent::Button { button, down } => {
                match self.router.route_button(button, down) {
                    Route::Local => Verdict::Pass,
                    Route::Remote => {
                        self.send(Message::Button { button, down });
                        Verdict::Suppress
                    }
                    Route::Drop => Verdict::Suppress,
                }
            }
            CapturedEvent::Wheel { dx, dy } => {
                if self.active.is_some() {
                    let percent = self.cfg.wheel_percent;
                    let dx = scale_wheel(dx, percent, &mut self.wheel_rest.0);
                    let dy = scale_wheel(dy, percent, &mut self.wheel_rest.1);
                    if dx != 0 || dy != 0 {
                        self.send(Message::Wheel { dx, dy });
                    }
                    Verdict::Suppress
                } else {
                    Verdict::Pass
                }
            }
            CapturedEvent::Key { code, down, repeat } => self.key(code, down, repeat),
            CapturedEvent::CapsLock(on) => {
                // 本机的大写锁定状态：记下来，正在控制远端的话马上告诉对方。
                self.caps = Some(on);
                if let Some(a) = &self.active {
                    let _ = a.tx.send(caps_message(on));
                }
                Verdict::Pass
            }
        }
    }

    fn key(&mut self, code: KeyCode, down: bool, repeat: bool) -> Verdict {
        if code.modifier().is_some() {
            if down {
                self.mods_down.insert(code);
            } else {
                self.mods_down.remove(&code);
            }
        }
        if self.swallow.contains(&code) {
            if !down {
                self.swallow.remove(&code);
            }
            return Verdict::Suppress;
        }
        // 快捷键（默认 Ctrl + Alt + ←/→ 切到左右相邻的电脑，可以在设置里改）。修饰键要恰好相等。
        // 动作没做成时（没有那台电脑、对方没连着）不算快捷键，按键照常交给原来的去处。
        if down
            && !repeat
            && !self.hotkeys_suspended
            && let Some(action) = self.cfg.hotkeys.action(self.held_modifiers(), code)
            && self.run_hotkey(action)
        {
            self.swallow.insert(code);
            return Verdict::Suppress;
        }
        match self.router.route_key(code, down, repeat) {
            Route::Local => Verdict::Pass,
            Route::Remote => {
                self.send(Message::Key { code, down, repeat });
                if let Some(a) = self.active.as_mut() {
                    a.keys_sent += 1;
                }
                Verdict::Suppress
            }
            Route::Drop => Verdict::Suppress,
        }
    }

    fn local_move(&mut self, x: f64, y: f64, dx: f64, dy: f64) {
        let (ox, oy) = self.cfg.local_offset;
        let p = Point::new(x + ox, y + oy);
        let layout = &self.cfg.layout;
        let Some(d) = layout
            .displays_of(&self.local)
            .find(|d| d.rect.contains(p))
            .or_else(|| {
                // 光标坐标偶尔会落在显示器之间的缝隙里，取最近的一块。
                layout.displays_of(&self.local).min_by(|a, b| {
                    let da = a.rect.clamp(p);
                    let db = b.rect.clamp(p);
                    let dist = |q: Point| (q.x - p.x).abs() + (q.y - p.y).abs();
                    dist(da).total_cmp(&dist(db))
                })
            })
        else {
            return;
        };
        let p = d.rect.clamp(p);
        self.last_local = Some(p);
        if !self.cfg.enabled || self.cfg.outputs.is_empty() {
            return;
        }
        // 别的设备的键鼠正在操作本机：这时本机冒出来的位移不算"推出边缘"，否则两边会互相接管。
        if self.remote_input_active() {
            self.switcher.reset();
            return;
        }
        // 绝大多数移动离屏幕边缘很远，不可能贴边：直接返回，不必为每个鼠标事件克隆显示器 ID。
        let r = d.rect;
        let (tx, ty) = (p.x + dx, p.y + dy);
        if tx > r.left() + 1.0
            && tx < r.right() - 1.0
            && ty > r.top() + 1.0
            && ty < r.bottom() - 1.0
        {
            self.switcher.reset();
            return;
        }
        let (dx, dy) = nudge(d.rect, p, dx, dy);
        let mut vc = VirtualCursor::new(d.id.clone(), p);
        let contact = vc
            .apply_delta(layout, dx, dy, self.cfg.mapping)
            .ok()
            .flatten();
        let held = self.held();
        let now = self.now_ms();
        if let Decision::Switch(c) = self.switcher.update(contact.as_ref(), held, now) {
            // 本机光标停在离开处：移动方向上屏幕边缘的那个点。
            let exit = d.rect.clamp(Point::new(p.x + dx, p.y + dy));
            self.switch_to(c, Some(exit));
        }
    }

    fn remote_move(&mut self, dx: f64, dy: f64) {
        let held = self.held();
        let now = self.now_ms();
        let mapping = self.cfg.mapping;
        let Some(a) = self.active.as_mut() else {
            return;
        };
        a.moves += 1;
        let Ok(contact) = a.cursor.apply_delta(&self.cfg.layout, dx, dy, mapping) else {
            // 远端显示器已不在布局中。
            self.leave_to_local(None);
            return;
        };
        if let Some(d) = self.cfg.layout.display(&a.cursor.display) {
            let lp = d.to_local(a.cursor.pos);
            let _ = a.tx.send(Message::MouseMove {
                session: a.session,
                display: d.id.local.clone(),
                x: lp.x,
                y: lp.y,
            });
        }
        if let Decision::Switch(c) = self.switcher.update(contact.as_ref(), held, now) {
            self.switch_to(c, None);
        }
    }

    /// `exit`：从本机离开时本机光标应停的位置（布局坐标）。
    fn switch_to(&mut self, c: Crossing, exit: Option<Point>) {
        if c.display.device == self.local {
            let was_remote = self.active.is_some();
            self.leave_to_local(Some(c.point));
            if was_remote {
                let _ = self.events.send(ControlEvent::Returned(c.point));
            }
        } else {
            self.enter(c.display, c.point, exit);
        }
    }

    /// 开始控制 `target`。从本机进入时，本机光标锁定在 `exit`（布局坐标），`None` 表示留在原处。
    fn enter(&mut self, target: DisplayId, point: Point, exit: Option<Point>) {
        let Some(tx) = self.cfg.outputs.get(&target.device).cloned() else {
            return;
        };
        let Some(d) = self.cfg.layout.display(&target) else {
            return;
        };
        let lp = d.to_local(point);
        match self.active.take() {
            Some(old) => self.release_remote(&old),
            None => {
                self.return_to = self.last_local;
                let (ox, oy) = self.cfg.local_offset;
                (self.cursor)(true, exit.map(|p| (p.x - ox, p.y - oy)));
            }
        }
        self.next_session = self.next_session.wrapping_add(1);
        let session = self.next_session;
        let _ = tx.send(Message::Enter {
            session,
            display: target.local.clone(),
            x: lp.x,
            y: lp.y,
        });
        // 告诉对方本机大写锁定的状态：能直接查就查（macOS、Windows），
        // 否则用平台层上次报来的（Linux，稍后报来新的状态时还会再发）。
        let caps = self.caps_probe.as_ref().and_then(|probe| probe());
        if let Some(on) = caps.or(self.caps) {
            let _ = tx.send(caps_message(on));
        }
        info!(device = %target.device.short(), "键鼠切到远端");
        self.router.enter_remote();
        self.switcher.reset();
        self.active = Some(Active {
            device: target.device.clone(),
            tx,
            cursor: VirtualCursor::new(target, point),
            session,
            last_beat: Instant::now(),
            last_ack: Instant::now(),
            keys_sent: 0,
            moves: 0,
            started: Instant::now(),
        });
        let _ = self.events.send(ControlEvent::Changed);
    }

    /// 释放远端所有按键并通知对方结束控制。
    fn release_remote(&mut self, old: &Active) {
        let secs = old.started.elapsed().as_secs_f64();
        info!(
            keys = old.keys_sent,
            moves = old.moves,
            secs = format!("{secs:.1}"),
            "结束控制远端"
        );
        for ev in self.router.leave_remote() {
            let _ = old.tx.send(input_message(ev));
        }
        let _ = old.tx.send(Message::Leave {
            session: old.session,
        });
    }

    /// 回到本机。`at` 为布局坐标，`None` 时回到离开本机时的位置。
    fn leave_to_local(&mut self, at: Option<Point>) {
        let Some(old) = self.active.take() else {
            return;
        };
        self.release_remote(&old);
        self.switcher.reset();
        let (ox, oy) = self.cfg.local_offset;
        let restore = at.or(self.return_to).map(|p| (p.x - ox, p.y - oy));
        (self.cursor)(false, restore);
        info!("键鼠回到本机");
        let _ = self.events.send(ControlEvent::Changed);
    }

    fn run_hotkey(&mut self, action: HotkeyAction) -> bool {
        match action {
            HotkeyAction::SwitchLeft => self.hotkey_switch(false),
            HotkeyAction::SwitchRight => self.hotkey_switch(true),
            HotkeyAction::SwitchTo(device) => self.switch_to_device(&device),
            HotkeyAction::Send { device, keys } => self.send_keys(&device, keys),
        }
    }

    /// 桌面直接交给流界的快捷键（Hyprland 的全局快捷键，见 `flow_platform::LocalShortcut`）：
    /// `id` 是 `mapping:<序号>`，执行设置里对应的快捷键映射。和按下触发键走同一条路。
    pub fn run_shortcut(&mut self, id: &str) -> bool {
        let index = id
            .strip_prefix("mapping:")
            .and_then(|n| n.parse::<usize>().ok());
        let Some(mapping) = index
            .and_then(|i| self.cfg.hotkeys.mappings.get(i))
            .cloned()
        else {
            return false;
        };
        self.send_keys(&mapping.device, mapping.send)
    }

    /// 快捷键映射：让 `device` 按一下 `keys`，键鼠不用切过去。对方没连着、被暂停、
    /// 或版本太旧不认得这条消息时返回 `false`。锁定在本机不影响（它只限制键鼠切出去）。
    fn send_keys(&mut self, device: &DeviceId, keys: KeyCombo) -> bool {
        if !self.cfg.enabled || !self.cfg.remote_keys.contains(device) {
            return false;
        }
        let Some(tx) = self.cfg.outputs.get(device) else {
            return false;
        };
        info!(device = %device.short(), "快捷键映射：发送按键");
        tx.send(Message::KeyStroke { keys: keys.keys() }).is_ok()
    }

    /// 把键鼠切到指定的设备（快捷键、托盘菜单、界面按钮、对方的请求都走这里）。`target` 是本机时
    /// 回到本机。已经在那台设备上时算成功、什么都不做；切不过去（没连着、被暂停、锁定在本机）
    /// 时返回 `false`。
    pub fn switch_to_device(&mut self, target: &DeviceId) -> bool {
        if !self.cfg.enabled {
            return false;
        }
        if self.active_device().unwrap_or(&self.local) == target {
            return true;
        }
        if target == &self.local {
            self.leave_to_local(None);
            return true;
        }
        if self.cfg.policy.locked || !self.cfg.outputs.contains_key(target) {
            return false;
        }
        // 进入目标设备面积最大的屏幕的中央。
        let Some(d) = self.cfg.layout.displays_of(target).max_by(|a, b| {
            (a.rect.width * a.rect.height).total_cmp(&(b.rect.width * b.rect.height))
        }) else {
            return false;
        };
        let center = Point::new(
            d.rect.x + d.rect.width / 2.0,
            d.rect.y + d.rect.height / 2.0,
        );
        let id = d.id.clone();
        self.enter(id, center, None);
        true
    }

    /// 快捷键切换：按设备在布局中的水平位置，切到左边或右边最近的一台。
    fn hotkey_switch(&mut self, right: bool) -> bool {
        if !self.cfg.enabled || self.cfg.policy.locked || self.cfg.outputs.is_empty() {
            return false;
        }
        match self.neighbor(right) {
            Some(target) => self.switch_to_device(&target),
            None => false,
        }
    }

    /// 键鼠现在所在的设备左边或右边最近的一台（按布局里的水平位置；只在本机和可控制的设备里找）。
    pub fn neighbor(&self, right: bool) -> Option<DeviceId> {
        let layout = &self.cfg.layout;
        let center_x = |dev: &DeviceId| {
            let (min, max) = layout
                .displays_of(dev)
                .fold((f64::MAX, f64::MIN), |(lo, hi), d| {
                    (lo.min(d.rect.left()), hi.max(d.rect.right()))
                });
            (min <= max).then_some((min + max) / 2.0)
        };
        let current = self.active_device().unwrap_or(&self.local).clone();
        let cx = center_x(&current)?;
        layout
            .devices()
            .into_iter()
            .filter(|d| d != &current && (d == &self.local || self.cfg.outputs.contains_key(d)))
            .filter_map(|d| center_x(&d).map(|x| (d, x)))
            .filter(|(_, x)| if right { *x > cx } else { *x < cx })
            .min_by(|a, b| (a.1 - cx).abs().total_cmp(&(b.1 - cx).abs()))
            .map(|(d, _)| d)
    }

    /// 定时调用（约每 40 毫秒）：发送心跳、检查心跳回执、处理贴边停留计时。
    pub fn tick(&mut self) {
        // 对方不再回执：网络断了。先把键鼠还给本机（释放远端按键、解锁光标），再通知界面。
        // 不等 QUIC 的 8 秒空闲超时，否则这段时间本机键鼠全被拦截、光标锁死。
        if let Some(a) = &self.active
            && self.cfg.acks.contains(&a.device)
            && a.last_ack.elapsed() > self.ack_timeout
        {
            let device = a.device.clone();
            info!(device = %device.short(), "对方没有回应心跳，键鼠回到本机");
            self.leave_to_local(None);
            let _ = self.events.send(ControlEvent::Lost(device));
        }
        if let Some(a) = self.active.as_mut()
            && a.last_beat.elapsed() >= HEARTBEAT
        {
            self.heartbeat_seq = self.heartbeat_seq.wrapping_add(1);
            let _ = a.tx.send(Message::Heartbeat {
                seq: self.heartbeat_seq,
            });
            a.last_beat = Instant::now();
        }
        // 贴边停留的计时开始之后，别的设备的键鼠进来了：这次停留作废。
        if self.active.is_none() && self.remote_input_active() {
            self.switcher.reset();
            return;
        }
        let held = self.held();
        let now = self.now_ms();
        if let Decision::Switch(c) = self.switcher.poll(held, now) {
            // 贴边停留触发：光标本来就停在边上，留在原处即可。
            self.switch_to(c, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flow_core::Rect;
    use flow_core::layout::Display;
    use flow_core::protocol::Message;
    use std::sync::{Arc, Mutex};

    fn dev(s: &str) -> DeviceId {
        DeviceId(s.repeat(8))
    }

    fn display(device: &str, rect: Rect) -> Display {
        Display {
            id: DisplayId {
                device: dev(device),
                local: "1".into(),
            },
            name: device.into(),
            rect,
            scale: 1.0,
            native: None,
        }
    }

    type Calls = Arc<Mutex<Vec<(bool, Option<(f64, f64)>)>>>;

    struct Rig {
        ctl: Controller,
        pc_rx: mpsc::UnboundedReceiver<Message>,
        events: mpsc::UnboundedReceiver<ControlEvent>,
        cursor: Calls,
        pc_tx: Output,
    }

    /// 本机 Mac 1440×900 在左，PC 1920×1080 在右（布局中本机偏移 100,0）。
    fn rig(policy: SwitchPolicy) -> Rig {
        let (ev_tx, events) = mpsc::unbounded_channel();
        let cursor: Calls = Arc::default();
        let c = cursor.clone();
        let mut ctl = Controller::new(
            dev("m"),
            ev_tx,
            Box::new(move |cap, at| c.lock().unwrap().push((cap, at))),
        );
        let (pc_tx, pc_rx) = mpsc::unbounded_channel();
        ctl.configure(config(policy, pc_tx.clone(), true));
        Rig {
            ctl,
            pc_rx,
            events,
            cursor,
            pc_tx,
        }
    }

    fn config(policy: SwitchPolicy, pc: Output, with_pc: bool) -> ControllerConfig {
        let mut outputs = HashMap::new();
        if with_pc {
            outputs.insert(dev("p"), pc);
        }
        ControllerConfig {
            layout: Layout::new(vec![
                display("m", Rect::new(100.0, 0.0, 1440.0, 900.0)),
                display("p", Rect::new(1540.0, 0.0, 1920.0, 1080.0)),
            ]),
            local_offset: (100.0, 0.0),
            outputs,
            policy,
            mapping: EdgeMapping::Continuous,
            wheel_percent: 100,
            enabled: true,
            acks: HashSet::new(),
            hotkeys: Hotkeys::default(),
            remote_keys: HashSet::from([dev("p")]),
        }
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<Message>) -> Vec<Message> {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            out.push(m);
        }
        out
    }

    fn mv(x: f64, y: f64, dx: f64, dy: f64) -> CapturedEvent {
        CapturedEvent::MouseMove { x, y, dx, dy }
    }

    fn key(code: KeyCode, down: bool) -> CapturedEvent {
        CapturedEvent::Key {
            code,
            down,
            repeat: false,
        }
    }

    #[test]
    fn crosses_right_edge_and_back() {
        let mut r = rig(SwitchPolicy::default());
        // 本机系统坐标 (1439, 400) 是 Mac 屏幕最右一列，继续往右推。
        assert_eq!(r.ctl.handle(mv(1439.0, 400.0, 5.0, 0.0)), Verdict::Suppress);
        assert_eq!(r.ctl.active_device(), Some(&dev("p")));
        let msgs = drain(&mut r.pc_rx);
        match &msgs[..] {
            [Message::Enter { x, y, .. }] => {
                assert!(*x < 1.0);
                assert_eq!(*y, 400.0);
            }
            other => panic!("期望 Enter，实际 {other:?}"),
        }
        // 本机光标锁定在离开处：右边缘、同一高度（本机系统坐标）。
        let calls = r.cursor.lock().unwrap().clone();
        let [(true, Some((ex, ey)))] = calls.as_slice() else {
            panic!("期望锁定光标并给出离开处，实际 {calls:?}");
        };
        assert!(*ex >= 1439.0 && *ex <= 1440.0 && *ey == 400.0);
        assert_eq!(r.events.try_recv(), Ok(ControlEvent::Changed));

        // 在 PC 上移动：发绝对坐标
        r.ctl.handle(mv(1439.0, 400.0, 100.0, 20.0));
        match &drain(&mut r.pc_rx)[..] {
            [Message::MouseMove { x, y, .. }] => {
                assert!((*x - 100.0).abs() < 1.0);
                assert_eq!(*y, 420.0);
            }
            other => panic!("期望 MouseMove，实际 {other:?}"),
        }

        // 往左推回 Mac：结束控制，光标放回 Mac 右边缘。触发切回的这次移动也要拦截，
        // 否则系统会按事件里的坐标把光标挪走。
        assert_eq!(
            r.ctl.handle(mv(1439.0, 400.0, -150.0, 0.0)),
            Verdict::Suppress
        );
        assert_eq!(r.ctl.active_device(), None);
        let msgs = drain(&mut r.pc_rx);
        assert!(matches!(msgs.last(), Some(Message::Leave { .. })));
        let (cap, at) = r.cursor.lock().unwrap()[1];
        assert!(!cap);
        let (x, y) = at.unwrap();
        assert!(x > 1438.0 && x < 1440.0 && y == 420.0);
        // 先通知键鼠换了地方，再给出回到本机的位置（用来在这条边上闪浮光）。
        assert_eq!(r.events.try_recv(), Ok(ControlEvent::Changed));
        match r.events.try_recv() {
            // 布局坐标 = 本机系统坐标 + 偏移（测试里是 100, 0）。
            Ok(ControlEvent::Returned(p)) => assert_eq!((p.x - 100.0, p.y), (x, y)),
            other => panic!("期望 Returned，实际 {other:?}"),
        }
    }

    #[test]
    fn keys_go_to_the_side_that_saw_the_press() {
        let mut r = rig(SwitchPolicy::default());
        // 在本机按住 Shift，然后跨屏
        assert_eq!(r.ctl.handle(key(KeyCode::LEFT_SHIFT, true)), Verdict::Pass);
        r.ctl.handle(mv(1439.0, 400.0, 5.0, 0.0));
        drain(&mut r.pc_rx);
        // 远端打字
        assert_eq!(r.ctl.handle(key(KeyCode::A, true)), Verdict::Suppress);
        // 松开 Shift 交给本机
        assert_eq!(r.ctl.handle(key(KeyCode::LEFT_SHIFT, false)), Verdict::Pass);
        assert_eq!(
            drain(&mut r.pc_rx),
            vec![Message::Key {
                code: KeyCode::A,
                down: true,
                repeat: false
            }]
        );
        // A 还按着就回到本机：远端收到 A 的抬起
        r.ctl.handle(mv(0.0, 0.0, -2000.0, 0.0));
        let msgs = drain(&mut r.pc_rx);
        assert!(msgs.contains(&Message::Key {
            code: KeyCode::A,
            down: false,
            repeat: false
        }));
    }

    #[test]
    fn hold_key_policy_blocks_until_pressed() {
        let mut r = rig(SwitchPolicy {
            require_modifiers: Modifiers::CTRL,
            ..Default::default()
        });
        assert_eq!(r.ctl.handle(mv(1439.0, 400.0, 5.0, 0.0)), Verdict::Pass);
        assert_eq!(r.ctl.active_device(), None);
        r.ctl.handle(key(KeyCode::LEFT_CTRL, true));
        assert_eq!(r.ctl.handle(mv(1439.0, 400.0, 5.0, 0.0)), Verdict::Suppress);
        assert_eq!(r.ctl.active_device(), Some(&dev("p")));
    }

    #[test]
    fn caps_lock_state_is_forwarded_while_controlling() {
        let mut r = rig(SwitchPolicy::default());
        let caps = |on| Message::Key {
            code: KeyCode::caps_state(on),
            down: false,
            repeat: false,
        };
        // 在本机时只记下，不发给任何人。
        assert_eq!(r.ctl.handle(CapturedEvent::CapsLock(true)), Verdict::Pass);
        assert!(drain(&mut r.pc_rx).is_empty());
        // 切过去：紧跟着进入消息把已知的状态告诉对方。
        r.ctl.handle(mv(1439.0, 400.0, 5.0, 0.0));
        let sent = drain(&mut r.pc_rx);
        assert!(matches!(sent[0], Message::Enter { .. }));
        assert_eq!(sent[1], caps(true));
        // 控制期间状态变了：马上转告。
        r.ctl.handle(CapturedEvent::CapsLock(false));
        assert_eq!(drain(&mut r.pc_rx), vec![caps(false)]);
    }

    #[test]
    fn wheel_speed_scales_what_is_sent_and_keeps_fractions() {
        let mut rest = 0;
        // 100% 和没设（0）都是原样。
        assert_eq!(scale_wheel(-120, 100, &mut rest), -120);
        assert_eq!(scale_wheel(120, 0, &mut rest), 120);
        assert_eq!(scale_wheel(120, 200, &mut rest), 240);
        assert_eq!(scale_wheel(-120, 150, &mut rest), -180);
        assert_eq!(rest, 0);
        // 一半速度下每次 3 个单位：1.5 攒起来，不丢。
        let sent: i32 = (0..4).map(|_| scale_wheel(3, 50, &mut rest)).sum();
        assert_eq!(sent, 6);
        assert_eq!(rest, 0);
        // 反方向同样。
        let sent: i32 = (0..4).map(|_| scale_wheel(-3, 50, &mut rest)).sum();
        assert_eq!(sent, -6);
    }

    #[test]
    fn wheel_speed_setting_applies_only_while_controlling() {
        let mut r = rig(SwitchPolicy::default());
        let mut cfg = config(SwitchPolicy::default(), r.pc_tx.clone(), true);
        cfg.wheel_percent = 200;
        r.ctl.configure(cfg);
        // 在本机：放行，不发给任何人。
        let wheel = CapturedEvent::Wheel { dx: 0, dy: -120 };
        assert_eq!(r.ctl.handle(wheel), Verdict::Pass);
        assert!(drain(&mut r.pc_rx).is_empty());
        r.ctl.handle(mv(1439.0, 400.0, 5.0, 0.0));
        drain(&mut r.pc_rx);
        assert_eq!(r.ctl.handle(wheel), Verdict::Suppress);
        assert_eq!(
            drain(&mut r.pc_rx),
            vec![Message::Wheel { dx: 0, dy: -240 }]
        );
    }

    #[test]
    fn caps_lock_is_probed_when_entering_a_remote() {
        let mut r = rig(SwitchPolicy::default());
        // 每次查到的状态不一样：第一次开着，第二次关着。
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();
        r.ctl.set_caps_probe(Box::new(move || {
            Some(seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0)
        }));
        let caps = |on| Message::Key {
            code: KeyCode::caps_state(on),
            down: false,
            repeat: false,
        };
        r.ctl.handle(mv(1439.0, 400.0, 5.0, 0.0));
        let sent = drain(&mut r.pc_rx);
        assert!(matches!(sent[0], Message::Enter { .. }));
        assert_eq!(sent[1], caps(true));
        // 回到本机再切过去：重新查，不沿用上一次的。
        r.ctl.force_local();
        drain(&mut r.pc_rx);
        r.ctl.handle(mv(1439.0, 400.0, 5.0, 0.0));
        let sent = drain(&mut r.pc_rx);
        assert!(matches!(sent[0], Message::Enter { .. }));
        assert_eq!(sent[1], caps(false));
    }

    #[test]
    fn hotkey_switches_and_swallows_arrow() {
        let mut r = rig(SwitchPolicy::default());
        r.ctl.handle(mv(500.0, 500.0, 0.0, 0.0));
        r.ctl.handle(key(KeyCode::LEFT_CTRL, true));
        r.ctl.handle(key(KeyCode::LEFT_ALT, true));
        assert_eq!(r.ctl.handle(key(KeyCode::RIGHT, true)), Verdict::Suppress);
        assert_eq!(r.ctl.active_device(), Some(&dev("p")));
        assert_eq!(r.ctl.handle(key(KeyCode::RIGHT, false)), Verdict::Suppress);
        // PC 没有收到方向键
        assert!(
            drain(&mut r.pc_rx)
                .iter()
                .all(|m| !matches!(m, Message::Key { .. }))
        );
        // 反方向回到本机，光标回到原来的位置
        r.ctl.handle(key(KeyCode::LEFT, true));
        assert_eq!(r.ctl.active_device(), None);
        let (cap, at) = *r.cursor.lock().unwrap().last().unwrap();
        assert!(!cap);
        assert_eq!(at, Some((500.0, 500.0)));
    }

    const CTRL_ALT: Modifiers = Modifiers(Modifiers::CTRL.0 | Modifiers::ALT.0);
    const KEY_S: KeyCode = KeyCode(0x16);
    const KEY_1: KeyCode = KeyCode(0x1E);

    fn hold_ctrl_alt(r: &mut Rig) {
        r.ctl.handle(key(KeyCode::LEFT_CTRL, true));
        r.ctl.handle(key(KeyCode::LEFT_ALT, true));
    }

    #[test]
    fn switch_hotkeys_can_be_rebound_or_turned_off() {
        let mut r = rig(SwitchPolicy::default());
        let mut cfg = config(SwitchPolicy::default(), r.pc_tx.clone(), true);
        // 「切到右边」改成 Ctrl+Alt+S，「切到左边」关掉。
        cfg.hotkeys.switch_right = Some(KeyCombo::new(CTRL_ALT, KEY_S));
        cfg.hotkeys.switch_left = None;
        r.ctl.configure(cfg);
        hold_ctrl_alt(&mut r);
        // 原来的方向键不再是快捷键。
        assert_eq!(r.ctl.handle(key(KeyCode::RIGHT, true)), Verdict::Pass);
        assert_eq!(r.ctl.handle(key(KeyCode::RIGHT, false)), Verdict::Pass);
        assert_eq!(r.ctl.active_device(), None);
        assert_eq!(r.ctl.handle(key(KEY_S, true)), Verdict::Suppress);
        assert_eq!(r.ctl.active_device(), Some(&dev("p")));
        assert_eq!(r.ctl.handle(key(KEY_S, false)), Verdict::Suppress);
        drain(&mut r.pc_rx);
        // 「切到左边」关掉了：在 PC 上按 Ctrl+Alt+← 就是普通按键，发给 PC。
        assert_eq!(r.ctl.handle(key(KeyCode::LEFT, true)), Verdict::Suppress);
        assert_eq!(r.ctl.active_device(), Some(&dev("p")));
        assert!(drain(&mut r.pc_rx).contains(&Message::Key {
            code: KeyCode::LEFT,
            down: true,
            repeat: false
        }));
    }

    #[test]
    fn extra_modifiers_do_not_trigger_a_hotkey() {
        let mut r = rig(SwitchPolicy::default());
        hold_ctrl_alt(&mut r);
        r.ctl.handle(key(KeyCode::LEFT_SHIFT, true));
        assert_eq!(r.ctl.handle(key(KeyCode::RIGHT, true)), Verdict::Pass);
        assert_eq!(r.ctl.active_device(), None);
    }

    #[test]
    fn device_hotkey_switches_there_and_back() {
        use flow_core::hotkeys::DeviceHotkey;
        let mut r = rig(SwitchPolicy::default());
        let mut cfg = config(SwitchPolicy::default(), r.pc_tx.clone(), true);
        cfg.hotkeys.devices = vec![
            DeviceHotkey {
                device: dev("p"),
                combo: KeyCombo::new(CTRL_ALT, KEY_1),
            },
            DeviceHotkey {
                device: dev("m"),
                combo: KeyCombo::new(CTRL_ALT, KeyCode(0x27)),
            },
        ];
        r.ctl.configure(cfg);
        r.ctl.handle(mv(500.0, 500.0, 0.0, 0.0));
        hold_ctrl_alt(&mut r);
        assert_eq!(r.ctl.handle(key(KEY_1, true)), Verdict::Suppress);
        assert_eq!(r.ctl.active_device(), Some(&dev("p")));
        assert_eq!(r.ctl.handle(key(KEY_1, false)), Verdict::Suppress);
        // 已经在那台电脑上：吞掉按键，什么都不做，也不会把 Ctrl+Alt+1 发过去。
        drain(&mut r.pc_rx);
        assert_eq!(r.ctl.handle(key(KEY_1, true)), Verdict::Suppress);
        assert_eq!(r.ctl.handle(key(KEY_1, false)), Verdict::Suppress);
        assert!(drain(&mut r.pc_rx).is_empty());
        // 回到本机的快捷键。
        assert_eq!(r.ctl.handle(key(KeyCode(0x27), true)), Verdict::Suppress);
        assert_eq!(r.ctl.active_device(), None);
        assert_eq!(
            *r.cursor.lock().unwrap().last().unwrap(),
            (false, Some((500.0, 500.0)))
        );
    }

    /// 在远端按下的修饰键，切走后手还按着：接着按第二个快捷键照样要认。
    #[test]
    fn modifiers_held_across_a_switch_still_count_for_the_next_hotkey() {
        let mut r = rig(SwitchPolicy::default());
        r.ctl.handle(mv(500.0, 500.0, 0.0, 0.0));
        // 先用鼠标切到 PC，在 PC 上按下 Ctrl+Alt（这两个键是发给 PC 的）。
        r.ctl.handle(mv(1439.0, 400.0, 5.0, 0.0));
        assert_eq!(r.ctl.active_device(), Some(&dev("p")));
        hold_ctrl_alt(&mut r);
        // ← 切回本机：PC 那边的 Ctrl+Alt 被释放，但手没松。
        assert_eq!(r.ctl.handle(key(KeyCode::LEFT, true)), Verdict::Suppress);
        assert_eq!(r.ctl.handle(key(KeyCode::LEFT, false)), Verdict::Suppress);
        assert_eq!(r.ctl.active_device(), None);
        drain(&mut r.pc_rx);
        // 不松手接着按 →：仍然是快捷键，切到 PC，本机收不到裸的方向键。
        assert_eq!(r.ctl.handle(key(KeyCode::RIGHT, true)), Verdict::Suppress);
        assert_eq!(r.ctl.active_device(), Some(&dev("p")));
        assert_eq!(r.ctl.handle(key(KeyCode::RIGHT, false)), Verdict::Suppress);
        // 松开修饰键后就不是快捷键了。
        r.ctl.handle(key(KeyCode::LEFT_CTRL, false));
        r.ctl.handle(key(KeyCode::LEFT_ALT, false));
        drain(&mut r.pc_rx);
        assert_eq!(r.ctl.handle(key(KeyCode::LEFT, true)), Verdict::Suppress);
        assert_eq!(r.ctl.active_device(), Some(&dev("p")));
        assert!(drain(&mut r.pc_rx).contains(&Message::Key {
            code: KeyCode::LEFT,
            down: true,
            repeat: false
        }));
    }

    #[test]
    fn mapped_hotkey_sends_keys_without_switching() {
        use flow_core::hotkeys::KeyMapping;
        let mut r = rig(SwitchPolicy::default());
        let mapping = |keys: KeyCombo| {
            let mut cfg = config(SwitchPolicy::default(), mpsc::unbounded_channel().0, true);
            cfg.hotkeys.mappings = vec![KeyMapping {
                trigger: KeyCombo::new(CTRL_ALT, KEY_S),
                send: keys,
                device: dev("p"),
            }];
            cfg
        };
        let mut cfg = mapping(KeyCombo::new(Modifiers::SHIFT, KEY_S));
        cfg.outputs.insert(dev("p"), r.pc_tx.clone());
        r.ctl.configure(cfg);
        hold_ctrl_alt(&mut r);
        // 在本机按 Ctrl+Alt+S：PC 收到 Shift+S，键鼠留在本机，本机收不到这个 S。
        assert_eq!(r.ctl.handle(key(KEY_S, true)), Verdict::Suppress);
        assert_eq!(r.ctl.active_device(), None);
        assert_eq!(
            drain(&mut r.pc_rx),
            vec![Message::KeyStroke {
                keys: vec![KeyCode::LEFT_SHIFT, KEY_S]
            }]
        );
        // 长按的连发和抬起都吞掉，不重复发。
        assert_eq!(
            r.ctl.handle(CapturedEvent::Key {
                code: KEY_S,
                down: true,
                repeat: true
            }),
            Verdict::Suppress
        );
        assert_eq!(r.ctl.handle(key(KEY_S, false)), Verdict::Suppress);
        assert!(drain(&mut r.pc_rx).is_empty());

        // 锁定在本机时映射照常能用（没有把键鼠切出去）。
        let mut cfg = mapping(KeyCombo::new(Modifiers::NONE, KEY_S));
        cfg.outputs.insert(dev("p"), r.pc_tx.clone());
        cfg.policy.locked = true;
        r.ctl.configure(cfg);
        assert_eq!(r.ctl.handle(key(KEY_S, true)), Verdict::Suppress);
        r.ctl.handle(key(KEY_S, false));
        assert_eq!(
            drain(&mut r.pc_rx),
            vec![Message::KeyStroke { keys: vec![KEY_S] }]
        );

        // 对方的版本太旧、不认得这条消息：不算快捷键，按键照常留在本机。
        let mut cfg = mapping(KeyCombo::new(Modifiers::NONE, KEY_S));
        cfg.outputs.insert(dev("p"), r.pc_tx.clone());
        cfg.remote_keys.clear();
        r.ctl.configure(cfg);
        assert_eq!(r.ctl.handle(key(KEY_S, true)), Verdict::Pass);
        assert_eq!(r.ctl.handle(key(KEY_S, false)), Verdict::Pass);
        assert!(drain(&mut r.pc_rx).is_empty());
    }

    #[test]
    fn recording_a_hotkey_suspends_all_hotkeys() {
        let mut r = rig(SwitchPolicy::default());
        r.ctl.set_hotkeys_suspended(true);
        hold_ctrl_alt(&mut r);
        assert_eq!(r.ctl.handle(key(KeyCode::RIGHT, true)), Verdict::Pass);
        assert_eq!(r.ctl.handle(key(KeyCode::RIGHT, false)), Verdict::Pass);
        assert_eq!(r.ctl.active_device(), None);
        r.ctl.set_hotkeys_suspended(false);
        assert_eq!(r.ctl.handle(key(KeyCode::RIGHT, true)), Verdict::Suppress);
        assert_eq!(r.ctl.active_device(), Some(&dev("p")));
    }

    #[test]
    fn switching_to_a_device_on_request() {
        let mut r = rig(SwitchPolicy::default());
        // 不认识的、没连着的设备切不过去。
        assert!(!r.ctl.switch_to_device(&dev("x")));
        assert!(r.ctl.switch_to_device(&dev("p")));
        assert_eq!(r.ctl.active_device(), Some(&dev("p")));
        assert!(matches!(
            drain(&mut r.pc_rx).first(),
            Some(Message::Enter { .. })
        ));
        // 再回到本机。
        assert!(r.ctl.switch_to_device(&dev("m")));
        assert_eq!(r.ctl.active_device(), None);
        assert!(matches!(
            drain(&mut r.pc_rx).last(),
            Some(Message::Leave { .. })
        ));
        // 锁定在本机时切不出去。
        r.ctl.configure(config(
            SwitchPolicy {
                locked: true,
                ..Default::default()
            },
            r.pc_tx.clone(),
            true,
        ));
        assert!(!r.ctl.switch_to_device(&dev("p")));
    }

    #[test]
    fn locking_blocks_edges_and_hotkeys_and_releases_remote_keys() {
        let locked = SwitchPolicy {
            locked: true,
            ..Default::default()
        };
        let mut r = rig(locked.clone());
        assert_eq!(r.ctl.handle(mv(1439.0, 400.0, 5.0, 0.0)), Verdict::Pass);
        r.ctl.handle(key(KeyCode::LEFT_CTRL, true));
        r.ctl.handle(key(KeyCode::LEFT_ALT, true));
        assert_eq!(r.ctl.handle(key(KeyCode::RIGHT, true)), Verdict::Pass);
        assert_eq!(r.ctl.active_device(), None);
        assert!(drain(&mut r.pc_rx).is_empty());

        let mut r = rig(SwitchPolicy::default());
        r.ctl.handle(mv(1439.0, 400.0, 5.0, 0.0));
        r.ctl.handle(key(KeyCode::A, true));
        r.ctl.handle(CapturedEvent::Button {
            button: flow_core::keys::MouseButton::Left,
            down: true,
        });
        drain(&mut r.pc_rx);
        while r.events.try_recv().is_ok() {}
        r.ctl.configure(config(locked, r.pc_tx.clone(), true));
        assert_eq!(r.ctl.active_device(), None);
        let msgs = drain(&mut r.pc_rx);
        assert!(msgs.contains(&Message::Key {
            code: KeyCode::A,
            down: false,
            repeat: false
        }));
        assert!(msgs.contains(&Message::Button {
            button: flow_core::keys::MouseButton::Left,
            down: false
        }));
        assert!(matches!(msgs.last(), Some(Message::Leave { .. })));
        assert!(!r.cursor.lock().unwrap().last().unwrap().0);
        assert_eq!(r.events.try_recv(), Ok(ControlEvent::Changed));
        assert!(r.events.try_recv().is_err(), "主动锁定不会报告连接中断");
        assert_eq!(r.ctl.handle(key(KeyCode::A, false)), Verdict::Pass);
    }

    #[test]
    fn locking_cancels_pending_dwell() {
        let mut r = rig(SwitchPolicy {
            dwell_ms: 30,
            ..Default::default()
        });
        r.ctl.handle(mv(1439.0, 400.0, 5.0, 0.0));
        r.ctl.configure(config(
            SwitchPolicy {
                locked: true,
                dwell_ms: 30,
                ..Default::default()
            },
            r.pc_tx.clone(),
            true,
        ));
        std::thread::sleep(Duration::from_millis(40));
        r.ctl.tick();
        assert_eq!(r.ctl.active_device(), None);
        assert!(drain(&mut r.pc_rx).is_empty());
    }

    #[test]
    fn losing_the_peer_returns_control() {
        let mut r = rig(SwitchPolicy::default());
        r.ctl.handle(mv(1439.0, 400.0, 5.0, 0.0));
        while r.events.try_recv().is_ok() {}
        r.ctl
            .configure(config(SwitchPolicy::default(), r.pc_tx.clone(), false));
        assert_eq!(r.ctl.active_device(), None);
        assert_eq!(r.events.try_recv(), Ok(ControlEvent::Changed));
        assert_eq!(r.events.try_recv(), Ok(ControlEvent::Lost(dev("p"))));
        assert!(!r.cursor.lock().unwrap().last().unwrap().0);
    }

    #[test]
    fn yielding_to_a_takeover_releases_remote_without_moving_cursor() {
        let mut r = rig(SwitchPolicy::default());
        // 本机没有在控制远端时，被接管什么也不用做。
        assert!(!r.ctl.yield_control());
        assert!(r.cursor.lock().unwrap().is_empty());

        // 本机控制 PC，按住 A 时 PC 那边的人接管了本机。
        r.ctl.handle(mv(1439.0, 400.0, 5.0, 0.0));
        r.ctl.handle(key(KeyCode::A, true));
        drain(&mut r.pc_rx);
        while r.events.try_recv().is_ok() {}

        assert!(r.ctl.yield_control());
        assert_eq!(r.ctl.active_device(), None);
        // PC 收到 A 的抬起和 Leave。
        let msgs = drain(&mut r.pc_rx);
        assert!(msgs.contains(&Message::Key {
            code: KeyCode::A,
            down: false,
            repeat: false
        }));
        assert!(matches!(msgs.last(), Some(Message::Leave { .. })));
        // 光标解锁但不挪动：位置由接管方注入。
        assert_eq!(*r.cursor.lock().unwrap().last().unwrap(), (false, None));
        assert_eq!(r.events.try_recv(), Ok(ControlEvent::Changed));

        // 之后本机键鼠照常留在本机，A 的抬起不再发给 PC。
        assert_eq!(r.ctl.handle(key(KeyCode::A, false)), Verdict::Pass);
        assert_eq!(r.ctl.handle(mv(500.0, 400.0, 5.0, 0.0)), Verdict::Pass);
        assert!(drain(&mut r.pc_rx).is_empty());
        // 再推过边缘可以重新控制 PC。
        assert_eq!(r.ctl.handle(mv(1439.0, 400.0, 5.0, 0.0)), Verdict::Suppress);
        assert_eq!(r.ctl.active_device(), Some(&dev("p")));
    }

    #[test]
    fn local_edge_pushes_do_not_take_over_while_a_remote_is_driving() {
        let mut r = rig(SwitchPolicy::default());
        // PC 的键鼠正在操作本机（光标刚从右边进来，正贴着右边缘）：本机冒出一点朝右的位移，
        // 不能反过来接管 PC。
        r.ctl.note_remote_input();
        assert_eq!(r.ctl.handle(mv(1439.0, 400.0, 1.0, 0.0)), Verdict::Pass);
        assert_eq!(r.ctl.active_device(), None);
        assert!(drain(&mut r.pc_rx).is_empty());
        assert!(r.cursor.lock().unwrap().is_empty());
        // 快捷键是明确的操作，不受影响；这里只验证屏幕边。PC 离开后马上可以推过去。
        r.ctl.remote_input_ended();
        assert_eq!(r.ctl.handle(mv(1439.0, 400.0, 1.0, 0.0)), Verdict::Suppress);
        assert_eq!(r.ctl.active_device(), Some(&dev("p")));
        r.ctl.force_local();
        drain(&mut r.pc_rx);

        // PC 没离开但停手了（过了保护时间）：本机的人拿起鼠标推过去，照常接管。
        r.ctl.takeover_guard = Duration::from_millis(20);
        r.ctl.note_remote_input();
        assert_eq!(r.ctl.handle(mv(1439.0, 400.0, 1.0, 0.0)), Verdict::Pass);
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(r.ctl.handle(mv(1439.0, 400.0, 1.0, 0.0)), Verdict::Suppress);
        assert_eq!(r.ctl.active_device(), Some(&dev("p")));
    }

    #[test]
    fn remote_input_cancels_a_pending_dwell() {
        let mut r = rig(SwitchPolicy {
            dwell_ms: 30,
            ..Default::default()
        });
        // 贴着右边往外推，开始计时；这时 PC 的键鼠进来了，到时间也不切。
        assert_eq!(r.ctl.handle(mv(1439.0, 400.0, 1.0, 0.0)), Verdict::Pass);
        r.ctl.note_remote_input();
        std::thread::sleep(Duration::from_millis(50));
        r.ctl.tick();
        assert_eq!(r.ctl.active_device(), None);
        assert!(drain(&mut r.pc_rx).is_empty());
    }

    #[test]
    fn missing_heartbeat_acks_return_control_quickly() {
        let mut r = rig(SwitchPolicy::default());
        r.ctl.ack_timeout = Duration::from_millis(150);
        // 对方不支持回执（旧版本）：不做快速检测。
        r.ctl.handle(mv(1439.0, 400.0, 5.0, 0.0));
        std::thread::sleep(Duration::from_millis(200));
        r.ctl.tick();
        assert_eq!(r.ctl.active_device(), Some(&dev("p")));
        r.ctl.force_local();

        let mut cfg = config(SwitchPolicy::default(), r.pc_tx.clone(), true);
        cfg.acks.insert(dev("p"));
        r.ctl.configure(cfg);
        r.ctl.handle(mv(1439.0, 400.0, 5.0, 0.0));
        r.ctl.handle(key(KeyCode::A, true));
        // 回执按时到达：保持控制。
        std::thread::sleep(Duration::from_millis(100));
        r.ctl.on_ack(&dev("p"));
        std::thread::sleep(Duration::from_millis(20));
        r.ctl.tick();
        assert_eq!(r.ctl.active_device(), Some(&dev("p")));
        drain(&mut r.pc_rx);
        while r.events.try_recv().is_ok() {}

        // 回执断了：释放远端按键、解锁光标、通知引擎。
        std::thread::sleep(Duration::from_millis(200));
        r.ctl.tick();
        assert_eq!(r.ctl.active_device(), None);
        let msgs = drain(&mut r.pc_rx);
        assert!(msgs.contains(&Message::Key {
            code: KeyCode::A,
            down: false,
            repeat: false
        }));
        assert!(!r.cursor.lock().unwrap().last().unwrap().0);
        assert_eq!(r.events.try_recv(), Ok(ControlEvent::Changed));
        assert_eq!(r.events.try_recv(), Ok(ControlEvent::Lost(dev("p"))));
    }

    #[test]
    fn dwell_switches_via_tick() {
        let mut r = rig(SwitchPolicy {
            dwell_ms: 30,
            ..Default::default()
        });
        assert_eq!(r.ctl.handle(mv(1439.0, 400.0, 5.0, 0.0)), Verdict::Pass);
        assert_eq!(r.ctl.active_device(), None);
        std::thread::sleep(Duration::from_millis(40));
        r.ctl.tick();
        assert_eq!(r.ctl.active_device(), Some(&dev("p")));
    }
}
