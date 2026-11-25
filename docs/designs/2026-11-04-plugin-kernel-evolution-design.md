# 插件内核演进设计（轻量 · 优雅 · 可演进）

> **状态**：draft（改进建议，未实施）
> **日期**：2026-11-04（起草）
> **实施**：未开始
> **代码**：目标新建 `src/plugin/{descriptor,runtime,catalog}.rs` + `crates/rsiot-plugin-sdk/` + `crates/rsiot-plugin-conformance/`（均未创建）
> **测试**：随实施建立（conformance 套件 + 现有 `tests/*_e2e.rs` 零回归）
> **遗留**：全部（P0-P2 八项 + 两个可选项；见 §十六）
>
> **裁决（2026-11-25，部分；与下文冲突处以本块为准）**：
> - **不做**（用户裁决）：G8 WASM——rs-iot 没有这个目标。G2 `PluginRuntime` trait 与 G4 能力闭集的唯一启动条件是 WASM，随之关闭（**由上一句推出，未单独裁决**）。
> - **G6 SDK 不做**（用户裁决）：§十一 的 `rsiot-plugin-sdk` 混了两样东西。「甲」插件契约包（把 `Source`/`Sink` trait 及其类型抽成独立 crate）维持[扩展策略 ADR](../decisions/2026-08-07-plugin-extension-strategy.md) 2026-10-31 勘误的结论「明确不做」；「乙」外部插件客户端库（连接/握手/重连/缓存补传/帧编解码）现在不做，触发条件是仓库外出现第一个外部插件作者。
> - **待裁决**：G1 目录、G3 schema、G5 协议扩展、G7 健康与页面、§6.5 托管进程。裁决前须知本文与代码现状的五处出入：
>   1. sidecar 基线是 v3（`73e005a`，2026-10-27，含版本集合协商）；握手按字符串集合精确匹配，§八 的 `ver:"2.1"` 会被拒。
>   2. 健康与 schema 应按实例/脚本归属，不能按插件名：Lua 的 `@param` 属于每个脚本，sidecar 的 api_ver 属于每条连接。
>   3. sidecar 实例的 `app_config` 由宿主原样下发、宿主不解释，宿主侧没有可生成 schema 的 `Cfg` 结构体。
>   4. modbus/opcua/s7/eip 四个协议插件没有 typed 配置结构体（全是手写 `parse_cfg`），§9.1 的 `ModbusCfg` 示例不存在。
>   5. [架构优化评估](2026-10-27-architecture-optimization-review.md)（2026-10-27）把 `find_source` 查找与实例生命周期列为「已确认的良好实践，勿在优化中破坏」，并定下「按触发点偿还，不宜提前抽象」；本文未引用它。
>
> 本文是 2026-11-04 对 rs-iot 插件体系的结构性改进建议，目标是在**不改变现有数据流与部署行为**的前提下，
> 把 static / Lua / sidecar 三种插件的注册、配置、能力、版本、生命周期与可观测性收进一个“插件内核”，
> 让后续 WASM 只是第四个 runtime adapter，而不是第四套体系。
>
> 前置决策：[插件扩展策略](../decisions/2026-08-07-plugin-extension-strategy.md)（已实施：拒绝 Rust 动态库，
> 走 SDK + sidecar + Lua + Cargo feature）。本文不推翻该决策，只在其上补“内核层”。
> 相关：[插件开发指南](../guides/plugin-development-guide.md)、[外部插件协议](../reference/external-plugin-protocol.md)、
> [埋点规范](../conventions/instrumentation-conventions.md)、[REST API](../reference/rest-api.md)。

## 一、判断：缺的不是第四种插件，而是插件内核

rs-iot 目前已有三条可用的插件路径，而且方向是对的：

- **L1 静态插件**：`inventory` 编译期注册，零运行时开销（`src/plugin/registry.rs:9`）。
- **Lua 动态脚本**：`luaClient` / `luaNorth` / `luaTransform`，有 manifest、能力卡、沙箱、热加载与失败回退
  （`src/plugin/lua_core/`）。
- **sidecar 外部插件**：任意语言经 TCP bridge 接入，v2 协议有版本协商、token、心跳、ConfigPush 与 DataUp 校验闸
  （`src/plugin/builtin/sidecar_client/`、`docs/reference/external-plugin-protocol.md`）。
- **L3 WASM**：v2 路线预留，尚未决策/实施（根 `README.md`「v2 路线」）。

问题不在于“少一种运行时”，而在于**四种契约各自为政**：

