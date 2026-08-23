# 实时数据订阅接口（OPC UA 式）实施计划

> **状态**：implemented（2026-08-23 于 worktree feat/rtdata-subscribe 执行完毕，T1-T6 全落地，子代理驱动 + task 间审查；OPC UA 南向原生订阅不在本计划——用户明示下一轮另立）
> **日期**：2026-08-23
> **实施**：T1 tag_of 提升（13e16d7）→ T2 纯函数 TDD 10 单测（2f02869）→ T3 端点骨架+路由+401/200 e2e（b200986）→ T4 snapshot 全量+过滤（188ceb6）→ T5 datachange+removed（d9bdc89）→ T6 文档收口+全量验证
> **代码**：`src/api/routes_rtdata_subscribe.rs`（新增）、`src/api/mod.rs`（模块声明 + 路由注册）、`src/api/routes_rtdata.rs`（`tag_of` 提升 `pub(crate)`）
> **测试**：`tests/rtdata_subscribe_e2e.rs`（6 e2e：401/200/snapshot 全量/dev+tag 过滤/值变才推/removed）+ `src/api/routes_rtdata_subscribe.rs` 内单测（10）
> **遗留**：可选增强未做：订阅者数/帧量 Prometheus 指标、点级精确订阅白名单、超大单设备 snapshot 分帧、多订阅者共享 diff 服务、deadband 死区过滤（SampleValue 无类型元数据）

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 新增 `GET /api/v1/rtdatas/subscribe` SSE 端点——外部客户端订阅 rs-iot 实时数据：连接建立即推**全量 snapshot**，之后按订阅周期**只推值变化/删除的 datachange**（对标 OPC UA CreateSubscription 语义：初次全量 + 周期变化推送，支持万点级）。

**Architecture:** 每连接独立 producer task 做**读侧 poll-diff-push**（HGETALL 订阅范围 → 与已推基线比对 cell 值串 → 变化才推帧），**不经 EventBus**——绕过 pipeline 聚合的 `RTDATA_NOTIFY_MAX_DEVS=50` 截断（`src/pipeline.rs:235`）与 broadcast 容量 256 的 Lagged 丢帧（订阅定位是外部客户端可靠数据面）；interval 天然 per-connection（对标 OPC UA publishing interval）；断线重连即重新 snapshot 全量重同步。pipeline / event_bus **零改动**——现有 `/api/v1/events` 与 lua_transform 订阅面不受影响。

**Tech Stack:** axum 0.8 SSE（`Event`/`KeepAlive`/`Sse` + mpsc → `ReceiverStream`，模式同 `src/api/routes_events.rs`）、lux `EmbeddedClient`（`KEYS`/`HGETALL`，复用 `api::hgetall`）、既有 JWT 中间件（Authorization / cookie / X-Access-Key，零新增鉴权面）。

**执行前置：** 跨 ≥5 文件 ≥6 commit，**必须在独立 worktree 执行**：`git worktree add ../rs-iot-rtdata-sub -b feat/rtdata-subscribe`（worktree 与主仓同级，`../lux` path 依赖解析关系不变，同 stage-b 模式）。

**约定：** 包名 `rs-iot`；每 Task 末 commit（Conventional Commits + 中文）；测试先写失败再实现；单测 `cargo test -p rs-iot --lib api::routes_rtdata_subscribe`，e2e `cargo test -p rs-iot --test rtdata_subscribe_e2e`；文档改动后 `python scripts/check_docs.py` 本地过一遍再提交。

---

## 已定取舍（计划级决策，执行不再议）

1. **机制 = 每连接读侧 poll-diff，不走 EventBus。** EventBus 路径有两处丢数据特性（pipeline 聚合 50 dev 截断、broadcast Lagged 丢帧），二者对 Web 前端可接受（2s 轮询兜底），对"外部客户端可靠订阅"不可接受。读侧每 tick 全量重读 + 比对天然无损：丢帧不存在（状态机式 diff），慢消费只延迟不丢失（mpsc 背压 + tick Skip，恢复后下一个 tick 从全量状态重算）。代价是每连接每 tick 一次 HGETALL（万点量级 ms 级，lux bench 写侧 1.65M 点/秒），多订阅者各自重读——订阅者数量大时的共享 diff 服务列为遗留增强。
2. **值变化判定 = cell[2] 值串比较。** rtdata cell `[ts, localTime, value]` 的 value 恒为字符串（`db::rtdata::write_batch` 统一 `to_string` 落库），比较该字符串即可。ts 变化值不变**不算变化**——对标 OPC UA DataChangeFilter 的值触发语义；采集插件（如 opcuaClient 周期 Read）写回同值时不产生通知。无 deadband（死区/阈值过滤）——SampleValue 无类型元数据，遗留。
3. **帧格式与 `GET /api/v1/rtdatas` 输出同构。** 点位 cell 同为 `[ts, localTime, value]`（value 字符串），客户端复用同一解析逻辑；field 名同为 `<devid>.<tag>`。
4. **snapshot 按设备分帧。** HGETALL 天然按 `rtdata:<devid>` key 粒度，一设备一帧是自然分块；单设备超大点位（如 10k 点单设备）一帧 1~2MB，v1 接受（SSE 无帧大小限制），超大分帧列为遗留。
5. **过滤参数对齐 `/rtdatas` 语义。** `dev_id` 逗号分隔精确匹配（缺省 = 全部设备，每 tick 重查 `KEYS rtdata:*`，新设备自动纳入）；`tag` 取 field 末段 contains（复用 `tag_of`）。不做点级精确白名单（YAGNI，遗留）。
6. **interval clamp [100ms, 60000ms]，默认 1000ms。** 下限是成本护栏（每连接每 tick 全量 HGETALL，防外部客户端 10ms 高频滥用）；interval 语义 = 推送周期（对标 publishing interval），**不是**采样周期（数据新鲜度由采集实例的 period 决定，rs-iot 是最新值缓存模型）。

## 接口规格

`GET /api/v1/rtdatas/subscribe` → `text/event-stream`（JWT / X-Access-Key 保护，同其余 `/api/v1`）

| 参数 | 说明 |
|---|---|
| `dev_id` | 可选。设备 ID 逗号列表，精确匹配（`?dev_id=devA,devB`）；缺省 = 全部设备 |
| `tag` | 可选。tag 模糊过滤（field 末段 contains，语义同 `GET /rtdatas?tag=`） |
| `interval` | 可选。订阅周期 ms，默认 1000，clamp [100, 60000] |

帧类型（`event:` 头，EventSource `addEventListener` 分发）：

