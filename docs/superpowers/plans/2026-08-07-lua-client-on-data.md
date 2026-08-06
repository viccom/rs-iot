# Lua 脚本采集 — on_data 被动帧（M2 欠账补全）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development 实现本计划。步骤用 checkbox（`- [ ]`）。由 `/loop` 调度（CronCreate `116fd1e9`，每 10m 一轮）。

**Goal:** 补齐 luaClient 的被动帧能力——IO task 在「无在飞 request」时主动收帧 → 经独立 channel → 主循环 `Evt::Data` → 脚本 `on_data(frame)`。覆盖广播/推送协议（设备主动上报，不一问一答）。属 spec §附 M2「IO 引擎 + on_data 触发」的欠账（MVP 只实现 request-response）。

**Architecture:** 守 spec §8 模式 Z（单 IO task 独占连接）。IO task 改 `loop { select!{ biased; req=io_rx.recv() → request-response / recv_idle() → on_data_tx } }`——request 优先（biased），无在飞 request 时 `recv_idle` 主动 read 一帧（idle 成帧），推 `on_data_tx`。主循环 `select!` 加 `data_rx.recv()` 分支 → `call_on_data`。`on_data` 与 `collect` 可共存（spec §6）。MVP 已有的 ctx 全局/冒号 `_self`/create_async_function 约定延续。

**Tech Stack:** tokio `select!`（biased 优先 request）、`mpsc`（on_data channel）、mlua `LuaString`（frame 字节串）。

**执行顺序：** Task 1（io_engine）→ Task 2（mod.rs）→ Task 3（集成测）。Task 1 改 spawn_io_task 签名（加 on_data_tx），Task 2 改 mod.rs 调用 + 主循环。

**已知简化（注明，非占位）：**
- 混合协议（collect + on_data 共存）下，request 到来 cancel 正在进行的 `recv_idle`，可能丢「已读但未成帧」的广播片段（TCP 内核 buffer 已取走）。纯被动协议（period=0，无 request）不受影响——这是 spec §8 单流单 task 的固有限制，v1 接受 + 审计日志兜底。
- `recv_idle` idle 超时返回 None → IO task 回 loop 重 select!（recv_idle 重启 read）。无连接时 recv_idle 返回 None（ensure_connected 失败），IO task 50ms 级轮询（轻量，非忙循环）。

---

## File Structure

| 文件 | 动作 | 职责 |
|---|---|---|
| `src/plugin/builtin/lua_client/io_engine.rs` | Modify | IoBackend 加 `recv_idle`（默认 None）+ TcpIo `recv_idle`（主动 read）+ spawn_io_task 加 on_data_tx + IO task select! |
| `src/plugin/builtin/lua_client/mod.rs` | Modify | 建 on_data channel + spawn_io_task 传 data_tx + 主循环 select! 加 data_rx 分支 + `call_on_data` |

---

## Task 1: io_engine on_data 改造（IoBackend::recv_idle + spawn_io_task select!）

**Files:**
- Modify: `src/plugin/builtin/lua_client/io_engine.rs`

### - [ ] Step 1：IoBackend 加 recv_idle 默认方法

`IoBackend` trait（约 line 17-21）加默认方法 `recv_idle`：

```rust
/// IO 后端 trait：IO task 持有一个，处理 request + 被动帧。
#[async_trait::async_trait]
pub(super) trait IoBackend: Send + 'static {
    /// 处理一次 request：发 frame、收响应、返回字节（或 Err）。
    async fn request(&mut self, frame: &[u8]) -> mlua::Result<Vec<u8>>;

    /// 无在飞 request 时主动收一帧（广播/推送场景）。默认 `None`（不支持被动帧）；
    /// `TcpIo` 覆盖为 idle 成帧 read。`Some(frame)` 推 `on_data`；`None` = idle 超时/未连接。
    async fn recv_idle(&mut self) -> Option<Vec<u8>> {
        None
    }
}
```

### - [ ] Step 2：TcpIo 实现 recv_idle

在 `impl IoBackend for TcpIo`（request 方法后）加 `recv_idle`：

```rust
    /// 无在飞 request 时主动收一帧（idle 成帧）：ensure_connected + read 累积，
    /// idle 静默返回 `Some(buf)`；idle 超时无数据 / 未连接 / 对端关闭返回 `None`。
    ///
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
```

### - [ ] Step 3：spawn_io_task 加 on_data_tx + IO task select!

`spawn_io_task`（约 line 51-60）整体替换：

