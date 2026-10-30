//! 画面/主题版本管理 e2e（P0 第 1 轮，design §6 十一组）。
//!
//! 复用 `tests/events_e2e.rs` 的 test_app 范式：独立 lux data_dir + 手工组装
//! AppState + build_router + tower oneshot。版本层契约（键族 `sv:` / 判重 /
//! if_version 乐观锁 / rollback / TTL / 紧缩 / 级联删 / SSE screen 事件）的
//! 权威源 = `docs/designs/2026-10-30-screen-versioning-design.md`。
//!
//! 覆盖口径说明（第 11 组）：embedded lux 无错误注入面，「捕获成功、INCR 失败」
//! 「INCR 成功、head 失败」两个中间态不可在线构造——e2e 级验「lux 停机 →
//! 多命令序列统一 500（不吞错报 ok）」；捕获-写 head 的顺序不变量由实现注释
//! 与第 2/4 组的行为断言（快照先于 head 生效可观察）共同覆盖。

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rs_iot::api::{build_router, AppState};
use rs_iot::config::{AppConfig, MetaConfig};
use rs_iot::event_bus::EventBus;
use rs_iot::host::{CtlRegistry, HostContext};
use rs_iot::log_ringbuffer::LogRingBuffer;
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::types::Hwid;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// 建测试 app（默认 [meta] 配置：TTL 30 天 / cap 50——不触发 TTL 与紧缩）。
async fn test_app() -> (axum::Router, lux::ServerHandle) {
    test_app_meta(MetaConfig::default()).await
}

/// 建测试 app，[meta] 可覆盖（第 5/6 组需要短 TTL / 小 cap）。
async fn test_app_meta(meta: MetaConfig) -> (axum::Router, lux::ServerHandle) {
    let data_dir = common::test_data_dir("metaver");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    let metrics = Arc::new(rs_iot::process_metrics::ProcessMetrics::new());
    let event_bus = EventBus::new(256);
    let trace_registry = rs_iot::io_trace::TraceRegistry::with_sender(event_bus.clone());
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown: CancellationToken::new(),
        ctl_registry: CtlRegistry::new(metrics.clone()),
        bridge_registry: rs_iot::host::BridgeRegistry::new(),
        metrics: metrics.clone(),
        trace_registry: trace_registry.clone(),
        event_tx: event_bus.clone(),
        history: None,
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
        event_bus: event_bus.clone(),
        config: Arc::new(AppConfig {
            meta,
            ..Default::default()
        }),
        history_stats: None,
        history_engine_state: Arc::from("disabled"),
        history_reader: None,
        history_telemetry: None,
        rtdata_watch: Arc::new(rs_iot::api::rtdata_watch::RtdataWatchService::new()),
    };
    (build_router(state), handle)
}

/// 登录拿 JWT（admin/admin，author 断言依赖此 sub）。
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
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    v["token"].as_str().unwrap().to_string()
}

/// 构造合法 doc body（n 进内容，保证逐次不同触发建版）。
fn doc(n: u32) -> Vec<u8> {
    format!(r#"{{"name":"v{n}","widgets":[{n}]}}"#).into_bytes()
}

/// PUT screen（可选 if_version query），返回响应。
async fn put(
    app: &axum::Router,
    tok: &str,
    id: &str,
    body: &[u8],
    if_version: Option<u64>,
) -> axum::response::Response {
    let uri = match if_version {
        Some(v) => format!("/api/v1/screens/{id}?if_version={v}"),
        None => format!("/api/v1/screens/{id}"),
    };
    app.clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(uri)
                .header(header::AUTHORIZATION, tok)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_vec()))
                .unwrap(),
        )
        .await
        .unwrap()
}

/// 带认证 POST JSON（rollback 等），返回响应。
async fn post_json(
    app: &axum::Router,
    tok: &str,
    path: &str,
    body: &str,
) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header(header::AUTHORIZATION, tok)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

/// 带认证 DELETE（无 body）。
async fn delete(app: &axum::Router, tok: &str, path: &str) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(path)
                .header(header::AUTHORIZATION, tok)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

