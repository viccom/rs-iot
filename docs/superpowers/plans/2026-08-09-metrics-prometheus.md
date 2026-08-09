# `/metrics` Prometheus 端点 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 新增 `GET /metrics` 端点,把 M4 已埋的指标(ProcessMetrics atomic + InstanceStat lux + sysinfo 资源)以 Prometheus exposition 文本格式输出,免 JWT,供 Prometheus scrape。

**Architecture:** 手写 Prometheus 文本格式化(零依赖)。新 `src/api/routes_metrics.rs`(render 函数 + handler);sysinfo 采样从 `routes_sys.rs` 抽到 `process_metrics.rs::sample_process_resource`(`/sysstatus` + `/metrics` DRY 复用);`build_router` 注册路由 + `PUBLIC_PATHS` 白名单。数据源全来自 M4,零新埋点。

**Tech Stack:** Rust + axum + sysinfo 0.32 + lux EmbeddedClient + serde_json(InstState→label 字符串)。

**Spec:** `docs/superpowers/specs/2026-08-09-metrics-prometheus-design.md`

---

## File Structure

- **Create** `src/api/routes_metrics.rs`——Prometheus 文本 render 函数(`render_process`/`render_instances` + helper)+ `metrics` handler + 单测
- **Modify** `src/process_metrics.rs`——新增 `sample_process_resource() -> (f32, u64)` 公共函数 + 烟雾单测
- **Modify** `src/api/routes_sys.rs`——`sysstatus` 改调 `sample_process_resource`(删内联 sysinfo 块)
- **Modify** `src/api/mod.rs`——`pub mod routes_metrics` + `build_router` 注册 `GET /metrics` + `PUBLIC_PATHS` 加 `/metrics`
- **Create** `tests/metrics_e2e.rs`——axum oneshot 集成测(免 JWT + 200 + text/plain + 进程级 metric)

---

## Task 1: sysinfo 采样公共化

**Files:**
- Modify: `src/process_metrics.rs`(新增 `sample_process_resource` + 单测)
- Modify: `src/api/routes_sys.rs:15-55`(`sysstatus` 改调公共函数)

**目的:** `/sysstatus` 与 `/metrics` 都要采进程 CPU/mem,抽公共函数避免复制。封装现有已验证的内联 sysinfo 逻辑。

- [ ] **Step 1: 写失败测**

在 `src/process_metrics.rs` 的 `#[cfg(test)] mod tests` 末尾(`hub_depth_tracks_max` 测试后)加:

```rust
    #[test]
    fn sample_process_resource_smoke() {
        // sysinfo 现采,值随环境变化(cpu 首次可能 0);只验封装正确、不 panic、类型对。
        let (cpu, mem) = sample_process_resource();
        let _ = (cpu, mem);
    }
```

- [ ] **Step 2: 跑测确认失败**

Run: `cargo test --lib sample_process_resource_smoke`
Expected: 编译失败(`cannot find function sample_process_resource`)。

- [ ] **Step 3: 实现 sample_process_resource**

在 `src/process_metrics.rs` 的 `impl ProcessMetrics { ... }` 块**之后**、`impl Default` **之前**加自由函数:

```rust
/// 现采进程 CPU%(0.0-100.0)与常驻内存字节(sysinfo 0.32)。
///
/// `/sysstatus` 与 `/metrics` 复用(DRY)。`System::new_all()` 构造并刷新;
/// `get_current_pid` 返回 `Result`,失败兜底 `Pid::from(0_usize)`。
pub fn sample_process_resource() -> (f32, u64) {
    let sys = sysinfo::System::new_all();
    let pid = sysinfo::get_current_pid().unwrap_or_else(|_| sysinfo::Pid::from(0_usize));
    sys.process(pid)
        .map(|p| (p.cpu_usage(), p.memory()))
        .unwrap_or((0.0, 0))
}
```

- [ ] **Step 4: 跑测确认通过**

Run: `cargo test --lib sample_process_resource_smoke`
Expected: PASS。

- [ ] **Step 5: 改 sysstatus 调公共函数**

`src/api/routes_sys.rs` 的 `sysstatus` 函数,把内联 sysinfo 块(第 21-28 行注释 + `let sys = ...` 到 `unwrap_or((0.0, 0))`)替换为一行:

旧(删除):
```rust
    // sysinfo 0.32 现采：进程 CPU% 与内存字节。
    // System::new_all() 构造并刷新；get_current_pid 返回 Result，失败兜底 Pid 0。
    let sys = sysinfo::System::new_all();
    let pid = sysinfo::get_current_pid().unwrap_or_else(|_| sysinfo::Pid::from(0_usize));
    let (cpu, mem) = sys
        .process(pid)
        .map(|p| (p.cpu_usage(), p.memory()))
        .unwrap_or((0.0, 0));
```

新(替换为):
```rust
    // sysinfo 现采进程 CPU%/mem（process_metrics 公共函数，/metrics 复用）。
    let (cpu, mem) = crate::process_metrics::sample_process_resource();
```

- [ ] **Step 6: fmt + clippy + 全量测**

Run: `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets`
Expected: fmt 通过、clippy 零警告、全量测通过(`/sysstatus` 行为不变,既有 `api_flow` 不破坏)。

- [ ] **Step 7: commit**

```bash
git add src/process_metrics.rs src/api/routes_sys.rs
git commit -m "refactor(metrics): 抽 sysinfo 采样公共函数（/sysstatus + /metrics 复用）"
```

---

## Task 2: 进程级 metrics render 函数

**Files:**
- Create: `src/api/routes_metrics.rs`(骨架 + helper + `render_process` + 单测)
- Modify: `src/api/mod.rs`(加 `pub mod routes_metrics;`)

**目的:** 把 `ProcessMetrics` 的 atomic 字段格式化成 Prometheus 文本(进程标识 + 资源 + ctl/pipeline/hub/mqtt)。本 task 不接 handler,纯函数 + 单测。

- [ ] **Step 1: 注册模块(让 render 单测能编译到)**

`src/api/mod.rs` 的 `pub mod` 列表(第 15-25 行)按字母序加一行(在 `routes_lua` 后、`routes_rtdata` 前):

```rust
pub mod routes_metrics;
```

- [ ] **Step 2: 写失败测**

创建 `src/api/routes_metrics.rs`,先只写测试模块(函数尚未定义 → 编译失败):

```rust
//! Prometheus `/metrics` 端点(M4 指标的标准化机器可读出口)。spec §3-§6。

use crate::process_metrics::ProcessMetrics;
use std::sync::atomic::Ordering;

#[cfg(test)]
mod tests {
    use super::*;

    fn proc_metrics() -> ProcessMetrics {
        let m = ProcessMetrics::new();
        m.ctl_send_count.store(7, Ordering::Relaxed);
        m.ctl_dropped_count.store(1, Ordering::Relaxed);
        m.hub_depth_last.store(42, Ordering::Relaxed);
        m.hub_depth_max.store(99, Ordering::Relaxed);
        m.mqtt_publish_count.store(5, Ordering::Relaxed);
        m.mqtt_connected.store(true, Ordering::Relaxed);
        m
    }

    #[test]
    fn render_process_has_help_type_and_values() {
        let m = proc_metrics();
        let out = render_process(&m, "eiot_abc", "0.1.0", 12.5, 8192);
        // HELP / TYPE 每个 family 齐全
        assert!(out.contains("# HELP rs_iot_info "));
        assert!(out.contains("# TYPE rs_iot_info gauge"));
        // info 含 hwid + version label
        assert!(out.contains(r#"rs_iot_info{hwid="eiot_abc",version="0.1.0"} 1"#));
        // counter 带值 + _total 后缀
        assert!(out.contains("# TYPE rs_iot_ctl_send_total counter"));
        assert!(out.contains("rs_iot_ctl_send_total 7"));
        assert!(out.contains("rs_iot_ctl_dropped_total 1"));
        // gauge(hub 深度,无 _total 后缀)
        assert!(out.contains("# TYPE rs_iot_hub_depth gauge"));
        assert!(out.contains("rs_iot_hub_depth 42"));
        assert!(out.contains("rs_iot_hub_depth_max 99"));
        // mqtt
        assert!(out.contains("rs_iot_mqtt_publish_total 5"));
        assert!(out.contains("rs_iot_mqtt_connected 1"));
        // 资源
        assert!(out.contains("rs_iot_process_cpu_pct 12.5"));
        assert!(out.contains("rs_iot_process_mem_bytes 8192"));
        assert!(out.contains("rs_iot_uptime_seconds"));
    }

    #[test]
    fn render_process_counters_all_end_with_total() {
        // 命名规范锁定:每个 counter 名以 _total 结尾。
        let m = ProcessMetrics::new();
        let out = render_process(&m, "h", "v", 0.0, 0);
        for line in out.lines() {
            if line.starts_with("# TYPE ") && line.ends_with(" counter") {
                let name = line.trim_start_matches("# TYPE ").trim_end_matches(" counter");
                assert!(
                    name.ends_with("_total"),
                    "counter {name} 应以 _total 结尾"
                );
            }
        }
    }
}
```

