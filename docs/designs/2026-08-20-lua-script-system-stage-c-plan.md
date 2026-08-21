# Lua 脚本系统演进 · 阶段 C（数据加工）实施计划

> 日期：2026-08-20
> **状态**：implemented（2026-08-21 于 worktree `feat/lua-stage-c` 执行完毕，9 task 全落地，待合并 master）
> **实施**：2026-08-21，9 task（`aac7cb9..` 分支首末：T1 aac7cb9 → T9 本分支末位 commit），全程 worktree 隔离 + task 间审查
> **代码**：`src/event_bus.rs`（RtdataSample/载荷带值）+ `src/pipeline.rs`（aggregate_rtdata，仅 publish 路径——dispatcher/worker/背压零触碰，diff 留证见文末）+ `src/config.rs`/`src/process_metrics.rs`/`src/api/routes_{events,metrics}/`（容量配置化 + lagged 可观测）+ `src/transform/rhai.rs`/`Cargo.toml`（冻结标注）+ `src/plugin/lua_core/{api,manifest}.rs`（Role::Transform 能力裁剪 + any-of 契约 + self-write tracker）+ `src/plugin/builtin/lua_transform/`（薄壳：mod.rs + config.rs）+ `frontend/`（types/monitor 直用 samples + luaTransform 表单 schema）+ `examples/lua-scripts/agg-filter/`
> **测试**：`cargo test -p rs-iot --features lua,lua-http -j 4` 全绿（数字见文末核对记录）；lua_transform_e2e 6 例（生命周期/订阅过滤/拒载/自环/跨实例/示例实测）全绿
> **遗留**：见设计文档头部「阶段 C 登记」（跨源同 tag 静默取舍 / reload 不重评估订阅模式 / EventBus fan-out 成本 / 自环窗口过期边界未专测）

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 落地演进设计阶段 C——luaTransform 薄壳（transform 角色订阅型实例，Q19 三薄插件之三）、EventBus Rtdata 载荷带值（Q6，SSE/前端同 PR）、subscribe 宿主过滤 + 自环防护（Q6 硬性验收）、on_timer 周期回调、rhai 冻结标注（Q7）。

**Architecture:** transform 不进 pipeline Processor（ADR D1：`Sample` 结构不动是 `pipeline.rs:6` 硬约束）——做成 **Source trait 订阅型实例**：luaTransform 薄壳订阅 EventBus Rtdata 事件（带值后）→ subscribe 模式过滤 → `on_rtdata(devid,tag,value)` 逐点回调；`on_timer(period)` 定时回调；输出经既有 `ctx:set_value` → hub → pipeline 全链路。数据面前置：Rtdata 载荷扩展 `samples` 字段（按 dev 聚合 + 截断），前端直用 + 轮询兜底并存。

**Tech Stack:** tokio broadcast（EventBus 现有）、mlua 0.12（lua_core 全量复用：engine/loader/api/manifest/hotload）、inventory 静态注册（registry.rs）。

**执行前置：** 跨 ≥5 文件 ≥10 commit，**必须在独立 worktree 执行**（`git worktree add ../rs-iot-lua-c -b feat/lua-stage-c`）。

**约定：** `cargo test -p rs-iot --features lua,lua-http`（包名 `rs-iot`）；每 Task 末 commit；TDD 先失败测试。

**C 阶段五项已定取舍（计划级决策，执行不再议）：**

