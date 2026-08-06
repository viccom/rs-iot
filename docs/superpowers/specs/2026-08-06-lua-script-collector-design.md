# rs-iot Lua 脚本采集系统 设计文档

> 日期：2026-08-06
> 状态：设计草案（已与用户逐点确认，待 writing-plans 拆分实现计划）
> 流程：brainstorming → 本 spec → writing-plans → 实现

## 1. 背景与目标

### 1.1 背景
工业现场存在大量「简单自定义协议」的设备，覆盖多个行业、多种设备类型。它们多通过串口（RS232/RS422/RS485）或以太网通讯，协议特征：

- 交换方式多为**一问一答**，也有部分**广播/被动推送**报文；
- 报文简单、规范性差（厂商自定义，常参考 Modbus 但不标准）；
- 单设备数据点不多。

若这类长尾协议全部走 rs-iot 的「内置 Rust 采集插件」方式（modbus_client/opcua_client/s7_client 那样每个协议一个 crate 模块），会导致**插件数量与代码量激增、每个协议都要重新编译发布**，维护成本不可接受。

### 1.2 目标
为 rs-iot 引入一个基于 **Lua 5.4** 的动态脚本采集系统，专门覆盖上述长尾协议：

- **随改随用、无需编译**：脚本改动即时生效，实施工程师可现场适配新协议；
- **入门简单**：脚本只聚焦「协议报文解析 + 数据计算/处理」；
- **遵循 rs-iot 现有架构**：作为一个新的南向 Source 接入双总线，不破坏 hub/pipeline/lux/CtlRegistry 等核心；
- **宿主统一管理通讯链路**：串口 / TCP / UDP 由 Rust 宿主封装，脚本不碰底层连接；
- **不止采集**：通过丰富的 ctx API，脚本可做边缘计算、设备联动、云对接等「简单灵活」的事。

### 1.3 参考与边界
- 技术栈参考 `D:\rs_codes\rs-freeioe`（mlua + lua54 + async 桥接 + IoChannel 抽象 + 每脚本独立 VM），**不复刻、不照搬**其 actor/mailbox/Registry 运行时（那与 rs-iot 的 hub/lux 双总线是两套设计）。
- 不替代内置高性能插件（Modbus/OPC UA/S7 仍用内置 Rust 插件）；Lua 脚本系统面向「协议简单、点位不多、需快速适配」的长尾场景。

## 2. 关键决策汇总（已确认）

| 决策项 | 选择 | 理由摘要 |
|---|---|---|
| 产物形态 | **集成进 rs-iot**（进程内 Lua Source） | 契合「南向 Source」架构；mlua 三层隔离护栏足够；独立二进制有 IPC/生命周期/认证额外复杂度 |
| Lua 后端 | **Lua 5.4**（mlua 0.12，vendored） | 稳定成熟、语法完整（64bit 整数/位运算/string.pack）、生态正 |
| 脚本组织 | **文件目录** `scripts/<name>/main.lua` | 独立文件夹、可本地 require |
| 热加载 | **notify 监听 + 去抖 + 重建 VM + 失败回退** | 随改随用，与 InstanceManager 启停同构 |
| 编程模型 | **回调式**：`collect` / `on_write` / `on_data`（可选） | 覆盖一问一答 + 广播两类，入门门槛最低 |
| 报文 framing | **透传字节**（脚本自己拼包/CRC） | 最灵活；宿主不假设协议格式 |
| API 面 | **全量 L1-L6**（见 §7） | 采集 + 计算 + 联动 + 持久 + 云对接 |
| HTTP 出站 | **默认开 + 全审计日志** | 简单灵活，靠日志追溯滥用 |
| 状态监测 | **lux `stat:<inst>` + REST 查询** | 复用 lux 作为状态真相源 |
| 并发模型 | **每实例一个独立 Lua VM + 一个 tokio task** | 故障/状态隔离最强；不跨线程共享 VM |
| IO 模型 | **channel + 独占 IO task**（模式 Z） | reactor 不被占；连接生命周期与 Lua 解耦 |

## 3. 架构定位

`luaClient` 是一个新的南向 Source，与 datasim/modbus_client/opcua_client/s7Client/sidecarClient 同构。**不碰** hub / pipeline / lux / InstanceManager / CtlRegistry / 北向——符合 rs-iot CLAUDE.md「横向扩展点」表「加一个南向协议只动边界」的承诺。

```
── 数据上行总线 ────────────────────────────────────────────────────
luaClient 实例A (scripts/devA/, 独立 Lua VM)  ─┐
luaClient 实例B (scripts/devB/, 独立 Lua VM)  ─┼──► tx:mpsc::Sender<Sample> ──► hub(256)
  内部：Lua VM ──ctx:request()──► IO task(独占 serial/tcp/udp)         │
                                                                     ▼
                                                              pipeline(Processor)
                                                                     │
                                                                     ▼
                                                           lux HSET rtdata:<devid>
── 控制下行总线 ────────────────────────────────────────────────────
MQTT <hwid>/command ─┐
POST /api/v1/command/send ─┼──► CtlRegistry.try_send(64) ──► 实例 cmd_rx
                            │           │
                            │           ▼
                            │   luaClient::run match Cmd ──► ctx 调 on_write(cmd) ──► IO task
                            │
ctx:send_command(他实例) ───┘  （Lua 脚本发起的联动写命令，经同一 CtlRegistry）
```

**一个脚本目录 = 一个 InstanceManager 实例 = 一个独立 Lua VM = 一个 tokio task。** 多脚本即多实例，天然支持。

## 4. 模块划分（新增文件，全部在边界内）

