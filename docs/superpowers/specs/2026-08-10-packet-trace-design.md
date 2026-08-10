# 报文 trace 开关(阶段 1:裸报文主战场)设计

> **状态**:设计(spec)阶段,待 review → writing-plans。
> **日期**:2026-08-10。
> **范围**:阶段 1 —— lua + modbus 两个裸报文协议的 per-instance 运行时报文 trace。
> **关联**:scenario-gap §5(报文输出)与 §8 #3(P1 可观测体系子系统 2);承接 `/metrics`(子系统 1,已完成)。
> **不在范围**:opcua/s7(非裸报文协议)、sidecar(bridge IPC 帧)、GET 拉取端点 + ring buffer(随子系统 3)。

## 1. 背景与动机

工业现场大量通讯协议是**裸报文/原始报文**(Modbus RTU、各类仪表私有串口协议、DL/T 645、CJT188 等)。现场调试人员**高度依赖看原始报文**定位诊断:寄存器映射对不对、字节序(ABCD/CDAB)对不对、从站地址对不对、设备回的什么错——全靠看一去一回的字节流。这比看"解码后的值"有用得多,因为值错了无法区分是"请求就错了"、"设备回错了"还是"解析错了"。

尤其 RS485 总线,物理抓包要专用 USB-RS485 抓包器(现场常没有),**软件内按实例按需抓包是质变能力**。

诉求(scenario-gap §0 诉求④):「rs-iot 可**按需激活通讯报文输出和发送**」。当前现状(scenario-gap §5):南向插件零报文 debug/trace,无 dump 开关,无 per-instance 运行时激活。

本设计交付**阶段 1**:覆盖 lua + modbus 两个裸报文协议主战场。

## 2. 现状(代码核实)

5 个南向插件的"原始字节可获取性"呈三档异构(Explore agent 调研):

| 插件 | 字节可获取性 | 收发集中点 | 阶段 1 |
|---|---|---|---|
| **lua** | ✅ 完全(`Vec<u8>` 进出) | `io_engine/mod.rs:85`(`backend.request`)/`:90`(`recv_idle`) | ✅ 做(direct dump) |
| **modbus** | 🔴 基本不可(只 `Vec<u16>`/`Vec<bool>`) | `Context`(`modbus_client/mod.rs:326`) | ✅ 做(TraceIo wrapper) |
| sidecar | ✅ 完全(bridge 帧) | `sidecar_client/mod.rs:323` | ❌ 后置(IPC 帧语义不同) |
| s7 | 🟡 部分(读 item.data;写无字节) | `s7_client/mod.rs:450` | ❌ 后置(非裸报文) |
| opcua | 🔴 不可(只 Variant,安全层密文) | `opcua_client/mod.rs:336` | ❌ 后置(非裸报文) |

**关键事实**:
- modbus 用 `tokio-modbus` 应用层 API,字节(功能码/MBAP/CRC)在库内部消耗,插件只拿 `Vec<u16>`/`Vec<bool>`(`codec.rs:59-65` `Buf` enum)。但 `Context<T>` 是泛型 over transport(`T: AsyncRead+AsyncWrite+Unpin+Send`),rs-iot 已有自定义 transport 先例(`transport.rs:116-206` `UdpIo` 实现 `AsyncRead+AsyncWrite` 喂给 `rtu::attach_slave`)——这是注入拦截的公开扩展点,**无需 fork 库**。
- lua 的 `io_engine` 是 rs-iot 自写,`IoBackend::request(frame: &[u8]) -> Vec<u8>` 字节原生在手(`io_engine/mod.rs:30-43`)。
- 5 插件**无共享 IO/报文工具层**(`json_to_sample_value` 都 copy-paste 4 份:`modbus_client/mod.rs:119`/`opcua_client/mod.rs:196`/`s7_client/mod.rs:287`/`sidecar_client/mod.rs:128`)。本设计新建共享 `io_trace` 模块填补此缺口。

## 3. 架构

```
                    报文 trace 架构(阶段 1)

   REST POST /api/v1/inst/trace                    lua io_engine
        │                                           (frame 已在手)
        ▼                                                │
   ┌─────────────────────────┐                         │ direct dump
   │  TraceRegistry          │  ← HostContext 字段      │
   │  DashMap<id, TraceState>│                         ▼
   └────────┬────────────────┘                  ┌──────────────┐
            │ get(id) → Arc<TraceState>           │ io_trace::    │
            │   active: AtomicBool 快速路径         │ dump(id,dir,  │
   ┌────────▼────────────┐  拦截 poll_read/write  │ bytes)        │
   │  TraceIo<T>         │ ──────────────────────►│ 共享:格式/    │
   │  (仅 modbus)         │  dump(RX/TX)          │ 开关/超时      │
   └────────┬────────────┘                       └──────┬───────┘
            │                                           │
       TcpStream/                               tracing target
       SerialStream/UdpIo                         "io_trace"
```

