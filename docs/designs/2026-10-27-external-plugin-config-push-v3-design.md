# 外部插件配置下发升级设计（ConfigPush v3）

> **状态**：implemented（批次 1+2 已实施；批次 3 可选未做）
> **日期**：2026-10-27
> **实施**：2026-10-27 `f77dc25` 设计入库 → 本 commit 批次 1+2 落地（Go 示例升 v2 / 文档版本修正 / ConfigPush v3 / 版本集合协商 / 凭证解析下发）
> **代码**：`src/plugin/builtin/sidecar_client/mod.rs`（`build_config_push`/`push_devices`/`push_app_config`/`resolve_app_config_for_push` + `SUPPORTED_VERSIONS`）、`src/db/secrets.rs`（`resolve_ref`）、`src/host.rs`（`BridgeConn`）、`examples/ext-plugin-opcda-go/main.go`
> **测试**：`tests/sidecar_e2e.rs`（10 用例，含 v3 载荷/v2 兼容/凭证解析/未写入凭证/未知版本拒连）+ `sidecar_client::tests`（14 单测）
> **遗留**：批次 3（Go 示例升 v3 删连接 flag、前端 `pluginSchemas.ts` 加 sidecarClient 条目）——可选，未做
> **范围**：sidecarClient bridge 协议配置面——连接参数、设备级绑定参数、凭证、版本协商
> **依据**：设计期基于 HEAD `b821c9f` 逐行核实；实施后行号已随代码变动，功能位置以函数名定位
> **关联**：[协议规约 v3](../reference/external-plugin-protocol.md)｜[开发指南](../guides/external-plugin-dev-guide.md)｜[架构优化评估](2026-10-27-architecture-optimization-review.md) §3.3（bind 类型化，本设计的相邻议题）

## 0. 一句话结论

协议文档第 5 行把外部插件定义为**纯运行时**（"配置/数据/状态全在 rs-iot，本地仅缓存数据待补传"），但实现只下发了三层配置中的最末一层（点位）。连接参数、设备级绑定参数、凭证通道三处缺口，使这个定位目前只在文档上成立。升级路径不是引入新概念，而是**把数据模型里已经齐备的结构补全下发**——`app_config`、`DeviceBinding.bind`、模型 tags 三层在 rs-iot 中都已存在，缺的只是 `ConfigPush` 组装时把它们带上。

## 1. 定位与必须保持的不变量

### 1.1 现有定位（原文引用）

- 协议文档 `external-plugin-protocol.md:5`：「外部插件是**纯运行时**：配置/数据/状态全在 rs-iot，本地仅缓存数据待补传。」
- 模块头 `sidecar_client/mod.rs:12`：「外部插件=纯运行时：配置/数据/状态全在 rs-iot，外部仅本地缓存数据待补传（断连容错）。」
- `CLAUDE.md:27`：「**外部=纯运行时**（配置/数据/状态全在 rs-iot，本地仅缓存数据待补传）」

三处表述一致，说明这是**设计意图**而非事后追认。本设计是对该意图的实现补全。

### 1.2 必须保持的不变量（任何方案不得违反）

| # | 不变量 | 现有保障 |
|---|---|---|
| I1 | 内建插件（Rust Source）行为零变化 | 本设计只触碰 `sidecar_client` 与 `expand` 的下发路径，不进入任何内建插件 |
| I2 | 现有 v2 外部插件在过渡期内继续工作 | 版本协商改为集合匹配（§4.3），v2 载荷组装路径保留 |
| I3 | lux 永不对外暴露 | 外部只连 bridge；配置由宿主读出后下发，外部无 lux 客户端 |
| I4 | 管理面回显继续脱敏 | `redact_inst`（`routes_inst.rs:297`）不动；下发视图与回显视图是两条独立路径 |
| I5 | 校验闸语义不变 | DataUp 的 devid/tag 闸（`sidecar_client/mod.rs:385-446`）不动 |
| I6 | 实例生命周期与故障隔离不变 | 一实例一 task 一 slot（`host.rs:89-91` + `sidecar_client/mod.rs:228`）不动 |

## 2. 现状核实（事实与证据）

### 2.1 ConfigPush v2 实际载荷

`sidecar_client/mod.rs:328-332`：

```json
{"inst_id": "...", "v": 2, "devices": [{"devid": "...", "model": "...", "tags": [...]}]}
```

其中 `tags[]` 由 `:302-319` 组装，含 `name/type/addr/unit/rw`；`addr` 是 `addr::resolve` 后的绝对地址（`:307-311`）。

