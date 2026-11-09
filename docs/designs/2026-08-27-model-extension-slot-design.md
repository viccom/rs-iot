# 设备模型域扩展性设计（DeviceModel / TagTemplate 开放扩展槽）

> **状态**：implemented（扩展槽 + 键契约于本文档定稿日落地；读透出面同批对齐）
> **日期**：2026-08-27
> **实施**：2026-08-27 `3c59989`（扩展槽 + `x_` 键契约 + MCP 读工具/前端展示）
> **代码**：`src/model/types.rs`（`DeviceModel.extra` / `TagTemplate.extra`）、`src/model/validate.rs`（键契约校验）、`src/model/store.rs`（写入口把关）
> **前置**：[2026-08-25-device-model-registry.md](../decisions/2026-08-25-device-model-registry.md)（物模型注册表，D-M1/D-M2）、[2026-08-24-tag-registry-plan.md](2026-08-24-tag-registry-plan.md)（TagProps 起源）
> **关联**：[2026-08-21-alarm-minimal-loop-design.md](2026-08-21-alarm-minimal-loop-design.md)（报警边界，本文 §5.1 重述）、[2026-08-24-history-storage-master-plan.md](2026-08-24-history-storage-master-plan.md)（存储梯度，本文 §5.2 引用）
> **一句话定位**：把"开放 schema"放在**数据面**（模型可携带任意受治理的扩展元数据），rs-iot 自身保持**控制面最小**（采集、点表、前置加工归宿主；判定、报警、大规模存储、应用逻辑归外置应用）。

---

## 0. 文档定位

本文回答一个问题：**为什么以及如何让 DeviceModel / TagTemplate 支持字段扩展，同时不破坏现有逻辑与代码**。它是三方面讨论的定稿：

1. 结构层面——扩展槽的 serde 语义、wire 行为、前向兼容证明；
2. 治理层面——扩展键的命名契约与执行点；
3. 定位层面——扩展性是**设计预案**而非当前功能义务；是否有内容、由谁消费，随实际项目与场景决定。

本文所有结构描述与 `src/model/` 当前代码逐字段契合；行号以定稿日工作区为准。**规范性小节**（MUST/NOT 语式）为 §3.1、§4；其余为背景与路线。

---

## 1. 背景与动机

### 1.1 为什么模型层需要可扩展

`DeviceModel` / `TagTemplate` 是全系统的元数据单一事实源（D-M2：型号即模板；宪法原则：元数据必须有唯一出生地与管理入口）。但围绕模型的**消费者是开放集合**——协议插件（Modbus/OPC UA/S7/EIP/…）、部署版本（滚动升级、备份恢复）、外置应用（报警、资产、SCADA 层）、第三方集成。封闭的结构体定义服务不了开放的消费面。四个已发生的具体压力：

| # | 压力 | 代码证据 | 后果（无扩展槽时） |
|---|---|---|---|
| 1 | **协议私有元数据无结构化去处** | Modbus 的 `byte_order`/位号挤在 addr 字符串内（`holding:0:f32:ABCD:3`，`modbus_client/address.rs:111-168`）；S7 的 `string[N]` 塞在 `data_type` 串内（`s7_client/mod.rs:38-45`） | 类型/字序/位号混在字符串里，前端只能整框文本处理，校验只能运行时做——addr 串成了"不够体面的扩展槽" |
| 2 | **版本偏差下的静默数据丢失** | 模型读写为「反序列化成结构体 → 整体序列化覆盖回写」（`store.rs:68-74` HSET 整串），serde 默认丢弃未知字段 | 滚动升级期间旧版本实例读-改-写新版本数据时，**无声抹掉新字段**——对采集系统这是最难排查的故障类别 |
| 3 | **模型是跨边界流转的资产** | `migrate.rs`（旧实例配置迁入）、`discovery` adopt（发现结果建模型）、sidecar ConfigPush 下发、REST 供前端与第三方读写 | 每个边界两侧代码版本都可能不一致，需要"信封容差"：收信方不认识的内容原样保留而非销毁 |
| 4 | **props 已示范半条路线** | `TagProps`（`src/types.rs:131-148`）承载 history/deadband/owner_inst 等跨插件业务属性 | 证明"属性层集中演进"有效，但只覆盖协议无关的业务属性；协议私有、来源未知、尚未转正的键没有去处 |

