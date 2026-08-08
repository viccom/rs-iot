# M4 埋点体系 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 rs-iot 建立统一埋点规范 + 核心框架咽喉埋点 + 内置/外置插件埋点 + 统一 REST 暴露，消除 CtlRegistry 静默丢命令等盲区。

**Architecture:** `InstanceStat`（核心 6 字段 + serde flatten 扩展）作通用契约，`StatTracker` 统一插件接入，`ProcessMetrics`（进程级内存 AtomicU64）承载核心框架咽喉指标。per-instance stat 落 lux `stat:<inst>` 周期 flush；进程级 REST 直读 atomic。REST 合并 `{state, health}`（进程级死活权威 + 插件级健康）。前瞻 Prometheus 命名，不引依赖。

**Tech Stack:** Rust + tokio + serde(flatten) + std::sync::atomic + sysinfo + axum + lux EmbeddedClient。

**Spec:** `docs/superpowers/specs/2026-08-08-m4-instrumentation-design.md`

**验证基线（每个 task 后跑）：** `cargo fmt --all --check` + `cargo clippy --all-targets --all-features -- -D warnings` + 相关 `cargo test`。Windows 增量缓存偶发 E0463 用 `CARGO_BUILD_JOBS=1` 规避。

---

## Phase 1：规范地基

### Task 1: `db::stat` 通用模块（替 luastat）

**Files:**
- Create: `src/db/stat.rs`
- Modify: `src/db/mod.rs:1-5`（`pub mod luastat;` → `pub mod stat;`）
- Delete: `src/db/luastat.rs`

- [ ] **Step 1: 建 `src/db/stat.rs`（InstanceStat + write/read）**

```rust
//! 通用实例指标（M4）：lux string key `stat:<inst_id>` 存 JSON。
//!
//! 核心 6 字段所有实例一致；插件扩展走 `extra`（serde flatten 透传）。
//! 替代 M2 的 `luastat`（lua 专属）——lua 迁移后此模块是全部实例的 stat 承载。

use crate::error::RsIotError;
use lux::{EmbeddedClient, EmbeddedValue};

/// 实例状态快照（写 lux `stat:<inst_id>`）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InstanceStat {
    /// health 维度：`running` / `degraded` / `stopped`。死活看进程级 `InstState`。
    pub status: String,
    /// 累计活动成功次数（counter）。
    pub tick_count: u64,
    /// 累计活动失败次数（counter）。
    pub fail_count: u64,
    /// 最近一次活动墙上时间（unix 秒）。
    pub last_activity_ts: i64,
    /// 最近一次活动耗时（毫秒）。
    pub last_activity_duration_ms: u64,
    /// 最近一次失败原因（成功时清空）。
    pub last_err: String,
    /// 插件扩展字段（flatten 透传，如 opcua session_reconnect_count、lua vm_restart_count）。
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// key 名 `stat:<inst_id>`。
fn key(inst_id: &str) -> String {
    format!("stat:{inst_id}")
}

/// 写状态快照（JSON，`SET stat:<inst>`）。
pub async fn write(
    c: &EmbeddedClient,
    inst_id: &str,
    stat: &InstanceStat,
) -> Result<(), RsIotError> {
    let json = serde_json::to_string(stat)
        .map_err(|e| RsIotError::Config(format!("InstanceStat 序列化失败：{e}")))?;
    c.execute("SET", &[&key(inst_id), &json]).await?;
    Ok(())
}

/// 读状态快照（无则 `None`）。
pub async fn read(
    c: &EmbeddedClient,
    inst_id: &str,
) -> Result<Option<InstanceStat>, RsIotError> {
    let v = c.execute_value("GET", &[&key(inst_id)]).await?;
    match v {
        EmbeddedValue::Bulk(b) => {
            let s = String::from_utf8_lossy(&b);
            if s.is_empty() {
                return Ok(None);
            }
            let stat: InstanceStat = serde_json::from_str(&s)
                .map_err(|e| RsIotError::Config(format!("InstanceStat 反序列化失败：{e}")))?;
            Ok(Some(stat))
        }
        EmbeddedValue::Simple(s) if !s.is_empty() => {
            let stat: InstanceStat = serde_json::from_str(&s)
                .map_err(|e| RsIotError::Config(format!("InstanceStat 反序列化失败：{e}")))?;
            Ok(Some(stat))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn client() -> EmbeddedClient {
        let cfg = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: std::env::temp_dir()
                    .join(format!("inststat-{}", std::process::id()))
                    .to_str()
                    .unwrap()
                    .to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let handle = lux::run_with_config(crate::db::build_lux_config(&cfg))
            .await
            .expect("lux");
        handle.client()
    }

    fn sample() -> InstanceStat {
        InstanceStat {
            status: "running".into(),
            tick_count: 5,
            fail_count: 1,
            last_activity_ts: 123,
            last_activity_duration_ms: 42,
            last_err: "timeout".into(),
            extra: serde_json::Map::new(),
        }
    }

    #[tokio::test]
    async fn write_read_roundtrip() {
        let c = client().await;
        write(&c, "inst1", &sample()).await.unwrap();
        let got = read(&c, "inst1").await.unwrap().expect("stat 应存在");
        assert_eq!(got.status, "running");
        assert_eq!(got.tick_count, 5);
        assert_eq!(got.fail_count, 1);
        assert_eq!(got.last_err, "timeout");
    }

    #[tokio::test]
    async fn read_missing_is_none() {
        let c = client().await;
        assert!(read(&c, "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn instances_are_isolated() {
        let c = client().await;
        write(&c, "a", &sample()).await.unwrap();
        assert!(read(&c, "b").await.unwrap().is_none(), "实例间隔离");
    }

    #[test]
    fn extra_flatten_preserves_extension_fields() {
        // 序列化时 extra 字段平铺到顶层；反序列化时未知字段回到 extra。
        let mut s = sample();
        s.extra
            .insert("vm_restart_count".into(), serde_json::json!(3));
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"vm_restart_count\":3"), "extra 应 flatten 到顶层");
        let back: InstanceStat = serde_json::from_str(&json).unwrap();
        assert_eq!(back.extra["vm_restart_count"], 3, "未知字段回到 extra");
        assert_eq!(back.tick_count, 5, "核心字段正常");
    }
}
```

- [ ] **Step 2: 更新 `src/db/mod.rs`**

将 `pub mod luastat;`（第 4 行）改为 `pub mod stat;`。

- [ ] **Step 3: 删除 `src/db/luastat.rs`**

```bash
git rm src/db/luastat.rs
```

- [ ] **Step 4: 验证**

```bash
cargo test --lib db::stat
cargo clippy --all-targets --all-features -- -D warnings
```
Expected: stat 测试全过；clippy 零警告。**注意**：此步 `lua_client` 仍引用 `db::luastat` 会编译失败——先不跑全量 build，下个 task 修。本 task 只确保 `db::stat` 自身测试通过（`cargo test --lib db::stat` 在 lua 引用断裂前可能失败，故此 task 实际要和 Task 5 lua 迁移在同一次可编译状态完成；若独立验证，临时把 lua_client 的 luastat 引用注释掉跑 stat 测试，再恢复）。

实际操作：Task 1-5 是一个编译闭环（luastat 删后 lua 断裂，需 Task 5 修复）。**Task 1 提交后允许中间编译断裂，Task 5 完成后恢复全绿**。提交信息注明「M4 规范地基进行中：db::stat 建模 + 删 luastat（lua 迁移见 Task 5）」。

- [ ] **Step 5: Commit**

```bash
git add src/db/stat.rs src/db/mod.rs
git rm src/db/luastat.rs
git commit -m "feat(m4): db::stat 通用 InstanceStat 模块（替 luastat）"
```

---

### Task 2: `StatTracker` 统一接入 helper

**Files:**
- Create: `src/plugin/stat_tracker.rs`
- Modify: `src/plugin/mod.rs`（加 `pub mod stat_tracker;`）

- [ ] **Step 1: 建 `src/plugin/stat_tracker.rs`**