### 2.2 三层配置在 rs-iot 中的存储位置

| 层 | 语义 | 存储位置 | 现有下发 |
|---|---|---|---|
| 连接级 | 服务器地址、账号、协议参数 | `InstConfig.app_config`（`types.rs:280`，自由 JSON） | ❌ 完全不下发 |
| 设备级 | OPC DA group / Modbus unit id / 更新速率 | `DeviceBinding.bind`（`types.rs:241`，`BTreeMap<String,String>`） | ❌ 组装时被丢弃 |
| 点位级 | OPC item id / Modbus 寄存器地址 | `DeviceModel.tags[].addr`（模板）+ `DeviceEntry.params`（实例化参数） | ✅ 已下发（解析后） |

### 2.3 `bind` 被丢弃的确切位置

`expand::ExpandedDevice` 同时携带 `bind` 与 `entry`（`expand.rs:22-31`），`expand_binding` 原样填入（`expand.rs:94-99`）。但 ConfigPush 组装只取了三个字段：

```rust
// sidecar_client/mod.rs:321-325  ← 三字段里没有 bind
serde_json::json!({
    "devid": ex.devid,
    "model": ex.model.model_id,
    "tags": tags,
})   // ex.bind 与 ex.entry 在此丢失
```

`bind` 的唯一现有消费者是 `to_device_spec` 里的 `slave_id`（`expand.rs:60`），即内建 modbus。外部插件拿不到它。

### 2.4 `app_config` 的唯一消费者

`sidecar_client` 全文只读一个键：

```rust
// sidecar_client/mod.rs:188-192
fn parse_hb_timeout(app: &serde_json::Value, default: Duration) -> Duration {
    app.get("heartbeat_timeout_secs")...
}
```

其余连接参数（OPC DA 的 `prog_id`/`host`/`username`/`password`、Modbus 的串口参数等）只能靠外部进程自己的命令行或配置文件——`examples/ext-plugin-opcda-go/main.go:541-543` 正是这么做的（`--prog-id`/`--opc-host`/`--period` 三个 flag）。

### 2.5 凭证链路现状

- lux `secrets` 表已有 AEAD 加密存储（`db/secrets.rs:116-128`）与透明解密读取（`:170-177`）。
- 两条既有消费路径：脚本侧 `ctx:secret_get`（`lua_core/api/mod.rs:519-565`，受实例 `app_config.secrets` 授权清单约束）；宿主侧 `resolve_password_opt`（`lua_north/contract.rs:439-454`，把 `secret:<ref>` 解析成明文，连接前一刻完成）。
- `sidecar_client` 对 `secrets` **零引用**——外部插件没有任何凭证获取通道。

### 2.6 版本协商是精确匹配

```rust
// sidecar_client/mod.rs:49
const PROTO_VER: &str = "2";

// sidecar_client/mod.rs:606-617
if hello.ver != PROTO_VER { /* 拒连 + 升级提示 */ }
```

精确相等，没有"宿主支持多版本"的概念。任何新增字段若伴随版本号提升，会**同时拒掉所有旧客户端**——M-3 升 v2 时已发生过一次（v1 全拒）。

### 2.7 两处文档/实现不一致（独立于本设计，需一并修正）

1. **`ver` 缺失语义**：协议文档 `external-plugin-protocol.md:52` 写「ver 缺失或 ≠ "2" → 拒连」，但代码是 `#[serde(default = "default_ver")]`（`sidecar_client/mod.rs:66-67`），缺失时默认填 `"2"` 从而**放行**。测试 `hello_ver_defaults_to_proto`（`:733-736`）把这个行为锁定了。
2. **文档版本标记滞后**：`docs/README.md:38` 仍写「协议规约 v1 … v1 已实施」，而实现早已是 v2；`external-plugin-protocol.md:113` 与 `:115` 是逐字重复的两行。

### 2.8 示例与协议脱节

| 示例 | 协议版本 | 状态 |
|---|---|---|
| `examples/ext-plugin-python/main.py:32` | `PROTO_VER = "2"` | ✅ 可连 |
| `examples/ext-plugin-opcda-go/main.go:47` | `const protoVer = "1"` | ❌ **连不上**（被 `:606` 拒） |

Go 示例的 `configPush` struct（`main.go:50-65`）仍是 v1 的 `{label,value,children}` 形态，`README.md:3` 也自称"协议 v1 的 Go 参考实现"。这是现成的 bug，不是文档问题。

