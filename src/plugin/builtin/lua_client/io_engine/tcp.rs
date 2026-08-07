//! TCP IO backend（spec §8）：独占流，懒连接 + 断连重连。

use super::IoBackend;

/// TCP IO backend：独占一条 `TcpStream`，懒连接 + 断连置 None（下次 request 重连）。
///
/// spec §8 不变式：连接生命周期（connect/reconnect）由 IO task 管，Lua 只感知 request
/// 成功/失败。重连频率受 collect period 天然限速（period=3s → 最多每 3s 重连一次），
/// 故不在 request 内 sleep 退避（避免阻塞 IO task 串行处理）。
pub(in crate::plugin::builtin::lua_client) struct TcpIo {
    /// 当前连接（None = 断连，下次 request 前 ensure_connected 重连）。
    stream: Option<tokio::net::TcpStream>,
    endpoint: String,
    idle: std::time::Duration,
}

impl TcpIo {
    /// 懒连接构造：不立即连，首次 request 才连（spec §5.4「ensure io task connected」）。
    pub(in crate::plugin::builtin::lua_client) fn new(endpoint: String, recv_idle_ms: u64) -> Self {
        Self {
            stream: None,
            endpoint,
            idle: std::time::Duration::from_millis(recv_idle_ms),
        }
    }

    /// 确保已连接；None 则 connect（失败返回 Err，不 sleep）。
    async fn ensure_connected(&mut self) -> mlua::Result<()> {
        if self.stream.is_some() {
            return Ok(());
        }
        match tokio::net::TcpStream::connect(&self.endpoint).await {
            Ok(s) => {
                self.stream = Some(s);
                Ok(())
            }
            Err(e) => Err(mlua::Error::RuntimeError(format!(
                "tcp connect {}: {e}",
                self.endpoint
            ))),
        }
    }
}

#[async_trait::async_trait]
impl IoBackend for TcpIo {
    async fn request(&mut self, frame: &[u8]) -> mlua::Result<Vec<u8>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        self.ensure_connected().await?;
        // write：失败 → 断连，置 None。
        if let Err(e) = self
            .stream
            .as_mut()
            .expect("ensure_connected 后必有 stream")
            .write_all(frame)
            .await
        {
            self.stream = None;
            return Err(mlua::Error::RuntimeError(format!("tcp write: {e}")));
        }
        // read（Timeout 成帧）：每次迭代重新取 stream 引用，断连标志带出循环外再置 None
        // （循环内长期持有 &mut stream 会与循环外 self.stream = None 借用冲突）。
        let mut buf = Vec::new();
        let mut tmp = [0u8; 256];
        let mut io_err: Option<std::io::Error> = None;
        let mut peer_closed = false;
        loop {
            let stream = self.stream.as_mut().expect("read 期间 stream 必在");
            match tokio::time::timeout(self.idle, stream.read(&mut tmp)).await {
                Ok(Ok(0)) => {
                    peer_closed = true;
                    break;
                }
                Ok(Ok(n)) => buf.extend_from_slice(&tmp[..n]),
                Ok(Err(e)) => {
                    io_err = Some(e);
                    break;
                }
                Err(_) => break, // idle 静默 → 帧完成
            }
        }
        if let Some(e) = io_err {
            self.stream = None;
            return Err(mlua::Error::RuntimeError(format!("tcp read: {e}")));
        }
        // buf 非空 → 帧完成（即使随后对端 Ok(0) 关闭，已收到完整响应，返回 buf）。
        // peer_closed 置 None（下次 request 重连）；仅 buf 空 + peer_closed 才报对端关闭。
        if peer_closed {
            self.stream = None;
        }
        if buf.is_empty() {
            return Err(mlua::Error::RuntimeError(
                (if peer_closed {
                    "tcp: 对端关闭"
                } else {
                    "tcp: 无响应"
                })
                .into(),
            ));
        }
        Ok(buf)
    }

    /// 无在飞 request 时主动收一帧（idle 成帧）：ensure_connected + read 累积，
    /// idle 静默返回 `Some(buf)`；idle 超时无数据 / 未连接 / 对端关闭返回 `None`。
    /// 对端关闭或 read err → 置 `stream = None`（下次 request/recv_idle 重连）。
    async fn recv_idle(&mut self) -> Option<Vec<u8>> {
        use tokio::io::AsyncReadExt;
        self.ensure_connected().await.ok()?;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 256];
        loop {
            let stream = self.stream.as_mut()?;
            match tokio::time::timeout(self.idle, stream.read(&mut tmp)).await {
                Ok(Ok(0)) => {
                    self.stream = None; // 对端关闭 → 重连
                    break;
                }
                Ok(Ok(n)) => buf.extend_from_slice(&tmp[..n]),
                Ok(Err(_)) => {
                    self.stream = None;
                    break;
                }
                Err(_) => break, // idle 静默 → 帧完成（buf 非空）或无数据
            }
        }
        if buf.is_empty() {
            None
        } else {
            Some(buf)
        }
    }
}