### 1.2 定位哲学（能力可选、场景启用、应用外置）

- **设计上可扩展 ≠ 功能上扩展了**。扩展槽落地后是零行为、零消费者的纯信封；它的全部即时影响是"跨版本读写不丢未知字段"。
- **能力按场景裁剪是本项目既成惯例**：编译面 default 只编 `[lua, lua-http, opcua, modbus]`，s7/eip/transform 显式启用（[2026-08-26-default-compile-surface.md](../decisions/2026-08-26-default-compile-surface.md)）；扩展槽把同一惯例延伸到 schema 层。
- **报警一定是外置应用**；rs-iot 只保留最小事件通道（§5.1）。对纯边缘采集场景，报警类扩展键可以永远为空，无任何代价。
- **存储模块定位小规模（10w tag 级以内）自持**，规模大了宿主退出归档、退回"边缘采集 + 前置数据加工"本位（§5.2）。

支持扩展的目的只有一个：**让模型对未来未知的消费者保持可用**——更好的灵活性与适配性，而非增加宿主功能。

---

## 2. 结构现状（与代码契合）

### 2.1 三层数据结构

```
DeviceModel（模板，型号即模板）             lux Hash `devmodels`，field = model_id
 ├── model_id / vendor / model_no / category / desc
 ├── params: Vec<String>                    声明的实例化参数名
 ├── tags: Vec<TagTemplate>                 标签模板集合（name 模型内唯一）
 └── extra: Map<String, Value>             ★ 开放扩展槽（本文 §3）
         ↓ 实例化（DeviceEntry.params 提供 {param} 实参）
DeviceEntry（实例，全局设备实体）            lux Hash `devreg`，field = devid
 ├── devid / model（引用）/ params / desc
 └── overrides: Map<tag名, TagOverride>     三态覆盖（缺省=继承模板）
         ↓ 合并（effective = 模板默认 ∪ 设备覆盖）
生效视图（effective_all）                   进程级缓存 model::cache，key = tag_key(devid, tag)
```

`TagTemplate` 字段全集（`src/model/types.rs:40-60`）：

| 字段 | 类型 | 消费者 | 备注 |
|---|---|---|---|
| `name` | `String` | 全链路 | 设备内唯一；Sample/rtdata/tag registry 的 key 源 |
| `data_type` | `String` | 各插件私有解释 | S7 强制（位宽唯一来源，`s7_client/mod.rs:42-45`）；Modbus 忽略（类型在 addr 串内）；OPC UA 读方向忽略（类型由 server Variant 决定） |
| `desc` / `unit` | `Option<String>` | effective 垫底进 TagProps；UI | unit 对齐 OPC UA EngineeringUnits |
| `addr` | `Option<String>` | 展开期 resolve → 插件解析器 | 参数化地址，`{name}` 占位符 + `+N` 偏移（`addr.rs:50-93`）；lua/纯软模型可 None |
| `rw` | `TagRw`（默认 `R`） | sidecar ConfigPush（唯一运行时消费） | 最小权限：缺省只读；内置插件写拦截按协议语义（如 Modbus discrete/input 拒写）而非此字段 |
| `props` | `TagProps` | effective → history 引擎 | 型号级默认属性（history/deadband/desc/unit/owner_inst） |
| `extra` | `Map<String, Value>` | 外置应用（宿主不解释） | ★ 本文 §3 |

### 2.2 职责两轴：协议私有 vs 协议无关

TagTemplate 的字段按"解释权归属"分两轴：