## 3. 缺口归纳

| # | 缺口 | 后果 | 严重度 |
|---|---|---|---|
| G1 | `bind` 不下发 | 设备级协议参数（OPC group、更新速率、unit id）无法集中管理，必须写进外部进程配置 | 高 |
| G2 | `app_config` 不下发 | 连接参数（服务器/账号/端口）无法集中管理，违背"纯运行时"定位 | 高 |
| G3 | 无凭证通道 | 密码只能落外部进程配置文件，无法用 lux `secrets` 加密存储 | 高（安全） |
| G4 | 版本精确匹配 | 每次加字段都要么破坏旧客户端、要么无法演进 | 中（工程） |
| G5 | `ver` 缺失放行 | 文档承诺的严格性未实现 | 低 |
| G6 | Go 示例 v1 | 示例不可运行 | 中 |
| G7 | 文档版本漂移 | 索引与正文版本不一致、重复段落 | 低 |

## 4. 设计

### 4.1 核心决策：不引入"连接"新概念

**问题**：用户需求是"一个 OPC DA 插件采集多个服务器"。有两种实现：

- **(a) 一实例一服务器**：每服务器建一个 sidecar 实例，`app_config` 各不同。
- **(b) 一实例多服务器**：协议引入 `connections[]` + 帧级 `conn_id` 多路复用。

**推荐 (a)**，理由：

1. **与内建插件一致**：`opcua_client`/`modbus_client`/`s7_client` 都是"一实例一连接"。modbus 的多从站是**同一 TCP 连接内的不同 unit id**（`modbus_client/mod.rs:191` `dev.slave_id.unwrap_or(fallback)`），不是多连接。
2. **故障隔离天然成立**：一个服务器不可达只让该实例 `degraded`，其余实例不受影响。
3. **生命周期独立**：独立启停、独立 `stat`、独立 token（`routes_inst.rs:115-121` 每次 `set` 轮换 token）。
4. **零协议改动**：多实例是 `InstanceManager` 现成能力。
5. **改动 (b) 是破坏性的**：`BridgeRegistry` 的 slot 容量为 1（`host.rs:89-91` 注册表 + `sidecar_client/mod.rs:228` 容量 1），多连接需要改注册表结构 + 所有帧增加路由字段 + 全部示例重写。

**决策**：本次升级保持一实例一连接。(b) 若将来确有需求，作为独立议题，不在本设计范围。

### 4.2 ConfigPush v3 载荷

在 v2 基础上**只增不改**（v2 客户端忽略未知字段即兼容）：

```json
{
  "inst_id": "opcda-1",
  "v": 3,
  "app_config": {
    "prog_id": "Matrikon.OPC.Simulation.1",
    "host": "192.168.1.20",
    "username": "opcuser",
    "password": "<宿主解析后的明文，或 null>"
  },
  "devices": [
    {
      "devid": "devOpc",
      "model": "vx_dtz341",
      "bind": {"group": "g1", "update_rate": "1000"},
      "tags": [
        {"name": "temp", "type": "f32", "addr": "Random.Real4", "unit": "C", "rw": "r"}
      ]
    }
  ]
}
```

变更点：

1. **新增顶层 `app_config`**——`InstConfig.app_config` 减去宿主内部键后的视图。
2. **新增 `devices[].bind`**——`ExpandedDevice.bind` 原样（`:26`），零转换。
3. `v` 由 `2` 升 `3`；`devices[]` 其余字段与 v2 逐字节一致。

#### 4.2.1 `app_config` 的裁剪规则

复用 north 的 `to_script_cfg` 原则（`lua_north/config.rs:202-216`；测试 `:318-320` 明确断言"宿主内部项不进脚本 cfg"）：**排除宿主内部键，其余原样下发**。

| 排除键 | 理由 |
|---|---|
| `secrets` | 脚本侧授权清单，宿主内部语义；外部插件不需要知道 |
| `heartbeat_timeout_secs` | 已通过 `HelloAck.heartbeat_secs` 下发（`sidecar_client/mod.rs:272-276`），重复无意义 |

**不做键白名单**。理由：`app_config` 的定义就是"任意 JSON，由各插件自行解析"（`types.rs:278-280`）；宿主不知道 OPC DA 用 `prog_id` 而 Modbus 用 `serial_port`，白名单会强迫宿主理解每个协议。排除清单是封闭的、可审计的，白名单是开放的、必然遗漏。