```rust
//! 实例指标追踪器（M4）：插件主采集 task 内持有，封装 InstanceStat 计数 + 周期 flush。
//!
//! 单 task 串行持有（`&mut self`，无需锁）；插件 select! 加 flush ticker + 采集后 tick_ok/fail。

use crate::db::stat::InstanceStat;
use lux::EmbeddedClient;

/// 周期 flush：每 N 次活动落一次 lux（防大点表热点）。
pub const FLUSH_EVERY_TICKS: u64 = 50;
/// 周期 flush：每 N 秒落一次 lux（与 FLUSH_EVERY_TICKS 取先到）。
pub const FLUSH_INTERVAL_SECS: u64 = 5;
/// 连续失败 N 次后 status 转 degraded。
const DEGRADED_AFTER_FAILS: u32 = 3;

/// 实例指标追踪器。
pub struct StatTracker {
    stat: InstanceStat,
    inst_id: String,
    lux: EmbeddedClient,
    consec_fails: u32,
}

impl StatTracker {
    /// 构造（status 初始 running，计数 0）。
    pub fn new(inst_id: String, lux: EmbeddedClient) -> Self {
        Self {
            stat: InstanceStat {
                status: "running".into(),
                tick_count: 0,
                fail_count: 0,
                last_activity_ts: 0,
                last_activity_duration_ms: 0,
                last_err: String::new(),
                extra: serde_json::Map::new(),
            },
            inst_id,
            lux,
            consec_fails: 0,
        }
    }

    /// 活动成功：tick_count++，更新 last_activity_*，清 last_err，status=running。
    pub fn tick_ok(&mut self, duration_ms: u64) {
        self.stat.tick_count = self.stat.tick_count.saturating_add(1);
        self.stat.last_activity_ts = chrono::Local::now().timestamp();
        self.stat.last_activity_duration_ms = duration_ms;
        self.stat.last_err.clear();
        self.consec_fails = 0;
        self.stat.status = "running".into();
    }

    /// 活动失败：fail_count++，last_err=err；连续失败达阈值 → status=degraded。
    pub fn tick_fail(&mut self, err: &str) {
        self.stat.fail_count = self.stat.fail_count.saturating_add(1);
        self.stat.last_activity_ts = chrono::Local::now().timestamp();
        self.stat.last_err = err.into();
        self.consec_fails = self.consec_fails.saturating_add(1);
        if self.consec_fails >= DEGRADED_AFTER_FAILS {
            self.stat.status = "degraded".into();
        }
    }

    /// 写扩展字段（opcua session_reconnect_count / lua vm_restart_count 等）。
    pub fn extra_insert(&mut self, k: &str, v: serde_json::Value) {
        self.stat.extra.insert(k.into(), v);
    }

    /// 直接设 status（如退出时设 stopped）。
    pub fn set_status(&mut self, s: &str) {
        self.stat.status = s.into();
    }

    /// flush 到 lux stat:<inst>（失败仅 warn，不阻塞采集）。
    pub async fn flush(&self) {
        if let Err(e) = crate::db::stat::write(&self.lux, &self.inst_id, &self.stat).await {
            tracing::warn!("[stat] flush {} failed: {e}", self.inst_id);
        }
    }

    /// 是否到周期 flush 点（tick % FLUSH_EVERY_TICKS == 0 且 tick>0）。
    pub fn should_flush(&self) -> bool {
        self.stat.tick_count != 0 && self.stat.tick_count % FLUSH_EVERY_TICKS == 0
    }

    /// 当前 tick_count（测试用）。
    #[cfg(test)]
    pub fn tick_count(&self) -> u64 {
        self.stat.tick_count
    }

    /// 当前 status（测试用）。
    #[cfg(test)]
    pub fn status(&self) -> &str {
        &self.stat.status
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker() -> StatTracker {
        StatTracker::new("t".into(), lux::EmbeddedClient::default())
    }

    #[test]
    fn tick_ok_increments_and_clears_err() {
        let mut t = tracker();
        t.tick_fail("e");
        t.tick_ok(10);
        assert_eq!(t.tick_count(), 1);
        assert_eq!(t.status(), "running");
    }

    #[test]
    fn tick_fail_degrades_after_threshold() {
        let mut t = tracker();
        for _ in 0..3 {
            t.tick_fail("e");
        }
        assert_eq!(t.status(), "degraded", "连续 3 次失败转 degraded");
        assert_eq!(t.stat_fail_count_for_test(), 3);
    }

    #[test]
    fn tick_ok_resets_degraded() {
        let mut t = tracker();
        for _ in 0..3 {
            t.tick_fail("e");
        }
        assert_eq!(t.status(), "degraded");
        t.tick_ok(5);
        assert_eq!(t.status(), "running", "成功恢复转 running");
    }

    #[test]
    fn should_flush_at_every_50_boundary() {
        let mut t = tracker();
        assert!(!t.should_flush(), "tick=0 不 flush");
        for _ in 0..50 {
            t.tick_ok(1);
        }
        assert!(t.should_flush(), "tick=50 flush");
        t.tick_ok(1);
        assert!(!t.should_flush(), "tick=51 不 flush");
    }
}
```

- [ ] **Step 2: 测试辅助字段 + 修复**

上面测试用了 `t.stat_fail_count_for_test()`，需在 `impl StatTracker` 的 `#[cfg(test)]` 块补：

```rust
    #[cfg(test)]
    pub fn stat_fail_count_for_test(&self) -> u64 {
        self.stat.fail_count
    }
```

- [ ] **Step 3: 注册模块 `src/plugin/mod.rs`**

在 `pub mod instance;` 一行后加 `pub mod stat_tracker;`。

- [ ] **Step 4: 验证**

```bash
cargo test --lib plugin::stat_tracker
```
Expected: 4 测试全过。stat_tracker 不依赖 lux 运行（flush 未调），单测无 lux 实例即可。

- [ ] **Step 5: Commit**

```bash
git add src/plugin/stat_tracker.rs src/plugin/mod.rs
git commit -m "feat(m4): StatTracker 统一插件指标接入 helper"
```

---

### Task 3: InstanceManager configs 内存表（分类数据源）

**Files:**
- Modify: `src/plugin/instance.rs`

- [ ] **Step 1: 加 configs 字段 + 注入 + 查询方法**

在 `InstanceManager` struct（约第 50 行）加字段：

```rust
pub struct InstanceManager {
    ctx: HostContext,
    states: Arc<DashMap<String, InstState>>,
    handles: Arc<DashMap<String, InstanceHandle>>,
    /// 实例配置快照（start 时记，stop 保留；/inst/stats 分类数据源）。
    configs: Arc<DashMap<String, InstConfig>>,
    hub_tx: mpsc::Sender<Sample>,
}
```

`new()`（约第 60 行）加初始化：

```rust
    pub fn new(ctx: HostContext, hub_tx: mpsc::Sender<Sample>) -> Self {
        Self {
            ctx,
            states: Arc::new(DashMap::new()),
            handles: Arc::new(DashMap::new()),
            configs: Arc::new(DashMap::new()),
            hub_tx,
        }
    }
```

`start_source`（约第 89 行，`self.states.insert(id_key.clone(), InstState::Starting);` 之后）加：

```rust
        self.configs.insert(id_key.clone(), inst.clone());
```

`start_sink`（约第 130 行，同样位置）加同样一行。

`stop`（约第 167 行）**不加 configs 清理**（spec：stop 保留配置态；状态走 Stopped）。

在 impl 末尾（`send_ctl` 之后）加查询方法：

```rust
    /// 取实例配置快照（/inst/stats 分类用：app_code + host_kind）。
    pub fn config_snapshot(&self, inst_id: &str) -> Option<InstConfig> {
        self.configs.get(inst_id).map(|r| r.value().clone())
    }
```

- [ ] **Step 2: 验证编译**

```bash
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | head -20
```
Expected: InstanceManager 改动无新警告（configs 字段已用，非 dead）。**编译仍因 lua_client 引用 luastat 断裂**——只看 instance.rs 本身无错。

- [ ] **Step 3: Commit**