| 维度 | L1 静态 | Lua | sidecar | WASM（计划） |
|---|---|---|---|---|
| 发现/注册 | `SourceEntry { name, create }`（`registry.rs:9`） | 脚本目录 + `main.lua` 头 | TCP `Hello` | 未定 |
| 契约 | `Source`/`Sink` trait（`traits.rs:17`） | `@role`/`@api_ver` 头（`lua_core/manifest.rs`） | 7 种帧（协议 v2） | 未定 |
| 配置 | `InstConfig.app_config: Value`，各插件手写解析 | `@param` 仅文档/dry-run | `ConfigPush` 手拼 JSON | 未定 |
| 能力/权限 | 完全信任 | `CapabilityCard` C0-C7 / R0-R3 | 仅 token 鉴权 | 未定 |
| 版本 | 编译期 | `@api_ver` + `@stdlib` | `ver == "2"` 精确匹配 | 未定 |
| 热更新 | 重编译 | VM 级 reload + 旧 VM 回退 | 重连 + 重启实例 | 未定 |
| 元数据 | 只有名字 | manifest（部分字段未消费） | Hello 只有 inst_id/token | 无 |
| 可观测 | 实例级 `InstanceStat` | `stat:<inst>` | bridge 观察派生 | 无 |

这带来三个直接后果：

1. `InstanceManager::start_source` 硬编码 `registry::find_source(app_code)`（`src/plugin/instance.rs:107`），
   “插件是什么”被绑定为“inventory 里的一个工厂函数”；sidecar/Lua 只能靠 `app_code` 伪装成静态插件。
2. `/api/v1/plugins` 只返回 `sources` / `sinks` 两个名字数组（`src/api/routes_sys.rs:194`），
   管理面看不到 runtime、版本、能力、配置 schema、健康状态。
3. 每加一种 runtime，就要在 `InstanceManager`、配置解析、观测、前端各加一套分支；WASM 会变成第五套。

**结论**：下一步最高杠杆的动作不是“加 WASM”，而是**先做插件内核**。内核稳定后，WASM 只是 `PluginRuntime` 的第四个实现。

## 二、目标与非目标

### 2.1 目标

| ID | 目标 | 验收 |
|---|---|---|
| G1 | 一个 `PluginDescriptor` 描述所有插件 | 静态/Lua/sidecar 都能映射到同一结构；`/api/v1/plugins` 返回完整目录 |
| G2 | 一个 `PluginRuntime` trait 管理生命周期 | `InstanceManager` 不再直接依赖 `find_source`/`find_sink` |
| G3 | 配置 schema 化 | 每个插件配置在启动前校验；`GET /api/v1/plugins/{id}/schema` 可用 |
| G4 | 能力/权限统一 | sidecar 在 Hello 声明能力；未声明调用被拒并计数 |
| G5 | sidecar 协议 2.1 | 能力协商、ConfigAck、Credit/Pause、Drain/Goodbye；v2 兼容 |
| G6 | SDK + conformance | 第三方插件可独立开发并用统一套件验收 |
| G7 | 插件级健康面 | `/api/v1/plugins` 带版本/状态/重启/握手/拒绝计数；前端有 Plugins 页面 |
| G8 | WASM 作为第四个 adapter | 只在 G1-G7 稳定后启动；WIT 契约，不自造 ABI |

### 2.2 非目标

- 不做 Rust 原生动态库（`cdylib`/`dlopen`）——已在 2026-08-07 ADR 否决，本文不重开。
- 不替换静态核心协议（opcua/modbus/s7/eip）；它们继续走静态 + Cargo feature 裁剪。
- 不在每个 `Sample` 上做 trait 抽象/虚调用；`Source::run`/`Sink::run` 的热路径保持直接调用。
- 不破坏 sidecar v2：v2 在一个弃用窗口内继续接受，2.1 通过 feature 渐进升级。
- 不改变现有部署文件格式：`config.toml`、lux `instances`、`app_config` 均保持兼容。
- 不在第一阶段做 WASM；先完成 G1-G7。

## 三、设计原则

1. **一个内核，多种 runtime**：内核只认 `PluginDescriptor` 与 `PluginRuntime`，不认具体协议。
2. **窄接口，实例粒度**：runtime trait 只覆盖 load/start/stop 与健康检查，不进入逐样本路径。
3. **descriptor 是单一事实源**：注册、配置、能力、版本、UI、文档都从它派生。
4. **schema 先行**：配置先校验再启动；错误带字段路径，不把解析错误留到运行期。
5. **最小权限**：能力是闭集，默认不授予；sidecar/WASM 必须显式声明并由宿主校验。
6. **生命周期可逆**：任何插件都能 drain → stop → 回滚；失败不拖垮其他插件。
7. **失败策略显式**：采集失败、转换失败、发布失败、写命令失败各有明确语义，不靠“看代码猜”。
8. **热路径不抽象**：内核层在实例级别，数据面仍是 `sample_sender` → `hub` → `pipeline`。

