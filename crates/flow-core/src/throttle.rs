//! 鼠标位置发送节流。
//!
//! 高回报率鼠标（1000 Hz 以上）每秒会产生上千个移动事件，逐条发给远端会让网络排队、
//! 远端逐条注入也跟不上，表现为卡顿和延迟。这里限制两次发送鼠标位置的最小间隔，
//! 间隔内只保留最新位置；其他消息（按键、点击、滚轮等）到来时先把待发的位置发出去，
//! 保证"先移到那里再点"的顺序。时间由调用方以毫秒传入。

use crate::protocol::Message;

#[derive(Debug, Clone)]
pub struct MoveThrottle {
    interval_ms: u64,
    last_sent_ms: Option<u64>,
    pending: Option<Message>,
}

impl MoveThrottle {
    pub fn new(interval_ms: u64) -> Self {
        Self {
            interval_ms,
            last_sent_ms: None,
            pending: None,
        }
    }

    /// 提交一条要发的消息，返回现在应该按顺序发出的消息。
    pub fn push(&mut self, msg: Message, now_ms: u64) -> Vec<Message> {
        if matches!(msg, Message::MouseMove { .. }) {
            let due = self
                .last_sent_ms
                .is_none_or(|t| now_ms.saturating_sub(t) >= self.interval_ms);
            if due {
                self.pending = None;
                self.last_sent_ms = Some(now_ms);
                vec![msg]
            } else {
                self.pending = Some(msg);
                Vec::new()
            }
        } else {
            let mut out = Vec::with_capacity(2);
            if let Some(m) = self.pending.take() {
                self.last_sent_ms = Some(now_ms);
                out.push(m);
            }
            out.push(msg);
            out
        }
    }

    /// 待发的位置应在何时发出（毫秒）；没有待发位置时为 `None`。
    pub fn deadline(&self) -> Option<u64> {
        self.pending.as_ref()?;
        Some(self.last_sent_ms.unwrap_or(0) + self.interval_ms)
    }

    /// 到期后取出待发的位置。
    pub fn flush(&mut self, now_ms: u64) -> Option<Message> {
        let m = self.pending.take()?;
        self.last_sent_ms = Some(now_ms);
        Some(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::MouseButton;

    fn mv(x: f64) -> Message {
        Message::MouseMove {
            session: 1,
            display: "1".into(),
            x,
            y: 0.0,
        }
    }

    fn click() -> Message {
        Message::Button {
            button: MouseButton::Left,
            down: true,
        }
    }

    #[test]
    fn first_move_goes_out_immediately() {
        let mut t = MoveThrottle::new(4);
        assert_eq!(t.push(mv(1.0), 100), vec![mv(1.0)]);
        assert_eq!(t.deadline(), None);
    }

    #[test]
    fn moves_within_interval_keep_only_latest() {
        let mut t = MoveThrottle::new(4);
        t.push(mv(1.0), 100);
        assert!(t.push(mv(2.0), 101).is_empty());
        assert!(t.push(mv(3.0), 102).is_empty());
        assert_eq!(t.deadline(), Some(104));
        assert_eq!(t.flush(104), Some(mv(3.0)));
        assert_eq!(t.flush(105), None);
        // 刚发过，下一次又要等一个间隔。
        assert!(t.push(mv(4.0), 106).is_empty());
        assert_eq!(t.push(mv(5.0), 108), vec![mv(5.0)]);
    }

    #[test]
    fn other_messages_flush_pending_move_first() {
        let mut t = MoveThrottle::new(4);
        t.push(mv(1.0), 100);
        t.push(mv(2.0), 101);
        assert_eq!(t.push(click(), 102), vec![mv(2.0), click()]);
        assert_eq!(t.deadline(), None);
        // 没有待发位置时，其他消息直接发出。
        assert_eq!(t.push(click(), 103), vec![click()]);
    }

    #[test]
    fn thousand_hz_mouse_is_limited_to_interval() {
        let mut t = MoveThrottle::new(4);
        let mut sent = 0;
        for ms in 0..1000u64 {
            if let Some(d) = t.deadline()
                && ms >= d
            {
                sent += t.flush(ms).into_iter().count();
            }
            sent += t.push(mv(ms as f64), ms).len();
        }
        // 1 秒 1000 个事件，最多发出约 250 条。
        assert!(sent <= 251, "sent {sent}");
        assert!(sent >= 240, "sent {sent}");
    }
}

/// 滑动窗口限流：`window_ms` 内最多允许 `max` 次。用来限制未主动发起的配对请求等
/// 可以被局域网里任何人触发的操作。时间由调用方以毫秒传入。
#[derive(Debug, Clone)]
pub struct RateLimit {
    window_ms: u64,
    max: usize,
    hits: std::collections::VecDeque<u64>,
}

impl RateLimit {
    pub fn new(max: usize, window_ms: u64) -> Self {
        Self {
            window_ms,
            max,
            hits: std::collections::VecDeque::new(),
        }
    }

    /// 记录一次并返回是否允许。被拒绝的不计入。
    pub fn allow(&mut self, now_ms: u64) -> bool {
        while self
            .hits
            .front()
            .is_some_and(|&t| now_ms.saturating_sub(t) >= self.window_ms)
        {
            self.hits.pop_front();
        }
        if self.hits.len() >= self.max {
            return false;
        }
        self.hits.push_back(now_ms);
        true
    }
}

#[cfg(test)]
mod rate_limit_tests {
    use super::RateLimit;

    #[test]
    fn allows_max_per_window_then_recovers() {
        let mut r = RateLimit::new(3, 60_000);
        assert!(r.allow(0));
        assert!(r.allow(10));
        assert!(r.allow(20));
        assert!(!r.allow(30));
        assert!(!r.allow(59_999));
        // 第一次过了窗口，空出一个名额。
        assert!(r.allow(60_000));
        assert!(!r.allow(60_005));
        assert!(r.allow(60_010));
    }
}