```rust
/// 启动 IO task（独占 backend）。`on_data_tx` 接被动帧（无在飞 request 时 recv_idle 收到）。
/// 返回 IoEngine 句柄（Lua 侧 request 用）。
pub(super) fn spawn_io_task(
    mut backend: Box<dyn IoBackend>,
    on_data_tx: mpsc::Sender<Vec<u8>>,
) -> IoEngine {
    let (tx, mut rx) = mpsc::channel::<IoRequest>(64);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased; // request 优先：有 request 时立即处理，不被 recv_idle 阻塞
                req = rx.recv() => match req {
                    Some(req) => {
                        let r = backend.request(&req.frame).await;
                        let _ = req.reply_to.send(r);
                    }
                    None => break, // 所有 IoEngine 句柄 drop → 退出
                },
                res = backend.recv_idle() => {
                    if let Some(frame) = res {
                        let _ = on_data_tx.send(frame).await; // 主循环 drop → 忽略
                    }
                    // None（idle 无数据/未连接）→ 回 loop 重 select!（recv_idle 重启）
                }
            }
        }
    });
    IoEngine { tx }
}
```

### - [ ] Step 4：更新既有 io_engine 测试（spawn_io_task 新签名）

既有 4 个测试（`mock_roundtrip` / `timeout_when_no_backend_processing` / `tcp_roundtrip` / `tcp_reconnects_after_drop`）调用 `spawn_io_task(Box::new(backend))`——新签名要 `on_data_tx`。在测试模块顶部加 helper + 改调用：

测试模块内（`use super::*;` 后）加：
```rust
    /// 测试用：spawn_io_task 配一个丢弃被动帧的 on_data channel（单测不测 on_data）。
    fn spawn_no_on_data(backend: Box<dyn IoBackend>) -> IoEngine {
        let (tx, _rx) = mpsc::channel::<Vec<u8>>(8);
        spawn_io_task(backend, tx)
    }
```

把 `mock_roundtrip` 的 `spawn_io_task(Box::new(mock))` 改 `spawn_no_on_data(Box::new(mock))`。（`timeout_when_no_backend_processing` 不调 spawn_io_task，不改；`tcp_roundtrip`/`tcp_reconnects_after_drop` 用 `TcpIo::new` 直接测 backend，不经 spawn_io_task，不改。）

### - [ ] Step 5：加 on_data 单测（TcpIo recv_idle + IO task 推帧）

测试模块末尾（`tcp_reconnects_after_drop` 后、`}` mod 前）加：

```rust
    /// on_data：mock server 无请求时主动推一帧 → IO task recv_idle 收到 → on_data channel。
    #[tokio::test]
    async fn on_data_frame_from_idle_server() {
        use std::time::Duration;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // server：accept 后 sleep 100ms（让 client 连上 + IO task 进 recv_idle）→ 主动推一帧。
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let (mut s, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = s.write_all(b"\x02\xbroadcast").await;
            tokio::time::sleep(Duration::from_millis(200)).await; // 保连接不断
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let backend = Box::new(TcpIo::new(format!("127.0.0.1:{port}"), 50));
        let (on_data_tx, mut on_data_rx) = mpsc::channel::<Vec<u8>>(8);
        let _engine = spawn_io_task(backend, on_data_tx);

        let frame = tokio::time::timeout(Duration::from_secs(2), on_data_rx.recv())
            .await
            .expect("超时")
            .expect("应收到被动帧");
        assert_eq!(frame, b"\x02\xbroadcast");
    }
```

### - [ ] Step 6：run 测试 + fmt + clippy + 提交

```bash
CARGO_BUILD_JOBS=1 cargo test --lib plugin::builtin::lua_client::io_engine
cargo fmt --all --check && CARGO_BUILD_JOBS=1 cargo clippy --all-targets -- -D warnings
git add src/plugin/builtin/lua_client/io_engine.rs
git commit -m "feat(lua): on_data IO 改造（recv_idle + spawn_io_task select! + on_data_tx）"
```
Expected: io_engine 5 测试绿（mock/timeout/tcp_roundtrip/tcp_reconnects/on_data）。

---

## Task 2: mod.rs 主循环 data_rx + call_on_data

**Files:**
- Modify: `src/plugin/builtin/lua_client/mod.rs`

### - [ ] Step 1：建 on_data channel + spawn_io_task 传 data_tx

mod.rs 的 backend 选择块后（`let io_engine = spawn_io_task(backend);`，约 line 88）改为：

```rust
        // 3. IO 引擎 + on_data 被动帧 channel（IO task 无在飞 request 时收到的帧 → 主循环）。
        let (data_tx, mut data_rx) = mpsc::channel::<Vec<u8>>(64);
        let io_engine = spawn_io_task(backend, data_tx);
```

（`use tokio::sync::mpsc;` 已在 mod.rs 顶部。）

### - [ ] Step 2：主循环 select! 加 data_rx 分支

主循环 `select!`（约 line 117-155）加 `data_rx.recv()` 分支（token/cmd/ticker 之间或之后）：