```bash
git add src/plugin/instance.rs
git commit -m "feat(m4): InstanceManager configs 内存表（/inst/stats 分类数据源）"
```

---

### Task 4: 统一 `/inst/stats` 端点 + 废弃 `/lua/stats`

**Files:**
- Create: `src/api/routes_stat.rs`
- Modify: `src/api/routes_lua.rs`（删 `stats` handler）
- Modify: `src/api/mod.rs`（注册 /inst/stats、删 /lua/stats、加 `pub mod routes_stat;`）

- [ ] **Step 1: 建 `src/api/routes_stat.rs`**

```rust
//! 统一实例指标端点（M4）：`GET /api/v1/inst/stats`，合并进程级 state + 插件级 health。
//!
//! 响应 `{inst_id, app_code, host_kind, state, health}`：
//! - `state`：进程级 InstState（InstanceManager::status，死活权威）
//! - `health`：lux stat:<inst> JSON（核心+extra），无则 null
//! - `app_code`/`host_kind`：来自 InstanceManager configs（实例分类）
//!
//! 不带 inst_id → 全实例数组。默认 JWT 保护。

use crate::api::AppState;
use crate::types::HostKind;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct InstQuery {
    pub inst_id: Option<String>,
}

/// `GET /api/v1/inst/stats?inst_id=<inst>` 单查 / 不带参全列。
pub async fn stats(State(st): State<AppState>, Query(q): Query<InstQuery>) -> Response {
    let states = st.mgr.status();
    match q.inst_id {
        Some(inst_id) => {
            let state = states.get(&inst_id).copied();
            let health = crate::db::stat::read(&st.lux, &inst_id).await.ok().flatten();
            let class = st.mgr.config_snapshot(&inst_id);
            let (app_code, host_kind) = class
                .as_ref()
                .map(|c| (c.app_code.to_string(), host_kind_str(c.host_kind)))
                .unwrap_or((String::new(), ""));
            Json(serde_json::json!({
                "inst_id": inst_id,
                "app_code": app_code,
                "host_kind": host_kind,
                "state": state.map(state_str).unwrap_or_else(|| serde_json::Value::Null),
                "health": health,
            }))
            .into_response()
        }
        None => {
            let mut arr = Vec::with_capacity(states.len());
            for (inst_id, state) in states {
                let health = crate::db::stat::read(&st.lux, &inst_id)
                    .await
                    .ok()
                    .flatten();
                let class = st.mgr.config_snapshot(&inst_id);
                let (app_code, host_kind) = class
                    .as_ref()
                    .map(|c| (c.app_code.to_string(), host_kind_str(c.host_kind)))
                    .unwrap_or((String::new(), ""));
                arr.push(serde_json::json!({
                    "inst_id": inst_id,
                    "app_code": app_code,
                    "host_kind": host_kind,
                    "state": state_str(state),
                    "health": health,
                }));
            }
            Json(serde_json::Value::Array(arr)).into_response()
        }
    }
}

/// HostKind → 字符串（builtin/external）。
fn host_kind_str(k: HostKind) -> &'static str {
    match k {
        HostKind::Builtin => "builtin",
        HostKind::External => "external",
    }
}

/// InstState → JSON 字符串（与 InstState 的 serde lowercase 一致）。
fn state_str(s: crate::plugin::instance::InstState) -> serde_json::Value {
    serde_json::to_value(s).unwrap_or(serde_json::Value::Null)
}

// ---- 局部类型 ----
type Response = axum::response::Response;
```

- [ ] **Step 2: 删 `routes_lua::stats`（废弃 /lua/stats）**

在 `src/api/routes_lua.rs` 删除 `pub async fn stats(...)` 函数（约第 26-38 行）及其上面的 doc 注释。`InstQuery` 结构体保留（reload 用）。删除文件顶部不再需要的 `use`（`str_from_value` 若仅 stats 用则删；`EmbeddedValue` 若仅 stats 用则删——检查 reload 是否用，reload 用了 `str_from_value` + `EmbeddedValue`，故保留）。

- [ ] **Step 3: 注册路由 `src/api/mod.rs`**

`pub mod` 声明区（约第 17-24 行）加 `pub mod routes_stat;`。

`build_router`（约第 116-118 行）删除：
```rust
        .route("/api/v1/lua/stats", get(routes_lua::stats))
```

在同区（instance 端点附近）加：
```rust
        .route("/api/v1/inst/stats", get(routes_stat::stats))
```

- [ ] **Step 4: 验证编译（仍可能因 lua_client luastat 断裂）**

```bash
cargo check --bin rs-iot 2>&1 | grep -E "error|routes_stat|routes_lua" | head
```
Expected: routes_stat/routes_lua 本身无错（lua_client 的 luastat 错误见 Task 5）。

- [ ] **Step 5: Commit**

```bash
git add src/api/routes_stat.rs src/api/routes_lua.rs src/api/mod.rs
git commit -m "feat(m4): 统一 /api/v1/inst/stats 端点 + 废弃 /lua/stats"
```

---

### Task 5: lua 迁移到 StatTracker（恢复编译闭环）

**Files:**
- Modify: `src/plugin/builtin/lua_client/mod.rs`

- [ ] **Step 1: 读 lua_client/mod.rs 确认 stat 写入点**

```bash
grep -n "LuaStat\|luastat\|stat\." src/plugin/builtin/lua_client/mod.rs | head -40
```
确认 `let mut stat = LuaStat {...}` 定义点、collect 成功/失败处、reload 处、退出处。

- [ ] **Step 2: 替换 LuaStat → StatTracker**

把 `let mut stat = crate::db::luastat::LuaStat {...}` 定义替换为：

```rust
        let mut tracker = crate::plugin::stat_tracker::StatTracker::new(
            inst.inst_id.to_string(),
            ctx.lux.clone(),
        );
```

删除原 `let _ = crate::db::luastat::write(&ctx.lux, inst.inst_id.as_ref(), &stat).await;`（启动写一次）——StatTracker 启动不预写，靠周期 flush。

- [ ] **Step 3: collect 成功/失败 → tick_ok/tick_fail**

原 collect 成功处（约 `stat.collect_count += 1; stat.last_err.clear(); stat.status = "running"`）替换为：

```rust
                        tracker.tick_ok(started.elapsed().as_millis() as u64);
```

原 collect 失败处（约 `stat.collect_fail_count += 1; stat.last_err = e; ... stat.status = "degraded"`）替换为：

```rust
                        tracker.tick_fail(&e);
```

删除 `stat.last_collect_ts = now;` / `stat.last_collect_duration_ms = ...`（tick_ok 已含）。

- [ ] **Step 4: reload → extra_insert**

原 VM 重建处（`stat.vm_restart_count += 1; stat.script_version = ...`）替换为（需先在 StatTracker 定义后加 `let mut vm_restarts: u64 = 0;`）：

```rust
                        vm_restarts += 1;
                        tracker.extra_insert(
                            "vm_restart_count",
                            serde_json::json!(vm_restarts),
                        );
                        tracker.extra_insert(
                            "script_version",
                            serde_json::json!(script_version(&scripts_root, &cfg.script)),
                        );
```

注意：vm_restart_count 若要累计，需在 task 外维护一个 `let mut vm_restarts: u64 = 0;`，每次 reload `vm_restarts += 1; tracker.extra_insert("vm_restart_count", serde_json::json!(vm_restarts))`（上面占位处即填 `vm_restarts` 变量）。

- [ ] **Step 5: select! 加 flush ticker + 退出 flush**

在 lua 主 select! 加一个 flush 分支：

```rust
                _ = flush_ticker.tick() => { tracker.flush().await; }
```

并在循环前加 `let mut flush_ticker = tokio::time::interval(std::time::Duration::from_secs(crate::plugin::stat_tracker::FLUSH_INTERVAL_SECS));`

collect 后加 `if tracker.should_flush() { tracker.flush().await; }`。

退出处（原 `stat.status = "stopped"; ... luastat::write`）替换为：

```rust
        tracker.set_status("stopped");
        tracker.flush().await;
```

