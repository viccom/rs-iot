# Lua 脚本采集 — M8a serial/udp 传输实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development 实现本计划。步骤用 checkbox（`- [ ]`）。由 `/loop` 调度（CronCreate `39632fab`，每 10m 一轮）。

**Goal:** 补齐 luaClient 的 serial / udp IO 传输——`io_engine` 新增 `SerialIo`（tokio-serial）/ `UdpIo`（connected UdpSocket）两个 `IoBackend` 实现，`mod.rs` backend 选择接 Serial/UDP（去 `MockIoTask` 占位），`config.rs` 加 serial 参数（baud/data/parity/stop/flow）。spec §8（IO 引擎三传输）+ §16（serial/udp）。

**Architecture:** `SerialIo`/`UdpIo` 同构 `TcpIo`（`Option<stream>` 懒连接 + `ensure_connected` + `request` write/read + `recv_idle` 被动帧），实现 `IoBackend` trait。三传输统一 `IoBackend`（IO task 独占 + oneshot 回执 + timeout，spec §8 模式 Z 不变）。`mod.rs` line 92-104 的 backend 选择从「Serial/Udp → MockIoTask 占位」改为「Serial → SerialIo，Udp → UdpIo」。serial 配置对标 modbus_client（tokio_serial::new + data_bits/parity/stop_bits/flow_control → open_native_async）。

**Tech Stack:** tokio-serial 5.5（`SerialStream` impl AsyncRead+AsyncWrite，modbus_client 已引）、tokio UdpSocket（connected，datagram 边界）、tokio io AsyncReadExt/AsyncWriteExt。

**执行顺序：** Task 1（config serial 参数）→ Task 2（SerialIo）→ Task 3（UdpIo）→ Task 4（mod.rs 接 backend）→ Task 5（udp 集成测）。

**已知简化 / 风险（注明）：**
- **SerialIo 单测 `#[ignore]`**：serial 需真机/虚拟串口（COM*/dev/tty*），CI 不跑。单测仅编译验证 + 标 ignore（手动跑需配串口）。serial reopen 复杂（设备拔插），v1 `ensure_connected` err 报不重连（对标 TcpIo 但 serial 重连语义弱——下次 request 仍尝试 open，失败报）。
- **UdpIo datagram 边界**：一 send 一 recv（天然保帧，无 idle 成帧）。`request` send frame → recv 一个 datagram（整包响应）；`recv_idle` recv 被动 datagram。不像 tcp/serial 需 idle 累积。
- **SerialIo/udp 的 read 累积 vs datagram**：serial 流式（同 tcp，idle 成帧 read 累积）；udp datagram（一次 recv = 整包，不累积）。
- **recv_mode 4 种（length/delimiter/fixed）后置**：v1 仅 `Timeout`（idle 累积，serial/tcp）+ udp datagram。spec §8 其他 recv_mode 留 v2。
- **mod.rs backend 选择去 MockIoTask 占位后**：MockIoTask 仍保留（测试用，api/io_engine 单测依赖）。

---

## File Structure

| 文件 | 动作 | 职责 |
|---|---|---|
| `src/plugin/builtin/lua_client/config.rs` | Modify | `LuaCfg` 加 serial 参数（baud_rate/data_bits/parity/stop_bits/flow_control）+ `Parity`/`FlowControl` enum + parse + 单测 |
| `src/plugin/builtin/lua_client/io_engine.rs` | Modify | 加 `SerialIo`（tokio-serial）+ `UdpIo`（connected UdpSocket）两个 `IoBackend` impl + UdpIo 单测（SerialIo `#[ignore]`） |
| `src/plugin/builtin/lua_client/mod.rs` | Modify | backend 选择 Serial → SerialIo，Udp → UdpIo（去 MockIoTask 占位） |
| `tests/lua_udp_e2e.rs` | **Create** | udp 端到端（mock udp server + luaClient Udp 实例 + collect → rtdata） |

---

## Task 1: config.rs serial 参数

**Files:** Modify `src/plugin/builtin/lua_client/config.rs`

### - [x] Step 1：先加失败单测（TDD）