## 四、总体架构

```
┌────────────────────────────── rs-iot 内核 ──────────────────────────────┐
│  PluginCatalog（目录/元数据/健康）                                       │
│    ├─ PluginDescriptor：id / role / runtime / api_ver / params / caps    │
│    └─ PluginHealth：state / version / restarts / handshake / denied      │
│                                                                          │
│  PluginRuntime trait（load / start / stop / health）                      │
│    ├─ BuiltinRuntime  ← inventory（现有静态插件）                         │
│    ├─ LuaRuntime      ← lua_core（现有 Lua 脚本）                         │
│    ├─ SidecarRuntime  ← bridge（现有外部插件 + 2.1 协议）                  │
│    └─ WasmRuntime     ← wasmtime + WIT（后续）                            │
│                                                                          │
│  InstanceManager（现有：状态机/守卫/supervisor/hub）                       │
│    └─ 只依赖 PluginRuntime，不再直接 find_source/find_sink                │
└───────────────▲───────────────────────────────────────────────┬──────────┘
                │ 数据面不变（SampleSender → hub → pipeline）    │ 管理面 /api/v1/plugins
        ┌───────┴────────┐                              ┌───────▼─────────┐
        │ 现有协议插件     │                              │ 前端 Plugins 页面 │
        │ opcua/modbus/... │                              │ schema 表单/健康  │
        └─────────────────┘                              └─────────────────┘
```

**数据面零改动**：`Source`/`Sink` 的 `run` 签名、hub、pipeline、`SampleSender`、lux 读写都不变。
**管理面新增**：目录、schema、能力、健康、runtime 适配。

## 五、核心模型

### 5.1 `PluginDescriptor`

```rust
// src/plugin/descriptor.rs（目标；名称可按评审调整）
pub struct PluginDescriptor {
    /// 稳定 id。沿用现有 app_code（已锁定，不可改名），如 "modbusClient"、"luaClient"。
    pub id: PluginName,
    /// 角色：南向采集 / 北向发布 / 数据转换 / 告警。
    pub role: PluginRole,
    /// 运行位置。
    pub runtime: RuntimeKind,
    /// 宿主 API 版本，如 "1.2"；sidecar 对应 Hello.api_ver，Lua 对应 @api_ver。
    pub api_ver: ApiVersion,
    /// 入口：内置类型 / Lua 目录 / 外部命令 / wasm 文件。
    pub entry: Entry,
    /// 配置 JSON Schema（见 §九）。
    pub params: ConfigSchema,
    /// 能力闭集（见 §十）。
    pub capabilities: CapabilitySet,
    /// 健康检查与超时。
    pub health: HealthSpec,
    /// 展示元数据：name / description / version / source / icon。
    pub display: DisplayMeta,
}

pub enum PluginRole {
    South,      // Source
    North,      // Sink
    Transform,  // Processor / luaTransform
    Alarm,      // 预留：告警规则
}

pub enum RuntimeKind {
    Builtin,   // inventory，编译进主程序
    Lua,       // mlua + 沙箱
    Sidecar,   // TCP bridge + 外部进程
    Wasm,      // wasmtime + WIT（后续）
}

pub enum Entry {
    Builtin { type_name: &'static str },
    Lua { dir: PathBuf },
    Sidecar { command: Option<CommandSpec> }, // None = 外部自行启动，仅 connect
    Wasm { path: PathBuf, world: String },
}
```

**映射规则**：

- 静态插件：`id = SourceEntry.name`，`role` 由注册的是 Source/Sink 决定，`runtime = Builtin`，
  `params` 从新增的 `Cfg: JsonSchema` 派生，`capabilities` 默认为“宿主可信”。
- Lua 插件：`id = app_code`（`luaClient`/`luaNorth`/`luaTransform`），`role` 从 `@role` 映射，
  `api_ver` 从 `@api_ver`，`params` 从 `@param` 生成，`capabilities` 复用 `CapabilityCard`。
- sidecar：`id = inst_id` 对应的 `app_code = "sidecarClient"`，`role = South`，`runtime = Sidecar`，
  `api_ver` 从 `Hello.api_ver` 读取；`params` 从 `InstConfig.app_config` 的 schema 生成。

