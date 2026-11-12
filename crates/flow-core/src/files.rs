//! 跨设备复制文件：文件清单、路径校验、重名处理、接收校验，以及"什么时候去取"的判定。
//!
//! 流程：
//! 1. A 上复制了文件 → A 向支持的设备广播 [`Message::FileOffer`]，只有清单，不含内容。
//! 2. 键鼠来到 B 时（B 开始被控制，或 B 控制远端后回到本机），B 在同一条 QUIC 连接上新开一条流，
//!    发 [`Message::FileRequest`]；A 依次回 `FileEntry` / `FileChunk`，最后 `FileEnd`（失败时 `FileError`）。
//! 3. B 收完后把文件移进接收文件夹，并写入本机剪贴板，用户直接粘贴。
//!
//! 文件内容走单独的流，不会挡住控制流里的键鼠消息。

use crate::layout::DeviceId;
use crate::protocol::{Message, version_at_least};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 从这个版本起支持跨设备复制文件。对方更旧时不发文件清单（旧版本解析不了新消息，会断开连接）。
pub const FILE_TRANSFER_SINCE: (u32, u32, u32) = (0, 1, 5);

/// 每个 `FileChunk` 最多这么多字节。
pub const CHUNK_SIZE: usize = 256 * 1024;

/// 不超过这个大小的文件，键鼠来到本机时自动去取；更大的要用户在界面上点「接收」。
pub const AUTO_FETCH_LIMIT: u64 = 1 << 30;

/// 键鼠来到本机后这么久之内收到的文件清单也自动去取（复制后马上切过来，清单可能晚到一点）。
pub const ARRIVAL_WINDOW_MS: u64 = 2_000;

/// 一次最多传这么多个文件和目录。
pub const MAX_ENTRIES: usize = 100_000;

/// 清单里最多列出这么多个顶层名称（界面显示用）。
pub const MAX_LISTED: usize = 20;

/// 目录最多嵌套这么多层。
pub const MAX_DEPTH: usize = 64;

/// 一次复制的文件清单（不含内容）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileOffer {
    /// 提供方本地递增的编号，取文件时带上。提供方的剪贴板换了内容后，旧编号就取不到了。
    pub id: u64,
    /// 顶层条目的名称（最多 [`MAX_LISTED`] 个）。
    pub names: Vec<String>,
    /// 顶层条目总数。为 0 表示撤回：提供方的剪贴板里已经不是文件了。
    pub items: u32,
    /// 文件总数（不含目录）。
    pub files: u64,
    /// 总字节数。
    pub bytes: u64,
}

impl FileOffer {
    pub fn withdrawn(id: u64) -> Self {
        Self {
            id,
            names: Vec::new(),
            items: 0,
            files: 0,
            bytes: 0,
        }
    }

    pub fn is_withdrawn(&self) -> bool {
        self.items == 0
    }

    /// 给用户看的简短描述：「报告.pdf」或「报告.pdf 等 3 项」。
    pub fn label(&self) -> String {
        let first = self.names.first().map(String::as_str).unwrap_or("文件");
        match self.items {
            0 | 1 => first.to_owned(),
            n => format!("{first} 等 {n} 项"),
        }
    }
}

/// 对方是否支持跨设备复制文件：版本不低于 [`FILE_TRANSFER_SINCE`]。
///
/// 不能用"版本号相同"判断：开发版本和已发布的同号版本协议不同，发错消息会让对方断开连接。
/// 本机开发调试时，先把两边的版本号改到不低于 [`FILE_TRANSFER_SINCE`]。
pub fn peer_supports_files(peer_version: &str) -> bool {
    version_at_least(peer_version, FILE_TRANSFER_SINCE)
}

/// 系统生成的、不值得传的文件。
pub fn is_junk(name: &str) -> bool {
    matches!(name, ".DS_Store" | "Thumbs.db" | "desktop.ini") || name.starts_with("._")
}

const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 把一段文件名改成在 macOS 和 Windows 上都合法的名字：替换 Windows 不允许的字符和控制字符，
/// 去掉结尾的点和空格，避开 CON、NUL 等保留名，长度不超过 255 字节。
pub fn sanitize_component(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    while s.ends_with(['.', ' ']) {
        s.pop();
    }
    if s.is_empty() {
        s.push('_');
    }
    let stem = s.split('.').next().unwrap_or("").trim_end();
    if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
        s.insert(0, '_');
    }
    if s.len() > 255 {
        let mut end = 255;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        // 截断后结尾可能又露出点或空格。
        while s.ends_with(['.', ' ']) {
            s.pop();
        }
    }
    s
}

