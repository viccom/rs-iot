# 日志 ring buffer(P1 子系统 3)设计

> **状态**:设计(spec)阶段,待 review → writing-plans。
> **日期**:2026-08-10。
> **范围**:tracing layer 截获最近 N 条日志到内存 ring buffer + `GET /api/v1/logs/recent` REST 快照。
> **关联**:scenario-gap §3(故障定位诉求②"故障前日志")与 §8 #3(P1 可观测体系子系统 3);承接 `/metrics`(子系统 1)+ 报文 trace(子系统 2,target `io_trace` 经本层衔接)。
> **不在范围**:`tail -f`/SSE 推送、错误聚合/限流、per-target buffer、落盘持久化。

## 1. 背景与动机

工业现场诉求(scenario-gap §0 诉求②):故障时能**快速定位**(时间/类型/模块)+ 拿到**故障前一段时间的日志**。当前(scenario-gap §3):日志落 daily 文件 + 保留期清理(M3 已做),但**无内存 ring buffer**——"故障前快照"只能翻 daily 文件,无 REST 可快照取最近 N 条。

P1 子系统 3:tracing subscriber 加一个**自定义 Layer**(RingBufferLayer),截获最近 N 条日志到内存 ring buffer,经 `GET /api/v1/logs/recent` 取 JSON 快照(支持 level/target/limit 过滤)。

**与报文 trace(子系统 2)衔接**:子系统 2 的报文 hex dump 走 `tracing::trace!(target:"io_trace", ...)`,但全局 `EnvFilter` 默认 info 会滤掉 trace level。RingBufferLayer 用**独立 filter**(`info,io_trace=trace`)抓报文 trace 进 buffer,`GET /logs/recent?target=io_trace` 即可 REST 拉取报文 trace——**补子系统 2 阶段 1 缺的 GET 拉取出口**(阶段 1 trace 只进日志文件,靠 grep)。

## 2. 现状(代码核实)

- `init_tracing`(`lib.rs:54-87`):`tracing_subscriber::registry().with(layers).try_init()`,layers 是 `Vec<Box<dyn Layer<Registry> + Send + Sync>>`,含 fmt 终端层 + fmt 文件层(daily appender + non_blocking + `WorkerGuard`)。filter 是 `EnvFilter`(RUST_LOG 优先,缺省 `[log].level`)。
- **无自定义 Layer**:rs-iot 现只用 fmt 层(现成)。RingBufferLayer 是首个自定义 Layer。
- **无 logs REST 端点**:`src/api/` 无 `routes_logs`(grep 确认)。
- `[log]` config(`config.rs::LogConfig`):`dir`/`level`/`max_days`/`max_size_mb`(M3 文件日志 + 保留期)。

## 3. 架构

```
tracing event → init_tracing 的 layers vec
   ├─ fmt 终端层(现有,全局 EnvFilter)
   ├─ fmt 文件层(现有 daily appender,全局 EnvFilter)
   └─ RingBufferLayer(新)─ 独立 EnvFilter:"info,io_trace=trace"
                              ↓ on_event 结构化(Visit)
                     LogRingBuffer(VecDeque<LogEntry> + Mutex,容量 N=2000)
                              ↓ GET 查询(过滤)
                     GET /api/v1/logs/recent?level=&target=&limit=  → JSON(JWT)
```

**关键**:RingBufferLayer 用**独立 EnvFilter**(`info,io_trace=trace`),而非全局 info——这样报文 trace(trace level,target `io_trace`)能进 buffer(全局 info filter 会滤掉 trace level)。这是子系统 2 衔接的技术核心,也是"独立 filter"的必要性的来源。

## 4. 组件设计

### 4.1 `LogEntry`(结构化,新 `src/log_ringbuffer.rs`)

```rust
/// 一条日志(结构化,tracing event 经 Visit 收集)。
#[derive(Debug, Clone, serde::Serialize)]
pub struct LogEntry {
    /// 时间戳(ISO8601,on_event 时取 SystemTime)。
    pub ts: String,
    /// 级别("ERROR"/"WARN"/"INFO"/"DEBUG"/"TRACE")。
    pub level: String,
    /// target(模块路径,如 "io_trace"、"modbus_client")。
    pub target: String,
    /// 格式化可读文本(tracing 的 message 字段经 display 包装,Visit record_debug 提取)。
    pub message: String,
    /// 结构化字段(Visit record_* 收集,值统一 String 化,如 inst_id/dir/hex)。
    pub fields: std::collections::HashMap<String, String>,
}
```

### 4.2 `LogRingBuffer`(进程级一个,新 `src/log_ringbuffer.rs`)

