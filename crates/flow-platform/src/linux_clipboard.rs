//! Linux（X11）剪贴板：文本、文件和图片，走 `CLIPBOARD` 选区。
//!
//! X11 的剪贴板不存数据：谁复制了，谁就成为选区的"所有者"，别人粘贴时再向它要。所以流界写入剪贴板后
//! 必须一直在线应答请求，这件事由一个进程内只有一份的后台服务（[`Service`]）负责，它有自己的 X 连接、
//! 一个不可见的窗口和一个事件线程；[`X11Clipboard`] 只是它的句柄，可以随时创建多个。
//!
//! - **读**：向所有者请求某个格式，数据写到我们窗口的属性上，事件线程读出来交回给调用方。
//!   大数据按 ICCCM 的 `INCR` 约定分块收。调用方最多等一小段时间（所有者不应答时返回"没有"）。
//! - **写**：把各格式的数据存在服务里，声明成为所有者；别的程序请求时事件线程应答，大数据用 `INCR` 分块发。
//!   流界退出后剪贴板里由它写入的内容会消失（除非桌面有剪贴板管理器接手），这是 X11 的通常行为。
//! - **变化计数**：XFixes 在所有者变化时通知；自己写入时直接加一，不等通知，调用方写完马上读计数就是新的。
//!
//! 格式：文本 `UTF8_STRING` / `text/plain;charset=utf-8`；文件 `text/uri-list` 加上 GNOME 文件管理器
//! 认的 `x-special/gnome-copied-files`；图片只认 `image/png`。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask,
    PropMode, Property, SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, SelectionRequestEvent,
    Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::linux::{self, os_err};
use crate::{ClipboardAccess, ClipboardImage, PlatformError};

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        CLIPBOARD,
        TARGETS,
        INCR,
        UTF8_STRING,
        FLOWACROSS_SELECTION,
        TEXT_PLAIN_UTF8: b"text/plain;charset=utf-8",
        TEXT_PLAIN: b"text/plain",
        TEXT_URI_LIST: b"text/uri-list",
        GNOME_COPIED_FILES: b"x-special/gnome-copied-files",
        IMAGE_PNG: b"image/png",
    }
}

/// 读文本、文件清单最多等这么久。引擎是拿着状态锁来读的，所有者不应答时不能卡太久。
const READ_TIMEOUT: Duration = Duration::from_millis(500);
/// 读图片在后台线程做，数据大，多等一会儿。
const IMAGE_TIMEOUT: Duration = Duration::from_secs(5);
/// 一次属性写入最多带的字节数；超过就用 `INCR` 分块。
const MAX_CHUNK: usize = 1 << 20;
/// 对方迟迟不取下一块的 `INCR` 发送，过这么久就放弃。
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(60);

/// 一次写入剪贴板的内容：各个格式及其数据。
type Offer = Arc<Vec<(Atom, Arc<Vec<u8>>)>>;

/// 正在等结果的一次读取。
struct Pending {
    target: Atom,
    tx: mpsc::Sender<Option<Vec<u8>>>,
    /// `Some`：所有者选择分块发，这里攒着已收到的部分。
    incr: Option<Vec<u8>>,
}

/// 正在分块发给别的程序的数据。
struct Transfer {
    requestor: Window,
    property: Atom,
    target: Atom,
    data: Arc<Vec<u8>>,
    offset: usize,
    started: Instant,
}