```
src/plugin/builtin/lua_client/
  mod.rs        LuaClient + Source impl + 主循环（照抄 modbus_client 的 select!{token/cmd/tick} 骨架）
  config.rs     app_config 解析 → LuaCfg（含 transport/period/timeout/reconnect/mem_limit/dump_comm/enable_http）
  engine.rs     VM 创建（Lua::new_with 沙箱）+ 指令 hook + 内存限额 + 注册全部 ctx host API
  io_engine.rs  channel + 独占 IO task + serial/tcp/udp 三传输（复用 modbus_client 的 AsyncRead+AsyncWrite+Unpin+Send transport 模式）
  api.rs        ctx 对象（UserData）+ L1-L6 host 函数实现
  script.rs     脚本加载/编译 + notify 热加载（notify-debouncer-mini 去抖 + 重建 VM + 失败回退）
  loader.rs     scripts/<name>/ 目录约定解析（main.lua + 本地 lib/*.lua require 限定在目录内）
src/plugin/builtin/mod.rs            +pub mod lua_client;            （一行，触发 inventory）
src/db/luakv.rs                       kv 持久化（lux Hash luakv:<inst_id> 读写）
src/db/mod.rs                         引出 luakv
src/db/identity.rs                    hash 清单注释新增 luakv:<inst_id>
src/api/routes_lua.rs                 Lua 实例状态/重载 REST 端点（GET /api/v1/lua/<inst>/stats、POST .../reload）
src/api/mod.rs                        build_router 注册 routes_lua
scripts/<name>/main.lua               用户脚本（每协议一个独立文件夹）
docs/lua-script-guide.md              脚本开发指南（API 参考 + 示例）
examples/lua-scripts/                 示例脚本（含 mock 协议，配集成测试）
```

插件文件底部：
```rust
inventory::submit! {
    crate::plugin::registry::SourceEntry {
        name: "luaClient",
        create: || Box::new(LuaClient),
    }
}
```
`app_code="luaClient"` 经 `registry::find_source` 匹配。

## 5. 脚本组织与生命周期

### 5.1 目录约定
```
scripts/<name>/
  main.lua          入口（定义 on_init/collect/on_data/on_write）
  lib/*.lua         可选，脚本本地模块（Lua require 限定在此目录内，不越过沙箱）
```
实例 `InstConfig.app_config.script = "<name>"`。`<name>` 仅允许 `[A-Za-z0-9_-]`，防路径穿越。

### 5.2 启动流程
1. `InstanceManager::start_source`（与现有插件同路径）调 `LuaClient::run(ctx, inst, tx, cmd_rx, token)`。
2. `config::parse_cfg(&inst.app_config)` → `LuaCfg`，非法字段 `Err(String)`，warn 后 return。
3. `engine::new_vm(&cfg)`：`Lua::new_with(StdLib::TABLE|STRING|MATH|COROUTINE, opts)` + 指令 hook + 内存限额。
4. `api::register(&lua, &ctx_handle)`：把 ctx 对象与全部 L1-L6 host 函数注册到 Lua 全局。
5. `script::load(&lua, &scripts_dir, &name)`：编译 `main.lua`（本地 require 解析到 `lib/`）。
6. 调 `on_init(ctx)`（若脚本定义），失败 warn 继续。
7. 进入主循环（§5.4）。

### 5.3 热加载
- `notify::RecommendedWatcher` 监听 `scripts/`，经 `notify-debouncer-mini` 去抖 200ms（编辑器一次保存常触发多次事件）。
- 收到 `<name>/` 改动事件 → 在该实例的 Source task 内：编译新脚本到**新 VM**（沙箱+hook）→ 跑 dry-run（on_init + 一次 collect 带严格 hook，验证不死循环）→ 成功则 `CancellationToken` 旧 VM 优雅退出并切换 → 失败 `warn!`，**旧 VM 继续跑，永不半生效**。
- 与 `InstanceManager` 的 stop/start 同构（VM 替换 ≈ 轻量重启 instance）。

### 5.4 主循环骨架（照抄 modbus_client）
```rust
loop {
    let evt = tokio::select! {
        _ = token.cancelled() => break,
        msg = cmd_rx.recv() => Evt::Cmd(msg),
        frame = data_rx.recv() => Evt::Data(frame),   // IO task 被动帧（触发 on_data，见 §8）
        _ = ticker.tick() => Evt::Tick,     // period 来自 cfg；period=0 时不产生 Tick（纯被动）
    };
    match evt {
        Evt::Cmd(Some(m)) => if m.cmd == "write" { call_on_write(&vm, &m.data).await; }
        Evt::Cmd(None) => break,
        Evt::Data(Some(frame)) => { let _ = call_on_data(&vm, &frame).await; }  // 脚本 on_data(frame)
        Evt::Data(None) => break,
        Evt::Tick => {
            // ensure io task connected（懒连接 + 断链重连，参考 modbus Option<Session>）
            match call_collect(&vm).await {   // 调脚本 collect()，底层 ctx:request 经 IO task
                Ok(Some(sample)) => { let _ = tx.send(sample).await; }  // → hub
                Ok(None) => {}                 // 脚本返回 nil，本次无数据
                Err(e) => warn!(%e, "collect failed"),  // 脚本 error，VM 不死，下 tick 重试
            }
        }
    }
}
```
**关键不变式**（守 modbus_client 经验）：
- `Evt` 枚举解耦 `select!` 与 `&mut` 借用；
- `Option<IoHandle>` 懒连接 + 断链重连（**Plan 2 实现**；MVP 启动连一次 `TcpIo::connect`，失败回退 `MockIoTask`，运行中断连不自动重连——对标 modbus `Option<Session>` 模式待补）；
- 脚本 error 仅 warn，不杀 VM；
- `ticker.set_missed_tick_behavior(Skip)` 防积压雪崩。

## 6. 脚本编程模型

回调式：脚本按需定义下列函数，宿主在对应时机调用。

| 函数 | 调用时机 | 返回 | 用途 |
|---|---|---|---|
| `on_init()` | 启动时一次 | 忽略 | 初始化（建表、读 kv、日志）；用全局 `ctx` |
| `collect()` | 每个 `period` 周期（period=0 时不调） | `table\|nil`（key→value） | 主动一问一答采集；返回 table 转 Sample 推 hub；用全局 `ctx` |
| `on_data(frame)` | IO task 收到帧时（被动） | 忽略 | 广播/推送协议；脚本内 `ctx:set_value` 上报；`frame` 为被动帧字节串 |
| `on_write(cmd)` | 收到写命令时（cmd_rx） | `bool\|nil` | 处理写命令；`cmd` 为 JSON 解析后的 table；用全局 `ctx` |