- **协议私有轴**（`addr`/`data_type`/`rw`）：宿主只管公共部分（addr 的 `{param}+N` 参数化在展开期一次性 resolve），具体语法下放给各插件私有解析器。理由：三种协议的地址要素完全不同（Modbus 功能码+寄存器+字序 / OPC UA NodeId / S7 区域+DB块+偏移），统一结构体会把扩展点放错位置——宿主被迫理解每种协议，新协议要改核心 schema。现行设计下**新协议 = 新插件 + 新解析器，模型字段零改动**。
- **协议无关轴**（`props` 及 `name`）：跨插件共享的业务语义，宿主统一消费（history 白名单/死区已闭环）。

`extra` 补的是协议私有轴之外的第三象限：**协议私有或业务私有、来源开放、宿主不解释、尚未转正**的元数据。

### 2.3 公共展开链路（extra 在链路中的位置）

```
InstConfig.devices: Vec<DeviceBinding>
  → expand_to_specs（插件 run() 启动期；试调 addr::resolve，坏配置拒启动）
    → DeviceSpec{ label=devid, slave_id←bind } / ChildSpec{ label=tag名, value=绝对地址, type=data_type }
      → 各协议插件私有解析（parse_point / parse::<NodeId>() / S7 kind）
```

**规范性事实**：`extra` **不进** `ChildSpec`、不进 effective 产物（`TagProps` 无扩展槽）、不下发 sidecar ConfigPush、不进 lua `TagSnap`（`lua_core/api/mod.rs:39-60`）。扩展槽只存在于存储与 API 面。这是刻意的：宿主对 extra 是**不透明信封**，任何运行时消费都需显式接线并先过键契约（§4）。

---

## 3. 扩展槽设计（extra）

### 3.1 语义定义（规范性）

```rust
/// 开放扩展槽：捕获 wire 上未匹配已知字段的键，读-改-写原样保留
/// （前向兼容——旧代码不丢新版本写入的扩展字段）。
/// 空时不序列化，不用扩展槽的 wire 逐字节不变。
#[serde(flatten, skip_serializing_if = "serde_json::Map::is_empty")]
pub extra: serde_json::Map<String, serde_json::Value>,
```

1. **捕获**：反序列化时，所有未匹配已知字段的顶层键落入 `extra`（serde flatten），不再被静默丢弃。
2. **平铺**：序列化时 extra 的键平铺回顶层（不产生 `"extra": {...}` 嵌套层）；**空槽 MUST 不产生任何 wire 输出**（`skip_serializing_if`）——存量数据、旧客户端、既有测试逐字节不变。
3. **保留**：读-改-写循环（get → 修改已知字段 → set）原样保留 extra 内容。
4. **域**：模型级（`DeviceModel.extra`）与 tag 级（`TagTemplate.extra`）各一个，互不继承、互不合并。
5. **先例**：`db::stat::InstanceStat.extra`（`src/db/stat.rs:25-26`）为库内同款 flatten 先例。

### 3.2 wire 形态实例

**A. 不用扩展槽（与过去逐字节一致）：**

```json
{ "model_id": "vx_dtz341", "vendor": "威胜", "params": ["base"],
  "tags": [ { "name": "ua", "data_type": "f32", "addr": "holding:{base}:f32", "rw": "r" } ] }
```

**B. 模型级扩展（外置应用附着的资产元数据）：**

```json
{ "model_id": "vx_dtz341", "vendor": "威胜", "params": ["base"],
  "x_asset_category": "动力配电",
  "x_geo": { "site": "1号配电室", "row": 3 },
  "tags": [ ... ] }
```

**C. tag 级扩展（协议私有/策略元数据）：**

```json
{ "tags": [
    { "name": "ua", "data_type": "f32", "addr": "holding:0:f32",
      "x_scale": { "k": 0.1, "b": 0 }, "x_alarm_high": 242.0 },
    { "name": "recipe", "data_type": "string[64]", "addr": "DB10.DBB0",
      "x_s7_max_len": 64 } ] }
```