struct Service {
    conn: RustConnection,
    win: Window,
    atoms: Atoms,
    chunk: usize,
    count: AtomicU64,
    /// 流界是所有者时，这里是它提供的内容。
    owned: Mutex<Option<Offer>>,
    pending: Mutex<Option<Pending>>,
    /// 同一时间只做一次读取（共用一个属性）。
    read_lock: Mutex<()>,
    /// 当前所有者支持的格式，按变化计数缓存。
    targets: Mutex<Option<(u64, Vec<Atom>)>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn service() -> Result<Arc<Service>, PlatformError> {
    static SERVICE: OnceLock<Result<Arc<Service>, String>> = OnceLock::new();
    SERVICE
        .get_or_init(|| Service::start().map_err(|e| e.to_string()))
        .clone()
        .map_err(PlatformError::Os)
}

impl Service {
    fn start() -> Result<Arc<Self>, PlatformError> {
        let (conn, screen) = linux::connect()?;
        let root = conn.setup().roots[screen].root;
        let fail = |e: &dyn std::fmt::Display| os_err("初始化剪贴板失败", e);
        let atoms = Atoms::new(&conn)
            .map_err(|e| fail(&e))?
            .reply()
            .map_err(|e| fail(&e))?;
        let win = conn.generate_id().map_err(|e| fail(&e))?;
        conn.create_window(
            0,
            win,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(|e| fail(&e))?;
        conn.xfixes_query_version(5, 0)
            .map_err(|e| fail(&e))?
            .reply()
            .map_err(|e| os_err("XFixes 不可用", e))?;
        conn.xfixes_select_selection_input(
            win,
            atoms.CLIPBOARD,
            SelectionEventMask::SET_SELECTION_OWNER
                | SelectionEventMask::SELECTION_WINDOW_DESTROY
                | SelectionEventMask::SELECTION_CLIENT_CLOSE,
        )
        .map_err(|e| fail(&e))?;
        conn.flush().map_err(|e| fail(&e))?;
        // 留出请求头的空间。
        let chunk = conn
            .maximum_request_bytes()
            .saturating_sub(256)
            .clamp(4096, MAX_CHUNK);
        let service = Arc::new(Service {
            conn,
            win,
            atoms,
            chunk,
            count: AtomicU64::new(1),
            owned: Mutex::new(None),
            pending: Mutex::new(None),
            read_lock: Mutex::new(()),
            targets: Mutex::new(None),
        });
        let for_thread = service.clone();
        std::thread::Builder::new()
            .name("flowacross-x11-clipboard".into())
            .spawn(move || for_thread.run())
            .map_err(|e| os_err("无法创建剪贴板线程", e))?;
        Ok(service)
    }

    // ----- 事件线程 -----

    fn run(&self) {
        let mut transfers: Vec<Transfer> = Vec::new();
        while let Ok(event) = self.conn.wait_for_event() {
            match event {
                Event::SelectionRequest(e) => self.serve(&e, &mut transfers),
                Event::SelectionClear(e) if e.selection == self.atoms.CLIPBOARD => {
                    // 可能是上一次所有权留下的迟到通知：确认现在真的不是我们了再清。
                    let owner = self
                        .conn
                        .get_selection_owner(self.atoms.CLIPBOARD)
                        .ok()
                        .and_then(|c| c.reply().ok())
                        .map(|r| r.owner);
                    if owner != Some(self.win) {
                        *lock(&self.owned) = None;
                    }
                }
                Event::SelectionNotify(e) => self.on_selection_notify(e.target, e.property),
                Event::PropertyNotify(e) => {
                    if e.window == self.win {
                        if e.atom == self.atoms.FLOWACROSS_SELECTION
                            && e.state == Property::NEW_VALUE
                        {
                            self.on_incr_chunk();
                        }
                    } else if e.state == Property::DELETE {
                        self.continue_transfer(&mut transfers, e.window, e.atom);
                    }
                }
                // 自己写入时已经加过一了。
                Event::XfixesSelectionNotify(e) if e.owner != self.win => {
                    self.count.fetch_add(1, Ordering::SeqCst);
                }
                _ => {}
            }
            transfers.retain(|t| t.started.elapsed() < TRANSFER_TIMEOUT);
            let _ = self.conn.flush();
        }
    }

    fn read_property(&self, delete: bool) -> Option<(Atom, Vec<u8>)> {
        let r = self
            .conn
            .get_property(
                delete,
                self.win,
                self.atoms.FLOWACROSS_SELECTION,
                AtomEnum::ANY,
                0,
                u32::MAX,
            )
            .ok()?
            .reply()
            .ok()?;
        Some((r.type_, r.value))
    }

    fn finish(&self, data: Option<Vec<u8>>) {
        if let Some(p) = lock(&self.pending).take() {
            let _ = p.tx.send(data);
        }
    }

    fn on_selection_notify(&self, target: Atom, property: Atom) {
        {
            let pending = lock(&self.pending);
            match pending.as_ref() {
                Some(p) if p.target == target && p.incr.is_none() => {}
                _ => return,
            }
        }
        if property == x11rb::NONE {
            // 所有者没有这个格式，或者根本没有所有者。
            return self.finish(None);
        }
        match self.read_property(false) {
            Some((kind, _)) if kind == self.atoms.INCR => {
                if let Some(p) = lock(&self.pending).as_mut() {
                    p.incr = Some(Vec::new());
                }
                // 删掉属性表示"可以开始发了"，之后每来一块删一次。
                let _ = self
                    .conn
                    .delete_property(self.win, self.atoms.FLOWACROSS_SELECTION);
            }
            Some((_, value)) => {
                let _ = self
                    .conn
                    .delete_property(self.win, self.atoms.FLOWACROSS_SELECTION);
                self.finish(Some(value));
            }
            None => self.finish(None),
        }
    }

    fn on_incr_chunk(&self) {
        if !lock(&self.pending)
            .as_ref()
            .is_some_and(|p| p.incr.is_some())
        {
            return;
        }
        let Some((_, value)) = self.read_property(true) else {
            return self.finish(None);
        };
        let mut pending = lock(&self.pending);
        let Some(p) = pending.as_mut() else {
            return;
        };
        if value.is_empty() {
            // 空块表示发完了。
            let data = p.incr.take();
            if let Some(p) = pending.take() {
                let _ = p.tx.send(data);
            }
        } else if let Some(buf) = p.incr.as_mut() {
            buf.extend_from_slice(&value);
        }
    }

    fn serve(&self, e: &SelectionRequestEvent, transfers: &mut Vec<Transfer>) {
        // 很老的程序不给属性名，约定用格式名当属性名。
        let property = if e.property == x11rb::NONE {
            e.target
        } else {
            e.property
        };
        let offer = lock(&self.owned).clone();
        let mut ok = false;
        if e.selection == self.atoms.CLIPBOARD
            && let Some(offer) = offer
        {
            if e.target == self.atoms.TARGETS {
                let mut list = vec![self.atoms.TARGETS];
                list.extend(offer.iter().map(|(t, _)| *t));
                ok = self
                    .conn
                    .change_property32(
                        PropMode::REPLACE,
                        e.requestor,
                        property,
                        AtomEnum::ATOM,
                        &list,
                    )
                    .is_ok();
            } else if let Some((_, data)) = offer.iter().find(|(t, _)| *t == e.target) {
                if data.len() > self.chunk {
                    // 先订阅对方窗口的属性变化，再放上 INCR 标记：对方每删一次属性，我们发下一块。
                    let _ = self.conn.change_window_attributes(
                        e.requestor,
                        &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
                    );
                    ok = self
                        .conn
                        .change_property32(
                            PropMode::REPLACE,
                            e.requestor,
                            property,
                            self.atoms.INCR,
                            &[u32::try_from(data.len()).unwrap_or(u32::MAX)],
                        )
                        .is_ok();
                    if ok {
                        transfers
                            .retain(|t| !(t.requestor == e.requestor && t.property == property));
                        transfers.push(Transfer {
                            requestor: e.requestor,
                            property,
                            target: e.target,
                            data: data.clone(),
                            offset: 0,
                            started: Instant::now(),
                        });
                    }
                } else {
                    ok = self
                        .conn
                        .change_property8(PropMode::REPLACE, e.requestor, property, e.target, data)
                        .is_ok();
                }
            }
        }
        let reply = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property: if ok { property } else { x11rb::NONE },
        };
        let _ = self
            .conn
            .send_event(false, e.requestor, EventMask::NO_EVENT, reply);
    }