`config.rs` 的 `#[cfg(test)] mod tests` 内加：
```rust
    #[test]
    fn parse_serial_params() {
        let cfg = parse_cfg(&json!({
            "script":"a","kind":"serial","serial_port":"COM3",
            "baud_rate":19200,"data_bits":7,"parity":"even","stop_bits":2,"flow_control":"hardware"
        })).unwrap();
        assert_eq!(cfg.transport, Transport::Serial);
        assert_eq!(cfg.serial_port.as_deref(), Some("COM3"));
        assert_eq!(cfg.baud_rate, 19200);
        assert_eq!(cfg.data_bits, 7);
        assert_eq!(cfg.parity, Parity::Even);
        assert_eq!(cfg.stop_bits, 2);
        assert_eq!(cfg.flow_control, FlowControl::Hardware);
    }

    #[test]
    fn parse_serial_defaults() {
        // 缺省：9600/8N1/NONE（对标 modbus_client）。
        let cfg = parse_cfg(&json!({"script":"a","kind":"serial","serial_port":"COM3"})).unwrap();
        assert_eq!(cfg.baud_rate, 9600);
        assert_eq!(cfg.data_bits, 8);
        assert_eq!(cfg.parity, Parity::None);
        assert_eq!(cfg.stop_bits, 1);
        assert_eq!(cfg.flow_control, FlowControl::None);
    }

    #[test]
    fn parse_bad_parity_rejected() {
        assert!(parse_cfg(&json!({"script":"a","kind":"serial","serial_port":"COM3","parity":"x"})).is_err());
    }
```

### - [x] Step 2：跑测确认失败

```bash
CARGO_BUILD_JOBS=1 cargo test --lib plugin::builtin::lua_client::config 2>&1 | tail -8
```
Expected: 编译失败（`baud_rate`/`Parity` 等未定义）。

### - [x] Step 3：加 enum + LuaCfg 字段 + parse

`config.rs` 顶部（`RecvMode` enum 后）加 serial 枚举：
```rust
/// 串口校验（对标 modbus_client `ParityKind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Parity {
    /// 无校验。
    None,
    /// 偶校验。
    Even,
    /// 奇校验。
    Odd,
}

/// 串口流控（对标 modbus_client `FlowKind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FlowControl {
    /// 无流控。
    None,
    /// 软件流控（XON/XOFF）。
    Software,
    /// 硬件流控（RTS/CTS）。
    Hardware,
}
```

`LuaCfg` struct（`serial_port` 字段后）加：
```rust
    /// serial 波特率（默认 9600）。
    pub(super) baud_rate: u32,
    /// serial 数据位（5/6/7/8，默认 8）。
    pub(super) data_bits: u8,
    /// serial 校验（默认 None）。
    pub(super) parity: Parity,
    /// serial 停止位（1/2，默认 1）。
    pub(super) stop_bits: u8,
    /// serial 流控（默认 None）。
    pub(super) flow_control: FlowControl,
```

`parse_cfg`（serial_port 解析的 `Transport::Serial =>` 分支内，`sp` 取得后）加 serial 参数解析。原分支：
```rust
        Transport::Serial => {
            let sp = app
                .get("serial_port")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "serial 需要 serial_port".to_string())?
                .to_string();
            (String::new(), Some(sp))
        }
```
改为（在闭包外提 serial 参数，因 baud_rate 等也属 cfg 顶层字段——放 parse_cfg 主体，非 transport 分支）：

> **设计：** `baud_rate` 等是 `LuaCfg` 顶层字段（tcp/udp 时取缺省，serial 时用配置）。parse 在 `parse_cfg` 主体（统一），不嵌 transport 分支。`endpoint/serial_port` 的 match 保留（tcp/udp 要 endpoint，serial 要 serial_port）。

`parse_cfg` 在 `let (endpoint, serial_port) = match transport {...};` 后加：
```rust
    let baud_rate = app.get("baud_rate").and_then(|v| v.as_u64()).unwrap_or(9600) as u32;
    let data_bits = app.get("data_bits").and_then(|v| v.as_u64()).unwrap_or(8) as u8;
    let stop_bits = app.get("stop_bits").and_then(|v| v.as_u64()).unwrap_or(1) as u8;
    let parity = match app.get("parity").and_then(|v| v.as_str()).unwrap_or("none") {
        "none" => Parity::None,
        "even" => Parity::Even,
        "odd" => Parity::Odd,
        other => return Err(format!("unsupported parity '{other}'（none/even/odd）")),
    };
    let flow_control = match app
        .get("flow_control")
        .and_then(|v| v.as_str())
        .unwrap_or("none")
    {
        "none" => FlowControl::None,
        "software" => FlowControl::Software,
        "hardware" => FlowControl::Hardware,
        other => return Err(format!("unsupported flow_control '{other}'（none/software/hardware）")),
    };