`x_alarm_high` 示例强调：宿主**不读它、不判定它**——外置报警应用经 REST/MCP 读模型后自行消费（§5.1）。

**D. 前向兼容（版本偏差场景）：** v2 写入 B → v1 旧二进制读入（`x_asset_category` 落 extra）→ v1 改 `vendor` 回写 → 扩展键原样保留。落地前的行为是 `Null`（丢失）——由红测 `device_model_unknown_fields_roundtrip_preserved` 实证。

### 3.3 Rust 消费方式（外置应用/未来接线示例）

```rust
// 读：extra 是普通 Map
if let Some(scale) = tag.extra.get("x_scale").and_then(|v| v.as_object()) { /* ... */ }
// 写：普通 Map 操作；空时不产生 wire 噪音
model.extra.insert("x_asset_category".into(), serde_json::json!("动力配电"));
```

### 3.4 宿主不解释原则

宿主核心（pipeline、插件装配、effective、history）**MUST NOT** 读取任何 extra 键产生行为。extra 是**透传层**，不是配置层——宿主要据其行为的语义必须走类型化字段（毕业机制 §4.4）。此原则保证：扩展键的数量与内容不影响宿主任何行为路径，不扩展时的性能与安全面与历史版本一致。

### 3.5 测试基线（已落地，`src/model/types.rs`）

| 测试 | 锁定性质 |
|---|---|
| `device_model_unknown_fields_roundtrip_preserved` | 模型级未知键读-改-写保留 |
| `tag_template_unknown_fields_roundtrip_preserved` | tag 级未知键读-改-写保留 |
| `device_model_minimal_json_and_full_roundtrip`（含 extra 内容） | 全字段往返 + flatten 平铺 |
| 同测试内 `!contains_key("extra")` 断言 | 空槽不引入 wire 新键 |
| `tests/model_registry_e2e.rs` 扩展用例（本期新增） | REST set 带 `x_` 键 → GET 回显；非 `x_` 键 400 |

### 3.6 边界与非目标

- **大资产（固件 bin/手册原文）不内嵌**：REST 无 `DefaultBodyLimit` 覆盖（axum 默认 ~2MB 体限）+ 模型整包读改写，内嵌不现实。现实路径是 extra 挂**外链**（`x_fw_url` / `x_datasheet_url`）或未来的独立资产面。中等文本（数十 KB 手册摘要）内嵌可行。
- **嵌套键不治理**：契约只约束 extra 的顶层键名；值的内部结构由消费方自定义。
- **不解决跨版本"新字段有新语义"**：信封只保证字节不丢，语义解释仍由版本对齐的消费者负责。

---

## 4. 扩展键契约（治理，规范性）

### 4.1 命名规则

extra 顶层键 **MUST** 匹配 `^x_[A-Za-z0-9_]+$`（`x_` 前缀 + 非空的字母/数字/下划线余部）。理由：

1. **与已知字段结构性不可能碰撞**：已知字段名无一以 `x_` 开头。反序列化时与已知字段同名的键会被 serde 路由进类型化字段、根本不落 extra；前缀规则使**程序化插入**碰撞键也成为非法——序列化重复键风险被结构性排除。
2. **防拼写错误持久化**：没有契约时，`"vendo"` 这类 typo 键会被捕获并永久存储回显（信封的副作用）；前缀校验使其在写入口被拒（fail-closed），错误可被发现而非无声积累。
3. **可识别、可审计**：一眼区分"宿主字段"与"扩展字段"，为毕业机制（§4.4）提供扫描依据。

### 4.2 消费者命名空间约定（惯例，非强制语法）

