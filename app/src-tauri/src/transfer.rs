//! 跨设备复制文件的执行部分：遍历本机文件、在传输流上发送、接收后落盘，以及界面上的传输列表。
//!
//! 什么时候发清单、什么时候去取由引擎决定，判定和校验逻辑在 `flow_core::files`。
//! 接收时先写进接收文件夹里的临时目录（`.flowacross-*`），全部收完才移出来，
//! 中途失败或取消时整个临时目录删掉，不会留下半个文件。

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use flow_core::files::{
    CHUNK_SIZE, FileOffer, Incoming, MAX_DEPTH, MAX_ENTRIES, MAX_LISTED, ReceiveStep, is_junk,
    sanitize_component, unique_name,
};
use flow_core::layout::DeviceId;
use flow_core::protocol::Message;
use flow_net::{LinkReceiver, LinkSender};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

/// 用户取消时的错误文本（界面据此显示「已取消」而不是失败）。
pub const CANCELED: &str = "已取消";

/// 临时目录的前缀。启动时清理上次异常退出留下的。
const STAGING_PREFIX: &str = ".flowacross-";

/// 界面上保留的已结束传输条数。
const KEEP_FINISHED: usize = 3;

/// 要发送的一项。
#[derive(Debug, Clone)]
pub struct Entry {
    /// `/` 分隔的相对路径，第一段是用户复制的那一项。
    pub rel: String,
    pub abs: PathBuf,
    pub size: u64,
    pub dir: bool,
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// 读写文件失败时给用户看的原因。
pub fn io_text(path: &Path, e: &std::io::Error) -> String {
    let name = display_name(path);
    match e.kind() {
        ErrorKind::NotFound => format!("找不到「{name}」，可能已被移动或删除，请重新复制。"),
        ErrorKind::PermissionDenied => format!("没有权限读写「{name}」。"),
        ErrorKind::StorageFull => "磁盘空间不足，请清理后重试。".into(),
        _ => format!("读写「{name}」失败：{e}"),
    }
}

/// 列出要发送的全部目录和文件（目录排在它的内容前面）。复制的项本身是符号链接时跟随；
/// 目录里的符号链接跳过，避免循环。`.DS_Store` 这类系统文件不传。
pub fn walk(roots: &[PathBuf]) -> Result<Vec<Entry>, String> {
    let mut out = Vec::new();
    for root in roots {
        let meta = std::fs::metadata(root).map_err(|e| io_text(root, &e))?;
        let rel = sanitize_component(&display_name(root));
        walk_into(root, rel, 1, &meta, &mut out)?;
    }
    Ok(out)
}

fn walk_into(
    path: &Path,
    rel: String,
    depth: usize,
    meta: &std::fs::Metadata,
    out: &mut Vec<Entry>,
) -> Result<(), String> {
    if out.len() >= MAX_ENTRIES {
        return Err(format!(
            "文件太多（超过 {MAX_ENTRIES} 个），请分批复制或先压缩。"
        ));
    }
    if depth > MAX_DEPTH {
        return Err("文件夹层级太深，请先压缩后再复制。".into());
    }
    if meta.is_dir() {
        out.push(Entry {
            rel: rel.clone(),
            abs: path.to_owned(),
            size: 0,
            dir: true,
        });
        let mut children: Vec<_> = std::fs::read_dir(path)
            .map_err(|e| io_text(path, &e))?
            .filter_map(Result::ok)
            .collect();
        children.sort_by_key(|c| c.file_name());
        for child in children {
            let name = child.file_name().to_string_lossy().into_owned();
            if is_junk(&name) {
                continue;
            }
            // DirEntry::metadata 不跟随符号链接。
            let Ok(meta) = child.metadata() else {
                continue;
            };
            if meta.is_symlink() {
                continue;
            }
            let rel = format!("{rel}/{}", sanitize_component(&name));
            walk_into(&child.path(), rel, depth + 1, &meta, out)?;
        }
    } else if meta.is_file() {
        out.push(Entry {
            rel,
            abs: path.to_owned(),
            size: meta.len(),
            dir: false,
        });
    }
    Ok(())
}

/// 根据遍历结果生成清单（编号由引擎分配）。
pub fn summarize(roots: &[PathBuf], entries: &[Entry]) -> FileOffer {
    FileOffer {
        id: 0,
        names: roots
            .iter()
            .take(MAX_LISTED)
            .map(|p| display_name(p))
            .collect(),
        items: roots.len() as u32,
        files: entries.iter().filter(|e| !e.dir).count() as u64,
        bytes: entries.iter().map(|e| e.size).sum(),
    }
}

/// 一次传输的进度和取消开关，传输任务和界面共享。
pub struct Ticket {
    done: AtomicU64,
    cancel: watch::Sender<bool>,
}

impl Ticket {
    fn new() -> Self {
        Self {
            done: AtomicU64::new(0),
            cancel: watch::channel(false).0,
        }
    }

