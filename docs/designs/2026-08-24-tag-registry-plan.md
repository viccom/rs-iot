# 标签/设备元数据统一存储（Tag Registry）实施计划

> **状态**：implemented
> **日期**：2026-08-24（于 worktree feat/tag-registry 执行完毕，T1-T7 全落地）
> **实施**：T1 tag_key+TagProps → T2 tag_registry 存储（glob/note_once/declared）→ T3 ChildSpec.props → T4 pipeline 接线+吞吐门槛 → T5 REST 四端点 → T6 e2e 六场景 → T7 文档收口
> **代码**：`src/db/tag_registry.rs`（新增）、`src/api/routes_tag.rs`（新增）、`src/types.rs`（tag_key/TagProps/ChildSpec.props）、`src/db/rtdata.rs`（改用 tag_key）、`src/pipeline.rs`（flush 出口登记）、`src/lib.rs`+`src/api/routes_sys.rs`（declared 双刷新点）、`src/api/mod.rs`（路由）
> **测试**：`tests/tag_registry_e2e.rs`（6 e2e：401/全链路登记+owner/声明式初值/不覆盖/glob 批量/删除重占位）+ 单测（tag_registry 7/routes_tag 2/types 新增 8）；全库 `--lib` 508 过；`throughput_e2e` 过
> **性能（3×3 交替 A/B，本机）**：出口登记初版逐点 String 分配实测 -13% 端到端吞吐 → 优化为 scratch 复用+借用查找后 **-8%**（TAGREG 2.14/2.34/2.26M vs BASELINE 2.41/2.28/2.67M，mean 点/秒）；**仓库门槛 ≥50万点/秒 PASS（4.5× 余量）**；本计划自设 <5% 目标未达——剩余为逐点"组合键+哈希查重"固有开销（精确登记语义下限），后续优化项登记：per-worker 本地 seen 缓存 + delete 代际失效、批量布隆预过滤
> **遗留**：KSUB `tags:*` 缓存失效（当前 API 直读 lux）、前端标签管理页（P3）、alarm 结构化绑定/scale/history_mode（P2 backlog）
> **背景**：历史时序存储（tsink 内嵌）总方案的前置地基——用户定调"先完善基础设计的缺陷：标签/设备元数据统一存储"。业界对标：Ignition per-tag `History Enabled`、OPC UA per-node 元数据、PI exception/compression 两级过滤（调研记录见会话）。现状缺口：跨插件唯一 tag 结构 `ChildSpec` 仅 3 字段（label/value/type），无任何 tag 级属性（history/deadband/unit/desc）持久化；lua/sidecar 动态 tag 无注册处；告警 key 是自由字符串。
> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 建立以 `(devid, tag)` 为主键的全局标签属性注册表（lux Hash `tags`），提供三路径登记（配置声明式 / pipeline 出口自动 / API 人工）与 REST 管理面。本表是后续历史存储归档白名单（`history=true`）、死区过滤（`deadband`）、报警结构化绑定（P2）的唯一权威源。**独立交付**：即使历史存储 feature 关闭，本表与 API 照常工作。

**Architecture:** **属性叠加层**，不夺实例配置的权（对齐 `dev2inst` 软关联哲学，决策 D-OWN-2 同款）：采集地址语义继续由各插件 `ChildSpec` 私有承载；本表只存跨插件共享的**属性**。tag 身份单一规范化源 = `types::tag_key(devid, tag)`（从 `db/rtdata.rs` field 构造逻辑抽出的共享纯函数）——rtdata field、tags field、后续 tsink series、报警 key 全部经它，保证"标签变量 id 一致性"。登记接线点 = pipeline `worker_run` flush 路径（所有 Source（内置/lua/sidecar）的唯一数据咽喉），进程级 once-set 去重，稳态零 lux 写。

**Tech Stack:** lux `EmbeddedClient` HSET/HGET/HGETALL/HDEL（模式同 `src/db/dev_registry.rs`）、axum 0.8 JSON 端点（模式同 `routes_device.rs`）、既有 JWT 中间件零新增鉴权面。

**执行前置：** 跨 ≥5 文件 ≥6 commit，**必须在独立 worktree 执行**：`git worktree add ../rs-iot-tagreg -b feat/tag-registry`（与主仓同级，`../lux` path 依赖解析不变）。

**约定：** 每 Task 末 commit（Conventional Commits + 中文）；测试先写失败再实现；单测 `cargo test -p rs-iot --lib db::tag_registry`，e2e `cargo test -p rs-iot --test tag_registry_e2e`；文档改动后 `python scripts/check_docs.py` 本地过再提交；本机 cargo 加 `-j 2`（禁页面文件）。

---

## 已定取舍（计划级决策，执行不再议）

