//! init_failed 真实路径独立 e2e（Phase 3 Task 4，P9）。
//!
//! 与 `history_visibility_e2e`（注入态 AppState）不同，本测试走 `lib.rs`
//! 真实 init 折叠：先对临时目录 `build_storage` 占住 `.tsink.lock`（句柄
//! 存活即持锁），再 `run_app_with_shutdown` 进程内起**全量 app**（真实 lux
//! + identity bootstrap + axum 真端口），断言三面：
//!
//! 1. `GET /api/v1/history` → 503，body `{error:"history unavailable",
//!    reason:"init_failed"}`（Phase 2 P5 契约）；
//! 2. `GET /api/v1/sysstatus` → history.engine=="init_failed"、stats null、
//!    Phase 3 治理字段省略（P6 非 ok 整体省略语义）；
//! 3. `GET /api/v1/tags` → 200（fail-soft：归档初始化失败不影响主链路）。
//!
//! P9 预期定性：行为已正确，首跑即绿=固化测试（非红→绿）；若红=真缺陷。
//!
//! 端口模型：api_port 先 bind 127.0.0.1:0 选空闲再 drop 复用（TOCTOU 竞态
//! 可接受，同 `lua_mqtt_pass_e2e` 选端口先例）；lux 走 port 0 由 OS 分配。

#![cfg(feature = "history")]

use rs_iot::config::{AppConfig, HistoryConfig, LogConfig, LuxConfig, MqttConfig};
use rs_iot::history::engine::build_storage;
use rs_iot::run_app_with_shutdown;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// 唯一临时目录：tag + pid + 纳秒时钟（`src/history/engine.rs` 测试 unique_dir
/// 惯例——dataflow 的 `test_data_dir` 复用同名目录，而本测试目录须全新保证
/// `.tsink.lock` 可被占锁方首次获取，故用纳秒唯一形态）。
fn unique_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "rsiot-history-initfailed-{tag}-{}-{nanos}",
        std::process::id()
    ))
}

/// 选空闲 TCP 端口：bind 127.0.0.1:0 取端口后 drop 再用（drop 与复用间的
/// 竞态窗口可接受——仅防与残留进程撞号，照 `lua_mqtt_pass_e2e` 先例）。
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind :0 选空闲端口")
        .local_addr()
        .expect("local_addr")
        .port()
}

