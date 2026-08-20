# Lua 脚本系统演进 · 阶段 C（数据加工）实施计划

> 日期：2026-08-20
> **状态**：planned（依据 `2026-08-18-lua-script-system-evolution-design.md` v0.6 §7 路线图 C 行 + Q6/Q7/Q19/D1 定案拆分）
> **实施**：（执行时回填）
> **代码**：（执行时回填）
> **测试**：（执行时回填）
> **遗留**：（执行时回填）

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 落地演进设计阶段 C——luaTransform 薄壳（transform 角色订阅型实例，Q19 三薄插件之三）、EventBus Rtdata 载荷带值（Q6，SSE/前端同 PR）、subscribe 宿主过滤 + 自环防护（Q6 硬性验收）、on_timer 周期回调、rhai 冻结标注（Q7）。

**Architecture:** transform 不进 pipeline Processor（ADR D1：`Sample` 结构不动是 `pipeline.rs:6` 硬约束）——做成 **Source trait 订阅型实例**：luaTransform 薄壳订阅 EventBus Rtdata 事件（带值后）→ subscribe 模式过滤 → `on_rtdata(devid,tag,value)` 逐点回调；`on_timer(period)` 定时回调；输出经既有 `ctx:set_value` → hub → pipeline 全链路。数据面前置：Rtdata 载荷扩展 `samples` 字段（按 dev 聚合 + 截断），前端直用 + 轮询兜底并存。

**Tech Stack:** tokio broadcast（EventBus 现有）、mlua 0.12（lua_core 全量复用：engine/loader/api/manifest/hotload）、inventory 静态注册（registry.rs）。

**执行前置：** 跨 ≥5 文件 ≥10 commit，**必须在独立 worktree 执行**（`git worktree add ../rs-iot-lua-c -b feat/lua-stage-c`）。

**约定：** `cargo test -p rs-iot --features lua,lua-http`（包名 `rs-iot`）；每 Task 末 commit；TDD 先失败测试。

**C 阶段五项已定取舍（计划级决策，执行不再议）：**

1. **Rtdata 带值形状**：`EventPayload::Rtdata` 新增 `samples: Vec<RtdataSample>`（`{devid, values: BTreeMap<tag, SampleValue>, ts}`）；同批同 dev 合并（后写覆盖 tag），**dev 数沿用 `RTDATA_NOTIFY_MAX_DEVS`(50) 截断**，`count` 仍记真实点数；`dev_ids`/`count` 字段原样保留（前端旧消费零破坏）。
2. **自环防护 = api 层 self-write tracker**：`ApiCtx` 加 `self_writes: Option<Arc<Mutex<HashMap<(devid,tag), Instant>>>>`（仅 Role::Transform 传 Some），`push_sample` 记录自身写点，shell 收 Rtdata 时窗口（10s）内命中即跳过该点。备选「Sample 加 origin 字段」被 `pipeline.rs:6`「Sample 结构不动」硬约束否决。取舍：窗口内**其他源**对同 tag 的并发写入会被一并静默（文档注明；跨源不误自环是宿主保证，跨源可见性不受保证）。
3. **on_rtdata 单点形态**（Q6 定案原文 `on_rtdata(devid,tag,value)`）：批量事件由宿主拆点逐调，每次调用独立包 `callback_timeout_ms` 预算；超预算跳过剩余点 + 计数（与 collect 超时同语义）。
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
//     / event_rx（EventBus subscribe——仅 subscribe 非空 && 脚本有 on_rtdata 时订阅）
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
**Step 3:** 验收清单核对（对照路线图 C 行，实测留证）：
- [ ] 一个清洗/聚合脚本上线（agg-filter 经 e2e 实跑，产出派生点可见）
- [ ] 自环测试通过（transform 输出不触发自身——T8 测试名）
- [ ] pipeline 零改动（口径：`git diff` 中 `src/pipeline.rs` 仅 `publish_rtdata_notify`/聚合函数与常量，dispatcher/worker/背压链零触碰）

**Step 4:** Commit `docs(lua)+examples: 阶段 C 收口——示例/指南/schema/发布说明`

---

## 验收清单（对照路线图阶段 C——执行完回填）

- [ ] Rtdata 载荷带 values：pipeline 聚合/SSE/前端直用，旧字段兼容（T1）
- [ ] event_bus 容量可配 + SSE lagged 可观测（T2）
- [ ] rhai 冻结标注（Q7）（T3）
- [ ] Role::Transform 能力包 = C0/C2/C4/C6，一致性测试锁（T4）
- [ ] manifest `@role transform` any-of 契约（T5）
- [ ] luaTransform 走 InstanceManager 全套（生命周期/stat/热加载）（T7）
- [ ] subscribe 宿主过滤 + on_timer + on_rtdata 逐点预算（T6/T7）
- [ ] 自环测试通过（硬性验收）；跨实例单跳不误伤（T8）
- [ ] 清洗/聚合示例上线 + 文档五处同步 + CI 三组合全绿（T9）

> D 阶段计划待 C 落地后按同法拆分（D：Sparkplug Q9/alarm 角色 Q12 独立 spec/脚本包导入导出 Q14；透传正式版已随 B 提前完成）。