1. **Rtdata 带值形状**：`EventPayload::Rtdata` 新增 `samples: Vec<RtdataSample>`（`{devid, values: BTreeMap<tag, SampleValue>, ts}`）；同批同 dev 合并（后写覆盖 tag），**dev 数沿用 `RTDATA_NOTIFY_MAX_DEVS`(50) 截断**，`count` 仍记真实点数；`dev_ids`/`count` 字段原样保留（前端旧消费零破坏）。
2. **自环防护 = api 层 self-write tracker**：`ApiCtx` 加 `self_writes: Option<Arc<Mutex<HashMap<(devid,tag), Instant>>>>`（仅 Role::Transform 传 Some），`push_sample` 记录自身写点，shell 收 Rtdata 时窗口（10s）内命中即跳过该点。备选「Sample 加 origin 字段」被 `pipeline.rs:6`「Sample 结构不动」硬约束否决。取舍：窗口内**其他源**对同 tag 的并发写入会被一并静默（文档注明；跨源不误自环是宿主保证，跨源可见性不受保证）。
3. **on_rtdata 单点形态**（Q6 定案原文 `on_rtdata(devid,tag,value)`）：批量事件由宿主拆点逐调，每次调用独立包 `callback_timeout_ms` 预算；**超预算（await 型挂起被切断）跳过批内剩余点** + 计数（与 collect 超时同语义）；脚本即时错误（不产生停摆）逐点继续。（复审 P1-1 后定稿：T7 首版曾误实现为"超预算逐点继续"——会把整批停摆放大到 命中点数×预算，已修复并加回归锁 `transform_on_rtdata_budget_exceeded_abandons_batch`。）
4. **跨实例环不做静态消除**（Q6 定案「配置责任」）：异常频率告警依赖 Q12 告警模型（D 阶段独立 spec），C 阶段指南写明配置责任 + 复用既有 stat 体系可观测，不建半成品告警。
5. **rhai 冻结 = 文档标注**（Q7「标 deprecated、不接线不投入」）：模块 doc + Cargo feature 注释 + 文档一致性，不删代码不摘 feature（`#[deprecated]` 会响编译警告，不取）。

---

## 组 0：数据面基础（transform 的数据源，先行）

### Task 1: Rtdata 载荷带 values（payload/emit/SSE/前端，Q6 同 PR 面）

**Files:**
- Modify: `src/event_bus.rs`（EventPayload::Rtdata 加字段 + RtdataSample 结构）
- Modify: `src/pipeline.rs:235-254`（`publish_rtdata_notify` 聚合带值；抽纯函数便于单测）
- Test: `src/pipeline.rs` tests 模块（267 行起）追加；`src/event_bus.rs` tests 如有
- Modify: `frontend/src/types.ts` + `frontend/src/stores/monitor.ts`（rtdata SSE handler 直用 values）

**Step 1: 失败测试**——pipeline tests：
- 抽纯函数 `aggregate_rtdata(buf: &[Sample]) -> (Vec<String> /*dev_ids*/, usize /*count*/, Vec<RtdataSample>)`：断言同批同 dev 合并（后写覆盖 tag 值）、>50 dev 截断（samples 与 dev_ids 同步截断）、count 为真实点数、`ts` 取该 dev 最后一笔 Sample.timestamp。
- serde 形状锁：Rtdata 序列化 JSON 含 `samples` 数组且 `dev_ids`/`count` 字段仍在（兼容字段不丢）。

**Step 2: 实现**：

```rust
/// Rtdata 事件带的单设备最新值（Q6 带值扩展，C 阶段 T1）。
#[derive(Debug, Clone, Serialize)]
pub struct RtdataSample {
    pub devid: String,
    pub values: BTreeMap<String, SampleValue>,
    /// 该 dev 本批最后一笔的 Unix 秒时间戳。
    pub ts: i64,
}
// EventPayload::Rtdata { dev_ids, count, samples, ts } —— dev_ids/count 保留（前端兼容）。
```

`publish_rtdata_notify` 内改调 `aggregate_rtdata`（50ms 攒批窗口 buf 全量在手，聚合 O(n)）。

**Step 3: 前端**——`types.ts` Rtdata 事件类型加 `samples?: {devid, values, ts}[]`；`monitor.ts` rtdata handler：payload 带 samples 时直接按现有 cell 合并路径合入 `rtdata` store（值形状对齐 `loadRtdata` 产物），**保留**去抖补拉与 2s 轮询（lagged 丢帧兜底，Q6「尽量处理」语义）。
**Step 4:** 全绿 + `cd frontend && npm run build` 过 → commit `feat(event): Rtdata 载荷带 values（Q6——pipeline 聚合/SSE/前端直用）`

### Task 2: EventBus 容量配置化 + SSE lag 可观测（R6）