```rust
/// 进程级日志 ring buffer(VecDeque + Mutex,容量 N)。
pub struct LogRingBuffer {
    entries: Mutex<VecDeque<LogEntry>>,
    capacity: usize,
}

impl LogRingBuffer {
    pub fn new(capacity: usize) -> Self { /* VecDeque::with_capacity */ }
    /// push 一条(满则 pop_front 淘汰最旧)。
    pub fn push(&self, entry: LogEntry) { /* lock + push_back + 若 len>capacity pop_front */ }
    /// 取最近条目(按 level/target/limit 过滤;level 过滤为 >= 给定级别)。
    pub fn recent(&self, level: Option<&str>, target: Option<&str>, limit: usize) -> Vec<LogEntry> {
        /* lock + iter().rev() + filter + take(limit) + 反转(最近在前) */
    }
}
```

**并发**:`std::sync::Mutex`(日志非超热路径,on_event 在 subscriber 线程顺序调用,uncontended ~ns)。容量 N 固定,内存上限 ~1MB(2000 条 × ~500 字节)。`recent` 从最新向前迭代 + 过滤 + take(limit),返回最近 `limit` 条(新在前)。

### 4.3 `RingBufferLayer`(`impl Layer`,新 `src/log_ringbuffer.rs`)

```rust
/// tracing subscriber layer:截获 event → LogEntry → push LogRingBuffer。
/// 独立 EnvFilter("info,io_trace=trace")经 .with_filter() attach(见 init_tracing)。
pub struct RingBufferLayer {
    buffer: Arc<LogRingBuffer>,
}

impl<S: Subscriber> Layer<S> for RingBufferLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let meta = event.metadata();
        let mut visitor = EntryVisitor::default();
        event.record(&mut visitor);  // 驱动 Visit 收 message + fields
        self.buffer.push(LogEntry {
            ts: now_iso8601(),                         // SystemTime → ISO8601
            level: meta.level().as_str().to_string(),
            target: meta.target().to_string(),
            message: visitor.message,
            fields: visitor.fields,
        });
    }
}

/// Visit:message 字段(名为 "message")→ message;其余 → fields。
#[derive(Default)]
struct EntryVisitor {
    message: String,
    fields: HashMap<String, String>,
}

impl tracing_core::field::Visit for EntryVisitor {
    fn record_str(&mut self, field: &tracing_core::field::Field, value: &str) {
        // str 字段直接存(不带 Debug 引号):inst_id/dir/hex 等。
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.insert(field.name().to_string(), value.to_string());
        }
    }
    fn record_debug(&mut self, field: &tracing_core::field::Field, value: &dyn std::fmt::Debug) {
        // message 字段经 display 包装(DisplayValue 的 Debug=Display),得可读文本(R1 单测锁定)。
        // 其余非 str 字段(bool/i64/u64/error)归这里,format!("{:?}")。
        let s = format!("{value:?}");
        if field.name() == "message" {
            self.message = s;
        } else {
            self.fields.insert(field.name().to_string(), s);
        }
    }
}
```

**message 可读性**(风险 R1,见 §10):tracing 宏的 message 经 `field::display(args)` 包装为 `DisplayValue<fmt::Arguments>`,其 `Debug` impl 等于 `Display`——故 `format!("{value:?}")` 对 message 字段得**可读文本**(如 `hello`,非 `"hello"`)。单测锁定(`info!("hello")` → `message == "hello"`)。

### 4.4 REST 端点(新 `src/api/routes_logs.rs`)

```
GET /api/v1/logs/recent?level=&target=&limit=  → 200 [{"ts","level","target","message","fields"}, ...]
```

- **JWT 保护**(非 `PUBLIC_PATHS`,同 trace 端点——logs 含报文 hex/错误细节/业务数据)。
- query 参数(均可选):
  - `level`(如 `warn`):过滤 `level >= 给定`(level 数字 `<=` 给定;Error=1<Warn=2<Info=3<Debug=4<Trace=5)。`level=warn` → error+warn。
  - `target`(如 `io_trace`):精确匹配 target。报文 trace 用 `?target=io_trace`。
  - `limit`(默认 200,上限 = buffer 容量):返回最近 N 条(新在前)。
- 返回 `LogEntry` JSON 数组(serde Serialize)。

## 5. 关键设计决策(选定 + 理由)