```

`Ok(LuaCfg { ... })` 构造（`serial_port,` 后）加：
```rust
        baud_rate,
        data_bits,
        parity,
        stop_bits,
        flow_control,
```

### - [x] Step 4：跑测通过 + fmt + clippy + 提交

```bash
CARGO_BUILD_JOBS=1 cargo test --lib plugin::builtin::lua_client::config
cargo fmt --all && cargo fmt --all --check && CARGO_BUILD_JOBS=1 cargo clippy --all-targets -- -D warnings
git add src/plugin/builtin/lua_client/config.rs
git commit -m "feat(lua): M8a config serial 参数（baud/data/parity/stop/flow，对标 modbus）"
```
Expected: config 全测试绿（含新 3 serial 单测）。

---

## Task 2: io_engine.rs SerialIo backend

**Files:** Modify `src/plugin/builtin/lua_client/io_engine.rs`

### - [x] Step 1：SerialIo struct + ensure_connected

`io_engine.rs`（`TcpIo` impl 块后、`#[cfg(test)]` 前）加：
```rust
// ===== SerialIo（serial backend：tokio-serial，懒 open + err 置 None 重 open）=====

/// serial IO backend：独占一条 `tokio_serial::SerialStream`，懒 open + err 置 None。
///
/// 对标 `TcpIo` 模式（spec §8 不变式：连接生命周期由 IO task 管）。serial 无"对端关闭"，
/// read/write err（设备拔插/线缆）→ 置 None，下次 request 重 open（reopen 失败报错）。
/// read 用 idle 累积成帧（同 tcp，串口流式）；recv_idle 读被动帧。
pub(super) struct SerialIo {
    /// 当前串口流（None = 未 open/err，下次 request 前 ensure_connected 重 open）。
    stream: Option<tokio_serial::SerialStream>,
    port: String,
    baud_rate: u32,
    data_bits: u8,
    parity: super::config::Parity,
    stop_bits: u8,
    flow_control: super::config::FlowControl,
    idle: std::time::Duration,
}

impl SerialIo {
    /// 构造（不立即 open，首次 request 才 open）。
    pub(super) fn new(
        port: String,
        baud_rate: u32,
        data_bits: u8,
        parity: super::config::Parity,
        stop_bits: u8,
        flow_control: super::config::FlowControl,
        recv_idle_ms: u64,
    ) -> Self {
        Self {
            stream: None,
            port,
            baud_rate,
            data_bits,
            parity,
            stop_bits,
            flow_control,
            idle: std::time::Duration::from_millis(recv_idle_ms),
        }
    }

    /// 确保串口已 open；None 则 open（失败返回 Err）。
    async fn ensure_connected(&mut self) -> mlua::Result<()> {
        if self.stream.is_some() {
            return Ok(());
        }
        let builder = tokio_serial::new(&self.port, self.baud_rate)
            .data_bits(map_data_bits(self.data_bits))
            .parity(map_parity(self.parity))
            .stop_bits(map_stop_bits(self.stop_bits))
            .flow_control(map_flow(self.flow_control));
        match builder.open_native_async() {
            Ok(s) => {
                self.stream = Some(s);
                Ok(())
            }
            Err(e) => Err(mlua::Error::RuntimeError(format!(
                "serial open {}: {e}",
                self.port
            ))),
        }
    }
}

/// `data_bits`(u8) → tokio_serial `DataBits`。
fn map_data_bits(b: u8) -> tokio_serial::DataBits {
    match b {
        5 => tokio_serial::DataBits::Five,
        6 => tokio_serial::DataBits::Six,
        7 => tokio_serial::DataBits::Seven,
        _ => tokio_serial::DataBits::Eight,
    }
}

/// `Parity` → tokio_serial `Parity`。
fn map_parity(p: super::config::Parity) -> tokio_serial::Parity {
    match p {
        super::config::Parity::None => tokio_serial::Parity::None,
        super::config::Parity::Even => tokio_serial::Parity::Even,
        super::config::Parity::Odd => tokio_serial::Parity::Odd,
    }
}

/// `stop_bits`(u8) → tokio_serial `StopBits`。
fn map_stop_bits(b: u8) -> tokio_serial::StopBits {
    match b {
        2 => tokio_serial::StopBits::Two,
        _ => tokio_serial::StopBits::One,
    }
}

/// `FlowControl` → tokio_serial `FlowControl`。
fn map_flow(f: super::config::FlowControl) -> tokio_serial::FlowControl {
    match f {
        super::config::FlowControl::None => tokio_serial::FlowControl::None,
        super::config::FlowControl::Software => tokio_serial::FlowControl::Software,
        super::config::FlowControl::Hardware => tokio_serial::FlowControl::Hardware,
    }
}
```