#### 4.2.2 空 `app_config` 的处理

`app_config` 缺省是 `serde_json::Value::Null`（`types.rs:279` 的 `#[serde(default)]`）。v3 载荷中统一序列化为 `{}` 而非 `null`，降低外部插件解析分支。

### 4.3 版本协商改为集合匹配

```rust
// 替换 sidecar_client/mod.rs:49
/// 宿主支持的协议版本。集合匹配而非精确匹配——v3 加字段后 v2 客户端仍可连。
const SUPPORTED_VERSIONS: &[&str] = &["2", "3"];
/// v3 引入的当前版本（新建示例与文档以此为准）。
const CURRENT_VERSION: &str = "3";
```

```rust
// 替换 sidecar_client/mod.rs:606-617
if !SUPPORTED_VERSIONS.contains(&hello.ver.as_str()) {
    /* 拒连；msg 列出 SUPPORTED_VERSIONS，而非单一版本 */
}
```

ConfigPush 按客户端版本分支组装：`ver == "2"` 走现有载荷（`:299-332` 原样），`ver == "3"` 追加 `app_config` 与 `bind`。**两条路径共用同一次 `expand_devices` 调用**（`:286`），不增加 lux 读取。

#### 4.3.1 `ver` 缺失语义（G5 的裁决）

两个选项：

- **(i) 保持现状**（缺失默认 `"2"`），修正文档为「ver 缺失按 v2 处理」。
- **(ii) 改为必填**（`Option<String>`，缺失即拒），与文档现有措辞一致。

**推荐 (i)**，理由：改动最小；且不存在"不发 ver"的真实客户端（v1 时代的 Go 示例发的是 `"ver":"1"`，`main.go:289`），改成必填无实际收益却引入风险。文档按 (i) 修正，并把 `hello_ver_defaults_to_proto` 测试注释改为"缺失按 v2 兼容"而非"默认等于 PROTO_VER"。

### 4.4 凭证：宿主解析后下发，边界写清楚

`app_config.password` 若形如 `secret:<ref>`，宿主在组装 ConfigPush 前用 `secrets::get` 解析成明文（复用 `lua_north/contract.rs:439-454` 的同一函数，避免两处实现漂移）。字面值原样下发。

**安全边界（必须写进协议文档，不是隐含约定）**：

- bridge 通道承载**明文凭证**。token 保护的是"谁能连"，不是"通道是否加密"。
- 同机部署（`bind_host=127.0.0.1`，默认）风险可控；跨机部署必须 TLS 或隔离网段。
- 与 north 的取舍一致：`resolve_password_opt` 也是在连接前把 secret 解析成明文（`:439-454`），明文只存在于建立连接所需的生命周期内。

**为什么不新增 SecretGet 帧**：外部进程要维持 OPC DA 的持久连接，密码必须常驻其内存；"按需拉取后立即丢弃"在这个场景不成立。增加一个帧只增加实现负担，不减少实际暴露面。

**为什么不支持 `push_secrets: false` 开关**：多一个配置维度、多一条测试路径，而收益（让 ConfigPush 可被安全抓包）在 TLS 缺失时并不成立——抓包者同样能看到 DataUp 明文。留到 TLS 落地时一并考虑。

### 4.5 `bind` 的类型化（可选，与评估文档议题 3 合并）

本次只做"原样下发字符串 map"，不做类型化。理由：

- 类型化（`bind_schema`）是**契约变更**，影响 `DeviceBinding` 序列化、`/api/v1/devices` 返回、MCP 工具输出、全部协议插件——已在 `2026-10-27-architecture-optimization-review.md` §3.3 单独立项。
- 两者可以先后做：先下发（本次，零风险），后类型化（独立议题，需要 ADR 与迁移路径）。
- 若先做类型化，下发格式会随之改变，等于一次做两个破坏性变更。

### 4.6 配置变更通知（明确不做）

现状是"改配置 → 重启实例 → 外部重连 → 拿新 ConfigPush"（协议文档 `:113`）。v3 不改变这一点。

理由：增量通知需要 diff 协议、版本序号、外部确认回执三件套；而重启实例是秒级操作，且外部插件本来就要处理重连（协议已有完整重连语义，`:83`）。在没有任何现场反馈"重启代价不可接受"之前，不做。

## 5. 兼容性与迁移