```
# 连接建立即推（一设备一帧）：
event: snapshot
data: {"dev":"devA","points":{"devA.temp":[1724400000,"2026-08-23 15:00:00","25.5"],...}}

# 之后每 interval，仅当有值变化/删除（一 tick 一帧）：
event: datachange
data: {"changes":{"devA.temp":[1724400100,"2026-08-23 15:00:10","26.0"]},"removed":["devB.old1"],"ts":"2026-08-23T07:00:10.000Z"}

# 无事件时 15s 一次注释帧（: —— axum KeepAlive 默认，代理防超时）
```

语义保证：

- **初次全量**：连接建立即逐设备推订阅范围内全部点位（snapshot 建基线）。
- **只推变化**：值串（cell[2]）与上次已推值不同才进 `changes`；周期写回同值不产生通知。
- **删除通知**：设备删除（`DEL rtdata:<dev>`）或点位消失 → 该 tick 的 `datachange.removed` 列出。
- **重连即重同步**：断线重连（EventSource 自动重连）触发新 snapshot，无需 resync 协议。
- **鉴权**：`Authorization: Bearer <jwt>` / cookie `token=` / `X-Access-Key`，同全站中间件。

非目标（本计划不做）：前端 monitor store 切换到本端点（外部客户端 API，前端保持既有 SSE+轮询）；rsiot-gateway 适配（纯反代 `/api/v1/*` 自动透传，零改动）；rsiot-mcp 工具暴露；OPC UA 南向原生订阅（用户明示下一轮）。

---

## Task 1: `tag_of` 提升 `pub(crate)`

订阅模块复用 `/rtdatas` 的 tag 末段语义，避免复制。

**Files:**
- Modify: `src/api/routes_rtdata.rs:35`

**Step 1: 修改可见性**

```rust
/// 取 field 末段（`<devid>.<tag>` → `<tag>`；devid 不含 `.`，故 split 末段即 tag）。
pub(crate) fn tag_of(field: &str) -> &str {
```

（仅 `fn` 前加 `pub(crate)`，doc 注释不变。）

**Step 2: 跑既有测试确认零行为变化**

Run: `cargo test -p rs-iot --lib api::routes_rtdata`
Expected: PASS（含 `tag_of_takes_last_segment` 等既有 6 测）

**Step 3: Commit**

```bash
git add src/api/routes_rtdata.rs
git commit -m "refactor(api): tag_of 提升 pub(crate)——订阅端点复用 tag 末段语义（零行为变化）"
```

---

## Task 2: 订阅模块骨架 + 纯函数 TDD

先落地全部纯函数（clamp / dev 解析 / 值串提取 / diff / tag 过滤）与单测——producer 是这些函数的组装，纯函数测全了端点只剩接线。

**Files:**
- Create: `src/api/routes_rtdata_subscribe.rs`（本 Task：常量 + 5 纯函数 + 单测；完整最终形态见附录 A）
- Modify: `src/api/mod.rs:26`（`pub mod routes_rtdata;` 后加一行模块声明）

**Step 1: 注册模块**（`src/api/mod.rs`，`pub mod routes_rtdata;` 之后）

```rust
pub mod routes_rtdata_subscribe;
```

**Step 2: 写失败测试**（新文件 `src/api/routes_rtdata_subscribe.rs`，先只写模块 doc + 常量 + tests；函数未定义）

文件头（模块 doc 说明机制取舍，最终文件保留）：

```rust
//! 实时数据订阅端点（OPC UA 式订阅语义；设计见
//! docs/designs/2026-08-23-rtdata-subscribe-plan.md）。
//!
//! `GET /api/v1/rtdatas/subscribe?dev_id=a,b&tag=x&interval=1000` → SSE：
//! - **snapshot**：连接建立即逐设备推全量快照（HGETALL `rtdata:<dev>`，一设备一帧）；
//! - **datachange**：之后每 `interval`（默认 1s，clamp [100ms,60s]）重读订阅范围，
//!   与上次已推值比对（仅比 cell[2] 值串——ts 变化值不变不算），有变化/删除才推一帧。
//!
//! 机制取舍：**每连接读侧 poll-diff，不走 EventBus**——绕过 pipeline 聚合的
//! `RTDATA_NOTIFY_MAX_DEVS=50` 截断与 broadcast 容量 256 的 Lagged 丢帧（订阅定位
//! 外部客户端可靠数据面）；interval 天然 per-connection（对标 OPC UA publishing
//! interval）；断线重连即重新 snapshot 全量重同步。pipeline / event_bus 零改动
//! ——现有 `/api/v1/events` 与 lua_transform 订阅面不受影响。

/// 订阅周期下限 ms（防高频滥用——每连接每 tick 全量 HGETALL 的成本护栏）。
const MIN_INTERVAL_MS: u64 = 100;
/// 订阅周期上限 ms。
const MAX_INTERVAL_MS: u64 = 60_000;
/// 默认订阅周期 ms。
const DEFAULT_INTERVAL_MS: u64 = 1_000;
```