### - [x] Step 2：SerialIo impl IoBackend（request + recv_idle）

紧接 Step 1 后加：
```rust
#[async_trait::async_trait]
impl IoBackend for SerialIo {
    async fn request(&mut self, frame: &[u8]) -> mlua::Result<Vec<u8>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        self.ensure_connected().await?;
        if let Err(e) = self
            .stream
            .as_mut()
            .expect("ensure_connected 后必有 stream")
            .write_all(frame)
            .await
        {
            self.stream = None;
            return Err(mlua::Error::RuntimeError(format!("serial write: {e}")));
        }
        // read（idle 累积成帧，同 TcpIo）：每次迭代重新取 stream 引用。
        let mut buf = Vec::new();
        let mut tmp = [0u8; 256];
        let mut io_err: Option<std::io::Error> = None;
        loop {
            let stream = self.stream.as_mut().expect("read 期间 stream 必在");
            match tokio::time::timeout(self.idle, stream.read(&mut tmp)).await {
                Ok(Ok(0)) => break,       // serial 无"对端关闭"，Ok(0) 视为本次无数据
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
            return Err(mlua::Error::RuntimeError(format!("serial read: {e}")));
        }
        if buf.is_empty() {
            return Err(mlua::Error::RuntimeError("serial: 无响应".into()));
        }
        Ok(buf)
    }

    /// 无在飞 request 时主动收一帧（idle 累积），同 TcpIo::recv_idle。
    async fn recv_idle(&mut self) -> Option<Vec<u8>> {
        use tokio::io::AsyncReadExt;
        self.ensure_connected().await.ok()?;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 256];
        loop {
            let stream = self.stream.as_mut()?;
            match tokio::time::timeout(self.idle, stream.read(&mut tmp)).await {
                Ok(Ok(0)) => break,
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
```

### - [x] Step 3：SerialIo 单测（`#[ignore]`，需真串口）

`#[cfg(test)] mod tests` 内加（serial 需真机/虚拟串口，CI 跳过）：
```rust
    /// serial 闭环：需真机/虚拟串口（如 socatPTY）。CI 标 ignore；手动跑配 `serial_port`。
    #[tokio::test]
    #[ignore = "serial 需真机/虚拟串口（配置 SERIAL_PORT_A 环境变量）"]
    async fn serial_roundtrip() {
        let port = std::env::var("SERIAL_PORT_A").expect("设 SERIAL_PORT_A");
        let mut io = SerialIo::new(port, 9600, 8, super::super::config::Parity::None, 1, super::super::config::FlowControl::None, 50);
        // write 一帧，read 响应（需对端回显设备）。
        let r = io.request(b"\x01\x02").await;
        assert!(r.is_ok(), "serial request: {r:?}");
    }
```

### - [x] Step 4：编译 + fmt + clippy + 提交

```bash
CARGO_BUILD_JOBS=1 cargo build --lib
cargo fmt --all && cargo fmt --all --check && CARGO_BUILD_JOBS=1 cargo clippy --all-targets -- -D warnings
git add src/plugin/builtin/lua_client/io_engine.rs
git commit -m "feat(lua): M8a SerialIo backend（tokio-serial，IoBackend impl）"
```
Expected: 编译过（SerialIo 未在 mod.rs 用，可能 dead_code 警告——Task 4 接入后活；若 clippy 报 dead_code，Task 4 前用 `#[expect(dead_code)]` 临时标，Task 4 移除）。serial 单测 ignore 不跑。

> **dead_code 处理：** SerialIo 在 Task 4 才被 mod.rs 引用。Task 2 单独编译时 SerialIo::new/impl 未用 → clippy dead_code。两种处理：① Task 2+3+4 合并提交（避免中间 dead）；② Task 2 加 `#[expect(dead_code)]`（Task 4 移除）。推荐 ①（Task 2/3/4 同分支连续做，最后一起 clippy）。但为"每 task 一提交"，用 ②（临时 expect，Task 4 移除）。实现时按 ②：SerialIo struct 加 `#[expect(dead_code)]`，Task 4 接入后删。