/// 解析对方发来的相对路径（`/` 分隔）。绝对路径、空段、`.`、`..` 一律拒绝，
/// 防止写到接收目录外面；每一段都经过 [`sanitize_component`]。
pub fn parse_rel_path(path: &str) -> Option<Vec<String>> {
    if path.is_empty() || path.starts_with('/') {
        return None;
    }
    let mut out = Vec::new();
    for comp in path.split('/') {
        if comp.is_empty() || comp == "." || comp == ".." {
            return None;
        }
        out.push(sanitize_component(comp));
    }
    (out.len() <= MAX_DEPTH).then_some(out)
}

/// 接收目录里已经有同名的文件时改名：「报告.pdf」→「报告 (1).pdf」，目录「照片」→「照片 (1)」。
pub fn unique_name(name: &str, dir: bool, exists: impl Fn(&str) -> bool) -> String {
    if !exists(name) {
        return name.to_owned();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if !dir && i > 0 => name.split_at(i),
        _ => (name, ""),
    };
    (1u32..)
        .map(|i| format!("{stem} ({i}){ext}"))
        .find(|c| !exists(c))
        .expect("总能找到不重名的名字")
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ReceiveError {
    #[error("{0}")]
    Remote(String),
    #[error("对方发来的文件路径不合法：{0}")]
    BadPath(String),
    #[error("文件数据和声明的大小不一致，可能是文件在传输过程中被修改了")]
    SizeMismatch,
    #[error("文件数量超过上限（{MAX_ENTRIES} 个）")]
    TooMany,
    #[error("传输数据格式错误")]
    Unexpected,
}

/// 接收方要执行的一步文件操作。路径是相对接收目录、已经校验过的各段名称。
#[derive(Debug, PartialEq, Eq)]
pub enum ReceiveStep<'a> {
    MakeDir(Vec<String>),
    /// 新建文件，接下来的 `Write` 都写进它。`size` 为 0 时文件已经完整。
    CreateFile {
        path: Vec<String>,
        size: u64,
    },
    /// 追加到当前文件；`last` 为 true 表示这个文件收完了。
    Write {
        data: &'a [u8],
        last: bool,
    },
    Finished,
}

/// 校验传输流里的消息顺序和大小，翻译成文件操作。不碰文件系统。
#[derive(Debug, Default)]
pub struct Incoming {
    /// 当前文件还差多少字节。
    remaining: Option<u64>,
    received: u64,
    entries: usize,
    finished: bool,
}

impl Incoming {
    /// 已收到的文件字节数。
    pub fn received(&self) -> u64 {
        self.received
    }

    pub fn finished(&self) -> bool {
        self.finished
    }

    pub fn step<'a>(&mut self, msg: &'a Message) -> Result<ReceiveStep<'a>, ReceiveError> {
        if self.finished {
            return Err(ReceiveError::Unexpected);
        }
        match msg {
            Message::FileEntry { path, size, dir } => {
                if self.remaining.is_some() {
                    return Err(ReceiveError::SizeMismatch);
                }
                self.entries += 1;
                if self.entries > MAX_ENTRIES {
                    return Err(ReceiveError::TooMany);
                }
                let parts =
                    parse_rel_path(path).ok_or_else(|| ReceiveError::BadPath(path.clone()))?;
                if *dir {
                    return Ok(ReceiveStep::MakeDir(parts));
                }
                if *size > 0 {
                    self.remaining = Some(*size);
                }
                Ok(ReceiveStep::CreateFile {
                    path: parts,
                    size: *size,
                })
            }
            Message::FileChunk(data) => {
                let Some(left) = self.remaining else {
                    return Err(ReceiveError::SizeMismatch);
                };
                let len = data.len() as u64;
                if len == 0 || len > left {
                    return Err(ReceiveError::SizeMismatch);
                }
                self.received += len;
                let last = len == left;
                self.remaining = (!last).then_some(left - len);
                Ok(ReceiveStep::Write { data, last })
            }
            Message::FileEnd => {
                if self.remaining.is_some() {
                    return Err(ReceiveError::SizeMismatch);
                }
                self.finished = true;
                Ok(ReceiveStep::Finished)
            }
            Message::FileError { reason } => Err(ReceiveError::Remote(reason.clone())),
            _ => Err(ReceiveError::Unexpected),
        }
    }
}