**三个组件**:

| 组件 | 职责 | 服务对象 |
|---|---|---|
| **`io_trace` 模块**(新) | `TraceRegistry`(开关+超时)+ `TraceState`+ `dump()`(格式化+输出)+ `TraceIo<T>` | 全部 |
| **`TraceIo<T>`** | 实现 `AsyncRead+AsyncWrite`,poll 拦截字节 → 调 `dump` | 仅 modbus(字节被库吞) |
| **direct dump** | frame 已在手,直接调 `dump` | lua(io_engine) |

**分层原则**:**dump 层统一**(所有插件一个 `dump` 函数、一套开关/格式/超时),**拦截层按协议特性二选一**(用第三方库的走 TraceIo,自收发的走 direct)。未来裸协议插件同理:用库的(MC/FINS 等)走 TraceIo,自收发的走 direct dump。

## 4. 组件设计

### 4.1 `io_trace` 模块(新,`src/io_trace.rs`)

```rust
/// 单实例 trace 状态(并发访问)。
pub struct TraceState {
    /// 是否激活(快速路径:trace 关时 dump 只读此 AtomicBool,~ns)。
    active: AtomicBool,
    /// 截止时刻(unix 毫秒,墙钟;惰性超时检查)。
    deadline_ms: AtomicI64,
}

impl TraceState {
    /// 是否激活且未超时(超时则惰性置 active=false 并返回 false)。
    fn check_alive(&self) -> bool { /* now_ms > deadline_ms → active=false; else active */ }
}

/// per-instance trace 开关注册表(DashMap),挂 HostContext。
#[derive(Clone, Default, Debug)]
pub struct TraceRegistry {
    map: Arc<DashMap<String, Arc<TraceState>>>,
}

impl TraceRegistry {
    pub fn new() -> Self { ... }
    /// 激活实例 trace(duration 后惰性过期)。覆盖旧状态。
    pub fn activate(&self, inst_id: &str, deadline_ms: i64) { ... }
    /// 手动关闭。
    pub fn deactivate(&self, inst_id: &str) { ... }
    /// 取实例 trace 状态(None = 未激活;Some → check_alive 查超时)。
    pub fn get(&self, inst_id: &str) -> Option<Arc<TraceState>> { ... }
    /// 当前激活且未超时的实例(供 GET 端点;遍历时 check_alive 过滤超时项)。
    pub fn active_ids(&self) -> Vec<String> { ... }
}

/// 方向。
#[derive(Copy, Clone)]
pub enum Dir { Tx, Rx }

/// dump 一段字节(查 registry → 未激活/超时则跳过;激活则 tracing 输出)。
/// 格式:`[2026-08-10T14:23:01.123] [inst_id] TX 01 03 00 00 00 0A C5 CD`
pub fn dump(registry: &TraceRegistry, inst_id: &str, dir: Dir, bytes: &[u8]) {
    let Some(state) = registry.get(inst_id) else { return; };      // 快速路径
    if !state.check_alive() { return; }                              // 超时惰性
    tracing::trace!(
        target: "io_trace",
        inst_id, dir = ?dir,
        hex = %hex(bytes),
    );
}
```

**超时机制**:惰性。`dump` 入口查 `now_ms > deadline_ms` → 置 `active=false` 并跳过。无需后台清理 task(dump 是天然检查点)。`deadline_ms` 用墙钟(unix ms via `SystemTime`),精度足够(60s 级);系统时间跳变对短时调试影响可忽略。

**模块组织**:初版单文件 `src/io_trace.rs`(registry + state + dump + TraceIo + `hex` 私有工具)。`hex(bytes: &[u8]) -> String` 输出空格分隔大写字节序列(如 `"01 03 00 00 00 0A C5 CD"`)。若超 500 行(CLAUDE.md 规则 5),拆 `io_trace/{mod.rs(registry+dump), trace_io.rs(TraceIo<T>), hex.rs}`。

### 4.2 `TraceIo<T>`(transport 拦截层,仅 modbus)