```rust
                msg = cmd_rx.recv() => {
                    if let Some(m) = msg {
                        if m.cmd == "write" {
                            if let Err(e) = call_on_write(&lua, &m.data).await {
                                tracing::warn!("[luaClient] on_write 失败：{e}");
                            }
                        }
                    }
                }
                // on_data 被动帧（IO task 无在飞 request 时收到 → 脚本 on_data(frame)）。
                Some(frame) = data_rx.recv() => {
                    if let Err(e) = call_on_data(&lua, &frame).await {
                        tracing::warn!("[luaClient] on_data 失败：{e}");
                    }
                }
                _ = ticker.tick() => {
```

（`Some(frame) = data_rx.recv()` pattern：data_rx 关闭（None）时不匹配该分支，不退出主循环——与 cmd_rx 的 None-break 语义不同，因 data_tx 在 IO task，IO task 退出时 data_rx None 但主循环可能仍收 cmd/tick。）

### - [ ] Step 3：加 call_on_data helper

在 `call_on_write` 后（约 line 210）加：

```rust
/// 调脚本 on_data(frame)。frame 是 IO task 被动收到的字节串。脚本未定义 on_data 则忽略。
async fn call_on_data(lua: &mlua::Lua, frame: &[u8]) -> mlua::Result<()> {
    let f: mlua::Function = match lua.globals().get("on_data") {
        Ok(f) => f,
        Err(_) => return Ok(()),
    };
    let frame_str = lua.create_string(frame)?;
    let _: mlua::Value = f.call_async(frame_str).await?;
    Ok(())
}

### - [ ] Step 4：run 全量 lib + lua_modbus_e2e + fmt + clippy + 提交

```bash
CARGO_BUILD_JOBS=1 cargo test --lib
CARGO_BUILD_JOBS=1 cargo test --test lua_modbus_e2e   # 确保主循环改造不破坏既有端到端
cargo fmt --all --check && CARGO_BUILD_JOBS=1 cargo clippy --all-targets -- -D warnings
git add src/plugin/builtin/lua_client/mod.rs
git commit -m "feat(lua): on_data 主循环 data_rx + call_on_data（被动帧触发脚本）"
```
Expected: lib 全绿 + lua_modbus_e2e 1 passed（mock server 在线，collect 闭环不变；data_rx 在 mock 无主动推帧时空闲）。

---

## Task 3: on_data 集成测（mock server 推帧 → on_data → rtdata）

**Files:**
- Create: `tests/lua_on_data_e2e.rs`

### - [ ] Step 1：写集成测

新建 `tests/lua_on_data_e2e.rs`（仿 `lua_modbus_e2e.rs` 的 InstanceManager 模式）。脚本 on_data 内 `ctx:set_value` 上报：

```rust
//! on_data 端到端：mock server 主动推帧 → IO task recv_idle → 主循环 on_data(frame)
//! → 脚本 ctx:set_value 上报 → rtdata。验证 M2 欠账补全的被动帧链路。

mod common;

use common::test_data_dir;
use rs_iot::host::{BridgeRegistry, CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::plugin::traits::Processor;
use rs_iot::transform::passthrough::Passthrough;
use rs_iot::types::{AppType, DeviceSpec, HostKind, Hwid, InstConfig, PluginName, Sample};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn on_data_pushes_to_rtdata() {
    let data_dir = test_data_dir("lua-ondata");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client).await.expect("bootstrap");

    // 脚本：period=0（纯被动，不 collect）+ on_data 把帧首字节转 number 上报 heartbeat。
    let scripts = std::env::temp_dir().join(format!("lua-ondata-{}", std::process::id()));
    std::fs::create_dir_all(scripts.join("ond")).expect("mkdir");
    std::fs::write(
        scripts.join("ond/main.lua"),
        "function on_data(frame)\n  ctx:set_value('heartbeat', frame:byte(1))\nend\n",
    )
    .expect("write script");

    // mock server：accept 后 sleep 200ms（等 client 连 + IO task 进 recv_idle）→ 推帧 0x2A=42。
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.expect("accept");
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = s.write_all(&[0x2A]).await;
        tokio::time::sleep(Duration::from_secs(2)).await; // 保连接
    });

    let shutdown = CancellationToken::new();
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown: shutdown.clone(),
        ctl_registry: CtlRegistry::new(),
        bridge_registry: BridgeRegistry::new(),
    };
    let (hub_tx, hub_rx) = tokio::sync::mpsc::channel::<Sample>(64);
    let mgr = Arc::new(InstanceManager::new(ctx.clone(), hub_tx));
    let proc: Arc<dyn Processor> = Arc::new(Passthrough);
    let pt = shutdown.child_token();
    let _pipeline = tokio::spawn(rs_iot::pipeline::run(ctx, hub_rx, proc, pt));

    let inst = InstConfig {
        inst_id: "lua-ond".into(),
        inst_name: "ond".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: true,
        app_config: json!({
            "script": "ond",
            "kind": "tcp",
            "endpoint": format!("127.0.0.1:{port}"),
            "period": 0,
            "recv_idle_ms": 80,
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "devOnd".into(),
            children: vec![],
        }],
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start lua");

    // 等 server 推帧 + on_data + pipeline 落库。
    tokio::time::sleep(Duration::from_millis(1200)).await;

    let v = hget_cell(&client, "rtdata:devOnd", "devOnd.heartbeat").await;
    assert!(v.contains("42"), "on_data 帧首字节 0x2A=42 应上报 heartbeat，得到 '{v}'");

    shutdown.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
    let _ = std::fs::remove_dir_all(&scripts);
}

