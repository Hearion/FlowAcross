//! Linux（Wayland）剪贴板：`zwlr_data_control`（Sway、Hyprland、KDE 都支持）。
//!
//! Wayland 普通程序只有在自己的窗口有焦点时才能读写剪贴板；data-control 是给剪贴板管理器用的协议，
//! 在后台也能读写，正合流界的需要。
//!
//! 和 X11 一样，剪贴板不存数据：谁复制谁就提供一个"数据源"，别人粘贴时合成器让它把数据写进一个管道。
//! 所以由进程内唯一的后台服务（[`Service`]，自己的连接和事件线程）负责应答，[`WlClipboard`] 只是句柄。
//!
//! - **读**：让当前的数据源把某个格式写进套接字的一端，从另一端读到结束，最多等一小段时间。
//! - **写**：建一个数据源，声明各个格式，设为当前选区；收到请求时在单独的线程里把数据写进对方给的管道。
//! - **变化计数**：合成器每次换选区都会通知。自己写入时直接加一，随后合成器回报的那一次不再重复计数。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use wayland_client::backend::ObjectId;
use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

use crate::linux::os_err;
use crate::linux_clipboard::{decode_text, parse_file_list, path_to_uri};
use crate::linux_wl;
use crate::{ClipboardAccess, ClipboardImage, PlatformError};

/// 读文本、文件清单最多等这么久。引擎是拿着状态锁来读的，对方不应答时不能卡太久。
const READ_TIMEOUT: Duration = Duration::from_millis(500);
/// 读图片在后台线程做，数据大，多等一会儿。
const IMAGE_TIMEOUT: Duration = Duration::from_secs(5);

const TEXT_TYPES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];
const URI_LIST: &str = "text/uri-list";
const GNOME_FILES: &str = "x-special/gnome-copied-files";
const PNG: &str = "image/png";

/// 一次写入剪贴板的内容：各个格式及其数据。跟着数据源走，收到请求时直接从这里取。
type Offer = Arc<Vec<(String, Arc<Vec<u8>>)>>;

#[derive(Default)]
struct Shared {
    /// 当前选区（别的程序复制的，或合成器回报的我们自己的）和它提供的格式。
    current: Option<(ZwlrDataControlOfferV1, Vec<String>)>,
    /// 流界是当前选区的提供者时，这里是它提供的内容和数据源。
    owned: Option<(Offer, ZwlrDataControlSourceV1)>,
    /// 自己设了选区、合成器还没回报的次数。回报的那次不算"剪贴板变了"。
    own_pending: u32,
}

struct Service {
    conn: Connection,
    qh: QueueHandle<State>,
    manager: ZwlrDataControlManagerV1,
    device: ZwlrDataControlDeviceV1,
    count: AtomicU64,
    shared: Mutex<Shared>,
}