---

## Task 3: io_engine.rs UdpIo backend

**Files:** Modify `src/plugin/builtin/lua_client/io_engine.rs`

### - [x] Step 1：UdpIo struct + ensure_connected + impl IoBackend

`io_engine.rs`（`SerialIo` impl 块后）加：
```rust
// ===== UdpIo（udp backend：connected UdpSocket，datagram 边界天然保帧）=====

/// UDP IO backend：connected `UdpSocket`，懒 connect + err 置 None。
///
/// UDP datagram 边界天然保帧（一 send 一 recv = 整包响应），无 tcp/serial 的 idle 累积。
/// `request`：send frame → recv 一个 datagram（整包响应）。`recv_idle`：recv 被动 datagram。
/// connected socket（bind 0.0.0.0:0 + connect endpoint），对标 modbus rtu_udp（spec §8）。
pub(super) struct UdpIo {
    /// 当前 socket（None = 未 connect/err，下次 request 前 ensure_connected 重 connect）。
    sock: Option<tokio::net::UdpSocket>,
    endpoint: String,
    idle: std::time::Duration,
}

impl UdpIo {
    /// 构造（不立即 connect，首次 request 才 connect）。
    pub(super) fn new(endpoint: String, recv_idle_ms: u64) -> Self {
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
        let addr: std::net::SocketAddr = self
            .endpoint
            .parse()
            .map_err(|e| mlua::Error::RuntimeError(format!("udp endpoint {}: {e}", self.endpoint)))?;
        let sock = tokio::net::UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|e| mlua::Error::RuntimeError(format!("udp bind: {e}")))?;
        sock.connect(addr)
            .await
            .map_err(|e| mlua::Error::RuntimeError(format!("udp connect {}: {e}", self.endpoint)))?;
        self.sock = Some(sock);
        Ok(())
    }
}

#[async_trait::async_trait]
impl IoBackend for UdpIo {
    async fn request(&mut self, frame: &[u8]) -> mlua::Result<Vec<u8>> {
        use tokio::io::AsyncWriteExt;
        self.ensure_connected().await?;
        let sock = self
            .sock
            .as_mut()
            .expect("ensure_connected 后必有 sock");
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
```

> **注意 `use tokio::io::AsyncWriteExt`：** UdpSocket 有 `send/recv`（UdpSocket 固有方法，datagram 语义），不需 AsyncWriteExt（那是 stream 的 poll_write）。这里 `send` 是 UdpSocket::send。`use AsyncWriteExt` 误加会 unused import warning——实现时确认只用 `sock.send(...)`/`sock.recv(...)`，删掉 `use AsyncWriteExt`（UdpSocket::send/recv 是 inherent method）。**实现时移除该 use。**

### - [x] Step 2：UdpIo 单测（mock udp server）

`#[cfg(test)] mod tests` 内加：
```rust
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
```

### - [x] Step 3：run udp 单测 + fmt + clippy + 提交

```bash
CARGO_BUILD_JOBS=1 cargo test --lib plugin::builtin::lua_client::io_engine
cargo fmt --all && cargo fmt --all --check && CARGO_BUILD_JOBS=1 cargo clippy --all-targets -- -D warnings
git add src/plugin/builtin/lua_client/io_engine.rs
git commit -m "feat(lua): M8a UdpIo backend（connected UdpSocket，datagram 边界）"
```
Expected: io_engine 全测试绿（含新 udp_roundtrip / udp_bad_endpoint_errors；serial ignore 跳过）。

---

## Task 4: mod.rs 接 Serial/Udp backend

**Files:** Modify `src/plugin/builtin/lua_client/mod.rs`

### - [ ] Step 1：backend 选择接 SerialIo/UdpIo

