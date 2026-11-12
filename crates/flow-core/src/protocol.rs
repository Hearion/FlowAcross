//! 设备之间的消息与帧编解码。
//!
//! - 可靠流（控制流、传文件的流）：每帧 = 4 字节大端长度 + postcard 编码的 [`Message`]。
//! - 不可靠数据报（鼠标绝对坐标）：4 字节大端序号 + postcard 编码的 [`Message`]。
//!   数据报可能丢失或乱序，接收方按序号丢弃比已收到的更旧的位置。网络短暂停顿后
//!   直接跳到最新位置，而不是像可靠流那样把积压的旧位置逐个重放。

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::files::FileOffer;
use crate::geometry::Rect;
use crate::keys::{InputEvent, KeyCode, MouseButton};
use crate::layout::DeviceId;

/// 协议版本。不兼容的改动必须递增，握手时版本不同则拒绝连接并提示升级。
pub const PROTOCOL_VERSION: u16 = 1;

/// 单帧最大长度。
pub const MAX_FRAME_LEN: usize = 8 * 1024 * 1024;

/// 对方通过认证（双方互信、进入会话）之前的单帧上限。握手和配对消息都很小，
/// 不让未认证的连接让本机缓冲 8 MB。
pub const HANDSHAKE_FRAME_LEN: usize = 16 * 1024;

/// 剪贴板文本最大字节数，超过则不同步（界面提示"内容过大"）。
pub const MAX_CLIPBOARD_TEXT: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Os {
    MacOs,
    Windows,
    Linux,
    Other,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(target_os = "macos") {
            Os::MacOs
        } else if cfg!(target_os = "windows") {
            Os::Windows
        } else if cfg!(target_os = "linux") {
            Os::Linux
        } else {
            Os::Other
        }
    }
}