**`ctx` 是全局**（宿主 `api::register` 注册 `lua.globals().set("ctx", ...)`），脚本直接用全局 `ctx`，回调函数**不取 ctx 参数**。Lua 冒号语法 `ctx:request(...)`/`ctx:log_info(...)`/`ctx:now()` 的隐式 self 由宿主函数 `_self: Value` 首参吸收（mlua 0.12，见实现 api.rs）。

- `collect` 与 `on_data` **可共存**：一问一答走 collect，被动报文走 on_data。
- 纯被动协议设 `period=0`，仅靠 on_data。
- 脚本产出数据两条等价路径：`collect` 返回 table（批量）、`ctx:set_value(tag,v)`（单点，适合 on_data 内）。

### 示例脚本（一问一答 + 广播）
```lua
-- scripts/devA/main.lua
local function crc16(buf)
    local s = 0
    for i = 1, #buf do
        s = (s ~ buf:byte(i)) & 0xFFFF
        for _ = 1, 8 do
            s = (s & 1 ~= 0) and ((s >> 1) ~ 0xA001) or (s >> 1)
        end
    end
    return s
end

function on_init()
    ctx.counter = ctx:kv_get("counter") or 0   -- 读持久状态
    ctx:log_info("devA started")
end

function collect()  -- 周期一问一答；用全局 ctx（无参）
    local req = "\x01\x03\x00\x00\x00\x02"
    local resp = ctx:request(req .. string.pack("<H", crc16(req)), 100)
    if not resp or #resp < 7 then return nil end
    local temp = string.unpack(">i2", resp:sub(4, 5)) / 10
    ctx.counter = ctx.counter + 1
    ctx:kv_set("counter", ctx.counter)         -- 持久化累计
    return { temp = temp, counter = ctx.counter }
end

function on_data(frame)                   -- 被动广播帧（frame 为字节串；用全局 ctx）
    ctx:set_value("heartbeat", ctx:time())
end

function on_write(cmd)                    -- 写命令（cmd = JSON 解析后 table；用全局 ctx）
    local f = "\x01\x06" .. string.pack(">H>i2", cmd.addr, cmd.value)
    return ctx:request(f .. string.pack("<H", crc16(f)), 100) ~= nil
end
```

## 7. 宿主 ctx API 面（L1-L6）

设计原则：**丰富，但所有跨边界能力都走 rs-iot 现有总线**（数据走 lux、控制走 CtlRegistry、出站走受控 http/request），不搞 Lua 间直连或绕过数据模型。

### L1 通讯（采集主流程）
| API | 说明 |
|---|---|
| `ctx:request(frame, timeout_ms)` → `string\|nil, err?` | 主动一问一答（serial/tcp/udp 统一）；底层 coroutine.yield + IO task async |
| `on_data(frame)`（脚本定义） | 被动收报；IO task 收到帧触发；用全局 ctx |
| `ctx:sleep(ms)` | 主动延迟（底层 tokio sleep，不阻塞 reactor） |

### L2 数据读写
| API | 说明 |
|---|---|
| `ctx:set_value(tag, v)` | 写**自己**当前 devid 的值（转 Sample 推 hub） |
| `collect` 返回 table | 批量写自己（等价 set_value） |
| `ctx:get_value(devid, tag)` → `v\|nil` | 读**任意**设备最新值（从 lux rtdata，只读） |
| `ctx:get_device(devid)` → `table\|nil` | 读一个设备全部点位最新值 |
| `ctx:list_devices()` → `table` | 列出本机设备树 |

### L3 控制下行（边缘联动）
| API | 说明 |
|---|---|
| `ctx:send_command(inst_id, devid, cmd, data)` → `bool` | 向**另一实例**发写命令（经 `HostContext.ctl_registry.send`）；联动 |

### L4 持久状态（边缘计算记忆）
| API | 说明 |
|---|---|
| `ctx:kv_set(key, v)` / `ctx:kv_get(key)` → `v\|nil` | 脚本本地持久化（存 lux `luakv:<inst_id>`，跨重启保留）；累计/状态机/上次值 |

### L5 网络与云对接（feature `lua-http`，默认开，全审计）
| API | 说明 |
|---|---|
| `ctx:http_get(url, headers?, timeout?)` → `table{status,body}` | HTTP GET（reqwest，同步语法底层 async） |
| `ctx:http_post(url, body, headers?, timeout?)` → `table{status,body}` | HTTP POST |
| 审计 | 所有出站 HTTP 写 `tracing` 日志（INFO 级，含 url/method/状态码/耗时），可追溯 |

### L6 工具
| API | 说明 |
|---|---|
| `ctx:log_info/warn/err/debug(msg)` | 日志（tracing，前缀 `[lua:<name>]`） |
| `ctx:now()` → `number` | 单调毫秒（替代被禁的 os.clock 系） |
| `ctx:time()` → `number` | 墙上秒（替代 os.time） |
| `ctx:config(key)` → `v\|nil` | 读自己 app_config 自定义参数 |
| `ctx:device_config(devid)` → `table\|nil` | 读 devices 声明 |
| `ctx:dump_comm(dir, bytes)` | 原始报文导出（调试/统计；开关 `dump_comm`） |
| `ctx:crc16(buf)` / `ctx:modbus_crc(buf)` → `number` | 常用 CRC helper |

### 有意**不暴露**（架构/安全红线）
- **lux 任意命令** —— 只通过受控 `get_value`/`get_device`/`kv_*` 接口，不让脚本绕过数据模型直接翻 lux；
- **文件 IO / os.execute / 任意 socket** —— 持久化用 `kv_*`、联网用受控 `request`/`http_*`，杜绝逃逸；
- **Lua 脚本间直接通信** —— 跨脚本经 lux（`get_value`/`kv`）或 `send_command` 间接，守 single source of truth；
- **写其他设备的值** —— `set_value` 仅限自己当前 devid，数据归属清晰，避免脚本互相覆盖。