/// init_failed 真实路径：占锁 → 全量 app → 503/三态/fail-soft → 优雅退出。
#[tokio::test]
async fn init_failed_real_lock_503_and_failsoft() {
    // ---- 1. 占锁：临时目录 A 上 build_storage 一次，句柄存活即 .tsink.lock 被占 ----
    let hist_dir = unique_dir("lock");
    let lock_cfg = HistoryConfig {
        data_dir: hist_dir.to_string_lossy().into_owned(),
        ..HistoryConfig::default()
    };
    let lock_handle = build_storage(&lock_cfg, 8).expect("占锁 build_storage 必须成功(全新目录)");

    // ---- 2. AppConfig：history.enabled=true 指向同目录；lux 独立唯一目录(port 0)； ----
    // api_port 空闲；mqtt 显式关(host="" 不生成默认北向)；log 文件层关(dir=""
    // 不落 ./logs)；其余极简(无采集实例，lux 全新无 autoStart)。
    let lux_dir = unique_dir("lux");
    let api_port = free_port();
    let cfg = AppConfig {
        lux: LuxConfig {
            port: 0,
            data_dir: lux_dir.to_string_lossy().into_owned(),
            ..LuxConfig::default()
        },
        api_port,
        mqtt: MqttConfig {
            host: String::new(),
            ..MqttConfig::default()
        },
        log: LogConfig {
            dir: String::new(),
            ..LogConfig::default()
        },
        hwid: "initfailed-e2e".into(),
        history: HistoryConfig {
            enabled: true,
            data_dir: hist_dir.to_string_lossy().into_owned(),
            cardinality_limit: 8,
            ..HistoryConfig::default()
        },
        ..AppConfig::default()
    };

    // ---- 3. 起全量 app：真 run_app_with_shutdown（init 折叠在 axum bind 之前， ----
    // 端口就绪时 engine_state 已定格）。ring buffer / event bus 用测试构造面
    // （dataflow 同款），不装 tracing subscriber（日志宏无 subscriber 为 no-op）。
    let shutdown = CancellationToken::new();
    let app_token = shutdown.clone();
    let ring = Arc::new(rs_iot::log_ringbuffer::LogRingBuffer::new(200));
    let bus = rs_iot::event_bus::EventBus::dummy();
    let app_handle =
        tokio::spawn(async move { run_app_with_shutdown(cfg, app_token, ring, bus).await });

    // ---- 4. 轮询端口就绪（HTTP 可连；/metrics 免 JWT 白名单，超时 10s） ----
    let base = format!("http://127.0.0.1:{api_port}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("reqwest client build");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if client.get(format!("{base}/metrics")).send().await.is_ok() {
            break; // 任何 HTTP 响应(含 404)即端口在服务
        }
        assert!(
            Instant::now() < deadline,
            "REST :{api_port} 10s 内未就绪(全量 app 启动失败?)"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // ---- 5. 登录 admin/admin 拿 JWT（identity bootstrap 默认；reqwest 发 HTTP） ----
    let login = client
        .post(format!("{base}/api/v1/user/login"))
        .json(&serde_json::json!({"user_id": "admin", "pass": "admin"}))
        .send()
        .await
        .expect("login 请求");
    assert_eq!(
        login.status(),
        200,
        "admin/admin 登录必须成功(bootstrap 默认)"
    );
    let login_body: serde_json::Value = login.json().await.expect("login body JSON");
    let jwt = login_body["token"]
        .as_str()
        .expect("token 字段")
        .to_string();
    let auth = format!("Bearer {jwt}");

    // ---- ① /api/v1/history → 503 + reason=init_failed（P5 契约） ----
    let resp = client
        .get(format!(
            "{base}/api/v1/history?dev_id=d&tag=t&start=1&end=2"
        ))
        .header("Authorization", auth.as_str())
        .send()
        .await
        .expect("history 请求");
    assert_eq!(resp.status(), 503, "init_failed 态读端点必须 503");
    let body: serde_json::Value = resp.json().await.expect("503 body 必须是 JSON");
    assert_eq!(body["error"], "history unavailable", "error 字段照 P5 契约");
    assert_eq!(
        body["reason"], "init_failed",
        "reason 必须命中 init_failed 分支"
    );

    // ---- ② /sysstatus → engine=init_failed、stats null、治理字段省略（P6） ----
    let resp = client
        .get(format!("{base}/api/v1/sysstatus"))
        .header("Authorization", auth.as_str())
        .send()
        .await
        .expect("sysstatus 请求");
    assert_eq!(resp.status(), 200);
    let sys: serde_json::Value = resp.json().await.expect("sysstatus body JSON");
    let hist = &sys["history"];
    assert_eq!(
        hist["engine"], "init_failed",
        "引擎三态必须暴露 init_failed"
    );
    assert!(hist["stats"].is_null(), "非 ok 态 stats 恒 null");
    assert_eq!(hist["feature"], true, "feature on 构建");
    assert_eq!(hist["enabled"], true, "L2 配置已开(init 失败非配置关)");
    for key in [
        "memory_used_bytes",
        "memory_budget_bytes",
        "series",
        "archived_tags",
        "deadbands",
    ] {
        assert!(
            hist.get(key).is_none(),
            "init_failed 态治理字段 {key} 应整体省略(非 null,P6)"
        );
    }

    // ---- ③ /api/v1/tags → 200（fail-soft：主链路存活） ----
    let resp = client
        .get(format!("{base}/api/v1/tags"))
        .header("Authorization", auth.as_str())
        .send()
        .await
        .expect("tags 请求");
    assert_eq!(
        resp.status(),
        200,
        "归档 init 失败不得影响主链路(fail-soft)"
    );

    // ---- ③b /metrics → history family 省略（P6 契约的真实 init_failed 态锁点；
    //      原仅 visibility e2e 注入态覆盖，审查 Phase3-L6）----
    let resp = client
        .get(format!("{base}/metrics"))
        .send()
        .await
        .expect("metrics 请求");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("metrics body 文本");
    assert!(
        !body.contains("rs_iot_history_"),
        "init_failed 态 /metrics 不得输出 history family（误导性零值防线）"
    );

    // ---- 6. cancel → app 正常退出（返回 Ok；30s 兜底容 pipeline join 5s + lux SAVE） ----
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(30), app_handle)
        .await
        .expect("app 应在 30s 内退出")
        .expect("app join 不 panic")
        .expect("run_app_with_shutdown 应 Ok 退出");

    // ---- 7. 释放占锁句柄 + 清理临时目录（best-effort；Windows 句柄释放有延迟） ----
    drop(lock_handle);
    let _ = std::fs::remove_dir_all(&hist_dir);
    let _ = std::fs::remove_dir_all(&lux_dir);
}
