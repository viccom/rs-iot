//! udp IO backend（spec §8）：connected UdpSocket，datagram 边界天然保帧。

use super::IoBackend;

/// UDP IO backend：connected `UdpSocket`，懒 connect + err 置 None。
///
/// UDP datagram 边界天然保帧（一 send 一 recv = 整包响应），无 tcp/serial 的 idle 累积。
/// `request`：send frame → recv 一个 datagram（整包响应）。`recv_idle`：recv 被动 datagram。
/// connected socket（bind 0.0.0.0:0 + connect endpoint），对标 modbus rtu_udp（spec §8）。
pub(in crate::plugin::builtin::lua_client) struct UdpIo {
    /// 当前 socket（None = 未 connect/err，下次 request 前 ensure_connected 重 connect）。
    sock: Option<tokio::net::UdpSocket>,
    endpoint: String,
    idle: std::time::Duration,
}

impl UdpIo {
    /// 构造（不立即 connect，首次 request 才 connect）。
    pub(in crate::plugin::builtin::lua_client) fn new(endpoint: String, recv_idle_ms: u64) -> Self {
        Self {
            sock: None,
            endpoint,
            idle: std::time::Duration::from_millis(recv_idle_ms),
        }
    }

    /// 确保已 connect；None 则 bind + connect（失败返回 Err）。
    async fn ensure_connected(&mut self) -> mlua::Result<()> {
        if self.sock.is_some() {
            return Ok(());
        }
        let addr: std::net::SocketAddr = self.endpoint.parse().map_err(|e| {
            mlua::Error::RuntimeError(format!("udp endpoint {}: {e}", self.endpoint))
        })?;
        let sock = tokio::net::UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|e| mlua::Error::RuntimeError(format!("udp bind: {e}")))?;
        sock.connect(addr).await.map_err(|e| {
            mlua::Error::RuntimeError(format!("udp connect {}: {e}", self.endpoint))
        })?;
        self.sock = Some(sock);
        Ok(())
    }
}

#[async_trait::async_trait]
impl IoBackend for UdpIo {
    async fn request(&mut self, frame: &[u8]) -> mlua::Result<Vec<u8>> {
        self.ensure_connected().await?;
        let sock = self.sock.as_mut().expect("ensure_connected 后必有 sock");
        // send 整 frame（一个 datagram）；失败置 None。
        if let Err(e) = sock.send(frame).await {
            self.sock = None;
            return Err(mlua::Error::RuntimeError(format!("udp send: {e}")));
        }
        // recv 一个 datagram（整包响应）；timeout 用 idle；失败置 None。
        let mut buf = vec![0u8; 1500]; // 以太网 MTU
        match tokio::time::timeout(self.idle, sock.recv(&mut buf)).await {
            Ok(Ok(n)) => {
                buf.truncate(n);
                Ok(buf)
            }
            Ok(Err(e)) => {
                self.sock = None;
                Err(mlua::Error::RuntimeError(format!("udp recv: {e}")))
            }
            Err(_) => Err(mlua::Error::RuntimeError(format!(
                "udp recv timeout ({}ms)",
                self.idle.as_millis()
            ))),
        }
    }

    /// 无在飞 request 时主动收一个 datagram（被动广播/推送）。
    async fn recv_idle(&mut self) -> Option<Vec<u8>> {
        self.ensure_connected().await.ok()?;
        let sock = self.sock.as_mut()?;
        let mut buf = vec![0u8; 1500];
        match tokio::time::timeout(self.idle, sock.recv(&mut buf)).await {
            Ok(Ok(n)) => {
                buf.truncate(n);
                Some(buf)
            }
            _ => None, // timeout / err → None（不置 None sock，recv_idle 偶发 err 不致命）
        }
    }
}