## 8. IO 引擎（channel + 独占 IO task，模式 Z）

研究证实这是唯一不让 reactor 被占、且连接生命周期与 Lua 解耦的模式。

```
Lua VM (tokio task)                        IO task（独占该 serial/tcp/udp，单实例一个）
  ctx:request(frame,100)                      loop { select! {
    │                                            req = io_rx.recv() → writer.write_all(&req.frame)
    │ create_async_function                       read 帧（透传：读到帧尾/超时/定长，见下）
    ▼                                            req.reply_to.send(frame)   // 按 req_id 配对
  mpsc::send(IoRequest{req_id, reply_to})      on_data frame → on_data_rx（触发脚本 on_data）
    ▼                                          reconnect on transport err
  timeout(oneshot_rx).await                     }
    └─ coroutine.yield 期间 tokio reactor 释放 ──→ 结果 resume 回 Lua
```

- **三传输复用 modbus_client 的 transport 模式**：`AsyncRead+AsyncWrite+Unpin+Send` 泛型，IO task 代码三传输几乎一致；UDP 用 connected socket 保 datagram 边界（同 modbus rtu_udp）。
- **连接唯一所属**：IO task 独占流，无跨任务借用；连接生命周期（connect/reconnect）由 Rust 管，Lua 只感知「这次 request 成功/失败」。
- **request_id 配对 + 超时丢包**：每个 request 带递增 req_id，IO task 按req_id 回执；`tokio::time::timeout` 包 oneshot，超时后迟到回包丢弃（不错配）。
- **透传 framing**：脚本自己拼包/CRC；IO task 的「读一帧」策略由 `app_config.recv_mode` 配：
  - `timeout`（默认）：发完 request 后读到静默或超时返回累积字节；
  - `length`：`len_prefix bytes` + 长度字段（位置/宽度可配）；
  - `delimiter`：读到指定结尾字节序列；
  - `fixed:N`：读固定 N 字节。
- **on_data 被动帧**：IO task 在「无在飞 request」时收到的帧，经独立 channel 触发脚本 `on_data`（广播/推送场景）。

## 9. 沙箱与错误隔离（三层护栏）

1. **库裁剪**：`Lua::new_with(StdLib::TABLE|STRING|MATH|COROUTINE, LuaOptions::new().catch_rust_panics(true))`。剔除 `io/os/debug/package`，从根断危险入口。**必须保留 `COROUTINE`**——mlua async 桥接内部依赖它。
2. **指令 hook**：`Lua::set_hook(HookTriggers::new().every_nth_instruction(1_000_000), hook_fn)`，`hook_fn` 返回 `Err(mlua::Error::RuntimeError(...))` 中断执行，挡死循环（kill 本次调用，VM 不死）。**注意 `every_nth_instruction(n: u32)` 是 builder 方法（吃 `self`，需 `HookTriggers::new()` 起步），不是单参数构造器**——已核实 mlua 0.12.0 `src/debug.rs`；`set_hook` 回调签名 `Fn(&Lua, &Debug) -> Result<VmState>`，返回 Err 即传播中断（官方文档原文）。hook 阈值 `1_000_000` 留作 `app_config.hook_instr_limit` 可配。
3. **内存限额**：`set_memory_limit(cfg.mem_limit_mb * 1024 * 1024)`（默认 32MB），挡爆内存，触发 `MemoryError`。
4. **per-VM 独立 tokio task + `catch_rust_panics`**：单脚本崩/panic 不影响进程；mlua 把 Rust callback panic 转 RuntimeError。
5. **脚本 error 降级**：collect/on_write/on_init/on_data 内 `error()` 被 `pcall`/`Result` 捕获 → `warn!` + 跳过本次 → VM 继续下周期。
6. **连续失败标记 degraded**：连续 N 次 collect 失败（N 可配，默认 10）→ 实例标记 `degraded`（写 lux `stat:<inst>`），**不杀实例**，下周期继续重试。

## 10. 写命令链路（零改 CtlRegistry）

```
MQTT <hwid>/command 或 POST /api/v1/command/send
  → InstanceManager::send_ctl
  → CtlRegistry.send(inst, CtlMessage)        // try_send(64)，非阻塞，满丢弃
  → 实例 cmd_rx
  → luaClient::run match Evt::Cmd
  → ctx 调 on_write(ctx, cmd_table)           // cmd.data JSON 经 lua.to_value 转 table
  → 脚本内 ctx:request 发出写帧
```
- `cmd.data` 是 JSON 文本，脚本自解析（与 modbus/opcua/s7 完全一致）。
- `on_write` 返回 bool 表示成功与否；失败仅 warn（与 modbus 写失败语义一致）。
- 实例未注册/未运行 → `send_ctl` 返回 false → REST 404（复用现有 routes_command 逻辑）。

## 11. kv 持久化（lux 新 hash）

- 新 hash 表：`luakv:<inst_id>`，field = key，value = JSON（支持 number/string/bool/table-可序列化）。
- 读写放 `src/db/luakv.rs`：`get(lux, inst, key)` / `set(lux, inst, key, value)`，经 `EmbeddedClient.execute("HGET"/"HSET")`。
- 须更新 `src/db/identity.rs:3-11` 的 hash 清单注释，加 `luakv:<instId>`（脚本本地持久化状态）。
- key 限制 `[A-Za-z0-9_.-]`，单值大小上限（如 16KB），防滥用。
- 实例删除时清理对应 `luakv:<inst_id>`（在 routes_inst::remove 内，类似 ext_tokens 清理）。

## 12. HTTP 出站与审计（feature `lua-http`）

- `ctx:http_get/http_post` 基于 `reqwest`（同步语法、底层 async，同 `ctx:request` 模式：create_async_function + yield）。
- feature `lua-http`（默认随 `lua` 开），可独立关闭裁 release / 减少出站风险面。
- **全审计**：每次出站写 `tracing` INFO 日志 `[lua:<name>] HTTP GET <url> -> <status> (<ms>ms)`；失败也记。
- 默认超时 5s（可 app_config 配 `http_timeout_secs`）；响应体大小上限（如 1MB）防爆内存。
- v1 不做白名单/域名限制（靠审计日志追溯）；白名单为 v2 后置项。