`mod.rs` 的 backend 选择（line 92-104，IO 引擎构造段）当前：
```rust
        let backend: Box<dyn IoBackend> = match cfg.transport {
            config::Transport::Tcp => Box::new(TcpIo::new(cfg.endpoint.clone(), cfg.recv_idle_ms)),
            _ => {
                tracing::warn!(
                    "[luaClient] inst {} transport {:?} 暂未实现（Plan 1 仅 tcp），用 mock 占位",
                    inst.inst_id,
                    cfg.transport
                );
                Box::new(MockIoTask {
                    responder: Box::new(|_| Ok(vec![])),
                })
            }
        };
```
替换为（Serial → SerialIo，Udp → UdpIo，去 MockIoTask 占位）：
```rust
        let backend: Box<dyn IoBackend> = match cfg.transport {
            config::Transport::Tcp => {
                Box::new(TcpIo::new(cfg.endpoint.clone(), cfg.recv_idle_ms))
            }
            config::Transport::Serial => Box::new(SerialIo::new(
                cfg.serial_port.clone().unwrap_or_default(),
                cfg.baud_rate,
                cfg.data_bits,
                cfg.parity,
                cfg.stop_bits,
                cfg.flow_control,
                cfg.recv_idle_ms,
            )),
            config::Transport::Udp => {
                Box::new(UdpIo::new(cfg.endpoint.clone(), cfg.recv_idle_ms))
            }
        };
```

同时 `use io_engine::{...}`（line 24）加 `SerialIo, UdpIo`：
```rust
use io_engine::{spawn_io_task, IoBackend, MockIoTask, SerialIo, TcpIo, UdpIo};
```

> **MockIoTask 去除：** backend 选择不再用 MockIoTask。但 `MockIoTask` 仍被 `io_engine`/`api` 单测用（`spawn_io_task(Box::new(MockIoTask{...}))`）——保留 `MockIoTask` 在 use（避免 unused import，因 mod.rs run() 不再用，但单测/其他模块用）。**若 mod.rs run() 不再用 MockIoTask，clippy 报 unused import `MockIoTask`**——从 mod.rs use 移除 MockIoTask（io_engine/api 模块各自 use）。实现时按编译反馈：mod.rs use 只留 `spawn_io_task, IoBackend, SerialIo, TcpIo, UdpIo`（去 MockIoTask）。

### - [ ] Step 2：移除 SerialIo/SerialIo 的临时 `#[expect(dead_code)]`

Task 2 若在 SerialIo 加了 `#[expect(dead_code)]`（Task 2 Step 4 注），Task 4 接入后移除（mod.rs 引用 → 不再 dead）。`config::Parity/FlowControl` 同理（Task 4 后 SerialIo 用 → 活）。

### - [ ] Step 3：run lua_client 全单测 + fmt + clippy + 提交

```bash
CARGO_BUILD_JOBS=1 cargo test --lib plugin::builtin::lua_client
cargo fmt --all && cargo fmt --all --check && CARGO_BUILD_JOBS=1 cargo clippy --all-targets -- -D warnings
git add src/plugin/builtin/lua_client/mod.rs src/plugin/builtin/lua_client/io_engine.rs
git commit -m "feat(lua): M8a mod.rs 接 Serial/Udp backend（去 MockIoTask 占位）"
```
Expected: lua_client 全单测绿（collect/on_write/on_data/stat/script_version 不破坏；backend 选择三传输就绪）。

---

## Task 5: lua_udp_e2e 集成测

**Files:** Create `tests/lua_udp_e2e.rs`

### - [ ] Step 1：写集成测