1. **叠加层，不做权威点表迁移。** 实例配置（`InstConfig.devices[].children[]`）保持采集声明原状；tags 表只存属性。旧实例配置零迁移（无 props 字段照常工作）。理由：lua/sidecar 动态 tag 无配置可迁；dev2inst 先例证明软关联可演进。
2. **登记三路径，"只补缺不覆盖"。** 配置声明式（`ChildSpec.props` 初值）→ 出口自动（pipeline 首见 `(devid,tag)` 建占位，props 默认）→ API 人工（持久修改）。规则：任何自动路径**永不覆盖**表中已有记录（人工修改在实例重启后不丢）。对齐 dev2inst `note` 的 no-op 语义。
3. **出口登记接线 pipeline flush 路径**（非 SampleSender）。理由：pipeline 是全部 Source（含 lua/sidecar）唯一咽喉；批量上下文天然；SampleSender 是 devid 级遮蔽边界，tag 级登记放数据面消费侧更统一。代价：碰 pipeline 硬约束区 → **必须跑 `throughput_e2e` + `bench_pipeline_e2e` 验证劣化 <5%**（稳态 once-set 命中零 lux 写，预期无损）。
4. **进程级 once-set 去重放 `tag_registry` 模块内**（`OnceLock<Mutex<HashSet<String>>>`），不进 `HostContext`（管线结构零改动，低耦合）。重启后 set 空 → 首批重新 HGET 探测一次（幂等）。
5. **P1 属性集收窄**：`history`(bool, 默认 false) / `deadband`(Option<f64>, 绝对死区, PI exception 语义) / `desc` / `unit` / `owner_inst`(系统只读)。P2（alarm 结构化、scale/offset、history_mode、quality）登记遗留，本计划不做。
6. **glob batch 是批量初始化工具，不是运行时过滤规则。** `POST /api/v1/tag/batch` 对**已存在** tags 表记录按 `dev.tag` glob 批量改属性；历史存储运行时过滤只认 tags 表（无 include/exclude 配置项——V1 方案的该配置废弃）。
7. **tag 删除语义**：`tag/delete` 只删属性记录；rtdata 值不动（另有过期语义）；后续历史数据不删（同设备删除 D9 哲学）。tag 再次出数 → 重新占位登记（默认 props）。
8. **API 风格对齐 device 系**（POST /set、POST /delete，非 RESTful PUT/DELETE）：`GET /api/v1/tags`、`POST /api/v1/tag/set`、`POST /api/v1/tag/batch`、`POST /api/v1/tag/delete`。

## 数据模型

lux Hash `tags`：field = `tag_key(devid, tag)`（即 `<devid>.<tag>`，与 rtdata field 同构），value = `TagProps` JSON：

```rust
/// 标签全局属性（lux hash `tags`；叠加层——采集地址语义仍在各插件 ChildSpec）。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct TagProps {
    /// 历史归档白名单（后续 tsink 历史存储消费；默认 false = 不归档）。
    #[serde(default)]
    pub history: bool,
    /// 绝对死区：|new − last_archived| ≥ deadband 才归档（PI exception 语义；
    /// 仅数值型生效，Bool/Str 值变即写）。None = 无死区。
    #[serde(default)]
    pub deadband: Option<f64>,
    /// 描述（UI 展示）。
    #[serde(default)]
    pub desc: Option<String>,
    /// 工程单位（对齐 OPC UA EngineeringUnits）。
    #[serde(default)]
    pub unit: Option<String>,
    /// 首个生产者实例 id（系统出口登记填，API set 不修改）。
    #[serde(default)]
    pub owner_inst: String,
}
```

`ChildSpec` 扩展（声明式初值，向后兼容——缺省 `None`）：

```rust
    /// 声明式标签属性初值：出口登记首见建卡时落表（表已有记录则忽略——只补缺）。
    #[serde(default)]
    pub props: Option<TagProps>,
```

## API 规格（JWT / X-Access-Key 保护，同全站）

| 端点 | 方法 | 说明 |
|---|---|---|
| `/api/v1/tags?dev_id=&history=` | GET | 全表/按设备/按 history 过滤列表：`{ "<dev.tag>": TagProps }` |
| `/api/v1/tag/set` | POST | body `{dev_id, tag, history?, deadband?, desc?, unit?}`——单点改属性（owner_inst 忽略不修改） |
| `/api/v1/tag/batch` | POST | body `{pattern, props}`——glob（`*` 通配，匹配 `dev.tag` 全键）批量改已存在记录，返回 `{matched, updated}` |
| `/api/v1/tag/delete` | POST | body `{dev_id, tag}`——删属性记录（rtdata/历史不动） |

非目标（本计划不做）：前端标签管理页（P3 跟进）、KSUB `tags:*` 缓存失效（API 直读 lux，量大时再缓存——10 万点 HGETALL ~ms 级）、报警绑定（P2）、历史存储本身（另立 plan）。

---

## Task 1: `tag_key` 纯函数 + `TagProps`（types 层）

**Files:** `src/types.rs`（+测试）；`src/db/rtdata.rs`（改用共享函数）