## 13. 状态监测与日志

- **日志**：`tracing`，所有 Lua 脚本日志带前缀 `[lua:<name>]`，与项目 `[modbusClient]`/`[opcuaClient]` 约定一致。
- **状态指标**（写 lux `stat:<inst_id>`，JSON）：
  - `status`: running / degraded / stopped
  - `collect_count` / `collect_fail_count`（及最近失败原因）
  - `last_collect_ts` / `last_collect_duration_ms`
  - `vm_restart_count`（热加载/崩溃恢复次数）
  - `io_connected`: bool / `last_io_err`
  - `script_version`（脚本文件 hash，标识当前运行的版本）
- **REST 端点**（`src/api/routes_lua.rs`，JWT 保护，默认）：
  - `GET /api/v1/lua/<inst>/stats` → 读 lux `stat:<inst>`
  - `POST /api/v1/lua/<inst>/reload` → 手动触发热加载
  - `GET /api/v1/lua/scripts` → 列出 scripts/ 下可用脚本

## 14. 配置示例

### InstConfig（REST 建实例）
```jsonc
{
  "inst_id": "lua-devA",
  "inst_name": "devA 采集",
  "app_code": "luaClient",
  "app_type": "South",
  "host_kind": "Builtin",
  "auto_start": true,
  "app_config": {
    "script": "devA",                       // → scripts/devA/main.lua
    "transport": {
      "kind": "serial",                     // serial | tcp | udp
      "serial_port": "COM3",                // serial 专用
      "baud_rate": 9600, "data_bits": 8, "parity": "none", "stop_bits": 1, "flow_control": "none"
      // tcp/udp: { "kind":"tcp", "endpoint":"192.168.1.10:502" }
    },
    "period": 2,                            // collect 周期秒；0 = 纯被动
    "timeout_ms": 200,                      // ctx:request 默认超时
    "recv_mode": { "mode": "timeout", "idle_ms": 50 },  // timeout|length|delimiter|fixed
    "reconnect_secs": 5,
    "mem_limit_mb": 32,
    "fail_threshold": 10,                   // 连续失败 N 次标记 degraded
    "enable_http": true,                    // http 开关（feature lua-http 时生效）
    "http_timeout_secs": 5,
    "dump_comm": false                      // 原始报文落日志
  },
  "devices": [ { "value": "devA", "label": "设备A", "children": [ {"value":"temp","label":"温度"} ] } ]
}
```

`devices[].children` 是声明式点位，脚本可选用于 set_value 校验/UI 展示；脚本不被强制使用（脚本可自由产出任意 tag）。

## 15. 测试策略

测试复用 rs-iot 现有范式：`tests/common::start_lux`（端口 0 + 临时 data_dir）+ `#[tokio::test]` + `hget_cell` 验 rtdata + `mgr.send_ctl` 验写命令。**`tests/s7_e2e.rs` 是黄金参考**（`TcpListener` mock 协议 server + `ServerState{mem,writes}` + Source 实例 + pipeline 落 rtdata + 闭环断言），Lua 端到端测试直接复用其骨架，把 mock S7 server 换成 mock Modbus/自定义协议 server。

### 15.1 单测（`#[cfg(test)]` 模块内）
- VM 沙箱：`io`/`os`/`debug`/`package` 不可访问（断言 `pcall(io.read)` 失败）；
- 指令 hook：插入 `while true do end` 脚本，断言被 kill 且 VM 存活（下个 collect 正常）；
- 内存限额：疯狂 `s = s .. "x"` 拼接，断言 MemoryError；
- config 解析：各字段缺省/非法/越界；
- ctx API 契约：每个 host 函数 happy path + error path（超时/IO 错/空值）；
- **loader.rs 沙箱**：`require("../../etc/passwd")` 失败、`require("lib.foo")` 限定在 `scripts/<name>/lib/`（验证接管了 `package.searchers`）。

### 15.2 集成测（`tests/lua_*.rs`，CI 内，无真机）

每类测试闭环：起 lux + mock 协议 server（127.0.0.1:0）+ luaClient 实例（加载 Lua 脚本）→ 周期采集 → `HGET rtdata:<devid>` 验值 → `send_ctl` 验写命令。`MockIoTask` 可注入 IO 层做纯逻辑测；真实协议测用 `TcpListener` mock server（同 s7_e2e）。

#### 测试 A：标准 Modbus TCP（Lua 重写 modbus 客户端，表达力上限）
- **协议**：MBAP 头 7B + FC03 读 / FC06 写。
- **mock server**：`ServerState{regs:HashMap<u16,u16>, writes:Vec}`；解析 FC03 请求的 `start/qty` 回寄存器值（大端 u16）；解析 FC06 落 `writes` + echo。
- **Lua 脚本**（`scripts/it_modbus/main.lua`）：
  ```lua
  function collect()
    -- MBAP(tid,pid,len=6,uid) + FC03(fc=3,addr=0,qty=2)，全大端
    local req = string.pack(">I2I2I2BBI2I2", 1, 0, 6, 1, 3, 0, 2)
    local resp = ctx:request(req, 200)
    if not resp or #resp < 11 then return nil end
    local r0, r1 = string.unpack(">I2I2", resp:sub(10))   -- 跳 MBAP(7)+fc(1)+bc(1)
    return { temp = r0/10, speed = r1 }
  end
  function on_write(c)
    local req = string.pack(">I2I2I2BBI2I2", 2, 0, 6, 1, 6, c.addr, c.value)
    return ctx:request(req, 200) ~= nil
  end
  ```
- **断言**：预播 regs[0]=125, regs[1]=32000 → `HGET rtdata:devM devM.temp` 含 `12.5`、`devM.speed` 含 `32000`；`send_ctl{addr=0,value=1}` → mock `writes` 收 FC06 帧 + addr/value 正确。