    fn add(&self, n: usize) {
        self.done.fetch_add(n as u64, Ordering::Relaxed);
    }

    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }

    pub fn cancel(&self) {
        self.cancel.send_replace(true);
    }

    /// 等到被取消为止。
    async fn cancelled(&self) {
        let mut rx = self.cancel.subscribe();
        // Ticket 存活期间发送端不会被丢弃，wait_for 只会因为取消而返回。
        let _ = rx.wait_for(|c| *c).await;
    }
}

fn net_text<E: std::fmt::Display>(e: E) -> String {
    format!("传输中断：{e}")
}

/// 在传输流上发送 `roots`（已收到对方的 `FileRequest`）。返回前已发出 `FileEnd`，
/// 出错时由调用方决定发 `FileError` 还是直接中止。
pub async fn send(
    tx: &mut LinkSender,
    roots: Vec<PathBuf>,
    ticket: &Ticket,
    on_progress: impl Fn(),
) -> Result<(), String> {
    let entries = tokio::task::spawn_blocking(move || walk(&roots))
        .await
        .map_err(|e| e.to_string())??;
    let mut buf = vec![0u8; CHUNK_SIZE];
    for e in entries {
        let msg = Message::FileEntry {
            path: e.rel.clone(),
            size: e.size,
            dir: e.dir,
        };
        tokio::select! {
            r = tx.send(&msg) => r.map_err(|_| peer_gone())?,
            _ = ticket.cancelled() => return Err(CANCELED.into()),
        }
        if e.dir {
            continue;
        }
        let mut file = tokio::fs::File::open(&e.abs)
            .await
            .map_err(|err| io_text(&e.abs, &err))?;
        let mut left = e.size;
        while left > 0 {
            let want = left.min(CHUNK_SIZE as u64) as usize;
            let n = tokio::select! {
                r = file.read(&mut buf[..want]) => r.map_err(|err| io_text(&e.abs, &err))?,
                _ = ticket.cancelled() => return Err(CANCELED.into()),
            };
            if n == 0 {
                return Err(format!(
                    "「{}」在传输过程中被修改了，请重新复制。",
                    display_name(&e.abs)
                ));
            }
            let chunk = Message::FileChunk(buf[..n].to_vec());
            tokio::select! {
                r = tx.send(&chunk) => r.map_err(|_| peer_gone())?,
                _ = ticket.cancelled() => return Err(CANCELED.into()),
            }
            left -= n as u64;
            ticket.add(n);
            on_progress();
        }
    }
    tx.send(&Message::FileEnd).await.map_err(|_| peer_gone())?;
    tx.finish();
    Ok(())
}

fn peer_gone() -> String {
    "对方取消了接收，或连接已断开。".into()
}

/// 请求编号为 `id` 的文件并收进 `root`。成功时返回移进 `root` 之后的顶层路径（重名时已改名）。
pub async fn receive(
    tx: &mut LinkSender,
    rx: &mut LinkReceiver,
    id: u64,
    root: &Path,
    ticket: &Ticket,
    on_progress: impl Fn(),
) -> Result<Vec<PathBuf>, String> {
    tokio::fs::create_dir_all(root)
        .await
        .map_err(|e| io_text(root, &e))?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let staging = root.join(format!("{STAGING_PREFIX}{id}-{nonce}"));
    let result = tokio::select! {
        r = receive_into(tx, rx, id, &staging, ticket, &on_progress) => r,
        _ = ticket.cancelled() => Err(CANCELED.into()),
    };
    let result = match result {
        Ok(tops) => finalize(&staging, root, &tops).await,
        Err(e) => {
            tx.reset();
            Err(e)
        }
    };
    let _ = tokio::fs::remove_dir_all(&staging).await;
    result
}