- [ ] **Step 6: 验证全量编译（恢复闭环）**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo test --lib
```
Expected: 零警告；lib 测试全过（lua_client 测试 + db::stat + stat_tracker）。此步后 luastat 引用全清，编译闭环恢复。

- [ ] **Step 7: Commit**

```bash
git add src/plugin/builtin/lua_client/mod.rs
git commit -m "refactor(m4): lua 迁移到 StatTracker（范例→通用样板）"
```

---

### Task 6: 6 约束文档 + CLAUDE.md 更新

**Files:**
- Create: `docs/instrumentation-conventions.md`
- Modify: `CLAUDE.md`

- [ ] **Step 1: 建 `docs/instrumentation-conventions.md`**

```markdown
# rs-iot 埋点规范与约束（M4）

本文件是所有实例指标（内置插件 / 外置插件 / 核心框架部件）的「宪法」。新增埋点必读。

## 1. 字段分层

- **核心 6 字段**（`InstanceStat`，编译期强制）：`status` / `tick_count` / `fail_count` / `last_activity_ts` / `last_activity_duration_ms` / `last_err`。
- **插件扩展**：走 `extra`（`#[serde(flatten)]` 透传）。加新插件**不改** `InstanceStat` struct。

## 2. 命名前瞻 Prometheus

- counter（单调增）用 `_count`；耗时 `_ms`；字节 `_bytes`。
- `status` 受控词表：`running` / `degraded` / `stopped`。
- 未来 `/metrics` 端点零重命名。

## 3. status 双轨制（不互相覆盖）

- 进程级 `state`（`InstState`：Starting/Running/Stopping/Stopped/Failed）= **死活权威**（supervisor 基于 JoinHandle）。
- 插件级 `status`（InstanceStat）= **health**（running/degraded）。
- REST 合并 `{state, health}`：Failed/Stopped 时 health 保留最后值，**不影响** state 判定。

## 4. 命名空间

- per-instance → lux key `stat:<inst_id>`（周期 flush）。
- 进程级 → 内存 `AtomicU64`（不落 lux，REST 直读）。

## 5. 写入频率

- per-instance：内存持有 + 周期 flush（默认每 5s 或每 50 次活动；常量见 `stat_tracker.rs`）。
- 进程级：atomic 不落盘（重启归零合理）。

## 6. 并发模型

- 跨 task 指标（hub/ctl/pipeline 进程级）→ `AtomicU64`。
- 单 task 内（插件采集循环）→ 普通字段（`StatTracker` 持有 `&mut self`，无需锁）。

## 接入清单

| 实例类型 | 接入方式 | tick 语义 |
|---|---|---|
| 内置插件（opcua/modbus/s7/datasim） | `StatTracker` | 采集周期成功/失败 |
| lua | `StatTracker`（已迁移） | collect 周期 |
| 外置（sidecarClient） | `StatTracker`（observed） | 收到 DataUp |
| 核心框架（pipeline/hub/ctl/mqtt） | `ProcessMetrics` atomic | 各自咽喉 |
```

- [ ] **Step 2: 更新 `CLAUDE.md`**

在「模块职责」表的 `plugin/` 行后或「常见流程」加一条埋点流程；在 REST 端点列表加 `/api/v1/inst/stats`、移除 `/api/v1/lua/stats`。具体：在「常见流程」末尾加：

```markdown
- **新增/修改插件埋点**：遵循 `docs/instrumentation-conventions.md` 6 约束。内置插件用 `StatTracker`（select! 加 flush ticker + tick_ok/fail）；进程级部件用 `ProcessMetrics`（atomic，/sysstatus 直读）。实例指标统一查 `GET /api/v1/inst/stats`（`{inst_id, app_code, host_kind, state, health}`，废弃的 `/lua/stats` 已移除）。
```

- [ ] **Step 3: Commit**

```bash
git add docs/instrumentation-conventions.md CLAUDE.md
git commit -m "docs(m4): 埋点 6 约束文档 + CLAUDE.md 更新"
```

**Phase 1 完成检查点**：`cargo test --all-targets --all-features` 全绿；`GET /api/v1/inst/stats?inst_id=<lua实例>` 返 `{state, health:{status, tick_count, ..., vm_restart_count, script_version}}`；`/api/v1/lua/stats` 返 404。

---

## Phase 2：核心框架咽喉

### Task 7: `ProcessMetrics` struct + 注入

**Files:**
- Create: `src/process_metrics.rs`
- Modify: `src/lib.rs`（`mod process_metrics;` + 构造 + 注入 HostContext）
- Modify: `src/host.rs`（HostContext 加 metrics 字段）
- Modify: `src/api/mod.rs`（AppState 加 metrics 字段）

- [ ] **Step 1: 建 `src/process_metrics.rs`**

```rust
//! 进程级指标（M4）：核心框架咽喉部位的 AtomicU64 计数器，REST /sysstatus 直读。
//!
//! 不落 lux（热路径无 await；进程级语义是本进程实时健康，重启归零合理）。
//! 注入 HostContext（所有 task 共享）+ AppState（REST 直读）。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

/// 进程级指标集合。
pub struct ProcessMetrics {
    /// 进程启动时刻（REST 算 uptime）。
    pub started: Instant,
    // 控制下行（CtlRegistry）
    pub ctl_send_count: AtomicU64,
    pub ctl_dropped_count: AtomicU64,
    pub ctl_not_found_count: AtomicU64,
    // 数据上行（pipeline）
    pub pipeline_processed_count: AtomicU64,
    pub pipeline_write_fail_count: AtomicU64,
    // hub 背压（gauge）
    pub hub_depth_last: AtomicU64,
    pub hub_depth_max: AtomicU64,
    // 北向（mqtt）
    pub mqtt_publish_count: AtomicU64,
    pub mqtt_publish_fail_count: AtomicU64,
    pub mqtt_connected: AtomicBool,
}

impl ProcessMetrics {
    /// 构造（所有计数器 0，started = now）。
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            ctl_send_count: AtomicU64::new(0),
            ctl_dropped_count: AtomicU64::new(0),
            ctl_not_found_count: AtomicU64::new(0),
            pipeline_processed_count: AtomicU64::new(0),
            pipeline_write_fail_count: AtomicU64::new(0),
            hub_depth_last: AtomicU64::new(0),
            hub_depth_max: AtomicU64::new(0),
            mqtt_publish_count: AtomicU64::new(0),
            mqtt_publish_fail_count: AtomicU64::new(0),
            mqtt_connected: AtomicBool::new(false),
        }
    }

    /// ctl_send_count +1（Relaxed）。
    pub fn ctl_send(&self) {
        self.ctl_send_count.fetch_add(1, Ordering::Relaxed);
    }
    /// ctl_dropped_count +1。
    pub fn ctl_dropped(&self) {
        self.ctl_dropped_count.fetch_add(1, Ordering::Relaxed);
    }
    /// ctl_not_found_count +1。
    pub fn ctl_not_found(&self) {
        self.ctl_not_found_count.fetch_add(1, Ordering::Relaxed);
    }
    /// pipeline 处理成功 +1。
    pub fn pipeline_processed(&self) {
        self.pipeline_processed_count.fetch_add(1, Ordering::Relaxed);
    }
    /// pipeline 写失败 +1。
    pub fn pipeline_write_fail(&self) {
        self.pipeline_write_fail_count.fetch_add(1, Ordering::Relaxed);
    }
    /// 更新 hub 深度 gauge（last + fetch_max）。
    pub fn hub_depth(&self, depth: usize) {
        let d = depth as u64;
        self.hub_depth_last.store(d, Ordering::Relaxed);
        self.hub_depth_max.fetch_max(d, Ordering::Relaxed);
    }
    /// mqtt publish 成功 +1。
    pub fn mqtt_publish(&self) {
        self.mqtt_publish_count.fetch_add(1, Ordering::Relaxed);
    }
    /// mqtt publish 失败 +1。
    pub fn mqtt_publish_fail(&self) {
        self.mqtt_publish_fail_count.fetch_add(1, Ordering::Relaxed);
    }
    /// 更新 mqtt 连接状态。
    pub fn mqtt_connected(&self, connected: bool) {
        self.mqtt_connected.store(connected, Ordering::Relaxed);
    }
}