```rust
/// 包装底层 transport,拦截字节调 dump。仅服务 modbus(字节被 tokio-modbus 吞)。
pub struct TraceIo<T> {
    inner: T,
    inst_id: String,
    registry: TraceRegistry,
}

impl<T: AsyncRead + AsyncWrite + Unpin> TraceIo<T> {
    pub fn new(inner: T, inst_id: String, registry: TraceRegistry) -> Self { ... }
}

impl<T: AsyncRead + Unpin> AsyncRead for TraceIo<T> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>)
        -> Poll<io::Result<()>> {
        // delegate poll_read(inner);Ready(Ok) 后对 buf.filled() 调 dump(RX)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for TraceIo<T> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8])
        -> Poll<io::Result<usize>> {
        // delegate poll_write(inner, buf) 先行;Ready(Ok(n)) 后 dump(TX, &buf[..n])
        // (Pending/错误不 dump;WouldPoll 部分写由下次 poll_write 续写完成时 dump)
    }
    // poll_flush / poll_close:纯 delegate
}
```

**关键**:`poll_read` delegate 到 inner,`Ready(Ok)` 后对 `ReadBuf::filled()`(本次读到的字节)调 `dump(RX)`;`poll_write` delegate 到 inner,`Ready(Ok(n))` 后对 `&buf[..n]`(本次实际写入)调 `dump(TX)`。Pending/错误不 dump(避免重复/无效 dump);`WouldPoll` 部分写由下次 poll_write 续写完成时 dump。trace 关闭时,`dump` 快速路径只一次 `DashMap::get` + `AtomicBool::load`。

**与 `UdpIo` 同构**:`UdpIo`(`transport.rs:116-206`)已是 rs-iot 自写的 `AsyncRead+AsyncWrite` 喂给 tokio-modbus 的先例;`TraceIo` 是同模式的再包装(内层包 `UdpIo`/`TcpStream`/`SerialStream` 任一)。

### 4.3 REST 端点(新,`src/api/routes_trace.rs`)

```
POST /api/v1/inst/trace     { inst_id, duration_secs?: 60 }  → 200 {"status":"ok","deadline_ms":...} / 404
GET  /api/v1/inst/trace                                     → 200 [{"inst_id":"...","deadline_ms":...}]
DELETE /api/v1/inst/trace?inst_id=...                       → 200 {"status":"ok"}
```

- **JWT 保护**(非 `PUBLIC_PATHS`,与 `/metrics` 免认证不同——trace 是主动调试动作,且 hex 报文可能含敏感字段)。
- POST:从 body `duration_secs`(默认 60)算 `deadline_ms = now + duration`;查 `mgr` 实例运行中(未运行 → 404);`trace_registry.activate(inst_id, deadline_ms)`。
- 注:trace 开关作用于"实例",不依赖实例当前是否在收发——开启后该实例后续收发即被拦截。实例未运行 404 是"无意义开启"的保护(开了也没报文)。

## 5. 数据流

```
开启:REST POST {inst_id, duration_secs:60}
   → mgr 查实例运行(未运行 404)
   → trace_registry.activate(inst_id, now_ms + 60000)
   → TraceState{active:true, deadline_ms}

拦截 modbus:transport.rs::connect 时底层 IO 包 TraceIo::new(raw_io, inst_id, registry)
   → Context<TraceIo<底层IO>>
   → tokio-modbus 每次 read/write 过 TraceIo
   → poll_write(frame):dump(inst, TX, frame) → 写库
   → poll_read(buf):  dump(inst, RX, filled) → 读库
   (快速路径:registry.get → None 或 state.active==false → 直接 return,不格式化)

拦截 lua:io_engine::spawn_io_task 内
   → backend.request(&frame) 返回 resp 后:dump(inst, TX, &frame) + dump(inst, RX, &resp)
   → backend.recv_idle() 返回 frame 后:dump(inst, RX, &frame)
   (同快速路径)

超时:dumpp 入口 check_alive():now_ms > deadline_ms → active=false; return
输出:tracing target "io_trace" → 终端 + daily 文件(复用现有日志层,无需新配置)
关闭:DELETE 端点 / 超时惰性 / 进程重启(内存态,不恢复)
```

## 6. 各协议接入改动点

### 6.1 modbus(`src/plugin/builtin/modbus_client/`)