/// 设备上报的本机显示器信息。`rect` 是该设备系统自己的坐标（逻辑点），
/// 首次加入布局时按此保持相对位置，之后整体可被用户拖动。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub local_id: String,
    pub name: String,
    pub rect: Rect,
    /// 物理像素 / 逻辑点。macOS 上是渲染倍率（HiDPI 为 2），Windows 上是系统缩放（如 1.5）。
    pub scale: f64,
    pub primary: bool,
    /// 屏幕的物理分辨率（像素）：macOS 为面板原生分辨率，Windows 为当前分辨率。
    /// 只用于显示，坐标换算一律用 `rect` 和 `scale`。
    #[serde(default)]
    pub native: Option<(u32, u32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ByeReason {
    /// 应用退出或电脑关机。
    Shutdown,
    /// 用户暂停了共享。
    Paused,
    /// 用户移除了这台设备。
    Removed,
    /// 协议版本不兼容。
    VersionMismatch,
    /// 对方不在本机的可信设备列表中（未配对，或已被移除）。
    Unpaired,
    /// 用户拒绝或取消了配对。
    PairingRejected,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
    /// 连接建立后双方第一条消息（发起方先发）。
    /// `device` 必须等于 TLS 证书指纹，否则接收方断开连接。
    Hello {
        protocol: u16,
        device: DeviceId,
        name: String,
        os: Os,
        app_version: String,
        /// 发送方是否已把接收方列为可信设备。双方都为 true 才直接建立会话，否则进入配对。
        trusts_you: bool,
    },
    /// 发起方请求配对。之后按 [`crate::pairing`] 的流程交换承诺和随机数（`PairCommit` / `PairNonce`），
    /// 双方各自算出配对码并显示给用户核对。
    PairRequest,
    /// 本端用户已确认配对码一致。双方都确认后互相保存为可信设备。
    PairConfirm,
    /// 本端用户拒绝或取消配对。
    PairReject,
    /// 设备名称变更。
    Profile {
        name: String,
    },
    /// 本机显示器变化（插拔、改分辨率/缩放）时重新发送。
    Displays(Vec<DisplayInfo>),
    /// 源设备开始控制目标设备。坐标为目标显示器内的逻辑坐标。
    Enter {
        session: u32,
        display: String,
        x: f64,
        y: f64,
    },
    /// 源设备结束控制。目标设备收到后释放全部按键。
    Leave {
        session: u32,
    },
    /// 鼠标绝对位置（走数据报）。
    MouseMove {
        session: u32,
        display: String,
        x: f64,
        y: f64,
    },
    Button {
        button: MouseButton,
        down: bool,
    },
    /// 滚轮，单位：1/120 格（与 Windows WHEEL_DELTA 一致），支持触控板高精度滚动。
    Wheel {
        dx: i32,
        dy: i32,
    },
    Key {
        code: KeyCode,
        down: bool,
        repeat: bool,
    },
    /// 立即释放全部按键和鼠标按钮。
    ReleaseAll,
    /// 控制期间的心跳，目标侧超时未收到则自行释放按键、结束会话。
    Heartbeat {
        seq: u32,
    },
    Clipboard {
        origin: DeviceId,
        seq: u64,
        text: String,
    },
    Bye {
        reason: ByeReason,
    },
    // ---- 以下为 0.1.5 新增，只发给支持的版本（见 `files::peer_supports_files`）。
    // 新变体只能加在末尾：postcard 按序号编码变体，插在中间会让旧版本解析错。
    /// 本机剪贴板里是文件：只发清单，对方需要时再新开一条流来取（见 [`crate::files`]）。
    FileOffer(FileOffer),
    /// 传文件的流上的第一条消息：请求编号为 `id` 的那批文件。
    FileRequest {
        id: u64,
    },
    /// 接下来是一个目录或文件。`path` 是以 `/` 分隔的相对路径；文件内容在随后的 `FileChunk` 里。
    FileEntry {
        path: String,
        size: u64,
        dir: bool,
    },
    /// 当前文件的一段内容，最多 [`crate::files::CHUNK_SIZE`] 字节。
    FileChunk(#[serde(with = "raw_bytes")] Vec<u8>),
    /// 全部发完。
    FileEnd,
    /// 发送方出错，传输结束。`reason` 直接显示给用户。
    FileError {
        reason: String,
    },
    // ---- 以下为 0.1.7 新增，只发给版本不低于 [`SECURE_SINCE`] 的设备。
    /// 被控制方对 `Heartbeat` 的回执。控制方据此在断网后 1.5 秒内把键鼠还给本机，
    /// 不用等 QUIC 的 8 秒空闲超时。
    HeartbeatAck {
        seq: u32,
    },
    /// 配对第一步（发起方）：随机数的承诺 `H(发起方指纹, nonce)`，见 [`crate::pairing`]。
    PairCommit([u8; 32]),
    /// 配对第二步（响应方）和第三步（发起方公开承诺过的随机数）。
    PairNonce([u8; 32]),
    /// 本机现在是否接受对方的控制（暂停共享、暂停了这台设备、锁定时为 false）。
    /// 会话建立时和变化时发送；对方据此不再把键鼠切过来，正在控制的立即回到它自己那边。
    Availability {
        accepting: bool,
    },
}

/// 从这个版本起支持心跳回执（[`Message::HeartbeatAck`]）、带承诺的配对（[`Message::PairCommit`]）
/// 和 [`Message::Availability`]。不按"版本号相同"判断：开发版本和已发布的同号版本协议不同。
/// 两台电脑联调开发版本前，先把版本号改到不低于这个值。
pub const SECURE_SINCE: (u32, u32, u32) = (0, 1, 7);

/// 对方是否支持 0.1.7 新增的消息（见 [`SECURE_SINCE`]）。
pub fn peer_supports_secure(peer_version: &str) -> bool {
    version_at_least(peer_version, SECURE_SINCE)
}

/// 让 `Vec<u8>` 按字节串整体编码（postcard 默认逐个字节序列化，大块文件内容很慢）。
mod raw_bytes {
    use serde::{Deserializer, Serializer, de};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(v)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        struct Bytes;
        impl<'de> de::Visitor<'de> for Bytes {
            type Value = Vec<u8>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("字节串")
            }
            fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Vec<u8>, E> {
                Ok(v.to_vec())
            }
            fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
                Ok(v)
            }
            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
                let mut v = Vec::new();
                while let Some(b) = seq.next_element()? {
                    v.push(b);
                }
                Ok(v)
            }
        }
        d.deserialize_byte_buf(Bytes)
    }
}