/// 带认证 GET，返回 (status, json)。
async fn get(app: &axum::Router, tok: &str, path: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header(header::AUTHORIZATION, tok)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

/// versions 列表的 seq 序列（降序断言用）。
fn seqs(v: &serde_json::Value) -> Vec<u64> {
    v["versions"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x["seq"].as_u64())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

// ---------- 第 1 组：判重 ----------

/// 同内容二次 PUT → version 不变、无新版本条目（改一个像素保存十次只一条版本；
/// e2e 播种幂等同款收益——fixtures 全量 upsert 逐字节相同不产生版本垃圾）。
#[tokio::test]
async fn dedup_same_content_put_keeps_version() {
    let (app, handle) = test_app().await;
    let tok = jwt(&app).await;

    put(&app, &tok, "scr", &doc(1), None).await; // 创建：version 1，无历史
    let r2 = body_json(put(&app, &tok, "scr", &doc(2), None).await).await; // 变更：捕获 v1，version 2
    assert_eq!(r2["version"], 2, "内容变更应建版: {r2}");
    // 判重：同内容再来一次
    let r3 = body_json(put(&app, &tok, "scr", &doc(2), None).await).await;
    assert_eq!(r3["status"], "ok", "判重命中也应 ok: {r3}");
    assert_eq!(r3["version"], 2, "判重命中 version 不变: {r3}");

    let (_, versions) = get(&app, &tok, "/api/v1/screens/scr/versions").await;
    assert_eq!(
        seqs(&versions),
        vec![1],
        "判重不得产生新版本条目: {versions}"
    );
    common::stop_lux(handle).await;
}

// ---------- 第 2 组：捕获 ----------

/// 内容变更 PUT → 旧内容可从 versions/{seq} 取回，author = JWT sub。
#[tokio::test]
async fn capture_old_doc_retrievable_with_author() {
    let (app, handle) = test_app().await;
    let tok = jwt(&app).await;

    put(&app, &tok, "scr", &doc(1), None).await;
    put(&app, &tok, "scr", &doc(2), None).await;

    let (st, v1) = get(&app, &tok, "/api/v1/screens/scr/versions/1").await;
    assert_eq!(st, StatusCode::OK, "{v1}");
    assert_eq!(v1["doc"]["name"], "v1", "取回的应是旧内容: {v1}");
    assert_eq!(v1["author"], "admin", "author 应取 JWT sub: {v1}");
    assert_eq!(v1["seq"], 1);

    // head 是新内容，GET 本体带 version
    let (_, head) = get(&app, &tok, "/api/v1/screens/scr").await;
    assert_eq!(head["doc"]["name"], "v2", "head 应为新内容: {head}");
    assert_eq!(head["version"], 2, "GET 响应应带 version: {head}");
    common::stop_lux(handle).await;
}

// ---------- 第 3 组：乐观锁 ----------

/// 遗留画面（P0 上线前已存在、无 sv: 键）首次内容变更：旧内容捕获为**版本 0**
/// （设计 §3.1 迁移期关键路径——线上 11 个长期资产全是这种形态，复审 L13 补测）。
#[tokio::test]
async fn legacy_screen_first_change_captures_version_zero() {
    let (app, handle) = test_app().await;
    let tok = jwt(&app).await;

    // 模拟 P0 前既有画面：只有 head hash，无 seq/meta/正文键
    handle
        .client()
        .execute(
            "HSET",
            &["screen:legacy", "doc", r#"{"name":"pre-p0","widgets":[]}"#],
        )
        .await
        .expect("seed legacy head");

    // GET：遗留画面 version=0
    let (_, head) = get(&app, &tok, "/api/v1/screens/legacy").await;
    assert_eq!(head["version"], 0, "无 seq 键应视为 0: {head}");

    // if_version=0 是合法断言（迁移期）；内容变更 → 捕获版本 0 + INCR 到 1
    let r = body_json(put(&app, &tok, "legacy", &doc(2), Some(0)).await).await;
    assert_eq!(r["version"], 1, "遗留画面首次改动应 0→1: {r}");

    let (_, versions) = get(&app, &tok, "/api/v1/screens/legacy/versions").await;
    assert_eq!(seqs(&versions), vec![0], "旧内容应捕获为版本 0: {versions}");
    let (st, v0) = get(&app, &tok, "/api/v1/screens/legacy/versions/0").await;
    assert_eq!(st, StatusCode::OK, "{v0}");
    assert_eq!(v0["doc"]["name"], "pre-p0", "版本 0 = 机制上线前原貌: {v0}");
    assert_eq!(v0["author"], "admin");
    common::stop_lux(handle).await;
}

/// 过期 if_version → 409 + current_version 且不留痕；缺省 = 无条件写（向后兼容）。
#[tokio::test]
async fn optimistic_lock_409_and_default_unconditional() {
    let (app, handle) = test_app().await;
    let tok = jwt(&app).await;

    put(&app, &tok, "scr", &doc(1), None).await; // v1
    put(&app, &tok, "scr", &doc(2), None).await; // v2

    // 拿着过期版本号（1）写 → 409 + current_version=2
    let resp = put(&app, &tok, "scr", &doc(3), Some(1)).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let e = body_json(resp).await;
    assert_eq!(e["error"], "version conflict", "{e}");
    assert_eq!(e["current_version"], 2, "{e}");

    // 失败不留痕：head 未变、无新版本
    let (_, head) = get(&app, &tok, "/api/v1/screens/scr").await;
    assert_eq!(head["doc"]["name"], "v2", "409 不得写 head: {head}");
    let (_, versions) = get(&app, &tok, "/api/v1/screens/scr/versions").await;
    assert_eq!(seqs(&versions), vec![1], "409 不得建版本: {versions}");

    // 匹配的 if_version（2）→ 正常写入
    let r = body_json(put(&app, &tok, "scr", &doc(3), Some(2)).await).await;
    assert_eq!(r["version"], 3, "{r}");

    // 缺省 if_version = 无条件写（既有编辑器/e2e 播种零改动路径）
    let r = body_json(put(&app, &tok, "scr", &doc(4), None).await).await;
    assert_eq!(r["version"], 4, "{r}");

    // if_version=0 是合法断言（"从未被本机制改过"）——首建即用 0 抢占
    let r = body_json(put(&app, &tok, "scr2", &doc(1), Some(0)).await).await;
    assert_eq!(r["version"], 1, "seq 缺失视为 0，if_version=0 应通过: {r}");
    common::stop_lux(handle).await;
}

/// if_version 校验先于判重（复审 L4）：携带过期版本号、但提交内容与旧 head
/// 逐字节相同的写，也必须 409——判重不得成为绕过乐观锁的旁路。
#[tokio::test]
async fn stale_if_version_conflicts_even_when_content_identical() {
    let (app, handle) = test_app().await;
    let tok = jwt(&app).await;

    put(&app, &tok, "scr", &doc(1), None).await; // v1
    put(&app, &tok, "scr", &doc(2), None).await; // v2

    // 内容与当前 head 相同（doc(2)），但版本号停在 1：必须 409 而不是判重 ok
    let resp = put(&app, &tok, "scr", &doc(2), Some(1)).await;
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "过期 if_version 不得被判重旁路"
    );
    let e = body_json(resp).await;
    assert_eq!(e["current_version"], 2, "{e}");

    // 失败不留痕：无新版本
    let (_, versions) = get(&app, &tok, "/api/v1/screens/scr/versions").await;
    assert_eq!(seqs(&versions), vec![1], "{versions}");
    common::stop_lux(handle).await;
}

// ---------- 第 4 组：回滚 ----------

/// rollback 后 head == 目标版内容，且回滚前的 head 已存为新版本（回滚可再回滚）。
#[tokio::test]
async fn rollback_restores_target_and_snapshots_head() {
    let (app, handle) = test_app().await;
    let tok = jwt(&app).await;

    for n in 1..=3 {
        put(&app, &tok, "scr", &doc(n), None).await;
    }
    // versions: [1(v1), 2(v2)]，head=v3/seq3

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/screens/scr/rollback")
                .header(header::AUTHORIZATION, &tok)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"to_version":1}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let r = body_json(resp).await;
    assert_eq!(r["version"], 4, "回滚产生新版本号: {r}");

    // head 内容 == 目标版
    let (_, head) = get(&app, &tok, "/api/v1/screens/scr").await;
    assert_eq!(head["doc"]["name"], "v1", "回滚后 head 应为目标版: {head}");
    assert_eq!(head["version"], 4);

    // 回滚前的 head（v3）已被快照为版本 3——可再回滚回去
    let (_, v3) = get(&app, &tok, "/api/v1/screens/scr/versions/3").await;
    assert_eq!(v3["doc"]["name"], "v3", "回滚前 head 应已存为新版本: {v3}");

    // 再回滚到 3：head 恢复 v3
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/screens/scr/rollback")
                .header(header::AUTHORIZATION, &tok)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"to_version":3}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let (_, head2) = get(&app, &tok, "/api/v1/screens/scr").await;
    assert_eq!(head2["doc"]["name"], "v3", "回滚的回滚: {head2}");

    // rollback 目标版不存在 → 404；正文过期语义见第 5 组（410）
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/screens/scr/rollback")
                .header(header::AUTHORIZATION, &tok)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"to_version":99}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    common::stop_lux(handle).await;
}

