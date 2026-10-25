# 设备模型注册表（物模型）实施计划

> **状态**：implemented（M-1~M-4 全部合并 master，2026-08-25）
> **日期**：2026-08-25
> **决策**：[2026-08-25-device-model-registry.md](../decisions/2026-08-25-device-model-registry.md)（元数据唯一来源原则 + D-M1~D-M4，执行不再议）
> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.
> **实施**：M-1 T1~T8、M-2 T1~T5 + e2e 模型驱动 + 前端绑定式、M-3 T1~T4（lua v2 + sidecar v2 + 外部示例）、M-4（`feat/device-model-m4`）T1~T5（发现引擎 + discovery REST + tags 退役 + 前端发现面板/adopt/apitest + 文档收口）均全绿合并。TDD 每任务 red→green→commit。
> **代码**：`src/model/` 九模块（types/addr/effective/validate/store/migrate/cache/expand/discovery）+ `src/api/routes_model.rs`/`routes_devreg.rs`/`routes_discovery.rs` + `routes_tag.rs`（M-4 改造：effective 视图 + devreg.overrides 三态写入 + legacy-export）+ `POST /api/v1/migrate/inline-devices`；`db/tag_registry.rs` 已随 M-4 T3 删除（tags 表退役，glob_match 迁 `model/discovery.rs`）。
> **测试**：`cargo test --lib model::` 107 单测 + `tests/model_registry_e2e.rs` 15 e2e（401/CRUD/409/迁移幂等/**发现列表过滤+clear+adopt 建模→清队→再流过闸**）+ `tests/tag_effective_e2e.rs` 7 e2e（effective 视图/三态/**覆盖不被模板刷新冲掉**/glob/delete/legacy 导出）+ 全量 35 套件绿 + 前端 154 测绿。
> **遗留**：① M-4 T1 吞吐门槛 7 轮交错 A/B 实测劣化 2.9%（<5% 达标；246 万点/秒 ≈ 4.9× 于 50 万目标）；bench 场景改模型化形态（未建模脏点由 discovery 单测覆盖）。② 旧 tags 表遗留数据经 `GET /api/v1/tags/legacy-export` 只读导出（保留一个版本后可物理删表）。③ `TagSnap.unit`（lua 模型快照）`#[expect(dead_code)]` 预留，待 ctx:tags API 增强消费。④ M-1 两项裁量项已随 M-2 落地（`/api/v1/devices` 系切 devreg 派生；迁移端点含实例改写）。

**Goal:** 落地设备模型（devmodels）+ 全局设备注册表（devreg）+ 发现队列（discoveries），使设备/标签元数据有唯一出生地与唯一管理面；三类采集实现（内置插件/lua/sidecar）全部改为消费模型；Phase 1a 的 tags 表退役。**对业务流/数据流是夯实不是破坏：数据面（pipeline/rtdata/tag_key/吞吐资产）一行不动。**

**Architecture:** 新增 `src/model/` 域（高内聚：类型/存储/地址解析/属性合并/校验/迁移六模块，依赖方向单向向下）；采集器实例从"自带点表"改为"绑定全局设备"；lua/sidecar 宿主层加模型校验闸；出口登记转型发现引擎。分层：

```
┌─ API 层  routes_model.rs / routes_devreg.rs / routes_discovery.rs（薄委托）
├─ 域层    src/model/（本计划核心）
│    types.rs      DeviceModel/TagTemplate/DeviceEntry/TagOverride + serde
│    addr.rs       参数化地址 resolve 纯函数（"{base}+2" → 绝对地址串）
│    effective.rs  effective props 合并纯函数（模板默认 ∪ 覆盖，三态）
│    validate.rs   引用完整性（模型存在/参数齐/覆盖 tag 在模型中/绑定合法）
│    store.rs      lux CRUD（devmodels / devreg / discoveries 三表）
│    migrate.rs    旧内联配置一次性迁移
├─ 接线层  pipeline（发现引擎）· lua_core/api（校验闸+模型感知）· sidecar v2 · 内置插件 resolve 前置
└─ 数据面  pipeline/rtdata/tag_key/dev2inst/tsink（冻结不动）
```