| 决策 | 选定 | 理由(备选已排除) |
|---|---|---|
| 截获范围 | 独立 filter `info,io_trace=trace` | 全局 info filter 会滤掉报文 trace(trace level);全收(含 debug)噪声多。独立 filter 衔接子系统 2 + 抓故障 info/warn/error |
| 内容形态 | 结构化 `LogEntry` + JSON 返回 | 便于 level/target 过滤 + 前端渲染 + trace 衔接;文本行过滤难 |
| 粒度 | 进程级一个 buffer | per-target 隔离好但复杂(动态 map + 内存不可控);进程级 + `?target=` 过滤够 |
| 容量 | 默认 2000(config `[log].ring_buffer_capacity`) | ~1MB 内存;2000 条够大,按需激活(60s)的报文 trace 不易被 info 刷掉 |
| 并发 | `std::sync::Mutex` | 日志非超热路径,uncontended ~ns;parking_lot 优化后置 |
| 访问控制 | JWT 保护 | logs 含报文 hex/业务数据/错误细节,需鉴权(对标 trace 端点,非 /metrics 免认证) |
| 默认 limit | 200 | 故障快照够用;防一次性返回全 buffer(2000 条)过大 |

## 6. 数据流

```
启动:init_tracing 建 LogRingBuffer(capacity) → Arc
   → RingBufferLayer { buffer: Arc::clone } .with_filter(EnvFilter::new("info,io_trace=trace"))
   → 加进 layers vec(终端层/文件层之后)
   → registry().with(layers).try_init()
   → init_tracing 返回 (Option<WorkerGuard>, Arc<LogRingBuffer>)
   → run_app 把 Arc 注入 AppState

运行:tracing event(info/warn/error + io_trace=trace)
   → 各 layer 各自 filter 决定是否处理
   → RingBufferLayer.on_event(独立 filter 放行)→ Visit → LogEntry → buffer.push

查询:GET /api/v1/logs/recent?level=&target=&limit=
   → handler 从 AppState 取 Arc<LogRingBuffer>
   → buffer.recent(level, target, limit) → Vec<LogEntry>
   → Json(entries)
```

## 7. 与报文 trace(子系统 2)衔接

子系统 2 报文 trace(`io_trace::dump` 调 `tracing::trace!(target:"io_trace", inst_id, dir, hex)`)的 event:
- 终端/文件层:全局 EnvFilter 决定(RUST_LOG 含 `io_trace=trace` 才打;默认 info 不打 trace)。
- **RingBufferLayer:独立 filter `io_trace=trace` 放行** → `inst_id`/`dir`/`hex` 经 Visit 进 `LogEntry.fields` → buffer。

`GET /logs/recent?target=io_trace` → 拉报文 trace 条目(`fields` 含 inst_id/dir/hex)。**子系统 2 的报文 trace 输出统一到终端/文件 + ring buffer 两个出口**,后者可 REST 拉取——补阶段 1 缺的 GET 出口。

## 8. 边界(YAGNI:不做)

- ❌ **不落盘**:ring buffer 是内存快照;落盘靠现有文件层(M3 daily + 保留期)。
- ❌ **不持久化**:进程重启 ring buffer 清空(合理——是"最近"快照,非历史归档)。
- ❌ **不做 `tail -f`/SSE 推送**:GET 取快照;实时看日志文件。
- ❌ **不做错误聚合/限流**:scenario-gap §3 P1 项(相同错误去重/降频),后置。
- ❌ **不做 per-target buffer**:进程级一个,`?target=` 过滤。
- ❌ **不做结构化字段类型化**:fields 统一 String(Visit record_debug 的 `{:?}`);JSON Value 类型化后置。
- ❌ **不做 span 上下文**:`on_event` 不收集 span 链(`ctx.event_scope()`);LogEntry 只含 event 本身。后置。

## 9. 安全边界

- **JWT 保护**:`GET /api/v1/logs/recent` 不进 `PUBLIC_PATHS`(默认 JWT)。logs 含报文 hex(业务数据)、错误细节(可能含路径/参数)、tracing 字段——需鉴权(对标 trace 端点)。
- **内存固定**:容量 N 固定(默认 2000),无内存增长风险。config 可调(`ring_buffer_capacity`)。
- **不影响数据面**:RingBufferLayer 在 subscriber 线程,GET 在 REST 线程;Mutex 竞争低(GET 偶尔,push 顺序)。trace 关闭时 buffer 仍收 info/warn/error(它不是 trace 开关,是常驻日志快照)。

## 10. 风险与未覆盖点