impl Default for ProcessMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_increment() {
        let m = ProcessMetrics::new();
        m.ctl_send();
        m.ctl_send();
        m.ctl_dropped();
        assert_eq!(m.ctl_send_count.load(Ordering::Relaxed), 2);
        assert_eq!(m.ctl_dropped_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn hub_depth_tracks_max() {
        let m = ProcessMetrics::new();
        m.hub_depth(10);
        m.hub_depth(5);
        m.hub_depth(20);
        assert_eq!(m.hub_depth_last.load(Ordering::Relaxed), 20);
        assert_eq!(m.hub_depth_max.load(Ordering::Relaxed), 20);
    }
}
```

- [ ] **Step 2: HostContext 加 metrics 字段 `src/host.rs`**

在 `HostContext` struct（约第 122-135 行）加字段：

```rust
pub struct HostContext {
    pub lux: EmbeddedClient,
    pub hwid: crate::types::Hwid,
    pub shutdown: CancellationToken,
    pub ctl_registry: CtlRegistry,
    pub bridge_registry: BridgeRegistry,
    /// 进程级指标（M4）。
    pub metrics: std::sync::Arc<process_metrics::ProcessMetrics>,
}
```

`process_metrics` 模块在 lib.rs 声明（Step 4）。host.rs 引用用 `crate::process_metrics::ProcessMetrics`。

- [ ] **Step 3: AppState 加 metrics `src/api/mod.rs`**

```rust
pub struct AppState {
    pub lux: EmbeddedClient,
    pub mgr: Arc<InstanceManager>,
    pub hwid: crate::types::Hwid,
    pub jwt_secret: String,
    pub version: &'static str,
    pub metrics: std::sync::Arc<crate::process_metrics::ProcessMetrics>,
}
```

- [ ] **Step 4: lib.rs 声明模块 + 构造 + 注入**

`src/lib.rs` 模块声明区（约第 14-30 行）加 `pub mod process_metrics;`。

`run_app_with_shutdown`（约第 124-135 行）构造 HostContext 前加：

```rust
    let metrics = std::sync::Arc::new(process_metrics::ProcessMetrics::new());
```

`HostContext { ... }` 字面量加 `metrics: metrics.clone(),`。

`AppState { ... }`（约第 164 行）加 `metrics: metrics.clone(),`。

- [ ] **Step 5: 修复 CtlRegistry 构造（HostContext 改动连带）**

`CtlRegistry` 当前 `#[derive(Clone, Default, Debug)]`，HostContext 改动不影响它。但 HostContext 的构造点（lib.rs）已更新。**CtlRegistry::send 改造在 Task 8**。本步 HostContext 加 metrics 后，所有 `HostContext { ... }` 字面量都要加 metrics（lib.rs 一处）。grep 确认无其他构造点：

```bash
grep -rn "HostContext {" src/ tests/
```
仅 lib.rs 一处（测试里若用 HostContext 字面量需补——`instance_failed_e2e.rs` 用了字面量构造，补 `metrics: Arc::new(ProcessMetrics::new())`）。

- [ ] **Step 6: 验证**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo test --lib process_metrics
```
Expected: 零警告；process_metrics 测试过。修复所有 HostContext 字面量构造点。

- [ ] **Step 7: Commit**

```bash
git add src/process_metrics.rs src/lib.rs src/host.rs src/api/mod.rs tests/
git commit -m "feat(m4): ProcessMetrics 进程级指标 + 注入 HostContext/AppState"
```

---

### Task 8: CtlRegistry 改造（堵静默丢命令盲区）

**Files:**
- Modify: `src/host.rs`（CtlRegistry 加 metrics + send 计数+warn）
- Modify: `src/lib.rs`（CtlRegistry::new 签名）
- Test: `src/host.rs` 单测

- [ ] **Step 1: CtlRegistry 加 metrics 字段**

```rust
#[derive(Clone, Debug)]
pub struct CtlRegistry {
    map: Arc<DashMap<String, mpsc::Sender<CtlMessage>>>,
    metrics: Arc<crate::process_metrics::ProcessMetrics>,
}
```

去掉 `Default` derive（ProcessMetrics 无 Default 友好构造；改显式 new）。改 impl：

```rust
impl CtlRegistry {
    pub fn new(metrics: Arc<crate::process_metrics::ProcessMetrics>) -> Self {
        Self {
            map: Arc::new(DashMap::new()),
            metrics,
        }
    }
    // register/unregister/contains 不变
```

`send`（约第 41-50 行）改为：

```rust
    pub fn send(&self, inst: &str, msg: CtlMessage) -> bool {
        self.metrics.ctl_send();
        if let Some(ref_tx) = self.map.get(inst) {
            match ref_tx.value().try_send(msg) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    self.metrics.ctl_dropped();
                    tracing::warn!("[ctl] {inst} channel full, command dropped");
                    true
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    self.metrics.ctl_dropped();
                    true
                }
            }
        } else {
            self.metrics.ctl_not_found();
            false
        }
    }
```

- [ ] **Step 2: 修复 CtlRegistry::new 调用点**

```bash
grep -rn "CtlRegistry::new" src/
```
`src/lib.rs`（约第 124 行）改为：

```rust
    let ctl_registry = host::CtlRegistry::new(metrics.clone());
```
（注意：此行必须在 `let metrics = Arc::new(...)` 之后。lib.rs 当前顺序是先 ctl_registry 后 ctx，需调整：metrics 构造提到 ctl_registry 之前。）

- [ ] **Step 3: 加单测**

在 `src/host.rs` 末尾加：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::CtlMessage;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    fn registry() -> CtlRegistry {
        CtlRegistry::new(Arc::new(crate::process_metrics::ProcessMetrics::new()))
    }

    #[tokio::test]
    async fn send_counts_and_returns_semantics_unchanged() {
        let r = registry();
        let (tx, mut rx) = mpsc::channel::<CtlMessage>(1);
        r.register("a", tx);
        // 未注册实例 → false + not_found++
        assert!(!r.send("nope", dummy()));
        assert_eq!(r.metrics.ctl_not_found_count.load(Ordering::Relaxed), 1);
        // 命中 + Ok → true + send++
        assert!(r.send("a", dummy()));
        assert!(rx.recv().await.is_some(), "命令应送达");
        assert_eq!(r.metrics.ctl_send_count.load(Ordering::Relaxed), 2);
        // 命中 + channel 满 → true + dropped++
        let (tx2, _rx2) = mpsc::channel::<CtlMessage>(1);
        r.register("full", tx2);
        assert!(r.send("full", dummy()), "满丢仍返 true（对标 Go default）");
        assert!(r.send("full", dummy()), "第二次满");
        assert_eq!(r.metrics.ctl_dropped_count.load(Ordering::Relaxed), 1);
    }

    fn dummy() -> CtlMessage {
        CtlMessage {
            instance: "x".into(),
            devid: "d".into(),
            cmd: "write".into(),
            data: String::new(),
        }
    }
}
```

（`CtlMessage` 字段以 `types.rs` 实际为准——若字段不同，调整 dummy。）

- [ ] **Step 4: 验证**

```bash
cargo test --lib host::tests
cargo clippy --all-targets --all-features -- -D warnings
```
Expected: send 计数 + 返回语义测试过；零警告。

- [ ] **Step 5: Commit**

```bash
git add src/host.rs src/lib.rs
git commit -m "feat(m4): CtlRegistry 计数+warn（堵静默丢命令盲区，返回语义不变）"
```

---

### Task 9: pipeline 计数 + hub 深度 gauge

**Files:**
- Modify: `src/pipeline.rs`

- [ ] **Step 1: pipeline.rs run() 加计数 + hub 深度**

`run` 函数的 `sample = hub_rx.recv()` 匹配 `Some(s)` 后，处理完加计数。修改 `pipeline.rs`（约第 32-50 行）：