**Tech Stack:** lux `EmbeddedClient` Hash CRUD（同 `dev_registry` 模式）、axum 0.8 JSON 端点（同 `routes_device` 风格：POST /set、POST /delete）、既有 JWT 中间件零新增鉴权面、KSUB 失效复用（Phase 1a 模式）。

**执行前置：** 跨 ≥5 文件 ≥6 commit，**必须独立 worktree**：`git worktree add ../rs-iot-devmodel -b feat/device-model`（与主仓同级，`../lux` path 依赖不变）。**每期（M-1~M-4）独立分支/独立合并**，期内部按 Task 小步 commit。

**约定：** 每 Task 先写失败测试再实现（TDD）；Conventional Commits + 中文；单测 `cargo test -p rs-iot --lib model::`，e2e `cargo test -p rs-iot --test model_registry_e2e` 等；文档改动后 `python scripts/check_docs.py`；本机 cargo `-j 2`；改 pipeline 相关跑 `throughput_e2e` + bench 门槛（M-4 T1 涉及）。

---

## 一、已定取舍（补充决策，与决策文档 D-M1~D-M4 配套）

1. **表命名**：`devmodels`（模型）、`devreg`（设备注册表，避开已废弃死表 `devices` 的语义混淆——D-CLN-1）、`discoveries`（发现队列）。Phase 1a 的 `tags` 表在 M-4 退役删除。
2. **地址参数化语法（最小集）**：`{name}` 占位符替换 + `+N` 数字后缀偏移。例：`holding:{base}+2:f32` + `{base:100}` → `holding:102:f32`；`DB{db}.DBD{base}+4`；`ns=2;s={p}ua`。resolve 是**纯字符串→字符串**函数（`model/addr.rs`），各插件把结果喂给**现有**地址解析器（如 modbus `address.rs::PointSpec`）——插件地址解析器零改动，模型层不感知协议，插件不感知模型。
3. **属性三态**：`TagOverride` 全 Option 字段——`None`=继承模板，`Some`=覆盖。模板 `TagTemplate.props: TagProps` 是完整默认值。Phase 1a `apply_set` 的全量覆盖语义废弃。
4. **多宿主合法化**：同一 devreg 设备可被多个采集器实例绑定（N:M 绑定表），dev2inst 运行时记账保留不变。
5. **selfMonitor 模型化**：系统自监控设备用**内置隐式模型**（代码注册进 devmodels，model_id=`_sys_rsiot`），验证"运行时设备也走模型"的完整性。
6. **迁移策略**：一次性命令 `POST /api/v1/migrate/inline-devices`（幂等，可重复执行）：扫描 lux `instances` 内联 children → 生成隐式模型（`_inline_<inst>_<dev>`，地址原样、props 初值带入模板默认）→ 建 devreg 条目 → 实例配置改写为 DeviceBinding。**不提供运行时双结构兼容**。
7. **lua API 集合（v2）**：`ctx:devices()` / `ctx:tags(devid)` / `ctx:param(devid,name)` 新增（模型感知）；`ctx:set_value(devid,tag,v)` 与 `collect()` 返回值过闸。L1-L6 既有 API（fire_event/secret/http 等）不动。
8. **sidecar 协议 v2**：HELLO 携带版本，v2 的 ConfigPush 载荷升级为 `{inst_id, v:2, devices:[{devid, model, tags:[{name,type,addr(已解析),unit,rw}]}]}`；DataUp 过闸。v1 拒连（外部示例同步重写，`examples/ext-plugin-*`）。

## 二、数据模型