tests 模块（完整 10 测，`#[cfg(test)] mod tests`）：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn clamp_interval_defaults_and_bounds() {
        assert_eq!(clamp_interval(None), DEFAULT_INTERVAL_MS);
        assert_eq!(clamp_interval(Some(0)), MIN_INTERVAL_MS);
        assert_eq!(clamp_interval(Some(50)), MIN_INTERVAL_MS);
        assert_eq!(clamp_interval(Some(2000)), 2000);
        assert_eq!(clamp_interval(Some(u64::MAX)), MAX_INTERVAL_MS);
    }

    #[test]
    fn parse_dev_ids_none_empty_or_all_blank() {
        assert!(parse_dev_ids(None).is_none());
        assert!(parse_dev_ids(Some("")).is_none());
        assert!(parse_dev_ids(Some("  ")).is_none());
        assert!(parse_dev_ids(Some(",,")).is_none(), "全空段 = None（全部设备）");
    }

    #[test]
    fn parse_dev_ids_splits_trims_and_dedups() {
        let s = parse_dev_ids(Some("devA, devB ,devA")).unwrap();
        assert_eq!(s.len(), 2, "去重");
        assert!(s.contains("devA") && s.contains("devB"), "trim 生效");
    }

    #[test]
    fn value_str_takes_third_element() {
        assert_eq!(value_str(&json!([1, "t", "1.5"])), "1.5");
        assert_eq!(value_str(&json!([1, "t", "true"])), "true");
    }

    #[test]
    fn value_str_malformed_falls_back_to_json() {
        // 越界（无第三元素）→ 整体 JSON 串兜底，而非空串误判恒等。
        assert_eq!(value_str(&json!([1, "t"])), json!([1, "t"]).to_string());
        // 非数组 → 同上。
        assert_eq!(value_str(&json!("x")), "\"x\"");
    }

    /// 构造 diff 输入：field → (值串, cell)。
    fn scope(pairs: &[(&str, &str)]) -> HashMap<String, (String, serde_json::Value)> {
        pairs
            .iter()
            .map(|(f, v)| ((*f).to_string(), ((*v).to_string(), json!([1, "lt", v]))))
            .collect()
    }

    /// 构造已推基线：field → 值串。
    fn baseline(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(f, v)| ((*f).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn diff_detects_value_change_and_new_field() {
        let last = baseline(&[("d.t1", "1"), ("d.t2", "2")]);
        let cur = scope(&[("d.t1", "9"), ("d.t2", "2"), ("d.t3", "3")]);
        let (changes, removed) = diff_scope(cur, &last);
        assert_eq!(changes.len(), 2, "t1 值变 + t3 新增");
        assert!(changes.contains_key("d.t1") && changes.contains_key("d.t3"));
        assert!(removed.is_empty());
    }

    #[test]
    fn diff_same_value_new_ts_not_a_change() {
        // OPC UA 语义核心：值同 ts 变（周期写回同值）不产生通知。
        let last = baseline(&[("d.t1", "1")]);
        let cur: HashMap<String, (String, serde_json::Value)> = [(
            "d.t1".to_string(),
            ("1".to_string(), json!([999, "lt", "1"])),
        )]
        .into_iter()
        .collect();
        let (changes, removed) = diff_scope(cur, &last);
        assert!(changes.is_empty() && removed.is_empty(), "值未变不推: {changes:?}");
    }

    #[test]
    fn diff_reports_removed_fields() {
        let last = baseline(&[("d.t1", "1"), ("d.t2", "2")]);
        let cur = scope(&[("d.t1", "1")]);
        let (changes, removed) = diff_scope(cur, &last);
        assert!(changes.is_empty());
        assert_eq!(removed, vec!["d.t2".to_string()]);
    }

    #[test]
    fn diff_empty_current_removes_all() {
        // 设备删除后其全部点位进 removed（e2e removed_on_device_delete 的纯函数面）。
        let last = baseline(&[("d.t1", "1")]);
        let (changes, removed) = diff_scope(HashMap::new(), &last);
        assert!(changes.is_empty());
        assert_eq!(removed.len(), 1);
    }

    #[test]
    fn filter_by_tag_keeps_matching_tail() {
        let m: serde_json::Map<String, serde_json::Value> = [
            ("devA.temp".to_string(), json!([0, "t", "1"])),
            ("devA.hum".to_string(), json!([0, "t", "2"])),
        ]
        .into_iter()
        .collect();
        let got = filter_by_tag(m, Some("temp"));
        assert_eq!(got.len(), 1);
        assert!(got.contains_key("devA.temp"));
        // None → 全保留。
        let m2: serde_json::Map<String, serde_json::Value> =
            [("a.b".to_string(), json!(1))].into_iter().collect();
        assert_eq!(filter_by_tag(m2, None).len(), 1);
    }
}
```

**Step 3: 跑测试确认失败（编译错 = 红）**

Run: `cargo test -p rs-iot --lib api::routes_rtdata_subscribe`
Expected: FAIL——`error[E0425]/[E0432]: cannot find function clamp_interval ...`（5 函数未定义）

**Step 4: 实现 5 个纯函数**（tests 模块之前插入；签名与实现为最终形态，见附录 A §纯函数）

```rust
use std::collections::HashSet;

/// interval clamp：缺省 DEFAULT，夹到 [MIN, MAX]。纯函数便于单测。
pub fn clamp_interval(raw: Option<u64>) -> u64 {
    raw.unwrap_or(DEFAULT_INTERVAL_MS)
        .clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS)
}

/// dev_id 逗号列表 → 去重集合。None/空白/全空段 → None（= 全部设备）。
pub fn parse_dev_ids(raw: Option<&str>) -> Option<HashSet<String>> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    let set: HashSet<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    if set.is_empty() { None } else { Some(set) }
}

/// cell（`[ts, localTime, value]`）→ 值串（diff 比较键）。非数组/越界/非串 → 整体
/// JSON 串兜底（防畸形 cell 误判恒等）。
pub fn value_str(cell: &serde_json::Value) -> String {
    cell.get(2)
        .and_then(serde_json::Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| cell.to_string())
}

/// 全订阅范围 diff：`current` = 本 tick 过滤后的 field → (值串, cell)；
/// `last_sent` = 已推基线 field → 值串。返回 (变更 field → cell, 消失 field 列表)。
///
/// **值串比较**——ts/localTime 变化值不变不算变更（对标 OPC UA DataChangeFilter
/// 的值触发语义；采集插件周期写回同值不产生通知）。
pub fn diff_scope(
    current: HashMap<String, (String, serde_json::Value)>,
    last_sent: &HashMap<String, String>,
) -> (serde_json::Map<String, serde_json::Value>, Vec<String>) {
    let mut now: HashSet<&str> = HashSet::with_capacity(current.len());
    let mut changes = serde_json::Map::new();
    for (field, (val, cell)) in &current {
        now.insert(field.as_str());
        if last_sent.get(field) != Some(val) {
            changes.insert(field.clone(), cell.clone());
        }
    }
    let removed: Vec<String> = last_sent
        .keys()
        .filter(|f| !now.contains(f.as_str()))
        .cloned()
        .collect();
    (changes, removed)
}

/// tag contains 过滤（field 末段匹配，复用 `GET /rtdatas` 的 tag_of 语义）。None 全保留。
pub fn filter_by_tag(
    m: serde_json::Map<String, serde_json::Value>,
    tag: Option<&str>,
) -> serde_json::Map<String, serde_json::Value> {
    match tag {
        None => m,
        Some(t) => m
            .into_iter()
            .filter(|(k, _)| tag_of(k).contains(t))
            .collect(),
    }
}
```

（`use crate::api::routes_rtdata::tag_of;` 加到文件头 use 区。）

**Step 5: 跑测试确认通过**

Run: `cargo test -p rs-iot --lib api::routes_rtdata_subscribe`
Expected: PASS（10 tests）

**Step 6: Commit**

```bash
git add src/api/routes_rtdata_subscribe.rs src/api/mod.rs
git commit -m "feat(api): rtdata 订阅模块纯函数骨架——interval clamp/dev 解析/值 diff/removed（TDD）"
```

---

## Task 3: 端点骨架 + 路由注册 + 401/200 e2e

**Files:**
- Create: `tests/rtdata_subscribe_e2e.rs`（本 Task：测试基建 + 2 测；完整最终形态见附录 B）
- Modify: `src/api/mod.rs:176`（路由注册，`/api/v1/rtdatas` 行后）
- Modify: `src/api/routes_rtdata_subscribe.rs`（加 `SubQuery` + `subscribe` 骨架）

**Step 1: 写 e2e 测试基建 + 2 个失败测试**（新文件 `tests/rtdata_subscribe_e2e.rs`；`test_app`/`jwt`/`seed`/`connect`/`collect_window` 全量见附录 B，本 Task 先写 `test_app`/`jwt`/`connect`/`collect_window` + 2 测，`seed` 在 Task 4 加）

```rust
//! 实时数据订阅端到端：OPC UA 式语义（snapshot 全量 → 周期 datachange 只推变化）。
//!
//! 覆盖：JWT 保护、snapshot 全量+过滤、值变才推（值同不推）、设备删除 removed。
//! 测试基建对标 tests/events_e2e.rs（同 test_app 骨架）；SSE 读流用窗口收集断言
//! （先收 snapshot 窗口，再变更数据，再收 datachange 窗口）。