先写失败测试：`tag_key` 语义 = 现 rtdata field 构造（tag 含 devid 串则原样、否则拼接；`tag_key("dev1","temp")=="dev1.temp"`、`tag_key("dev1","dev1.temp")=="dev1.temp"`）；`TagProps` serde 默认值往返。实现后 `rtdata::write_batch` 内联逻辑替换为 `tag_key(s.devid, tag)`（零行为变化——`cargo test -p rs-iot --lib db::rtdata` 全过证明）。

## Task 2: `db/tag_registry.rs` 存储模块

**Files:** Create `src/db/tag_registry.rs`；Modify `src/db/mod.rs`（+1 行）

- 纯函数（单测覆盖）：`glob_match(pattern, key)`（`*` 通配，编译为分段匹配）、`filter_history(map)`。
- lux 交互：`note_once(lux, samples: &[Sample], declared: DeclaredProps 查询函数或预载 map)`——进程 once-set（模块内 `OnceLock<Mutex<HashSet<String>>>`）去重，首见 tag：HGET 探测，缺 → 占位 HSET（declared props 或默认）；已存在 → 仅补 once-set 不写。`get/list/set/batch_set/delete`（RMW 口径同 dev_registry：管理面最终一致可接受）。声明式 props 由调用方（Task 4）从实例配置预载 `HashMap<dev, HashMap<tag, TagProps>>` 传入——存储模块不依赖插件配置结构（低耦合）。
- 单测：glob/纯函数 + `lux EmbeddedClient`（`tests/common` 模式起临时 lux——若 lib 单测基建不便则 lux 交互全部走 Task 6 e2e，纯函数留单测；执行时按实际取舍记录）。

## Task 3: `ChildSpec.props` 声明式初值

**Files:** `src/types.rs`

失败测试先行：旧 JSON（无 props）反序列化 `props==None`；带 props 往返。各插件零改动（ChildSpec 透传）。

## Task 4: pipeline 出口登记接线

**Files:** `src/pipeline.rs`（flush 路径 +1 行调用 + 声明式预载装载）

- 启动/实例启动时预载声明 props（`HostContext` 或从 lux instances 读——执行时按现有实例装载点选最小侵入位，倾向 `run.rs` 装配处一次预载 + `startinst` 增量补）。
- `flush_batch` 成功分支后调 `db::tag_registry::note_once(lux, buf, &declared)`（fail-soft：登记失败仅 warn+计数，不影响写路径）。
- **验证门槛**：`cargo test -p rs-iot --test throughput_e2e` 全过 + `cargo run --release --example bench_pipeline_e2e` 对照基线劣化 <5%（写入 commit message）。

## Task 5: REST API

**Files:** Create `src/api/routes_tag.rs`；Modify `src/api/mod.rs`（模块声明 + 4 路由）

401/200 骨架先行 → 各 handler 直委托 `db::tag_registry`。单测：glob 参数校验、set 的 owner_inst 忽略。

## Task 6: e2e

**Files:** Create `tests/tag_registry_e2e.rs`（复用 `tests/common`）

场景：①401 未授权；②dataSim 采集 → 自动登记（props 默认 history=false、owner_inst=宿主实例）；③配置声明 props → 出数后落表初值；④API set 改 history → 实例 stop/start 再出数**不覆盖**（只补缺）；⑤batch glob 批量 + matched/updated 计数；⑥delete 后再出数重新占位。

## Task 7: 文档收口 + 计划回填

`docs/reference/rest-api.md` 新端点登记；CLAUDE.md 数据表清单 +1 行（tags）；`docs/README.md` 索引行状态 implemented + 本文件头部实施块回填；`python scripts/check_docs.py` 过。

---

## 后续路线图（本计划在总方案中的位置）

```
Phase 1a  本计划（Tag Registry）——独立交付，无 tsink 依赖
Phase 1b  历史存储写路径（tsink feature 引入；三层开关 L1 feature/L2 [history].enabled/L3 TagProps.history；
          deadband 过滤消费本表）——另立 plan，引用本文 D 决策
Phase 2   /api/v1/history 读路径 + subscribe KSUB dirty-set 升级（双 impl 过渡）
Phase 3   metrics/治理/关停序列/前端标签管理页
Phase 4   10 万点位 PoC 大压测 + 24h soak（验收：主链路劣化<5%、归档外标签零写入、deadband 削减率）
Phase 5   演进（PromQL 端点/rollup/中心侧 tsink-server/alarm P2 结构化绑定）
```

总方案关键 ADR（Phase 1b+ 引用）：形态=内嵌核心库+feature-gate（D1）；path 依赖 pin fork tag（D2）；单写线程 8MiB 栈+有界 channel（D3）；类型化写取 pipeline 内存值（D4）；metric=tag+labels[devid,hwid]+ms（D5）；资源全部显式有界（D6 修订：cardinality 按本表 history=true 计数×1.2 自动校准）；/api/v1/history 类型化查询（D7）；设备删除不级联删历史（D9）；close 超时 15s 兜底（D10）。