async fn receive_into(
    tx: &mut LinkSender,
    rx: &mut LinkReceiver,
    id: u64,
    staging: &Path,
    ticket: &Ticket,
    on_progress: &impl Fn(),
) -> Result<Vec<(String, bool)>, String> {
    tokio::fs::create_dir_all(staging)
        .await
        .map_err(|e| io_text(staging, &e))?;
    tx.send(&Message::FileRequest { id })
        .await
        .map_err(net_text)?;
    let mut incoming = Incoming::default();
    // 顶层条目（名称，是否目录），按收到的顺序。
    let mut tops: Vec<(String, bool)> = Vec::new();
    let mut file: Option<(tokio::fs::File, PathBuf)> = None;
    let join = |parts: &[String]| parts.iter().fold(staging.to_owned(), |p, c| p.join(c));
    loop {
        let msg = match rx.recv().await {
            Ok(Some(m)) => m,
            Ok(None) => return Err("传输中断：对方停止了发送。".into()),
            Err(e) => return Err(net_text(e)),
        };
        match incoming.step(&msg).map_err(|e| e.to_string())? {
            ReceiveStep::MakeDir(parts) => {
                note_top(&mut tops, &parts, true);
                let p = join(&parts);
                tokio::fs::create_dir_all(&p)
                    .await
                    .map_err(|e| io_text(&p, &e))?;
            }
            ReceiveStep::CreateFile { path, size } => {
                note_top(&mut tops, &path, false);
                let p = join(&path);
                if let Some(parent) = p.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| io_text(parent, &e))?;
                }
                let f = tokio::fs::File::create(&p)
                    .await
                    .map_err(|e| io_text(&p, &e))?;
                if size > 0 {
                    file = Some((f, p));
                }
            }
            ReceiveStep::Write { data, last } => {
                let Some((f, p)) = file.as_mut() else {
                    return Err("传输数据格式错误".into());
                };
                f.write_all(data).await.map_err(|e| io_text(p, &e))?;
                ticket.add(data.len());
                if last {
                    // tokio 的文件写入在后台线程完成，flush 之后才算真正写完。
                    f.flush().await.map_err(|e| io_text(p, &e))?;
                    file = None;
                }
                on_progress();
            }
            ReceiveStep::Finished => return Ok(tops),
        }
    }
}

fn note_top(tops: &mut Vec<(String, bool)>, parts: &[String], dir: bool) {
    let Some(first) = parts.first() else {
        return;
    };
    if !tops.iter().any(|(n, _)| n == first) {
        tops.push((first.clone(), dir || parts.len() > 1));
    }
}

/// 把临时目录里的顶层条目移进接收文件夹，重名时改名。
async fn finalize(
    staging: &Path,
    root: &Path,
    tops: &[(String, bool)],
) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for (name, dir) in tops {
        let target = unique_name(name, *dir, |n| root.join(n).exists());
        let to = root.join(&target);
        tokio::fs::rename(staging.join(name), &to)
            .await
            .map_err(|e| io_text(&to, &e))?;
        out.push(to);
    }
    Ok(out)
}

