# rs-iot 插件开发指南

> **状态**：活文档（权威规范，随插件体系演进同步）
> 本文档是 rs-iot 插件体系的**权威开发规范**。内置 Source/Sink/Processor、外部 sidecar 插件、
> 北向发布、命名约定、目录结构、测试与质量门槛均在此规定。新增插件前必读；review 插件 PR 时
> 以本指南为准绳。
>
> 相关文档：外部插件二进制协议详见 [`external-plugin-protocol.md`](../reference/external-plugin-protocol.md)；
> 工业协议库选型详见外部资料《RUST生态-工业协议研究报告》（仓库外本地目录，未入库）。

## 目录

- [1. 插件体系总览](#1-插件体系总览)
- [2. 命名规范（强制）](#2-命名规范强制)
- [3. 目录结构规范](#3-目录结构规范)
- [4. 内置 Source 开发（南向采集）](#4-内置-source-开发南向采集)
- [5. 内置 Sink 开发（北向发布）](#5-内置-sink-开发北向发布)
- [6. Processor 开发（L2 数据转换）](#6-processor-开发l2-数据转换)
- [7. 外部插件开发（sidecar bridge）](#7-外部插件开发sidecar-bridge)
- [8. 通用数据契约](#8-通用数据契约)
- [9. 测试规范](#9-测试规范)
- [10. 验证与质量门槛](#10-验证与质量门槛)
- [11. 迁移与兼容](#11-迁移与兼容)
- [12. 附录：新增插件检查清单](#12-附录新增插件检查清单)

---

## 1. 插件体系总览

rs-iot 是工业协议网关：**南向 N 种设备协议 × 北向 M 种对接协议**。插件体系用分层 + 双总线把
N×M 网状耦合降维成 N+M——每个协议只对接总线，不互相适配。

### 1.1 三类插件（按职责正交）

| trait | 职责 | 方向 | 现有实现（10 个，2026-10-31；feature 门见 §3.4） |
|---|---|---|---|
| `Source` | 产出数据，推入 hub | 南向（设备 → rs-iot） | dataSim / selfMonitor / sidecarClient / modbusClient / opcuaClient / s7Client / eipClient / luaClient / **luaTransform**（订阅型：消费 EventBus 而非真机采集） |
| `Sink` | 消费数据，发往外部 | 北向（rs-iot → 云/总线） | **luaNorth**（唯一 Sink，MQTT 北向；旧 mqttPub 已删，T8 无损迁移为默认 luaNorth 实例，见 §5） |
| `Processor` | 转换 Sample | 中间（hub → pipeline） | passthrough（恒定）/ rhai（feature `transform`，**Q7 冻结不接线**，见 §6） |

契约定义见 `src/plugin/traits.rs`；权威注册清单 = `src/plugin/builtin/mod.rs` 的模块声明（带 feature 门）。
运行时清单 = `GET /api/v1/plugins`（返回**当前编译面**的 sources/sinks，裁剪构建下 s7/eip 不在列表）。

### 1.2 三层形态（按运行位置）

| 层 | 形态 | 注册方式 | 适用 |
|---|---|---|---|
| **L1 内置** | Rust trait，编进单一二进制 | `inventory` 静态注册 | 主流协议（OPC UA/Modbus/S7），零开销 |
| **L2 外部** | 进程外任意语言，经 TCP bridge 接入 | `host_kind=external` + token | 协议 rs-iot 暂不支持、或复用既有 C/Go/Python 采集库 |
| **L3 WASM** | wasmtime/extism 扩展（路线） | 动态加载 | v2，隔离沙箱 + 热加载 |

### 1.3 双总线架构

```
── 数据上行总线 ──────────────────────────────────────────────
N × Source ──► hub(mpsc 512) ──► pipeline(Processor) ──► lux rtdata:<devid>
   读出口（都从 lux rtdata 取最新值）：luaNorth 北向实例(pull) / GET /api/v1/rtdatas

── 控制下行总线（CtlRegistry）──────────────────────────────
MQTT <hwid>/command / POST /api/v1/command/send ──► CtlRegistry.try_send(64)
   ──► 实例 Source::run 的 cmd_rx ──► handle_write
```

> MQTT `<hwid>/command` 订阅随 T8 迁移落在默认 luaNorth 实例的 down 回调
> （`lua_north/compat.rs`，订阅项由宿主配置生成），不再有独立 mqtt_pub 模块。

- **南向采集与北向发布完全解耦**：Source 只写 hub，Sink 只从 lux rtdata 读（pull）或从独立
  channel 读（push）。lux rtdata 是唯一数据交汇点。
- **单 Source 启停不影响管道**：所有 Source 输出汇聚到一个共享 hub，pipeline 单消费者
  （见 `src/plugin/instance.rs` `InstanceManager`）。

### 1.4 核心解耦机制（五条）

1. **trait 契约**——框架依赖抽象（Source/Sink/Processor），不依赖具体协议实现
2. **channel 解耦**——hub（512，`lib.rs` 构造）+ cmd（64）异步边界 + 背压；写命令 `try_send`
   非阻塞，channel 满丢弃（对标 Go 版 `default` 语义，**勿改阻塞**）
3. **lux 作进程内数据总线**——RESP 契约，上下游无直接引用
4. **`inventory` 静态注册**——编译期发现，无注册顺序依赖，编进单一二进制
5. **`CancellationToken`**——独立启停；控制面（cmd）不阻塞数据面（hub）

---

## 2. 命名规范（强制）

插件有**三层命名**，必须从注册名机械派生，保证可互推。

### 2.1 三层命名映射规则

| 层 | 规则 | 从注册名派生 | 示例 |
|---|---|---|---|
| **注册名** `app_code` | lowerCamelCase | （基准） | `s7Client` |
| **目录名** | snake_case | `s7Client` → `s7_client` | `s7_client/` |
| **struct 名** | UpperCamelCase | `s7Client` → `S7Client` | `S7Client` |

互转规则：lowerCamelCase ↔ snake_case（在大小写边界插 `_` 再小写）；lowerCamelCase ↔
UpperCamelCase（首字母大写）。

### 2.2 注册名锁定（持久化契约）

**注册名（`app_code`）一经发布即锁定，不得改名**。原因：

- 它持久化在 lux `instances` Hash 表（`HGET instances` 的 JSON `app_code` 字段）
- REST 建实例、MQTT 写命令路由、`registry::find_source(name)` 都按它匹配
- 改名 = 破坏所有已部署实例的配置（启动后找不到插件 → 实例无法拉起）

如果必须改，只能新增别名映射（`find_source` 兼容老名），不得直接 rename。

### 2.3 现状清单与偏差（10 个，2026-10-31）

| 注册名 | 目录名 | struct | feature | 状态 |
|---|---|---|---|---|
| `dataSim` | `data_sim/` | `DataSim` | — | ✓ 已规范化（原 `datasim.rs` 连写，2026-08 修正） |
| `selfMonitor` | `self_monitor/` | `SelfMonitor` | — | ✓（自监控设备 devid=rsiot） |
| `sidecarClient` | `sidecar_client/` | `SidecarClient` | — | ✓ |
| `modbusClient` | `modbus_client/` | `ModbusClient` | modbus（默认开） | ✓（拆子模块，见 [§3.1](#31-子目录结构目标态)） |
| `opcuaClient` | `opcua_client/` | `OpcuaClient` | opcua（默认开） | ✓ |
| `s7Client` | `s7_client/` | `S7Client` | s7（默认关） | ✓ |
| `eipClient` | `eip_client/` | `EipClientPlugin` | eip（默认关） | ⚠ struct 带.Plugin 后缀，是唯一偏离三层派生的存量偏差（保留不改，记录在案） |
| `luaClient` | `lua_client/` | `LuaClient` | lua（默认开） | ✓ |
| `luaTransform` | `lua_transform/` | `LuaTransform` | lua（默认开） | ✓（Source 订阅型，非 Processor） |
| `luaNorth` | `lua_north/` | `LuaNorth` | lua（默认开） | ✓（唯一 Sink；旧 mqttPub 已删，见 §5） |

新增插件必须从注册名严格派生目录名与 struct 名（存量偏差 `EipClientPlugin` 不作为先例）。

---

## 3. 目录结构规范

### 3.1 子目录结构（目标态）

```
src/plugin/builtin/
├── mod.rs                  # 仅 `pub mod xxx;` 声明（一行一插件，字母序；协议插件带 #[cfg(feature)]
├── data_sim/               # —（无条件编译）
├── self_monitor/           # —
├── sidecar_client/         # —（或拆 acceptor.rs / codec.rs / frame.rs）
│   └── mod.rs
├── eip_client/             # #[cfg(feature = "eip")]
├── lua_client/             # #[cfg(feature = "lua")]
├── lua_north/              # #[cfg(feature = "lua")]（唯一 Sink + compat 默认北向实例）
├── lua_transform/          # #[cfg(feature = "lua")]
├── modbus_client/          # #[cfg(feature = "modbus")]；大插件：拆子模块（原 1733 行 → 6 文件）
│   ├── mod.rs              # Source impl + handle_write + Evt + inventory + tests（~960 行）
│   ├── config.rs           # Transport / ModbusCfg / parse_cfg
│   ├── address.rs          # Func / Type / Order / PointSpec / parse_* / addr_*
│   ├── codec.rs            # 字节序 perm / decode / encode / decode_point / encode_value
│   ├── merge.rs            # Region / merge_regions
│   └── transport.rs        # connect / UdpIo / 串口参数映射
├── opcua_client/           # #[cfg(feature = "opcua")]；中等插件：单 mod.rs 足够
│   └── mod.rs
└── s7_client/              # #[cfg(feature = "s7")]
    └── mod.rs
```

**规则**：
- 每个插件一个**唯一命名的子目录**（目录名 = 注册名 snake_case，见 [§2.1](#21-三层命名映射规则)）
- 子目录内至少一个 `mod.rs`；复杂插件可拆同级 `.rs` 子模块，由 `mod.rs` 用 `mod xxx;` 声明并
  `pub use` 重导出对外项
- `builtin/mod.rs` **只放 `pub mod xxx;` 声明**，不放任何逻辑

### 3.2 单文件 vs 多文件（阈值）

| 插件规模 | 结构 | 判断依据 |
|---|---|---|
| < 500 行 | 单 `mod.rs` | datasim / self_monitor / opcua / s7 / sidecar |
| 500–1000 行 | 单 `mod.rs` 仍可接受 | 接近上限时评估拆分 |
| > 1000 行 | **拆子模块** | modbus_client（~1700 行）应拆 transport/codec/address/merge |

拆分边界按**正交关注点**（传输 / 编解码 / 地址模型 / 批量合并），不按"函数多寡"。拆分是机械
重构，必须保证行为零变化（全量测试保回归）。

### 3.3 `mod.rs` 职责

子目录的 `mod.rs` 负责：
1. 插件 struct 定义 + `impl Source`/`Sink`
2. `inventory::submit!` 静态注册（**每个插件恰好一次**）
3. `pub use` 重导出插件 struct（供 `find_source` 工厂返回）
4. 子模块声明（`mod transport;` 等，私有，不对外）

### 3.4 feature gate（编译面裁剪）

协议插件**必须**挂 feature 门（决策 [2026-08-26 默认编译面](../decisions/2026-08-26-default-compile-surface.md)；
机制见 [2026-08-07 feature-gating](../decisions/2026-08-07-feature-gating-protocols.md)）。新增协议插件四步：

1. `Cargo.toml` `[dependencies]`：协议库 `default-features = false, optional = true`（惯例见 tokio-modbus/rust-ethernet-ip 条目）
2. `Cargo.toml` `[features]`：`<name> = ["dep:<crate>"]`；**是否进 `default` 需裁决**——当前
   `default = ["lua", "lua-http", "opcua", "modbus"]`，s7/eip 显式 `--features` 启用
3. `src/plugin/builtin/mod.rs`：`#[cfg(feature = "<name>")] pub mod xxx_client;`
4. 测试：新增端到端测试文件（对标 `tests/s7_e2e.rs` / `tests/eip_client_e2e.rs`），非默认 feature
   的测试文件头加 `#![cfg(feature = "xxx")]`（文件级门）；CI 侧 `just check-s7-eip` 同理扩

**行为语义**：关掉 feature ≠ 编译失败——`find_source` 优雅降级，建实例时返回
`source plugin not found: <app_code>`（`instance.rs`），实例无法拉起但主程序照常运行。
**排障提示**：实例报 plugin not found / `/api/v1/plugins` 列表里没有某插件 → 先确认构建带了
对应 feature（`cargo build --features s7,eip`），再查注册名拼写。

dataSim / selfMonitor / sidecarClient 无条件编译（不挂门——自监控与外部接入是系统能力，非可选协议）。

---

## 4. 内置 Source 开发（南向采集）

Source 是最常见插件类型（设备协议采集）。本节给出强制契约 + 推荐范式。

### 4.1 Source trait 契约

```rust
// src/plugin/traits.rs
#[async_trait]
pub trait Source: Send + Sync {
    /// 插件名（注册表 key，必须与 inventory submit 的 name、app_code 三者一致）。
    fn name(&self) -> &'static str;

    /// 运行采集循环：产出经 tx 推入 hub，命令从 cmd_rx 消费，token 取消时退出。
    ///
    /// `tx` 是 `SampleSender`（D-OWN-1：send/try_send 遮蔽转发前把 devid 归属
    /// 登记到 lux dev2inst）；`Deref` 回 `mpsc::Sender` 兼容其余用法。
    async fn run(
        &self,
        ctx: HostContext,
        inst: InstConfig,
        tx: SampleSender,
        cmd_rx: mpsc::Receiver<CtlMessage>,
        token: CancellationToken,
    );
}
```

**`SampleSender` 契约**（`src/plugin/sample_sender.rs`，D-OWN-1 起替换裸 `mpsc::Sender<Sample>`）：

- **用法与裸 Sender 几乎一致**：`tx.send(sample).await` / `tx.try_send(sample)` 直接用，
  其余方法（`clone`/`is_closed`/`capacity`…）经 `Deref` 透传——**自身 `send`/`try_send` 方法
  遮蔽 Deref 的**（Rust 方法查找优先自身），转发前把 `sample.devid` 归属登记到 lux `dev2inst`
  （设备 → 宿主实例路由表；进程内 `seen` 去重，同 devid 仅首次 HSET）
- **先登记后转发**：样本即使因背压被丢弃，devid 已产出即归属成立（丢样不否定归属）；
  登记失败仅 warn 不阻断数据面（最终一致，启动预登记兜底）
- `Clone` 全共享（inner/lux/seen 同一登记态）——同实例多任务克隆后共享去重表
- **插件不要绕过它**：不要缓存裸 Sender、不要自己写 lux dev2inst——直接用 `tx` 即可
- `InstanceManager::start_source` 是唯一构造点；测试/北向黑洞通道用 `SampleSender::untracked`（纯转发不登记）

**硬性约束**：
- `name()` 返回值、`inventory::submit!` 的 `name` 字段、配置里的 `app_code` **三者必须完全一致**
  （否则 `find_source` 找不到）
- `run` 在独立 tokio task 中执行（`InstanceManager::start_source` spawn），**task 退出即实例停止**
- `run` 必须在 `token.cancelled()` 时及时退出（`InstanceManager::stop` 有 3s 超时，超时强杀）

### 4.2 HostContext 注入

`ctx: HostContext`（`src/host.rs`）注入宿主能力，所有插件可用：

| 字段 | 类型 | 用途 |
|---|---|---|
| `ctx.lux` | `lux::EmbeddedClient` | 进程内 lux（零拷贝）。一般 Source 不直接用——推 hub 即可，pipeline 写 rtdata |
| `ctx.hwid` | `Hwid` | 边缘端硬件 ID，组装 `Sample.hwid` |
| `ctx.shutdown` | `CancellationToken` | 全局关闭（task 应用 `token` = `shutdown.child_token()`，但框架已传 `token` 参数，直接用） |
| `ctx.ctl_registry` | `CtlRegistry` | 命令路由表（框架已自动 register/unregister，插件一般不直接用） |
| `ctx.bridge_registry` | `BridgeRegistry` | 外部插件连接分发（仅 sidecarClient 用） |
| `ctx.metrics` | `Arc<ProcessMetrics>` | 进程级指标（M4，`/metrics` 出口） |
| `ctx.trace_registry` | `TraceRegistry` | 报文 trace 开关注册表（P1：裸报文协议包 `TraceIo` 用，见 §4.12） |
| `ctx.event_tx` | `EventBus` | SSE 事件总线（luaTransform 订阅它做数据加工；普通 Source 不用） |
| `ctx.history` | `Option<HistoryHandle>` | 历史归档旁路句柄（Phase 1b；恒由 pipeline 出口 fan-out，插件不碰） |

（字段清单以 `src/host.rs` `HostContext` 为权威，共 9 项。）

**普通采集插件只需 `ctx.hwid`**（组装 Sample）+ 按需 `ctx.trace_registry`/`ctx.metrics`。lux/registry 由框架管理。

### 4.3 配置解析（`app_config`）

`inst.app_config: serde_json::Value`（任意 JSON）由插件自行解析。约定：

```rust
fn parse_cfg(app: &serde_json::Value) -> Result<S7Cfg, String> {
    let endpoint = app
        .get("endpoint")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "app_config.endpoint missing".to_string())?;
    let period = app.get("period").and_then(|v| v.as_u64()).unwrap_or(2).max(1);
    // ...
    Ok(S7Cfg { /* ... */ })
}
```

- **缺失字段容错**：必填字段缺失报错（`Err(String)`），可选字段用 `unwrap_or(default)`
- **边界 clamp**：如 `period` 最小 1（`.max(1)`），避免 0 导致 ticker 空转
- **返回 `Result<_, String>`**：插件内部用 String 错误（对标 opcua/modbus/s7），不进 `RsIotError`
  （见 [§4.7](#47-错误处理)）
- 解析失败：`tracing::warn!` + `return`（task 退出，实例标记 Failed）

### 4.4 设备/变量模型

> **M-2 起变更（2026-08-25）**：`InstConfig.devices` 已是 **`Vec<DeviceBinding>`**，
> 不再是 `DeviceSpec`。旧 `DeviceSpec`/`ChildSpec`（`label/value/children`）**已退役**——
> 持久层不再写入，仅 `model::migrate` 迁移命令与装配期展开消费。

`inst.devices: Vec<DeviceBinding>`（`src/types.rs:236`）：

```rust
pub struct DeviceBinding {
    pub devid: String,                    // 引用的 devreg 设备 id
    pub bind: BTreeMap<String, String>,   // 协议绑定参数（如 {"slave_id":"2"}；值均为字符串）
}
```

**插件拿点表的正确方式**（装配期展开，勿自行解析元数据）：

```rust
// 在 run 入口一次性展开（对标 modbus_client/mod.rs:305、opcua_client、s7_client）
let specs: Vec<DeviceSpec> = crate::model::expand::expand_to_specs(&ctx.lux, &inst.devices).await?;
// specs[i].value = devid（Sample.devid）
// specs[i].children[].value = 协议地址串
// specs[i].children[].label = Sample.values 的 key（空则回退 value）
// specs[i].children[].r#type = 值类型（float/int/bool/i16/f32…，插件自定义语义）
```

**DeviceSpec / ChildSpec 仍存在，但只是"装配期只读视图"**（`model::expand` 合成）：

```rust
struct DeviceSpec  { label: String, value: String, children: Vec<ChildSpec> }
struct ChildSpec   { label: String, value: String, r#type: String }
```

**约定**（所有内置 Source 一致）：
- `DeviceSpec.value` = 设备 ID（`Sample.devid`）
- `ChildSpec.value` = 协议地址串（来自设备模型模板 + 参数/覆盖解析）
- `ChildSpec.label` = `Sample.values` 的 key（空则回退用 `ChildSpec.value`）
- `ChildSpec.r#type` = 值类型，由插件定义语义（opcua 的 opc_type / modbus 的 bit/i16/f32 / s7 的 kind）
- 绑定参数（`bind`）由插件自行解释（如 modbus 的 `slave_id`）；展开时 `params` 占位符替换与
  `overrides` 三态覆盖已在 `expand_to_specs` 内完成

**预解析**：`run` 入口一次性展开并解析成插件内部结构，采集循环内不再重复
（对标 `modbus_client/mod.rs:305`）。

**启动前置**：`startinst` 前绑定必须可展开——devices 引用的 devreg 设备/模型不存在或参数缺失，
展开失败直接 **404 拒启**（`routes_sys.rs`，语义见 rest-api.md §4.2）。新插件联调"实例起不来"
先查这条链：devmodel → devreg → inst.devices 绑定。

### 4.5 采集循环范式（`select!` 模板）

所有内置 Source 遵循同一骨架（参考 `opcua_client/mod.rs` run、`s7_client/mod.rs` run）：

```rust
async fn run(&self, ctx, inst, tx, mut cmd_rx, token) {
    let inst_id = inst.inst_id.clone();
    let cfg = match parse_cfg(&inst.app_config) {
        Ok(c) => c,
        Err(e) => { tracing::warn!("[s7Client] inst {inst_id} bad config: {e}"); return; }
    };
    // 预解析 devices（见 §4.4）
    let devices = /* ... */;

    let mut ticker = tokio::time::interval(Duration::from_secs(cfg.period));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut conn: Option<Connection> = None;   // 懒连接 + 断线重连

    loop {
        tokio::select! {
            _ = token.cancelled() => {
                tracing::info!("[s7Client] inst {inst_id} stopping");
                break;
            }
            msg = cmd_rx.recv() => {
                if let Some(m) = msg {
                    if m.cmd == "write" { handle_write(&conn, &m.data).await; }
                    else { tracing::info!("[s7Client] inst {inst_id} unknown cmd: {}", m.cmd); }
                }
            }
            _ = ticker.tick() => {
                // 1. 懒连接 / 重连
                if conn.is_none() { match connect(&cfg).await { Ok(c) => conn = Some(c), Err(e) => { tracing::warn!(...); continue; } } }
                // 2. 逐设备采集 → 组装 Sample → tx.send
                // 3. 读失败且为连接错误 → conn = None（下个 tick 重连）
            }
        }
    }
    // 清理（断开连接、abort handle）
    if let Some(mut c) = conn { let _ = c.disconnect().await; }
    tracing::info!("[s7Client] inst {inst_id} exited");
}
```

**关键点**：
- **三个 select 分支**：`token.cancelled()` / `cmd_rx.recv()` / `ticker.tick()`——缺一不可
- **懒连接**：首个 tick 才连接，连接失败 `continue`（下个 tick 重试），不阻塞 task
- **断线重连**：读失败 + 连接错误 → `conn = None`，下个 tick 重连（对标 opcua `session=None`）
- **`MissedTickBehavior::Skip`**：积压 tick 跳过，不补偿（避免catch-up 风暴）
- **ticker 错峰**：多实例整点同步触发会形成脉冲——用 `src/ticker.rs` 的 FNV-1a 确定性相位
  （`fnv1a(inst_id) % period_ms`）把首 tick 散布开（对标 data_sim/modbus；`pipeline::route`
  的 devid 分片复用同算法）
- **`tx.send(sample).await.is_err()` → break**：hub 关闭（pipeline 挂），task 退出
- **埋点（M4）**：task 内持 `StatTracker::new(inst_id, ctx.lux.clone())`，select! 加第 4 分支 `flush_ticker.tick() → tracker.flush()`；采集成功 `tracker.tick_ok(elapsed_ms)`、失败 `tracker.tick_fail(reason)`。连接底层 IO 包 `TraceIo::new(stream, inst_id, ctx.trace_registry.clone())` 接入报文 trace（详见 [§4.12](#412-运行指标与诊断m4--p1-已落地)）

### 4.6 写命令（`cmd_rx` + `handle_write`）

控制下行经 `CtlRegistry` 路由到实例 `cmd_rx`（channel 64，`try_send` 非阻塞）。插件在 select
的 `cmd_rx.recv()` 分支处理：

```rust
async fn handle_write(conn: &Option<Connection>, data: &str) {
    let c = match conn { Some(c) => c, None => { tracing::warn!("[s7Client] write ignored: not connected"); return; } };
    let req: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v, Err(e) => { tracing::warn!("[s7Client] write bad json: {e}"); return; }
    };
    // 解析 address/value/type → 调库 typed write
    // 失败 tracing::warn!（不 panic，不阻塞采集）
}
```

**约定**：
- `CtlMessage.cmd` 通常是 `"write"`；其他 cmd `tracing::info!` 忽略
- `CtlMessage.data` 是 JSON 字符串，**插件自定义 schema**（opcua `{node_id,value,opc_type}` /
  modbus `{address,value}` / s7 `{address,value,kind}`）
- 未连接时 `warn` 丢弃（对标 opcuaClient），不排队（写命令"最新值生效"语义）
- 写失败 `warn`，不 panic

**写入口**（插件不感知，统一经 CtlRegistry）：
- MQTT `<hwid>/command` 订阅（T8 起随默认 luaNorth 实例的 down 回调，`lua_north/compat.rs`）
- REST `POST /api/v1/command/send`（`routes_command.rs` → `InstanceManager::send_ctl`）

### 4.7 错误处理

**两层错误**：
- **库公开 API**（`src/error.rs` `RsIotError` + `TResult`）：thiserror + `#[non_exhaustive]`，
  覆盖 lux/io/bcrypt/json/toml/mqtt/jwt + Config/Identity。**新错误来源加 `#[from]` 变体**
- **插件内部**：`Result<_, String>`（对标所有内置插件）。协议库错误（如 `S7Error`/`modbus Error`）
  经 `to_string()` 转 String；**不进 `RsIotError`**（插件错误不在库公开 API 暴露）

**运行时错误降级**：连接/读/写失败一律 `tracing::warn!` 继续，**绝不 panic**（对标
pipeline/mqtt/opcua/modbus/s7 既有约定）。panic 会拖垮整个 task 甚至进程。

**配置错误**：`parse_cfg` 失败 → warn + task 退出（实例 Failed），不 panic。

### 4.8 日志

- **`tracing`**（不是 `log`/`println!`）
- **模块前缀**：`[pluginName]`（如 `[s7Client]`/`[modbusClient]`），便于 grep 过滤
- **关键入口加 `#[tracing::instrument]`**（对标 `InstanceManager::start_source`）
- **级别**：
  - `info!`：连接建立/断开、task 启动/退出、配置推送
  - `warn!`：连接失败、读/写失败、坏配置、坏帧、丢命令（降级继续类）
  - `error!`：罕见（一般用 warn，error 留给致命）
- **不输出敏感信息**：bcrypt 哈希、JWT 明文、lux passwords 表、外部 token 明文。`hwid` 可输出

### 4.9 异步生命周期与取消

- **`CancellationToken`**：task 在 `select!` 监听 `token.cancelled()`；`InstanceManager::stop`
  cancel 后等 join 3s 超时
- **独占持有连接**：协议 client（如 `S7Client`/modbus `Context`）在 task 内 owned（`Option<Conn>`），
  不跨 task 共享；写命令经 `cmd_rx` 投递到该 task 内执行（避免 Arc/Mutex）
- **`connect(&mut self)` 适配**：若库的 connect 需可变借用，task 内 `Option<Conn>` 模式天然满足
  （构造时 connect 后 `Some`，重连时 take + 重新 connect）
- **`try_send` 非阻塞**：CtlRegistry 投递写命令用 `try_send`（channel 满丢弃），**勿改阻塞**
  （对标 Go 版 default 语义；阻塞会让写入口拖垮采集 task）

### 4.10 静态注册（`inventory`）

文件末尾（`mod.rs`）：

```rust
inventory::submit! {
    crate::plugin::registry::SourceEntry {
        name: "s7Client",              // 必须与 name() 返回值、app_code 一致
        create: || Box::new(S7Client), // 工厂函数
    }
}
```

- **每个插件恰好一次** `submit!`（重复注册编译期报错或运行时覆盖）
- `registry::find_source(name)` 编译期 collect，无注册顺序依赖
- 无需手改 `registry.rs`（`collect!(SourceEntry)` 自动发现）

### 4.11 完整骨架（最小可运行 Source）

最小参考：`src/plugin/builtin/data_sim/mod.rs`（DataSim，模拟采集，~170 行）。协议采集参考：
`opcua_client/mod.rs`（中等，批量读 + 类型映射）、`s7_client/mod.rs`（中等，批量读 + 大端解码）、
`eip_client/mod.rs`（批量读 + tag 位寻址写）。

### 4.12 运行指标与诊断（M4 + P1 已落地）

> 本节是**已落地的统一规范**：M4 把"埋点"从 lua 专属提升为框架统一 + 全覆盖（`StatTracker` + `InstanceStat`），P1 补报文 trace（`io_trace`）+ `/metrics` Prometheus 端点。新增插件**直接接入**下列机制，无需"预留"。完整约定见 [`instrumentation-conventions.md`](../conventions/instrumentation-conventions.md)。

**指标埋点（M4，`StatTracker`）**：每个 Source task 持有一个 `StatTracker`（`src/plugin/stat_tracker.rs`），周期（`FLUSH_INTERVAL_SECS`）+ 阈值触发 flush 到 lux `stat:<inst_id>`（`db/stat.rs` `InstanceStat`）。核心 6 字段所有插件一致：
- `status`：`running` / `degraded`（连续失败 ≥ 阈值）/ `stopped`
- `tick_count` / `fail_count`：活动成功/失败累计计数
- `last_activity_ts` / `last_activity_duration_ms`：最近活动墙上时间戳与耗时
- `last_err`：最近失败原因（成功时清空）

插件扩展字段经 `extra`（serde flatten）透传——如 opcua `session_reconnect_count`、modbus `exception_count`、s7 `reconnect_count`、lua `vm_restart_count`/`script_version`、sidecar `connected`/`heartbeat`。`select!` 主循环加 flush ticker + `tick_ok(elapsed_ms)`/`tick_fail(reason)` 两个上报入口（参考任一内置插件实现）。

**报文 trace（P1 #2，`io_trace`）**：收发报文 hex dump，per-instance 运行时开关（`POST /api/v1/inst/trace` 按需激活，惰性超时关闭）。**阶段 1 已覆盖 lua（io_engine direct dump：request TX/RX + recv_idle 被动 RX 三点）+ modbus 4 传输（`TraceIo<T>` 包底层 IO，`src/io_trace.rs`）**。

**豁免清单（三条，均为结构性不可接，非欠账）**：
- `opcua` / `s7`：库自管会话/协议栈，非裸报文——线缆字节对诊断无意义
- `eip`：`rust-ethernet-ip` 在 `EipClient::connect` 内部自建并独占 TCP 流，外层拿不到裸
  `TcpStream`，`TraceIo<T>` 无处包装（诊断走库日志 + `/logs/recent`）
- `sidecar`：bridge 是 IPC 帧，已有 DataUp/Command 帧级日志

新增裸报文协议插件（自己持有 socket 的）：底层 IO 包一层 `TraceIo<T>` 即可接入（trace 关闭时 dump 快速路径几乎零成本）。

**链路类型感知（lua）**：lua 脚本经 `ctx:transport()`（返回 `"tcp"`/`"serial"`/`"udp"`）感知底层链路，据此选帧格式（与 `TraceIo` 报文 trace 同属 lua IO 层能力，详见 [`lua-script-guide.md`](lua-script-guide.md)「通讯链路抽象」）。

**自愈（M3）**：§4.5 的懒连接 + 断线重连（`Option<Conn>` + `is_connection_error()`）是 v1 既有范式；**`Failed` 状态真实写入 + 实例 supervisor 已落地**（M3）：`InstanceManager::spawn_supervisor` 唯一 await source task join，task 异常退出（panic/配置错误早退）且 token 未 cancel → 标 `InstState::Failed` + `error!` 落盘；运维 `startinst` 重拉。统一指数退避/熔断仍未做（见场景评估 §2 后续项）。

**写新插件时的实务建议**：① 按 §4.5 范式实现基础重连；② `select!` 加 flush ticker + `StatTracker`（`tick_ok`/`tick_fail`）；③ 若是裸报文协议，底层 IO 包 `TraceIo<T>` 接入报文 trace。三步对照任一内置插件（如 `data_sim`/`modbus_client`）即可。

---

## 5. 内置 Sink 开发（北向发布）

Sink 消费数据发往外部（MQTT broker / 数据库 / 云）。**现役唯一 Sink = `luaNorth`**
（MQTT 北向，Lua 脚本可编程发布面）；独立 `mqttPub` 插件已删——T8（2026-08）把 `[mqtt]` 配置
无损迁移为**默认 luaNorth 实例** `mqtt-pub-default`（内置 mqttPub 兼容脚本）。

### 5.1 Sink trait 契约

```rust
// src/plugin/traits.rs
#[async_trait]
pub trait Sink: Send + Sync {
    fn name(&self) -> &'static str;
    async fn run(&self, ctx: HostContext, inst: InstConfig,
                 rx: mpsc::Receiver<Sample>,          // 注意：rx 而非 tx
                 cmd_rx: mpsc::Receiver<CtlMessage>,
                 token: CancellationToken);
}
```

### 5.2 两种范式

- **pull 模式**（luaNorth 现状）：Sink 不从 `rx` 消费 Sample，而是 mqtt task 独占网络 IO、
  BatchTick 经 lux `pull` 收拢批量（`KEYS rtdata:*` → 批量取值 → PUBLISH）。多 Source 产出
  汇 lux，pull 天然聚合 + 单一发布点；`rx` 通道当前是黑洞（`SampleSender::untracked`）。
- **push 模式**：Sink 从 `rx` 消费 Sample（`InstanceManager::start_sink` 接入）。trait 保留此
  能力，pipeline 尚不 fan-out 到 sink——留给 v2 逐 tag 低延迟发布场景。

### 5.3 北向现状与扩展路径

- **`[mqtt]` 配置默认北向**：`lib.rs` 启动时 `compat::start_default_north`（R5 单通道锁：本进程
  已运行不二次启动；lux 已有同名配置沿用不覆盖；auto_start 交 lux autoStart 路径）——经
  InstanceManager 全套生命周期管理（supervisor/Failed 状态/stat 埋点一体）。
- **luaNorth 脚本契约**：`auth/up/down/on_connack/on_disconnect` 回调（`down` 即
  `<hwid>/command` 命令订阅转发），详见 `docs/guides/lua-script-guide.md` 北向章节。
- **新增北向协议**：优先评估 **luaNorth + Lua 脚本**（零编译，MQTT 之外协议再议）；
  确需 Rust 进程内实现时：实现 `Sink` trait + `inventory::submit! { SinkEntry {...} }`，
  `startinst` 对 `app_type=north` 走 `start_sink`（R4 起支持，此前被 400 拒）。

---

## 6. Processor 开发（L2 数据转换）——**冻结**

Processor 在 pipeline 内转换 Sample（hub → Processor → lux rtdata）。

> **Q7 裁决（2026-08）：冻结不接线**。`lib.rs` 构建 pipeline 时**恒用 `Passthrough`**
> （rhai 不经配置切换）；feature `transform` 已出 default，仅为保留解冻入口。
> **数据加工一律走 `luaTransform`**（Source 订阅型：消费 EventBus 上的点值变更做计算/衍生点，
> 产出经 SampleSender 回 hub，不占 Processor 位）——新增转换需求先看 luaTransform 范式。

### 6.1 Processor trait 契约

```rust
#[async_trait]
pub trait Processor: Send + Sync {
    /// 返回 None 表示丢弃该样本。
    async fn process(&self, sample: Sample) -> Option<Sample>;
}
```

### 6.2 现有实现

- `transform::passthrough::Passthrough`：恒定使用，原样透传
- `transform::rhai::RhaiProcessor`（feature `transform`，**冻结 deprecated**）：存在但不被接线

### 6.3 新增 Processor

原则上不新增（见本节开头冻结裁决）。确需解冻：实现 trait + `src/transform/` 新文件 +
`lib.rs` 接线选择逻辑——须先在 decisions 登记推翻 Q7 的新裁决。

---

## 7. 外部插件开发（sidecar bridge）

外部进程（任意语言：C/Go/Python/Java）作为南向 Source 接入，无需改 rs-iot 代码。**外部 = 纯
运行时**（配置/数据/状态全在 rs-iot，外部仅本地缓存数据待补传）。

### 7.1 架构（方案 C：lux 不对外暴露）

```
外部进程 ──TCP──► bridge acceptor(:8781, token 鉴权)
                        │ 按 inst_id 分发到实例 task
                        ▼
                  sidecarClient task（每实例一个）
                        │ DataUp → Sample → hub → pipeline → rtdata（与内置同构）
                        │ 写命令 ← CtlRegistry ← MQTT/REST → Command 帧 → 外部
                        ▼
                  lux（loopback，外部永不触碰）
```

- 认证/授权集中在 rs-iot（外部只连 bridge，校验 token）
- lux 永不对外暴露（安全边界）
- 双向重连容错：外部指数退避 + 缓存补传；bridge 断连感知 + 失联期间写命令丢弃

### 7.2 接入流程（rs-iot 侧，无需写代码）

1. REST 建实例：`app_code="sidecarClient"` + `host_kind="external"` + `app_config{heartbeat_timeout_secs?}`
   + `devices:[{devid, bind}]`（**扁平绑定**——设备须先在 devreg 注册，完整流程与 curl 示例见
   [`external-plugin-dev-guide.md`](external-plugin-dev-guide.md) §3.2；`routes_inst::set` 自动签发
   token，**明文返回一次**）
2. `startinst`（注册 slot 等外部接入）
3. 外部进程连 bridge（`config.bridge`，默认 `127.0.0.1:8781`）报到

### 7.3 二进制协议

帧格式 `[4B BE length][1B type][JSON body]`，7 种帧类型（Hello/HelloAck/ConfigPush/DataUp/
Command/CommandAck/Heartbeat）。**完整规范见 [`external-plugin-protocol.md`](../reference/external-plugin-protocol.md)**。

实现：`src/plugin/builtin/sidecar_client/mod.rs`（acceptor + task + codec）。集成测试：
`tests/sidecar_e2e.rs`（进程内模拟客户端闭环）。

### 7.4 安全

- bridge `bind_host` 默认 `127.0.0.1`（同机零暴露）；跨机改 `0.0.0.0` **必须**配 token（强制）
- token：`hmac_sha256` 校验，明文仅建实例时返回一次，存 hash 于 lux `ext_tokens`
- PoC 不上 TLS（局域网信任）；跨机敏感场景 TLS 后置
- 删实例自动清除 token

### 7.5 各语言样例

- Python：`examples/ext-plugin-python/`（见其 README）
- Go（OPC DA）：`examples/ext-plugin-opcda-go/`（huskar-t/opcda，Windows-only，COM）

### 7.6 运行指标（sidecarClient 已接入 M4 统一埋点）

sidecarClient 实例的 health 经 `GET /api/v1/inst/stats?inst_id=<ext>` 查（与内置插件同出口）。`StatTracker` 上报：每收 `DataUp` 帧 → `tick_ok`；扩展字段经 `extra` 透传 `connected`（外部进程在线）/`heartbeat`（最近心跳 ts）。外部进程自身诊断不依赖 rs-iot 埋点（独立进程自带），rs-iot 侧只把连接状态/心跳纳入统一 stat。

---

## 8. 通用数据契约

### 8.1 Sample / SampleValue（`src/types.rs`）

```rust
struct Sample {
    hwid: Hwid,
    devid: String,
    values: BTreeMap<String, SampleValue>,   // key = ChildSpec.label
    timestamp: i64,                           // Unix 秒
}
#[serde(untagged)]
enum SampleValue { Float(f64), Int(i64), Bool(bool), Str(String) }
```

- `values` 是一设备一时刻的多变量 map（不是逐 tag 拆 Sample）
- `SampleValue` untagged JSON（数值/布尔/字符串直接序列化）
- 协议值映射：`f32`/`f64` → `Float`，`i16..i64`/`u16..u64` → `Int`，`bool` → `Bool`，字符串 → `Str`

### 8.2 CtlMessage（`src/types.rs`）

```rust
struct CtlMessage {
    instance: InstId,   // 路由 key
    devid: String,
    cmd: String,        // 通常 "write"
    data: String,       // JSON，插件自定义 schema
}
```

### 8.3 lux rtdata 布局

- Hash `rtdata:<devid>`，field `<devid>.<tag>`，value `[ts, localTime, value]` JSON
- 由 pipeline 写（`db::rtdata::write_batch`），**插件不直接写**（推 hub 即可）
- 读出口：luaNorth 北向实例（pull）/ `GET /api/v1/rtdatas`

### 8.4 CtlRegistry 路由（`src/host.rs`）

- `CtlRegistry`：`instId → mpsc::Sender<CtlMessage>`（DashMap）
- `send(inst, msg)`：`try_send` 非阻塞，channel 满丢弃，实例未注册返回 `false`
- 框架在 `start_source`/`stop` 自动 register/unregister，**插件不直接操作**

---

## 9. 测试规范

### 9.1 单元测试（`#[cfg(test)] mod tests`，文件内嵌）

测**纯函数**（解析 / 编解码 / 合并 / 类型映射），隔离测，无真机依赖：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cfg_defaults_and_validation() { /* 缺省值 + 必填 + clamp */ }
    #[test]
    fn decode_item_big_endian_roundtrip() { /* 大端字节 → 值 round-trip */ }
    #[test]
    fn decode_item_rejects_bad_return_code() { /* 协议部分失败处理 */ }
}
```

- **一断言一测试**（尽量）、**描述性命名**（`xxx_should_yyy_when_zzz`）
- `serde_json::json!({...})` 构造配置
- 协议采集的端到端（连真 PLC）**不进单测**，靠 [§9.2 集成测试](#92-集成测试teststests) + 人工

### 9.2 集成测试（`tests/`）

- `tests/common::start_lux`：**端口 0（OS 分配）+ 独立临时 data_dir**，勿固定端口、勿写仓库 `./data/`
- 现有：`dataflow.rs`（datasim→hub→pipeline→rtdata）/ `api_flow.rs`（axum + JWT）/ `config_load.rs`
  / `sidecar_e2e.rs`（bridge 闭环，进程内模拟客户端）
- 协议端到端（opcua/modbus/s7 连真 PLC/仿真器）**CI 不跑**，文档记录步骤，人工验证

### 9.3 测试要验证意图

测试必须表达**行为为什么重要**，不只验证"它做了什么"。业务逻辑变时测试应失败。参考全局
AGENTS.md 规则 7。

---

## 10. 验证与质量门槛

改 `src/` 任意代码，**四项全绿才算完成**（与 CI `.github/workflows/ci.yml` 一致）：

```bash
cargo fmt --all --check                    # 格式
cargo clippy --all-targets -- -D warnings  # lint（零 warning）
cargo test --all-targets --all-features    # 全量测试
RUSTFLAGS="-D warnings" cargo doc --no-deps # 文档（missing_docs 基线，零 warning）
```

- **`missing_docs`**：库公开项（`pub`）必须有 `///`/`//!`；模块、struct、字段、函数都要
- **`#[expect(...)]` 而非 `#[allow(...)]`**：lint 不再触发时编译器提示移除，杜绝永久豁免
- **`#[non_exhaustive]`**：`RsIotError` 等公开枚举加它，新增变体不破坏下游 match
- **注释中文**（与现有代码 + 文档一致）

---

## 11. 迁移与兼容

### 11.1 子目录迁移（已完成 2026-08）

Rust 模块系统对 `foo.rs` vs `foo/mod.rs` 透明，迁移**零代码改动**。**全量迁移已完成**：所有 6 个
内置插件已从单文件 `xxx.rs` 迁到 `xxx/mod.rs`（`data_sim/` 目录名同步规范化），76 测试零回归。

迁移步骤（历史记录，新插件直接用子目录）：
1. `xxx.rs` → `xxx/mod.rs`（tracked 用 `git mv` 保历史，untracked 普通 `mv`）
2. `builtin/mod.rs` 的 `pub mod xxx;` **不变**
3. `registry.rs` / import 路径 / lux 持久化数据 **全不变**
4. 跑四项质量门确认零回归（76 测试是硬凭证）

`modbus_client` 已进一步拆子模块（config/address/codec/merge/transport + mod.rs，见 [§3.1](#31-子目录结构目标态)）。
拆分是机械重构（`pub(super)` 可见性 + sibling `use`），逻辑体与测试断言字节级不变。

### 11.2 命名偏差收敛（已完成 2026-08）

- `datasim.rs` → `data_sim/mod.rs`（目录名规范化；注册名 `dataSim` 锁定不变）
- `MqttPublisher` → `MqttPub`（struct 重命名 + `lib.rs` 调用 + 4 处 doc 注释连带清理；不影响持久化）
- **T8（2026-08）**：`mqttPub` 插件整体删除——`[mqtt]` 配置无损迁移为默认 luaNorth 实例
  `mqtt-pub-default`（`lua_north/compat.rs`）；本节为历史记录，§2.3 现状清单不再含 mqttPub

现状全部插件已符合三层命名规范（见 [§2.3](#23-现状清单与偏差)；存量偏差 `EipClientPlugin` 记录在案）。
**注册名永不改**（持久化契约，见 [§2.2](#22-注册名锁定持久化契约)）。

### 11.3 向后兼容红线

- **`app_code`（注册名）锁定**——改它破坏所有已部署实例
- **`CtlMessage.data` JSON schema 加字段向前兼容**（新字段可选 + 缺省）
- **lux Hash 布局变更需端到端验证**（启动恢复 + 读写）
- **`db::shutdown_lux` 必须先 SAVE**（lux shutdown 默认不落盘）

---

## 12. 附录：新增插件检查清单

新增一个内置 Source 时，逐项确认：

- [ ] 注册名 lowerCamelCase（如 `s7Client`），目录名 snake_case（`s7_client/`），struct UpperCamelCase（`S7Client`）
- [ ] `name()` / `inventory::submit!` name / `app_code` 三者一致
- [ ] 协议插件挂 feature 门四步全做（§3.4：optional dep / `[features]` / `#[cfg]` / 文件级门测试）
- [ ] `Source::run` 三分支 select（token / cmd_rx / ticker）；`tx` 用 `SampleSender` 语义（§4.1，勿缓存裸 Sender）
- [ ] 配置 `parse_cfg` 容错（必填报错 + 可选缺省 + 边界 clamp；`period` 单位=秒且 `.max(1)`）
- [ ] 预解析 `devices`（`expand_to_specs`），采集循环不重复 parse；绑定不可展开 → startinst 404（§4.4）
- [ ] ticker 错峰（`src/ticker.rs` FNV-1a 相位，§4.5）
- [ ] 连接懒连 + 断线重连（`Option<Conn>` + `is_connection_error()`）
- [ ] 写命令 `handle_write`（cmd_rx 分支 + JSON data + 未连接 warn 丢弃）
- [ ] 错误 `tracing::warn!` 降级，绝不 panic
- [ ] 日志 `[pluginName]` 前缀
- [ ] 接入 `StatTracker`（`tick_ok`/`tick_fail` + flush ticker，对标 `instrumentation-conventions.md`，见 [§4.12](#412-运行指标与诊断m4--p1-已落地)）
- [ ] 裸报文协议插件：底层 IO 包 `TraceIo<T>` 接入报文 trace（见 [§4.12](#412-运行指标与诊断m4--p1-已落地)）
- [ ] `pub` 项有 `///` doc 注释（中文）
- [ ] 单测覆盖纯函数（解析/编解码/类型映射）
- [ ] 四项质量门全绿（fmt / clippy -D warnings / test / doc -D warnings）
- [ ] **运行时自检**：`GET /api/v1/plugins` 出现新注册名；带 feature 门的插件在默认构建与 `--features` 构建下各验证一次（默认构建下应优雅报 plugin not found）
- [ ] `README.md` 插件清单 + 里程碑更新
- [ ] `AGENTS.md` 模块表 + 验证规则段更新

新增外部插件（sidecar）：无需写 rs-iot 代码，按 [`external-plugin-protocol.md`](../reference/external-plugin-protocol.md)
实现外部进程 + 经 REST 建实例（`host_kind=external`）。

---

*本指南随插件体系演进持续更新。新增插件类型（如 v2 WASM）或命名规则变更须同步本文件。*
