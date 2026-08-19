//! api 模块测试（register + ctx API）。Plan 8 Task 1 拆分自原 api.rs。
//!
//! 覆盖：register 成功 + ctx 暴露 request/log/now/time（冒烟）；kv_set/get round-trip；
//! send_command 经 CtlRegistry 路由；http_get 返回 {status, body}（feature lua-http）。

use super::*;
use crate::host::{BridgeRegistry, CtlRegistry, HostContext};
use crate::plugin::lua_core::io_engine::{spawn_io_task, IoEngine, MockIoTask};
use crate::types::{AppType, DeviceSpec, HostKind, InstConfig, PluginName};
use tokio_util::sync::CancellationToken;

/// 测试用：spawn_io_task 配丢弃被动帧的 on_data channel（api 单测不测 on_data）。
fn spawn_mock_io() -> IoEngine {
    let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    spawn_io_task(
        Box::new(MockIoTask {
            responder: Box::new(|_| Ok(vec![])),
        }),
        tx,
        "test-inst".into(),
        crate::io_trace::TraceRegistry::new(),
    )
}

/// 测试用 mock hub tx（满足 register 的 tx 参数；接收端立即丢弃，不验接收）。
///
/// 现有 L1/L3/L4/L6 测试不触 set_value，故 tx 仅占位；L2 `set_value` 测自建真实 channel。
fn mock_hub_tx() -> tokio::sync::mpsc::Sender<crate::types::Sample> {
    let (tx, _rx) = tokio::sync::mpsc::channel::<crate::types::Sample>(8);
    tx
}

/// 测试用零值丢样计数（满足 register 的 dropped 参数；背压行为测试见
/// `send_with_deadline_drops_on_full_channel`，其余测试只占位不计数）。
fn zero_dropped() -> std::sync::Arc<std::sync::atomic::AtomicU64> {
    std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0))
}

