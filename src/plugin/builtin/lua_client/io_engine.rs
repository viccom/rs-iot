//! IO 引擎：channel + 独占 IO task + oneshot 回执 + timeout（spec §8 模式 Z）。
//!
//! Lua 的 ctx:request 经 create_async_function → mpsc 送 IoRequest → IO task 独占连接
//! 处理 → oneshot 回执；timeout 包 oneshot，超时丢弃迟到回包。
//!
//! 脚手架：本模块全部项（IoRequest/IoBackend/IoEngine/spawn_io_task/MockIoTask）在
//! lib build 无调用方（Task 6 LuaClient 主循环 + Task 8 ctx API 接入），仅 test build
//! 与后续 task 使用。用 `#[allow(dead_code)]` 而非 CLAUDE.md 首选的 `#[expect]`：与
//! config/engine/loader 同属 dual-build 脚手架（trait 方法 request 在 lib build 未被调、
//! test build 调用），部分项仅在 test build 命中，无条件 expect 会暴露 unfulfilled。
//! Task 6/8 接入后移除。

use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

/// 一次 request：帧字节 + 回执通道。
#[allow(dead_code)]
pub(super) struct IoRequest {
    pub(super) frame: Vec<u8>,
    pub(super) reply_to: oneshot::Sender<mlua::Result<Vec<u8>>>,
}

/// IO 后端 trait：IO task 持有一个，处理 request。Plan 1 有 MockIoTask；Task 7 加 TcpIo。
#[allow(dead_code)] // trait 方法 request 在 lib build 未被调；test build 经 MockIoTask 调用
#[async_trait::async_trait]
pub(super) trait IoBackend: Send + 'static {
    /// 处理一次 request：发 frame、收响应、返回字节（或 Err）。
    async fn request(&mut self, frame: &[u8]) -> mlua::Result<Vec<u8>>;
}

/// IO 引擎句柄：Lua 侧通过它发 request（经 channel 送 IO task）。
#[allow(dead_code)]
#[derive(Clone)]
pub(super) struct IoEngine {
    tx: mpsc::Sender<IoRequest>,
}

impl IoEngine {
    /// 发一次 request，等回执（带超时）。供 ctx:request 的 async fn 调用。
    #[allow(dead_code)] // 脚手架：Task 8 ctx:request 调用
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
#[allow(dead_code)] // 脚手架：Task 6 run 调用
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
#[allow(dead_code)] // 仅 test build 使用；lib build 不构造
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