#### 测试 B：类 Modbus 变种（自定义校验 + 小端地址，背景核心场景）
- **协议**：`[0xAA 头][unit][fc][addr_le][qty][累加和校验]`，响应同构；mock 可配置「下次响应带坏校验」。
- **mock server**：按变种格式响应；提供 `poison_next_checksum()` 触发一次坏校验响应。
- **Lua 脚本**（`scripts/it_modbus_like/main.lua`）：
  ```lua
  local function ck(b) local s=0 for i=1,#b do s=(s+b:byte(i))&0xFF end return s end
  function collect()
    local body = string.pack("BBBB", 1, 3, 0, 2)                -- unit,fc,addr_le,qty
    local resp = ctx:request("\xAA"..body..string.char(ck(body)), 200)
    if not resp or resp:byte(1)~=0xAA then return nil end
    if resp:byte(-1)~=ck(resp:sub(2,-2)) then return nil end    -- 校验失败拒收
    local v = string.unpack("<I2", resp:sub(3,4))               -- 小端
    return { pressure = v/100 }
  end
  ```
- **断言**：正常响应 → rtdata `pressure` 正确；mock 发坏 checksum → 脚本拒收、rtdata 维持上次值（验证校验逻辑生效，对标背景「规范性不好」的厂商协议）。

#### 测试 C：完全自定义协议 + 广播推送（验证 `on_data` 被动收报）
- **协议**：`[STX=0x02][cmd][len][payload...][ETX=0x03]`；mock server 周期主动推心跳广播 + 响应查询。
- **mock server**：spawn 定时推心跳 task（`02 10 02 <seq> 03`）+ 响应查询帧（`02 01 01 <level> 03`）。
- **Lua 脚本**（`scripts/it_custom/main.lua`）：
  ```lua
  function collect()
    local r = ctx:request("\x02\x01\x00\x03", 200)             -- 主动查询
    if r and #r >= 6 then return { level = r:byte(4) } end
  end
  function on_data(frame)                                 -- 被动广播
    if frame:byte(1)==0x02 and frame:byte(2)==0x10 then
      ctx:set_value("heartbeat", ctx:time())
    end
  end
  ```
- **断言**：`rtdata:devC` 同时含 `level`（collect 产出）和 `heartbeat`（on_data 产出）——证明一问一答与被动广播两条产出路径都闭环。

#### 测试 D-G：能力与韧性（贯穿，CI 内）
- **D 联动**：脚本 A `ctx:send_command("instB",...)` → 实例 B 的 on_write 收到（双实例 setup）。
- **E kv 持久**：`ctx:kv_set("k",v)` → 重建 VM（热加载）后 `kv_get("k")` 命中（验证落 lux `luakv:`）。
- **F 热加载**：改 `scripts/it_modbus/main.lua`（返回值 +1）→ 去抖 → rtdata 值变化；故意改坏脚本 → 旧脚本继续跑（失败回退）。
- **G 韧性**：mock 不响应 → collect 超时返回 nil → rtdata 不崩、实例不挂、`stat` 标 degraded；mock 主动断连 → 下 tick 重连恢复采集。
- **H HTTP**（feature `lua-http`）：mock HTTP server（如 `wiremock` 或裸 `TcpListener`）验 `http_get` + 审计日志含 url/status/耗时。
- **I send feature 红线**：压测「高频 `ctx:request` + 持续 on_data 广播」长跑 30s，断言无 Lua 栈错乱/无 panic（守 §18 ReentrantMutex 红线）。

### 15.3 端到端（手动，CI 不跑）
真实串口/TCP 设备验证（对标 modbus_client/s7Client 的 e2e 段）：REST 建 `app_code="luaClient"` 实例 + 脚本 → `redis-cli -p 6390 HGETALL rtdata:<devid>` 验采集；写命令经 MQTT `<hwid>/command` 或 REST。

### 15.4 CI
`cargo fmt --all --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test --all-targets --all-features`；mlua vendored 在 Windows + Linux 均编译通过（CI 矩阵）。

## 16. feature 门控与依赖

### Cargo.toml（feature `lua` 与 `lua-http` 默认开启）
`lua` / `lua-http` 默认开启（加入 `[features].default`，与现有 `transform` feature 默认开同构）；build 时 `--no-default-features` 可关闭以裁 release / 减少出站风险面。
```toml
[features]
# default = [..., "lua", "lua-http"]   # 实现时按现有 Cargo.toml 的 default 结构对齐（与 transform 并列）
lua = ["dep:notify", "dep:notify-debouncer-mini", "dep:mlua"]
lua-http = ["lua", "dep:reqwest"]      # ctx:http_*；独立可关

[dependencies]
mlua = { version = "0.12", features = ["lua54", "vendored", "async", "send", "serde", "macros"], optional = true }
notify = { version = "8", optional = true }
notify-debouncer-mini = { version = "0.7", optional = true }   # 0.7 依赖 notify ^8.2；0.5 要 notify 7 会冲突（已核实 crates.io）
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json"], optional = true }  # lua-http；rustls-tls 独立不拉 OpenSSL，json 供 http_post body 序列化
# tokio-serial 已被 modbus_client 引入，复用
```
- `lua_client` 模块整体 `#[cfg(feature = "lua")]` 门控（参考 transform/mod.rs `#[cfg(feature="transform")]`）。
- 关 `lua` feature 时，`app_code="luaClient"` 会在 `find_source` 返回 None → 建实例报错（与未编译插件一致）。

### mlua feature 说明
- `lua54`：Lua 5.4 后端；`vendored`：cc 编译 C 源码进二进制（Windows/Linux 交叉编译友好）；`async`：`create_async_function`/`call_async`；`send`：`Lua` 变 Send+Sync（跨 await 持有，代价 ~10-20% 性能，秒级采集无感）；`serde`：`to_value`/`from_value`；`macros`：`#[derive(UserData)]`。

## 17. v1 范围与 v2 后置

### v1（本 spec 交付，全量 L1-L6）
- L1-L6 全部 ctx API（含 http + crc helper）；
- serial/tcp/udp 三传输 + 透传 framing（4 种 recv_mode）；
- 沙箱三层护栏 + 热加载 + kv + 联动 + 状态监测 + REST；
- 完整测试（单测 + 集成 + mock）。