### 5.2 `PluginRuntime` trait

```rust
// src/plugin/runtime.rs（目标）
#[async_trait]
pub trait PluginRuntime: Send + Sync {
    /// 校验 descriptor 与入口是否存在；不启动。
    async fn load(&self, d: &PluginDescriptor) -> Result<PluginHandle, PluginError>;

    /// 启动实例：spawn task / 建立连接 / instantiate wasm。
    async fn start(&self, h: &PluginHandle, ctx: PluginCtx) -> Result<(), PluginError>;

    /// 优雅停止：停止接新数据，等待在途，超时强制。
    async fn stop(&self, h: PluginHandle) -> Result<(), PluginError>;

    /// 健康检查：握手延迟、最后错误、重启次数。
    async fn health(&self, h: &PluginHandle) -> PluginHealth;
}
```

约束：

- `PluginHandle` 只持有该 runtime 需要的句柄（JoinHandle、TcpStream、Store 等），不暴露给数据面。
- `PluginCtx` 是现有 `HostContext` 的窄化视图：只给插件真正需要的能力（lux、hwid、shutdown、ctl、
  bridge、metrics、trace、event bus），由 capability 过滤。
- **不做逐样本抽象**：`Source::run`/`Sink::run` 仍然是具体插件实现，runtime 只负责生命周期。

### 5.3 `PluginCatalog`

```rust
pub struct PluginCatalog {
    descriptors: DashMap<PluginName, PluginDescriptor>,
    health: DashMap<PluginName, PluginHealth>,
}

impl PluginCatalog {
    pub fn list(&self) -> Vec<PluginView>;           // /api/v1/plugins
    pub fn get(&self, id: &str) -> Option<PluginView>;
    pub fn schema(&self, id: &str) -> Option<ConfigSchema>;
    pub fn register(&self, d: PluginDescriptor);
    pub fn note_health(&self, id: &str, h: PluginHealth);
}
```

目录同时服务：

- `GET /api/v1/plugins`：完整目录（id/role/runtime/api_ver/version/state/capabilities/health）。
- `GET /api/v1/plugins/{id}/schema`：配置 schema。
- 前端 Plugins 页面：列表、配置表单、状态、日志、重启/卸载。
- `scripts/check_docs.py` 之外的能力表/文档生成（可选）。

### 5.4 `CapabilitySet`

复用 Lua 的 `CapabilityCard` 思路（`src/plugin/lua_core/capability.rs`），收敛成闭集：

| 能力 | 含义 | Lua 对应 | sidecar 对应 | WASM 对应 |
|---|---|---|---|---|
| `net` | 出站 TCP/UDP/HTTP | `C1/C5` | 外部自行实现，需声明 | host function |
| `serial` | 串口 | `C1` | 外部自行实现，需声明 | host function |
| `kv` | 实例持久 KV | `C4` | 无（外部自管缓存） | host function |
| `secrets` | 读实例密钥 | `C4/R3` | 无（token 之外） | host function |
| `control` | 跨实例写命令 | `C3/R3` | 无 | host function |
| `alarm` | 告警事件 | `C3` | 预留 | host function |
| `log` | 结构化日志 | `C6` | bridge 已观察 | host function |

每个能力带 `risk`、`timeout_cap_ms`、`rate_per_sec`、`api_ver`，与 Lua 现有卡片同构。
sidecar 在 `Hello.capabilities` 声明；未声明调用 → `denied` 计数 + 明确错误；WASM 的 host function 按同一表授权。

### 5.5 `ConfigSchema`

```rust
pub struct ConfigSchema {
    pub version: u32,
    pub json: serde_json::Value, // JSON Schema (draft 2020-12)
}
```

生成方式：`#[derive(serde::Deserialize, schemars::JsonSchema)]` 的 `Cfg` 结构体。
校验时机：`PluginRuntime::load` 之前；错误返回 `RsIotError::Config` + 字段路径。
暴露方式：`GET /api/v1/plugins/{id}/schema`；sidecar `ConfigPush.schema_version`。

## 六、Runtime adapter 设计

### 6.1 `BuiltinRuntime`

- 包装现有 `inventory`：`SourceEntry`/`SinkEntry` 注册时同时注册 `PluginDescriptor`。
- `load`：检查 `app_code` 存在、feature 是否编译；`start`：调用现有 `source.run(...)`。
- `stop`：复用 `InstanceManager` 的 `CancellationToken` + 3s 超时。
- 能力：默认 `Trusted`；可选在 descriptor 标注 `capabilities` 供 UI 展示，但不做运行时限制（编译期可信）。