- **R1(低,message 格式化)**:tracing message 字段经 `field::display(args)` 包装,`format!("{value:?}")` 得可读文本(DisplayValue 的 Debug=Display)。**单测锁定**:`info!("hello")` → `message == "hello"`(非 `"hello"` 带引号);`info!("v={}", 42)` → `message == "v=42"`。若实际带引号,plan 改用 `fmt::Arguments` 直接渲染或 `field::display` 专用 Visit 方法。
- **R2(低,init_tracing 返回类型改动)**:现返 `Option<WorkerGuard>`,要加返 `Arc<LogRingBuffer>`(tuple `(Option<WorkerGuard>, Arc<LogRingBuffer>)` 或单独返回)。触及 main.rs/service 调用点(`lib.rs:91 run_app` + service)。改动小(解构),非破坏性。
- **R3(低,高频日志锁竞争)**:若 info 日志极高频(如 pipeline 每条 sample),`push` 的 Mutex lock 有竞争。但日志非超热路径 + uncontended ~ns,可接受。若现场反馈,改 `parking_lot::Mutex`(更快)或 lock-free,后置。
- **R4(低,字段类型化)**:fields 统一 String(str 经 `record_str` 不带引号;bool/i64 经 `record_debug` 得 `"true"`/`"42"`,可读)。前端渲染够;若需 JSON 类型化(serde_json::Value 保留 bool/i64 类型),后置。
- **未覆盖**:ring buffer 实路径 e2e(真实高频日志 + 大容量)靠现场;CI 验单测 + 集成测(mock event + GET 过滤)。

## 11. 测试策略

- **`log_ringbuffer` 单测**:
  - `LogRingBuffer`:push/recent/容量淘汰(push N+1 → 最旧被淘汰)/recent 过滤(level>= / target 精确 / limit)/空 buffer 返回空
  - `RingBufferLayer` + `EntryVisitor`:用 `tracing::dispatcher::with_default`(&一个含 RingBufferLayer 的 subscriber)驱动 `info!/warn!/error!` + `tracing::trace!(target:"io_trace",...)` → 验 LogEntry 的 ts/level/target/message/fields 正确;**message 可读性锁定**(R1,不带引号);filter 生效(io_trace=trace 进,普通 debug 不进)
- **REST 集成测**(`tests/` 扩 trace_e2e 或新 log_e2e):GET 默认/level 过滤/target 过滤/limit/JWT(无 JWT 401)/空 buffer;**报文 trace 衔接测**(activate trace + 触发收发 → `GET ?target=io_trace` 见 inst_id/dir/hex)
- **验证规则**(CLAUDE.md 三项):`cargo fmt --all --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test --all-targets --all-features --jobs 1`(Windows 页面文件)全绿。

## 12. 文件清单

| 操作 | 文件 | 说明 |
|---|---|---|
| 新 | `src/log_ringbuffer.rs` | `LogEntry`+`LogRingBuffer`+`RingBufferLayer`+`EntryVisitor`+`now_iso8601`(超 500 行拆 `log_ringbuffer/`) |
| 改 | `src/lib.rs` | `pub mod log_ringbuffer;`;`init_tracing` 建 buffer + 加 layer + 返回 `(Option<WorkerGuard>, Arc<LogRingBuffer>)`;`run_app` 注入 AppState |
| 改 | `src/api/mod.rs` | `AppState` 加 `log_ringbuffer: Arc<LogRingBuffer>` 字段;注册 `routes_logs` 路由 |
| 新 | `src/api/routes_logs.rs` | `GET /api/v1/logs/recent?level=&target=&limit=`(JWT) |
| 改 | `src/config.rs` | `LogConfig` 加 `ring_buffer_capacity: usize`(`#[serde(default)]`,默认 2000) |
| 改 | 所有 `HostContext`/`AppState`/`init_tracing` 构造点 | 加字段/解构(类似报文 trace Task 3) |
| 改 | `main.rs`/`service/` | `init_tracing` 返回类型改动的解构(R2) |
| 新/改 | `tests/` | log ring buffer 集成测(扩 trace_e2e 或新 log_e2e) |
| 改 | `CLAUDE.md` + `docs/scenario-gap-assessment.md` | 验证条目 + §3/§8 #3 状态 |

## 13. 后续(不在本 spec)

- **错误聚合/限流**(scenario-gap §3 P1):相同错误去重/降频(前 N 次详细、之后采样),减少刷屏。独立 spec。
- **结构化字段类型化**:fields `String` → `serde_json::Value`(bool/i64/u64/f64 等保留类型),便于前端 + ELK 采集。
- **span 上下文**:LogEntry 加 span 链(`ctx.event_scope()`),定位 event 所属 span(如 inst_id span)。
- **P2 结构化日志(JSON)**:scenario-gap §3 P2,便于 ELK;ring buffer 的结构化 LogEntry 是其前身。

---

**设计取舍小结**:独立 filter 抓 trace level(衔接子系统 2,补 GET 出口);结构化 LogEntry + JSON(过滤 + 前端);进程级一个 buffer + Mutex(简单);容量 2000 config 可配;JWT 保护。核心风险 R1(message 格式化)由单测锁定;RingBufferLayer 是 tracing 标准扩展模式(查证有完整示例),非未知。