```rust
// ── src/model/types.rs ──────────────────────────────────────────
/// 设备模型（物模型；lux Hash `devmodels`，field=model_id）。
pub struct DeviceModel {
    pub model_id: String,               // 全局唯一，如 "vx_dtz341"
    pub vendor: Option<String>,         // 厂商
    pub model_no: Option<String>,       // 型号
    pub category: Option<String>,       // 类别（电表/PLC/变频器…）
    pub desc: Option<String>,
    pub params: Vec<String>,            // 声明的实例化参数名，如 ["base"]
    pub tags: Vec<TagTemplate>,         // 标签模板（name 模型内唯一）
}

/// 标签模板（乐高最小对象）。
pub struct TagTemplate {
    pub name: String,                   // 如 "ua"
    pub data_type: String,              // f32/f64/i16/u16/bool/string…
    pub desc: Option<String>,
    pub unit: Option<String>,
    pub addr: Option<String>,           // 参数化地址（lua/纯软设备模型可 None）
    pub rw: TagRw,                      // R | Rw
    pub props: TagProps,                // 型号级默认（复用 Phase 1a TagProps）
}

/// 全局设备实体（lux Hash `devreg`，field=devid）。
pub struct DeviceEntry {
    pub devid: String,                  // 物理设备全局唯一 id（= rtdata/tsink 的 devid，不变）
    pub model: String,                  // model_id 引用
    pub params: BTreeMap<String,String>,// {"base":"100"}
    pub overrides: BTreeMap<String, TagOverride>,  // per-tag 覆盖（key=tag name）
    pub desc: Option<String>,
}

/// 实例级标签覆盖（三态：None=继承模板）。
pub struct TagOverride {
    pub history: Option<bool>,
    pub deadband: Option<f64>,
    pub desc: Option<String>,
    pub unit: Option<String>,
}

/// 采集器实例的设备绑定（InstConfig.devices 的新形态）。
pub struct DeviceBinding {
    pub devid: String,                  // 引用 devreg
    pub bind: BTreeMap<String,String>,  // 协议绑定参数，如 {"slave_id":"2"}
}

/// 发现队列条目（lux Hash `discoveries`，field=tag_key）。
pub struct Discovery {
    pub devid: String, pub tag: String,
    pub first_ts: i64, pub last_ts: i64, pub count: u64,
    pub sample_value: String,           // 最近样本值（建模参考）
}
```

lux 表全景（改造后）：`devmodels` / `devreg` / `discoveries` / `instances`（结构升级）/ `dev2inst`（不变）/ `rtdata:<devid>`（不变）；`tags` 表 M-4 删除。

## 三、现状代码处置清单（file:line 映射——演进不是推倒）

| 现状 | 位置 | 处置 | 期 |
|---|---|---|---|
| `tag_key()` / `TagProps` | `src/types.rs` | **原样保留**（TagProps 转任模板默认结构） | — |
| `ChildSpec{label,value,type,props}` | `src/types.rs:194-206` | M-2 废弃（迁移命令消费后删）；`DeviceSpec.slave_id` → `DeviceBinding.bind` | M-2 |
| `InstConfig.devices: Vec<DeviceSpec>` | `src/types.rs:208-231` | 改 `Vec<DeviceBinding>`；其余字段不动 | M-2 |
| 各插件 children 解析（modbus `address.rs` PointSpec / opcua NodeId / s7 kind / eip tag） | `src/plugin/builtin/*/` | **地址解析器零改动**；仅采集装配前加 `model::addr::resolve` 前置 + 点表来源改为模型 | M-2 |
| `note_once` 出口登记 + once-set | `src/db/tag_registry.rs` | 转型发现引擎：比对模型缓存，未建模 → discoveries（节流计数 60s/批）；once-set 机制换壳保留 | M-4 |
| `declared()` 预载（lib.rs/startinst 刷新） | `src/lib.rs` / `routes_sys.rs` | 转型 effective 物化缓存装载（数据源 模型∪覆盖；KSUB `devmodels:*`/`devreg:*` 失效或写路径失效） | M-1 装、M-4 接线 |
| tags 表 CRUD + `glob_match` + `apply_set` | `src/db/tag_registry.rs` | CRUD 退役；`glob_match` **复用**（批量建模/清理）；`apply_set` 删（三态语义替代） | M-4 |
| `routes_tag.rs` 四端点 | `src/api/routes_tag.rs` | GET /tags 改读 effective 视图；set/batch/delete 改写 devreg.overrides | M-4 |
| lua `set_value`/`collect` | `src/plugin/lua_core/api/mod.rs:468-526` | 宿主持模型点表快照（Arc），过闸；新增 devices()/tags()/param() | M-3 |
| sidecar ConfigPush/DataUp | `src/plugin/builtin/sidecar_client/mod.rs:69-79,280-290` | 协议 v2 + 校验闸 | M-3 |
| selfMonitor 硬编码 17+3 标签 | `src/plugin/builtin/self_monitor/mod.rs:577-611` | 注册内置隐式模型 `_sys_rsiot`，设备入 devreg | M-2 |
| `tag_registry_e2e.rs` 六场景 | `tests/` | M-4 改写（语义平移：不覆盖 → 覆盖不被模板刷新冲掉） | M-4 |
| 死表 `devices` WARN | `src/lib.rs:305-313` | 改为提示运行迁移命令（若检测到内联结构） | M-2 |