impl From<InputEvent> for Message {
    fn from(ev: InputEvent) -> Self {
        match ev {
            InputEvent::Key { code, down, repeat } => Message::Key { code, down, repeat },
            InputEvent::Button { button, down } => Message::Button { button, down },
        }
    }
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("帧长度 {0} 超过上限")]
    TooLarge(usize),
    #[error("消息编解码失败: {0}")]
    Codec(#[from] postcard::Error),
}

/// 编码为不带长度前缀的载荷（用于数据报）。
pub fn encode_payload(msg: &Message) -> Result<Vec<u8>, ProtocolError> {
    let bytes = postcard::to_stdvec(msg)?;
    if bytes.len() > MAX_FRAME_LEN {
        return Err(ProtocolError::TooLarge(bytes.len()));
    }
    Ok(bytes)
}

pub fn decode_payload(bytes: &[u8]) -> Result<Message, ProtocolError> {
    Ok(postcard::from_bytes(bytes)?)
}

/// 编码为带 4 字节大端长度前缀的帧（用于可靠流）。
pub fn encode_frame(msg: &Message) -> Result<Vec<u8>, ProtocolError> {
    let payload = encode_payload(msg)?;
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// 从这个版本起，流界能接收数据报里的鼠标位置。对方版本更旧时只能走可靠流。
pub const DATAGRAM_MOVES_SINCE: (u32, u32, u32) = (0, 1, 3);

/// 编码数据报：4 字节大端序号 + 消息载荷。
pub fn encode_datagram(seq: u32, msg: &Message) -> Result<Vec<u8>, ProtocolError> {
    let payload = encode_payload(msg)?;
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// 解码数据报，返回序号和消息；不足 4 字节或载荷损坏时返回 `None`。
pub fn decode_datagram(bytes: &[u8]) -> Option<(u32, Message)> {
    let (head, payload) = bytes.split_first_chunk::<4>()?;
    let msg = decode_payload(payload).ok()?;
    Some((u32::from_be_bytes(*head), msg))
}

/// 序号 `a` 是否比 `b` 新（按回绕比较：相差不到 2^31 时，大的更新）。
pub fn seq_newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// 设备名称等对端提供的短文本最多保留的字符数。
pub const MAX_NAME_CHARS: usize = 64;

/// 清理对端提供的名称（mDNS、`Hello`、`Profile`）：去掉控制字符和首尾空白，最多保留
/// [`MAX_NAME_CHARS`] 个字符。对端可以发任意长、任意内容的字符串，原样写日志、配置和界面不安全。
pub fn clean_name(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(MAX_NAME_CHARS)
        .collect::<String>()
        .trim_end()
        .to_owned()
}

/// 版本号 `version`（如 "0.1.3"、"1.2.0-beta"）是否不低于 `min`；无法解析时视为不满足。
pub fn version_at_least(version: &str, min: (u32, u32, u32)) -> bool {
    let core = version.split(['-', '+']).next().unwrap_or("");
    let mut parts = core.split('.').map(|p| p.parse::<u32>());
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Ok(a)), Some(Ok(b)), Some(Ok(c))) => (a, b, c) >= min,
        _ => false,
    }
}

/// 从字节流中切出完整帧。网络数据可能被任意拆分或粘连，逐段 `push` 后循环调用 `next_message`。
#[derive(Debug)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    limit: usize,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self {
            buf: Vec::new(),
            limit: MAX_FRAME_LEN,
        }
    }
}

impl FrameDecoder {
    /// 单帧最大长度为 `limit`（不超过 [`MAX_FRAME_LEN`]）。
    pub fn with_limit(limit: usize) -> Self {
        Self {
            buf: Vec::new(),
            limit: limit.min(MAX_FRAME_LEN),
        }
    }