/// 事件线程上的状态。
struct State {
    service: Option<Arc<Service>>,
    /// 合成器刚介绍、还没说明用途的选区对象，以及陆续报上来的格式。
    offers: HashMap<ObjectId, (ZwlrDataControlOfferV1, Vec<String>)>,
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
        let fail = |e: &dyn std::fmt::Display| os_err("初始化剪贴板失败", e);
        let conn = linux_wl::connect()?;
        let (globals, mut queue): (GlobalList, EventQueue<State>) =
            registry_queue_init(&conn).map_err(|e| fail(&e))?;
        let qh = queue.handle();
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ()).map_err(|e| fail(&e))?;
        let manager: ZwlrDataControlManagerV1 = globals.bind(&qh, 1..=2, ()).map_err(|_| {
            PlatformError::Os(
                "合成器不支持后台读写剪贴板的协议（wlr-data-control），无法同步剪贴板".into(),
            )
        })?;
        let device = manager.get_data_device(&seat, &qh, ());
        let service = Arc::new(Service {
            conn,
            qh,
            manager,
            device,
            count: AtomicU64::new(1),
            shared: Mutex::new(Shared::default()),
        });
        let mut state = State {
            service: Some(service.clone()),
            offers: HashMap::new(),
        };
        // 合成器会马上报告当前的选区。
        queue.roundtrip(&mut state).map_err(|e| fail(&e))?;
        std::thread::Builder::new()
            .name("flowacross-wl-clipboard".into())
            .spawn(move || while queue.blocking_dispatch(&mut state).is_ok() {})
            .map_err(|e| os_err("无法创建剪贴板线程", e))?;
        Ok(service)
    }

    fn shared(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 当前选区提供的格式。
    fn types(&self) -> Vec<String> {
        let shared = self.shared();
        match (&shared.owned, &shared.current) {
            (Some((offer, _)), _) => offer.iter().map(|(t, _)| t.clone()).collect(),
            (None, Some((_, types))) => types.clone(),
            (None, None) => Vec::new(),
        }
    }

    /// 读当前选区的一种格式。流界自己是提供者时直接从内存里取。
    fn read(&self, mime: &str, timeout: Duration) -> Option<Vec<u8>> {
        let mut ours = {
            let shared = self.shared();
            if let Some((offer, _)) = &shared.owned {
                return offer
                    .iter()
                    .find(|(t, _)| t == mime)
                    .map(|(_, d)| d.to_vec());
            }
            let (offer, types) = shared.current.as_ref()?;
            if !types.iter().any(|t| t == mime) {
                return None;
            }
            // 拿着锁发请求：事件线程换选区、销毁旧对象前也要拿这把锁，不会撞上。
            let (ours, theirs) = UnixStream::pair().ok()?;
            offer.receive(mime.to_owned(), theirs.as_fd());
            self.conn.flush().ok()?;
            ours
            // `theirs` 在这里关掉：合成器已经把它转交给数据源，对方写完关闭后我们就能读到结束。
        };
        let deadline = Instant::now() + timeout;
        let mut data = Vec::new();
        let mut chunk = [0u8; 64 * 1024];
        loop {
            let left = deadline.checked_duration_since(Instant::now())?;
            ours.set_read_timeout(Some(left.max(Duration::from_millis(1))))
                .ok()?;
            match ours.read(&mut chunk) {
                Ok(0) => return Some(data),
                Ok(n) => data.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                // 超时或出错：对方没有按时给完，当作没有。
                Err(_) => return None,
            }
        }
    }

    /// 成为剪贴板选区的提供者。
    fn offer(&self, items: Vec<(&str, Vec<u8>)>) -> Result<(), PlatformError> {
        let mut shared: Vec<(String, Arc<Vec<u8>>)> = Vec::new();
        for (mime, data) in items {
            let data = Arc::new(data);
            shared.push((mime.to_owned(), data));
        }
        let offer: Offer = Arc::new(shared);
        let source = self.manager.create_data_source(&self.qh, offer.clone());
        for (mime, _) in offer.iter() {
            source.offer(mime.clone());
        }
        {
            let mut shared = self.shared();
            self.device.set_selection(Some(&source));
            shared.own_pending += 1;
            // 旧的数据源会收到"已取消"，在那里销毁。
            shared.owned = Some((offer, source));
            self.count.fetch_add(1, Ordering::SeqCst);
        }
        self.conn.flush().map_err(|e| os_err("写入剪贴板失败", e))
    }
}

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

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ZwlrDataControlManagerV1);

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                state.offers.insert(id.id(), (id, Vec::new()));
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                let new = id.and_then(|o| state.offers.remove(&o.id()));
                let Some(service) = &state.service else {
                    return;
                };
                let mut shared = service.shared();
                if shared.own_pending > 0 {
                    // 合成器回报的是我们自己刚设的选区。
                    shared.own_pending -= 1;
                } else {
                    shared.owned = None;
                    service.count.fetch_add(1, Ordering::SeqCst);
                }
                if let Some((old, _)) = std::mem::replace(&mut shared.current, new) {
                    old.destroy();
                }
            }
            zwlr_data_control_device_v1::Event::PrimarySelection { id } => {
                // 鼠标选中即复制的那个选区不同步，直接丢掉。
                if let Some((offer, _)) = id.and_then(|o| state.offers.remove(&o.id())) {
                    offer.destroy();
                }
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event
            && let Some((_, types)) = state.offers.get_mut(&offer.id())
        {
            types.push(mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, Offer> for State {
    fn event(
        state: &mut Self,
        source: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        offer: &Offer,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                let Some((_, data)) = offer.iter().find(|(t, _)| *t == mime_type) else {
                    return;
                };
                let data = data.clone();
                // 对方读得慢（或根本不读）时写入会卡住，不能占着事件线程。
                let _ = std::thread::Builder::new()
                    .name("flowacross-wl-paste".into())
                    .spawn(move || {
                        let mut file = std::fs::File::from(fd);
                        let _ = file.write_all(&data);
                    });
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                // 别的程序（或我们自己的下一次写入）取代了这个数据源。
                if let Some(service) = &state.service {
                    let mut shared = service.shared();
                    if shared.owned.as_ref().is_some_and(|(_, s)| s == source) {
                        shared.owned = None;
                    }
                }
                source.destroy();
            }
            _ => {}
        }
    }
}

/// 能按格式读写剪贴板选区的后端：这里的 data-control 服务，或 portal 的剪贴板（`linux_portal.rs`）。
pub(crate) trait Selection: Send + Sync {
    /// 当前选区提供的格式。
    fn types(&self) -> Vec<String>;
    /// 读当前选区的一种格式；对方没按时给完时返回 `None`。
    fn read(&self, mime: &str, timeout: Duration) -> Option<Vec<u8>>;
    /// 成为选区的提供者。
    fn offer(&self, items: Vec<(&str, Vec<u8>)>) -> Result<(), PlatformError>;
    fn change_count(&self) -> u64;
}

impl Selection for Service {
    fn types(&self) -> Vec<String> {
        Service::types(self)
    }
    fn read(&self, mime: &str, timeout: Duration) -> Option<Vec<u8>> {
        Service::read(self, mime, timeout)
    }
    fn offer(&self, items: Vec<(&str, Vec<u8>)>) -> Result<(), PlatformError> {
        Service::offer(self, items)
    }
    fn change_count(&self) -> u64 {
        self.count.load(Ordering::SeqCst)
    }
}

/// Wayland 上的剪贴板句柄：文本、文件、图片和各种格式名之间的对应，后端可换。
pub struct WlClipboard {
    service: Arc<dyn Selection>,
}

impl WlClipboard {
    pub fn new() -> Result<Self, PlatformError> {
        Ok(Self {
            service: service()?,
        })
    }

    pub(crate) fn with_backend(service: Arc<dyn Selection>) -> Self {
        Self { service }
    }
}

/// 合成器的协议列表里有没有 data-control（KDE 也有，GNOME 没有）。
pub(crate) fn available() -> bool {
    service().is_ok()
}

impl ClipboardAccess for WlClipboard {
    fn get_text(&mut self) -> Result<Option<String>, PlatformError> {
        let types = self.service.types();
        let Some(mime) = TEXT_TYPES.iter().find(|t| types.iter().any(|x| x == *t)) else {
            return Ok(None);
        };
        Ok(self.service.read(mime, READ_TIMEOUT).and_then(decode_text))
    }

    fn set_text(&mut self, text: &str) -> Result<(), PlatformError> {
        let bytes = text.as_bytes().to_vec();
        self.service
            .offer(TEXT_TYPES.iter().map(|t| (*t, bytes.clone())).collect())
    }

    fn get_files(&mut self) -> Result<Option<Vec<PathBuf>>, PlatformError> {
        let types = self.service.types();
        let Some(mime) = [URI_LIST, GNOME_FILES]
            .into_iter()
            .find(|t| types.iter().any(|x| x == t))
        else {
            return Ok(None);
        };
        let files = self
            .service
            .read(mime, READ_TIMEOUT)
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
        // text/uri-list 每行以 CRLF 结尾；GNOME 的格式第一行是操作，行间用 LF，末尾不换行。
        let uri_list: String = uris.iter().map(|u| format!("{u}\r\n")).collect();
        let gnome = format!("copy\n{}", uris.join("\n"));
        self.service.offer(vec![
            (URI_LIST, uri_list.into_bytes()),
            (GNOME_FILES, gnome.into_bytes()),
        ])
    }

    fn get_image(&mut self) -> Result<Option<Vec<u8>>, PlatformError> {
        if !self.service.types().iter().any(|t| t == PNG) {
            return Ok(None);
        }
        Ok(self
            .service
            .read(PNG, IMAGE_TIMEOUT)
            .filter(|d| d.starts_with(b"\x89PNG\r\n\x1a\n")))
    }

    fn set_image(&mut self, image: &ClipboardImage) -> Result<(), PlatformError> {
        self.service.offer(vec![(PNG, image.png().to_vec())])
    }

    fn change_count(&self) -> u64 {
        self.service.change_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    // 需要一个支持 data-control 的合成器（无头的 sway）和 wl-clipboard（充当"别的程序"）；串行跑。

    fn paste(mime: &str) -> Vec<u8> {
        Command::new("wl-paste")
            .args(["--no-newline", "--type", mime])
            .output()
            .expect("运行 wl-paste")
            .stdout
    }

    fn copy(mime: &str, data: &[u8]) {
        let mut child = Command::new("wl-copy")
            .args(["--type", mime])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("运行 wl-copy");
        child.stdin.take().unwrap().write_all(data).unwrap();
        child.wait().unwrap();
    }

    fn wait_for_change(cb: &WlClipboard, before: u64) {
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
    #[ignore = "需要 wlroots 系合成器和 wl-clipboard"]
    fn text_goes_both_ways_and_counts_changes() {
        let mut cb = WlClipboard::new().unwrap();
        let before = cb.change_count();
        cb.set_text("流界 FlowAcross ✓").unwrap();
        assert_eq!(cb.change_count(), before + 1, "自己写入后计数立即加一");
        assert_eq!(cb.get_text().unwrap().as_deref(), Some("流界 FlowAcross ✓"));
        assert_eq!(
            paste("text/plain;charset=utf-8"),
            "流界 FlowAcross ✓".as_bytes()
        );
        // 合成器回报我们自己的选区时不再重复计数。
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(cb.change_count(), before + 1);

        let before = cb.change_count();
        copy("text/plain;charset=utf-8", "来自别的程序".as_bytes());
        wait_for_change(&cb, before);
        assert_eq!(cb.get_text().unwrap().as_deref(), Some("来自别的程序"));
        assert_eq!(cb.get_files().unwrap(), None);
        assert_eq!(cb.get_image().unwrap(), None);
    }

    #[test]
    #[ignore = "需要 wlroots 系合成器和 wl-clipboard"]
    fn files_go_both_ways() {
        let mut cb = WlClipboard::new().unwrap();
        let paths = vec![
            PathBuf::from("/tmp/报告 (1).pdf"),
            PathBuf::from("/tmp/b.txt"),
        ];
        cb.set_files(&paths).unwrap();
        assert_eq!(cb.get_files().unwrap(), Some(paths.clone()));
        assert_eq!(
            String::from_utf8(paste("text/uri-list")).unwrap(),
            "file:///tmp/%E6%8A%A5%E5%91%8A%20%281%29.pdf\r\nfile:///tmp/b.txt\r\n"
        );
        assert_eq!(
            String::from_utf8(paste("x-special/gnome-copied-files")).unwrap(),
            "copy\nfile:///tmp/%E6%8A%A5%E5%91%8A%20%281%29.pdf\nfile:///tmp/b.txt"
        );

        let before = cb.change_count();
        copy("text/uri-list", b"file:///tmp/from%20other.txt\r\n");
        wait_for_change(&cb, before);
        assert_eq!(
            cb.get_files().unwrap(),
            Some(vec![PathBuf::from("/tmp/from other.txt")])
        );
    }

    #[test]
    #[ignore = "需要 wlroots 系合成器和 wl-clipboard"]
    fn large_images_go_both_ways() {
        let mut cb = WlClipboard::new().unwrap();
        // 约 4 MB，远大于管道缓冲区：两个方向都要边写边读才传得完。
        let png = noisy_png(1024);
        assert!(png.len() > 2 << 20, "测试图片不够大：{}", png.len());

        cb.set_image(&ClipboardImage::from_png(png.clone()).unwrap())
            .unwrap();
        assert!(
            paste("image/png") == png,
            "别的程序读到的图片和写入的不一致"
        );

        let before = cb.change_count();
        copy("image/png", &png);
        wait_for_change(&cb, before);
        let got = cb.get_image().unwrap().expect("读到图片");
        assert!(got == png, "读到的图片和别的程序写入的不一致");
        assert_eq!(cb.get_text().unwrap(), None);
    }
}
