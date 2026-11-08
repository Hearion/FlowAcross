//! 设备之间的消息与帧编解码。
//!
//! - 可靠流（控制流、剪贴板流）：每帧 = 4 字节大端长度 + postcard 编码的 [`Message`]。
//! - 不可靠数据报（鼠标绝对坐标）：4 字节大端序号 + postcard 编码的 [`Message`]。
//!   数据报可能丢失或乱序，接收方按序号丢弃比已收到的更旧的位置。网络短暂停顿后
//!   直接跳到最新位置，而不是像可靠流那样把积压的旧位置逐个重放。

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::geometry::Rect;
use crate::keys::{InputEvent, KeyCode, MouseButton};
use crate::layout::DeviceId;

/// 协议版本。不兼容的改动必须递增，握手时版本不同则拒绝连接并提示升级。
pub const PROTOCOL_VERSION: u16 = 1;

/// 单帧最大长度。
pub const MAX_FRAME_LEN: usize = 8 * 1024 * 1024;

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
    /// 发起方请求配对。双方各自根据两端证书指纹计算配对码并显示给用户核对。
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
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    pub fn next_message(&mut self) -> Result<Option<Message>, ProtocolError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes(self.buf[..4].try_into().expect("4 bytes")) as usize;
        if len > MAX_FRAME_LEN {
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
        ]
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
    fn garbage_payload_is_error_not_panic() {
        let mut dec = FrameDecoder::default();
        dec.push(&[0, 0, 0, 2, 0xFF, 0xFF]);
        assert!(dec.next_message().is_err());
    }
}