- **`transport.rs::connect`**:4 传输(tcp/rtu_tcp/rtu_udp/rtu_serial)底层 IO 包 `TraceIo::new(raw_io, inst_id.clone(), ctx.trace_registry.clone())` → 产 `Context<TraceIo<...>>`。
  - connect 签名加参数:`inst_id: &str` + `trace_registry: TraceRegistry`(从 `Source::run` 的 `ctx: HostContext` 拿)。
  - **TCP**:`tcp::connect(addr)` 产的 `TcpStream` 包 TraceIo。
  - **RTU-serial/over-tcp/over-udp**:各自的 `SerialStream`/`TcpStream`/`UdpIo` 包 TraceIo。
- **`mod.rs::session` 字段类型**:`Option<Context<...>>` 的泛型 T 因包 TraceIo 而变,类型标注跟进(见 §9 风险 R1)。
- **读/写逻辑不动**:`read_region`(`mod.rs:244-269`)、`handle_write`(`mod.rs:478-587`)零改动——拦截在 transport 层,对插件透明。
- **`Source::run`**(`traits.rs` 契约):从 `ctx.trace_registry` + `inst.inst_id` 取依赖,传给 `connect`。

### 6.2 lua(`src/plugin/builtin/lua_client/`)

- **`io_engine/mod.rs::spawn_io_task`**:`backend.request(&req.frame)`(`:85`)返回后 + `backend.recv_idle()`(`:90`)收到后,各加 `io_trace::dump(&registry, &inst_id, Dir::Tx/Rx, &bytes)`。
- **io_engine 拿 ctx**:当前 io_engine 不持 `HostContext`;`Source::run` 构造 io_engine 时注入 `inst_id: String` + `trace_registry: TraceRegistry`(Clone 廉价,Arc 内部)。
- **3 传输(tcp/serial/udp)stream 不动**:lua direct dump,不包 TraceIo,frame 天然完整(完美分帧)。

## 7. 边界(YAGNI:阶段 1 不做)

- ❌ **opcua/s7**:非裸报文协议,线缆字节是编码后二进制流(OPC UA 安全层开启还是密文),dump 协议栈字节无意义。后置或走语义摘要。
- ❌ **sidecar**:dump 的是 bridge IPC 帧(Hello/DataUp/Command),非外部设备总线报文(后者 rs-iot 看不见);真设备报文需扩 bridge 协议(外部进程上报),独立子系统。
- ❌ **完美分帧**:抓包风格起步(TX 天然完整帧;RX 可能分片但带时间戳可读)。RTU 3.5 字符间隔 / CRC 校验分帧后置。
- ❌ **GET 报文拉取 + ring buffer**:随子系统 3(tracing layer 截获 + `/api/v1/logs/recent`)。阶段 1 靠 grep 日志/文件查看。
- ❌ **trace 内容落 lux**:违背 M4"进程级热路径不落 lux"原则;trace 是调试态,进 tracing 日志足够。
- ❌ **进程重启恢复 trace 状态**:运行时调试工具,内存态合理(重启归零可接受)。
- ❌ **额外限频**:用户主动开 trace 就是要看全部报文;靠时长(默认 60s)+ 专用 target 过滤收敛。若现场反馈日志爆,后置加每实例限频。

## 8. 安全边界

- **trace 端点 JWT 保护**:hex 报文可能含寄存器值/节点数据(业务敏感),必须鉴权(对标写命令端点,非 /metrics 的免认证)。
- **trace 输出不含凭据**:报文是设备协议字节,不含 rs-iot 的 jwt_secret/lux password/外部 token。但 modbus 寄存器值/lua 私有协议数据是业务数据——靠 JWT 准入(仅管理员开 trace)。
- **不影响数据面**:trace 关闭时快速路径零开销(AtomicBool);trace 开启时 dump 代价(hex 格式化 + 日志 IO)仅在调试态,且用户主动开启即接受。
- **按实例隔离**:trace 只影响指定实例,其他实例零开销、零干扰。

## 9. 风险与未覆盖点

- **R1(中,modbus 类型传播)**:`Context<TraceIo<T>>` 的 T 变化可能影响 `session: Option<Context<...>>` 字段在 4 传输下的类型统一。当前 `session` 如何统一 4 传输的 Context 类型(具体泛型 / boxed / enum)需在 plan 首个任务验证。**缓解**:plan Task 1 安排 TraceIo 类型传播 spike(4 传输各包一次验编译),失败则退回 boxed trait object 或重审。
- **R2(低,modbus TCP MBAP 头)**:TCP 路径 TraceIo 拦截 MBAP+PDU(TX 含 7 字节 MBAP 头),现场需知首 7 字节是 MBAP。文档/dump 注释标注即可,非阻塞。
- **R3(低,trace 日志量)**:高频报文(modbus 100ms/opcua 多节点)× 长超时 → 大量 hex 行。靠专用 target `io_trace`(可独立 RUST_LOG 调级)+ 默认 60s 时长收敛。opcua 不在本阶段,实际高频源主要是 modbus,可控。
- **R4(低,墙钟超时)**:`deadline_ms` 用 `SystemTime` 墙钟,NTP 跳变/夏令时理论影响。trace 是 60s 级短时调试,影响可忽略。若需单调钟可改 `Instant`+`Mutex`(快速路径不触锁),后置。
- **未覆盖**:trace 实路径 e2e 仅 mock(modbus TCP mock server + lua mock backend);真实硬件/串口的 trace 行为靠现场验证(无 CI 真机)。