| 前缀示例 | 归属域 | 说明 |
|---|---|---|
| `x_alarm_*` | 外置报警应用 | 阈值/级别/迟滞等**声明式配置**，判定在外置应用（§5.1） |
| `x_clean_*` / `x_transform_*` | 数据清洗/加工策略 | 外置或脚本侧消费 |
| `x_fw_*` / `x_asset_*` / `x_geo_*` | 资产/固件/地理 | 外链 URL 或小体积元数据 |
| `x_s7_*` / `x_mb_*` / `x_opcua_*` | 协议私有 | 未来协议元数据从 addr/data_type 串中结构化的去处（按需迁移，非当前承诺） |
| `x_<vendor>_*` | 厂商域 | 厂商逻辑包绑定等信息 |

惯例不进校验（校验只查 §4.1 语法），冲突由命名空间自律与 code review 把握。

### 4.3 执行点

`validate_model`（`src/model/validate.rs:114`）新增校验：`DeviceModel.extra` 与每个 `TagTemplate.extra` 的顶层键逐一过 §4.1 规则，违规返回 `ModelError::BadExtraKey`（带 model/tag/键名明细）。**store 层两个写入口统一把关**（`store.rs:47` update / `store.rs:69` set），REST 面 `POST /api/v1/devmodel/set|update` 违规键 → **400 BAD_REQUEST**（随 ModelError 既有映射，`routes_model.rs:67,99`）。内部构造点（migrate/self_monitor/discovery adopt）不产生 extra，天然合规。

### 4.4 毕业机制（extra → 类型化字段）

某个扩展键满足以下任一条件时**应**提升为类型化字段（走既有演进路径：结构体字段 + `#[serde(default)]` + TagOverride 三态覆盖 + effective 合并 + 前端镜像 + 镜像锁测试）：

1. 宿主自身需要据其行为（违反 §3.4 的信号）；
2. ≥3 个独立消费者依赖同一键且语义趋同；
3. 需要实例级覆盖能力（现状 extra 无覆盖语义——`TagOverride` 只有 4 个固定字段，模板 extra 设备实例**改不了**，见 §7 延后项）。

先例：`TagProps.history/deadband` 即"毕业后转正"的完整示范（声明式 per-tag 属性 → 三态覆盖 → effective 缓存 → 热路径消费）。扩展槽是**孵化器，不是永久住所**。

### 4.5 值域约束

当前对 extra 的值**不设**大小/深度限制（校验只查键名）。理由：宿主不解释值，限制缺乏行为依据；REST 体限与 lux 值域已是物理上界。若未来出现滥用（超大值拖慢模型整包读写），在 validate 加 serialized size 上限即可，属向后兼容收紧。

---

## 5. 系统定位与边界

### 5.1 报警外置，宿主只留事件通道

rs-iot **不集成报警应用**。现状宿主内的报警最小闭环（lux `alarms` 活跃清单表、Lua `ctx:fire_event/clear_event` 上报通道、REST 查询、SSE 推送、频控护栏）性质是「**事件通道在宿主内，报警应用在宿主外**」——宿主只做存/推/清三件事，阈值/迟滞/持续时间的判定全在脚本或外置应用（[2026-08-21-alarm-minimal-loop-design.md](2026-08-21-alarm-minimal-loop-design.md) §0 有意否决宿主侧规则引擎，本文重申并延伸该边界）。

外置报警应用的工作方式：经 REST/MCP 读模型 → 消费 tag 级 `x_alarm_*` 声明式配置 → 订阅 SSE/rtdata 判定 → 判定结果可经通道回写。宿主对 `x_alarm_*` 保持不解释（§3.4），报警功能对纯边缘采集场景零成本缺席。

### 5.2 存储梯度与规模退出

历史时序存储为三级门控的嵌入式单机形态：L1 Cargo feature（编译期）→ L2 `[history]` 部署总闸 → L3 `TagProps.history` per-tag 白名单（默认零归档）。定位 **10w tag 量级以内**自持；规模再大时退出方式现成：L2 关闭、数据经北向（MQTT/lua north/transform 前置加工后）外推，rs-iot 退回「边缘数据采集 + 前置数据加工」本位。模型层不关心下游谁消费时序数据——`unit`/`desc`/props 的消费方天然可替换。