## 四、API 规格（JWT 保护，风格对齐 device/tag 系）

| 端点 | 方法 | 说明 |
|---|---|---|
| `/api/v1/devmodels` | GET | 模型列表（`?model_id=` 单查） |
| `/api/v1/devmodel/set` · `/delete` | POST | 模型 CRUD（删除校验：无 devreg 引用，有则 409 列引用） |
| `/api/v1/devices` | GET | 设备注册表（`?devid=` / `?model=` 过滤，含 effective 摘要） |
| `/api/v1/device/set` · `/delete` | POST | 设备 CRUD（validate：模型存在/参数齐/覆盖 tag 在模型中；删除级联：解绑实例引用+dev2inst+rtdata 清理对齐现有 cascade） |
| `/api/v1/discoveries` | GET | 发现队列（count 降序；`?dev_id=`） |
| `/api/v1/discovery/clear` · `/adopt` | POST | 清除；一键建模（body 指定目标 model_id + tag 名/类型/地址建议 → 写入模型 tags → 队列清除） |
| `/api/v1/migrate/inline-devices` | POST | 一次性迁移（幂等；返回 {converted_models, converted_devices, converted_instances}） |
| `/api/v1/tags`（改造） | GET | effective 视图：`{tag_key: {…effective, _source: "model"/"override"}}` |
| `/api/v1/tag/set` · `/batch` · `/delete`（改造） | POST | 写 devreg.overrides（三态：传 null=恢复继承模板）；delete=清该 tag 全部覆盖 |

## 五、四期 Task 分解（TDD：每 Task 标注先行失败测试）

### M-1 模型层 + 设备注册表（纯函数域 + 存储 + 迁移；无采集器接线，独立可合并）