mod common;

use axum::body::{Body, BodyDataStream};
use axum::http::{header, Request, StatusCode};
use futures_util::StreamExt;
use rs_iot::api::{build_router, AppState};
use rs_iot::event_bus::EventBus;
use rs_iot::host::{CtlRegistry, HostContext};
use rs_iot::log_ringbuffer::LogRingBuffer;
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::types::Hwid;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// 建测试 app（订阅端点读 lux，不依赖 EventBus 事件流；骨架同 events_e2e 便于对照）。
async fn test_app(name: &str) -> (axum::Router, lux::ServerHandle, lux::EmbeddedClient) {
    let data_dir = common::test_data_dir(name);
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client).await.expect("bootstrap");

    let shutdown = CancellationToken::new();
    let metrics = Arc::new(rs_iot::process_metrics::ProcessMetrics::new());
    let event_bus = EventBus::new(256);
    let trace_registry = rs_iot::io_trace::TraceRegistry::with_sender(event_bus.clone());
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown,
        ctl_registry: CtlRegistry::new(metrics.clone()),
        bridge_registry: rs_iot::host::BridgeRegistry::new(),
        metrics: metrics.clone(),
        trace_registry: trace_registry.clone(),
        event_tx: event_bus.clone(),
    };
    let (hub_tx, _hub_rx) = tokio::sync::mpsc::channel(64);
    let mgr = Arc::new(InstanceManager::new(ctx, hub_tx));
    let state = AppState {
        lux: client.clone(),
        mgr,
        hwid: Hwid::new("it-hwid"),
        jwt_secret: "test-secret-0123456789abcdef".into(),
        version: "itest",
        metrics: metrics.clone(),
        trace_registry,
        log_ringbuffer: Arc::new(LogRingBuffer::new(2000)),
        event_bus,
        config: std::sync::Arc::new(rs_iot::config::AppConfig::default()),
    };
    (build_router(state), handle, client)
}

/// 登录拿 JWT（admin/admin）。
async fn jwt(app: &axum::Router) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/user/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"user_id":"admin","pass":"admin"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    v["token"].as_str().unwrap().to_string()
}

/// 建订阅连接（带 JWT）：断言 200 + text/event-stream，返回 body 流。
async fn connect(app: &axum::Router, query: &str, tok: &str) -> BodyDataStream {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/rtdatas/subscribe{query}"))
                .header(header::AUTHORIZATION, tok)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/event-stream"
    );
    resp.into_body().into_data_stream()
}

/// 收集窗口：读流 dur 时长（或流尽）→ String（窗口式断言基建，events_e2e 内联模式的函数化）。
async fn collect_window(stream: &mut BodyDataStream, dur: std::time::Duration) -> String {
    let mut buf = String::new();
    let _ = tokio::time::timeout(dur, async {
        while let Some(chunk) = stream.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        }
    })
    .await;
    buf
}

#[tokio::test]
async fn subscribe_without_jwt_401() {
    let (app, handle, _lux) = test_app("sub401").await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/rtdatas/subscribe")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn subscribe_with_jwt_200_event_stream() {
    let (app, handle, _lux) = test_app("sub200").await;
    let tok = jwt(&app).await;
    let mut stream = connect(&app, "", &tok).await;
    // 骨架（空流）下流立即结束——窗口收集返回空或 keep-alive，不卡死即可。
    let buf = collect_window(&mut stream, std::time::Duration::from_millis(300)).await;
    assert!(buf.len() < 1 << 20);
    common::stop_lux(handle).await;
}
```

**Step 2: 跑 e2e 确认失败**

Run: `cargo test -p rs-iot --test rtdata_subscribe_e2e`
Expected: FAIL——带 JWT 的测拿到 404（路由未注册）；401 测可能已绿（中间件先于 fallback 命中时），以 404 的那个为准红。

**Step 3: 路由注册 + 端点骨架**

`src/api/mod.rs`（`/api/v1/rtdatas` 路由行后加）：

```rust
        // 实时数据订阅（OPC UA 式：snapshot 全量 + 周期 datachange；
        // 设计 docs/designs/2026-08-23-rtdata-subscribe-plan.md）
        .route(
            "/api/v1/rtdatas/subscribe",
            get(routes_rtdata_subscribe::subscribe),
        )
```

`src/api/routes_rtdata_subscribe.rs`（use 区补 SSE 相关；tests 模块前加）：

```rust
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use serde::Deserialize;
use std::convert::Infallible;
use tokio_stream::wrappers::ReceiverStream;

/// query 参数（均可选）。
#[derive(Deserialize)]
pub struct SubQuery {
    /// 设备 ID 逗号列表（精确匹配；缺省 = 全部设备，每 tick 重查 KEYS，新设备自动纳入）。
    pub dev_id: Option<String>,
    /// tag 模糊过滤（field 末段 contains；语义同 `GET /api/v1/rtdatas?tag=`）。
    pub tag: Option<String>,
    /// 订阅周期 ms（clamp [100, 60000]，缺省 1000）。
    pub interval: Option<u64>,
}

/// `GET /api/v1/rtdatas/subscribe`：OPC UA 式实时数据订阅（SSE）。
///
/// T3 骨架：空流（headers + 立即结束）；T4 起挂 producer（snapshot + 周期 diff）。
/// 客户端断开 → mpsc send 失败 → producer task 退出（资源回收）。
pub async fn subscribe() -> impl IntoResponse {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(8);
    drop(tx);
    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}
```

**Step 4: 跑 e2e 确认通过**

Run: `cargo test -p rs-iot --test rtdata_subscribe_e2e`
Expected: PASS（2 tests）

**Step 5: Commit**

```bash
git add tests/rtdata_subscribe_e2e.rs src/api/mod.rs src/api/routes_rtdata_subscribe.rs
git commit -m "feat(api): /api/v1/rtdatas/subscribe 端点骨架——路由注册+JWT 401/200 e2e"
```

---

## Task 4: snapshot 全量推送 + dev_id/tag 过滤

producer Phase 1：连接建立读订阅范围（`read_scope`：dev 过滤 → 固定 key 列表；无过滤 → `KEYS rtdata:*`），逐设备 tag 过滤后推 snapshot 帧并建基线。

**Files:**
- Modify: `src/api/routes_rtdata_subscribe.rs`（`read_scope` + `run` Phase 1 + `subscribe` 换真 producer，最终形态见附录 A）
- Modify: `tests/rtdata_subscribe_e2e.rs`（`seed` helper + 2 新测）

**Step 1: 写失败测试**（`tests/rtdata_subscribe_e2e.rs`；use 区补 `use rs_iot::types::{Sample, SampleValue}; use std::collections::BTreeMap;`）

```rust
/// 种子数据：write_batch 一条 Sample（field 形如 <devid>.<tag>，cell[2] 为值串）。
async fn seed(lux: &lux::EmbeddedClient, devid: &str, pts: &[(&str, &str)], ts: i64) {
    let mut values = BTreeMap::new();
    for (t, v) in pts {
        values.insert((*t).to_string(), SampleValue::Str((*v).to_string()));
    }
    let s = Sample {
        hwid: Hwid::new("it-hwid"),
        devid: devid.into(),
        values,
        timestamp: ts,
    };
    rs_iot::db::rtdata::write_batch(lux, std::slice::from_ref(&s))
        .await
        .unwrap();
}