    /// 修改单帧上限：握手和配对期间用 [`HANDSHAKE_FRAME_LEN`]，对方通过认证后放开到 [`MAX_FRAME_LEN`]。
    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit.min(MAX_FRAME_LEN);
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    pub fn next_message(&mut self) -> Result<Option<Message>, ProtocolError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes(self.buf[..4].try_into().expect("4 bytes")) as usize;
        if len > self.limit {
            return Err(ProtocolError::TooLarge(len));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let msg = decode_payload(&self.buf[4..4 + len]);
        self.buf.drain(..4 + len);
        msg.map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples() -> Vec<Message> {
        vec![
            Message::Hello {
                protocol: PROTOCOL_VERSION,
                device: DeviceId("ab12".into()),
                name: "小严的 MacBook".into(),
                os: Os::MacOs,
                app_version: "0.1.0".into(),
                trusts_you: false,
            },
            Message::PairRequest,
            Message::PairConfirm,
            Message::Displays(vec![DisplayInfo {
                local_id: "1".into(),
                name: "内建显示器".into(),
                rect: Rect::new(0.0, 0.0, 1440.0, 900.0),
                scale: 2.0,
                primary: true,
                native: Some((2880, 1800)),
            }]),
            Message::Enter {
                session: 7,
                display: "1".into(),
                x: 0.0,
                y: 450.5,
            },
            Message::Key {
                code: KeyCode::A,
                down: true,
                repeat: false,
            },
            Message::Wheel { dx: 0, dy: -120 },
            Message::Clipboard {
                origin: DeviceId("ab12".into()),
                seq: 3,
                text: "你好，流界".into(),
            },
            Message::Bye {
                reason: ByeReason::Paused,
            },
            Message::FileOffer(FileOffer {
                id: 4,
                names: vec!["报告.pdf".into()],
                items: 1,
                files: 1,
                bytes: 1024,
            }),
            Message::FileRequest { id: 4 },
            Message::FileEntry {
                path: "照片/a.jpg".into(),
                size: 3,
                dir: false,
            },
            Message::FileChunk(vec![0, 255, 7]),
            Message::FileEnd,
            Message::FileError {
                reason: "文件已被删除".into(),
            },
            Message::HeartbeatAck { seq: 7 },
            Message::PairCommit([3; 32]),
            Message::PairNonce([9; 32]),
            Message::Availability { accepting: false },
        ]
    }

    #[test]
    fn new_variants_are_appended_after_old_ones() {
        // 旧版本只认识前 22 个变体；新变体必须排在后面，旧消息的编码保持不变。
        assert_eq!(
            encode_payload(&Message::HeartbeatAck { seq: 0 }).unwrap()[0],
            22
        );
        assert_eq!(
            encode_payload(&Message::PairCommit([0; 32])).unwrap()[0],
            23
        );
        assert_eq!(encode_payload(&Message::PairNonce([0; 32])).unwrap()[0], 24);
        assert_eq!(
            encode_payload(&Message::Availability { accepting: true }).unwrap()[0],
            25
        );
        assert!(peer_supports_secure("0.1.7"));
        assert!(!peer_supports_secure("0.1.6"));
    }

    /// 旧版本按变体序号解析消息，已有消息的编码不能变。
    #[test]
    fn existing_variant_tags_are_stable() {
        assert_eq!(encode_payload(&Message::PairRequest).unwrap(), [1]);
        assert_eq!(encode_payload(&Message::ReleaseAll).unwrap(), [12]);
        assert_eq!(
            encode_payload(&Message::Bye {
                reason: ByeReason::Paused
            })
            .unwrap(),
            [15, 1]
        );
        assert_eq!(encode_payload(&Message::FileEnd).unwrap(), [20]);
    }

    /// 文件内容按字节串编码：长度前缀 + 原样字节，不会膨胀。
    #[test]
    fn file_chunk_is_compact() {
        let chunk = vec![0xAB; crate::files::CHUNK_SIZE];
        let bytes = encode_payload(&Message::FileChunk(chunk.clone())).unwrap();
        assert!(bytes.len() <= chunk.len() + 8);
        assert_eq!(decode_payload(&bytes).unwrap(), Message::FileChunk(chunk));
    }

    #[test]
    fn datagram_roundtrip_and_rejects_garbage() {
        let m = Message::MouseMove {
            session: 3,
            display: "1".into(),
            x: 10.5,
            y: 20.0,
        };
        let bytes = encode_datagram(0xFFFF_FFFE, &m).unwrap();
        assert_eq!(decode_datagram(&bytes), Some((0xFFFF_FFFE, m)));
        assert_eq!(decode_datagram(&[1, 2, 3]), None);
        assert_eq!(decode_datagram(&[0, 0, 0, 1, 0xFF, 0xFF, 0xFF]), None);
    }

    #[test]
    fn seq_comparison_handles_wraparound() {
        assert!(seq_newer(2, 1));
        assert!(!seq_newer(1, 2));
        assert!(!seq_newer(5, 5));
        assert!(seq_newer(0, u32::MAX));
        assert!(seq_newer(3, u32::MAX - 2));
        assert!(!seq_newer(u32::MAX, 0));
    }

    #[test]
    fn version_gate() {
        let min = DATAGRAM_MOVES_SINCE;
        assert!(version_at_least("0.1.3", min));
        assert!(version_at_least("0.2.0", min));
        assert!(version_at_least("1.0.0-beta.1", min));
        assert!(!version_at_least("0.1.2", min));
        assert!(!version_at_least("0.1", min));
        assert!(!version_at_least("", min));
    }

    #[test]
    fn frames_roundtrip_even_when_split_byte_by_byte() {
        let mut stream = Vec::new();
        for m in samples() {
            stream.extend(encode_frame(&m).unwrap());
        }
        let mut dec = FrameDecoder::default();
        let mut got = Vec::new();
        for b in stream {
            dec.push(&[b]);
            while let Some(m) = dec.next_message().unwrap() {
                got.push(m);
            }
        }
        assert_eq!(got, samples());
    }

    #[test]
    fn datagram_roundtrip() {
        let m = Message::MouseMove {
            session: 1,
            display: "2".into(),
            x: 12.25,
            y: 900.0,
        };
        assert_eq!(decode_payload(&encode_payload(&m).unwrap()).unwrap(), m);
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let mut dec = FrameDecoder::default();
        dec.push(&((MAX_FRAME_LEN as u32) + 1).to_be_bytes());
        assert!(matches!(
            dec.next_message(),
            Err(ProtocolError::TooLarge(_))
        ));
    }

    #[test]
    fn handshake_limit_rejects_large_frames_until_raised() {
        let mut dec = FrameDecoder::with_limit(HANDSHAKE_FRAME_LEN);
        dec.push(&((HANDSHAKE_FRAME_LEN as u32) + 1).to_be_bytes());
        assert!(matches!(
            dec.next_message(),
            Err(ProtocolError::TooLarge(_))
        ));
        let big = encode_frame(&Message::Clipboard {
            origin: DeviceId("a".into()),
            seq: 1,
            text: "x".repeat(HANDSHAKE_FRAME_LEN),
        })
        .unwrap();
        let mut dec = FrameDecoder::with_limit(HANDSHAKE_FRAME_LEN);
        dec.push(&big);
        assert!(dec.next_message().is_err());
        let mut dec = FrameDecoder::with_limit(HANDSHAKE_FRAME_LEN);
        dec.set_limit(MAX_FRAME_LEN);
        dec.push(&big);
        assert!(matches!(
            dec.next_message(),
            Ok(Some(Message::Clipboard { .. }))
        ));
    }

    #[test]
    fn clean_name_strips_controls_and_truncates() {
        assert_eq!(clean_name("  小严的 MacBook\n"), "小严的 MacBook");
        assert_eq!(clean_name("a\u{0}b\u{1b}[31mc"), "ab[31mc");
        let long = "长".repeat(500);
        assert_eq!(clean_name(&long).chars().count(), MAX_NAME_CHARS);
        assert_eq!(clean_name("\t\r\n"), "");
    }

    #[test]
    fn garbage_payload_is_error_not_panic() {
        let mut dec = FrameDecoder::default();
        dec.push(&[0, 0, 0, 2, 0xFF, 0xFF]);
        assert!(dec.next_message().is_err());
    }
}
