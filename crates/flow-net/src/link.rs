//! 在 QUIC 连接的控制流上收发消息。
//!
//! 发起方打开双向流并先发消息（QUIC 在有数据写入前不会通知对方新流），接受方等待这条流。
//! 会话建立后，任何一方都可以用 [`open_stream`] 另开流传文件，对方用 [`accept_stream`] 接收。

use flow_core::layout::DeviceId;
use flow_core::pairing::{Fingerprint, device_id};
use flow_core::protocol::{FrameDecoder, Message, encode_frame};

use crate::NetError;
use crate::tls::peer_fingerprint;

/// 一条已认证的设备连接（尚未判断是否可信）。
pub struct Link {
    pub conn: quinn::Connection,
    pub peer_fingerprint: Fingerprint,
    pub peer_id: DeviceId,
    pub sender: LinkSender,
    pub receiver: LinkReceiver,
}

pub struct LinkSender {
    send: quinn::SendStream,
}

pub struct LinkReceiver {
    recv: quinn::RecvStream,
    decoder: FrameDecoder,
    buf: Vec<u8>,
}

impl Link {
    /// 发起方：打开控制流，并立即发送第一条消息（通常是 Hello）。
    pub async fn open(conn: quinn::Connection, first: &Message) -> Result<Link, NetError> {
        let fp = peer_fingerprint(&conn)?;
        let (send, recv) = conn.open_bi().await?;
        let mut sender = LinkSender { send };
        sender.send(first).await?;
        Ok(Link {
            peer_id: device_id(&fp),
            peer_fingerprint: fp,
            conn,
            sender,
            receiver: LinkReceiver::new(recv),
        })
    }

    /// 接受方：等待发起方打开的控制流。
    pub async fn accept(conn: quinn::Connection) -> Result<Link, NetError> {
        let fp = peer_fingerprint(&conn)?;
        let (send, recv) = conn.accept_bi().await?;
        Ok(Link {
            peer_id: device_id(&fp),
            peer_fingerprint: fp,
            conn,
            sender: LinkSender { send },
            receiver: LinkReceiver::new(recv),
        })
    }
}

/// 在已建立的连接上新开一条流（用来传文件，不阻塞控制流里的键鼠消息）。
/// QUIC 在写入数据前不会通知对方有新流，打开后要马上发第一条消息。
pub async fn open_stream(conn: &quinn::Connection) -> Result<(LinkSender, LinkReceiver), NetError> {
    let (send, recv) = conn.open_bi().await?;
    Ok((
        LinkSender { send },
        LinkReceiver::with_buffer(recv, BULK_BUF),
    ))
}

/// 等待对方新开的流（控制流之外的）。连接断开时返回错误。
pub async fn accept_stream(
    conn: &quinn::Connection,
) -> Result<(LinkSender, LinkReceiver), NetError> {
    let (send, recv) = conn.accept_bi().await?;
    Ok((
        LinkSender { send },
        LinkReceiver::with_buffer(recv, BULK_BUF),
    ))
}

/// 传文件的流读缓冲大一些，减少读取次数。
const BULK_BUF: usize = 64 * 1024;

impl LinkSender {
    pub async fn send(&mut self, msg: &Message) -> Result<(), NetError> {
        let frame = encode_frame(msg)?;
        self.send.write_all(&frame).await?;
        Ok(())
    }

    /// 发完了：正常关闭发送方向，对方读到结尾。
    pub fn finish(&mut self) {
        let _ = self.send.finish();
    }

    /// 中止发送（取消传输），对方读取时立即出错。
    pub fn reset(&mut self) {
        let _ = self.send.reset(0u32.into());
    }
}

impl LinkReceiver {
    fn new(recv: quinn::RecvStream) -> Self {
        Self::with_buffer(recv, 16 * 1024)
    }

    fn with_buffer(recv: quinn::RecvStream, size: usize) -> Self {
        Self {
            recv,
            decoder: FrameDecoder::default(),
            buf: vec![0u8; size],
        }
    }