| 对象 | 影响 | 处理 |
|---|---|---|
| 现有 v2 外部插件（Python 示例） | 无影响 | 继续按 `ver:"2"` 连接，收到 v2 载荷 |
| 内建插件 | 无影响 | 不经过 `sidecar_client` |
| `InstConfig` 结构 | **无变化** | 不新增字段；`app_config` 与 `bind` 都是既有字段 |
| REST API | **无变化** | `inst/set` 请求体不变；回显脱敏不变 |
| lux 表结构 | **无变化** | 不新增表/字段 |
| Go 示例 | 需要重写 | 升 v2 基线 + 可选 v3（见 §6） |

**关键**：本设计**不修改任何持久化结构**，因此无数据迁移、无回滚脚本、无版本兼容矩阵。

## 6. 实施顺序（建议分三批）

### 批次 1：修正既有不一致（无设计风险，可立即做）

1. Go 示例升 v2（`main.go:47` `protoVer = "2"`，`configPush` struct 改 `{inst_id, v, devices:[{devid, model, tags:[{name,type,addr,unit,rw}]}]}`，`collectItems` 改读 `tags[].name`）。**当前它连不上，这是 bug 修复不是新功能。**
2. `docs/README.md:38` 版本标记改 v2；删除 `external-plugin-protocol.md:113/115` 重复行。
3. 协议文档 `:52` 的 `ver` 缺失语义按 §4.3.1 裁决结果修正；`hello_ver_defaults_to_proto` 测试注释同步。

### 批次 2：v3 协议（本设计主体）

4. 抽 `build_config_push(inst, expanded, ver) -> serde_json::Value` 纯函数，单测覆盖 v2/v3 两种载荷形状与 `app_config` 裁剪规则。
5. 版本协商改集合匹配（§4.3）；新增 `unsupported version` 的 e2e（当前 `tests/sidecar_e2e.rs` 无此覆盖）。
6. `sidecar_client` 装配期解析 `app_config.password` 的 `secret:` 引用（复用 `resolve_password_opt`），解析失败按"凭证缺失"处理并 warn（不静默无密码连接——照 north 的 `contract.rs:434-437` 注释语义）。
7. e2e：v3 客户端收到 `app_config` 与 `bind`；v2 客户端仍收到 v2 形状；secret 引用解析成明文。
8. 协议文档升 v3（新增 `app_config`/`bind` 字段说明 + §10 安全节补"明文凭证过 bridge"）；开发指南 §7 的"当前只下发 devices"整节重写。

### 批次 3：示例与前端（可选）

9. Go 示例升 v3，删除 `--prog-id`/`--opc-host` 三个 flag，改从 ConfigPush 读。
10. 前端实例表单对 `sidecarClient` 增加连接参数提示（`pluginSchemas.ts` 现无 `sidecarClient` 条目）。

## 7. 验收判据

1. `cargo test --test sidecar_e2e` 全绿，且新增至少 4 个用例：v3 载荷含 `app_config`/`bind`、v2 客户端载荷不变、`secret:` 引用解析成明文、未知版本拒连并列出支持集。
2. `cargo test --test modbus_multi_slave_e2e` 全绿（证明 `expand` 未被本设计改动影响）。
3. `python3 scripts/check_docs.py` 通过。
4. 人工核对：`grep -n 'bind' src/plugin/builtin/sidecar_client/mod.rs` 在 ConfigPush 组装段命中。
5. Go 示例能连上并收到配置（需要 Windows + OPC DA Server；无环境时在 README 标注"未在本机验证"）。

## 8. 不做什么（明确排除）

| 不做 | 理由 |
|---|---|
| 一实例多服务器（`connections[]` + `conn_id`） | 破坏性变更；多实例已能覆盖；见 §4.1 |
| `bind` 类型化（`bind_schema`） | 独立议题，已在评估文档 §3.3 立项；先下发后类型化，避免一次两个破坏性变更 |
| 新增 SecretGet 帧 | 持久连接需要密码常驻内存，按需拉取不成立；见 §4.4 |
| 配置热更新通知 | 需要 diff 协议 + 版本序号 + 回执三件套；重启代价未被证明不可接受；见 §4.6 |
| 把 `app_config` 原样下发 | 排除宿主内部键（`secrets`/`heartbeat_timeout_secs`）；见 §4.2.1 |
| 改变 DataUp 校验闸 | 不在本设计范围（I5） |

## 9. 待裁决项

