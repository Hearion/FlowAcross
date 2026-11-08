//! 剪贴板文本同步：决定什么时候发送、什么时候写入，并防止 A→B→A 的回环。

use std::collections::HashMap;

use crate::layout::DeviceId;
use crate::protocol::{MAX_CLIPBOARD_TEXT, Message};

#[derive(Debug)]
pub struct ClipboardSync {
    local: DeviceId,
    pub enabled: bool,
    next_seq: u64,
    /// 最近一次由远端写入本机剪贴板的内容。本机随后收到的"剪贴板变化"如果就是它，说明是我们自己写的，不再回传。
    last_applied: Option<String>,
    /// 每个来源设备已处理的最大序号，丢弃乱序或重复的旧内容。
    seen: HashMap<DeviceId, u64>,
}

impl ClipboardSync {
    pub fn new(local: DeviceId) -> Self {
        Self {
            local,
            enabled: true,
            next_seq: 0,
            last_applied: None,
            seen: HashMap::new(),
        }
    }

    /// 本机剪贴板变化时调用，返回需要广播给其他设备的消息。
    pub fn on_local_change(&mut self, text: &str) -> Option<Message> {
        if self.last_applied.take().is_some_and(|t| t == text) {
            return None;
        }
        if !self.enabled || text.is_empty() || text.len() > MAX_CLIPBOARD_TEXT {
            return None;
        }
        self.next_seq += 1;
        Some(Message::Clipboard {
            origin: self.local.clone(),
            seq: self.next_seq,
            text: text.to_owned(),
        })
    }

    /// 收到远端剪贴板内容时调用，返回需要写入本机剪贴板的文本。
    pub fn on_remote(&mut self, origin: &DeviceId, seq: u64, text: &str) -> Option<String> {
        if !self.enabled || origin == &self.local || text.len() > MAX_CLIPBOARD_TEXT {
            return None;
        }
        if self.seen.get(origin).is_some_and(|&s| s >= seq) {
            return None;
        }
        self.seen.insert(origin.clone(), seq);
        self.last_applied = Some(text.to_owned());
        Some(text.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(s: &str) -> DeviceId {
        DeviceId(s.into())
    }

    #[test]
    fn local_copy_is_broadcast_with_increasing_seq() {
        let mut c = ClipboardSync::new(dev("a"));
        assert!(matches!(
            c.on_local_change("hi"),
            Some(Message::Clipboard { seq: 1, .. })
        ));
        assert!(matches!(
            c.on_local_change("yo"),
            Some(Message::Clipboard { seq: 2, .. })
        ));
    }

    #[test]
    fn remote_write_is_not_echoed_back() {
        let mut c = ClipboardSync::new(dev("b"));
        assert_eq!(c.on_remote(&dev("a"), 1, "hello").as_deref(), Some("hello"));
        // 写入本机剪贴板后，系统通知"剪贴板变了"——不回传
        assert!(c.on_local_change("hello").is_none());
        // 之后用户自己再复制同样内容，正常同步
        assert!(c.on_local_change("hello").is_some());
    }

    #[test]
    fn stale_or_own_messages_are_ignored() {
        let mut c = ClipboardSync::new(dev("b"));
        assert!(c.on_remote(&dev("a"), 5, "new").is_some());
        assert!(c.on_remote(&dev("a"), 4, "old").is_none());
        assert!(c.on_remote(&dev("b"), 9, "mine").is_none());
    }

    #[test]
    fn disabled_does_nothing() {
        let mut c = ClipboardSync::new(dev("a"));
        c.enabled = false;
        assert!(c.on_local_change("x").is_none());
        assert!(c.on_remote(&dev("z"), 1, "y").is_none());
    }
}