```rust
                sample = hub_rx.recv() => {
                    let sample = match sample {
                        Some(s) => s,
                        None => {
                            tracing::info!("[pipeline] hub closed, exiting");
                            break;
                        }
                    };
                    // hub 背压 gauge：消费后采样当前队列深度
                    ctx.metrics.hub_depth(hub_rx.len());
                    let processed = match processor.process(sample).await {
                        Some(p) => p,
                        None => continue,
                    };
                    match db::rtdata::write(
                        &ctx.lux, processed.hwid.as_ref(), &processed.devid,
                        &processed.values, processed.timestamp,
                    ).await {
                        Ok(()) => ctx.metrics.pipeline_processed(),
                        Err(e) => {
                            ctx.metrics.pipeline_write_fail();
                            tracing::warn!("[pipeline] rtdata write failed: {e}");
                        }
                    }
                }
```

- [ ] **Step 2: 验证**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo test --test dataflow
```
Expected: 零警告；dataflow 集成测仍过（pipeline 行为不变，只加计数）。

- [ ] **Step 3: Commit**

```bash
git add src/pipeline.rs
git commit -m "feat(m4): pipeline processed/write_fail 计数 + hub 深度 gauge"
```

---

### Task 10: `/sysstatus` 扩展（metrics + uptime）

**Files:**
- Modify: `src/api/routes_sys.rs`

- [ ] **Step 1: sysstatus 加 metrics + uptime**

```rust
pub async fn sysstatus(State(st): State<AppState>) -> Json<serde_json::Value> {
    let dbsize = match st.lux.execute_value("DBSIZE", &[]).await {
        Ok(EmbeddedValue::Int(n)) => n,
        _ => 0,
    };
    let m = &st.metrics;
    Json(serde_json::json!({
        "status": "ok",
        "dbsize": dbsize,
        "instances": st.mgr.status(),
        "metrics": {
            "ctl_send_count": m.ctl_send_count.load(std::sync::atomic::Ordering::Relaxed),
            "ctl_dropped_count": m.ctl_dropped_count.load(std::sync::atomic::Ordering::Relaxed),
            "ctl_not_found_count": m.ctl_not_found_count.load(std::sync::atomic::Ordering::Relaxed),
            "pipeline_processed_count": m.pipeline_processed_count.load(std::sync::atomic::Ordering::Relaxed),
            "pipeline_write_fail_count": m.pipeline_write_fail_count.load(std::sync::atomic::Ordering::Relaxed),
            "hub": {
                "depth_last": m.hub_depth_last.load(std::sync::atomic::Ordering::Relaxed),
                "depth_max": m.hub_depth_max.load(std::sync::atomic::Ordering::Relaxed),
            },
            "mqtt": {
                "publish_count": m.mqtt_publish_count.load(std::sync::atomic::Ordering::Relaxed),
                "publish_fail_count": m.mqtt_publish_fail_count.load(std::sync::atomic::Ordering::Relaxed),
                "connected": m.mqtt_connected.load(std::sync::atomic::Ordering::Relaxed),
            },
        },
        "process": {
            "uptime_secs": m.started.elapsed().as_secs(),
        }
    }))
}
```

- [ ] **Step 2: 验证**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo test --test api_flow
```
Expected: 零警告；api_flow 过（sysstatus 仍返 200，字段增多不破坏）。

- [ ] **Step 3: Commit**

```bash
git add src/api/routes_sys.rs
git commit -m "feat(m4): /sysstatus 扩展进程级 metrics + uptime"
```

**Phase 2 完成检查点**：`GET /api/v1/sysstatus` 返 `metrics.{ctl_*, pipeline_*, hub, mqtt}` + `process.uptime_secs`；填满 cmd channel 后 send，`ctl_dropped_count` 增长 + 日志见 `[ctl] ... dropped`。

---

## Phase 3：内置插件接入

### Task 11: datasim 接入 StatTracker（样板）

**Files:**
- Modify: `src/plugin/builtin/data_sim/mod.rs`

- [ ] **Step 1: run() 加 StatTracker + flush ticker**

`DataSim::run`（约第 25-95 行）在 `let mut ticker = ...` 后加：

```rust
        let mut tracker = crate::plugin::stat_tracker::StatTracker::new(
            inst.inst_id.to_string(),
            ctx.lux.clone(),
        );
        let mut flush_ticker = tokio::time::interval(std::time::Duration::from_secs(
            crate::plugin::stat_tracker::FLUSH_INTERVAL_SECS,
        ));
```

select!（约第 57-93 行）改造——`token.cancelled` 分支加 flush，`ticker.tick` 分支加 tick_ok：

```rust
            tokio::select! {
                _ = token.cancelled() => {
                    tracing::info!("[dataSim] inst {} stopping", inst.inst_id);
                    tracker.set_status("stopped");
                    tracker.flush().await;
                    break;
                }
                msg = cmd_rx.recv() => {
                    if let Some(m) = msg {
                        tracing::info!("[dataSim] inst {} cmd: {} (devid={})", inst.inst_id, m.cmd, m.devid);
                    }
                }
                _ = flush_ticker.tick() => { tracker.flush().await; }
                _ = ticker.tick() => {
                    let started = std::time::Instant::now();
                    let now = chrono::Local::now().timestamp();
                    for dev in &devices {
                        // ... 原 values 构造不变 ...
                        let sample = Sample { /* 不变 */ };
                        if tx.send(sample).await.is_err() {
                            break;
                        }
                    }
                    tracker.tick_ok(started.elapsed().as_millis() as u64);
                    if tracker.should_flush() { tracker.flush().await; }
                }
            }
```

（保留原 values 构造逻辑，只在末尾加 tick_ok。）

- [ ] **Step 2: 验证**

```bash
cargo test --lib plugin::builtin::data_sim
cargo clippy --all-targets --all-features -- -D warnings
```
Expected: 零警告；data_sim 现有测试过。

- [ ] **Step 3: Commit**

```bash
git add src/plugin/builtin/data_sim/mod.rs
git commit -m "feat(m4): dataSim 接入 StatTracker（接入样板）"
```

---

### Task 12: opcua 接入 StatTracker

**Files:**
- Modify: `src/plugin/builtin/opcua_client/mod.rs`

- [ ] **Step 1: run() 加 StatTracker + session_reconnect extra**

`OpcuaClient::run`（约第 272-410 行）在 `let mut ticker = ...` 后加（同 datasim 模式）：

```rust
        let mut tracker = crate::plugin::stat_tracker::StatTracker::new(
            inst.inst_id.to_string(),
            ctx.lux.clone(),
        );
        let mut reconnects: u64 = 0;
        let mut flush_ticker = tokio::time::interval(std::time::Duration::from_secs(
            crate::plugin::stat_tracker::FLUSH_INTERVAL_SECS,
        ));
```

select!（约第 329-400 行）改造：

```rust
            tokio::select! {
                _ = token.cancelled() => {
                    tracing::info!("[opcuaClient] inst {inst_id} stopping");
                    tracker.set_status("stopped");
                    tracker.flush().await;
                    break;
                }
                msg = cmd_rx.recv() => { /* 原样不变 */ }
                _ = flush_ticker.tick() => { tracker.flush().await; }
                _ = ticker.tick() => {
                    if session.is_none() {
                        match connect(&cfg).await {
                            Ok((s, h)) => {
                                reconnects += 1;
                                tracker.extra_insert("session_reconnect_count", serde_json::json!(reconnects));
                                tracing::info!("[opcuaClient] inst {inst_id} connected {}", cfg.endpoint);
                                session = Some(s);
                                ev_handle = Some(h);
                            }
                            Err(e) => {
                                tracker.tick_fail(&format!("connect failed: {e}"));
                                tracing::warn!("[opcuaClient] inst {inst_id} connect failed: {e}");
                                if tracker.should_flush() { tracker.flush().await; }
                                continue;
                            }
                        }
                    }
                    let s = match &session {
                        Some(s) => s.clone(),
                        None => continue,
                    };
                    let started = std::time::Instant::now();
                    let mut tick_ok = true;
                    for (devid, pairs) in &devices {
                        // ... 原 read 逻辑 ...
                        match s.read(&nodes, TimestampsToReturn::Both, 0.0).await {
                            Ok(results) => { /* 原样构造 sample + tx.send */ }
                            Err(e) => {
                                tick_ok = false;
                                tracker.tick_fail(&format!("read failed: {e:?}"));
                                tracing::warn!("[opcuaClient] inst {inst_id} read failed: {e:?}");
                                session = None;
                                if let Some(h) = ev_handle.take() { h.abort(); }
                                break;
                            }
                        }
                    }
                    if tick_ok {
                        tracker.tick_ok(started.elapsed().as_millis() as u64);
                    }
                    if tracker.should_flush() { tracker.flush().await; }
                }
            }
```