/// 回滚到"内容与当前 head 相同"的版本 = no-op（复审 L6）：不建版、不 INCR、
/// 不发帧——重复点同一版回滚不再制造重复历史条目。
#[tokio::test]
async fn rollback_to_same_content_is_noop() {
    let (app, handle) = test_app().await;
    let tok = jwt(&app).await;
    for n in 1..=3 {
        put(&app, &tok, "scr", &doc(n), None).await;
    }
    // rollback 到 1 → head=v1（seq4，v3 已快照）
    assert_eq!(
        post_json(
            &app,
            &tok,
            "/api/v1/screens/scr/rollback",
            r#"{"to_version":1}"#
        )
        .await
        .status(),
        StatusCode::OK
    );
    // rollback 到 3 → head=v3（seq5，v1 已快照）
    assert_eq!(
        post_json(
            &app,
            &tok,
            "/api/v1/screens/scr/rollback",
            r#"{"to_version":3}"#
        )
        .await
        .status(),
        StatusCode::OK
    );
    let (_, before) = get(&app, &tok, "/api/v1/screens/scr/versions").await;
    let count_before = seqs(&before).len();
    assert_eq!(count_before, 4, "{before}");

    // 再回滚到 3：head 内容 == 版本 3 → no-op（version 不变、无新版本）
    let resp = post_json(
        &app,
        &tok,
        "/api/v1/screens/scr/rollback",
        r#"{"to_version":3}"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let r = body_json(resp).await;
    assert_eq!(r["version"], 5, "no-op 不消耗新版本号: {r}");

    let (_, head) = get(&app, &tok, "/api/v1/screens/scr").await;
    assert_eq!(head["version"], 5, "{head}");
    let (_, after) = get(&app, &tok, "/api/v1/screens/scr/versions").await;
    assert_eq!(
        seqs(&after).len(),
        count_before,
        "no-op 不得新增版本: {after}"
    );
    common::stop_lux(handle).await;
}

// ---------- 第 5 组：TTL ----------

/// 正文明设短 TTL 过期后列表标 expired、取回 410（元数据小，多活无害）。
#[tokio::test]
async fn ttl_expired_marks_and_410() {
    let (app, handle) = test_app_meta(MetaConfig {
        versions_ttl_secs: 1,
        ..Default::default()
    })
    .await;
    let tok = jwt(&app).await;

    put(&app, &tok, "scr", &doc(1), None).await;
    put(&app, &tok, "scr", &doc(2), None).await; // 捕获 v1，TTL 1s

    // 未过期：expired=false、可取回
    let (_, versions) = get(&app, &tok, "/api/v1/screens/scr/versions").await;
    assert_eq!(versions["versions"][0]["expired"], false, "{versions}");

    tokio::time::sleep(std::time::Duration::from_millis(1300)).await;

    // 过期：列表标 expired、单版取回 410、元数据行仍在
    let (_, versions) = get(&app, &tok, "/api/v1/screens/scr/versions").await;
    assert_eq!(versions["versions"][0]["expired"], true, "{versions}");
    assert_eq!(
        versions["versions"][0]["seq"], 1,
        "元数据行保留: {versions}"
    );
    let (st, _) = get(&app, &tok, "/api/v1/screens/scr/versions/1").await;
    assert_eq!(st, StatusCode::GONE, "过期正文取回应 410");
    common::stop_lux(handle).await;
}

// ---------- 第 6 组：紧缩 ----------

/// 灌 2×cap+ 条元数据触发紧缩 → 保留最新 cap 条（按 seq 降序且正文仍在）。
#[tokio::test]
async fn compaction_keeps_latest_cap() {
    let cap: usize = 3;
    let (app, handle) = test_app_meta(MetaConfig {
        versions_cap: cap,
        ..Default::default()
    })
    .await;
    let tok = jwt(&app).await;

    // 首建 v1 + 2×cap=6 次变更 → 6 条版本（seq 1..=6），HLEN=6 > 2×cap? 不：
    // 6 == 2×cap 未超；再 +1 次变更触发（HLEN 7 > 6）→ 紧缩回 cap=3
    for n in 1..=(2 * cap + 2) as u32 {
        put(&app, &tok, "scr", &doc(n), None).await;
    }

    let (_, versions) = get(&app, &tok, "/api/v1/screens/scr/versions").await;
    let got = seqs(&versions);
    assert_eq!(
        got,
        vec![(2 * cap + 1) as u64, (2 * cap) as u64, (2 * cap - 1) as u64],
        "紧缩后应只保留最新 cap 条: {versions}"
    );
    // 全部正文仍在
    for s in &got {
        let (st, _) = get(&app, &tok, &format!("/api/v1/screens/scr/versions/{s}")).await;
        assert_eq!(st, StatusCode::OK, "保留版正文应在: seq={s}");
    }
    common::stop_lux(handle).await;
}

// ---------- 第 7 组：删除级联 ----------

/// DELETE 后 seq/meta/正文全清——同 slug 重建读不到上一世代孤儿版本。
#[tokio::test]
async fn delete_cascades_version_keys() {
    let (app, handle) = test_app().await;
    let tok = jwt(&app).await;

    put(&app, &tok, "scr", &doc(1), None).await;
    put(&app, &tok, "scr", &doc(2), None).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/screens/scr")
                .header(header::AUTHORIZATION, &tok)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 同 slug 重建：新首建无历史——旧世代版本不可见（否则 seq 撞车+幽灵历史）
    put(&app, &tok, "scr", &doc(9), None).await;
    let (_, versions) = get(&app, &tok, "/api/v1/screens/scr/versions").await;
    assert_eq!(
        seqs(&versions),
        Vec::<u64>::new(),
        "重建后不得见旧世代版本: {versions}"
    );

    // GET 旧 seq → 404（元数据已清，非 410）
    let (st, _) = get(&app, &tok, "/api/v1/screens/scr/versions/1").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    common::stop_lux(handle).await;
}

// ---------- 第 8 组：SSE screen 事件 ----------

/// put（内容变化）/rollback/delete 各发一帧 `event: screen`，payload 字段齐；
/// 判重命中不发帧（内容未变不该惊动运行页）；回滚 no-op 同样不发帧。
#[tokio::test]
async fn sse_screen_events_frames_and_dedup_silent() {
    use futures_util::StreamExt;
    let (app, handle) = test_app().await;
    let tok = jwt(&app).await;

    // 先建 SSE 连接（订阅在 handler 内完成），再触发写
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/events?types=screen")
                .header(header::AUTHORIZATION, &tok)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    put(&app, &tok, "scr", &doc(1), None).await; // 首建：put 帧
    put(&app, &tok, "scr", &doc(2), None).await; // 变更：put 帧（捕获 v1）
    put(&app, &tok, "scr", &doc(2), None).await; // 判重：无帧
                                                 // 回滚 → rollback 帧（seq3；回滚前 head v2 已快照为 v2）
    assert_eq!(
        post_json(
            &app,
            &tok,
            "/api/v1/screens/scr/rollback",
            r#"{"to_version":1}"#
        )
        .await
        .status(),
        StatusCode::OK
    );
    // 删除 → delete 帧（seq_before=3）
    assert_eq!(
        delete(&app, &tok, "/api/v1/screens/scr").await.status(),
        StatusCode::OK
    );

    // 逐 chunk 读：期望 4 帧（put×2 + rollback + delete），判重不发
    let mut stream = resp.into_body().into_data_stream();
    let mut buf = String::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_millis(500), stream.next()).await {
            Ok(Some(chunk)) => {
                buf.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
            }
            Ok(None) => break,
            Err(_) => {
                // 500ms 无新 chunk：收满预期帧才提前结束（少帧时继续等到 deadline 再断言失败）
                if buf.matches("event: screen").count() >= 4 {
                    break;
                }
            }
        }
    }
    let frames = buf.matches("event: screen").count();
    assert_eq!(
        frames, 4,
        "put×2 + rollback + delete 恰四帧（判重不发）: {buf}"
    );
    assert_eq!(
        buf.matches(r#""action":"put""#).count(),
        2,
        "两次有效 PUT 各一帧: {buf}"
    );
    assert!(
        buf.contains(r#""action":"rollback""#),
        "rollback 帧缺失: {buf}"
    );
    assert!(buf.contains(r#""action":"delete""#), "delete 帧缺失: {buf}");
    assert!(buf.contains(r#""id":"scr""#), "payload 带 id: {buf}");
    assert!(
        buf.contains(r#""actor":"admin""#),
        "payload 带 actor: {buf}"
    );
    assert!(buf.contains(r#""version":3"#), "payload 带 version: {buf}");
    assert!(buf.contains(r#""ts":""#), "payload 带 ts: {buf}");
    common::stop_lux(handle).await;
}

// ---------- 第 9 组：themes 同构 ----------

/// 同一套断言跑 theme 前缀（MetaKind 泛化回归：机制免费覆盖 themes）。
#[tokio::test]
async fn themes_same_versioning_semantics() {
    let (app, handle) = test_app().await;
    let tok = jwt(&app).await;

    async fn put_theme(
        app: &axum::Router,
        tok: &str,
        id: &str,
        n: u32,
    ) -> axum::response::Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/api/v1/themes/{id}"))
                    .header(header::AUTHORIZATION, tok)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(doc(n)))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    let r1 = body_json(put_theme(&app, &tok, "thm", 1).await).await;
    assert_eq!(r1["version"], 1, "themes 首建同款 version: {r1}");
    let r2 = body_json(put_theme(&app, &tok, "thm", 2).await).await;
    assert_eq!(r2["version"], 2, "{r2}");
    // 判重
    let r3 = body_json(put_theme(&app, &tok, "thm", 2).await).await;
    assert_eq!(r3["version"], 2, "themes 判重同款: {r3}");

    // 版本列表 + 取回 + 回滚
    let (_, versions) = get(&app, &tok, "/api/v1/themes/thm/versions").await;
    assert_eq!(seqs(&versions), vec![1], "{versions}");
    let (_, v1) = get(&app, &tok, "/api/v1/themes/thm/versions/1").await;
    assert_eq!(v1["doc"]["name"], "v1", "{v1}");

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/themes/thm/rollback")
                .header(header::AUTHORIZATION, &tok)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"to_version":1}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let (_, head) = get(&app, &tok, "/api/v1/themes/thm").await;
    assert_eq!(head["doc"]["name"], "v1", "themes 回滚同款: {head}");
    common::stop_lux(handle).await;
}

// ---------- 第 10 组：列表隔离回归（§1.2 脆弱点守护） ----------

/// 写入带版本历史的画面后，GET /screens 不含任何 `sv:` 派生项；即使人为往
/// lux 塞 `screen:{id}:vseq` 形态的键（放宽 id 词法前的隐式保护显式化），
/// 列表也被 valid_id 挡下。
#[tokio::test]
async fn list_isolation_from_version_keys() {
    let (app, handle) = test_app().await;
    let tok = jwt(&app).await;

    put(&app, &tok, "scr", &doc(1), None).await;
    put(&app, &tok, "scr", &doc(2), None).await; // 产生 sv: 键族

    let (_, list) = get(&app, &tok, "/api/v1/screens").await;
    let ids: Vec<&str> = list["screens"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
        .unwrap_or_default();
    assert_eq!(ids, vec!["scr"], "列表只含合法 id: {list}");
    assert!(!buf_contains_sv(&list), "列表不得混入 sv: 派生项: {list}");

    // 人为塞越界键（模拟手工改库/未来放宽 id 词法）：strip 后含冒号 → valid_id 拒绝
    let client = handle.client();
    client
        .execute("SET", &["screen:evil:vseq", "1"])
        .await
        .expect("seed dirty key");
    let (_, list2) = get(&app, &tok, "/api/v1/screens").await;
    let ids2: Vec<&str> = list2["screens"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
        .unwrap_or_default();
    assert_eq!(
        ids2,
        vec!["scr"],
        "越界键（含冒号形态）必须被 valid_id 挡下，不进列表: {list2}"
    );
    common::stop_lux(handle).await;
}

fn buf_contains_sv(v: &serde_json::Value) -> bool {
    v.to_string().contains("sv:")
}

// ---------- 第 11 组：失败矩阵（e2e 近似口径，见文件头说明） ----------

/// lux 停机 → PUT 多命令序列统一 500（不吞错报 ok——22ee8b0 原则延续）。
#[tokio::test]
async fn failure_matrix_lux_down_returns_500_not_ok() {
    let (app, handle) = test_app().await;
    let tok = jwt(&app).await;

    put(&app, &tok, "scr", &doc(1), None).await;

    common::stop_lux(handle).await; // lux 优雅停机（SAVE 后关）

    let resp = put(&app, &tok, "scr", &doc(2), None).await;
    assert_eq!(
        resp.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "lux 故障必须 500，不得静默 ok"
    );
    // 版本列表端点同样如实报错（500），不静默空列表误导调用方
    let (st, _) = get(&app, &tok, "/api/v1/screens/scr/versions").await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
}