#[tokio::test]
async fn snapshot_pushes_all_points_on_connect() {
    let (app, handle, lux) = test_app("subsnap").await;
    seed(&lux, "devA", &[("t1", "1.5"), ("t2", "2")], 1_700_000_000).await;
    seed(&lux, "devB", &[("t1", "3")], 1_700_000_000).await;
    let tok = jwt(&app).await;
    let mut stream = connect(&app, "?interval=200", &tok).await;
    let buf = collect_window(&mut stream, std::time::Duration::from_millis(800)).await;
    // 两设备各一帧 snapshot；cell 形如 [ts,"localTime","value"]（值串，与 /rtdatas 同构）。
    assert!(buf.contains("event: snapshot"), "应含 snapshot 帧: {buf}");
    assert!(
        buf.contains("\"dev\":\"devA\"") && buf.contains("\"dev\":\"devB\""),
        "逐设备分帧: {buf}"
    );
    assert!(buf.contains("devA.t1") && buf.contains("1.5"), "{buf}");
    assert!(buf.contains("devA.t2") && buf.contains("devB.t1"), "{buf}");
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn snapshot_respects_dev_and_tag_filter() {
    let (app, handle, lux) = test_app("subfilter").await;
    seed(&lux, "devA", &[("temp", "1"), ("hum", "2")], 1).await;
    seed(&lux, "devB", &[("temp", "3")], 1).await;
    let tok = jwt(&app).await;
    let mut stream = connect(&app, "?dev_id=devA&tag=temp&interval=200", &tok).await;
    let buf = collect_window(&mut stream, std::time::Duration::from_millis(800)).await;
    assert!(buf.contains("devA.temp"), "{buf}");
    assert!(!buf.contains("devA.hum"), "tag 过滤应排除: {buf}");
    assert!(!buf.contains("devB"), "dev 过滤应排除 devB: {buf}");
    common::stop_lux(handle).await;
}
```

**Step 2: 跑 e2e 确认失败**

Run: `cargo test -p rs-iot --test rtdata_subscribe_e2e`
Expected: FAIL——`snapshot_pushes_all_points_on_connect` 断言失败（流内无 `event: snapshot`，骨架空流）

**Step 3: 实现 producer Phase 1**（完整代码见附录 A §producer；本 Task 落 `read_scope` + `run` 的 Phase 1 段 + `subscribe` 换如下签名）

`subscribe` 替换骨架（producer 挂 `tokio::spawn`）：

```rust
pub async fn subscribe(
    State(st): State<AppState>,
    Query(q): Query<SubQuery>,
) -> impl IntoResponse {
    let dev_filter = parse_dev_ids(q.dev_id.as_deref());
    let tag_filter = q.tag.clone();
    let interval = Duration::from_millis(clamp_interval(q.interval));
    tracing::debug!(
        "[rtdata-subscribe] client connected: dev={:?} tag={:?} interval={:?}",
        dev_filter,
        tag_filter,
        interval
    );
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(8);
    let lux = st.lux.clone();
    tokio::spawn(async move {
        run(&lux, dev_filter, tag_filter, interval, tx).await;
        tracing::debug!("[rtdata-subscribe] client disconnected");
    });
    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}
```

`run` 本 Task 只实现到 Phase 1 结束（Phase 2 的 ticker 循环 Task 5 加；本 Task 循环体空转 `loop { tokio::time::sleep(interval).await; }` 占位，附录 A 为完整形态）：

```rust
/// 读订阅范围内全部设备 → (dev, field→cell) 列表。
///
/// dev_filter=None → 每 tick `KEYS rtdata:*` 重查（新设备自动纳入订阅）；
/// Some → 固定设备列表（设备已删时 HGETALL 返回空 map → 其点位进 removed）。
async fn read_scope(
    lux: &lux::EmbeddedClient,
    dev_filter: &Option<HashSet<String>>,
) -> Vec<(String, serde_json::Map<String, serde_json::Value>)> {
    let devs: Vec<String> = match dev_filter {
        Some(set) => set.iter().cloned().collect(),
        None => match lux.execute_value("KEYS", &["rtdata:*"]).await {
            Ok(lux::EmbeddedValue::Array(keys)) => keys
                .iter()
                .filter_map(|k| match k {
                    lux::EmbeddedValue::Bulk(b) => Some(String::from_utf8_lossy(b).to_string()),
                    lux::EmbeddedValue::Simple(s) => Some(s.clone()),
                    _ => None,
                })
                .map(|k| k.strip_prefix("rtdata:").unwrap_or(&k).to_string())
                .collect(),
            _ => Vec::new(),
        },
    };
    let mut out = Vec::with_capacity(devs.len());
    for dev in devs {
        let obj = hgetall(lux, &format!("rtdata:{dev}")).await;
        if let serde_json::Value::Object(m) = obj {
            out.push((dev, m));
        }
    }
    out
}

/// producer 主体：snapshot（Phase 1）+ 周期 diff（Phase 2，Task 5）。tx 断（客户端
/// 断开）即返回。
async fn run(
    lux: &lux::EmbeddedClient,
    dev_filter: Option<HashSet<String>>,
    tag_filter: Option<String>,
    interval: Duration,
    tx: tokio::sync::mpsc::Sender<Result<Event, Infallible>>,
) {
    let tag = tag_filter.as_deref();
    let mut last_sent: HashMap<String, String> = HashMap::new();

    // ---- Phase 1: snapshot（逐设备一帧，建基线）----
    for (dev, points) in read_scope(lux, &dev_filter).await {
        let filtered = filter_by_tag(points, tag);
        if filtered.is_empty() {
            continue; // tag 过滤后无点（或空 hash）不推空帧
        }
        for (field, cell) in &filtered {
            last_sent.insert(field.clone(), value_str(cell));
        }
        if let Ok(ev) = Event::default()
            .event("snapshot")
            .json_data(json!({ "dev": dev, "points": filtered }))
        {
            if tx.send(Ok(ev)).await.is_err() {
                return;
            }
        }
    }

    // ---- Phase 2: 周期 diff（Task 5 实现；本 Task 占位空转）----
    loop {
        tokio::time::sleep(interval).await;
    }
}
```

（use 区补：`use crate::api::{hgetall, routes_rtdata::tag_of, AppState};`、`use axum::extract::{Query, State};`、`use serde_json::json;`、`use std::collections::HashMap;`、`use std::time::Duration;`。）

**Step 4: 跑 e2e 确认通过**

Run: `cargo test -p rs-iot --test rtdata_subscribe_e2e`
Expected: PASS（4 tests）

**Step 5: Commit**

```bash
git add src/api/routes_rtdata_subscribe.rs tests/rtdata_subscribe_e2e.rs
git commit -m "feat(api): 订阅初次全量 snapshot 推送——逐设备分帧+dev_id/tag 过滤"
```

---

## Task 5: datachange 值变化推送 + removed

producer Phase 2：每 tick 重读订阅范围 → `diff_scope` → 变化/删除才推 `datachange` 帧 + 基线更新。

**Files:**
- Modify: `src/api/routes_rtdata_subscribe.rs`（`run` 的 Phase 2 段替换占位，最终形态见附录 A §producer）
- Modify: `tests/rtdata_subscribe_e2e.rs`（2 新测）

**Step 1: 写失败测试**

```rust
#[tokio::test]
async fn datachange_only_pushes_changed_values() {
    let (app, handle, lux) = test_app("subdiff").await;
    seed(&lux, "devA", &[("t1", "1"), ("t2", "2")], 1_700_000_000).await;
    let tok = jwt(&app).await;
    let mut stream = connect(&app, "?dev_id=devA&interval=100", &tok).await;
    // 窗口1：snapshot 收全。
    let buf1 = collect_window(&mut stream, std::time::Duration::from_millis(600)).await;
    assert!(buf1.contains("event: snapshot") && buf1.contains("devA.t1"), "{buf1}");
    // 变更：t1 新值；t2 同值不同 ts（采集周期写回同值场景）。
    seed(&lux, "devA", &[("t1", "9"), ("t2", "2")], 1_700_000_100).await;
    let buf2 = collect_window(&mut stream, std::time::Duration::from_millis(1500)).await;
    assert!(buf2.contains("event: datachange"), "应含 datachange: {buf2}");
    assert!(buf2.contains("devA.t1") && buf2.contains("9"), "{buf2}");
    assert!(!buf2.contains("devA.t2"), "值未变不应重推: {buf2}");
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn removed_on_device_delete() {
    let (app, handle, lux) = test_app("subdel").await;
    seed(&lux, "devA", &[("t1", "1")], 1).await;
    seed(&lux, "devB", &[("t1", "2")], 1).await;
    let tok = jwt(&app).await;
    let mut stream = connect(&app, "?interval=100", &tok).await;
    let buf1 = collect_window(&mut stream, std::time::Duration::from_millis(600)).await;
    assert!(buf1.contains("devA.t1") && buf1.contains("devB.t1"), "{buf1}");
    rs_iot::db::rtdata::delete_device(&lux, "devA").await.unwrap();
    let buf2 = collect_window(&mut stream, std::time::Duration::from_millis(1500)).await;
    assert!(
        buf2.contains("event: datachange") && buf2.contains("removed"),
        "{buf2}"
    );
    assert!(buf2.contains("devA.t1"), "removed 应列 devA.t1: {buf2}");
    common::stop_lux(handle).await;
}
```

**Step 2: 跑 e2e 确认失败**

Run: `cargo test -p rs-iot --test rtdata_subscribe_e2e`
Expected: FAIL——`datachange_only_pushes_changed_values` 断言失败（无 `event: datachange`，Phase 2 占位空转）

**Step 3: 实现 Phase 2**（`run` 内占位循环替换为附录 A §producer Phase 2 段）

```rust
    // ---- Phase 2: 周期 diff（值变/删除才推）----
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await; // interval 首拍立即完成——快照刚读，消费掉跳过
    loop {
        ticker.tick().await;
        let mut current: HashMap<String, (String, serde_json::Value)> = HashMap::new();
        for (_dev, points) in read_scope(lux, &dev_filter).await {
            for (field, cell) in filter_by_tag(points, tag) {
                let v = value_str(&cell);
                current.insert(field, (v, cell));
            }
        }
        let (changes, removed) = diff_scope(current, &last_sent);
        // 基线更新（变更覆盖 + 删除移除），与推帧解耦——推帧失败即退出无需回滚。
        for (field, cell) in &changes {
            last_sent.insert(field.clone(), value_str(cell));
        }
        for field in &removed {
            last_sent.remove(field);
        }
        if changes.is_empty() && removed.is_empty() {
            continue;
        }
        if let Ok(ev) = Event::default().event("datachange").json_data(json!({
            "changes": changes,
            "removed": removed,
            "ts": crate::event_bus::now_iso8601(),
        })) {
            if tx.send(Ok(ev)).await.is_err() {
                return;
            }
        }
    }
```

（背压语义：mpsc 容量 8，慢客户端 send 阻塞 → tick Skip 不追赶；恢复后下一 tick 从全量状态重算——只延迟不丢失。）

**Step 4: 跑 e2e 确认通过**

Run: `cargo test -p rs-iot --test rtdata_subscribe_e2e`
Expected: PASS（6 tests）

**Step 5: Commit**

```bash
git add src/api/routes_rtdata_subscribe.rs tests/rtdata_subscribe_e2e.rs
git commit -m "feat(api): 订阅周期 datachange 推送——读侧 poll-diff 值变才推+removed（OPC UA 语义）"
```

---

## Task 6: 文档收口 + 全量验证

**Files:**
- Modify: `docs/reference/rest-api.md`（路由表行 + 新小节）
- Modify: `docs/README.md`（designs 表登记行；实施后状态标记同步）
- Modify: `docs/designs/2026-08-23-rtdata-subscribe-plan.md`（头部回写 implemented）

**Step 1: rest-api.md 补接口**

- 路由表（`GET /api/v1/rtdatas` 行附近）加一行：

```markdown
| GET | `/api/v1/rtdatas/subscribe` | `?dev_id=`（逗号分隔，可选）`&tag=`（可选）`&interval=`（ms，可选，默认 1000，clamp [100,60000]） | SSE：`snapshot`（连接即推，逐设备全量）+ `datachange`（周期，仅值变化/删除）；cell 与 `/rtdatas` 同构 |
```

- `/api/v1/events` SSE 小节后加新小节（示例 curl 用 `-N` 关缓冲）：

```markdown
### 4.N 实时数据订阅（OPC UA 式）—— `GET /api/v1/rtdatas/subscribe`

外部客户端订阅实时数据（对标 OPC UA CreateSubscription 语义：初次全量 + 周期只推变化）。

```bash
curl -N -H "Authorization: Bearer $JWT" \
  "http://localhost:8790/api/v1/rtdatas/subscribe?dev_id=devA,devB&tag=temp&interval=1000"
```

帧（`text/event-stream`）：
- `event: snapshot`（连接建立即推，一设备一帧）：`{"dev":"devA","points":{"devA.temp":[ts,"localTime","value"],...}}`
- `event: datachange`（interval 周期，仅当有变化/删除）：`{"changes":{...同构 cell...},"removed":["devB.old1",...],"ts":"RFC3339"}`
- 注释帧（`: `，15s）——代理防超时

语义保证：值变化只比 cell[2] 值串（周期写回同值不产生通知）；断线重连即重新 snapshot
全量重同步（EventSource 自动重连即可恢复）；设备/点位删除 → `removed` 列出；鉴权同
其余 `/api/v1`。设计依据：[2026-08-23-rtdata-subscribe-plan.md](../designs/2026-08-23-rtdata-subscribe-plan.md)。
```

**Step 2: README 索引登记**（designs 表尾加行；本 Task 实施完成后状态直接写"已实施"）

```markdown
| `2026-08-23-rtdata-subscribe-plan.md`（README 行实际落地为 [链接](2026-08-23-rtdata-subscribe-plan.md)，相对路径 `designs/` 前缀以 README 位置为准） | 实时数据订阅接口实施计划（OPC UA 式 snapshot/datachange，SSE） | 已实施 |
```

**Step 3: 计划头部回写 implemented**（`> **状态**` 起的状态块整体替换：实施=各 Task commit 列表、代码/测试字段复核、遗留=可选增强清单或"无"）

**Step 4: 全量验证**

```bash
cargo test -p rs-iot                                   # 全量回归（含既有 e2e）
cargo clippy -p rs-iot --all-targets -- -D warnings    # 静态检查
python scripts/check_docs.py                           # 文档校验（CI docs job 同款）
```

Expected: 全绿；`check_docs.py` 无输出退出 0。

**Step 5: Commit + 合回 master**

```bash
git add docs/reference/rest-api.md docs/README.md docs/designs/2026-08-23-rtdata-subscribe-plan.md
git commit -m "docs(api): rtdata 订阅接口参考+索引登记+计划回写 implemented"
# master 合入（主仓执行）：
git checkout master && git merge feat/rtdata-subscribe
```

---

## 风险与边界

- **tick 全量读成本**：万点 × 1Hz ≈ ms 级 HGETALL + diff/客户端；clamp 100ms 下限防滥用。订阅者数量大（>10）时的共享 diff 服务列为遗留。
- **单设备超大点位**：snapshot/datachange 单帧随设备点位数线性（10k 点单设备一帧 1~2MB），v1 接受；分帧列为遗留。
- **lux `KEYS` 每 tick**（无 dev 过滤时）：rtdata key 数 = 设备数（数百量级），成本可忽略。
- **gateway / mcp**：rsiot-gateway 纯反代 `/api/v1/*` 自动透传零改动；rsiot-mcp 工具暴露不在本计划。

---

## 附录 A：最终模块代码（`src/api/routes_rtdata_subscribe.rs`）

Tasks 2-5 增量拼装的最终形态（执行时对照复核，避免增量拼接漂移）：

```rust
//! 实时数据订阅端点（OPC UA 式订阅语义；设计见
//! docs/designs/2026-08-23-rtdata-subscribe-plan.md）。
//!
//! `GET /api/v1/rtdatas/subscribe?dev_id=a,b&tag=x&interval=1000` → SSE：
//! - **snapshot**：连接建立即逐设备推全量快照（HGETALL `rtdata:<dev>`，一设备一帧）；
//! - **datachange**：之后每 `interval`（默认 1s，clamp [100ms,60s]）重读订阅范围，
//!   与上次已推值比对（仅比 cell[2] 值串——ts 变化值不变不算），有变化/删除才推一帧。
//!
//! 机制取舍：**每连接读侧 poll-diff，不走 EventBus**——绕过 pipeline 聚合的
//! `RTDATA_NOTIFY_MAX_DEVS=50` 截断与 broadcast 容量 256 的 Lagged 丢帧（订阅定位
//! 外部客户端可靠数据面）；interval 天然 per-connection（对标 OPC UA publishing
//! interval）；断线重连即重新 snapshot 全量重同步。pipeline / event_bus 零改动
//! ——现有 `/api/v1/events` 与 lua_transform 订阅面不受影响。

use crate::api::{hgetall, routes_rtdata::tag_of, AppState};
use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::time::Duration;
use tokio_stream::wrappers::ReceiverStream;

/// 订阅周期下限 ms（防高频滥用——每连接每 tick 全量 HGETALL 的成本护栏）。
const MIN_INTERVAL_MS: u64 = 100;
/// 订阅周期上限 ms。
const MAX_INTERVAL_MS: u64 = 60_000;
/// 默认订阅周期 ms。
const DEFAULT_INTERVAL_MS: u64 = 1_000;

/// query 参数（均可选）。
#[derive(Deserialize)]
pub struct SubQuery {
    /// 设备 ID 逗号列表（精确匹配；缺省 = 全部设备，每 tick 重查 KEYS，新设备自动纳入）。
    pub dev_id: Option<String>,
    /// tag 模糊过滤（field 末段 contains；语义同 `GET /api/v1/rtdatas?tag=`）。
    pub tag: Option<String>,
    /// 订阅周期 ms（clamp [100, 60000]，缺省 1000）。
    pub interval: Option<u64>,
}

// ---------- 纯函数（Task 2，单测覆盖） ----------

/// interval clamp：缺省 DEFAULT，夹到 [MIN, MAX]。纯函数便于单测。
pub fn clamp_interval(raw: Option<u64>) -> u64 {
    raw.unwrap_or(DEFAULT_INTERVAL_MS)
        .clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS)
}

/// dev_id 逗号列表 → 去重集合。None/空白/全空段 → None（= 全部设备）。
pub fn parse_dev_ids(raw: Option<&str>) -> Option<HashSet<String>> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    let set: HashSet<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    if set.is_empty() { None } else { Some(set) }
}

/// cell（`[ts, localTime, value]`）→ 值串（diff 比较键）。非数组/越界/非串 → 整体
/// JSON 串兜底（防畸形 cell 误判恒等）。
pub fn value_str(cell: &serde_json::Value) -> String {
    cell.get(2)
        .and_then(serde_json::Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| cell.to_string())
}

/// 全订阅范围 diff：`current` = 本 tick 过滤后的 field → (值串, cell)；
/// `last_sent` = 已推基线 field → 值串。返回 (变更 field → cell, 消失 field 列表)。
///
/// **值串比较**——ts/localTime 变化值不变不算变更（对标 OPC UA DataChangeFilter
/// 的值触发语义；采集插件周期写回同值不产生通知）。
pub fn diff_scope(
    current: HashMap<String, (String, serde_json::Value)>,
    last_sent: &HashMap<String, String>,
) -> (serde_json::Map<String, serde_json::Value>, Vec<String>) {
    let mut now: HashSet<&str> = HashSet::with_capacity(current.len());
    let mut changes = serde_json::Map::new();
    for (field, (val, cell)) in &current {
        now.insert(field.as_str());
        if last_sent.get(field) != Some(val) {
            changes.insert(field.clone(), cell.clone());
        }
    }
    let removed: Vec<String> = last_sent
        .keys()
        .filter(|f| !now.contains(f.as_str()))
        .cloned()
        .collect();
    (changes, removed)
}

/// tag contains 过滤（field 末段匹配，复用 `GET /rtdatas` 的 tag_of 语义）。None 全保留。
pub fn filter_by_tag(
    m: serde_json::Map<String, serde_json::Value>,
    tag: Option<&str>,
) -> serde_json::Map<String, serde_json::Value> {
    match tag {
        None => m,
        Some(t) => m
            .into_iter()
            .filter(|(k, _)| tag_of(k).contains(t))
            .collect(),
    }
}

// ---------- producer（Task 4/5 组装纯函数） ----------

/// 读订阅范围内全部设备 → (dev, field→cell) 列表。
///
/// dev_filter=None → 每 tick `KEYS rtdata:*` 重查（新设备自动纳入订阅）；
/// Some → 固定设备列表（设备已删时 HGETALL 返回空 map → 其点位进 removed）。
async fn read_scope(
    lux: &lux::EmbeddedClient,
    dev_filter: &Option<HashSet<String>>,
) -> Vec<(String, serde_json::Map<String, serde_json::Value>)> {
    let devs: Vec<String> = match dev_filter {
        Some(set) => set.iter().cloned().collect(),
        None => match lux.execute_value("KEYS", &["rtdata:*"]).await {
            Ok(lux::EmbeddedValue::Array(keys)) => keys
                .iter()
                .filter_map(|k| match k {
                    lux::EmbeddedValue::Bulk(b) => Some(String::from_utf8_lossy(b).to_string()),
                    lux::EmbeddedValue::Simple(s) => Some(s.clone()),
                    _ => None,
                })
                .map(|k| k.strip_prefix("rtdata:").unwrap_or(&k).to_string())
                .collect(),
            _ => Vec::new(),
        },
    };
    let mut out = Vec::with_capacity(devs.len());
    for dev in devs {
        let obj = hgetall(lux, &format!("rtdata:{dev}")).await;
        if let serde_json::Value::Object(m) = obj {
            out.push((dev, m));
        }
    }
    out
}

/// `GET /api/v1/rtdatas/subscribe`：OPC UA 式实时数据订阅（SSE）。
///
/// 连接建立：snapshot 逐设备全量推送 + 建基线；之后每 interval 读侧 diff，
/// 变化才推 datachange。客户端断开 → send 失败 → producer task 退出（资源回收）。
pub async fn subscribe(
    State(st): State<AppState>,
    Query(q): Query<SubQuery>,
) -> impl IntoResponse {
    let dev_filter = parse_dev_ids(q.dev_id.as_deref());
    let tag_filter = q.tag.clone();
    let interval = Duration::from_millis(clamp_interval(q.interval));
    tracing::debug!(
        "[rtdata-subscribe] client connected: dev={:?} tag={:?} interval={:?}",
        dev_filter,
        tag_filter,
        interval
    );
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(8);
    let lux = st.lux.clone();
    tokio::spawn(async move {
        run(&lux, dev_filter, tag_filter, interval, tx).await;
        tracing::debug!("[rtdata-subscribe] client disconnected");
    });
    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}

/// producer 主体：snapshot（Phase 1）+ 周期 diff（Phase 2）。tx 断（客户端断开）即返回。
async fn run(
    lux: &lux::EmbeddedClient,
    dev_filter: Option<HashSet<String>>,
    tag_filter: Option<String>,
    interval: Duration,
    tx: tokio::sync::mpsc::Sender<Result<Event, Infallible>>,
) {
    let tag = tag_filter.as_deref();
    let mut last_sent: HashMap<String, String> = HashMap::new();

    // ---- Phase 1: snapshot（逐设备一帧，建基线）----
    for (dev, points) in read_scope(lux, &dev_filter).await {
        let filtered = filter_by_tag(points, tag);
        if filtered.is_empty() {
            continue; // tag 过滤后无点（或空 hash）不推空帧
        }
        for (field, cell) in &filtered {
            last_sent.insert(field.clone(), value_str(cell));
        }
        if let Ok(ev) = Event::default()
            .event("snapshot")
            .json_data(json!({ "dev": dev, "points": filtered }))
        {
            if tx.send(Ok(ev)).await.is_err() {
                return;
            }
        }
    }

    // ---- Phase 2: 周期 diff（值变/删除才推）----
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await; // interval 首拍立即完成——快照刚读，消费掉跳过
    loop {
        ticker.tick().await;
        let mut current: HashMap<String, (String, serde_json::Value)> = HashMap::new();
        for (_dev, points) in read_scope(lux, &dev_filter).await {
            for (field, cell) in filter_by_tag(points, tag) {
                let v = value_str(&cell);
                current.insert(field, (v, cell));
            }
        }
        let (changes, removed) = diff_scope(current, &last_sent);
        // 基线更新（变更覆盖 + 删除移除），与推帧解耦——推帧失败即退出无需回滚。
        for (field, cell) in &changes {
            last_sent.insert(field.clone(), value_str(cell));
        }
        for field in &removed {
            last_sent.remove(field);
        }
        if changes.is_empty() && removed.is_empty() {
            continue;
        }
        if let Ok(ev) = Event::default().event("datachange").json_data(json!({
            "changes": changes,
            "removed": removed,
            "ts": crate::event_bus::now_iso8601(),
        })) {
            if tx.send(Ok(ev)).await.is_err() {
                return;
            }
        }
    }
}

// ---------- 单测（Task 2 落地，此处省略——见 Task 2 Step 2 完整清单） ----------
```

## 附录 B：最终 e2e（`tests/rtdata_subscribe_e2e.rs`）

Tasks 3-5 增量拼装的最终形态：测试基建（`test_app`/`jwt`/`seed`/`connect`/`collect_window`）+ 6 测（`subscribe_without_jwt_401` / `subscribe_with_jwt_200_event_stream` / `snapshot_pushes_all_points_on_connect` / `snapshot_respects_dev_and_tag_filter` / `datachange_only_pushes_changed_values` / `removed_on_device_delete`）——各 Task Step 1 已给全量代码，执行时按 Task 顺序累加即为本附录内容，不再重复。