**Files:**
- Modify: `src/config.rs`（`event_bus_capacity: usize` 默认 256）+ `src/lib.rs:107`（消费配置）
- Modify: `src/process_metrics.rs`（`sse_lagged_count: AtomicU64` + `sse_lagged(n)` 方法，照 ctl_* 模式）
- Modify: `src/api/routes_events.rs:56`（`Err(Lagged(n))` 分支累加计数——不再静默 None）
- Modify: `src/api/routes_metrics/render.rs`（输出 `rs_iot_sse_lagged_total`）

**Step 1: 失败测试**——render.rs 测试：计数 >0 时输出含 metric 行；config 默认值测试（256）。
**Step 2-4:** 实现 → 全绿 → commit `feat(observability): event_bus 容量配置化 + SSE lagged 计数（R6）`

---

## 组 1：rhai 冻结（Q7，独立小项）

### Task 3: rhai deprecated 标注

**Files:**
- Modify: `src/transform/rhai.rs`（模块头 doc 加冻结说明：Q7 定案——Lua 为唯一脚本体系，rhai 已实现未接线、不再投入，实战半年后复审移除或接线）
- Modify: `Cargo.toml:140-141`（transform feature 注释改「L2 数据转换（rhai，**已冻结 deprecated**——Q7，不接线不投入）」）
- Modify: `src/lib.rs` 恒 Passthrough 处注释复核（如已述及则仅校准措辞）；`README.md`/`CLAUDE.md` 如有 rhai 描述同步。

**Step 1-2:** 标注（零行为变化）→ 全绿 → commit `docs(lua): rhai 冻结标注（Q7——deprecated 不接线，D4 终审）`

---

## 组 2：lua_core 角色与契约扩展

### Task 4: Role::Transform 能力包裁剪（§5.2 矩阵）

**Files:**
- Modify: `src/plugin/lua_core/api/mod.rs`（Role 枚举 + send_command/http 注册门）
- Test: `src/plugin/lua_core/api/tests.rs` 追加

**Step 1: 失败测试**（能力卡一致性锁模式，照 B 阶段 north 断言）——Role::Transform 组装的 VM：`ctx` 表**无** `request`/`transport`/`send_command`/`http_get`（C1/C3/C5 ✗），**有** `set_value`/`get_value`/`get_device`/`list_devices`/`kv_set`/`kv_get`/`secret_get`/`log_info`/`now`（C0/C2/C4/C6 ✓）。
**Step 2: 实现**：

```rust
pub(crate) enum Role { Collector, North, Transform }
// api/mod.rs:332 send_command 注册包门：if ac.role != Role::Transform { ... }（矩阵 transform ✗；north 恒注入是 Q5 兼容依赖）
// api/mod.rs:538 http::register 包门：if ac.role != Role::Transform { ... }（矩阵 C5：collector ✓ north ✓ transform ✗）
```

**Step 3-4:** 全绿 → commit `feat(lua): Role::Transform 能力包裁剪（§5.2——C1/C3/C5 不注册）`

### Task 5: manifest transform 角色契约（any-of 语义）

**Files:**
- Modify: `src/plugin/lua_core/manifest.rs:156-178`（required_callbacks 机制扩展）
- Test: 同文件 tests

**Step 1: 失败测试**——`@role transform`：只有 `on_timer` 过；只有 `on_rtdata` 过；两者皆无 → 拒载（可行动错误「transform 角色需至少声明 on_timer 或 on_rtdata 其一」）；`on_init` 恒可选。
**Step 2: 实现**——现机制是「清单全必须」；transform 改为返回 `Required::Any(&["on_timer", "on_rtdata"])`（或等价分支），其余角色语义不变。
**Step 3-4:** 全绿 → commit `feat(lua): manifest transform 角色契约（on_timer/on_rtdata any-of，§2.2）`

---

## 组 3：luaTransform 薄壳（Q19 三薄插件之三）

### Task 6: TransformCfg 解析 + subscribe 模式匹配

**Files:**
- Create: `src/plugin/builtin/lua_transform/config.rs`
- Test: 同文件 tests

**Step 1: 失败测试**——解析：`script` 必填（沿用脚本名校验规则）、`period` 默认 3、`subscribe` 缺省空 vec；非法 pattern（空串/`..`/段数 >2）拒；匹配器：`"dev1"` 匹配 dev1 全部 tag；`"dev2.temp*"` 匹配 dev2 的 temp 开头 tag；`"*"` dev 通配（全设备）；dev 精确 + tag glob 语义单测（`*` 任意序列，无 `?`，无新依赖）。
**Step 2: 实现**：