- [ ] **Step 3: 跑测确认失败**

Run: `cargo test --lib render_process`
Expected: 编译失败(`cannot find function render_process`)。

- [ ] **Step 4: 实现 helper + render_process**

在 `src/api/routes_metrics.rs` 顶部(`use` 之后、`#[cfg(test)]` 之前)加:

```rust
/// Prometheus exposition 文本 Content-Type。
pub(crate) const METRICS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// 写 metric family 头(`# HELP` + `# TYPE`),每 family 一次。
fn family_header(s: &mut String, name: &str, help: &str, ty: &str) {
    s.push_str(&format!("# HELP {name} {help}.\n"));
    s.push_str(&format!("# TYPE {name} {ty}\n"));
}

/// 写一行 metric:`name{labels} value`。
fn metric_line(s: &mut String, name: &str, labels: &str, val: &str) {
    s.push_str(&format!("{name}{labels} {val}\n"));
}

/// label 值 escape(Prometheus 规范:`"` / `\` / 换行)。
fn escape_label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// 进程级指标 → Prometheus 文本(标识 + 资源 + ctl/pipeline/hub/mqtt)。
fn render_process(m: &ProcessMetrics, hwid: &str, version: &str, cpu: f32, mem: u64) -> String {
    let mut s = String::new();
    let l = Ordering::Relaxed;

    // 进程标识(info)
    family_header(&mut s, "rs_iot_info", "rs-iot 进程标识(hwid/version)", "gauge");
    metric_line(
        &mut s,
        "rs_iot_info",
        &format!(
            "{{hwid=\"{}\",version=\"{}\"}}",
            escape_label(hwid),
            escape_label(version)
        ),
        "1",
    );

    // 资源
    family_header(&mut s, "rs_iot_uptime_seconds", "进程运行秒数", "gauge");
    metric_line(&mut s, "rs_iot_uptime_seconds", "", &m.started.elapsed().as_secs().to_string());
    family_header(&mut s, "rs_iot_process_cpu_pct", "进程 CPU 占用百分比", "gauge");
    metric_line(&mut s, "rs_iot_process_cpu_pct", "", &cpu.to_string());
    family_header(&mut s, "rs_iot_process_mem_bytes", "进程常驻内存字节", "gauge");
    metric_line(&mut s, "rs_iot_process_mem_bytes", "", &mem.to_string());

    // 控制下行(counter)
    for (name, help, val) in [
        ("rs_iot_ctl_send_total", "控制命令投递尝试总数", m.ctl_send_count.load(l)),
        ("rs_iot_ctl_dropped_total", "控制命令 channel 满丢弃数", m.ctl_dropped_count.load(l)),
        ("rs_iot_ctl_not_found_total", "控制命令未找到目标实例数", m.ctl_not_found_count.load(l)),
    ] {
        family_header(&mut s, name, help, "counter");
        metric_line(&mut s, name, "", &val.to_string());
    }

    // 数据上行(counter)
    for (name, help, val) in [
        ("rs_iot_pipeline_processed_total", "pipeline 处理成功 Sample 数", m.pipeline_processed_count.load(l)),
        ("rs_iot_pipeline_write_fail_total", "pipeline 写 lux 失败次数", m.pipeline_write_fail_count.load(l)),
    ] {
        family_header(&mut s, name, help, "counter");
        metric_line(&mut s, name, "", &val.to_string());
    }

    // hub 背压(gauge)
    family_header(&mut s, "rs_iot_hub_depth", "hub channel 当前深度", "gauge");
    metric_line(&mut s, "rs_iot_hub_depth", "", &m.hub_depth_last.load(l).to_string());
    family_header(&mut s, "rs_iot_hub_depth_max", "hub channel 历史最大深度", "gauge");
    metric_line(&mut s, "rs_iot_hub_depth_max", "", &m.hub_depth_max.load(l).to_string());

    // 北向 mqtt
    family_header(&mut s, "rs_iot_mqtt_publish_total", "mqtt 发布成功次数", "counter");
    metric_line(&mut s, "rs_iot_mqtt_publish_total", "", &m.mqtt_publish_count.load(l).to_string());
    family_header(&mut s, "rs_iot_mqtt_publish_fail_total", "mqtt 发布失败次数", "counter");
    metric_line(&mut s, "rs_iot_mqtt_publish_fail_total", "", &m.mqtt_publish_fail_count.load(l).to_string());
    family_header(&mut s, "rs_iot_mqtt_connected", "mqtt 当前是否已连接(1/0)", "gauge");
    metric_line(
        &mut s,
        "rs_iot_mqtt_connected",
        "",
        if m.mqtt_connected.load(l) { "1" } else { "0" },
    );

    s
}
```

- [ ] **Step 5: 跑测确认通过**

Run: `cargo test --lib render_process`
Expected: 两个测试 PASS。

- [ ] **Step 6: fmt + clippy**

Run: `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings`
Expected: 通过(零警告)。

- [ ] **Step 7: commit**

```bash
git add src/api/routes_metrics.rs src/api/mod.rs
git commit -m "feat(metrics): 进程级 Prometheus 文本 render 函数 + 单测"
```

---

## Task 3: per-instance metrics render 函数

**Files:**
- Modify: `src/api/routes_metrics.rs`(加 `InstanceRow` + `render_instances` + 单测)

**目的:** 把 `InstanceManager::status()`(进程级 state) + lux `stat:<inst>`(插件级 InstanceStat)格式化成 per-instance Prometheus 文本(info metric 暴露 state/health/app_code/host_kind + tick/fail/duration/ts gauge)。本 task 仍纯函数 + 单测。

- [ ] **Step 1: 写失败测**

在 `src/api/routes_metrics.rs` 的 `#[cfg(test)] mod tests` 内追加(需在文件顶部 `use` 区补导入,见 Step 3):