### 6.2 `LuaRuntime`

- 包装 `lua_core`：`load` 解析 `@role/@api_ver/@stdlib/@param/@source`，生成 descriptor；
  `start` 走现有 `lua_client`/`lua_north`/`lua_transform` 的 `Source`/`Sink` 路径。
- 热更新：保留现有 `reload_vm` + 旧 VM 回退；新增 `PluginRevision`（见 §七）。
- 能力：现有 `CapabilityCard` 直接映射；`@param` → `ConfigSchema`。

### 6.3 `SidecarRuntime`

- 保留现有 bridge acceptor + `sidecarClient` Source，不重写数据面。
- 新增：
  - `PluginDescriptor` 从 `InstConfig.app_config` + `Hello` 生成；
  - `ConfigSchema` 从 `app_config` 的 `Cfg` 结构体生成；
  - 协议 2.1（§八）；
  - 可选托管进程（§6.5）。

### 6.4 `WasmRuntime`（后续）

- 选型：`wasmtime` + Component Model + WIT + `wasm32-wasip2`；不自造 ABI。
- WIT world 草案：

```wit
package rsiot:plugin@1.0.0;

world source {
  import rsiot:host/log;
  import rsiot:host/kv;
  import rsiot:host/net;
  import rsiot:host/serial;
  import rsiot:host/secrets;
  export on-start: func(cfg: config) -> result<_, error>;
  export on-tick: func() -> result<list<sample>, error>;
  export on-command: func(cmd: command) -> result<ack, error>;
}
```

- 工具链：Rust `cargo component`；Python `componentize-py`；JS/TS `jco`。
- 资源限制：fuel/epoch + memory max；能力按 `CapabilitySet` 授权。
- 卸载：drop `Store`/`Instance`，天然安全；不涉及 `dlclose`。
- 前置条件：G1-G7 完成；先做 1 个 demo 插件（如 Lua 已有的 modbus-like 场景）验证。

### 6.5 `ManagedProcessRuntime`（可选，P1）

- 现状：外部进程由用户启动，rs-iot 只 accept TCP；现场部署多一步，崩溃无人拉起。
- 新增 `host_kind=managed`（或 `RuntimeKind::Sidecar` 的 `command: Some(...)`）：
  - `command/args/env/cwd` 来自 descriptor；
  - rs-iot spawn 子进程，stdout/stderr 进 log ring buffer；
  - 退避重启；进程组 / Windows Job Object 防子进程树泄漏；
  - stop 时 Drain → SIGTERM → 超时 SIGKILL；
  - 仍保留“外部 connect”模式，用于跨机部署。

## 七、生命周期与热更新

### 7.1 状态机

```
Discovered → Validated → Loaded → Starting → Running → Draining → Stopped → Removed
                  │ fail                    │ fail
                  ▼                         ▼
                Failed ←────────────────────┘
```

与现有 `InstState` 的映射：

| 新状态 | 现有 `InstState` | 说明 |
|---|---|---|
| `Discovered` | — | catalog 里有 descriptor |
| `Validated` | — | schema/API/权限/入口校验通过 |
| `Loaded` | — | runtime 已加载（inventory 命中 / Lua VM 编译 / 进程已连接 / wasm instantiate） |
| `Starting` | `Starting` | 开始接数据 |
| `Running` | `Running` | 正常 |
| `Draining` | `Stopping` | 停止接新数据，等待在途 |
| `Stopped` | `Stopped` | 已停止，可重启 |
| `Failed` | `Failed` | 失败，保留错误 |
| `Removed` | — | 配置/插件已删除 |

### 7.2 `PluginRevision` 与 `last_good`

```rust
pub struct PluginRevision {
    pub version: semver::Version,
    pub artifact_hash: String,   // 脚本目录/二进制/wasm 的内容 hash
    pub api_ver: u32,
    pub config_hash: String,
}
```

- 每次加载记录 revision；保留最近 N 个（默认 2）。
- 新 revision 先 `load` + 健康检查，再原子切换；旧 revision 进入 `Draining`，超时停止。
- 失败回滚 `last_good`；审计记录（谁、何时、from → to、结果）。
- Lua 已有“reload 失败回退旧 VM”（`src/plugin/lua_core/reload.rs`），本设计把它泛化到所有 runtime。

### 7.3 失败策略矩阵