    /// 读取下一条消息；对方正常关闭流时返回 `None`。
    pub async fn recv(&mut self) -> Result<Option<Message>, NetError> {
        loop {
            if let Some(msg) = self.decoder.next_message()? {
                return Ok(Some(msg));
            }
            match self.recv.read(&mut self.buf).await? {
                Some(n) => self.decoder.push(&self.buf[..n]),
                None => return Ok(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Identity;
    use crate::tls::bind_endpoint;
    use flow_core::protocol::{Os, PROTOCOL_VERSION};

    fn hello(id: &Identity) -> Message {
        Message::Hello {
            protocol: PROTOCOL_VERSION,
            device: id.device_id.clone(),
            name: "test".into(),
            os: Os::current(),
            app_version: "0".into(),
            trusts_you: false,
        }
    }

    /// 两个端点在本机回环地址上建立双向认证的 QUIC 连接，互相看到对方的真实指纹并交换消息。
    #[tokio::test]
    async fn loopback_mutual_auth_and_messages() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let ep_a = bind_endpoint(&a, 0).unwrap();
        let ep_b = bind_endpoint(&b, 0).unwrap();
        let addr_b =
            std::net::SocketAddr::from(([127, 0, 0, 1], ep_b.local_addr().unwrap().port()));

        let b_id = b.clone();
        let server = tokio::spawn(async move {
            let conn = ep_b.accept().await.unwrap().await.unwrap();
            let mut link = Link::accept(conn).await.unwrap();
            let first = link.receiver.recv().await.unwrap().unwrap();
            link.sender.send(&hello(&b_id)).await.unwrap();
            let second = link.receiver.recv().await.unwrap().unwrap();
            (link.peer_id, first, second)
        });

        let conn = ep_a
            .connect(addr_b, "flowacross.local")
            .unwrap()
            .await
            .unwrap();
        let mut link = Link::open(conn, &hello(&a)).await.unwrap();
        assert_eq!(link.peer_id, b.device_id);
        let reply = link.receiver.recv().await.unwrap().unwrap();
        assert!(matches!(reply, Message::Hello { device, .. } if device == b.device_id));
        link.sender.send(&Message::PairRequest).await.unwrap();

        let (seen_by_b, first, second) = server.await.unwrap();
        assert_eq!(seen_by_b, a.device_id);
        assert!(matches!(first, Message::Hello { .. }));
        assert_eq!(second, Message::PairRequest);
    }

    /// 控制流建立后，另开的流可以独立传大块数据，控制流照常收发。
    #[tokio::test]
    async fn extra_streams_carry_bulk_data_beside_control() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let ep_a = bind_endpoint(&a, 0).unwrap();
        let ep_b = bind_endpoint(&b, 0).unwrap();
        let addr_b =
            std::net::SocketAddr::from(([127, 0, 0, 1], ep_b.local_addr().unwrap().port()));
        let chunk = vec![7u8; flow_core::files::CHUNK_SIZE];

        let expected = chunk.clone();
        let server = tokio::spawn(async move {
            let conn = ep_b.accept().await.unwrap().await.unwrap();
            let mut link = Link::accept(conn.clone()).await.unwrap();
            link.receiver.recv().await.unwrap().unwrap();
            let (mut tx, mut rx) = accept_stream(&conn).await.unwrap();
            assert_eq!(
                rx.recv().await.unwrap(),
                Some(Message::FileRequest { id: 9 })
            );
            for _ in 0..8 {
                tx.send(&Message::FileChunk(expected.clone()))
                    .await
                    .unwrap();
            }
            tx.send(&Message::FileEnd).await.unwrap();
            tx.finish();
            // 控制流不受影响。
            link.sender.send(&Message::ReleaseAll).await.unwrap();
            link.receiver.recv().await.unwrap()
        });

        let conn = ep_a
            .connect(addr_b, "flowacross.local")
            .unwrap()
            .await
            .unwrap();
        let mut link = Link::open(conn.clone(), &hello(&a)).await.unwrap();
        let (mut tx, mut rx) = open_stream(&conn).await.unwrap();
        tx.send(&Message::FileRequest { id: 9 }).await.unwrap();
        let mut chunks = 0;
        loop {
            match rx.recv().await.unwrap() {
                Some(Message::FileChunk(c)) => {
                    assert_eq!(c, chunk);
                    chunks += 1;
                }
                Some(Message::FileEnd) => break,
                other => panic!("意外的消息：{other:?}"),
            }
        }
        assert_eq!(chunks, 8);
        assert_eq!(rx.recv().await.unwrap(), None, "发送方正常关闭");
        assert_eq!(
            link.receiver.recv().await.unwrap(),
            Some(Message::ReleaseAll)
        );
        link.sender.send(&Message::PairConfirm).await.unwrap();
        assert_eq!(server.await.unwrap(), Some(Message::PairConfirm));
    }
}