### v2 后置（低频/复杂，本 spec 不实现）
- HTTP 出站白名单/域名限制（v1 靠审计日志）；
- 细粒度 raw TCP/UDP stream 控制（长连接推送、自定义成帧，v1 的 request+on_data 已覆盖大多数）；
- cron 表达式定时（v1 用 collect 周期 + ctx:sleep）；
- 脚本间消息总线（v1 经 lux kv/get_value 间接）；
- 脚本版本管理/回滚（v1 单文件 + 热加载，靠 git 管版本）；
- Luau 后端切换（v1 用 5.4；Luau 作 PoC 比对项）。

## 18. 风险与未覆盖点

> 以下风险已经两个核实 agent（mlua API + async/runtime）用权威源（docs.rs/GitHub `mlua-rs/mlua` v0.12.0 源码/context7/crates.io）交叉验证，详见 §20。

- **send feature ReentrantMutex 跨 yield 借用（核实确认）**：mlua 0.12 `send` feature 用 ReentrantMutex 保护 VM，async fn 在 `.await` 点释放锁；若另一 task 同时操作同一 VM → Lua 栈错乱（rs-freeioe `sys.fork` 场景的 gotcha）。**spec「每实例一 VM + §5.4 同 select! 循环顺序执行」已规避**（collect/on_write/on_data 不并发）。**实现红线**：collect/on_write/on_data/http_get 必须全部在同一 Source task 的 select! 循环内顺序执行，禁止拆独立 task 并发操作同一 VM；`add_async_method_mut` 闭包参数必须 `mut this`（否则 E0596）。测试 I 专项压测。
- **mlua 0.12 较新（2026-07-05 发布，约 1 个月）**：与项目已知「年轻依赖」风险同类（同 tokio-s7/tokio-modbus）；生产前必实测真机长稳 + 断连恢复。
- **`new_with` 禁 package 不彻底（核实确认）**：`Lua::new_with(StdLib::...)` 请求 PACKAGE 时只 `disable_c_modules()`（禁 C 模块加载），**Lua 端 `require` 机制本身仍在**。**实现红线**：`loader.rs` 必须真正接管 `package.searchers`/`package.loaders`（自定义 resolver 限定在 `scripts/<name>/lib/`），否则脚本仍可 `require` 全局路径逃逸沙箱（单测覆盖）。
- **`catch_rust_panics(true)` 副作用（核实确认）**：开启后 ctx host 函数里的 Rust panic 被转 RuntimeError 而非 unwind（符合「单脚本崩不杀进程」）；但副作用是 host 侧真正的 invariant 违背（程序员错误）也会被静默降级为 Lua error。**实现红线**：host 函数对「不应发生的程序员错误」应返回 `Err` 而非 panic，避免被吞。
- **`to_value(&Null)` 产出 truthy userdata 非 nil（核实确认）**：mlua 0.12 已知行为（rs-freeioe `lua_actor.rs:211-215` 记录）。**实现红线**：ctx 的 `kv_get`/`get_value` 返回空时显式判 `is_null()` 返回 `mlua::Value::Nil`，否则破坏 Lua `data or default` 惯用法。
- **Lua 5.4 纯解释性能**：秒级一问一答场景瓶颈是 IO 等待非 CPU，性能够；重计算靠指令 hook 兜底而非 JIT（指令 hook 本身 ~10% 量级开销，阈值 `hook_instr_limit` 可配）。
- **notify 在 Windows 上偶有事件丢失**：ReadDirectoryChangesW 已知行为；去抖 + REST 手动 reload 兜底（§5.3/§13），集成测验证。
- **tokio-serial 在 Windows 半双工**：一问一答（串行）无影响，IO task 避免同时 read+write（同 modbus_client 经验）。
- **HTTP 出站滥用**：v1 默认开 + 审计日志，无白名单——现场敏感时关 `enable_http` 或 `lua-http` feature。
- **未覆盖**：真实多语言/多协议现场压测（需真机）；脚本市场/分发（本 spec 只管本地 scripts/ 目录）。

## 19. 关键技术决策（为什么这样选）

1. **集成而非独立二进制**：mlua 的「指令 hook + 内存限额 + 每 VM 独立 task」三层隔离对秒级场景已足够；独立二进制额外带来 IPC 序列化、进程生命周期/token/崩溃恢复、失去 lux 进程内零拷贝访问。sidecar 路线的价值是「任意语言」，Lua 本就为嵌入而生。
2. **每实例一 VM（非单 VM 多协程）**：rs-iot「一协议一实例」语义 + 故障/状态隔离最强；mlua `send` feature 让 VM 跨 await 持有，配合 per-VM task 实现并发又不跨线程共享 VM 状态。
3. **channel + 独占 IO task（模式 Z）**：唯一不让 reactor 被占的姿势；连接生命周期与 Lua 解耦；天然支持多脚本复用、超时统一、断链重连（与 modbus_client `Option<Context>` 同构）。
4. **回调式编程模型**：`collect` 覆盖一问一答、`on_data` 覆盖广播，入门门槛最低；纯事件式/自管循环门槛高且易写错。
5. **透传 framing**：厂商自定义协议格式各异，宿主不假设；4 种 recv_mode 配置覆盖常见成帧需求，脚本仍可自己拼包/CRC。
6. **API 走现有总线**：`get_value` 走 lux 读出口、`send_command` 走 CtlRegistry、`kv` 走 lux 新 hash——所有跨边界能力复用 rs-iot 双总线，守 single source of truth，不破坏架构。
7. **库裁剪 + hook + memory 三层护栏**：库裁剪断危险入口、hook 挡死循环、memory 挡爆内存——工业网关里把「实施工程师写错的脚本」挡在 Source 边界，单脚本绝不拖垮进程。

## 20. 可行性核实结论（2026-08-06 双 agent 交叉验证）

为支撑「任务重大、反复核实」，派两个 agent 用权威源（crates.io / docs.rs / GitHub `mlua-rs/mlua` v0.12.0 源码 / context7）逐条核实 spec 的技术假设。

### 20.1 mlua API 核实（逐条）

