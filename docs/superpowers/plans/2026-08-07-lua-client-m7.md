# Lua 脚本采集 — M7 状态监测与 REST 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development 实现本计划。步骤用 checkbox（`- [ ]`）。由 `/loop` 调度（CronCreate `116fd1e9`，每 10m 一轮）。

**Goal:** 补齐 luaClient 状态监测与 REST 管理——run() 内采点（collect 成败/耗时 + 热加载次数 + 脚本版本）写 lux `stat:<inst_id>`；新增 `routes_lua.rs` 三端点（`GET /api/v1/lua/stats`、`POST /api/v1/lua/reload`、`GET /api/v1/lua/scripts`，JWT 保护）。spec §13（状态监测）+ §4（routes_lua）。

**Architecture:** 状态快照 `LuaStat` 结构存 `src/db/luastat.rs`（lux string key `stat:<inst>` SET/GET JSON，与 luakv.rs 同构）。luaClient `run()` 主循环 Evt 分支内采点（启动 running / Tick collect 计数 / Reload vm_restart+1 / 退出 stopped），每次变更 `luastat::write`。REST reload = `InstanceManager::stop + start_source(config_from_lux)`（复用 routes_sys::startinst 从 lux `instances` 读 config 的模式；spec §5.3「VM 替换 ≈ 轻量重启 instance」）。REST 用 query 参数（`?inst_id=`，与 routes_inst `IdQuery` 一致）。

**Tech Stack:** lux EmbeddedClient（SET/GET/HGET）、serde（LuaStat JSON）、sha2（script_version）、axum 0.8（Query/State/Json handler + tower middleware）。

**执行顺序：** Task 1（luastat 模块）→ Task 2（config fail_threshold）→ Task 3（mod.rs 采点 + script_version）→ Task 4（routes_lua + 注册）→ Task 5（集成测）。Task 1-4 各自 TDD 单测先行；Task 5 端到端覆盖。