/// 冒烟测：register 成功 + ctx 暴露 request/log_info/now/time（且都是 function）。
#[tokio::test]
async fn register_exposes_ctx_api() {
    let lua = Lua::new();
    let io = spawn_mock_io();
    let (ctx_host, handle, data_dir) = setup_ctx("api").await;
    let inst = InstConfig {
        inst_id: "it".into(),
        inst_name: "it".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: false,
        app_config: serde_json::json!({"script":"a","endpoint":"x"}),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "d".into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    };
    register(
        &lua,
        ctx_host,
        &inst,
        io,
        mock_hub_tx(),
        RuntimeDevs::default(),
        zero_dropped(),
        "tcp",
    )
    .expect("register");
    for api in [
        "request",
        "log_info",
        "log_warn",
        "log_err",
        "log_debug",
        "now",
        "time",
    ] {
        let src = format!("return type(ctx.{api}) == 'function'");
        let is_fn: bool = lua.load(&src).eval().expect(api);
        assert!(is_fn, "ctx.{api} 应是 function");
    }
    // 冒号语法冒烟：ctx:now() / ctx:log_info(...) 应工作（_self 首参吸收冒号隐式 self）。
    let now: f64 = lua
        .load("return ctx:now()")
        .eval()
        .expect("ctx:now 冒号调用");
    assert!(now > 0.0);
    let _: () = lua
        .load("ctx:log_info('hello')")
        .exec()
        .expect("ctx:log_info 冒号调用");

    // 优雅关闭 lux（先 SAVE 再 shutdown），并清理临时 data_dir。
    let _ = crate::db::shutdown_lux(handle).await;
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// kv_set 后 kv_get 命中；类型 round-trip（number/string/bool）；缺省返回 nil。
#[tokio::test]
async fn kv_set_get_roundtrip() {
    let lua = mlua::Lua::new();
    let io = spawn_mock_io();
    let (ctx_host, handle, data_dir) = setup_ctx("kv").await;
    let inst = InstConfig {
        inst_id: "it".into(),
        inst_name: "it".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: false,
        app_config: serde_json::json!({"script":"a","endpoint":"x"}),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "d".into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    };
    register(
        &lua,
        ctx_host,
        &inst,
        io,
        mock_hub_tx(),
        RuntimeDevs::default(),
        zero_dropped(),
        "tcp",
    )
    .expect("register");

    // kv_set/kv_get 是 async function（底层落 lux），用 exec_async/eval_async 驱动。
    lua.load("ctx:kv_set('counter', 42)")
        .exec_async()
        .await
        .expect("kv_set");
    lua.load("ctx:kv_set('name', 'alpha')")
        .exec_async()
        .await
        .expect("kv_set str");
    let n: i64 = lua
        .load("return ctx:kv_get('counter')")
        .eval_async()
        .await
        .expect("kv_get num");
    assert_eq!(n, 42);
    let s: String = lua
        .load("return ctx:kv_get('name')")
        .eval_async()
        .await
        .expect("kv_get str");
    assert_eq!(s, "alpha");
    let nil: mlua::Value = lua
        .load("return ctx:kv_get('missing')")
        .eval_async()
        .await
        .expect("kv_get nil");
    assert_eq!(nil, mlua::Value::Nil, "缺省 key 返回 nil");

    let _ = crate::db::shutdown_lux(handle).await;
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// send_command 经 CtlRegistry 投到目标实例；未注册实例返回 false。
#[tokio::test]
async fn send_command_routes_to_target() {
    let ctl = CtlRegistry::new(std::sync::Arc::new(
        crate::process_metrics::ProcessMetrics::new(),
    ));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::types::CtlMessage>(8);
    ctl.register("target", tx);

    let lua = mlua::Lua::new();
    let io = spawn_mock_io();
    let data_dir = std::env::temp_dir()
        .join(format!("lua-send-{}", std::process::id()))
        .to_str()
        .unwrap()
        .to_string();
    let cfg = crate::config::AppConfig {
        lux: crate::config::LuxConfig {
            port: 0,
            data_dir: data_dir.clone(),
            ..Default::default()
        },
        ..Default::default()
    };
    let handle = lux::run_with_config(crate::db::build_lux_config(&cfg))
        .await
        .expect("lux");
    let ctx_host = HostContext {
        lux: handle.client(),
        hwid: crate::types::Hwid::new("it"),
        shutdown: CancellationToken::new(),
        ctl_registry: ctl.clone(), // 共享：lua send_command → target cmd_rx
        bridge_registry: BridgeRegistry::new(),
        metrics: std::sync::Arc::new(crate::process_metrics::ProcessMetrics::new()),
        trace_registry: crate::io_trace::TraceRegistry::new(),
        event_tx: crate::event_bus::EventBus::dummy(),
    };
    let inst = InstConfig {
        inst_id: "src".into(),
        inst_name: "src".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: false,
        app_config: serde_json::json!({"script":"a","endpoint":"x"}),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "d".into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    };
    register(
        &lua,
        ctx_host,
        &inst,
        io,
        mock_hub_tx(),
        RuntimeDevs::default(),
        zero_dropped(),
        "tcp",
    )
    .expect("register");

    // 命中目标实例。
    let ok: bool = lua
        .load("return ctx:send_command('target', 'dev1', 'write', '{\"v\":1}')")
        .eval_async()
        .await
        .expect("send_command eval");
    assert!(ok, "目标实例已注册应返回 true");
    let m = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("超时")
        .expect("应收到 CtlMessage");
    assert_eq!(m.instance.as_str(), "target");
    assert_eq!(m.devid, "dev1");
    assert_eq!(m.cmd, "write");
    assert_eq!(m.data, "{\"v\":1}");

    // 未注册实例 → false。
    let miss: bool = lua
        .load("return ctx:send_command('nope', 'd', 'write', '{}')")
        .eval_async()
        .await
        .expect("eval");
    assert!(!miss, "未注册实例应返回 false");

    let _ = crate::db::shutdown_lux(handle).await;
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// http_get 返回 {status, body}；经 mock HTTP server 验证。
#[cfg(feature = "lua-http")]
#[tokio::test]
async fn http_get_returns_status_and_body() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    // mock HTTP server：读请求后回 200 + JSON body。
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 1024];
        let _ = s.read(&mut buf).await;
        let body = r#"{"ok":true}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = s.write_all(resp.as_bytes()).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let lua = mlua::Lua::new();
    let io = spawn_mock_io();
    let (ctx_host, handle, data_dir) = setup_ctx("http").await;
    let inst = InstConfig {
        inst_id: "it".into(),
        inst_name: "it".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: false,
        app_config: serde_json::json!({"script":"a","endpoint":"x"}),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "d".into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    };
    register(
        &lua,
        ctx_host,
        &inst,
        io,
        mock_hub_tx(),
        RuntimeDevs::default(),
        zero_dropped(),
        "tcp",
    )
    .expect("register");

    let resp: mlua::Table = lua
        .load(format!("return ctx:http_get('http://127.0.0.1:{port}/x')"))
        .eval_async()
        .await
        .expect("http_get");
    assert_eq!(resp.get::<i64>("status").unwrap(), 200);
    assert_eq!(resp.get::<String>("body").unwrap(), "{\"ok\":true}");

    let _ = crate::db::shutdown_lux(handle).await;
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// T5 http 测试共用：裸 VM + 直接 `http::register`（无 lux/io 依赖——http 闭包只捕获
/// inst_id 与频控器），供频控/clamp 行为断言用；完整 register 的场景测试见上文。
#[cfg(feature = "lua-http")]
fn http_only_vm() -> Lua {
    let lua = Lua::new();
    let inst = InstConfig {
        inst_id: "it".into(),
        inst_name: "it".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: false,
        app_config: serde_json::json!({"script":"a","endpoint":"x"}),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "d".into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    };
    let ctx = lua.create_table().expect("ctx table");
    super::http::register(&lua, &ctx, &inst).expect("http register");
    lua.globals().set("ctx", ctx).expect("ctx 全局");
    lua
}

/// R2 频控（Q3/§4.5-2/T5）：限值查能力卡 `rate_per_sec`（http_get/post 卡均 20/s，共享
/// 一个每实例出站预算）；同一秒窗口内第 21 次调用被拒且错误含 `rate limited（20/s）`
/// （脚本可感知退避）。URL 用非法值让放行的前 20 次在解析层即败（不触网、毫秒级），
/// 保证 21 连发落在同一 1s 窗口内。
#[cfg(feature = "lua-http")]
#[tokio::test]
async fn http_rate_limits_after_card_rate_per_sec() {
    let lua = http_only_vm();
    let mut rejected: Option<(usize, String)> = None;
    for i in 0..25 {
        let r: Result<mlua::Value, mlua::Error> = lua
            .load("return ctx:http_get('http://')")
            .eval_async()
            .await;
        match r {
            Ok(_) => panic!("非法 URL 不应成功（第 {i} 次）"),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("rate limited") {
                    rejected = Some((i, msg));
                    break;
                }
                // 前 20 次是 URL 解析错误（频控放行、请求本身失败）——预期路径。
            }
        }
    }
    let (at, msg) = rejected.expect("25 连发内应出现频控拒绝");
    assert_eq!(at, 20, "第 21 次（索引 20）应被频控拒，实得第 {at} 次");
    assert!(
        msg.contains("rate limited（20/s）"),
        "错误应带限值供脚本退避：{msg}"
    );
}

/// T4 遗留 clamp 集成断言（附录 B 冻结面②）——下限行为级：`timeout=0` 应被宿主钳到 1s
/// （静默 server 上等满 ~1s 才报超时错，而非 0s 立即失败）；上限 3600→30 行为级要等
/// 30s 不进测试套，由 `clamp_timeout_secs` 单测锁边界。
#[cfg(feature = "lua-http")]
#[tokio::test]
async fn http_timeout_clamped_lower_bound_to_1s() {
    use tokio::net::TcpListener;
    // 静默 server：bind 后不 accept 不响应，连接入 backlog、请求挂起等超时。
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let lua = http_only_vm();
    let started = std::time::Instant::now();
    let r: Result<mlua::Value, mlua::Error> = lua
        .load(format!(
            "return ctx:http_get('http://127.0.0.1:{port}/x', nil, 0)"
        ))
        .eval_async()
        .await;
    assert!(r.is_err(), "静默 server 应超时报错");
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(900),
        "timeout=0 应被 clamp 到 1s（实测 {:?}），宿主侧钳制生效",
        started.elapsed()
    );
    drop(listener);
}

