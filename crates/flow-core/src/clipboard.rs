//! 剪贴板文本同步：决定什么时候发送、什么时候写入，并防止 A→B→A 的回环。

use std::collections::HashMap;

use crate::layout::DeviceId;
use crate::protocol::{MAX_CLIPBOARD_TEXT, Message};

#[derive(Debug)]
pub struct ClipboardSync {
    local: DeviceId,
    pub enabled: bool,
    next_seq: u64,
    /// 每个来源设备已处理的最大序号，丢弃乱序或重复的旧内容。
    seen: HashMap<DeviceId, u64>,
}

impl ClipboardSync {
    pub fn new(local: DeviceId) -> Self {
        Self {
            local,
            enabled: true,
            next_seq: 0,
            seen: HashMap::new(),
        }
    }

    /// 本机剪贴板变化时调用，返回需要广播给其他设备的消息。
    ///
    /// 流界自己把远端内容写进剪贴板引起的那次"变化"不要报给这里：调用方靠系统的变化计数认出它
    /// （写入后记下新的计数）。以前这里还按内容比对了一遍，但调用方每次写入后都会把记下的内容
    /// 清掉，那段判断从来不起作用，还要多复制一份最长 4 MB 的文本。
    pub fn on_local_change(&mut self, text: &str) -> Option<Message> {
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

    /// 与 `device` 新建立会话时调用：对方可能重启过，序号从 1 重新开始，忘掉之前记下的最大序号。
    /// 否则对方重启（包括每次自动更新）后的前若干次复制都会被当成旧消息丢掉。
    pub fn forget(&mut self, device: &DeviceId) {
        self.seen.remove(device);
    }

    /// 收到远端剪贴板内容时调用，返回需要写入本机剪贴板的文本。
    pub fn on_remote(&mut self, origin: &DeviceId, seq: u64, text: &str) -> Option<String> {
        // 空文本不写：写进去等于清空本机剪贴板，本机复制时也从不发空文本。
        if !self.enabled
            || origin == &self.local
            || text.is_empty()
            || text.len() > MAX_CLIPBOARD_TEXT
        {
            return None;
        }
        if self.seen.get(origin).is_some_and(|&s| s >= seq) {
            return None;
        }
        self.seen.insert(origin.clone(), seq);
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
    fn same_text_copied_locally_after_a_remote_write_still_syncs() {
        let mut c = ClipboardSync::new(dev("b"));
        assert_eq!(c.on_remote(&dev("a"), 1, "hello").as_deref(), Some("hello"));
        // 写入本机剪贴板引起的那次变化由调用方按变化计数认出，不会报到这里；
        // 报到这里的就是用户自己复制的，哪怕内容一样也照常同步。
        assert!(c.on_local_change("hello").is_some());
    }

    #[test]
    fn empty_text_from_a_peer_is_not_applied() {
        let mut c = ClipboardSync::new(dev("b"));
        assert!(c.on_remote(&dev("a"), 1, "").is_none());
        // 没有被当成"已处理"的序号：同一个序号的正常内容还能进来。
        assert!(c.on_remote(&dev("a"), 1, "x").is_some());
    }

    #[test]
    fn stale_or_own_messages_are_ignored() {
        let mut c = ClipboardSync::new(dev("b"));
        assert!(c.on_remote(&dev("a"), 5, "new").is_some());
        assert!(c.on_remote(&dev("a"), 4, "old").is_none());
        assert!(c.on_remote(&dev("b"), 9, "mine").is_none());
    }

    #[test]
    fn peer_restart_is_accepted_after_forget() {
        let mut c = ClipboardSync::new(dev("b"));
        assert!(c.on_remote(&dev("a"), 37, "旧").is_some());
        // 对方重启，序号从 1 开始：不忘掉就会被当成旧消息。
        assert!(c.on_remote(&dev("a"), 1, "新").is_none());
        c.forget(&dev("a"));
        assert_eq!(c.on_remote(&dev("a"), 1, "新").as_deref(), Some("新"));
        assert!(c.on_remote(&dev("a"), 1, "重复").is_none());
    }

    #[test]
    fn disabled_does_nothing() {
        let mut c = ClipboardSync::new(dev("a"));
        c.enabled = false;
        assert!(c.on_local_change("x").is_none());
        assert!(c.on_remote(&dev("z"), 1, "y").is_none());
    }
}