```rust
pub(crate) struct TransformCfg {
    pub script: String,
    pub period: u64,                 // on_timer 周期秒（默认 3，对齐 lua_client）
    pub subscribe: Vec<String>,      // Q6：空 = 纯定时（不订 EventBus）
}
/// "dev" / "dev.tagpat" 两段解析 + glob 匹配纯函数（pub(crate) 供 shell 与测试）。
```

**Step 3-4:** 全绿 → commit `feat(lua): TransformCfg + subscribe 模式匹配（Q6 宿主过滤）`

### Task 7: luaTransform Source 薄壳主体

**Files:**
- Create: `src/plugin/builtin/lua_transform/mod.rs`（Source impl + 主循环）
- Modify: `src/plugin/builtin/mod.rs`、`src/plugin/registry.rs` 消费侧如需（SourceEntry 注册在 mod.rs `inventory::submit!`，照 lua_client 模式）
- Test: `tests/lua_transform_e2e.rs`（新）

**Step 1: 失败测试**（e2e 骨架，照 `tests/lua_north_e2e.rs` 模式）——lux 写 `app_code=luaTransform` 实例 → `try_start_from_lux` → states Running → `/inst/stats` 可见 → stop → Stopped；能力包断言（T4 e2e 面：request/send_command 不在 ctx 表）。
**Step 2: 实现**——镜像 lua_client 骨架去 IO 层：

```rust
enum Evt { Cmd(Option<CtlMessage>), Reload, Tick, Rtdata(EventPayload) }
// run()：VM 组装（Role::Transform、io=None、manifest dry_run 校验 @role transform）
//   biased select!：token / cmd_rx（"reload" → Evt::Reload，VM 级热加载回退）
//     / reload_rx（hotload watcher，照 lua_client:216） / ticker（period → on_timer）
//     / event_rx（EventBus subscribe——subscribe 非空即订阅；on_rtdata 存在性
//       批级检查——计划初稿的"订阅即查 on_rtdata"会使 reload 后新增回调收不到事件）
// on_rtdata 投递：EventPayload::Rtdata.samples 展开 → matches() 过滤 → 逐点
//   call_on_rtdata(lua, devid, tag, value)（with_budget 同款预算包装；超时跳过剩余点+计数）
// on_timer()：无参回调，同预算包装
```

**Step 3-4:** 全绿 → commit `feat(lua): luaTransform 薄壳——Source 订阅型实例 + InstanceManager 全套（Q19/D1）`

### Task 8: 自环防护（Q6 硬性验收）+ 跨实例 e2e

**Files:**
- Modify: `src/plugin/lua_core/api/mod.rs`（ApiCtx 加 `self_writes` + push_sample 记录）
- Modify: `src/plugin/builtin/lua_transform/mod.rs`（Rtdata 分支过滤）
- Test: `tests/lua_transform_e2e.rs` 追加

**Step 1: 失败测试**（硬性验收，路线图 C 行原文）：
- **自环**：transform 脚本 `on_rtdata` 内 `ctx:set_value` 写派生点（或写自身订阅的 tag）→ 样本经 hub→pipeline→EventBus 回流 → 断言该实例 `on_rtdata` 计数**不增长**（用 kv 计数器验证）；
- **不误伤正常路径**：transform A（订阅 dev1）在 on_timer 里 set_value 写 dev2 → transform B（订阅 dev2）on_rtdata **收到**（跨实例单跳正常传播）；
- 窗口语义：self_writes 记录 10s 内命中即跳过（常量 `SELF_LOOP_GUARD_WINDOW`，注释注明跨源同 tag 静默取舍）。
**Step 2: 实现**——`ApiCtx.self_writes: Option<Arc<Mutex<HashMap<(String,String), std::time::Instant>>>>`（Collector/North 传 None 零开销；push_sample Some 分支记录后即时清理过期项防膨胀）；shell 过滤在 subscribe 匹配之后、逐点回调之前。
**Step 3-4:** 全绿 → commit `feat(lua): transform 自环防护（Q6 硬性验收——self-write tracker 窗口过滤）`