    fn continue_transfer(&self, transfers: &mut Vec<Transfer>, window: Window, property: Atom) {
        let Some(i) = transfers
            .iter()
            .position(|t| t.requestor == window && t.property == property)
        else {
            return;
        };
        let t = &mut transfers[i];
        let end = (t.offset + self.chunk).min(t.data.len());
        // 发完后再发一个空块表示结束。
        let chunk = &t.data[t.offset..end];
        let done = chunk.is_empty();
        let _ =
            self.conn
                .change_property8(PropMode::REPLACE, t.requestor, t.property, t.target, chunk);
        t.offset = end;
        if done {
            transfers.swap_remove(i);
        }
    }

    // ----- 调用方线程 -----

    /// 向当前所有者要一种格式的数据。流界自己是所有者时直接从内存里取。
    fn read(&self, target: Atom, timeout: Duration) -> Option<Vec<u8>> {
        if let Some(offer) = lock(&self.owned).clone() {
            return offer
                .iter()
                .find(|(t, _)| *t == target)
                .map(|(_, d)| d.to_vec());
        }
        let _one_at_a_time = lock(&self.read_lock);
        let (tx, rx) = mpsc::channel();
        *lock(&self.pending) = Some(Pending {
            target,
            tx,
            incr: None,
        });
        let sent = self
            .conn
            .delete_property(self.win, self.atoms.FLOWACROSS_SELECTION)
            .is_ok()
            && self
                .conn
                .convert_selection(
                    self.win,
                    self.atoms.CLIPBOARD,
                    target,
                    self.atoms.FLOWACROSS_SELECTION,
                    x11rb::CURRENT_TIME,
                )
                .is_ok()
            && self.conn.flush().is_ok();
        let data = if sent {
            rx.recv_timeout(timeout).ok().flatten()
        } else {
            None
        };
        *lock(&self.pending) = None;
        data
    }