**已知简化 / 风险（注明）：**
- **`io_connected` / `last_io_err` v1 不实现**（spec §13 列出）：io_engine 当前是黑盒 `IoEngine`，不暴露连接状态；需 io_engine 改造（后置 Plan 7/M8）。stat v1 字段集 = status / collect_count / collect_fail_count / last_collect_ts / last_collect_duration_ms / vm_restart_count / script_version / last_err。
- **REST reload = stop + start（IO 重连）**，非 notify 自动热加载的 VM 替换（不重启 IO）。两条路径结果一致（新脚本生效），实现路径不同。spec §5.3 明确「VM 替换 ≈ 轻量重启 instance」，故 REST reload 走 stop+start 合理。手动 reload 低频，IO 重连中断可接受。
- **REST 用 query 参数**（`?inst_id=`），非 spec 字面路径参数 `<inst>`：避 axum 0.8 路径参数语法（`{param}` vs `:param`）歧义 + 贴 routes_inst `IdQuery` 约定（CLAUDE.md「匹配代码库约定」优先）。
- **script_version = main.lua sha256 前 8 hex**（单文件，不含 lib/*.lua）：轻量、启动一次算。脚本版本管理 v1 靠 git + 热加载（spec §17 v2 后置）。
- **stat 按 collect 周期计**（一次 Tick 一次计数）：多 dev 实例 `for dev { call_collect }` 时，任一 collect 失败即记 fail 周期（保守，degraded 更敏感）；collect 通常单 dev，多 dev 罕见。
- **degraded 恢复**：collect 成功或 reload 成功 → consecutive_fail 重置 0、status 回 running。

---

## File Structure

| 文件 | 动作 | 职责 |
|---|---|---|
| `src/db/luastat.rs` | **Create** | `LuaStat` 结构 + `write`/`read`（lux `stat:<inst>` SET/GET JSON）+ 单测 |
| `src/db/mod.rs` | Modify | `pub mod luastat;`（引出新模块） |
| `src/db/identity.rs` | Modify | hash 清单注释加 `stat:<instId>` |
| `src/plugin/builtin/lua_client/config.rs` | Modify | `LuaCfg` 加 `fail_threshold: u32` + parse + 单测 |
| `src/plugin/builtin/lua_client/mod.rs` | Modify | run() stat 采点（启动/Tick/Reload/退出）+ `script_version` helper + 单测 |
| `src/api/routes_lua.rs` | **Create** | `stats`/`reload`/`list_scripts` 三 handler + `scan_scripts` 纯函数 + 单测 |
| `src/api/mod.rs` | Modify | `pub mod routes_lua;` + `build_router` 注册三路由 |
| `tests/lua_stat_e2e.rs` | **Create** | stat 采集端到端（InstanceManager 起实例 → collect → 验 `stat:<inst>`） |
| `tests/lua_api_e2e.rs` | **Create** | REST 端点集成测（stats/reload/scripts，router oneshot + JWT） |

---

## Task 1: db/luastat.rs（LuaStat + write/read）

**Files:** Create `src/db/luastat.rs`; Modify `src/db/mod.rs`; Modify `src/db/identity.rs`

### - [x] Step 1：写 luastat.rs（含单测，TDD 先行）

新建 `src/db/luastat.rs`：
```rust
//! Lua 实例状态快照（spec §13）：lux string key `stat:<inst_id>` 存 JSON。
//!
//! luaClient `run()` 内采点（collect 成败/耗时 + 热加载次数 + 脚本版本），每次 collect/reload/
//! 启停写一次。REST `GET /api/v1/lua/stats?inst_id=<inst>` 读回（`routes_lua::stats`）。
//!
//! v1 不含 `io_connected`/`last_io_err`（io_engine 未暴露连接状态，后置 Plan 7/M8）。

use crate::error::RsIotError;
use lux::{EmbeddedClient, EmbeddedValue};

/// 实例状态快照（写 lux `stat:<inst_id>`）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LuaStat {
    /// `running` / `degraded` / `stopped`。
    pub status: String,
    /// 累计 collect 成功周期数。
    pub collect_count: u64,
    /// 累计 collect 失败周期数。
    pub collect_fail_count: u64,
    /// 最近一次 collect 的墙上时间（unix 秒）。
    pub last_collect_ts: i64,
    /// 最近一次 collect 耗时（毫秒）。
    pub last_collect_duration_ms: u64,
    /// VM 重建次数（热加载/崩溃恢复）。
    pub vm_restart_count: u64,
    /// 脚本版本（main.lua sha256 前 8 hex）。
    pub script_version: String,
    /// 最近一次失败原因（成功时清空）。
    pub last_err: String,
}

/// key 名 `stat:<inst_id>`。
fn key(inst_id: &str) -> String {
    format!("stat:{inst_id}")
}

/// 写状态快照（JSON，`SET stat:<inst>`）。
pub async fn write(c: &EmbeddedClient, inst_id: &str, stat: &LuaStat) -> Result<(), RsIotError> {
    let json = serde_json::to_string(stat)
        .map_err(|e| RsIotError::Config(format!("LuaStat 序列化失败：{e}")))?;
    c.execute("SET", &[&key(inst_id), &json]).await?;
    Ok(())
}

/// 读状态快照（无则 `None`）。
pub async fn read(c: &EmbeddedClient, inst_id: &str) -> Result<Option<LuaStat>, RsIotError> {
    let v = c.execute_value("GET", &[&key(inst_id)]).await?;
    match v {
        EmbeddedValue::Bulk(b) => {
            let s = String::from_utf8_lossy(&b);
            if s.is_empty() {
                return Ok(None);
            }
            let stat: LuaStat = serde_json::from_str(&s)
                .map_err(|e| RsIotError::Config(format!("LuaStat 反序列化失败：{e}")))?;
            Ok(Some(stat))
        }
        EmbeddedValue::Simple(s) if !s.is_empty() => {
            let stat: LuaStat = serde_json::from_str(&s)
                .map_err(|e| RsIotError::Config(format!("LuaStat 反序列化失败：{e}")))?;
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
                    .join(format!("luastat-{}", std::process::id()))
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

    /// 构造一个非空快照（覆盖全部字段）。
    fn sample() -> LuaStat {
        LuaStat {
            status: "running".into(),
            collect_count: 5,
            collect_fail_count: 1,
            last_collect_ts: 123,
            last_collect_duration_ms: 42,
            vm_restart_count: 2,
            script_version: "abcdef12".into(),
            last_err: "timeout".into(),
        }
    }

    #[tokio::test]
    async fn write_read_roundtrip() {
        let c = client().await;
        write(&c, "inst1", &sample()).await.unwrap();
        let got = read(&c, "inst1").await.unwrap().expect("stat 应存在");
        assert_eq!(got.status, "running");
        assert_eq!(got.collect_count, 5);
        assert_eq!(got.collect_fail_count, 1);
        assert_eq!(got.vm_restart_count, 2);
        assert_eq!(got.script_version, "abcdef12");
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
}
```

### - [x] Step 2：db/mod.rs 引出 luastat

`src/db/mod.rs` 顶部 `pub mod` 段（`pub mod luakv;` 后）加：
```rust
pub mod luastat;
```

### - [x] Step 3：identity.rs hash 清单加 stat

`src/db/identity.rs` 顶部清单注释（`luakv:<instId>` 行后、`rtdata:<devId>` 行前）加一行：
```rust
//! - `stat:<instId>`  : string，JSON 实例状态快照（M7 luaClient 运行指标：status/collect 计数/脚本版本）
```

### - [x] Step 4：run luastat 单测 + fmt + clippy + 提交

```bash
CARGO_BUILD_JOBS=1 cargo test --lib db::luastat
cargo fmt --all && cargo fmt --all --check && CARGO_BUILD_JOBS=1 cargo clippy --all-targets -- -D warnings
git add src/db/luastat.rs src/db/mod.rs src/db/identity.rs
git commit -m "feat(lua): M7 db/luastat 状态快照（stat:<inst> SET/GET JSON）"
```
Expected: luastat 3 测试绿（roundtrip / missing / isolated）。

---

## Task 2: config.rs fail_threshold 字段

**Files:** Modify `src/plugin/builtin/lua_client/config.rs`

### - [ ] Step 1：先加失败单测（TDD）

`config.rs` 的 `#[cfg(test)] mod tests` 内加：
```rust
    #[test]
    fn parse_fail_threshold_default_and_override() {
        // 缺省 = 10（spec §9.6）。
        let cfg = parse_cfg(&json!({"script":"devA","endpoint":"127.0.0.1:502"})).unwrap();
        assert_eq!(cfg.fail_threshold, 10);
        // 显式覆盖。
        let cfg = parse_cfg(&json!({"script":"devA","endpoint":"x","fail_threshold":3})).unwrap();
        assert_eq!(cfg.fail_threshold, 3);
    }
```

### - [ ] Step 2：跑测确认失败

```bash
CARGO_BUILD_JOBS=1 cargo test --lib plugin::builtin::lua_client::config::tests::parse_fail_threshold_default_and_override
```
Expected: 编译失败（`fail_threshold` 字段不存在）。

### - [ ] Step 3：LuaCfg 加字段 + parse

`LuaCfg` struct（`hook_instr_limit` 字段后）加：
```rust
    /// 连续 collect 失败 N 次标记 degraded（spec §9.6）。
    pub(super) fail_threshold: u32,
```

`parse_cfg` 函数（`hook_instr_limit` 解析后）加：
```rust
    let fail_threshold = app
        .get("fail_threshold")
        .and_then(|v| v.as_u64())
        .unwrap_or(10) as u32;
```

`Ok(LuaCfg { ... })` 构造（`hook_instr_limit,` 后）加：
```rust
        fail_threshold,
```

### - [ ] Step 4：跑测确认通过 + fmt + clippy + 提交

```bash
CARGO_BUILD_JOBS=1 cargo test --lib plugin::builtin::lua_client::config
cargo fmt --all && cargo fmt --all --check && CARGO_BUILD_JOBS=1 cargo clippy --all-targets -- -D warnings
git add src/plugin/builtin/lua_client/config.rs
git commit -m "feat(lua): M7 LuaCfg fail_threshold（连续失败 N 次标 degraded）"
```
Expected: config 全测试绿（含新 fail_threshold）。

---

## Task 3: mod.rs stat 采点 + script_version

**Files:** Modify `src/plugin/builtin/lua_client/mod.rs`

### - [ ] Step 1：顶层 use 加 sha2::Digest

mod.rs 顶部 `use` 段（`use mlua::LuaSerdeExt;` 后）加：
```rust
use sha2::Digest;
```

### - [ ] Step 2：script_version helper（含单测）

mod.rs（`lua_value_to_sample` 函数后、`inventory::submit!` 前）加 helper：
```rust
/// 算脚本版本（main.lua 内容 sha256 前 8 hex，spec §13 `script_version`）。
///
/// 文件读失败返回 `"unknown"`（不阻塞启动；load_main 会报真正的加载错误）。
fn script_version(scripts_root: &std::path::Path, name: &str) -> String {
    let path = loader::main_lua_path(scripts_root, name);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let mut h = sha2::Sha256::new();
            h.update(&bytes);
            let sum = h.finalize();
            let mut hex = String::with_capacity(8);
            for b in sum.iter().take(4) {
                hex.push_str(&format!("{:02x}", b));
            }
            hex
        }
        Err(_) => "unknown".into(),
    }
}
```

mod.rs `#[cfg(test)] mod tests` 内加单测：
```rust
    #[test]
    fn script_version_stable_and_short() {
        let tmp = std::env::temp_dir().join(format!("lua-sv-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("devA")).unwrap();
        std::fs::write(tmp.join("devA/main.lua"), "function collect() return { v = 1 } end").unwrap();
        let v1 = super::script_version(&tmp, "devA");
        assert_eq!(v1.len(), 8, "sha256 前 8 hex");
        assert_ne!(v1, "unknown");
        // 同内容 → 同 hash。
        let v2 = super::script_version(&tmp, "devA");
        assert_eq!(v1, v2);
        // 不存在 → unknown。
        assert_eq!(super::script_version(&tmp, "missing"), "unknown");
        let _ = std::fs::remove_dir_all(&tmp);
    }
```

### - [ ] Step 3：run() 内 stat 初始化 + 启动写 running

mod.rs `run()` 主循环前（`let mut lua = lua;` 行前）插入 stat 初始化：
```rust
        // 状态快照（spec §13）：启动写 running，collect/reload/退出 更新。
        let script_ver = script_version(&scripts_root, &cfg.script);
        let mut stat = crate::db::luastat::LuaStat {
            status: "running".into(),
            collect_count: 0,
            collect_fail_count: 0,
            last_collect_ts: 0,
            last_collect_duration_ms: 0,
            vm_restart_count: 0,
            script_version: script_ver,
            last_err: String::new(),
        };
        let _ = crate::db::luastat::write(&ctx.lux, inst.inst_id.as_ref(), &stat).await;
        // 连续失败计数（内存，不落 lux；仅 stat.status 反映 degraded）。
        let mut consecutive_fail: u32 = 0;
```

### - [ ] Step 4：Evt::Tick 分支加 stat 采点

mod.rs 主循环 `Evt::Tick => { ... }` 分支整体替换为（加耗时计量 + stat 采点，保留原 collect→Sample→hub 逻辑）：
```rust
                Evt::Tick => {
                    if cfg.period == 0 {
                        continue;
                    }
                    let started = std::time::Instant::now();
                    let now = chrono::Local::now().timestamp();
                    let mut tick_err: Option<String> = None;
                    for dev in &inst.devices {
                        match call_collect(&lua).await {
                            Ok(Some(values)) if !values.is_empty() => {
                                let sample = Sample {
                                    hwid: ctx.hwid.clone(),
                                    devid: dev.value.clone(),
                                    values,
                                    timestamp: now,
                                };
                                if tx.send(sample).await.is_err() {
                                    break;
                                }
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!("[luaClient] collect 失败：{e}");
                                tick_err = Some(e.to_string());
                            }
                        }
                    }
                    // 状态采点（spec §13）：按周期计，任一 collect 失败记 fail 周期。
                    stat.last_collect_ts = now;
                    stat.last_collect_duration_ms = started.elapsed().as_millis() as u64;
                    if let Some(e) = tick_err {
                        stat.collect_fail_count += 1;
                        stat.last_err = e;
                        consecutive_fail = consecutive_fail.saturating_add(1);
                        if consecutive_fail >= cfg.fail_threshold {
                            stat.status = "degraded".into();
                        }
                    } else {
                        stat.collect_count += 1;
                        stat.last_err.clear();
                        consecutive_fail = 0;
                        stat.status = "running".into();
                    }
                    let _ =
                        crate::db::luastat::write(&ctx.lux, inst.inst_id.as_ref(), &stat).await;
                }
```

### - [ ] Step 5：Evt::Reload Ok 分支加 vm_restart_count

mod.rs `Evt::Reload => { match reload_vm(...) { Ok(new_lua) => { ... } Err(e) => ... } }` 的 `Ok(new_lua)` 分支替换为：
```rust
                        Ok(new_lua) => {
                            lua = new_lua;
                            stat.vm_restart_count += 1;
                            consecutive_fail = 0;
                            stat.status = "running".into();
                            let _ = crate::db::luastat::write(
                                &ctx.lux,
                                inst.inst_id.as_ref(),
                                &stat,
                            )
                            .await;
                            tracing::info!("[luaClient] inst {} 热加载成功", inst.inst_id);
                        }
```

### - [ ] Step 6：loop 退出后写 stopped

mod.rs 主循环 `loop { ... }` 之后（`run` 函数末尾，`}` 闭合 loop 后）加：
```rust
        // 退出标记 stopped（spec §13 status）。
        stat.status = "stopped".into();
        let _ = crate::db::luastat::write(&ctx.lux, inst.inst_id.as_ref(), &stat).await;
```

> **注意：** 主循环 `loop { ... }` 结束（break）后，`ctx`/`stat`/`inst` 仍在作用域内（未被 move），可直接写。`inst.inst_id.as_ref()`——InstId 实现 `AsRef<str>`（`impl_string_newtype!`）。

### - [ ] Step 7：run 全量 lua_client + fmt + clippy + 提交

```bash
CARGO_BUILD_JOBS=1 cargo test --lib plugin::builtin::lua_client
cargo fmt --all && cargo fmt --all --check && CARGO_BUILD_JOBS=1 cargo clippy --all-targets -- -D warnings
git add src/plugin/builtin/lua_client/mod.rs
git commit -m "feat(lua): M7 run() stat 采点（collect/reload/退出写 stat:<inst>）"
```
Expected: lua_client 全单测绿（含新 script_version；collect/on_write/on_data/Evt 不破坏）。

---

## Task 4: api/routes_lua.rs + 注册路由

**Files:** Create `src/api/routes_lua.rs`; Modify `src/api/mod.rs`

### - [ ] Step 1：先写 scan_scripts 单测（TDD，纯函数）

新建 `src/api/routes_lua.rs`，先放单测骨架（验证 scan 逻辑）：
```rust
//! Lua 实例状态/重载 REST 端点（spec §13）。默认 JWT 保护。
//!
//! - `GET /api/v1/lua/stats?inst_id=<inst>` → 读 lux `stat:<inst>` JSON
//! - `POST /api/v1/lua/reload?inst_id=<inst>` → 手动重载（stop+start，复用 InstanceManager；
//!   与 notify 自动热加载结果一致，IO 重连——spec §5.3「VM 替换 ≈ 轻量重启 instance」）
//! - `GET /api/v1/lua/scripts` → 列 `scripts/` 下有 `main.lua` 的目录名
//!
//! v1 用 query 参数（与 `routes_inst::IdQuery` 一致，避 axum 0.8 路径参数语法歧义）。

use crate::api::{str_from_value, AppState};
use crate::types::InstConfig;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Json;
use lux::EmbeddedValue;
use serde::Deserialize;
use std::path::Path;

/// `?inst_id=<inst>` 查询参数。
#[derive(Deserialize)]
pub struct InstQuery {
    /// 实例 ID。
    pub inst_id: String,
}

/// `GET /api/v1/lua/stats?inst_id=<inst>` → 实例状态快照 JSON；无则空对象 `{}`。
pub async fn stats(State(st): State<AppState>, Query(q): Query<InstQuery>) -> Response {
    let key = format!("stat:{}", q.inst_id);
    match st.lux.execute_value("GET", &[&key]).await {
        Ok(v) if !matches!(v, EmbeddedValue::Nil) => {
            let raw = str_from_value(&v);
            let json: serde_json::Value =
                serde_json::from_str(&raw).unwrap_or(serde_json::json!({}));
            Json(json).into_response()
        }
        _ => Json(serde_json::json!({})).into_response(),
    }
}

/// `POST /api/v1/lua/reload?inst_id=<inst>` → 手动重载（stop + start，spec §5.3）。
///
/// 仅对 `app_code == "luaClient"` 实例生效（端点语义）。实例不在 lux `instances` 表 → 404；
/// 非 lua 实例 → 400。
pub async fn reload(State(st): State<AppState>, Query(q): Query<InstQuery>) -> Response {
    let raw = match st
        .lux
        .execute_value("HGET", &["instances", &q.inst_id])
        .await
    {
        Ok(v) if !matches!(v, EmbeddedValue::Nil) => str_from_value(&v),
        _ => return err(StatusCode::NOT_FOUND, "instance not found"),
    };
    let cfg: InstConfig = match serde_json::from_str(&raw) {
        Ok(c) => c,
        Err(e) => return err(StatusCode::BAD_REQUEST, &format!("invalid config: {e}")),
    };
    if cfg.app_code.as_ref() != "luaClient" {
        return err(StatusCode::BAD_REQUEST, "not a luaClient instance");
    }
    let _ = st.mgr.stop(&q.inst_id).await;
    if let Err(e) = st.mgr.start_source(cfg).await {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    Json(serde_json::json!({"status": "ok"})).into_response()
}

/// `GET /api/v1/lua/scripts` → `{ "scripts": ["devA", ...] }`（cwd/scripts 下有 main.lua 的目录）。
pub async fn list_scripts(State(_st): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "scripts": scan_scripts(Path::new("scripts")) }))
}

/// 扫描 `root` 下有 `main.lua` 的子目录名（排序）。纯函数便于单测（不依赖 cwd）。
fn scan_scripts(root: &Path) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() && p.join("main.lua").exists() {
                if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                    names.push(name.to_string());
                }
            }
        }
    }
    names.sort();
    names
}

// ---- 局部类型/工具 ----
type StatusCode = axum::http::StatusCode;
type Response = axum::response::Response;

fn err(code: StatusCode, msg: &str) -> Response {
    (code, msg.to_string()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_picks_dirs_with_main_lua() {
        let tmp = std::env::temp_dir().join(format!("lua-scripts-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("devA")).unwrap();
        std::fs::write(tmp.join("devA/main.lua"), "function collect() end").unwrap();
        std::fs::create_dir_all(tmp.join("empty")).unwrap(); // 无 main.lua，不列
        let got = scan_scripts(&tmp);
        let _ = std::fs::remove_dir_all(&tmp);
        assert_eq!(got, vec!["devA".to_string()]);
    }

    #[test]
    fn scan_missing_root_is_empty() {
        assert!(scan_scripts(Path::new("/no/such/rs-iot-lua-scripts-xyz")).is_empty());
    }
}
```

### - [ ] Step 2：api/mod.rs 引出 + 注册路由

`src/api/mod.rs` 顶部 `pub mod` 段（`pub mod routes_inst;` 后）加：
```rust
pub mod routes_lua;
```

`build_router` 函数内（`// 命令下发` 注释块前，或 `.route("/api/v1/command/send", ...)` 前）加：
```rust
        // Lua 实例状态/重载（spec §13，默认 JWT 保护）
        .route("/api/v1/lua/stats", get(routes_lua::stats))
        .route("/api/v1/lua/reload", post(routes_lua::reload))
        .route("/api/v1/lua/scripts", get(routes_lua::list_scripts))
```

### - [ ] Step 3：run scan 单测 + build + fmt + clippy + 提交

```bash
CARGO_BUILD_JOBS=1 cargo test --lib api::routes_lua
CARGO_BUILD_JOBS=1 cargo build --lib
cargo fmt --all && cargo fmt --all --check && CARGO_BUILD_JOBS=1 cargo clippy --all-targets -- -D warnings
git add src/api/routes_lua.rs src/api/mod.rs
git commit -m "feat(lua): M7 routes_lua REST 端点（stats/reload/scripts，JWT 保护）"
```
Expected: routes_lua 2 单测绿（scan 有/无 main.lua）；build 编译过（路由注册 + handler 类型对齐）。

---

## Task 5: 集成测（stat 采集 + REST 端点）

**Files:** Create `tests/lua_stat_e2e.rs`; Create `tests/lua_api_e2e.rs`

### - [ ] Step 1：lua_stat_e2e.rs（stat 采集端到端）

新建 `tests/lua_stat_e2e.rs`（仿 lua_hotload_e2e 的 InstanceManager setup）：
```rust
//! M7 状态监测端到端：起 luaClient 实例 → collect 周期 → 验 lux `stat:<inst>` 指标
//!（status=running / collect_count≥1 / script_version 非空）。spec §13。

mod common;

use common::test_data_dir;
use rs_iot::db::luastat;
use rs_iot::host::{BridgeRegistry, CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::plugin::traits::Processor;
use rs_iot::transform::passthrough::Passthrough;
use rs_iot::types::{AppType, DeviceSpec, HostKind, Hwid, InstConfig, PluginName, Sample};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn stat_written_after_collect() {
    let data_dir = test_data_dir("lua-stat");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client).await.expect("bootstrap");

    // 临时脚本目录（collect 返回 v=7）。
    let scripts = std::env::temp_dir().join(format!("lua-stat-{}", std::process::id()));
    let script_dir = scripts.join("st");
    std::fs::create_dir_all(&script_dir).expect("mkdir");
    std::fs::write(
        script_dir.join("main.lua"),
        "function collect() return { v = 7 } end\n",
    )
    .expect("write");

    // mock tcp server（luaClient endpoint；collect 不 request 本测，endpoint 闲置即可）。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);

    let shutdown = CancellationToken::new();
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown: shutdown.clone(),
        ctl_registry: CtlRegistry::new(),
        bridge_registry: BridgeRegistry::new(),
    };
    let (hub_tx, hub_rx) = tokio::sync::mpsc::channel::<Sample>(64);
    let mgr = Arc::new(InstanceManager::new(ctx.clone(), hub_tx));
    let proc: Arc<dyn Processor> = Arc::new(Passthrough);
    let pt = shutdown.child_token();
    let _pipeline = tokio::spawn(rs_iot::pipeline::run(ctx, hub_rx, proc, pt));

    let inst = InstConfig {
        inst_id: "lua-st".into(),
        inst_name: "st".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: true,
        app_config: json!({
            "script": "st",
            "kind": "tcp",
            "endpoint": format!("127.0.0.1:{port}"),
            "period": 1,
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "devST".into(),
            children: vec![],
        }],
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start lua");

    // 等 1-2 个 collect 周期（period 1s）。
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let stat = luastat::read(&client, "lua-st")
        .await
        .expect("read stat")
        .expect("stat 应已写入");
    assert_eq!(stat.status, "running", "应 running");
    assert!(
        stat.collect_count >= 1,
        "collect_count ≥ 1，得到 {}",
        stat.collect_count
    );
    assert!(!stat.script_version.is_empty(), "script_version 非空");
    assert_ne!(stat.script_version, "unknown", "脚本存在，version 应是真实 hash");
    assert!(stat.last_collect_ts > 0, "last_collect_ts 已记");
    assert!(stat.last_collect_duration_ms > 0 || stat.collect_count >= 1, "duration 已记或已采集");

    shutdown.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
    let _ = std::fs::remove_dir_all(&scripts);
}

#[tokio::test]
async fn stat_marked_stopped_on_exit() {
    let data_dir = test_data_dir("lua-stat-stop");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client).await.expect("bootstrap");

    let scripts = std::env::temp_dir().join(format!("lua-stat-stop-{}", std::process::id()));
    std::fs::create_dir_all(scripts.join("st2")).expect("mkdir");
    std::fs::write(
        scripts.join("st2/main.lua"),
        "function collect() return { v = 1 } end\n",
    )
    .expect("write");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);

    let shutdown = CancellationToken::new();
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown: shutdown.clone(),
        ctl_registry: CtlRegistry::new(),
        bridge_registry: BridgeRegistry::new(),
    };
    let (hub_tx, hub_rx) = tokio::sync::mpsc::channel::<Sample>(64);
    let mgr = Arc::new(InstanceManager::new(ctx.clone(), hub_tx));
    let proc: Arc<dyn Processor> = Arc::new(Passthrough);
    let pt = shutdown.child_token();
    let _pipeline = tokio::spawn(rs_iot::pipeline::run(ctx, hub_rx, proc, pt));

    let inst = InstConfig {
        inst_id: "lua-st2".into(),
        inst_name: "st2".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: true,
        app_config: json!({
            "script": "st2",
            "kind": "tcp",
            "endpoint": format!("127.0.0.1:{port}"),
            "period": 1,
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "devST2".into(),
            children: vec![],
        }],
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start lua");
    tokio::time::sleep(Duration::from_millis(800)).await;

    // 停实例 → run() 退出写 stopped。
    mgr.stop("lua-st2").await.expect("stop");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let stat = luastat::read(&client, "lua-st2")
        .await
        .expect("read")
        .expect("stat 存在");
    assert_eq!(stat.status, "stopped", "实例退出后应标 stopped");

    shutdown.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
    let _ = std::fs::remove_dir_all(&scripts);
}
```

### - [ ] Step 2：run lua_stat_e2e

```bash
CARGO_BUILD_JOBS=1 cargo test --test lua_stat_e2e
```
Expected: 2 passed（stat_written_after_collect / stat_marked_stopped_on_exit）。

### - [ ] Step 3：lua_api_e2e.rs（REST 端点集成测）

新建 `tests/lua_api_e2e.rs`（仿 api_flow.rs 的 router oneshot + JWT）：
```rust
//! M7 Lua REST 端点集成测：stats 读 stat / reload 404 / scripts 列表形状 / JWT 保护。
//! spec §13 + §4。仿 api_flow.rs（router oneshot + JWT token）。

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rs_iot::api::{build_router, AppState};
use rs_iot::db::luastat::LuaStat;
use rs_iot::host::{BridgeRegistry, CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::types::Hwid;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// 构造带（可选）预写 stat 的 app。
async fn app_with_stat(stat_inst: &str) -> (axum::Router, lux::ServerHandle) {
    let data_dir = common::test_data_dir("lua-api");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client).await.expect("bootstrap");
    if !stat_inst.is_empty() {
        let stat = LuaStat {
            status: "running".into(),
            collect_count: 3,
            collect_fail_count: 0,
            last_collect_ts: 99,
            last_collect_duration_ms: 5,
            vm_restart_count: 1,
            script_version: "deadbeef".into(),
            last_err: String::new(),
        };
        rs_iot::db::luastat::write(&client, stat_inst, &stat)
            .await
            .expect("write stat");
    }
    let shutdown = CancellationToken::new();
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown,
        ctl_registry: CtlRegistry::new(),
        bridge_registry: BridgeRegistry::new(),
    };
    let (hub_tx, _hub_rx) = tokio::sync::mpsc::channel(64);
    let mgr = Arc::new(InstanceManager::new(ctx, hub_tx));
    let state = AppState {
        lux: client.clone(),
        mgr,
        hwid: Hwid::new("it-hwid"),
        jwt_secret: "test-secret-0123456789abcdef".into(),
        version: "itest",
    };
    (build_router(state), handle)
}

async fn login_token(app: &axum::Router) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/user/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"user_id":"admin","pass":"admin"}"#))
                .expect("req"),
        )
        .await
        .expect("call");
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("body");
    serde_json::from_slice::<serde_json::Value>(&bytes).expect("json")["token"]
        .as_str()
        .expect("token")
        .to_string()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

#[tokio::test]
async fn stats_returns_written_stat() {
    let (app, handle) = app_with_stat("lua-api").await;
    let token = login_token(&app).await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/lua/stats?inst_id=lua-api")
                .header(header::AUTHORIZATION, &token)
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("call");
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["status"], "running");
    assert_eq!(json["collect_count"], 3);
    assert_eq!(json["script_version"], "deadbeef");
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn stats_missing_returns_empty_object() {
    let (app, handle) = app_with_stat("").await;
    let token = login_token(&app).await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/lua/stats?inst_id=nonexist")
                .header(header::AUTHORIZATION, &token)
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("call");
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(
        json.as_object().is_some_and(|m| m.is_empty()),
        "无 stat 返回空对象 {{}}，得到 {json}"
    );
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn stats_requires_token() {
    let (app, handle) = app_with_stat("").await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/lua/stats?inst_id=x")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("call");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn reload_unknown_instance_404() {
    let (app, handle) = app_with_stat("").await;
    let token = login_token(&app).await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/lua/reload?inst_id=nonexist")
                .header(header::AUTHORIZATION, &token)
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("call");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn scripts_returns_list_shape() {
    let (app, handle) = app_with_stat("").await;
    let token = login_token(&app).await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/lua/scripts")
                .header(header::AUTHORIZATION, &token)
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("call");
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["scripts"].is_array(), "返回 {{ scripts: [...] }}");
    common::stop_lux(handle).await;
}
```

### - [ ] Step 4：run lua_api_e2e + 全量 fmt/clippy/test + 提交

```bash
CARGO_BUILD_JOBS=1 cargo test --test lua_api_e2e
cargo fmt --all --check && CARGO_BUILD_JOBS=1 cargo clippy --all-targets -- -D warnings
CARGO_BUILD_JOBS=1 cargo test --all-targets --all-features
git add tests/lua_stat_e2e.rs tests/lua_api_e2e.rs
git commit -m "test(lua): M7 状态监测 + REST 集成测（stat 采集/stopped/stats/reload/scripts）"
```
Expected: lua_api_e2e 5 passed；全量测试绿（lib + 集成测，含 M1-M6 既有 111 + M7 新增）。

---

## Self-Review

**1. Spec coverage（M7 §13 状态监测 + §4 routes_lua）：**
- ✅ `stat:<inst>` JSON 指标（status / collect_count / collect_fail_count / last_collect_ts / last_collect_duration_ms / vm_restart_count / script_version / last_err）→ Task 1（结构）+ Task 3（采点）
- ⚠️ `io_connected` / `last_io_err`（spec §13 列出）v1 **不实现**——io_engine 未暴露连接状态，后置 Plan 7/M8（已注明简化）
- ✅ `vm_restart_count`（热加载次数）→ Task 3 Evt::Reload Ok 分支
- ✅ `script_version`（脚本 hash）→ Task 3 `script_version` helper（main.lua sha256 前 8 hex）
- ✅ degraded 标记（连续失败 N 次，spec §9.6）→ Task 2 `fail_threshold` + Task 3 采点逻辑
- ✅ REST `GET stats` → Task 4 `routes_lua::stats`（读 lux `stat:<inst>`）
- ✅ REST `POST reload` → Task 4 `routes_lua::reload`（stop + start，spec §5.3 同构）
- ✅ REST `GET scripts` → Task 4 `routes_lua::list_scripts`（scan scripts/）
- ✅ JWT 保护（默认）→ 不入 `PUBLIC_PATHS`，复用现有中间件（Task 5 `stats_requires_token` 验证）

**2. Placeholder scan：** 无 TBD/TODO；luastat/scan_scripts/script_version/reload 代码完整；所有 handler 实现到位；测试代码完整（非「写测试」描述）。

**3. Type consistency：**
- `LuaStat` 字段集贯穿 Task 1（定义）→ Task 3（构造/更新）→ Task 5（断言）一致
- `fail_threshold: u32`（Task 2 定义）→ Task 3 `cfg.fail_threshold`（比较 `consecutive_fail >= cfg.fail_threshold`，u32 vs u32）一致
- `scan_scripts(&Path) -> Vec<String>`（Task 4 定义）→ `list_scripts` 调用 `scan_scripts(Path::new("scripts"))` 一致
- `script_version(&Path, &str) -> String`（Task 3 定义 + 单测）→ run() 调用 `script_version(&scripts_root, &cfg.script)`（scripts_root: PathBuf deref &Path；cfg.script: String deref &str）一致
- `luastat::write(&EmbeddedClient, &str, &LuaStat)` / `read(&EmbeddedClient, &str) -> Option<LuaStat>`（Task 1）→ Task 3 / Task 5 调用签名一致
- `inst.inst_id.as_ref()`（InstId: AsRef<str>，impl_string_newtype! 宏）→ Task 3 多处 `inst.inst_id.as_ref()` 传 `&str` 一致

**已知简化（非占位）：** io_connected/last_io_err 后置；REST reload = stop+start（IO 重连）；REST query 参数非路径参数；script_version 单文件 sha256；stat 按 collect 周期计（多 dev 任一失败记 fail）。

---

## Execution Handoff

本计划由 `/loop`（CronCreate `116fd1e9`）驱动，每轮 subagent-driven-development 执行下一个未勾选 task。CI 门槛：
```bash
cargo fmt --all --check && CARGO_BUILD_JOBS=1 cargo clippy --all-targets -- -D warnings
CARGO_BUILD_JOBS=1 cargo test --all-targets --all-features
```
（Windows CARGO_BUILD_JOBS=1 规避 link.exe 并行 OOM）。API 503 间歇不稳时控制器接手（plan 含完整代码 + cargo 不依赖 LLM API）。

Task 1-5 完成 → `finishing-a-development-branch` 合并 master + 删分支 + 更新 PROGRESS.md → 写 Plan 7（M8 serial/udp 传输 + L2 数据 API + 文档）。

建分支：`feature/lua-m7`（从 master `818b926`）。