新建 `tests/lua_udp_e2e.rs`（仿 lua_modbus_e2e / lua_stat_e2e 模式，mock udp server + collect 端到端）：
```rust
//! M8a udp 传输端到端：mock udp server（回 Modbus-like 响应）+ luaClient Udp 实例
//! + collect → rtdata 验值。spec §8（三传输）+ §16（udp）。

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
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn udp_collect_writes_rtdata() {
    let data_dir = test_data_dir("lua-udp");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    // mock udp server：收任 frame 回 "[0x00, 0x2A]"（脚本解析 byte(2)=42 作为 v）。
    let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.expect("bind");
    let addr = server.local_addr().expect("addr");
    tokio::spawn(async move {
        let mut buf = [0u8; 64];
        loop {
            let (n, peer) = match server.recv_from(&mut buf).await {
                Ok(x) => x,
                Err(_) => break,
            };
            // 回两字节：0x00 + 0x2A（42）。
            let resp = if n > 0 { vec![0x00, 0x2A] } else { vec![] };
            let _ = server.send_to(&resp, peer).await;
        }
    });

    // 临时脚本：collect 发一帧，解析响应 byte(2) 为 v。
    let scripts = std::env::temp_dir().join(format!("lua-udp-{}", std::process::id()));
    let script_dir = scripts.join("u");
    std::fs::create_dir_all(&script_dir).expect("mkdir");
    std::fs::write(
        script_dir.join("main.lua"),
        "function collect()\n\
         local r = ctx:request(\"\\x01\", 300)\n\
         if not r or #r < 2 then return nil end\n\
         return { v = r:byte(2) }\n\
         end\n",
    )
    .expect("write");

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
        inst_id: "lua-udp".into(),
        inst_name: "u".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: true,
        app_config: json!({
            "script": "u",
            "kind": "udp",
            "endpoint": addr.to_string(),
            "period": 1,
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "devU".into(),
            children: vec![],
        }],
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start lua");

    // 等 collect 周期 → rtdata 落 v=42。
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let v = hget_cell(&client, "rtdata:devU", "devU.v").await;
    assert!(v.contains("42"), "udp collect 应得 v=42，得到 '{v}'");

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

### - [ ] Step 2：run + fmt + clippy + 全量 test + 提交

```bash
CARGO_BUILD_JOBS=1 cargo test --test lua_udp_e2e
cargo fmt --all --check && CARGO_BUILD_JOBS=1 cargo clippy --all-targets -- -D warnings
CARGO_BUILD_JOBS=1 cargo test --all-targets --all-features
git add tests/lua_udp_e2e.rs
git commit -m "test(lua): M8a udp 传输集成测（mock udp server → collect → rtdata）"
```
Expected: lua_udp_e2e 1 passed；全量测试绿（lib + 集成测，含 M7 既有 + M8a 新增）。

---

## Self-Review

**1. Spec coverage（M8a serial/udp 传输）：**
- ✅ serial 传输（tokio-serial，spec §8/§16）→ Task 1（config）+ Task 2（SerialIo）
- ✅ udp 传输（connected socket，spec §8/§16）→ Task 3（UdpIo）
- ✅ 三传输统一 IoBackend（spec §8 模式 Z 不变）→ Task 2/3 impl IoBackend
- ✅ mod.rs backend 选择（去 MockIoTask 占位）→ Task 4
- ✅ udp 端到端（mock server）→ Task 5
- ⚠️ serial 端到端留手动（真机/虚拟串口，CI 不跑）→ Task 2 单测 `#[ignore]`

**2. Placeholder scan：** 无 TBD；SerialIo/UdpIo/config 代码完整；udp 单测 + 集成测完整；serial 单测 ignore 标注（需真机）。`use AsyncWriteExt` 误加标注移除（Task 3 Step 1 注）。

**3. Type consistency：**
- `Parity`/`FlowControl` enum（Task 1 定义）→ Task 2 `SerialIo::new(...parity, ...flow_control)` + `map_parity/map_flow` 一致
- `SerialIo::new(port, baud_rate, data_bits, parity, stop_bits, flow_control, recv_idle_ms)`（Task 2）→ Task 4 mod.rs 调用签名一致
- `UdpIo::new(endpoint, recv_idle_ms)`（Task 3）→ Task 4 mod.rs 调用一致
- `IoBackend` trait（request/recv_idle）→ SerialIo/UdpIo impl 一致（TcpIo 同构）

**已知简化（非占位）：** serial 单测 ignore（需真机）；serial reopen err 不重连（弱语义）；recv_mode 仅 Timeout + udp datagram（length/delimiter/fixed 后置）；MockIoTask 保留（单测用）。

---

## Execution Handoff

本计划由 `/loop`（CronCreate `39632fab`）驱动，每轮 subagent-driven-development 执行下一个未勾选 task。CI 门槛：
```bash
cargo fmt --all --check && CARGO_BUILD_JOBS=1 cargo clippy --all-targets -- -D warnings
CARGO_BUILD_JOBS=1 cargo test --all-targets --all-features
```
（Windows CARGO_BUILD_JOBS=1 规避 link.exe 并行 OOM；serial 单测 ignore 不影响 CI）。API 503 间歇不稳时控制器接手。

Task 1-5 完成 → `finishing-a-development-branch` 合并 master + 删分支 → 写 Plan 8（M8b L2 数据 API + 文档 + 示例）。

建分支：`feature/lua-m8a`（从 master `cc92a9c`）。

> **注：** M8 拆为 Plan 7（M8a serial/udp，本计划）+ Plan 8（M8b L2 API + 文档）。spec 路线图原「Plan 7 = M8 全部」更新为 Plan 7（serial/udp）+ Plan 8（L2/文档）。两者独立可测。Plan 8 完成后 spec 全 milestone 实现 → CronDelete 收尾。