- **T1 `model/addr.rs` resolve 纯函数**（单测最密集，先行失败测试）：占位符替换、`+N` 偏移（仅数字替换结果生效）、缺参 Err、未知参数 Err、非数字 base 配 `+N` Err、无占位符原样返回、多占位符混合。*Files: Create `src/model/addr.rs` + `src/model/mod.rs`；Modify `src/lib.rs`（mod model）。*
- **T2 `model/effective.rs` 合并纯函数**：模板默认 ∪ 覆盖三态（None 继承/Some 覆盖/deadband 覆盖为 None 表示"显式关死区"与"未设"的区分——用 Option<Option<f64>> 或独立哨兵，测试锁定）；设备不存在模型 tag 的覆盖被 validate 拒于写入前。
- **T3 `model/types.rs` + serde**：model_id/devid 非空与字符集校验、tag 名模型内唯一（重复 Err）、params 与模板 addr 引用的参数一致性（addr 用了未声明参数 → Err，validate 拒）、旧 JSON 往返。
- **T4 `model/store.rs` devmodels CRUD**：HSET/HGET/HGETALL/HDEL + 删除引用校验。e2e 先行：`tests/model_registry_e2e.rs` 骨架（401/建查删/重名 409）。
- **T5 `model/store.rs` devreg CRUD + `validate.rs`**：模型引用存在、params 齐套、overrides 的 tag ∈ 模型 tags、bind 合法性预留。e2e：非法引用 4xx 矩阵。
- **T6 REST `routes_model.rs`/`routes_devreg.rs`**：上表端点薄委托 + mod.rs 注册 + 401 e2e。
- **T7 `migrate.rs` 一次性迁移**：纯函数 `convert(inst: InstConfig) -> (Vec<DeviceModel>, Vec<DeviceEntry>, Vec<DeviceBinding>)`（golden 测试：旧结构样例 → 断言产物）+ REST 端点（幂等：已迁移实例跳过）+ e2e（建旧式实例 → 迁移 → 新结构生效可采集前验证）。
- **T8 effective 物化缓存 + 失效**：`model/cache.rs`（`OnceLock<RwLock<Arc<HashMap<tag_key, TagProps>>>>`，模式同 Phase 1a declared）；写路径主动失效 + 启动装载；单测（写后读新值）。
- **M-1 合并门禁**：全量测试绿 + check_docs + CLAUDE.md 模块表登记 `src/model/`。

### M-2 内置插件接线（InstConfig 结构升级 + 迁移落地 + selfMonitor 模型化）

- **T1 `InstConfig.devices: Vec<DeviceBinding>`**：serde 新结构；`startinst` 校验绑定设备存在于 devreg（404 带指引"先迁移或先建设备"）；迁移命令在此期成为必经路径（README/CLAUDE.md 升级说明）。
- **T2 modbus**：装配前 `resolve` 前置（模板 addr × devreg params → 绝对地址串 → 既有 PointSpec 解析）；`slave_id` 从 `DeviceSpec` 迁至 `DeviceBinding.bind`；改造 `tests/modbus_multi_slave_e2e.rs` 为模型驱动（先行写失败 e2e）。
- **T3 s7**：同模式（`DB{db}.DBD{base}` 场景）；改造 `tests/s7_e2e.rs`。
- **T4 opcua/eip/dataSim**：占位符简单场景（`{p}` 前缀类）；dataSim 作为"无地址纯软模型"验证 addr=None 路径。
- **T5 selfMonitor 内置隐式模型**：启动时幂等注册 `_sys_rsiot` 模型 + devreg 设备；改造其单测。
- **M-2 合并门禁**：全部协议 e2e 绿（mock 链路）+ 迁移 e2e 绿 + `examples/` 相关更新。

### M-3 lua v2 + sidecar v2

- **T1 lua 模型感知 API**：宿主装配模型点表快照（Arc）；`ctx:devices()/tags(devid)/param(devid,name)` + 单测（lua 脚本内调用断言）。
- **T2 lua 校验闸**：`set_value`/`collect` 过闸（未建模 → lua error 可捕获 + 宿主 warn + 计数）；改造 `tests/lua_*_e2e.rs` 受影响场景 + 新增"未建模被拒"e2e（先行）。
- **T3 sidecar 协议 v2**：HELLO 版本协商（v1 拒连带升级提示）；ConfigPush v2 载荷；DataUp 闸；`docs/reference/external-plugin-protocol.md` 升 v2；改造 `tests/sidecar_e2e.rs`。
- **T4 外部插件示例重写**：`examples/ext-plugin-*` 同步 v2 + 快速验证脚本。

### M-4 发现引擎 + Tag Registry 退役 + 前端