| 角色/场景 | 策略 | 说明 |
|---|---|---|
| 南向采集断连 | 实例继续重连；`Failed` 仅在 task 退出/配置错误时 | 保持现有 `startinst` 手动重拉语义，可选自动重启 |
| 南向数据满 | `tx.send().await` 背压；sidecar 用 Credit/Pause 显式化 | 不丢数据优先；必要时按角色配置 drop 策略 |
| 转换失败 | 丢弃该样本 + 计数；不阻断 pipeline | 与现有 `Processor` 语义一致 |
| 北向发布失败 | 重连/重试；不阻塞 hub | 保持 `luaNorth` 现有语义 |
| 写命令失败 | 不重试，最新值生效 | 与协议文档 §6 一致 |
| 配置非法 | `Validated` 失败，不启动 | schema 校验前置 |
| 能力越权 | 拒绝 + 计数 + 审计 | sidecar/WASM 显式声明 |

## 八、sidecar 协议 2.1

### 8.1 帧变化

保留 v2 的 `[4B BE length][1B type][JSON body]` 与 7 种帧，新增/扩展：

| 帧 | 方向 | v2 | 2.1 增量 |
|---|---|---|---|
| `Hello` | ext→bridge | `{inst_id, token, ver}` | `{ver:"2.1", plugin_version, api_ver, capabilities[], features[]}` |
| `HelloAck` | bridge→ext | `{ok, msg, heartbeat_secs}` | `{host_api_ver, features[], limits:{max_frame, max_inflight}}` |
| `ConfigPush` | bridge→ext | `{inst_id, devices[]}` | `{config_version, etag, schema_version, devices[]}` |
| `ConfigAck` | ext→bridge | — | `{ok, config_version, err?}` |
| `Credit` | bridge→ext | — | `{n}`，显式放行 n 条 DataUp |
| `Pause`/`Resume` | bridge→ext | — | 背压暂停/恢复 |
| `Drain` | bridge→ext | — | 停止接新 DataUp，等待在途 |
| `Goodbye` | ext→bridge | — | 外部确认退出 |

### 8.2 兼容策略

- `Hello.ver == "2"` 继续接受，按 v2 语义运行；`2.1` 才启用新帧。
- `HelloAck.features[]` 协商：外部不认识的 feature 忽略；宿主不认识的 feature 拒绝并说明。
- v2 弃用窗口：至少一个大版本；文档标注 `v2 deprecated`。
- 所有新帧都可缺省；缺省时退化为 v2 行为（例如无 `ConfigAck` 则视为已应用）。

### 8.3 配置热更新

现有协议要求“配置变更 → 重启实例”（协议文档 §9）。2.1 改为：

1. 宿主检测 `InstConfig` 变化 → `ConfigPush{config_version}`。
2. 外部应用后回 `ConfigAck{ok, config_version}`。
3. 宿主记录 revision；失败则回滚上一版并告警。
4. 不支持 `ConfigAck` 的 v2 插件仍走“重启实例”。

## 九、配置 schema

### 9.1 生成与校验

```rust
#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModbusCfg {
    pub transport: Transport,
    #[schemars(range(min = 1, max = 247))]
    pub slave_id: u8,
    // ...
}
```

- `PluginDescriptor.params` 由 `schemars::schema_for!(Cfg)` 生成。
- `start_source` 前校验；错误返回 `RsIotError::Config` + 字段路径。
- Lua `@param` → 同一 JSON Schema 形态（类型/默认/范围/可选）。
- sidecar `ConfigPush.schema_version` 与 `Cfg` 的 schema 版本一致。

### 9.2 管理面

- `GET /api/v1/plugins/{id}/schema`：原始 JSON Schema。
- `GET /api/v1/plugins`：目录带 `schema_version`。
- 前端 `frontend/src/lib/pluginSchemas.ts` 已有 schema 相关基础，可扩展为通用表单渲染。
- `scripts/check_docs.py` 可生成能力表/配置表（可选）。

## 十、能力与权限

- 闭集：`net` / `serial` / `kv` / `secrets` / `control` / `alarm` / `log`。
- 每个能力：`risk`（R0-R3）、`timeout_cap_ms`、`rate_per_sec`、`api_ver`。
- 授予：
  - 静态：编译期可信，默认全量；descriptor 仅用于展示。
  - Lua：沿用现有 `CapabilityCard`；`@role` 决定默认能力包（transform 裁剪 C1/C3/C5）。
  - sidecar：`Hello.capabilities` 显式声明；宿主校验；未声明调用 → `denied`。
  - WASM：host function 按能力授权。
- 审计/观测：`denied` 计数进 `PluginHealth` 与 `/metrics`；越权日志带 `plugin_id`/`capability`。

## 十一、SDK 与 conformance