/// 接收方：记住其他设备最近一次复制的文件，决定什么时候去取。
///
/// 剪贴板只有一份内容，所以只记最新的一份清单。键鼠来到本机时（开始被控制，或控制远端后回到本机）
/// 说明用户可能要在这里粘贴，这时自动去取；太大的留给用户在界面上手动接收。
#[derive(Debug, Default)]
pub struct OfferBook {
    pending: Option<(DeviceId, FileOffer)>,
    arrived_at: Option<u64>,
}

impl OfferBook {
    /// 收到清单。`controlled`：本机此刻正被其他设备控制（键鼠就在这台电脑上）。
    /// 返回需要现在就去取的清单。
    pub fn on_offer(
        &mut self,
        from: DeviceId,
        offer: FileOffer,
        controlled: bool,
        now_ms: u64,
    ) -> Option<(DeviceId, FileOffer)> {
        if offer.is_withdrawn() {
            self.clear_from(&from);
            return None;
        }
        self.pending = Some((from, offer));
        let recent = self
            .arrived_at
            .is_some_and(|t| now_ms.saturating_sub(t) <= ARRIVAL_WINDOW_MS);
        if controlled || recent {
            self.take_auto()
        } else {
            None
        }
    }

    /// 键鼠来到本机。有可以自动去取的清单时返回它。
    pub fn on_arrive(&mut self, now_ms: u64) -> Option<(DeviceId, FileOffer)> {
        self.arrived_at = Some(now_ms);
        self.take_auto()
    }

    /// 剪贴板换成了别的内容（本机复制了东西，或收到了其他设备的文字），之前的清单作废。
    pub fn clear(&mut self) {
        self.pending = None;
    }

    /// 某台设备撤回了清单或断开了连接。
    pub fn clear_from(&mut self, device: &DeviceId) {
        if self.pending.as_ref().is_some_and(|(d, _)| d == device) {
            self.pending = None;
        }
    }

    /// 等待用户手动接收的清单。
    pub fn pending(&self) -> Option<&(DeviceId, FileOffer)> {
        self.pending.as_ref()
    }

    /// 取出来的清单这次没能去取（比如此刻暂停了共享）：放回去，等用户手动接收。
    /// 期间已经来了新的清单就以新的为准。
    pub fn restore(&mut self, from: DeviceId, offer: FileOffer) {
        if self.pending.is_none() {
            self.pending = Some((from, offer));
        }
    }

    /// 用户点了「接收」。
    pub fn take(&mut self) -> Option<(DeviceId, FileOffer)> {
        self.pending.take()
    }

