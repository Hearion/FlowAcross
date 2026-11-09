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

/// 控制器通知引擎的事件。
#[derive(Debug, Clone, PartialEq)]
pub enum ControlEvent {
    /// 键鼠所在的设备变了。
    Changed,
    /// 正在控制的设备不可用了（断线、被暂停、被移除），键鼠已回到本机。
    Lost(DeviceId),
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
    /// 共享未暂停。
    pub enabled: bool,
}

struct Active {
    device: DeviceId,
    tx: Output,
    cursor: VirtualCursor,
    session: u32,
    last_beat: Instant,
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
}

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
        }
    }

    /// 正在控制的远端设备；`None` 表示键鼠在本机。
    pub fn active_device(&self) -> Option<&DeviceId> {
        self.active.as_ref().map(|a| &a.device)
    }

    pub fn configure(&mut self, cfg: ControllerConfig) {
        self.switcher.policy = cfg.policy.clone();
        self.cfg = cfg;
        let Some(a) = &self.active else {
            return;
        };
        let reachable = self.cfg.outputs.contains_key(&a.device);
        if !reachable || !self.cfg.enabled {
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

    fn held(&self) -> HeldInput {
        HeldInput {
            modifiers: self.router.held_modifiers(),
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
                    self.send(Message::Wheel { dx, dy });
                    Verdict::Suppress
                } else {
                    Verdict::Pass
                }
            }
            CapturedEvent::Key { code, down, repeat } => self.key(code, down, repeat),
        }
    }

    fn key(&mut self, code: KeyCode, down: bool, repeat: bool) -> Verdict {
        if self.swallow.contains(&code) {
            if !down {
                self.swallow.remove(&code);
            }
            return Verdict::Suppress;
        }
        // 快捷键：Ctrl + Alt（Mac 上 ⌃⌥）+ ←/→ 切到左右相邻的电脑。
        if down
            && !repeat
            && (code == KeyCode::LEFT || code == KeyCode::RIGHT)
            && self
                .router
                .held_modifiers()
                .contains(Modifiers::CTRL | Modifiers::ALT)
            && self.hotkey_switch(code == KeyCode::RIGHT)
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
            self.leave_to_local(Some(c.point));
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
        info!(device = %&target.device.0[..8.min(target.device.0.len())], "键鼠切到远端");
        self.router.enter_remote();
        self.switcher.reset();
        self.active = Some(Active {
            device: target.device.clone(),
            tx,
            cursor: VirtualCursor::new(target, point),
            session,
            last_beat: Instant::now(),
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

    /// 快捷键切换：按设备在布局中的水平位置，切到左边或右边最近的一台。
    fn hotkey_switch(&mut self, right: bool) -> bool {
        if !self.cfg.enabled || self.cfg.outputs.is_empty() {
            return false;
        }
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
        let Some(cx) = center_x(&current) else {
            return false;
        };
        let target = layout
            .devices()
            .into_iter()
            .filter(|d| d != &current && (d == &self.local || self.cfg.outputs.contains_key(d)))
            .filter_map(|d| center_x(&d).map(|x| (d, x)))
            .filter(|(_, x)| if right { *x > cx } else { *x < cx })
            .min_by(|a, b| (a.1 - cx).abs().total_cmp(&(b.1 - cx).abs()))
            .map(|(d, _)| d);
        let Some(target) = target else {
            return false;
        };
        if target == self.local {
            self.leave_to_local(None);
            return true;
        }
        // 进入目标设备面积最大的屏幕的中央。
        let Some(d) = layout.displays_of(&target).max_by(|a, b| {
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

    /// 定时调用（约每 40 毫秒）：发送心跳、处理贴边停留计时。
    pub fn tick(&mut self) {
        if let Some(a) = self.active.as_mut()
            && a.last_beat.elapsed() >= HEARTBEAT
        {
            self.heartbeat_seq = self.heartbeat_seq.wrapping_add(1);
            let _ = a.tx.send(Message::Heartbeat {
                seq: self.heartbeat_seq,
            });
            a.last_beat = Instant::now();
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
            enabled: true,
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
    fn hotkey_switches_and_swallows_arrow() {
        let mut r = rig(SwitchPolicy {
            locked: true,
            ..Default::default()
        });
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