## 10. 测试策略

- **`io_trace` 单测**:
  - `TraceRegistry` activate/deactivate/get/active_ids
  - `TraceState::check_alive` 超时惰性(设过去 deadline → 返回 false + active 置 false)
  - `dump`:激活时输出正确(target/inst_id/dir/hex);未激活/超时不输出(快速路径)
  - `TraceIo<T>`:包 `mock AsyncRead/Write`(已知字节序列),验 poll_read/poll_write 拦截后 dump 的字节正确
- **REST 端点测**(axum oneshot + JWT):POST activate、GET active_ids、DELETE、未运行实例 404、JWT 拒绝
- **modbus 集成测**(`tests/trace_e2e.rs`):mock TCP modbus server(复用 `tests/modbus_multi_slave_e2e.rs` 模式),建实例 → activate trace → 触发采集 → 验日志(或 `tracing` test subscriber)含 TX/RX hex + 抓包格式
- **lua 集成测**:io_engine mock backend,activate trace → 触发 request/recv_idle → 验 dump 调用(hex 内容)
- **验证规则**(CLAUDE.md 三项):`cargo fmt --all --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test --all-targets --all-features --jobs 1`(Windows 页面文件限制)全绿。

## 11. 文件清单

| 操作 | 文件 | 说明 |
|---|---|---|
| 新 | `src/io_trace.rs` | `TraceRegistry`+`TraceState`+`dump`+`Dir`+`TraceIo<T>`+`hex` 工具(超 500 行拆 `io_trace/`) |
| 改 | `src/host.rs` | `HostContext` 加 `pub trace_registry: TraceRegistry` 字段 |
| 改 | `src/lib.rs` + 所有 HostContext 构造点 | 加字段(`tests/metrics_e2e.rs::test_app` 等) |
| 改 | `src/plugin/builtin/modbus_client/transport.rs` | 4 传输底层 IO 包 `TraceIo` + connect 签名加 `inst_id`+`trace_registry` |
| 改 | `src/plugin/builtin/modbus_client/mod.rs` | session 类型标注跟进;`Source::run` 传 inst_id/registry 给 connect |
| 改 | `src/plugin/builtin/lua_client/io_engine/mod.rs` | `spawn_io_task` 加 dump 调用;io_engine 注入 inst_id/registry |
| 改 | `src/plugin/builtin/lua_client/mod.rs` | `Source::run` 构造 io_engine 时传 inst_id/trace_registry |
| 新 | `src/api/routes_trace.rs` | POST/GET/DELETE `/api/v1/inst/trace`(JWT 保护) |
| 改 | `src/api/mod.rs` | 注册路由(`build_router`) |
| 新 | `tests/trace_e2e.rs` | modbus + lua + REST 集成测 |
| 改 | `CLAUDE.md` | 验证规则加报文 trace 条目 |
| 改 | `docs/scenario-gap-assessment.md` | §5/§8 #3 状态更新(子系统 2 阶段 1 完成) |

## 12. 后续阶段(不在本 spec)

- **阶段 2**:sidecar bridge 帧 trace + 扩 bridge 协议让外部进程上报设备报文(真·外部插件报文调试)。
- **阶段 3**(可选):opcua/s7 语义摘要(NodeId+值+StatusCode),明确标注"非报文 trace"。
- **子系统 3**:日志 ring buffer(tracing layer 截获最近 N 条)+ `GET /api/v1/logs/recent`;trace 经专用 target 自然衔接进 ring buffer。

---

**设计取舍小结**:dump 层统一 + 拦截层分工(modbus TraceIo / lua direct);抓包风格起步不分帧;专用 tracing target `io_trace`;惰性超时(墙钟);JWT 保护;阶段 1 聚焦 lua + modbus 裸报文主战场。核心未决风险 R1(modbus 类型传播)由 plan 首任务 spike 验证。