    /// 当前所有者提供的格式。
    fn targets(&self) -> Vec<Atom> {
        if let Some(offer) = lock(&self.owned).clone() {
            return offer.iter().map(|(t, _)| *t).collect();
        }
        let count = self.count.load(Ordering::SeqCst);
        if let Some((at, list)) = lock(&self.targets).as_ref()
            && *at == count
        {
            return list.clone();
        }
        let list: Vec<Atom> = self
            .read(self.atoms.TARGETS, READ_TIMEOUT)
            .map(|bytes| {
                bytes
                    .chunks_exact(4)
                    .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                    .collect()
            })
            .unwrap_or_default();
        *lock(&self.targets) = Some((count, list.clone()));
        list
    }

    /// 成为剪贴板的所有者，提供这些格式。
    fn offer(&self, items: Vec<(Atom, Vec<u8>)>) -> Result<(), PlatformError> {
        let offer: Offer = Arc::new(items.into_iter().map(|(t, d)| (t, Arc::new(d))).collect());
        *lock(&self.owned) = Some(offer);
        self.count.fetch_add(1, Ordering::SeqCst);
        let fail = |e: &dyn std::fmt::Display| os_err("写入剪贴板失败", e);
        self.conn
            .set_selection_owner(self.win, self.atoms.CLIPBOARD, x11rb::CURRENT_TIME)
            .map_err(|e| fail(&e))?;
        let owner = self
            .conn
            .get_selection_owner(self.atoms.CLIPBOARD)
            .map_err(|e| fail(&e))?
            .reply()
            .map_err(|e| fail(&e))?
            .owner;
        if owner != self.win {
            *lock(&self.owned) = None;
            return Err(PlatformError::Os(
                "写入剪贴板失败：别的程序正占着剪贴板".into(),
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 文件清单的格式
// ---------------------------------------------------------------------------

/// 路径转成 `file://` 地址：除了字母、数字和 `-._~/` 以外的字节都写成 `%XX`。
pub(crate) fn path_to_uri(path: &Path) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;
    if !path.is_absolute() {
        return None;
    }
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    Some(out)
}

/// `file://` 地址转回路径。只接受本机的文件（主机名为空或 `localhost`）。
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let rest = uri.trim().strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    // 路径里不会有 NUL。
    if out.contains(&0) {
        return None;
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

/// 解析 `text/uri-list`（每行一个地址，`#` 开头是注释）或 `x-special/gnome-copied-files`
/// （第一行是 `copy` / `cut`，后面每行一个地址）。不是 `file://` 的行跳过。
pub(crate) fn parse_file_list(data: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(data)
        .split(['\n', '\r', '\0'])
        .filter(|l| !l.starts_with('#'))
        .filter_map(uri_to_path)
        .collect()
}

pub(crate) fn decode_text(mut bytes: Vec<u8>) -> Option<String> {
    while bytes.last() == Some(&0) {
        bytes.pop();
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (!text.is_empty()).then_some(text)
}

// ---------------------------------------------------------------------------
// 对外接口
// ---------------------------------------------------------------------------

pub struct X11Clipboard {
    service: Arc<Service>,
}

impl X11Clipboard {
    pub fn new() -> Result<Self, PlatformError> {
        Ok(Self {
            service: service()?,
        })
    }
}

impl ClipboardAccess for X11Clipboard {
    fn get_text(&mut self) -> Result<Option<String>, PlatformError> {
        let s = &self.service;
        let targets = s.targets();
        let Some(target) = [s.atoms.UTF8_STRING, s.atoms.TEXT_PLAIN_UTF8]
            .into_iter()
            .find(|t| targets.contains(t))
        else {
            return Ok(None);
        };
        Ok(s.read(target, READ_TIMEOUT).and_then(decode_text))
    }

    fn set_text(&mut self, text: &str) -> Result<(), PlatformError> {
        let a = &self.service.atoms;
        let bytes = text.as_bytes().to_vec();
        self.service.offer(vec![
            (a.UTF8_STRING, bytes.clone()),
            (a.TEXT_PLAIN_UTF8, bytes.clone()),
            (a.TEXT_PLAIN, bytes),
        ])
    }

    fn get_files(&mut self) -> Result<Option<Vec<PathBuf>>, PlatformError> {
        let s = &self.service;
        let targets = s.targets();
        let Some(target) = [s.atoms.TEXT_URI_LIST, s.atoms.GNOME_COPIED_FILES]
            .into_iter()
            .find(|t| targets.contains(t))
        else {
            return Ok(None);
        };
        let files = s
            .read(target, READ_TIMEOUT)
            .map(|d| parse_file_list(&d))
            .unwrap_or_default();
        Ok((!files.is_empty()).then_some(files))
    }

    fn set_files(&mut self, paths: &[PathBuf]) -> Result<(), PlatformError> {
        let uris: Vec<String> = paths.iter().filter_map(|p| path_to_uri(p)).collect();
        if uris.is_empty() {
            return Err(PlatformError::Os(
                "写入剪贴板失败：没有可用的文件路径".into(),
            ));
        }
        let a = &self.service.atoms;
        // text/uri-list 每行以 CRLF 结尾；GNOME 的格式第一行是操作，行间用 LF，末尾不换行。
        let uri_list: String = uris.iter().map(|u| format!("{u}\r\n")).collect();
        let gnome = format!("copy\n{}", uris.join("\n"));
        self.service.offer(vec![
            (a.TEXT_URI_LIST, uri_list.into_bytes()),
            (a.GNOME_COPIED_FILES, gnome.into_bytes()),
        ])
    }

    fn get_image(&mut self) -> Result<Option<Vec<u8>>, PlatformError> {
        let s = &self.service;
        if !s.targets().contains(&s.atoms.IMAGE_PNG) {
            return Ok(None);
        }
        Ok(s.read(s.atoms.IMAGE_PNG, IMAGE_TIMEOUT)
            .filter(|d| d.starts_with(b"\x89PNG\r\n\x1a\n")))
    }

    fn set_image(&mut self, image: &ClipboardImage) -> Result<(), PlatformError> {
        let png = image.png().to_vec();
        self.service
            .offer(vec![(self.service.atoms.IMAGE_PNG, png)])
    }

    fn change_count(&self) -> u64 {
        self.service.count.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_round_trip_through_file_uris() {
        for p in [
            "/home/me/报告 (1).pdf",
            "/tmp/a b/c#d?e%f.txt",
            "/plain/path.tar.gz",
            "/",
        ] {
            let uri = path_to_uri(Path::new(p)).unwrap();
            assert!(uri.is_ascii(), "{uri}");
            assert_eq!(uri_to_path(&uri), Some(PathBuf::from(p)), "{uri}");
        }
        assert_eq!(
            path_to_uri(Path::new("/tmp/a b.txt")).unwrap(),
            "file:///tmp/a%20b.txt"
        );
        assert_eq!(path_to_uri(Path::new("relative/x")), None);
    }

    #[test]
    fn rejects_uris_that_are_not_local_files() {
        assert_eq!(uri_to_path("https://example.com/a"), None);
        assert_eq!(uri_to_path("file://otherhost/share/a"), None);
        assert_eq!(uri_to_path("file:///tmp/bad%zz"), None);
        assert_eq!(uri_to_path("file:///tmp/cut%2"), None);
        assert_eq!(uri_to_path("file:///tmp/nul%00x"), None);
        assert_eq!(
            uri_to_path("file://localhost/tmp/a"),
            Some(PathBuf::from("/tmp/a"))
        );
    }

    #[test]
    fn parses_both_file_list_formats() {
        let uri_list = b"# comment\r\nfile:///tmp/a%20b.txt\r\nfile:///home/me/c\r\n";
        assert_eq!(
            parse_file_list(uri_list),
            vec![PathBuf::from("/tmp/a b.txt"), PathBuf::from("/home/me/c")]
        );
        let gnome = b"copy\nfile:///tmp/x\nfile:///tmp/y";
        assert_eq!(
            parse_file_list(gnome),
            vec![PathBuf::from("/tmp/x"), PathBuf::from("/tmp/y")]
        );
        assert!(parse_file_list(b"hello world").is_empty());
    }

    #[test]
    fn text_drops_trailing_nul_and_empty_is_none() {
        assert_eq!(decode_text(b"hi\0".to_vec()), Some("hi".into()));
        assert_eq!(decode_text("中文".as_bytes().to_vec()), Some("中文".into()));
        assert_eq!(decode_text(Vec::new()), None);
        assert_eq!(decode_text(vec![0]), None);
    }

    // 以下测试需要一个 X 服务和 xclip（充当"别的程序"），没有时跳过；和其他 X 测试一样要串行跑。

    use std::io::Write;
    use std::process::{Command, Stdio};

    fn xclip_out(target: &str) -> Vec<u8> {
        Command::new("xclip")
            .args(["-selection", "clipboard", "-o", "-t", target])
            .output()
            .expect("运行 xclip")
            .stdout
    }

    fn xclip_in(target: &str, data: &[u8]) {
        let mut child = Command::new("xclip")
            .args(["-selection", "clipboard", "-i", "-t", target])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("运行 xclip");
        child.stdin.take().unwrap().write_all(data).unwrap();
        child.wait().unwrap();
    }

    /// 等变化计数超过 `before`（所有者变化的通知是异步到的）。
    fn wait_for_change(cb: &X11Clipboard, before: u64) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while cb.change_count() == before {
            assert!(Instant::now() < deadline, "变化计数没有更新");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// 一张内容不可压缩的 PNG，用来撑大数据量。
    fn noisy_png(side: u32) -> Vec<u8> {
        let mut seed = 0x2545_F491u32;
        let pixels: Vec<u8> = (0..side * side * 4)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect();
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, side, side);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()
            .unwrap()
            .write_image_data(&pixels)
            .unwrap();
        out
    }

    #[test]
    #[ignore = "需要 X 服务和 xclip"]
    fn text_goes_both_ways_and_counts_changes() {
        let mut cb = X11Clipboard::new().unwrap();
        let before = cb.change_count();
        cb.set_text("流界 FlowAcross ✓").unwrap();
        assert!(cb.change_count() > before, "自己写入后计数立即变化");
        assert_eq!(cb.get_text().unwrap().as_deref(), Some("流界 FlowAcross ✓"));
        assert_eq!(xclip_out("UTF8_STRING"), "流界 FlowAcross ✓".as_bytes());

        let before = cb.change_count();
        xclip_in("UTF8_STRING", "来自别的程序".as_bytes());
        wait_for_change(&cb, before);
        assert_eq!(cb.get_text().unwrap().as_deref(), Some("来自别的程序"));
        assert_eq!(cb.get_files().unwrap(), None);
        assert_eq!(cb.get_image().unwrap(), None);
    }

    #[test]
    #[ignore = "需要 X 服务和 xclip"]
    fn files_go_both_ways() {
        let mut cb = X11Clipboard::new().unwrap();
        let paths = vec![
            PathBuf::from("/tmp/报告 (1).pdf"),
            PathBuf::from("/tmp/b.txt"),
        ];
        cb.set_files(&paths).unwrap();
        assert_eq!(cb.get_files().unwrap(), Some(paths.clone()));
        assert_eq!(
            String::from_utf8(xclip_out("text/uri-list")).unwrap(),
            "file:///tmp/%E6%8A%A5%E5%91%8A%20%281%29.pdf\r\nfile:///tmp/b.txt\r\n"
        );
        assert_eq!(
            String::from_utf8(xclip_out("x-special/gnome-copied-files")).unwrap(),
            "copy\nfile:///tmp/%E6%8A%A5%E5%91%8A%20%281%29.pdf\nfile:///tmp/b.txt"
        );

        let before = cb.change_count();
        xclip_in("text/uri-list", b"file:///tmp/from%20other.txt\r\n");
        wait_for_change(&cb, before);
        assert_eq!(
            cb.get_files().unwrap(),
            Some(vec![PathBuf::from("/tmp/from other.txt")])
        );
    }

    #[test]
    #[ignore = "需要 X 服务和 xclip"]
    fn large_images_go_both_ways_in_chunks() {
        let mut cb = X11Clipboard::new().unwrap();
        // 约 4 MB，超过一次属性写入的上限，两个方向都要走 INCR 分块。
        let png = noisy_png(1024);
        assert!(png.len() > 2 * MAX_CHUNK, "测试图片不够大：{}", png.len());

        cb.set_image(&ClipboardImage::from_png(png.clone()).unwrap())
            .unwrap();
        assert!(
            xclip_out("image/png") == png,
            "别的程序读到的图片和写入的不一致"
        );

        let before = cb.change_count();
        xclip_in("image/png", &png);
        wait_for_change(&cb, before);
        let got = cb.get_image().unwrap().expect("读到图片");
        assert!(got == png, "读到的图片和别的程序写入的不一致");
        assert_eq!(cb.get_text().unwrap(), None);
    }
}