### 11.1 `rsiot-plugin-sdk`（目标新建）

```
crates/rsiot-plugin-sdk/
├── src/
│   ├── descriptor.rs   # PluginDescriptor / PluginRole / RuntimeKind / CapabilitySet
│   ├── protocol.rs     # sidecar 帧类型 + 编解码（v2 + 2.1）
│   ├── client.rs       # 握手、心跳、重连、缓存补传 helper
│   └── config.rs       # ConfigSchema / 校验 helper
└── tests/
```

- 目标：团队新协议开发不必拉全量 lux/协议栈；第三方插件有稳定契约。
- 版本：SDK 用 semver；sidecar 协议作为稳定边界；descriptor 的 `api_ver` 独立演进。

### 11.2 `rsiot-plugin-conformance`（目标新建）

- mock bridge：模拟 Hello/HelloAck/ConfigPush/Command/Heartbeat。
- 用例：
  - 版本协商（v2 / 2.1 / 拒绝未知大版本）
  - token 失败、实例未启动、slot 满
  - ConfigAck 成功/失败/超时
  - DataUp 校验闸（未绑定 devid / 未建模 tag）
  - Credit/Pause/Resume
  - Drain/Goodbye
  - 重连 + 缓存补传 + 迟到数据
- 输出：JUnit/JSON，CI 可跑；第三方插件仓库可直接引用。

### 11.3 语言 SDK

| 语言 | 优先级 | 理由 |
|---|---|---|
| Python | P0 | 现场集成商最多，样例已有 `examples/ext-plugin-python/` |
| Go | P1 | OPC DA 样例已有；工业库多 |
| TypeScript | P2 | Web 背景集成商；可选 |
| Rust | P0（内部） | 与 `rsiot-plugin-sdk` 同源 |

## 十二、可观测性与 Plugins 页面

### 12.1 `PluginHealth`

```rust
pub struct PluginHealth {
    pub id: PluginName,
    pub runtime: RuntimeKind,
    pub version: String,
    pub api_ver: ApiVersion,
    pub state: PluginState,
    pub last_error: Option<String>,
    pub restarts: u64,
    pub handshake_ms: Option<u64>,
    pub denied: u64,
    pub config_version: Option<String>,
}
```

### 12.2 管理面

- `GET /api/v1/plugins`：目录 + health。
- `GET /api/v1/plugins/{id}`：单插件详情（能力、schema、最近日志）。
- `POST /api/v1/plugins/{id}/restart`：重建实例（可选）。
- `/metrics`：新增 `plugin`/`runtime`/`api_ver` 标签；注意基数上限，只放闭集。
- 前端 Plugins 页面：列表、状态、版本、能力、配置表单、日志、重启/卸载。

## 十三、兼容与迁移

### 13.1 零行为变化

- 第一阶段只加目录/descriptor/runtime adapter，不改变 `Source::run`、hub、pipeline、lux。
- 现有 `tests/*_e2e.rs` 必须全绿；`/api/v1/plugins` 只增字段，不删 `sources`/`sinks`。
- `app_code` 不变；`InstConfig` 不变；`config.toml` 不变。

### 13.2 分 runtime 迁移

| runtime | 迁移方式 | 风险 |
|---|---|---|
| Builtin | 注册时附带 descriptor；`start_source` 改走 `BuiltinRuntime` | 低 |
| Lua | 解析 manifest 生成 descriptor；`start` 复用现有 `lua_client` | 低 |
| Sidecar | 生成 descriptor；协议 2.1 增量；v2 兼容 | 中 |
| WASM | 新增，不迁移 | 高（后续） |

### 13.3 文档与索引

- 新增本设计后更新 `docs/README.md` 主题链路 + designs 索引。
- 实施后回写状态、`**实施**`、`**代码**`、`**测试**`、`**遗留**`。
- 代码模块头 `//!` 反向链接本文（规范 §5）。

## 十四、测试与验收

| 层级 | 内容 | 门槛 |
|---|---|---|
| 单元 | descriptor 映射、schema 校验、capability 判定 | `cargo test` |
| 集成 | `BuiltinRuntime`/`LuaRuntime`/`SidecarRuntime` 各 1 条 happy path + 1 条失败路径 | `cargo test` |
| 契约 | conformance 套件 v2/2.1 全过 | `cargo test -p rsiot-plugin-conformance` |
| 回归 | 现有 `tests/*_e2e.rs` 全绿 | CI |
| 文档 | `python scripts/check_docs.py` | CI docs job |
| 现场 | 1 个 sidecar 插件 + 1 个 Lua 脚本 + 1 个静态插件同进程运行 | field-kit |