### 5.3 控制面最小原则

rs-iot 的控制面职责固定为：设备建模（模型+设备注册表）、采集装配（插件/实例）、前置加工（transform/脚本）、最小事件通道、小规模自持存储。其余一切（报警判定、大规模存储、资产应用、业务逻辑）外置。**模型扩展槽是数据面上递给外置应用的挂钩，挂钩本身不承重。**

---

## 6. 对齐面（读透出，本期落地状态）

| 面 | 状态 | 约定 |
|---|---|---|
| REST | ✅ 已通 | `GET /api/v1/devmodels` 全量 JSON 含 extra 回显；set/update 过 §4.3 键校验（违规 400）。文档：`docs/reference/rest-api.md` §4.16 |
| 前端 | ✅ 只读展示 | `devmeta.ts` 两接口声明可选 `extra?: Record<string, unknown>`；ModelForm 只读展示扩展键（不可编，防误改外置应用数据）；update 路径 deep-clone 保证 extra 往返不丢 |
| MCP | ✅ 读工具 | `rsiot_list_devmodels`（全量含 extra）+ `rsiot_list_effective_tags`（生效视图）两个只读工具，薄翻译层 |
| lua 快照 | ⛔ 不透出 | `TagSnap` 保持最小面（name/data_type/unit）；脚本消费 extra 需显式扩容快照，属场景驱动延后项 |
| sidecar ConfigPush | ⛔ 不下发 | 手工逐字段构造的对外契约，extra 不进；外部插件需要时显式协商加入 |

---

## 7. 演进路线（场景驱动，非当前承诺）

**已交付（本文定稿日）**：扩展槽（§3）+ 键契约（§4）+ 读透出面对齐（§6）。

**明确延后项**（触发条件 → 要做的事）：

| 延后项 | 触发条件 | 届时方案要点 |
|---|---|---|
| extra 实例级覆盖 | 出现"同型号设备需要差异化扩展键值"的真实场景 | `TagOverride` 加 per-key 三态槽（复制 deadband 双层 Option + `deserialize_some` 范式）+ `merge_tag_props` 合并 + effective 产物扩展 |
| 报警契约数据 | 外置报警应用实际接入 | `x_alarm_*` 键族 schema 约定（阈值/级别/迟滞），宿主仍不判定 |
| 模型↔脚本绑定 | 厂商逻辑包需求落地 | 模型 extra 挂脚本引用（如 `x_scripts: [...]`），装配期解析关联 `_lib/<org>` 共享库 |
| 协议元数据迁出字符串 | 某协议的 addr 串内嵌语义复杂到影响前端/校验 | 迁入 `x_<proto>_*` 结构化键，插件侧接线消费 |
| 大资产面 | 固件/手册分发的实际需求 | 外链 URL 约定或独立资产表/文件面 |
| 主动协议发现 | 需要扫描式建模型 | 全新能力，与本扩展槽无耦合 |

**判断准则**：每一项都等第一个真实消费者出现再动工——"设计上可扩展，内容上随场景"。

---

## 8. 与既有决策的关系

- **D-M1/D-M2（设备模型注册表）**：本文是其 schema 层的延伸——注册表确立"元数据唯一出生地"，扩展槽确立"出生地对外来元数据的收容规则"。不新增出生地（extra 仍只经 devmodels/devreg 管理面写入）。
- **alarm-minimal-loop §0**：本文 §5.1 重申"判定逻辑归脚本/外置"边界；`TagProps` 注释中"报警结构化绑定（P2 遗留）"的含义由本文改写为"**外置应用的契约数据**"，非宿主功能——不推翻原决策。
- **default-compile-surface**：扩展槽是同一"能力按场景裁剪"哲学在 schema 层的对应物。