（`subscription_alive` extra 暂不接入——async-opcua 订阅回调结构复杂，v1 只接 session_reconnect_count；subscription_alive 后置。）

- [ ] **Step 2: 验证**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo test --lib plugin::builtin::opcua_client
```
Expected: 零警告；opcua 现有解析测试过。

- [ ] **Step 3: Commit**

```bash
git add src/plugin/builtin/opcua_client/mod.rs
git commit -m "feat(m4): opcuaClient 接入 StatTracker（tick + session_reconnect_count）"
```

---

### Task 13: modbus 接入 StatTracker

**Files:**
- Modify: `src/plugin/builtin/modbus_client/mod.rs`

- [ ] **Step 1: run() 加 StatTracker + exception_count extra**

`ModbusClient::run`（约第 286-441 行）在 `let mut ticker = ...` 后加：

```rust
        let mut tracker = crate::plugin::stat_tracker::StatTracker::new(
            inst.inst_id.to_string(),
            ctx.lux.clone(),
        );
        let mut exceptions: u64 = 0;
        let mut flush_ticker = tokio::time::interval(std::time::Duration::from_secs(
            crate::plugin::stat_tracker::FLUSH_INTERVAL_SECS,
        ));
```

select!（约第 318-323 行）加 flush 分支：

```rust
            let evt = tokio::select! {
                _ = token.cancelled() => {
                    tracker.set_status("stopped");
                    tracker.flush().await;
                    break;
                }
                msg = cmd_rx.recv() => Evt::Cmd(msg),
                _ = flush_ticker.tick() => { tracker.flush().await; continue; }
                _ = ticker.tick() => Evt::Tick,
            };
```

`Evt::Tick` 分支末尾（约第 416-435 行，transport_failed 判断后）加 tick。在 `ReadOutcome::Exception` 处 `exceptions += 1`：

```rust
                            ReadOutcome::Exception(exc) => {
                                exceptions += 1;
                                tracker.extra_insert("exception_count", serde_json::json!(exceptions));
                                tracing::warn!( /* 原样 */ );
                            }
```

`Evt::Tick` 分支开头（`let now = chrono::Local::now().timestamp();` 旁）先加 `let tick_started = std::time::Instant::now();`，分支末尾（acc 发送 sample 之后）加：

```rust
                    if transport_failed {
                        tracker.tick_fail("transport failed");
                    } else {
                        tracker.tick_ok(tick_started.elapsed().as_millis() as u64);
                    }
                    tracker.extra_insert("exception_count", serde_json::json!(exceptions));
                    if tracker.should_flush() { tracker.flush().await; }
```

（modbus 单 tick 含多 slave region，tick 语义=「一轮轮询」。transport_failed→fail，否则 ok。）

- [ ] **Step 2: 验证**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo test --lib plugin::builtin::modbus_client
```
Expected: 零警告；modbus 现有测试过。

- [ ] **Step 3: Commit**

```bash
git add src/plugin/builtin/modbus_client/mod.rs
git commit -m "feat(m4): modbusClient 接入 StatTracker（tick + exception_count）"
```

---

### Task 14: s7 接入 StatTracker

**Files:**
- Modify: `src/plugin/builtin/s7_client/mod.rs`

- [ ] **Step 1: run() 加 StatTracker + reconnect_count extra**

`S7Client::run`（约第 316-464 行）在 `let mut ticker = ...` 后加：

```rust
        let mut tracker = crate::plugin::stat_tracker::StatTracker::new(
            inst.inst_id.to_string(),
            ctx.lux.clone(),
        );
        let mut reconnects: u64 = 0;
        let mut flush_ticker = tokio::time::interval(std::time::Duration::from_secs(
            crate::plugin::stat_tracker::FLUSH_INTERVAL_SECS,
        ));
```

select!（约第 370-457 行）改造（同 opcua 模式）：

```rust
            tokio::select! {
                _ = token.cancelled() => {
                    tracing::info!("[s7Client] inst {inst_id} stopping");
                    tracker.set_status("stopped");
                    tracker.flush().await;
                    break;
                }
                msg = cmd_rx.recv() => { /* 原样 */ }
                _ = flush_ticker.tick() => { tracker.flush().await; }
                _ = ticker.tick() => {
                    if client.is_none() {
                        match connect(&cfg).await {
                            Ok(c) => {
                                reconnects += 1;
                                tracker.extra_insert("reconnect_count", serde_json::json!(reconnects));
                                tracing::info!("[s7Client] inst {inst_id} connected {}", cfg.endpoint);
                                client = Some(c);
                            }
                            Err(e) => {
                                tracker.tick_fail(&format!("connect failed: {e}"));
                                tracing::warn!("[s7Client] inst {inst_id} connect failed: {e}");
                                if tracker.should_flush() { tracker.flush().await; }
                                continue;
                            }
                        }
                    }
                    let c = match &client { Some(c) => c, None => continue };
                    let started = std::time::Instant::now();
                    let mut tick_ok = true;
                    for (devid, pts) in &devices {
                        // ... 原 build_request + read 逻辑 ...
                        match c.read(items).await {
                            Ok(data_items) => { /* 原样构造 sample + tx.send */ }
                            Err(e) => {
                                tick_ok = false;
                                tracker.tick_fail(&format!("read failed: {e}"));
                                tracing::warn!("[s7Client] inst {inst_id} read failed: {e}");
                                if e.is_connection_error() { client = None; }
                                break;
                            }
                        }
                    }
                    if tick_ok { tracker.tick_ok(started.elapsed().as_millis() as u64); }
                    if tracker.should_flush() { tracker.flush().await; }
                }
            }
```

- [ ] **Step 2: 验证**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo test --lib plugin::builtin::s7_client
```
Expected: 零警告；s7 现有测试过。

- [ ] **Step 3: Commit**

```bash
git add src/plugin/builtin/s7_client/mod.rs
git commit -m "feat(m4): s7Client 接入 StatTracker（tick + reconnect_count）"
```

**Phase 3 完成检查点**：起 opcua/modbus/s7/datasim 实例（端到端或单测），`GET /api/v1/inst/stats?inst_id=<各>` 返 health 含真实 tick_count（随采集增长）+ 各自 extra 字段。

---

## Phase 4：外置插件 + 进程资源

### Task 15: sidecarClient observed StatTracker

**Files:**
- Modify: `src/plugin/builtin/sidecar_client/mod.rs`

- [ ] **Step 1: run() 加 StatTracker（observed 语义）**

`SidecarClient::run`（约第 201-369 行）在 `ctx.bridge_registry.register` 后加：

```rust
        let mut tracker = crate::plugin::stat_tracker::StatTracker::new(
            inst_id.clone(),
            ctx.lux.clone(),
        );
        let mut flush_ticker = tokio::time::interval(std::time::Duration::from_secs(
            crate::plugin::stat_tracker::FLUSH_INTERVAL_SECS,
        ));
        let mut reconnects: u64 = 0;
```

- [ ] **Step 2: 连接生命周期 → extra（connected/reconnect）**

`'outer` 循环拿 stream 成功后（握手前）加：

```rust
            reconnects += 1;
            tracker.extra_insert("reconnect_count", serde_json::json!(reconnects));
            tracker.extra_insert("connected", serde_json::json!(true));
```

内层 loop 的 select! 加 flush 分支，并在各 break（连接断）处前设 `tracker.extra_insert("connected", json!(false))`。内层 select!（约第 282-362 行）：