    fn take_auto(&mut self) -> Option<(DeviceId, FileOffer)> {
        if self
            .pending
            .as_ref()
            .is_some_and(|(_, o)| o.bytes <= AUTO_FETCH_LIMIT)
        {
            self.pending.take()
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(s: &str) -> DeviceId {
        DeviceId(s.into())
    }

    fn offer(id: u64, bytes: u64) -> FileOffer {
        FileOffer {
            id,
            names: vec!["报告.pdf".into(), "照片".into()],
            items: 2,
            files: 5,
            bytes,
        }
    }

    #[test]
    fn labels() {
        assert_eq!(offer(1, 1).label(), "报告.pdf 等 2 项");
        let mut one = offer(1, 1);
        one.items = 1;
        assert_eq!(one.label(), "报告.pdf");
        assert!(FileOffer::withdrawn(3).is_withdrawn());
    }

    #[test]
    fn version_support() {
        assert!(peer_supports_files("0.1.5"));
        assert!(peer_supports_files("0.2.0-beta.1"));
        assert!(!peer_supports_files("0.1.4"));
        assert!(!peer_supports_files("dev"));
    }

    #[test]
    fn sanitize_names_for_both_systems() {
        assert_eq!(sanitize_component("a:b?c*.txt"), "a_b_c_.txt");
        assert_eq!(sanitize_component("结尾有点. "), "结尾有点");
        assert_eq!(sanitize_component("..."), "_");
        assert_eq!(sanitize_component("con"), "_con");
        assert_eq!(sanitize_component("NUL.txt"), "_NUL.txt");
        assert_eq!(sanitize_component("console.log"), "console.log");
        assert_eq!(sanitize_component("tab\there"), "tab_here");
        let long = "长".repeat(100); // 300 字节
        let s = sanitize_component(&long);
        assert!(s.len() <= 255 && s.chars().all(|c| c == '长'));
        // 截断处正好露出点和空格：也要去掉。
        let s = sanitize_component(&format!("{}. .txt", "a".repeat(253)));
        assert_eq!(s, "a".repeat(253));
    }

    #[test]
    fn rejects_paths_escaping_the_target() {
        assert_eq!(
            parse_rel_path("照片/2024/a.jpg"),
            Some(vec!["照片".into(), "2024".into(), "a.jpg".into()])
        );
        assert_eq!(parse_rel_path("../etc/passwd"), None);
        assert_eq!(parse_rel_path("a/../../b"), None);
        assert_eq!(parse_rel_path("/abs"), None);
        assert_eq!(parse_rel_path("a//b"), None);
        assert_eq!(parse_rel_path("./a"), None);
        assert_eq!(parse_rel_path(""), None);
        // Windows 风格的路径不会被当成分隔符或盘符。
        assert_eq!(parse_rel_path("C:\\x"), Some(vec!["C__x".into()]));
        assert_eq!(parse_rel_path(&"d/".repeat(MAX_DEPTH + 1)[..]), None);
    }

    #[test]
    fn unique_names() {
        let taken = ["报告.pdf", "报告 (1).pdf", "照片", ".bashrc"];
        let exists = |n: &str| taken.contains(&n);
        assert_eq!(unique_name("新文件.txt", false, exists), "新文件.txt");
        assert_eq!(unique_name("报告.pdf", false, exists), "报告 (2).pdf");
        assert_eq!(unique_name("照片", true, exists), "照片 (1)");
        assert_eq!(unique_name(".bashrc", false, exists), ".bashrc (1)");
    }

    #[test]
    fn junk() {
        assert!(is_junk(".DS_Store"));
        assert!(is_junk("._报告.pdf"));
        assert!(!is_junk("报告.pdf"));
    }

    fn entry(path: &str, size: u64, dir: bool) -> Message {
        Message::FileEntry {
            path: path.into(),
            size,
            dir,
        }
    }

    #[test]
    fn incoming_happy_path() {
        let msgs = [
            entry("照片", 0, true),
            entry("照片/a.jpg", 5, false),
            Message::FileChunk(vec![1, 2, 3]),
            Message::FileChunk(vec![4, 5]),
            entry("空.txt", 0, false),
            Message::FileEnd,
        ];
        let mut inc = Incoming::default();
        let steps: Vec<_> = msgs.iter().map(|m| inc.step(m).unwrap()).collect();
        assert_eq!(steps[0], ReceiveStep::MakeDir(vec!["照片".into()]));
        assert_eq!(
            steps[1],
            ReceiveStep::CreateFile {
                path: vec!["照片".into(), "a.jpg".into()],
                size: 5
            }
        );
        assert_eq!(
            steps[2],
            ReceiveStep::Write {
                data: &[1, 2, 3],
                last: false
            }
        );
        assert_eq!(
            steps[3],
            ReceiveStep::Write {
                data: &[4, 5],
                last: true
            }
        );
        assert!(matches!(steps[4], ReceiveStep::CreateFile { size: 0, .. }));
        assert_eq!(steps[5], ReceiveStep::Finished);
        assert_eq!(inc.received(), 5);
        assert!(inc.finished());
        assert_eq!(inc.step(&Message::FileEnd), Err(ReceiveError::Unexpected));
    }

    #[test]
    fn incoming_rejects_bad_streams() {
        // 数据比声明的多
        let mut inc = Incoming::default();
        inc.step(&entry("a", 2, false)).unwrap();
        assert_eq!(
            inc.step(&Message::FileChunk(vec![0; 3])),
            Err(ReceiveError::SizeMismatch)
        );
        // 上一个文件没收完就开始下一个
        let mut inc = Incoming::default();
        inc.step(&entry("a", 2, false)).unwrap();
        assert_eq!(
            inc.step(&entry("b", 1, false)),
            Err(ReceiveError::SizeMismatch)
        );
        // 没收完就结束
        let mut inc = Incoming::default();
        inc.step(&entry("a", 2, false)).unwrap();
        assert_eq!(inc.step(&Message::FileEnd), Err(ReceiveError::SizeMismatch));
        // 没有文件却来了数据
        let mut inc = Incoming::default();
        assert_eq!(
            inc.step(&Message::FileChunk(vec![1])),
            Err(ReceiveError::SizeMismatch)
        );
        // 不安全的路径
        let mut inc = Incoming::default();
        assert!(matches!(
            inc.step(&entry("../x", 1, false)),
            Err(ReceiveError::BadPath(_))
        ));
        // 对方报错
        let mut inc = Incoming::default();
        assert_eq!(
            inc.step(&Message::FileError {
                reason: "文件已被删除".into()
            }),
            Err(ReceiveError::Remote("文件已被删除".into()))
        );
        // 其他消息
        let mut inc = Incoming::default();
        assert_eq!(
            inc.step(&Message::ReleaseAll),
            Err(ReceiveError::Unexpected)
        );
    }

    #[test]
    fn fetch_when_keyboard_and_mouse_arrive() {
        let mut book = OfferBook::default();
        // 用户在 A 上复制，键鼠还在 A：先记下，不取。
        assert_eq!(book.on_offer(dev("a"), offer(1, 10), false, 0), None);
        assert!(book.pending().is_some());
        // 键鼠来到本机：取。
        assert_eq!(book.on_arrive(5_000), Some((dev("a"), offer(1, 10))));
        assert!(book.pending().is_none());
        // 再次来到本机不会重复取。
        assert_eq!(book.on_arrive(9_000), None);
    }

    #[test]
    fn offer_arriving_shortly_after_switch_or_while_controlled_is_fetched() {
        let mut book = OfferBook::default();
        book.on_arrive(1_000);
        assert!(
            book.on_offer(dev("a"), offer(1, 10), false, 2_500)
                .is_some()
        );
        assert!(
            book.on_offer(dev("a"), offer(2, 10), false, 1_000 + ARRIVAL_WINDOW_MS + 1)
                .is_none()
        );
        assert!(
            book.on_offer(dev("a"), offer(3, 10), true, 99_000)
                .is_some()
        );
    }

    #[test]
    fn large_offers_wait_for_the_user() {
        let mut book = OfferBook::default();
        let big = offer(1, AUTO_FETCH_LIMIT + 1);
        assert_eq!(book.on_offer(dev("a"), big.clone(), true, 0), None);
        assert_eq!(book.on_arrive(10), None);
        assert_eq!(book.take(), Some((dev("a"), big)));
    }

    #[test]
    fn restored_offer_waits_and_does_not_replace_a_newer_one() {
        let mut book = OfferBook::default();
        book.on_offer(dev("a"), offer(1, 10), false, 0);
        let (from, o) = book.on_arrive(5).unwrap();
        // 这次没能去取：放回去，界面上还能手动接收，键鼠下次来到时也会再试。
        book.restore(from.clone(), o.clone());
        assert_eq!(book.pending().map(|(_, o)| o.id), Some(1));
        assert!(book.on_arrive(9_000).is_some());
        // 期间来了新的清单：不被旧的顶掉。
        book.on_offer(dev("b"), offer(2, 10), false, 20_000);
        book.restore(from, o);
        assert_eq!(book.pending().map(|(_, o)| o.id), Some(2));
    }

    #[test]
    fn newer_clipboard_content_replaces_offer() {
        let mut book = OfferBook::default();
        book.on_offer(dev("a"), offer(1, 10), false, 0);
        book.on_offer(dev("b"), offer(7, 10), false, 0);
        assert_eq!(
            book.pending().map(|(d, o)| (d.0.as_str(), o.id)),
            Some(("b", 7))
        );
        // 另一台设备撤回不影响
        book.on_offer(dev("a"), FileOffer::withdrawn(2), false, 0);
        assert!(book.pending().is_some());
        // 提供方撤回
        book.on_offer(dev("b"), FileOffer::withdrawn(8), false, 0);
        assert!(book.pending().is_none());
        // 剪贴板换成别的内容
        book.on_offer(dev("a"), offer(3, 10), false, 0);
        book.clear();
        assert_eq!(book.on_arrive(1), None);
    }
}