| # | 裁决点 | 推荐 | 备选 |
|---|---|---|---|
| D1 | 多服务器：多实例 vs 单实例多连接 | 多实例（§4.1） | 单实例多连接（需协议多路复用，独立议题） |
| D2 | `ver` 缺失：按 v2 兼容 vs 必填拒连 | 按 v2 兼容 + 修正文档（§4.3.1） | 必填（与现有文档措辞一致，但无真实客户端依赖） |
| D3 | 凭证：解析后下发明文 vs SecretGet 帧 | 解析后下发（§4.4） | SecretGet 帧（实现负担大，暴露面未减小） |
| D4 | v3 是否在 v2 之外**保留** v2 载荷路径 | 保留（I2） | 只发 v3（破坏现有 Python 示例） |
| D5 | 批次 1 是否先独立合并 | 是（纯 bug 修复，无需等设计裁决） | 与 v3 一起合 |

## 附录 A：本设计引用的代码位置

> 行号为**设计期**（HEAD `b821c9f`）快照；实施后代码已变动（`PROTO_VER` 拆为 `SUPPORTED_VERSIONS`/`CURRENT_VERSION`，ConfigPush 组装抽为 `build_config_push`）。实施后请按函数名定位。

| 位置 | 内容 |
|---|---|
| `src/plugin/builtin/sidecar_client/mod.rs:49` | `PROTO_VER` 常量 |
| `src/plugin/builtin/sidecar_client/mod.rs:62-68` | `Hello` struct（`:66-67` 为 `ver` 缺省语义） |
| `src/plugin/builtin/sidecar_client/mod.rs:188-192` | `parse_hb_timeout`（唯一 `app_config` 消费者） |
| `src/plugin/builtin/sidecar_client/mod.rs:272-276` | `HelloAck` 组装（已下发 `heartbeat_secs`） |
| `src/plugin/builtin/sidecar_client/mod.rs:286` | `expand_devices`（v2/v3 共用） |
| `src/plugin/builtin/sidecar_client/mod.rs:299-327` | `push_devices` 组装（`bind` 丢弃处） |
| `src/plugin/builtin/sidecar_client/mod.rs:328-332` | ConfigPush 顶层载荷 |
| `src/plugin/builtin/sidecar_client/mod.rs:385-446` | DataUp 校验闸（不动） |
| `src/plugin/builtin/sidecar_client/mod.rs:606-617` | 版本精确匹配（改集合匹配） |
| `src/plugin/builtin/sidecar_client/mod.rs:733-736` | `hello_ver_defaults_to_proto` 测试 |
| `src/model/expand.rs:22-31` | `ExpandedDevice`（含 `bind`） |
| `src/model/expand.rs:60` | `bind` 唯一现有消费者（`slave_id`） |
| `src/model/expand.rs:94-99` | `expand_binding` 填入 `bind` |
| `src/types.rs:236-242` | `DeviceBinding` 定义 |
| `src/types.rs:278-284` | `app_config` / `devices` 字段 |
| `src/plugin/builtin/lua_north/config.rs:202-216` | `to_script_cfg`（裁剪原则先例） |
| `src/plugin/builtin/lua_north/contract.rs:439-454` | `resolve_password_opt`（凭证解析复用点） |
| `src/db/secrets.rs:116-128` | AEAD 加密写入 |
| `src/db/secrets.rs:170-177` | 透明解密读取 |
| `src/api/routes_inst.rs:115-121` | external 实例 token 签发 |
| `src/api/routes_inst.rs:297-308` | `redact_inst`（回显脱敏，不动） |
| `src/host.rs:89-91` | `BridgeRegistry`（slot 容量 1） |
| `src/config.rs:18` | `DEFAULT_HEARTBEAT_TIMEOUT_SECS` |
| `tests/sidecar_e2e.rs` | 现有 e2e（无版本拒连覆盖） |
| `examples/ext-plugin-python/main.py:32` | v2 示例（可连） |
| `examples/ext-plugin-opcda-go/main.go:47` | v1 示例（**连不上**） |

## 附录 B：本设计未验证的部分（显式声明）

- 未编译/运行 Go 示例（需 Windows + OPC DA Server COM 环境）。"v1 连不上"的结论来自代码静态比对（`protoVer = "1"` vs `PROTO_VER = "2"` 精确匹配），不是运行实测。
- 未实测 v3 载荷在真实外部进程中的解析（设计阶段无代码）。
- 未评估 `MAX_FRAME`（4 MiB，`sidecar_client/mod.rs:51`）在大规模点位下的余量——若单实例绑定数千设备，`app_config` + `bind` 增加的字节量需在实施时实测。