---

## 组 4：示例与收口

### Task 9: 清洗/聚合示例 + 文档 + 验收核对

**Files:**
- Create: `examples/lua-scripts/agg-filter/main.lua`（`@role transform`：订阅原始点 → 滑动窗口均值 + 限幅 → set_value 写派生点；manifest 头注释块完整）
- Modify: `docs/guides/lua-script-guide.md`（transform 角色章：回调契约/能力矩阵/subscribe 语法/自环语义与跨实例配置责任/示例）、`frontend/src/lib/pluginSchemas.ts`（luaTransform 表单 schema）、`docs/README.md` 索引、`docs/ops/releases/2026-08-20-lua-stage-c.md`、设计文档头部实施块 + v0.7 行 + 遗留清单

**Step 1:** 示例脚本（滑动窗用 kv 存窗口状态，演示 C4；纯定时 + 订阅双模式注释说明）。
**Step 2:** 文档五处同步 + `python scripts/check_docs.py` 过。
**Step 3:** 验收清单核对（对照路线图 C 行，实测留证；2026-08-21 完成，证据见文末验收清单与核对记录）：
- [x] 一个清洗/聚合脚本上线（agg-filter 经 e2e 实跑，产出派生点可见——`agg_filter_example_end_to_end`：窗口均值 20.0 落库断言）
- [x] 自环测试通过（transform 输出不触发自身——`transform_self_loop_guard_suppresses_own_output`，真实链路回流）
- [x] pipeline 零改动（口径：`git diff` 中 `src/pipeline.rs` 仅 `publish_rtdata_notify`/聚合函数与常量，dispatcher/worker/背压链零触碰——diff stat 见核对记录）

**Step 4:** Commit `docs(lua)+examples: 阶段 C 收口——示例/指南/schema/发布说明`

---

## 验收清单（对照路线图阶段 C——已逐项实测核对）

- [x] Rtdata 载荷带 values：pipeline 聚合/SSE/前端直用，旧字段兼容（T1）
  ——测试：`pipeline.rs::aggregate_rtdata_merges_same_dev_last_write_wins` /
  `aggregate_rtdata_truncates_devs_in_sync` /
  `rtdata_payload_json_keeps_compat_fields_and_adds_samples`；前端
  `types.ts` samples 类型 + `monitor.ts::applyRtdataSamples`（带 samples 直合入，
  轮询兜底保留）
- [x] event_bus 容量可配 + SSE lagged 可观测（T2）
  ——测试：`config.rs::event_bus_capacity_default_and_override`（默认 256/可覆盖）+
  `render.rs::render_process_has_help_type_and_values`（sse_lagged_count=3 → 输出含
  `rs_iot_sse_lagged_total 3` + TYPE 行）+ `routes_events.rs` Lagged 分支累加
- [x] rhai 冻结标注（Q7）（T3）
  ——`rg` 验证在位：`src/transform/rhai.rs:3`（模块 doc「已冻结（deprecated，演进设计
  Q7/D4 终审 2026-08-20）」）+ `Cargo.toml:140`（feature 注释「已冻结 deprecated：Q7
  定案不接线不投入，数据加工走 luaTransform」）；零行为变化（恒 Passthrough 不变）
- [x] Role::Transform 能力包 = C0/C2/C4/C6，一致性测试锁（T4）
  ——测试：`api/tests.rs::transform_role_trims_c1_c3_c5`（C1/C3/C5 ✗ + C0/C2/C4 ✓）+
  e2e 顶层探针（`lua_transform_e2e.rs::transform_lifecycle_running_stats_stop` 内
  assert 探针——探针失败不进 Running）
- [x] manifest `@role transform` any-of 契约（T5）
  ——测试：`manifest.rs::dry_run_transform_any_of_on_timer_or_on_rtdata`（仅
  on_timer 过/仅 on_rtdata 过/皆无拒）+ e2e
  `transform_manifest_missing_callbacks_rejected`（拒载 → Failed，stat 不写）
- [x] luaTransform 走 InstanceManager 全套（生命周期/stat/热加载）（T7）
  ——e2e：`transform_lifecycle_running_stats_stop`（lux 写实例 → startinst → Running
  → on_timer kv ticks ≥1 → `/inst/stats` state=running + script_version → stopinst
  → Stopped + stat flush stopped）；热加载 watcher/VM 级 reload 路径在
  `lua_transform/mod.rs`（照 lua_client 模式接线）
