//! IO 引擎：channel + 独占 IO task + oneshot 回执 + timeout（spec §8 模式 Z）。
//!
//! Lua 的 ctx:request 经 create_async_function → mpsc 送 IoRequest → IO task 独占连接
//! 处理 → oneshot 回执；timeout 包 oneshot，超时丢弃迟到回包。

mod mock;
mod serial;
mod tcp;
mod udp;

use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

#[allow(unused_imports)]
// MockIoTask：luaClient run 不再用（Task 4 去占位）；io_engine/api tests 用，dual-build
pub(super) use mock::MockIoTask;
pub(super) use serial::SerialIo;
pub(super) use tcp::TcpIo;
pub(super) use udp::UdpIo;

/// 一次 request：帧字节 + 回执通道。
pub(super) struct IoRequest {
    pub(super) frame: Vec<u8>,
    pub(super) reply_to: oneshot::Sender<mlua::Result<Vec<u8>>>,
}

/// IO 后端 trait：IO task 持有一个，处理 request + 被动帧（on_data）。
#[async_trait::async_trait]
pub(super) trait IoBackend: Send + 'static {
    /// 处理一次 request：发 frame、收响应、返回字节（或 Err）。
    async fn request(&mut self, frame: &[u8]) -> mlua::Result<Vec<u8>>;

    /// 无在飞 request 时主动收一帧（广播/推送场景）。默认 `None`（不支持被动帧）；
    /// `TcpIo` 覆盖为 idle 成帧 read。`Some(frame)` 推 `on_data`；`None` = idle 超时/未连接。
    ///
    /// 默认实现 `sleep 50ms` 后返回 None——避免 IO task `select!` 在无 req 时忙循环
    /// （若立即返回 None，select! 不断 poll recv_idle → 100% CPU）。
    async fn recv_idle(&mut self) -> Option<Vec<u8>> {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        None
    }
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

/// 启动 IO task（独占 backend）。`on_data_tx` 接被动帧（无在飞 request 时 recv_idle 收到）。
/// 返回 IoEngine 句柄（Lua 侧 request 用）。`inst_id` + `trace_registry` 用于报文 trace
/// （request TX/RX + recv_idle 被动 RX 三点 dump；trace 关闭时 dump 快速路径跳过）。
pub(super) fn spawn_io_task(
    mut backend: Box<dyn IoBackend>,
    on_data_tx: mpsc::Sender<Vec<u8>>,
    inst_id: String,
    trace_registry: crate::io_trace::TraceRegistry,
) -> IoEngine {
    let (tx, mut rx) = mpsc::channel::<IoRequest>(64);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased; // request 优先：有 request 时立即处理，不被 recv_idle 阻塞
                req = rx.recv() => match req {
                    Some(req) => {
                        crate::io_trace::dump(&trace_registry, &inst_id, crate::io_trace::Dir::Tx, &req.frame);
                        let r = backend.request(&req.frame).await;
                        if let Ok(resp) = &r {
                            crate::io_trace::dump(&trace_registry, &inst_id, crate::io_trace::Dir::Rx, resp);
                        }
                        let _ = req.reply_to.send(r);
                    }
                    None => break, // 所有 IoEngine 句柄 drop → 退出
                },
                res = backend.recv_idle() => {
                    if let Some(frame) = res {
                        crate::io_trace::dump(&trace_registry, &inst_id, crate::io_trace::Dir::Rx, &frame);
                        let _ = on_data_tx.send(frame).await; // 主循环 drop → 忽略
                    }
                    // None（idle 无数据/未连接）→ 回 loop 重 select!（recv_idle 重启）
                }
            }
        }
    });
    IoEngine { tx }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// 测试用：spawn_io_task 配一个丢弃被动帧的 on_data channel（单测不测 on_data）。
    fn spawn_no_on_data(backend: Box<dyn IoBackend>) -> IoEngine {
        let (tx, _rx) = mpsc::channel::<Vec<u8>>(8);
        spawn_io_task(
            backend,
            tx,
            "test-inst".into(),
            crate::io_trace::TraceRegistry::new(),
        )
    }

    #[tokio::test]
    async fn mock_roundtrip() {
        let mock = MockIoTask {
            responder: Box::new(|f: &[u8]| {
                let mut r = f.to_vec();
                r.push(0xAA);
                Ok(r)
            }),
        };
        let engine = spawn_no_on_data(Box::new(mock));
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

    /// on_data：mock server 无请求时主动推一帧 → IO task recv_idle 收到 → on_data channel。
    #[tokio::test]
    async fn on_data_frame_from_idle_server() {
        use std::time::Duration;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // server：accept 后 sleep 100ms（让 client 连上 + IO task 进 recv_idle）→ 主动推一帧。
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = s.write_all(b"\x02ping").await;
            tokio::time::sleep(Duration::from_millis(200)).await; // 保连接不断
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let backend = Box::new(TcpIo::new(format!("127.0.0.1:{port}"), 50));
        let (on_data_tx, mut on_data_rx) = mpsc::channel::<Vec<u8>>(8);
        let _engine = spawn_io_task(
            backend,
            on_data_tx,
            "test-inst".into(),
            crate::io_trace::TraceRegistry::new(),
        );

        let frame = tokio::time::timeout(Duration::from_secs(2), on_data_rx.recv())
            .await
            .expect("超时")
            .expect("应收到被动帧");
        assert_eq!(frame, b"\x02ping");
    }

    /// serial 闭环：需真机/虚拟串口。CI 标 ignore；手动跑配 `SERIAL_PORT_A` 环境变量。
    #[tokio::test]
    #[ignore = "serial 需真机/虚拟串口（配置 SERIAL_PORT_A 环境变量）"]
    async fn serial_roundtrip() {
        let port = std::env::var("SERIAL_PORT_A").expect("设 SERIAL_PORT_A");
        let mut io = SerialIo::new(
            port,
            9600,
            8,
            super::super::config::Parity::None,
            1,
            super::super::config::FlowControl::None,
            50,
        );
        let r = io.request(b"\x01\x02").await;
        assert!(r.is_ok(), "serial request: {r:?}");
    }

    #[tokio::test]
    async fn udp_roundtrip() {
        use tokio::net::UdpSocket;
        // mock udp server：bind 127.0.0.1:0，收 frame 后回 echo + 0xAA。
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            let (n, peer) = server.recv_from(&mut buf).await.unwrap();
            let mut r = buf[..n].to_vec();
            r.push(0xAA);
            let _ = server.send_to(&r, peer).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut io = UdpIo::new(addr.to_string(), 200);
        let resp = io.request(b"\x01\x02").await.unwrap();
        assert_eq!(resp, vec![0x01, 0x02, 0xAA]);
    }

    #[tokio::test]
    async fn udp_bad_endpoint_errors() {
        let mut io = UdpIo::new("not-an-endpoint".into(), 200);
        let r = io.request(b"x").await;
        assert!(r.is_err(), "非法 endpoint 应 connect 失败，got {r:?}");
    }
}