/// 删除上次异常退出留下的临时目录。
pub fn clean_staging(root: &Path) {
    let Ok(dir) = std::fs::read_dir(root) else {
        return;
    };
    for entry in dir.filter_map(Result::ok) {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(STAGING_PREFIX)
        {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

// ---------------------------------------------------------------------------
// 传输列表
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferStatus {
    Running,
    Done,
    Failed,
    Canceled,
}

struct Transfer {
    id: u64,
    peer: DeviceId,
    incoming: bool,
    label: String,
    total: u64,
    ticket: Arc<Ticket>,
    status: TransferStatus,
    error: Option<String>,
    pasteable: bool,
}

/// 推给界面的一条传输。
#[derive(Debug, Clone, Serialize)]
pub struct TransferView {
    pub id: u64,
    pub peer: DeviceId,
    pub peer_name: String,
    /// true：从对方接收；false：发给对方。
    pub incoming: bool,
    pub label: String,
    pub done: u64,
    pub total: u64,
    pub status: TransferStatus,
    pub error: Option<String>,
    /// 接收完成并已写入本机剪贴板，可以直接粘贴。
    pub pasteable: bool,
}

#[derive(Default)]
pub struct Transfers {
    list: Vec<Transfer>,
    next_id: u64,
}

impl Transfers {
    pub fn start(
        &mut self,
        peer: DeviceId,
        incoming: bool,
        label: String,
        total: u64,
    ) -> (u64, Arc<Ticket>) {
        self.next_id += 1;
        let ticket = Arc::new(Ticket::new());
        self.list.push(Transfer {
            id: self.next_id,
            peer,
            incoming,
            label,
            total,
            ticket: ticket.clone(),
            status: TransferStatus::Running,
            error: None,
            pasteable: false,
        });
        self.prune();
        (self.next_id, ticket)
    }

    /// 传输结束。`Ok(pasteable)`；`Err(原因)`，原因是 [`CANCELED`] 时记为已取消。
    pub fn finish(&mut self, id: u64, result: Result<bool, String>) {
        if let Some(t) = self.list.iter_mut().find(|t| t.id == id) {
            match result {
                Ok(pasteable) => {
                    t.status = TransferStatus::Done;
                    t.pasteable = pasteable;
                }
                Err(e) if e == CANCELED => t.status = TransferStatus::Canceled,
                Err(e) => {
                    t.status = TransferStatus::Failed;
                    t.error = Some(e);
                }
            }
        }
        self.prune();
    }

    pub fn cancel(&self, id: u64) {
        if let Some(t) = self.list.iter().find(|t| t.id == id) {
            t.ticket.cancel();
        }
    }

    /// 取消正在进行的接收（新的复制内容取代了旧的）。
    pub fn cancel_incoming(&self) {
        for t in &self.list {
            if t.incoming && t.status == TransferStatus::Running {
                t.ticket.cancel();
            }
        }
    }

    pub fn cancel_all(&self) {
        for t in &self.list {
            t.ticket.cancel();
        }
    }

    /// 从列表中移除已结束的传输。
    pub fn dismiss(&mut self, id: u64) {
        self.list
            .retain(|t| t.id != id || t.status == TransferStatus::Running);
    }

    /// 已结束的只保留最近几条。
    fn prune(&mut self) {
        let finished = self
            .list
            .iter()
            .filter(|t| t.status != TransferStatus::Running)
            .count();
        let mut drop = finished.saturating_sub(KEEP_FINISHED);
        self.list.retain(|t| {
            if drop > 0 && t.status != TransferStatus::Running {
                drop -= 1;
                false
            } else {
                true
            }
        });
    }

    pub fn views(&self, name_of: impl Fn(&DeviceId) -> String) -> Vec<TransferView> {
        self.list
            .iter()
            .rev()
            .map(|t| TransferView {
                id: t.id,
                peer: t.peer.clone(),
                peer_name: name_of(&t.peer),
                incoming: t.incoming,
                label: t.label.clone(),
                done: t.ticket.done(),
                // 文件在复制后变大时，实际传的会比清单里多。
                total: t.total.max(t.ticket.done()),
                status: t.status,
                error: t.error.clone(),
                pasteable: t.pasteable,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("flowacross-transfer-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn walk_lists_dirs_before_contents_and_skips_junk() {
        let root = temp("walk");
        let dir = root.join("照片");
        std::fs::create_dir_all(dir.join("2024")).unwrap();
        std::fs::write(dir.join("2024").join("a.jpg"), b"abc").unwrap();
        std::fs::write(dir.join(".DS_Store"), b"x").unwrap();
        let file = root.join("报告.pdf");
        std::fs::write(&file, b"hello").unwrap();

        let roots = vec![dir.clone(), file.clone()];
        let entries = walk(&roots).unwrap();
        let rels: Vec<_> = entries.iter().map(|e| (e.rel.as_str(), e.dir)).collect();
        assert_eq!(
            rels,
            [
                ("照片", true),
                ("照片/2024", true),
                ("照片/2024/a.jpg", false),
                ("报告.pdf", false)
            ]
        );
        let offer = summarize(&roots, &entries);
        assert_eq!(offer.names, ["照片", "报告.pdf"]);
        assert_eq!((offer.items, offer.files, offer.bytes), (2, 2, 8));

        assert!(walk(&[root.join("不存在")]).unwrap_err().contains("找不到"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn transfer_list_keeps_recent_finished() {
        let mut t = Transfers::default();
        let dev = DeviceId("a".into());
        let ids: Vec<u64> = (0..5)
            .map(|i| t.start(dev.clone(), true, format!("{i}"), 1).0)
            .collect();
        t.finish(ids[0], Ok(true));
        t.finish(ids[1], Err(CANCELED.into()));
        t.finish(ids[2], Err("磁盘空间不足".into()));
        t.finish(ids[3], Ok(false));
        let views = t.views(|_| "A".into());
        // 最多保留 3 条已结束的，加上 1 条进行中的；最新的在前。
        assert_eq!(views.len(), 4);
        assert_eq!(views[0].status, TransferStatus::Running);
        assert_eq!(views[2].status, TransferStatus::Failed);
        assert_eq!(views[3].status, TransferStatus::Canceled);
        t.dismiss(ids[4]);
        assert_eq!(t.views(|_| "A".into()).len(), 4, "进行中的不能移除");
        t.dismiss(ids[3]);
        assert_eq!(t.views(|_| "A".into()).len(), 3);
    }
}