/// T4 遗留 clamp 单测锁：缺省 5s；下限 1（0→1）；上限 30（3600→30）。
#[cfg(feature = "lua-http")]
#[test]
fn clamp_timeout_secs_bounds() {
    assert_eq!(super::http::clamp_timeout_secs(None), 5);
    assert_eq!(super::http::clamp_timeout_secs(Some(0)), 1);
    assert_eq!(super::http::clamp_timeout_secs(Some(5)), 5);
    assert_eq!(super::http::clamp_timeout_secs(Some(30)), 30);
    assert_eq!(super::http::clamp_timeout_secs(Some(3600)), 30);
}

/// Q2 一致性锁（真注册断言）：每张 "ctx:" 能力卡必须真注册在 ctx 表上（mock VM）。
///
/// 与 [`crate::plugin::lua_core::capability`] 常量表反向对齐——新增 ctx API 而不登卡
/// （或登卡而不注册）此测试即红。feature 门控卡（lua-http）仅在 feature 开启时断言
/// （关闭时 http::register 不编译，卡上 `feature` 字段即门控事实源）。
#[tokio::test]
async fn capability_cards_match_registered_ctx() {
    let lua = Lua::new();
    let io = spawn_mock_io();
    let (ctx_host, handle, data_dir) = setup_ctx("cap").await;
    let inst = InstConfig {
        inst_id: "it".into(),
        inst_name: "it".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: false,
        app_config: serde_json::json!({"script":"a","endpoint":"x"}),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "d".into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    };
    register(
        &lua,
        ctx_host,
        &inst,
        io,
        mock_hub_tx(),
        RuntimeDevs::default(),
        zero_dropped(),
        "tcp",
    )
    .expect("register");
    let ctx: mlua::Table = lua.globals().get("ctx").expect("ctx 全局表");

    // 期望校验数 = 当前构建实际注册的卡（无门控卡全算；lua-http 卡仅 feature 开时算）。
    let expected = crate::plugin::lua_core::capability::CAPABILITIES
        .iter()
        .filter(|c| c.feature != Some("lua-http") || cfg!(feature = "lua-http"))
        .filter(|c| c.name.starts_with("ctx:"))
        .count();
    let mut checked = 0;
    for c in crate::plugin::lua_core::capability::CAPABILITIES {
        if c.feature == Some("lua-http") && !cfg!(feature = "lua-http") {
            continue; // 门控关闭：http::register 未编译，卡不参与断言
        }
        let Some(method) = c.name.strip_prefix("ctx:") else {
            continue; // builtin 模块卡（json）不挂 ctx 表——行为由 json_roundtrip_and_limit 锁
        };
        assert!(
            ctx.get::<mlua::Function>(method).is_ok(),
            "能力卡 {} 未注册在 ctx 表",
            c.name
        );
        checked += 1;
    }
    assert_eq!(checked, expected, "应校验全部在册 ctx 卡");
    assert!(expected >= 16, "16 个无门控 ctx API 应全部在表（实 {expected}）");

    let _ = crate::db::shutdown_lux(handle).await;
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// T4 防反压（Q3/附录 B 冻结面①）：hub channel 满且无消费者时，裸 `tx.send` 永挂
/// （冻结实例事件循环，stop/reload/cmd 全失效）；`send_with_deadline` 应在 2s 背压
/// 超时后返回 `Dropped` 丢样保实例，而非永挂。`start_paused = true` 虚拟时间加速：
/// 2s 等待真实耗时近零（auto-advance 直跳下一 timer）。
#[tokio::test(start_paused = true)]
async fn send_with_deadline_drops_on_full_channel() {
    fn sample(tag: &str) -> crate::types::Sample {
        crate::types::Sample {
            hwid: crate::types::Hwid::new("it"),
            devid: "d".into(),
            values: [(
                tag.to_string(),
                crate::types::SampleValue::Int(1),
            )]
            .into_iter()
            .collect(),
            timestamp: 0,
        }
    }

    // 满 channel：容量 1 塞 1 条不消费 → 后续 send 无限期等容量。
    let (tx, _rx) = tokio::sync::mpsc::channel::<crate::types::Sample>(1);
    tx.send(sample("a")).await.expect("塞满容量");
    let started = tokio::time::Instant::now();
    let out = send_with_deadline(&tx, sample("b")).await;
    assert!(
        matches!(out, SendOutcome::Dropped),
        "满 channel 应 2s 后 Dropped，实得 {out:?}"
    );
    assert!(
        started.elapsed() >= std::time::Duration::from_secs(2),
        "应等满 2s 背压预算再丢（虚拟时间实 elapsed={:?}）",
        started.elapsed()
    );

    // hub 关闭（接收端 drop）→ Closed（非超时路径）。
    let (tx2, rx2) = tokio::sync::mpsc::channel::<crate::types::Sample>(1);
    drop(rx2);
    let out2 = send_with_deadline(&tx2, sample("c")).await;
    assert!(matches!(out2, SendOutcome::Closed), "hub 关应 Closed");

    // 正常路径（有空位）→ Sent 且消费者可收。
    let (tx3, mut rx3) = tokio::sync::mpsc::channel::<crate::types::Sample>(1);
    let out3 = send_with_deadline(&tx3, sample("d")).await;
    assert!(matches!(out3, SendOutcome::Sent), "有空位应 Sent");
    assert!(rx3.recv().await.is_some(), "Sent 后消费者应能收到");
}

/// 测试用：起独立 lux（端口 0 + 临时 data_dir）+ 标准 HostContext（hwid="it"，
/// 空 registries）。返回 (ctx_host, handle, data_dir)——handle 供 `shutdown_lux`，
/// data_dir 供清理。L2 数据测共用；L3 send_command 测自建 ctl_registry 不走此路径。
async fn setup_ctx(tag: &str) -> (HostContext, lux::ServerHandle, String) {
    let data_dir = std::env::temp_dir()
        .join(format!("lua-{tag}-{}", std::process::id()))
        .to_str()
        .unwrap()
        .to_string();
    let cfg = crate::config::AppConfig {
        lux: crate::config::LuxConfig {
            port: 0,
            data_dir: data_dir.clone(),
            ..Default::default()
        },
        ..Default::default()
    };
    let handle = lux::run_with_config(crate::db::build_lux_config(&cfg))
        .await
        .expect("lux");
    let ctx_host = HostContext {
        lux: handle.client(),
        hwid: crate::types::Hwid::new("it"),
        shutdown: CancellationToken::new(),
        ctl_registry: CtlRegistry::new(std::sync::Arc::new(
            crate::process_metrics::ProcessMetrics::new(),
        )),
        bridge_registry: BridgeRegistry::new(),
        metrics: std::sync::Arc::new(crate::process_metrics::ProcessMetrics::new()),
        trace_registry: crate::io_trace::TraceRegistry::new(),
        event_tx: crate::event_bus::EventBus::dummy(),
    };
    (ctx_host, handle, data_dir)
}

/// L2 set_value 推 Sample 到 hub（devid=inst.devices[0].value，values 含 tag→value，
/// hwid=ctx_host.hwid）。
#[tokio::test]
async fn set_value_pushes_sample_to_hub() {
    let lua = Lua::new();
    let io = spawn_mock_io();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::types::Sample>(8);
    let (ctx_host, handle, data_dir) = setup_ctx("l2-set").await;
    let inst = InstConfig {
        inst_id: "it".into(),
        inst_name: "it".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: false,
        app_config: serde_json::json!({"script":"a","endpoint":"x"}),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "devX".into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    };
    register(&lua, ctx_host, &inst, io, tx, RuntimeDevs::default(), zero_dropped(), "tcp")
        .expect("register");

    lua.load("ctx:set_value('devX', 'temp', 42.5)")
        .exec_async()
        .await
        .expect("set_value");

    let s = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("超时")
        .expect("应收到 Sample");
    assert_eq!(s.devid, "devX");
    assert_eq!(s.hwid.as_str(), "it");
    match s.values.get("temp") {
        Some(crate::types::SampleValue::Float(f)) => {
            assert!((f - 42.5).abs() < 1e-9, "值应为 42.5，实得 {f}")
        }
        other => panic!("temp 应是 Float(42.5)，实得 {other:?}"),
    }

    let _ = crate::db::shutdown_lux(handle).await;
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// L2 set_values(devid, {tag=v,...}) 批量推一个 Sample（多 tag 同时刻）。
#[tokio::test]
async fn set_values_pushes_multi_tag_sample() {
    let lua = Lua::new();
    let io = spawn_mock_io();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::types::Sample>(8);
    let (ctx_host, handle, data_dir) = setup_ctx("l2-setvalues").await;
    let inst = InstConfig {
        inst_id: "it".into(),
        inst_name: "it".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: false,
        app_config: serde_json::json!({"script":"a","endpoint":"x"}),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "devX".into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    };
    register(&lua, ctx_host, &inst, io, tx, RuntimeDevs::default(), zero_dropped(), "tcp")
        .expect("register");

    lua.load("ctx:set_values('devX', { temp = 42.5, hum = 60 })")
        .exec_async()
        .await
        .expect("set_values");

    let s = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("超时")
        .expect("应收到一个 Sample");
    assert_eq!(s.devid, "devX", "一个 Sample 一个 devid");
    assert_eq!(s.values.len(), 2, "应含 2 个 tag（批量）");
    match s.values.get("temp") {
        Some(crate::types::SampleValue::Float(f)) => {
            assert!((f - 42.5).abs() < 1e-9, "temp 应为 42.5，实得 {f}")
        }
        other => panic!("temp 应是 Float(42.5)，实得 {other:?}"),
    }
    match s.values.get("hum") {
        Some(crate::types::SampleValue::Int(i)) => assert_eq!(*i, 60, "hum 应为 60"),
        other => panic!("hum 应是 Int(60)，实得 {other:?}"),
    }

    let _ = crate::db::shutdown_lux(handle).await;
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// L2 set_value(devid, tag, v) 按 devid 路由：多设备实例对不同 devid 各推一个 Sample。
#[tokio::test]
async fn set_value_routes_by_devid() {
    let lua = Lua::new();
    let io = spawn_mock_io();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::types::Sample>(8);
    let (ctx_host, handle, data_dir) = setup_ctx("l2-route").await;
    let inst = InstConfig {
        inst_id: "it".into(),
        inst_name: "it".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: false,
        app_config: serde_json::json!({"script":"a","endpoint":"x"}),
        devices: vec![
            DeviceSpec {
                label: "A".into(),
                value: "devA".into(),
                children: vec![],
                slave_id: None,
            },
            DeviceSpec {
                label: "B".into(),
                value: "devB".into(),
                children: vec![],
                slave_id: None,
            },
        ],
        host_kind: HostKind::Builtin,
    };
    register(&lua, ctx_host, &inst, io, tx, RuntimeDevs::default(), zero_dropped(), "tcp")
        .expect("register");

    lua.load("ctx:set_value('devB', 'v', 2)")
        .exec_async()
        .await
        .expect("set_value devB");

    let s = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("超时")
        .expect("应收到 Sample");
    assert_eq!(s.devid, "devB", "devid 应路由到 devB（非 devices[0]）");

    let _ = crate::db::shutdown_lux(handle).await;
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// L2 get_value 读 lux rtdata 最新值（经 db::rtdata::write 写入 → get_value 读回）。
#[tokio::test]
async fn get_value_reads_rtdata() {
    let lua = Lua::new();
    let io = spawn_mock_io();
    let (ctx_host, handle, data_dir) = setup_ctx("l2-gv").await;
    let inst = InstConfig {
        inst_id: "it".into(),
        inst_name: "it".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: false,
        app_config: serde_json::json!({"script":"a","endpoint":"x"}),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "devX".into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    };
    // 先写 rtdata（对标 pipeline 入库格式），再让 get_value 读回。
    let mut values = std::collections::BTreeMap::new();
    values.insert("temp".to_string(), crate::types::SampleValue::Float(42.5));
    crate::db::rtdata::write(&ctx_host.lux, "it", "devX", &values, 1_700_000_000)
        .await
        .expect("rtdata write");
    register(
        &lua,
        ctx_host,
        &inst,
        io,
        mock_hub_tx(),
        RuntimeDevs::default(),
        zero_dropped(),
        "tcp",
    )
    .expect("register");

    let v: String = lua
        .load("return ctx:get_value('devX', 'temp')")
        .eval_async()
        .await
        .expect("get_value");
    assert_eq!(v, "42.5");

    // 不存在的 tag → nil。
    let nil: mlua::Value = lua
        .load("return ctx:get_value('devX', 'missing')")
        .eval_async()
        .await
        .expect("get_value nil");
    assert_eq!(nil, mlua::Value::Nil, "缺省 tag 应返回 nil");

    let _ = crate::db::shutdown_lux(handle).await;
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// L2 get_device 读 lux HGETALL rtdata:<devid> → {tag: value_str}（全部数据点）。
#[tokio::test]
async fn get_device_returns_rtdata_values() {
    let lua = Lua::new();
    let io = spawn_mock_io();
    let (ctx_host, handle, data_dir) = setup_ctx("l2-gd").await;
    let inst = InstConfig {
        inst_id: "it".into(),
        inst_name: "it".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: false,
        app_config: serde_json::json!({"script":"a","endpoint":"x"}),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "devX".into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    };
    let mut values = std::collections::BTreeMap::new();
    values.insert("temp".to_string(), crate::types::SampleValue::Float(42.5));
    values.insert("hum".to_string(), crate::types::SampleValue::Int(60));
    crate::db::rtdata::write(&ctx_host.lux, "it", "devX", &values, 1_700_000_000)
        .await
        .expect("rtdata write");
    register(
        &lua,
        ctx_host,
        &inst,
        io,
        mock_hub_tx(),
        RuntimeDevs::default(),
        zero_dropped(),
        "tcp",
    )
    .expect("register");

    let t: mlua::Table = lua
        .load("return ctx:get_device('devX')")
        .eval_async()
        .await
        .expect("get_device");
    assert_eq!(t.get::<String>("temp").unwrap(), "42.5");
    assert_eq!(t.get::<String>("hum").unwrap(), "60");

    // 不存在的设备 → nil。
    let nil: mlua::Value = lua
        .load("return ctx:get_device('nope')")
        .eval_async()
        .await
        .expect("get_device nil");
    assert_eq!(nil, mlua::Value::Nil, "缺省设备应返回 nil");

    let _ = crate::db::shutdown_lux(handle).await;
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// L2 list_devices 返 inst.devices（{label, value} 数组；实例自身设备，非全库）。
#[tokio::test]
async fn list_devices_returns_inst_devices() {
    let lua = Lua::new();
    let io = spawn_mock_io();
    let (ctx_host, handle, data_dir) = setup_ctx("l2-ld").await;
    let inst = InstConfig {
        inst_id: "it".into(),
        inst_name: "it".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: false,
        app_config: serde_json::json!({"script":"a","endpoint":"x"}),
        devices: vec![
            DeviceSpec {
                label: "A".into(),
                value: "devX".into(),
                children: vec![],
                slave_id: None,
            },
            DeviceSpec {
                label: "B".into(),
                value: "devY".into(),
                children: vec![],
                slave_id: None,
            },
        ],
        host_kind: HostKind::Builtin,
    };
    register(
        &lua,
        ctx_host,
        &inst,
        io,
        mock_hub_tx(),
        RuntimeDevs::default(),
        zero_dropped(),
        "tcp",
    )
    .expect("register");

    // list_devices 是同步函数（create_function），用 eval（非 eval_async）。
    let t: mlua::Table = lua
        .load("return ctx:list_devices()")
        .eval()
        .expect("list_devices");
    assert_eq!(t.len().unwrap(), 2, "应返 2 个设备");
    let d1: mlua::Table = t.get::<mlua::Table>(1).unwrap();
    assert_eq!(d1.get::<String>("label").unwrap(), "A");
    assert_eq!(d1.get::<String>("value").unwrap(), "devX");
    let d2: mlua::Table = t.get::<mlua::Table>(2).unwrap();
    assert_eq!(d2.get::<String>("label").unwrap(), "B");
    assert_eq!(d2.get::<String>("value").unwrap(), "devY");

    let _ = crate::db::shutdown_lux(handle).await;
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// T8 builtin json 模块（§4.1/D5，C0/R0）：经 `require("json")` 完整路径（script.rs
/// Builtin 分发 → `api::builtin_module` → `api::json::make_module`）验证——
/// 1) encode→decode roundtrip 取回字段（Lua 表键序不保证——不锁序列化字符串）；
/// 2) 嵌套数组 `{"k":[1,2,3]}` → k[2]==2；
/// 3) decode 超 1MB 输入 → RuntimeError "json: 输入超 1MB"（先拒后解析，§4.5）；
/// 4) 非法 JSON → 错误含 "解析失败"。
#[test]
fn json_roundtrip_and_limit() {
    let tmp = std::env::temp_dir().join(format!("lua-json8-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("devA/lib")).unwrap();
    std::fs::write(
        tmp.join("devA/main.lua"),
        r#"
local json = require("json")
-- 1) roundtrip：encode 后 decode 取回字段（不锁 key 序）
local back = json.decode(json.encode({ a = 1, b = "x" }))
rt_ok = (back.a == 1 and back.b == "x")
-- 2) 嵌套数组
local d = json.decode('{"k":[1,2,3]}')
arr_ok = (d.k[2] == 2)
-- 3) 超 1MB 输入拒绝（pcall 捕错误信息）
local _, over_err = pcall(json.decode, string.rep("x", 1024 * 1024 + 1))
over_msg = tostring(over_err)
-- 4) 非法 JSON → 解析失败
local _, bad_err = pcall(json.decode, "{")
bad_msg = tostring(bad_err)
"#,
    )
    .unwrap();

    let cfg = crate::plugin::lua_core::config::parse_cfg(&serde_json::json!({
        "script": "devA", "endpoint": "x"
    }))
    .unwrap();
    let lua = crate::plugin::lua_core::engine::new_vm(&cfg).unwrap();
    let lctx = crate::plugin::lua_core::script::LoadCtx {
        scripts_root: std::sync::Arc::from(tmp.as_path()),
        script_name: std::sync::Arc::<str>::from("devA"),
    };
    crate::plugin::lua_core::script::load_main(&lua, &lctx).expect("load_main（含 require json）");

    assert!(
        lua.load("return rt_ok").eval::<bool>().unwrap(),
        "encode→decode 应取回原字段（.a==1 且 .b=='x'）"
    );
    assert!(
        lua.load("return arr_ok").eval::<bool>().unwrap(),
        "嵌套数组 {{\"k\":[1,2,3]}} 解码后 k[2] 应为 2"
    );
    let over: String = lua.load("return over_msg").eval().unwrap();
    assert!(
        over.contains("超 1MB"),
        "超 1MB 输入应先拒后解析（错误含 '超 1MB'），实得：{over}"
    );
    let bad: String = lua.load("return bad_msg").eval().unwrap();
    assert!(
        bad.contains("解析失败"),
        "非法 JSON 错误应含 '解析失败'，实得：{bad}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// `ctx:transport()` 返回 register 时传入的链路类型（tcp/serial/udp），脚本据此选帧格式
///（serial → PDU+CRC16、tcp → MBAP 头）。三种 kind 各注册一次验返回值一致。
#[tokio::test]
async fn ctx_transport_returns_configured_kind() {
    for (kind, expected) in [("tcp", "tcp"), ("serial", "serial"), ("udp", "udp")] {
        let lua = Lua::new();
        let io = spawn_mock_io();
        let (ctx_host, handle, data_dir) = setup_ctx(&format!("transport-{kind}")).await;
        let inst = InstConfig {
            inst_id: "it".into(),
            inst_name: "it".into(),
            app_code: PluginName::new("luaClient"),
            app_type: AppType::South,
            auto_start: false,
            app_config: serde_json::json!({"script":"a","endpoint":"x"}),
            devices: vec![DeviceSpec {
                label: "d".into(),
                value: "d".into(),
                children: vec![],
                slave_id: None,
            }],
            host_kind: HostKind::Builtin,
        };
        register(
            &lua,
            ctx_host,
            &inst,
            io,
            mock_hub_tx(),
            RuntimeDevs::default(),
            zero_dropped(),
            kind,
        )
        .expect("register");
        let got: String = lua
            .load("return ctx:transport()")
            .eval()
            .expect("ctx:transport eval");
        assert_eq!(got, expected, "ctx:transport() 应返回 register 传入的 kind");

        let _ = crate::db::shutdown_lux(handle).await;
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}
