//! 用 QUIC 数据报收发鼠标位置。
//!
//! 可靠流在网络短暂停顿（例如 Mac 的 Wi‑Fi 因 AWDL 切换信道）后会把积压的位置逐个重放，
//! 光标像"卡一下再快进"。数据报不重传：发送端每条带递增序号，接收端一次读完已到达的
//! 数据报，只交出最新的一个，过时和乱序的直接丢掉，停顿后光标直接跳到当前位置。
//!
//! 只有对方版本不低于 [`DATAGRAM_MOVES_SINCE`] 时才发数据报（旧版本不读数据报），
//! 否则调用方继续用可靠流发送。

use std::time::Duration;

use bytes::Bytes;
use flow_core::protocol::{
    DATAGRAM_MOVES_SINCE, Message, decode_datagram, encode_datagram, seq_newer, version_at_least,
};

use crate::NetError;

pub struct MoveSender {
    conn: quinn::Connection,
    enabled: bool,
    seq: u32,
}

impl MoveSender {
    /// `peer_version`：对方 Hello 里的程序版本。
    pub fn new(conn: quinn::Connection, peer_version: &str) -> Self {
        let enabled = version_at_least(peer_version, DATAGRAM_MOVES_SINCE)
            && conn.max_datagram_size().is_some();
        Self {
            conn,
            enabled,
            seq: 0,
        }
    }

    /// 是否在用数据报发送鼠标位置。
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 尝试用数据报发送；返回 `false` 时调用方应改用可靠流（不是鼠标位置、对方不支持或数据报发送失败）。
    pub fn try_send(&mut self, msg: &Message) -> bool {
        if !self.enabled || !matches!(msg, Message::MouseMove { .. }) {
            return false;
        }
        self.seq = self.seq.wrapping_add(1);
        let Ok(bytes) = encode_datagram(self.seq, msg) else {
            return false;
        };
        if self
            .conn
            .max_datagram_size()
            .is_none_or(|max| bytes.len() > max)
        {
            return false;
        }
        self.conn.send_datagram(Bytes::from(bytes)).is_ok()
    }
}

pub struct MoveReceiver {
    conn: quinn::Connection,
    last: Option<u32>,
}

impl MoveReceiver {
    pub fn new(conn: quinn::Connection) -> Self {
        Self { conn, last: None }
    }

    /// 等待下一批数据报，返回其中最新的鼠标位置；这一批全是过时的则返回 `Ok(None)`。
    /// 连接断开时返回错误。
    pub async fn recv_latest(&mut self) -> Result<Option<Message>, NetError> {
        let first = self.conn.read_datagram().await?;
        let mut best = self.accept(&first);
        // 把已经到达的都读掉（零超时 = 只取现成的，不等待），只留最新的。
        while let Ok(more) = tokio::time::timeout(Duration::ZERO, self.conn.read_datagram()).await {
            if let Some(m) = self.accept(&more?) {
                best = Some(m);
            }
        }
        Ok(best)
    }

    /// 比已收到的更新才接受，并记下它的序号。
    fn accept(&mut self, bytes: &[u8]) -> Option<Message> {
        let (seq, msg) = decode_datagram(bytes)?;
        if !matches!(msg, Message::MouseMove { .. }) {
            return None;
        }
        if self.last.is_some_and(|last| !seq_newer(seq, last)) {
            return None;
        }
        self.last = Some(seq);
        Some(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Identity;
    use crate::tls::bind_endpoint;

    fn mv(x: f64) -> Message {
        Message::MouseMove {
            session: 1,
            display: "1".into(),
            x,
            y: 0.0,
        }
    }

    async fn pair() -> (quinn::Connection, quinn::Connection) {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let ep_a = bind_endpoint(&a, 0).unwrap();
        let ep_b = bind_endpoint(&b, 0).unwrap();
        let addr_b =
            std::net::SocketAddr::from(([127, 0, 0, 1], ep_b.local_addr().unwrap().port()));
        let server = tokio::spawn(async move { ep_b.accept().await.unwrap().await.unwrap() });
        let client = ep_a
            .connect(addr_b, "flowacross.local")
            .unwrap()
            .await
            .unwrap();
        (client, server.await.unwrap())
    }

    /// 积压的一批位置只交出最新的一个；旧版本对端不发数据报；非鼠标消息不走数据报。
    #[tokio::test]
    async fn burst_yields_only_latest_move() {
        let (client, server) = pair().await;
        assert!(!MoveSender::new(client.clone(), "0.1.2").enabled());
        let mut tx = MoveSender::new(client.clone(), "0.1.3");
        assert!(tx.enabled());
        assert!(!tx.try_send(&Message::ReleaseAll));
        for i in 1..=5 {
            assert!(tx.try_send(&mv(i as f64)));
        }
        // 等这批都到达后再读，模拟网络停顿后成批到达。
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut rx = MoveReceiver::new(server.clone());
        assert_eq!(rx.recv_latest().await.unwrap(), Some(mv(5.0)));

        // 之后的新位置照常收到。
        assert!(tx.try_send(&mv(6.0)));
        assert_eq!(rx.recv_latest().await.unwrap(), Some(mv(6.0)));
        // 过时的位置（序号更旧）被丢弃。
        assert_eq!(rx.accept(&encode_datagram(3, &mv(3.0)).unwrap()), None);
    }
}