```rust
    use crate::db::stat::InstanceStat;
    use crate::plugin::instance::InstState;
    use crate::types::HostKind;

    fn row(inst_id: &str, state: InstState, app_code: &str, host_kind: &str, health: Option<InstanceStat>) -> InstanceRow {
        InstanceRow {
            inst_id: inst_id.into(),
            state,
            app_code: app_code.into(),
            host_kind,
            health,
        }
    }

    fn stat_running() -> InstanceStat {
        InstanceStat {
            status: "running".into(),
            tick_count: 100,
            fail_count: 2,
            last_activity_ts: 1700000000,
            last_activity_duration_ms: 42,
            last_err: String::new(),
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn render_instances_info_labels() {
        let rows = vec![
            row("opcua-1", InstState::Running, "opcuaClient", "builtin", Some(stat_running())),
            row("lua-1", InstState::Failed, "luaClient", "builtin", None),
        ];
        let out = render_instances(&rows);
        // info family 头一次
        assert!(out.contains("# HELP rs_iot_instance_info "));
        assert!(out.contains("# TYPE rs_iot_instance_info gauge"));
        // 有 stat 的实例:health=running
        assert!(out.contains(
            r#"rs_iot_instance_info{inst_id="opcua-1",app_code="opcuaClient",host_kind="builtin",state="running",health="running"} 1"#
        ));
        // 无 stat 的实例:health 空字符串
        assert!(out.contains(
            r#"rs_iot_instance_info{inst_id="lua-1",app_code="luaClient",host_kind="builtin",state="failed",health=""} 1"#
        ));
    }

    #[test]
    fn render_instances_counters_only_for_stat() {
        let rows = vec![
            row("opcua-1", InstState::Running, "opcuaClient", "builtin", Some(stat_running())),
            row("lua-1", InstState::Failed, "luaClient", "builtin", None),
        ];
        let out = render_instances(&rows);
        // tick/fail 只输出有 stat 的实例
        assert!(out.contains(r#"rs_iot_instance_tick_total{inst_id="opcua-1"} 100"#));
        assert!(out.contains(r#"rs_iot_instance_fail_total{inst_id="opcua-1"} 2"#));
        assert!(out.contains(r#"rs_iot_instance_last_activity_duration_ms{inst_id="opcua-1"} 42"#));
        assert!(out.contains(r#"rs_iot_instance_last_activity_ts{inst_id="opcua-1"} 1700000000"#));
        // 无 stat 的实例不应出现在 tick_total 行
        assert!(!out.contains(r#"{inst_id="lua-1"}"#) || !out.contains("rs_iot_instance_tick_total{inst_id=\"lua-1\"}"),
            "无 stat 实例不应有 tick_total 行");
    }

    #[test]
    fn render_instances_escapes_label_special_chars() {
        // inst_id 含引号/反斜杠 → 必须 escape
        let rows = vec![row(
            r#"a"b\c"#,
            InstState::Running,
            "x",
            "builtin",
            Some(stat_running()),
        )];
        let out = render_instances(&rows);
        assert!(
            out.contains(r#"inst_id="a\"b\\c""#),
            "inst_id 特殊字符应被 escape,实际:\n{out}"
        );
    }
```