async fn hget_cell(client: &lux::EmbeddedClient, key: &str, field: &str) -> String {
    use lux::EmbeddedValue;
    match client.execute_value("HGET", &[key, field]).await {
        Ok(EmbeddedValue::Bulk(b)) => String::from_utf8_lossy(&b).to_string(),
        Ok(EmbeddedValue::Simple(s)) => s,
        _ => String::new(),
    }
}
```

> **注：** 脚本用 `ctx:set_value('heartbeat', ...)`——`set_value` 是 spec §7 L2 API（写自己 devid），但 **MVP 未实现 set_value**（Plan 1 只 L1 request + L6 log/now/time）。本测依赖 set_value → 需 Plan 4 附带实现 set_value，或改脚本用 `collect` 返回（但 period=0 不 collect）。**实现决策**：Task 3 前先实现 `ctx:set_value`（简单：转 Sample 推 hub，mod.rs 内）。或在 Task 3 Step 1 前插一个 mini-task 实现 set_value。**推荐**：Task 3 改脚本不依赖 set_value——on_data 内用 `ctx:kv_set('heartbeat', frame:byte(1))`（已实现，Plan 2 Task 2），验 lux `luakv:<inst>` heartbeat==42（绕过 set_value + rtdata）。改断言为 HGET `luakv:lua-ond` heartbeat。

### - [ ] Step 2（采纳推荐）：改脚本用 kv_set + 验 luakv

按上述注，脚本改为：
```lua
function on_data(frame)
  ctx:kv_set('heartbeat', frame:byte(1))
end
```
断言改为：
```rust
    let v = hget_cell(&client, "luakv:lua-ond", "heartbeat").await;
    assert!(v.contains("42"), "on_data kv_set heartbeat=42，得到 '{v}'");
```

### - [ ] Step 3：run + fmt + clippy + 提交

```bash
CARGO_BUILD_JOBS=1 cargo test --test lua_on_data_e2e
cargo fmt --all --check && CARGO_BUILD_JOBS=1 cargo clippy --all-targets -- -D warnings
git add tests/lua_on_data_e2e.rs
git commit -m "test(lua): on_data 集成测（mock server 推帧 → on_data kv_set 端到端）"
```

---

## Self-Review

**1. Spec coverage（on_data = M2 欠账）：**
- ✅ IO task 无在飞 request 时主动收帧 → on_data channel（spec §8 line 281/295）→ Task 1
- ✅ 主循环 `data_rx.recv()` → `on_data(frame)`（spec §5.4 line 136/142）→ Task 2
- ✅ `on_data(frame)` 脚本回调（spec §6 line 169）→ Task 2 call_on_data
- ✅ collect 与 on_data 可共存（select! biased request 优先）→ Task 1/2

**2. Placeholder scan：** 无 TBD/TODO；Task 2 Step 3 call_on_data 笔误已标注修正；Task 3 set_value 依赖已给决策（改 kv_set 绕过）。

**3. Type consistency：** `IoBackend::recv_idle() -> Option<Vec<u8>>`（默认 None，TcpIo 覆盖）；`spawn_io_task(backend, on_data_tx: mpsc::Sender<Vec<u8>>)`；`call_on_data(lua, frame: &[u8])`；frame 经 `lua.create_string` → LuaString 传脚本。

**已知简化（非占位）：** 混合协议 request cancel recv_idle 丢部分广播片段（v1 接受）；recv_idle 未连接时 None 轮询（轻量）；set_value（L2）未实现，Task 3 用 kv_set 绕过（set_value 留后续 plan）。

---

## Execution Handoff

本计划由 `/loop`（CronCreate `116fd1e9`）驱动，每轮按 subagent-driven-development 执行下一个未勾选 task。CI 门槛（fmt + clippy -D warnings + test --all-targets --all-features，Windows CARGO_BUILD_JOBS=1）。API 503 间歇不稳时控制器接手。Task 1-3 完成 → `finishing-a-development-branch` 合并 → 写 Plan 5（M6 热加载）plan。

建分支：`feature/lua-on-data`（从 master `3de52f4`）。