验收判据：

1. `/api/v1/plugins` 能列出三种 runtime 的插件，带 role/runtime/api_ver/state。
2. 配置错误在启动前返回字段路径，不产生半启动实例。
3. sidecar 2.1 能完成 ConfigAck + Drain；v2 仍可连接。
4. 现有 e2e 零回归；指标/日志带 `plugin_id`。
5. `docs/README.md` 索引与状态块通过 `check_docs.py`。

## 十五、风险与不做的事

| 风险 | 缓解 |
|---|---|
| 过度抽象，热路径变慢 | runtime trait 只在实例粒度；`Source::run` 直接调用 |
| sidecar 2.1 破坏外部插件 | v2 继续接受；feature 协商；conformance 双版本 |
| schema 漂移 | `deny_unknown_fields` + 契约测试 + 生成文档 |
| 托管进程泄漏 | 进程组/Job Object；默认关闭；stop 超时 SIGKILL |
| WASM 工具链不成熟 | 放到最后；先做 1 个 demo；不自造 ABI |
| 能力模型太细 | 先 7 个闭集能力，新增需评审 |
| 指标基数爆炸 | 标签只放闭集；每插件固定标签 |

**不做**：

- 不做 Rust 原生动态库。
- 不替换静态核心协议。
- 不在第一阶段做 WASM。
- 不让插件 UI 直接调核心 API（前端仍走 REST/JWT）。
- 不为每个插件发明新的配置格式。

## 十六、分阶段路线（约 8 周）

| 周 | 阶段 | 产出 | 验收 |
|---|---|---|---|
| 1–2 | P0-1 内核 | `PluginDescriptor` + `PluginCatalog` + `PluginRuntime`；三个 adapter 包现有实现；`/api/v1/plugins` 返回目录 | 现有 e2e 全绿；目录可见 |
| 3 | P0-2 schema | `schemars` 接入 sidecarClient + luaClient；启动前校验；schema 端点 | 配置错误前置；schema 可读 |
| 4–5 | P1-3 sidecar 2.1 | 能力协商 + ConfigAck + Credit/Pause + Drain；v2 兼容 | conformance v2/2.1 全过 |
| 4–5 | P1-4 托管进程 | `ManagedProcessRuntime` MVP：spawn/重启/日志/停止 | 1 个外部插件单进程部署 |
| 6 | P1-5 SDK | `rsiot-plugin-sdk` + `rsiot-plugin-conformance` + Python SDK | 第三方样例用 SDK 重写 |
| 7 | P2-6/7 能力与健康 | `CapabilitySet` 统一；`PluginHealth`；Plugins 页面 | 能力拒绝可见；页面可操作 |
| 8 | 收口 | 文档/索引/状态回写；现场验证；WASM 立项评审 | `check_docs.py` + 现场 3 runtime 同跑 |

## 十七、开放问题

1. `PluginDescriptor` 放在主 crate 还是 `rsiot-plugin-sdk`？建议放 SDK，主 crate 依赖 SDK，避免第三方复制类型。
2. `PluginRuntime` 是否接管 `InstanceManager` 的 supervisor？建议保留 `InstanceManager` 作为唯一状态机，runtime 只做 load/start/stop。
3. sidecar 2.1 的 `features[]` 命名与语义需要一张表；建议 `config_ack`、`credit`、`drain`、`capabilities` 四个 feature。
4. `ConfigPush.schema_version` 与 `api_ver` 是否合并？建议分开：协议版本 vs 配置 schema 版本。
5. 托管进程在 Windows 用 Job Object 还是 `CREATE_NEW_PROCESS_GROUP` + `GenerateConsoleCtrlEvent`？建议 Job Object 为主，Ctrl 事件为优雅停止。
6. WASM world 是否把 `Source`/`Sink` 分成两个 world？建议按 role 分 world，避免 transform 拿到网络能力。
7. 能力闭集是否包含 `fs`？建议第一阶段不包含；现场脚本需要文件时用 `kv` 或宿主 API。

## 附录 A：与现有文档的关系

- 决策：[插件扩展策略](../decisions/2026-08-07-plugin-extension-strategy.md)（已实施）
- 指南：[插件开发指南](../guides/plugin-development-guide.md)（活文档）
- 协议：[外部插件协议](../reference/external-plugin-protocol.md)（v2 已实施）
- 规范：[埋点规范](../conventions/instrumentation-conventions.md)（活文档）
- 接口：[REST API](../reference/rest-api.md)（活文档）
- 索引：[docs/README.md](../README.md)