- [x] subscribe 宿主过滤 + on_timer + on_rtdata 逐点预算（T6/T7）
  ——测试：`lua_transform/config.rs` tests（parse_defaults/parse_rejects_bad_patterns/
  matcher_any_of/glob 语义 + 非法 pattern 拒载六形）+ e2e
  `transform_on_rtdata_matching_and_filtered`（dev1.temp*/dev2 命中、dev1.hum/dev3
  不投递）；预算：on_init/on_timer/on_rtdata 统一 `with_budget(callback_timeout_ms)`
  包装（超时跳过本轮/该点 + 计数，与 collect 同语义）
- [x] 自环测试通过（硬性验收）；跨实例单跳不误伤（T8）
  ——e2e：`transform_self_loop_guard_suppresses_own_output`（真实链路回流，kv 计数
  2.5s 五采样恒稳定不增长）+ `transform_cross_instance_propagation_not_blocked`
  （A 写 devB.derived → B on_rtdata 收到，got ≥1 + last_tag=derived）
- [x] 清洗/聚合示例上线 + 文档五处同步 + CI 三组合全绿（T9）
  ——e2e：`agg_filter_example_end_to_end`（读 `examples/lua-scripts/agg-filter/main.lua`
  文件本体跑真实链路：野点剔除 + 窗口 [10,20,30] 均值 20.0 落库 + cfg_window=3 生效）；
  文档五处：指南 transform 章 / pluginSchemas luaTransform schema / docs 索引 /
  发布说明 `2026-08-21-lua-stage-c.md` / 设计文档 v0.7 回填；验证四件套见下核对记录

> D 阶段计划待 C 落地后按同法拆分（D：Sparkplug Q9/alarm 角色 Q12 独立 spec/脚本包导入导出 Q14；透传正式版已随 B 提前完成）。

## 核对记录（T9，2026-08-21）

- **全量测试**：`cargo test -p rs-iot --features lua,lua-http -j 4` → **30 个测试目标
  全绿，467 passed / 0 failed / 7 ignored**（ignored 为需外部资源的既有项）；其中
  `tests/lua_transform_e2e.rs` 6/6（生命周期/订阅过滤/any-of 拒载/自环/跨实例/示例实测）。
- **agg-filter 示例实测数字**（`agg_filter_example_end_to_end`，真实链路 EventBus →
  subscribe 过滤 → on_rtdata → set_value → hub → pipeline → lux）：预写 kv
  `cfg_window=3`（on_init 读取覆盖脚本默认 5——窗口可配生效）；喂 `999`（探针，超上限
  剔除）/ `10` / `20` / `200`（野点剔除）/ `30` → 派生点 `rtdata:agg1` 的
  `agg1.temp_avg` 落库值 = **20.0**（窗口 [10,20,30] 均值）、窗口 kv = `[10,20,30]`、
  `rejected` 计数 ≥ 2、停发后窗口稳定（无自环放大）。
- **pipeline 零改动口径**：`git diff 13875a6..HEAD --stat -- src/pipeline.rs` →

  ```text
  src/pipeline.rs | 203 ++++++++++++++++++++++++++++++++++++++++++++++++++------
  1 file changed, 184 insertions(+), 19 deletions(-)
  ```

  hunk 落点逐一核对：imports（`RtdataSample`/`SampleValue`/`BTreeMap`+`HashMap`）、
  `flush_batch` 内注释更新（`publish_rtdata_notify` 调用不变）、`RTDATA_NOTIFY_MAX_DEVS`
  doc 注释、`publish_rtdata_notify` 重写（改调 `aggregate_rtdata`）+ 新增纯函数
  `aggregate_rtdata`、tests 模块追加 3 测试——**dispatcher（route/auto_workers/run）、
  worker_run、背压链零触碰**。
- **其余验证**：`cargo fmt --all --check` 过；`python scripts/check_docs.py` 过
  （32 个 md）；`cd frontend && npm run build` 过（vue-tsc -b + vite，含类型检查）。