| spec 声称 | 核实 | 证据 |
|---|---|---|
| mlua 0.12 已发布 | ✅ | crates.io 最新稳定 = 0.12.0（2026-07-05，非 rc）；`version="0.12"` 解析为 ^0.12 取 0.12.0，无需改 |
| feature `lua54`/`vendored`/`async`/`send`/`serde`/`macros` 独立 | ✅ | v0.12.0 Cargo.toml；`send` 连带 `error-send`；无 `async-send` 合并名；`serde` 是 0.12 正名（**勿回退到 rs-freeioe 的 0.11 旧名 `serialize`**） |
| `Lua::new_with(StdLib, LuaOptions)` 双参 + 库裁剪 | ✅ | src/state.rs；`TABLE\|STRING\|MATH\|COROUTINE` 合法且排除 IO/OS/DEBUG/PACKAGE；非 luau 后端显式拒 DEBUG |
| `LuaOptions::catch_rust_panics(true)` | ✅ | 默认 false，显式 true 正确 |
| `set_hook` + `HookTriggers::new().every_nth_instruction(N)` | ✅（已修硬错误） | src/debug.rs；`every_nth_instruction` 是 builder 方法（吃 self），非单参数构造器；回调 `Fn(&Lua,&Debug)->Result<VmState>` 返回 Err 中断 |
| `set_memory_limit(bytes)` per-state | ✅ | src/state.rs；超限 MemoryError |
| `send` → `Lua: Send+Sync`（代价 ~10-20%） | ✅ | README + src/state.rs 静态断言 + v0.10 release_notes |
| async coroutine 桥接（yield ↔ tokio poll） | ✅ | README "using Lua coroutines"；rs-freeioe async_bridge.rs 实证 |
| serde `to_value`/`from_value` | ✅ | LuaSerdeExt（serde feature） |
| macros `#[derive(UserData)]` + `chunk!{}` | ✅ | docs/UserData.md、docs/chunk.md |
| UserData `add_async_method(_mut)` | ✅ | docs/UserData.md（async feature） |
| `Lua::sandbox` 仅 Luau | ✅ | spec 用库裁剪替代，lua54 不调 sandbox |

### 20.2 async 桥接 / runtime / 依赖核实（逐条）

| spec 声称 | 核实 | 证据 |
|---|---|---|
| rs-iot runtime = multi_thread → send feature 必需 | ✅ 硬约束 | `src/main.rs:44` `#[tokio::main]` 默认 multi_thread + `Cargo.toml:15` tokio full（含 rt-multi-thread）；spawn 持 Lua 的 future 必须 Send |
| async fn：Lua yield ↔ tokio poll ↔ reactor 释放 | ✅ | context7 官方示例 + rs-freeioe async_bridge.rs 实证 |
| 模式 Z：mpsc.send + timeout(oneshot) 在 async fn 内挂起协程 | ✅ | rs-freeioe `lua_port.rs::request` 同构实现 |
| send ReentrantMutex 跨 yield 坑 | ⚠️ 存在但被单 task 规避 | docs.rs "reentrant mutex"；rs-freeioe `lua_actor.rs` sys.fork gotcha；spec §5.4 单循环顺序执行规避（实现红线见 §18） |
| notify 8 + debouncer-mini 0.5 | ❌ 版本冲突（已修） | crates.io：debouncer-mini 0.5 要 notify 7；已改 0.7（要 notify ^8.2） |
| reqwest rustls-tls 独立（不拉 OpenSSL） | ✅ | docs.rs/reqwest；已补 `json` feature |
| tokio-serial `SerialStream` 实现 AsyncRead+AsyncWrite+Unpin+Send | ✅ | modbus `transport.rs:47-55` 实证 |

### 20.3 结论

**spec 技术选型整体成立**：mlua 0.12 + lua54 + vendored + async + send + serde + macros，配 multi_thread runtime + 模式 Z IO + 单 task 顺序执行——全部经源码级核实，有 rs-freeioe 全链路实战佐证。核实发现并已修正：① `set_hook` builder 写法（§9.2）；② notify 版本冲突（§16）。新增强实现红线 4 条（§18）。**无阻塞性硬伤，可进入 writing-plans。**

### 20.4 权威源

- mlua 0.12 docs：https://docs.rs/mlua/0.12.0/mlua/
- mlua GitHub：https://github.com/mlua-rs/mlua（v0.12.0 tag 源码）
- crates.io：mlua / notify / notify-debouncer-mini / reqwest / tokio-serial
- rs-freeioe 实战佐证：`D:\rs_codes\rs-freeioe\crates\ioe-app-sdk\src\lua_port.rs`、`ioe-runtime\src\lua_actor.rs`、`ioe-lua\tests\async_bridge.rs`、`.remember\remember.md`
- rs-iot runtime：`src\main.rs:44`、`Cargo.toml:15,68`、`src\plugin\builtin\modbus_client\transport.rs:47-55`

---

## 附：实现里程碑（供 writing-plans 拆分参考）

1. **M1 引擎底座**：mlua 引入 + feature 门控 + VM 创建（沙箱/hook/memory）+ 脚本加载编译 + on_init 调通 + 主循环骨架（照抄 modbus）。
2. **M2 IO 引擎**：MockIoTask + serial/tcp/udp 三传输 + channel+独占 task + request 超时 + on_data 触发。
3. **M3 ctx API（L1+L2+L6）**：request/on_data/set_value/get_value/get_device/list_devices/log/now/time/config/dump_comm/crc + collect 返回 Sample 推 hub。
4. **M4 联动与持久**：send_command（L3）+ kv（L4，含 luakv.rs + identity 清单 + 实例删除清理）。
5. **M5 HTTP（L5）**：lua-http feature + reqwest + http_get/post + 全审计日志。
6. **M6 热加载**：notify + debouncer + 重建 VM + dry-run + 失败回退。
7. **M7 状态与 REST**：stat:<inst> 指标采集 + routes_lua（stats/reload/list）。
8. **M8 测试与文档**：单测 + 集成测（mock）+ lua-script-guide.md + examples/。