- [ ] **Step 2: 跑测确认失败**

Run: `cargo test --lib render_instances`
Expected: 编译失败(`cannot find type InstanceRow` / `cannot find function render_instances`)。

- [ ] **Step 3: 补导入 + 实现 InstanceRow/render_instances**

在 `src/api/routes_metrics.rs` 顶部 `use` 区追加(在现有 `use crate::process_metrics::ProcessMetrics;` 后):

```rust
use crate::db::stat::InstanceStat;
use crate::plugin::instance::InstState;
use crate::types::HostKind;
```

在 `render_process` 函数**之后**加:

```rust
/// per-instance 渲染输入(handler 从 mgr.status() + config_snapshot + lux stat 组装)。
struct InstanceRow {
    inst_id: String,
    state: InstState,
    app_code: String,
    host_kind: &'static str,
    /// 插件级 health + 计数;None = 实例刚启动尚未 flush stat。
    health: Option<InstanceStat>,
}

/// HostKind → 受控词汇字符串(builtin/external,与 routes_stat::host_kind_str 一致)。
fn host_kind_str(k: HostKind) -> &'static str {
    match k {
        HostKind::Builtin => "builtin",
        HostKind::External => "external",
    }
}

/// InstState → lowercase 字符串(serde 派生,新变体自动适配)。取引用避免依赖 Copy。
fn state_str(s: &InstState) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// per-instance 指标 → Prometheus 文本。
///
/// info family 输出所有实例(含无 stat 的,health 空串);tick/fail/duration/ts
/// 只输出有 stat 的实例。`# HELP`/`# TYPE` 每 family 一次。
fn render_instances(rows: &[InstanceRow]) -> String {
    let mut s = String::new();

    // info family(每实例一行)
    family_header(
        &mut s,
        "rs_iot_instance_info",
        "实例分类与状态(inst_id/app_code/host_kind/state/health)",
        "gauge",
    );
    for r in rows {
        let health = r.health.as_ref().map(|h| h.status.as_str()).unwrap_or("");
        let labels = format!(
            "{{inst_id=\"{}\",app_code=\"{}\",host_kind=\"{}\",state=\"{}\",health=\"{}\"}}",
            escape_label(&r.inst_id),
            escape_label(&r.app_code),
            r.host_kind,
            escape_label(&state_str(&r.state)),
            escape_label(health),
        );
        metric_line(&mut s, "rs_iot_instance_info", &labels, "1");
    }

    // 有 stat 的实例(用于 tick/fail/duration/ts)
    let with_stat: Vec<(&InstanceRow, &InstanceStat)> = rows
        .iter()
        .filter_map(|r| r.health.as_ref().map(|h| (r, h)))
        .collect();

    // tick_total(counter)
    family_header(
        &mut s,
        "rs_iot_instance_tick_total",
        "实例活动成功累计次数",
        "counter",
    );
    for (r, h) in &with_stat {
        metric_line(
            &mut s,
            "rs_iot_instance_tick_total",
            &format!("{{inst_id=\"{}\"}}", escape_label(&r.inst_id)),
            &h.tick_count.to_string(),
        );
    }

    // fail_total(counter)
    family_header(
        &mut s,
        "rs_iot_instance_fail_total",
        "实例活动失败累计次数",
        "counter",
    );
    for (r, h) in &with_stat {
        metric_line(
            &mut s,
            "rs_iot_instance_fail_total",
            &format!("{{inst_id=\"{}\"}}", escape_label(&r.inst_id)),
            &h.fail_count.to_string(),
        );
    }

    // last_activity_duration_ms(gauge)
    family_header(
        &mut s,
        "rs_iot_instance_last_activity_duration_ms",
        "实例最近一次活动耗时(毫秒)",
        "gauge",
    );
    for (r, h) in &with_stat {
        metric_line(
            &mut s,
            "rs_iot_instance_last_activity_duration_ms",
            &format!("{{inst_id=\"{}\"}}", escape_label(&r.inst_id)),
            &h.last_activity_duration_ms.to_string(),
        );
    }

    // last_activity_ts(gauge, unix 秒)
    family_header(
        &mut s,
        "rs_iot_instance_last_activity_ts",
        "实例最近一次活动墙上时间(unix 秒)",
        "gauge",
    );
    for (r, h) in &with_stat {
        metric_line(
            &mut s,
            "rs_iot_instance_last_activity_ts",
            &format!("{{inst_id=\"{}\"}}", escape_label(&r.inst_id)),
            &h.last_activity_ts.to_string(),
        );
    }

    s
}
```

- [ ] **Step 4: 跑测确认通过**

Run: `cargo test --lib render_instances`
Expected: 三个测试 PASS。

- [ ] **Step 5: fmt + clippy**

Run: `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings`
Expected: 通过(零警告)。

- [ ] **Step 6: commit**

```bash
git add src/api/routes_metrics.rs
git commit -m "feat(metrics): per-instance Prometheus 文本 render 函数 + 单测"
```

---

## Task 4: handler + 路由注册 + 白名单 + 集成测

**Files:**
- Modify: `src/api/routes_metrics.rs`(加 `metrics` handler)
- Modify: `src/api/mod.rs`(`build_router` 注册 `GET /metrics` + `PUBLIC_PATHS` 加 `/metrics`)
- Create: `tests/metrics_e2e.rs`(集成测)

**目的:** 接通 handler(组合 render_process + render_instances,遍历实例 + 读 lux stat),注册路由 + 免 JWT 白名单,集成测验证端到端。

- [ ] **Step 1: 写失败测(集成测)**

创建 `tests/metrics_e2e.rs`:

```rust
//! /metrics 端到端:axum oneshot,验证免 JWT + 200 + text/plain + 进程级 metric。

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rs_iot::api::{build_router, AppState};
use rs_iot::host::{CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::types::Hwid;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

async fn test_app() -> (axum::Router, lux::ServerHandle) {
    let data_dir = common::test_data_dir("metrics");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    let shutdown = CancellationToken::new();
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown,
        ctl_registry: CtlRegistry::new(std::sync::Arc::new(
            rs_iot::process_metrics::ProcessMetrics::new(),
        )),
        bridge_registry: rs_iot::host::BridgeRegistry::new(),
        metrics: std::sync::Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
    };
    let (hub_tx, _hub_rx) = tokio::sync::mpsc::channel(64);
    let mgr = Arc::new(InstanceManager::new(ctx, hub_tx));
    let state = AppState {
        lux: client.clone(),
        mgr,
        hwid: Hwid::new("it-hwid"),
        jwt_secret: "test-secret-0123456789abcdef".into(),
        version: "itest",
        metrics: std::sync::Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
    };
    (build_router(state), handle)
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("read body");
    String::from_utf8(bytes).expect("utf8 body")
}