```rust
                tokio::select! {
                    _ = token.cancelled() => { tracker.set_status("stopped"); tracker.flush().await; break 'outer }
                    m = cmd_rx.recv() => { /* 原样 */ }
                    _ = flush_ticker.tick() => { tracker.flush().await; }
                    res = reader.next() => {
                        match res {
                            Some(Ok(bytes)) => match decode(&bytes) {
                                Ok(Recv::DataUp(d)) => {
                                    last_hb = Instant::now();
                                    // ... 原 by_ts 合并 + tx.send ...
                                    tracker.tick_ok(0);  // observed：收到 DataUp 即一次成功 tick
                                    if tracker.should_flush() { tracker.flush().await; }
                                }
                                Ok(Recv::CommandAck(a)) => { /* 原样 */ }
                                Ok(Recv::Heartbeat) => {
                                    last_hb = Instant::now();
                                    tracker.extra_insert("last_heartbeat_ts", serde_json::json!(chrono::Local::now().timestamp()));
                                }
                                Ok(Recv::Hello(_)) => { /* 原样 */ }
                                Err(e) => {
                                    tracker.tick_fail(&format!("bad frame: {e}"));
                                    tracing::warn!("[sidecarClient] inst {inst_id} bad frame: {e}");
                                }
                            }
                            Some(Err(e)) => { tracker.tick_fail(&format!("read err: {e}")); tracing::warn!(/* 原样 */); break; }
                            None => { tracker.set_status("degraded"); tracker.flush().await; break; }
                        }
                    }
                    _ = ticker.tick() => {
                        if last_hb.elapsed() > hb_timeout {
                            tracker.set_status("degraded");
                            tracker.flush().await;
                            tracing::warn!("[sidecarClient] inst {inst_id} heartbeat timeout, disconnect");
                            break;
                        }
                    }
                }
```

`'outer` 退出前加 `tracker.set_status("stopped"); tracker.flush().await;`。

- [ ] **Step 3: 验证**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo test --test sidecar_e2e
```
Expected: 零警告；sidecar_e2e 完整闭环仍过（DataUp→rtdata、写命令→Ack、错误 token 拒绝）。验证 sidecar 实例 `/inst/stats` health 非 null（扩 sidecar_e2e 加断言）。

- [ ] **Step 4: 扩 sidecar_e2e 断言 stat**

在 `tests/sidecar_e2e.rs` 一个测试末尾，send DataUp 后查 `stat:<inst>`：

```rust
    // observed stat：DataUp 后 tick_count >= 1
    let stat_v = client.execute_value("GET", &[&format!("stat:{inst_id}")]).await.unwrap();
    assert!(matches!(stat_v, lux::EmbeddedValue::Bulk(_) | lux::EmbeddedValue::Simple(_)));
```
（需等 flush_ticker 触发或直接调 send_ctl 后等 5s；或测试中手动触发——简单起见，断言 stat key 存在即可。）

- [ ] **Step 5: Commit**

```bash
git add src/plugin/builtin/sidecar_client/mod.rs tests/sidecar_e2e.rs
git commit -m "feat(m4): sidecarClient observed StatTracker（DataUp→tick，connected/heartbeat extra）"
```

---

### Task 16: mqtt_pub 计数 + connected

**Files:**
- Modify: `src/plugin/builtin/mqtt_pub/mod.rs`

- [ ] **Step 1: run() 用 ctx.metrics 计数**

`MqttPub::run`（约第 30-83 行）在 subscribe 后加初始 connected 状态。select!（约第 56-81 行）改造：

```rust
                ev = connection.poll() => {
                    match ev {
                        Ok(Event::Incoming(Packet::Publish(p))) => {
                            if let Ok(cmd) = serde_json::from_slice::<CtlMessage>(&p.payload) {
                                let inst = cmd.instance.to_string();
                                ctx.ctl_registry.send(&inst, cmd);
                            }
                        }
                        Ok(Event::Incoming(Packet::ConnAck(_))) => {
                            ctx.metrics.mqtt_connected(true);
                            tracing::info!("[mqtt] connected to broker");
                        }
                        Ok(_) => {}
                        Err(e) => {
                            ctx.metrics.mqtt_connected(false);
                            tracing::warn!("[mqtt] connection error: {e}");
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                    }
                }
                _ = ticker.tick() => {
                    publish_rtdata(&ctx, &hwid, &client, &ctx.metrics).await;
                }
```

`publish_rtdata`（约第 89-125 行）加 metrics 参数 + publish 成败计数：

```rust
async fn publish_rtdata(
    ctx: &HostContext,
    hwid: &crate::types::Hwid,
    client: &AsyncClient,
    metrics: &crate::process_metrics::ProcessMetrics,
) {
    // ... 原 keys/HGETALL 循环 ...
            Ok(bytes) => {
                match client.publish(topic.clone(), QoS::AtMostOnce, false, bytes).await {
                    Ok(()) => metrics.mqtt_publish(),
                    Err(e) => { metrics.mqtt_publish_fail(); tracing::warn!("[mqtt] publish {topic} failed: {e}"); }
                }
            }
            Err(e) => tracing::warn!("[mqtt] encode payload failed: {e}"),
    // ...
}
```

- [ ] **Step 2: 验证**

```bash
cargo clippy --all-targets --all-features -- -D warnings
```
Expected: 零警告（mqtt_pub 无单测，靠 clippy + 编译；行为靠端到端）。

- [ ] **Step 3: Commit**

```bash
git add src/plugin/builtin/mqtt_pub/mod.rs
git commit -m "feat(m4): mqttPub publish 计数 + connected 状态"
```

---

### Task 17: 进程资源（sysinfo CPU/mem）

**Files:**
- Modify: `src/api/routes_sys.rs`

- [ ] **Step 1: sysstatus process 加 cpu/mem（sysinfo 现采）**

`routes_sys.rs` 顶部加 `use sysinfo::System;`。`sysstatus` 的 `process` 字段改为：

```rust
        "process": {
            let mut sys = System::new_all();
            sys.refresh_all();
            let pid = sysinfo::get_current_pid().unwrap_or(sysinfo::Pid::from(0));
            let (cpu, mem) = sys.process(pid)
                .map(|p| (p.cpu_usage(), p.memory()))
                .unwrap_or((0.0, 0));
            serde_json::json!({
                "uptime_secs": m.started.elapsed().as_secs(),
                "cpu_pct": cpu,
                "mem_bytes": mem,
            })
        }
```

（`sysinfo` 已是依赖（Cargo.toml:32）。`mem_bytes` 是 sysinfo 的 memory()（字节）。CPU 是 0-100 进程占用百分比。）

- [ ] **Step 2: 验证**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo test --test api_flow
```
Expected: 零警告；api_flow 过。

- [ ] **Step 3: Commit**

```bash
git add src/api/routes_sys.rs
git commit -m "feat(m4): /sysstatus 加进程 CPU/mem（sysinfo 现采）"
```

**Phase 4 完成检查点**：sidecar 实例 `/inst/stats` health 非 null（observed tick）；`/sysstatus.metrics.mqtt` 含 publish_count/connected；`/sysstatus.process` 含 cpu_pct/mem_bytes。

---

## 全量验收（所有 Phase 完成后）

- [ ] `cargo fmt --all --check`
- [ ] `CARGO_BUILD_JOBS=1 cargo test --all-targets --all-features`（全绿）
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`（零警告）
- [ ] 端到端：起 lux + datasim 实例 → `GET /api/v1/inst/stats` 返 health 非 null + app_code=dataSim/host_kind=builtin
- [ ] 端到端：`GET /api/v1/sysstatus` 含 metrics.ctl_*/pipeline_*/hub/mqtt + process.cpu_pct/mem_bytes
- [ ] 废弃验证：`GET /api/v1/lua/stats` → 404；`GET /api/v1/lua/reload` 仍可用
- [ ] CtlRegistry 盲区：填满 cmd channel 后 send_ctl → `/sysstatus` ctl_dropped_count 增长 + 日志见 `[ctl] ... dropped`