- **T1 发现引擎**：`note_once` 改造（比对 effective/模型缓存；未建模 → discoveries，60s 节流计数 + 样本）；**pipeline 吞吐门槛复验**（A/B bench，预期与 Phase 1a 同级——仍是 O(1) 缓存查询 + 首见罕见路径）。
- **T2 discovery REST**：列表/清除/adopt 一键建模 + e2e（模拟未建模数据 → 队列 → adopt → 模型更新 → 再流数据过闸）。
- **T3 tags 退役**：`routes_tag.rs` 改造（effective 视图/overrides 写入）；删 `db/tag_registry.rs` tags CRUD 与 `tags` 表（升级说明：数据丢弃，人工属性需先迁移——提供只读导出端点过渡一个版本）；`tag_registry_e2e.rs` 改写为 devreg 语义。
- **T4 前端**：模型管理页（模板编辑/参数声明/标签表格）+ 设备注册表页（列表/建设备选模型/覆盖编辑）+ 实例创建向导改造（选模型→选/建设备→通讯参数）+ apitest 按钮；vitest。
- **T5 文档总收口**：CLAUDE.md（模块表/数据表/验证规则段）、rest-api.md 全节、guides（lua-script-guide 的模型 API 节、external-plugin-dev-guide v2）、README 架构图、master plan 状态回填。

## 六、测试策略与验收门禁

- **纯函数层**（addr/effective/validate/migrate.convert）：密集单测，TDD 主战场（预计 40+ 用例）。
- **e2e**：`tests/model_registry_e2e.rs`（新建，M-1 起累加）+ 既有 modbus/s7/opcua/lua/sidecar 系列改造（每期绿了才合并）。
- **迁移正确性**：golden 样例测试 + 幂等重跑断言。
- **吞吐门槛**：M-4 T1 后 `bench_pipeline_e2e` A/B 对照（发现引擎 vs Phase 1a 出口登记，劣化应 <5%；once-set 语义不变）。
- **每期合并门禁**：全量 `cargo test -p rs-iot` + `check_docs.py` + fmt/clippy（CI 四门禁对齐）。

## 七、风险登记

| 风险 | 缓解 |
|---|---|
| InstConfig 结构变更是 breaking（前端/lua 脚本/外部插件/field-kit） | 迁移命令与代码同版本发布；前端 M-4 T4 同步；field-kit 配置随 M-2 更新；升级说明随每期 release note |
| dev2inst 与绑定表双记账漂移 | dev2inst 仍是运行时事实来源（出数登记），绑定表是配置意图——两者口径差异正是"配置声明 ∪ 运行时"既有哲学，文档写明 |
| 地址占位符与 modbus gap 合并（max_regs/相邻合并）交互 | resolve 在装配期一次性完成，后续逻辑看到的就是绝对地址串——零交互；e2e 含批量读场景 |
| selfMonitor runtime-only 重启路径（mgr 快照） | 快照结构随 InstConfig 序列化自动升级；`startinst` 回退路径 e2e 保留 |
| tags 表退役丢人工属性 | M-4 前提供只读导出；迁移命令在 M-1/T7 已把 props 初值带入模板默认——人工后改的部分需对照导出手动恢复（release note 明示） |
| 发现队列被脏数据灌爆 | discoveries 上限（如 5000，LRU 淘汰）+ count 聚合（同 key 不新增条目） |

## 八、新会话执行指引（重要——本文档即执行蓝图）

1. **先读**：CLAUDE.md → `docs/decisions/2026-08-25-device-model-registry.md`（原则与 D-M1~M4）→ 本文档全文 → `docs/designs/2026-08-24-history-storage-master-plan.md`（§一三层开关与 effective() 的下游关系）。
2. **执行方式**：Use superpowers:executing-plans，从 **M-1 T1（`model/addr.rs` 纯函数 TDD）** 开始，严格按 Task 顺序；每 Task：先写失败测试 → 实现 → 绿 → commit；每期完成跑合并门禁后合并 master、清理 worktree、开下一期 worktree（`feat/device-model-m1`…`-m4` 或单 worktree 分期合并，执行者按 CLAUDE.md worktree 规则裁量）。
3. **上下文恢复**：本文件 §三 处置清单含全部现状 file:line 映射；Phase 1a 的实现细节见 `docs/designs/2026-08-24-tag-registry-plan.md` 实施块。
4. **红线**：不碰 pipeline 核心/rtdata 写路径/tag_key；不引入运行时双结构兼容；每期必须独立可合并（全量测试绿）。