#[tokio::test]
async fn metrics_is_public_returns_text() {
    let (app, handle) = test_app().await;

    // 不带 JWT(Prometheus scrape 场景)→ 应 200(非 401)
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/metrics")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("call app");
    assert_eq!(resp.status(), StatusCode::OK, "免 JWT 应放行");
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .expect("content-type")
        .to_str()
        .expect("utf8")
        .to_string();
    assert!(ct.contains("text/plain"), "content-type 应含 text/plain,实际 {ct}");
    let body = body_text(resp).await;
    // 进程级 metric 齐全
    assert!(body.contains("rs_iot_info{hwid=\"it-hwid\""), "含进程标识");
    assert!(body.contains("# TYPE rs_iot_ctl_send_total counter"));
    assert!(body.contains("rs_iot_uptime_seconds"));
    assert!(body.contains("rs_iot_process_cpu_pct"));
    assert!(body.contains("rs_iot_mqtt_connected"));
    // per-instance family 头存在(无实例时只有头,无行)
    assert!(body.contains("# TYPE rs_iot_instance_info gauge"));

    common::stop_lux(handle).await;
}

#[tokio::test]
async fn metrics_survives_stat_read_failure() {
    // 进程级指标在任何情况下都应输出(单实例 stat 读失败不应让 /metrics 500)。
    let (app, handle) = test_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/metrics")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("call app");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_text(resp).await;
    assert!(!body.is_empty(), "body 非空");

    common::stop_lux(handle).await;
}
```

- [ ] **Step 2: 跑测确认失败**

Run: `cargo test --test metrics_e2e`
Expected: 编译失败或 404(`GET /metrics` 路由未注册)。

- [ ] **Step 3: 实现 metrics handler**

在 `src/api/routes_metrics.rs` 顶部 `use` 区追加:

```rust
use crate::api::AppState;
use axum::extract::State;
use axum::response::IntoResponse;
```

在 `render_instances` 函数**之后**加 handler:

```rust
/// `GET /metrics`(Prometheus 文本格式,免 JWT)。
///
/// 组合进程级 + per-instance 指标。per-instance 遍历 `mgr.status()`(进程级 state)
/// + `config_snapshot`(分类) + lux `stat:<inst>`(health/计数)。单实例 stat 读失败
/// 跳过其 per-instance 行,进程级指标照常输出(失败降级,见 spec §6)。
pub async fn metrics(State(st): State<AppState>) -> axum::response::Response {
    let m = &st.metrics;
    let (cpu, mem) = crate::process_metrics::sample_process_resource();

    let mut body = render_process(m, st.hwid.as_ref(), st.version, cpu, mem);

    // per-instance:遍历实例 + 读 lux stat。
    let states = st.mgr.status();
    let mut rows: Vec<InstanceRow> = Vec::with_capacity(states.len());
    for (inst_id, state) in states {
        let class = st.mgr.config_snapshot(&inst_id);
        let (app_code, host_kind) = class
            .as_ref()
            .map(|c| (c.app_code.to_string(), host_kind_str(c.host_kind)))
            .unwrap_or((String::new(), ""));
        // stat 读失败 → None(该实例 per-instance 计数缺,但 info 行仍输出)。
        let health = crate::db::stat::read(&st.lux, &inst_id)
            .await
            .ok()
            .flatten();
        rows.push(InstanceRow {
            inst_id,
            state,
            app_code,
            host_kind,
            health,
        });
    }
    body.push_str(&render_instances(&rows));

    ([(axum::http::header::CONTENT_TYPE, METRICS_CONTENT_TYPE)], body).into_response()
}
```

- [ ] **Step 4: 注册路由 + 白名单**

`src/api/mod.rs` 的 `build_router`,在 `.route("/api/v1/sysinfo", ...)` 那组**之前**加(`/metrics` 顶层路径,与 Prometheus 惯例一致):

```rust
        // Prometheus 抓取端点(免 JWT,机器出口;spec §3)
        .route("/metrics", get(routes_metrics::metrics))
