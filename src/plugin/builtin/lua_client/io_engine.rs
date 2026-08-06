//! IO 引擎：channel + 独占 IO task + oneshot 回执 + timeout（spec §8 模式 Z）。
//!
//! Lua 的 ctx:request 经 create_async_function → mpsc 送 IoRequest → IO task 独占连接
//! 处理 → oneshot 回执；timeout 包 oneshot，超时丢弃迟到回包。

use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

/// 一次 request：帧字节 + 回执通道。
pub(super) struct IoRequest {
    pub(super) frame: Vec<u8>,
    pub(super) reply_to: oneshot::Sender<mlua::Result<Vec<u8>>>,
}

/// IO 后端 trait：IO task 持有一个，处理 request。Plan 1 有 MockIoTask；Task 7 加 TcpIo。
#[async_trait::async_trait]
pub(super) trait IoBackend: Send + 'static {
    /// 处理一次 request：发 frame、收响应、返回字节（或 Err）。
    async fn request(&mut self, frame: &[u8]) -> mlua::Result<Vec<u8>>;
}

/// IO 引擎句柄：Lua 侧通过它发 request（经 channel 送 IO task）。
#[derive(Clone)]
pub(super) struct IoEngine {
    tx: mpsc::Sender<IoRequest>,
}

impl IoEngine {
    /// 发一次 request，等回执（带超时）。供 ctx:request 的 async fn 调用。
    pub(super) async fn request(&self, frame: &[u8], timeout_ms: u64) -> mlua::Result<Vec<u8>> {
        let (otx, orx) = oneshot::channel();
        self.tx
            .send(IoRequest {
                frame: frame.to_vec(),
                reply_to: otx,
            })
            .await
            .map_err(|_| mlua::Error::RuntimeError("IO task closed".into()))?;
        match timeout(Duration::from_millis(timeout_ms), orx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(mlua::Error::RuntimeError("IO reply dropped".into())),
            Err(_) => Err(mlua::Error::RuntimeError(format!(
                "IO request timeout ({timeout_ms}ms)"
            ))),
        }
    }
}

/// 启动 IO task（独占 backend）。返回 IoEngine 句柄。
pub(super) fn spawn_io_task(mut backend: Box<dyn IoBackend>) -> IoEngine {
    let (tx, mut rx) = mpsc::channel::<IoRequest>(64);
    tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            let r = backend.request(&req.frame).await;
            let _ = req.reply_to.send(r); // 回执失败说明 Lua 侧已超时放弃 → 忽略
        }
    });
    IoEngine { tx }
}

// ===== MockIoTask（测试用，可编程响应）=====

/// mock 响应闭包类型（frame 字节 → 响应字节 / Err）。拆 alias 规避 clippy type_complexity。
pub(super) type MockResponder = Box<dyn Fn(&[u8]) -> mlua::Result<Vec<u8>> + Send + Sync>;

/// mock IO backend：按闭包响应。用于无真实设备的单测/集成测。
pub(super) struct MockIoTask {
    /// 响应闭包：输入 frame 字节，返回响应字节（或 Err）。
    pub(super) responder: MockResponder,
}

#[async_trait::async_trait]
impl IoBackend for MockIoTask {
    async fn request(&mut self, frame: &[u8]) -> mlua::Result<Vec<u8>> {
        (self.responder)(frame)
    }
}

// ===== TcpIo（TCP backend：独占流，Option 懒连接 + 断连重连）=====

/// TCP IO backend：独占一条 `TcpStream`，懒连接 + 断连置 None（下次 request 重连）。
///
/// spec §8 不变式：连接生命周期（connect/reconnect）由 IO task 管，Lua 只感知 request
/// 成功/失败。重连频率受 collect period 天然限速（period=3s → 最多每 3s 重连一次），
/// 故不在 request 内 sleep 退避（避免阻塞 IO task 串行处理）。
pub(super) struct TcpIo {
    /// 当前连接（None = 断连，下次 request 前 ensure_connected 重连）。
    stream: Option<tokio::net::TcpStream>,
    endpoint: String,
    idle: std::time::Duration,
}

impl TcpIo {
    /// 懒连接构造：不立即连，首次 request 才连（spec §5.4「ensure io task connected」）。
    pub(super) fn new(endpoint: String, recv_idle_ms: u64) -> Self {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn mock_roundtrip() {
        let mock = MockIoTask {
            responder: Box::new(|f: &[u8]| {
                let mut r = f.to_vec();
                r.push(0xAA);
                Ok(r)
            }),
        };
        let engine = spawn_io_task(Box::new(mock));
        let resp = engine.request(b"\x01\x02", 200).await.unwrap();
        assert_eq!(resp, vec![0x01, 0x02, 0xAA]);
    }

    #[tokio::test]
    async fn timeout_when_no_backend_processing() {
        // 构造一个 IoEngine，其 channel 的接收端被立即 drop（无 backend 处理）。
        let (tx, _rx) = mpsc::channel::<IoRequest>(1);
        let dead = IoEngine { tx };
        let r = dead.request(b"x", 100).await;
        // _rx 还活着（未 drop），send 成功但无 backend 处理 → 超时。
        // 注意：若 drop(_rx) 则 send 失败 → "IO task closed" 错。这里保留 _rx 触发超时。
        assert!(r.is_err(), "无 backend 处理应超时或报错，got: {r:?}");
    }

    #[tokio::test]
    async fn tcp_roundtrip() {
        // mock server：收什么回什么 + 尾巴 0xAA。
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 64];
            loop {
                match s.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut r = buf[..n].to_vec();
                        r.push(0xAA);
                        let _ = s.write_all(&r).await;
                    }
                }
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // TcpIo::new 懒连接：不预连，首次 request 才 connect。
        let mut io = TcpIo::new(format!("127.0.0.1:{port}"), 50);
        let resp = io.request(b"\x01\x02").await.unwrap();
        assert_eq!(resp, vec![0x01, 0x02, 0xAA]);
    }

    /// 断连重连：第一次 request 连上但 server 立即断 → Err + stream 置 None；
    /// 第二次 request 自动重连（新连接）→ 成功。
    #[tokio::test]
    async fn tcp_reconnects_after_drop() {
        use std::time::Duration;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // 接受 2 次连接：round 0 读一帧后立即 drop（断连）；round 1 正常回显 + 0xAA。
        tokio::spawn(async move {
            for round in 0..2u32 {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 64];
                let n = s.read(&mut buf).await.unwrap_or(0);
                if round == 0 {
                    drop(s); // 读到即断
                } else if n > 0 {
                    let mut r = buf[..n].to_vec();
                    r.push(0xAA);
                    let _ = s.write_all(&r).await;
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut io = TcpIo::new(addr.to_string(), 50);
        // 第一次：连上但 server 立即断 → Err（stream 置 None）。
        let r1 = io.request(b"\x01").await;
        assert!(r1.is_err(), "第一次（server 断连）应 Err，got {r1:?}");
        // 第二次：重连 → 成功。
        tokio::time::sleep(Duration::from_millis(150)).await; // 等 server accept round 1
        let r2 = io.request(b"\x01").await;
        assert!(r2.is_ok(), "第二次（重连）应成功，got {r2:?}");
        assert_eq!(r2.unwrap(), vec![0x01, 0xAA]);
    }
}