```

`src/api/mod.rs` 的 `PUBLIC_PATHS`(第 39 行)加 `/metrics`:

旧:
```rust
pub const PUBLIC_PATHS: &[&str] = &["/api/v1/user/login", "/api/v1/user/register"];
```

新:
```rust
pub const PUBLIC_PATHS: &[&str] = &[
    "/api/v1/user/login",
    "/api/v1/user/register",
    "/metrics",
];
```

- [ ] **Step 5: 跑测确认通过**

Run: `cargo test --test metrics_e2e`
Expected: 两个测试 PASS(免 JWT 200 + text/plain + 进程级 metric)。

- [ ] **Step 6: fmt + clippy + 全量测**

Run: `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets --all-features`
Expected: fmt 通过、clippy 零警告、全量测通过(含既有 `api_flow` 鉴权测不破坏——`/metrics` 在白名单内,其余路径仍要 JWT)。

- [ ] **Step 7: commit**

```bash
git add src/api/routes_metrics.rs src/api/mod.rs tests/metrics_e2e.rs
git commit -m "feat(metrics): /metrics 端点 + 免 JWT 白名单 + 端到端集成测"
```

---

## 验证清单(全部 task 完成后)

- [ ] `cargo fmt --all --check` 通过
- [ ] `cargo clippy --all-targets -- -D warnings` 零警告
- [ ] `cargo test --all-targets --all-features` 全绿(含 `metrics_e2e` + 既有 `api_flow` 鉴权不破坏)
- [ ] `cargo doc --no-deps`(RUSTFLAGS=-D warnings)零警告
- [ ] 端到端(可选,需运行实例):`./target/debug/rs-iot --config config.toml &` → `curl -s localhost:8780/metrics | head` → 见 `rs_iot_*` metric 行 + `# HELP`/`# TYPE`
