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
    let (engine, _join) = spawn_io_task(
        Box::new(MockIoTask {
            responder: Box::new(|_| Ok(vec![])),
        }),
        tx,
        "test-inst".into(),
        crate::io_trace::TraceRegistry::new(),
        std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        tokio_util::sync::CancellationToken::new(),
    );
    engine
}

/// 测试用 mock hub tx（满足 ApiCtx.tx 字段；接收端立即丢弃，不验接收）。
///
/// 现有 L1/L3/L4/L6 测试不触 set_value，故 tx 仅占位；L2 `set_value` 测自建真实 channel。
fn mock_hub_tx() -> tokio::sync::mpsc::Sender<crate::types::Sample> {
    let (tx, _rx) = tokio::sync::mpsc::channel::<crate::types::Sample>(8);
    tx
}

/// 测试用零值丢样计数（满足 ApiCtx.dropped 字段；背压行为测试见
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
        &ApiCtx {
            ctx_host,
            inst: &inst,
            io: Some(io),
            role: Role::default(),
            tx: mock_hub_tx(),
            runtime_devs: RuntimeDevs::default(),
            dropped: zero_dropped(),
            self_writes: None,
            alarm: AlarmStats::default(),
            transport: "tcp",
            scripts_root: Path::new("scripts"),
        },
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

/// T9 接线：register 末尾挂全局 `stdlib` 表——脚本侧 `stdlib.list()/info()` 可用。
/// scripts_root 指向全新临时目录（无 `_stdlib/.index.json`）→ 纯种子视图，确定性。
#[tokio::test]
async fn register_exposes_stdlib_table() {
    let lua = Lua::new();
    let io = spawn_mock_io();
    let (ctx_host, handle, data_dir) = setup_ctx("stdlib-tbl").await;
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
    let tmp = std::env::temp_dir().join(format!("lua-stdlib-reg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    register(
        &lua,
        &ApiCtx {
            ctx_host,
            inst: &inst,
            io: Some(io),
            role: Role::default(),
            tx: mock_hub_tx(),
            runtime_devs: RuntimeDevs::default(),
            dropped: zero_dropped(),
            self_writes: None,
            alarm: AlarmStats::default(),
            transport: "tcp",
            scripts_root: &tmp,
        },
    )
    .expect("register");
    let n: i64 = lua
        .load("return #stdlib.list()")
        .eval()
        .expect("stdlib.list 应可用");
    assert_eq!(n as usize, 4, "纯种子视图应列 4 库（algo/codec/util×2）");
    let src: String = lua
        .load(r#"return stdlib.info("algo.crc16").source"#)
        .eval()
        .expect("stdlib.info 应可用");
    assert_eq!(src, "seed", "无 index 时徽标应为 seed");

    let _ = crate::db::shutdown_lux(handle).await;
    let _ = std::fs::remove_dir_all(&data_dir);
    let _ = std::fs::remove_dir_all(&tmp);
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
        &ApiCtx {
            ctx_host,
            inst: &inst,
            io: Some(io),
            role: Role::default(),
            tx: mock_hub_tx(),
            runtime_devs: RuntimeDevs::default(),
            dropped: zero_dropped(),
            self_writes: None,
            alarm: AlarmStats::default(),
            transport: "tcp",
            scripts_root: Path::new("scripts"),
        },
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
        &ApiCtx {
            ctx_host,
            inst: &inst,
            io: Some(io),
            role: Role::default(),
            tx: mock_hub_tx(),
            runtime_devs: RuntimeDevs::default(),
            dropped: zero_dropped(),
            self_writes: None,
            alarm: AlarmStats::default(),
            transport: "tcp",
            scripts_root: Path::new("scripts"),
        },
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
        &ApiCtx {
            ctx_host,
            inst: &inst,
            io: Some(io),
            role: Role::default(),
            tx: mock_hub_tx(),
            runtime_devs: RuntimeDevs::default(),
            dropped: zero_dropped(),
            self_writes: None,
            alarm: AlarmStats::default(),
            transport: "tcp",
            scripts_root: Path::new("scripts"),
        },
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

/// strip_dev_prefix 单测锁（终审计杂项登记收拢，4 处消费共用语义）：
/// 双形态（"tag" / "devid.tag"）与不可剥形态（他设备前缀 / 裸 devid / 空串）。
#[test]
fn strip_dev_prefix_dual_forms() {
    use super::strip_dev_prefix;
    // 全形态：带本 dev 前缀 → 剥；裸 tag → 原样。
    assert_eq!(strip_dev_prefix("dev1", "dev1.temp"), "temp");
    assert_eq!(strip_dev_prefix("dev1", "temp"), "temp");
    // 不可剥：他设备前缀 / tag 即 devid（无点分隔）/ devid 是 tag 前缀但非点界。
    assert_eq!(strip_dev_prefix("dev1", "dev2.temp"), "dev2.temp");
    assert_eq!(strip_dev_prefix("dev1", "dev1"), "dev1");
    assert_eq!(strip_dev_prefix("dev1", "dev10.temp"), "dev10.temp");
    // 边界：空 devid（前缀 "."）/ 空 tag。
    assert_eq!(strip_dev_prefix("", ".t"), "t");
    assert_eq!(strip_dev_prefix("", "t"), "t");
    assert_eq!(strip_dev_prefix("dev1", ""), "");
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
        &ApiCtx {
            ctx_host,
            inst: &inst,
            io: Some(io),
            role: Role::default(),
            tx: mock_hub_tx(),
            runtime_devs: RuntimeDevs::default(),
            dropped: zero_dropped(),
            self_writes: None,
            alarm: AlarmStats::default(),
            transport: "tcp",
            scripts_root: Path::new("scripts"),
        },
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
    assert!(
        expected >= 16,
        "16 个无门控 ctx API 应全部在表（实 {expected}）"
    );

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
            values: [(tag.to_string(), crate::types::SampleValue::Int(1))]
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
    register(
        &lua,
        &ApiCtx {
            ctx_host,
            inst: &inst,
            io: Some(io),
            role: Role::default(),
            tx,
            runtime_devs: RuntimeDevs::default(),
            dropped: zero_dropped(),
            self_writes: None,
            alarm: AlarmStats::default(),
            transport: "tcp",
            scripts_root: Path::new("scripts"),
        },
    )
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
    register(
        &lua,
        &ApiCtx {
            ctx_host,
            inst: &inst,
            io: Some(io),
            role: Role::default(),
            tx,
            runtime_devs: RuntimeDevs::default(),
            dropped: zero_dropped(),
            self_writes: None,
            alarm: AlarmStats::default(),
            transport: "tcp",
            scripts_root: Path::new("scripts"),
        },
    )
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
    register(
        &lua,
        &ApiCtx {
            ctx_host,
            inst: &inst,
            io: Some(io),
            role: Role::default(),
            tx,
            runtime_devs: RuntimeDevs::default(),
            dropped: zero_dropped(),
            self_writes: None,
            alarm: AlarmStats::default(),
            transport: "tcp",
            scripts_root: Path::new("scripts"),
        },
    )
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
        &ApiCtx {
            ctx_host,
            inst: &inst,
            io: Some(io),
            role: Role::default(),
            tx: mock_hub_tx(),
            runtime_devs: RuntimeDevs::default(),
            dropped: zero_dropped(),
            self_writes: None,
            alarm: AlarmStats::default(),
            transport: "tcp",
            scripts_root: Path::new("scripts"),
        },
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
        &ApiCtx {
            ctx_host,
            inst: &inst,
            io: Some(io),
            role: Role::default(),
            tx: mock_hub_tx(),
            runtime_devs: RuntimeDevs::default(),
            dropped: zero_dropped(),
            self_writes: None,
            alarm: AlarmStats::default(),
            transport: "tcp",
            scripts_root: Path::new("scripts"),
        },
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
        &ApiCtx {
            ctx_host,
            inst: &inst,
            io: Some(io),
            role: Role::default(),
            tx: mock_hub_tx(),
            runtime_devs: RuntimeDevs::default(),
            dropped: zero_dropped(),
            self_writes: None,
            alarm: AlarmStats::default(),
            transport: "tcp",
            scripts_root: Path::new("scripts"),
        },
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
            &ApiCtx {
                ctx_host,
                inst: &inst,
                io: Some(io),
                role: Role::default(),
                tx: mock_hub_tx(),
                runtime_devs: RuntimeDevs::default(),
                dropped: zero_dropped(),
                self_writes: None,
                alarm: AlarmStats::default(),
                transport: kind,
                scripts_root: Path::new("scripts"),
            },
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

/// T5 ctx:secret_get 授权清单语义（Q4/R3/D6）：在清单且已写 → 明文；越权可感知
/// （错误含「未授权」非静默 nil——脚本作者需要知道为什么读不到）；无前缀 ref 报错
/// （脚本侧防线，提示可行动）；在清单但 lux 未写 → nil（脚本可判空给默认）。
#[tokio::test]
async fn secret_get_allowlist_semantics() {
    let lua = Lua::new();
    let io = spawn_mock_io();
    let (ctx_host, handle, data_dir) = setup_ctx("t5-secret").await;
    let inst = InstConfig {
        inst_id: "it".into(),
        inst_name: "it".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: false,
        // 实例授权清单：test_key（已预写）/ absent_key（故意不写，验缺省 nil）。
        app_config: serde_json::json!({
            "script": "a", "endpoint": "x",
            "secrets": ["secret:test_key", "secret:absent_key"]
        }),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "d".into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    };
    // 预写 lux secrets（对标 REST 管理面写入路径，T4 routes_secret）。
    crate::db::secrets::set(&ctx_host.lux, "secret:test_key", "plain-1")
        .await
        .expect("预写 secret");
    register(
        &lua,
        &ApiCtx {
            ctx_host,
            inst: &inst,
            io: Some(io),
            role: Role::default(),
            tx: mock_hub_tx(),
            runtime_devs: RuntimeDevs::default(),
            dropped: zero_dropped(),
            self_writes: None,
            alarm: AlarmStats::default(),
            transport: "tcp",
            scripts_root: Path::new("scripts"),
        },
    )
    .expect("register");

    // 1) 在清单且已写 → 明文。
    let v: String = lua
        .load("return ctx:secret_get('secret:test_key')")
        .eval_async()
        .await
        .expect("授权 ref 应读到明文");
    assert_eq!(v, "plain-1");

    // 2) 不在清单 → 越权可感知：报错（含 ref 与清单提示），非静默 nil。
    let e = lua
        .load("return ctx:secret_get('secret:other')")
        .eval_async::<String>()
        .await
        .expect_err("越权引用应报错");
    let msg = e.to_string();
    assert!(msg.contains("未授权"), "错误应含「未授权」：{msg}");
    assert!(
        msg.contains("secret:other"),
        "错误应带 ref 本身（ref 非机密，可行动）：{msg}"
    );
    assert!(
        msg.contains("secrets"),
        "错误应提示检查实例配置 secrets 清单：{msg}"
    );

    // 3) 无前缀 ref → 报错（防读任意 lux key 的企图；提示应指向加前缀而非改清单）。
    let e = lua
        .load("return ctx:secret_get('foo')")
        .eval_async::<String>()
        .await
        .expect_err("无前缀 ref 应报错");
    assert!(
        e.to_string().contains("前缀"),
        "错误应含前缀提示（可行动）：{e}"
    );

    // 4) 在清单但 lux 未写 → nil。
    let nil: mlua::Value = lua
        .load("return ctx:secret_get('secret:absent_key')")
        .eval_async()
        .await
        .expect("在清单未写应返回 nil 而非报错");
    assert_eq!(
        nil,
        mlua::Value::Nil,
        "配置了但没写入 → nil（脚本判空给默认）"
    );

    let _ = crate::db::shutdown_lux(handle).await;
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// B 阶段 T6 角色裁剪（§5.2 矩阵 mqtt-north 列）：North 角色注册的 ctx 表**无**
/// C1（request/transport——io_engine 也不构建，`io: None`）；C0/C2/C4/C6 与
/// C3 send_command 保留（Q5 定案：down() 下行依赖 send_command，恒注入——
/// R3 显式授权收紧排后续 task）。
#[tokio::test]
async fn north_role_skips_c1_request_transport() {
    let lua = Lua::new();
    let (ctx_host, handle, data_dir) = setup_ctx("north-role").await;
    let inst = InstConfig {
        inst_id: "it".into(),
        inst_name: "it".into(),
        app_code: PluginName::new("luaNorth"),
        app_type: AppType::North,
        auto_start: false,
        app_config: serde_json::json!({"script":"a","host":"127.0.0.1"}),
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
        &ApiCtx {
            ctx_host,
            inst: &inst,
            io: None,
            tx: mock_hub_tx(),
            runtime_devs: RuntimeDevs::default(),
            dropped: zero_dropped(),
            self_writes: None,
            alarm: AlarmStats::default(),
            transport: "mqtt",
            scripts_root: Path::new("scripts"),
            role: Role::North,
        },
    )
    .expect("register");

    // C1 裁剪断言：ctx.request / ctx.transport 应为 nil（表上无此键）。
    for absent in ["request", "transport"] {
        let v: mlua::Value = lua
            .load(format!("return ctx.{absent}"))
            .eval()
            .expect(absent);
        assert_eq!(v, mlua::Value::Nil, "north ctx 应无 {absent}（C1 裁剪）");
    }
    // 其余能力保留：C2 set_value / C4 kv_get·secret_get / C3 send_command / C6 log / C0 now。
    for present in [
        "set_value",
        "kv_get",
        "secret_get",
        "send_command",
        "log_info",
        "now",
    ] {
        let is_fn: bool = lua
            .load(format!("return type(ctx.{present}) == 'function'"))
            .eval()
            .expect(present);
        assert!(is_fn, "north ctx 应有 {present}（§5.2 矩阵 mqtt-north 列）");
    }

    let _ = crate::db::shutdown_lux(handle).await;
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// C 阶段 T4 角色裁剪（§5.2 矩阵 transform 列）：Transform 角色注册的 ctx 表**无**
/// C1（request/transport——与 north 同因：无传输层，io=None）、C3 send_command、
/// C5 http_get/http_post（feature lua-http 开启时断言）；**有** C0/C2/C4/C6
/// （set_value/get_value/get_device/list_devices/kv_set/kv_get/secret_get/log_info/
/// now——secret_get 恒注入是 Q4 授权清单门控，非角色裁剪）。
#[tokio::test]
async fn transform_role_trims_c1_c3_c5() {
    let lua = Lua::new();
    let (ctx_host, handle, data_dir) = setup_ctx("transform-role").await;
    let inst = InstConfig {
        inst_id: "it".into(),
        inst_name: "it".into(),
        app_code: PluginName::new("luaTransform"),
        app_type: AppType::South, // Source 订阅型实例（Q19/D1），不走 Processor
        auto_start: false,
        app_config: serde_json::json!({"script":"a"}),
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
        &ApiCtx {
            ctx_host,
            inst: &inst,
            io: None, // transform 无传输层（同 north：C1 不注册，IO 引擎不构建）
            tx: mock_hub_tx(),
            runtime_devs: RuntimeDevs::default(),
            dropped: zero_dropped(),
            self_writes: None,
            alarm: AlarmStats::default(),
            transport: "none", // 占位（transform 不注册 transport，值不被消费）
            scripts_root: Path::new("scripts"),
            role: Role::Transform,
        },
    )
    .expect("register");

    // 裁剪断言：C1/C3/C5 应为 nil（表上无此键）；http 卡仅 feature 开时断言
    //（关闭时 http::register 不编译，capability 卡的 feature 字段即门控事实源）。
    let mut absent = vec!["request", "transport", "send_command"];
    if cfg!(feature = "lua-http") {
        absent.extend(["http_get", "http_post"]);
    }
    for a in absent {
        let v: mlua::Value = lua.load(format!("return ctx.{a}")).eval().expect(a);
        assert_eq!(v, mlua::Value::Nil, "transform ctx 应无 {a}（§5.2 矩阵 ✗）");
    }
    // 保留断言：C0/C2/C4/C6（§5.2 矩阵 transform 列 ✓）。
    for present in [
        "set_value",
        "get_value",
        "get_device",
        "list_devices",
        "kv_set",
        "kv_get",
        "secret_get",
        "log_info",
        "now",
    ] {
        let is_fn: bool = lua
            .load(format!("return type(ctx.{present}) == 'function'"))
            .eval()
            .expect(present);
        assert!(is_fn, "transform ctx 应有 {present}（§5.2 矩阵 ✓）");
    }

    let _ = crate::db::shutdown_lux(handle).await;
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// T5 恒注入（Q4）：无 secrets 配置（键缺失 / 类型不符按空清单降级）的实例
/// secret_get 仍注册可调，任何引用 → 未授权错误（默认空清单语义；未引用零成本）。
#[tokio::test]
async fn secret_get_always_registered_with_default_empty_allowlist() {
    for (tag, secrets_cfg) in [
        ("miss", None),
        ("badtype", Some(serde_json::json!("not-an-array"))),
    ] {
        let lua = Lua::new();
        let io = spawn_mock_io();
        let (ctx_host, handle, data_dir) = setup_ctx(&format!("t5-sec-{tag}")).await;
        let mut app_config = serde_json::json!({"script":"a","endpoint":"x"});
        if let Some(v) = secrets_cfg {
            app_config["secrets"] = v;
        }
        let inst = InstConfig {
            inst_id: "it".into(),
            inst_name: "it".into(),
            app_code: PluginName::new("luaClient"),
            app_type: AppType::South,
            auto_start: false,
            app_config,
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
            &ApiCtx {
                ctx_host,
                inst: &inst,
                io: Some(io),
                role: Role::default(),
                tx: mock_hub_tx(),
                runtime_devs: RuntimeDevs::default(),
                dropped: zero_dropped(),
                self_writes: None,
                alarm: AlarmStats::default(),
                transport: "tcp",
                scripts_root: Path::new("scripts"),
            },
        )
        .expect("register");

        let is_fn: bool = lua
            .load("return type(ctx.secret_get) == 'function'")
            .eval()
            .expect("type check");
        assert!(
            is_fn,
            "未配置 secrets 的实例也应注册 secret_get（恒注入，场景 {tag}）"
        );

        let e = lua
            .load("return ctx:secret_get('secret:test_key')")
            .eval_async::<String>()
            .await
            .expect_err("空清单任何引用应报未授权");
        assert!(
            e.to_string().contains("未授权"),
            "空清单语义 = 未授权错误（场景 {tag}）：{e}"
        );

        let _ = crate::db::shutdown_lux(handle).await;
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}

// ============ T4 告警（Q12，spec 2026-08-21-alarm §2 ctx API / §3 护栏）============

/// T4 告警测试共用：lux + **真实 EventBus**（16 容量，subscribe 断言告警帧）+
/// 按角色 register（io/transport/app_code 照既有角色测试）。返回
/// (lua, 订阅端, ctx_host, alarm, handle, data_dir)——alarm 由调用方传入
///（频控测注入小容量；None → 缺省 20/s）。
async fn alarm_vm(
    tag: &str,
    role: Role,
    alarm: Option<AlarmStats>,
) -> (
    Lua,
    tokio::sync::broadcast::Receiver<crate::event_bus::EventPayload>,
    HostContext,
    AlarmStats,
    lux::ServerHandle,
    String,
) {
    let data_dir = std::env::temp_dir()
        .join(format!("lua-{tag}-{}", std::process::id()))
        .to_str()
        .unwrap()
        .to_string();
    // 复审 P3-4：起跑先整删（pid 回收复用时残留数据带出旧告警/计数，精确断言
    // 假红——routes_secret 加固同款；末尾 remove 受 Windows 句柄延迟影响，双保险）。
    let _ = std::fs::remove_dir_all(&data_dir);
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
        event_tx: crate::event_bus::EventBus::new(16),
    };
    // 订阅先于任何 fire/clear（broadcast 收不到订阅前事件）。
    let rx = ctx_host.event_tx.subscribe();
    let (app_code, app_type, app_config) = match role {
        Role::Collector => (
            "luaClient",
            AppType::South,
            serde_json::json!({"script":"a","endpoint":"x"}),
        ),
        Role::North => (
            "luaNorth",
            AppType::North,
            serde_json::json!({"script":"a","host":"127.0.0.1"}),
        ),
        Role::Transform => (
            "luaTransform",
            AppType::South, // Source 订阅型实例（Q19/D1），不走 Processor
            serde_json::json!({"script":"a"}),
        ),
    };
    let inst = InstConfig {
        inst_id: "it".into(),
        inst_name: "it".into(),
        app_code: PluginName::new(app_code),
        app_type,
        auto_start: false,
        app_config,
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "d".into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    };
    let alarm = alarm.unwrap_or_default();
    let (io, transport) = match role {
        Role::Collector => (Some(spawn_mock_io()), "tcp"),
        Role::North => (None, "mqtt"),
        Role::Transform => (None, "none"),
    };
    let lua = Lua::new();
    register(
        &lua,
        &ApiCtx {
            ctx_host: ctx_host.clone(),
            inst: &inst,
            io,
            tx: mock_hub_tx(),
            runtime_devs: RuntimeDevs::default(),
            dropped: zero_dropped(),
            self_writes: None,
            transport,
            scripts_root: Path::new("scripts"),
            role,
            alarm: alarm.clone(),
        },
    )
    .expect("register");
    (lua, rx, ctx_host, alarm, handle, data_dir)
}

/// T4 注入矩阵（spec §2/§5.2 C6 行 ✓✓✓✓）：三角色（Collector/North/Transform）
/// 全部注册 fire_event/clear_event——无角色门（判定场景可能在任一角色：采集判
/// 原始点 / transform 判派生点 / north 判平台回执）。
#[tokio::test]
async fn alarm_api_injected_for_all_roles() {
    for role in [Role::Collector, Role::North, Role::Transform] {
        let (lua, _rx, _host, _alarm, handle, data_dir) = alarm_vm("alarm-roles", role, None).await;
        for api in ["fire_event", "clear_event"] {
            let is_fn: bool = lua
                .load(format!("return type(ctx.{api}) == 'function'"))
                .eval()
                .expect(api);
            assert!(is_fn, "{role:?} 角色应有 ctx.{api}（C6 跨角色全注入）");
        }
        let _ = crate::db::shutdown_lux(handle).await;
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}

/// spec §2 入口校验：key 非空 ≤128 字符 / level ∈ {info,warn,error} / message
/// ≤512 字符——违例返 `nil, err`（可感知，与 R2 频控同错误形态），且不落库。
#[tokio::test]
async fn fire_event_validates_args() {
    let (lua, _rx, ctx_host, _alarm, handle, data_dir) =
        alarm_vm("alarm-valid", Role::Collector, None).await;
    // 每条违例：首个返回值 nil（非 false——可感知错误而非限流）+ 错误消息非空。
    let cases: Vec<String> = vec![
        "ctx:fire_event('', 'warn', 'm')".to_string(), // key 空
        format!("ctx:fire_event('{}', 'warn', 'm')", "k".repeat(129)), // key 129 字符
        "ctx:fire_event('dev1.x', 'critical', 'm')".to_string(), // level 非三档
        format!("ctx:fire_event('dev1.x', 'warn', '{}')", "m".repeat(513)), // message 513
        "ctx:clear_event('')".to_string(),             // clear 同 key 规则
    ];
    for src in &cases {
        let perceptible: bool = lua
            .load(format!(
                "local ok, err = {src}; return ok == nil and err ~= nil and err ~= ''"
            ))
            .eval_async()
            .await
            .unwrap_or_else(|e| panic!("{src} 应可调用：{e}"));
        assert!(perceptible, "违例应返 nil, err（可感知）：{src}");
    }
    // 校验失败不落库（清单空）。
    let items = crate::db::alarms::list(&ctx_host.lux).await.unwrap();
    assert!(items.is_empty(), "校验拒绝的调用不应落库");

    let _ = crate::db::shutdown_lux(handle).await;
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// spec §3 每实例频控：token bucket 超限返 false（非错误——风暴不是脚本 bug，
/// 静默退避语义）+ `alarm_rate_limited` 计数；超限调用不落库（清单不被刷爆）。
/// 注入小容量（2/s）测第 3 次（生产 20/s 同逻辑，限值查能力卡）。
#[tokio::test]
async fn fire_event_rate_limited_returns_false() {
    let (lua, _rx, ctx_host, alarm, handle, data_dir) = alarm_vm(
        "alarm-rate",
        Role::Collector,
        Some(std::sync::Arc::new(super::alarm::AlarmState::new(2))),
    )
    .await;
    for k in ["k0", "k1"] {
        let ok: bool = lua
            .load(format!("return ctx:fire_event('{k}', 'warn', 'm')"))
            .eval_async()
            .await
            .unwrap_or_else(|e| panic!("{k} 应可调用：{e}"));
        assert!(ok, "{k} 窗口额度内应 true");
    }
    // 第 3 次（超 2/s 额度）：false 且非错误形态（ok == false，err == nil）。
    let limited: bool = lua
        .load(
            "local ok, err = ctx:fire_event('k2', 'warn', 'm'); return ok == false and err == nil",
        )
        .eval_async()
        .await
        .expect("第 3 次调用");
    assert!(limited, "频控超限应返 false（非 nil,err）");
    assert_eq!(
        alarm.rate_limited_count(),
        1,
        "alarm_rate_limited 计数应 +1"
    );
    let items = crate::db::alarms::list(&ctx_host.lux).await.unwrap();
    assert_eq!(items.len(), 2, "超限调用不落库（清单不被刷爆）");
    assert!(
        items.iter().all(|i| i.key != "k2"),
        "被限流的 k2 不应出现在清单"
    );

    let _ = crate::db::shutdown_lux(handle).await;
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// 复审 P1-2：clear 与 fire 同频控（同 20/s 桶，注入 2/s 测）——超限返 false
/// 不 HDEL 不发帧（此前 clear 无护栏，脚本循环 clear 可刷 SSE/日志/lux 无上限）。
#[tokio::test]
async fn clear_event_rate_limited_returns_false() {
    let (lua, _rx, ctx_host, alarm, handle, data_dir) = alarm_vm(
        "alarm-clear-rate",
        Role::Collector,
        Some(std::sync::Arc::new(super::alarm::AlarmState::new(2))),
    )
    .await;
    // 占满额度：fire + clear（同桶）各一次。
    let ok: bool = lua
        .load("return ctx:fire_event('k0', 'warn', 'm')")
        .eval_async()
        .await
        .unwrap();
    assert!(ok);
    let ok: bool = lua
        .load("return ctx:clear_event('k0')")
        .eval_async()
        .await
        .unwrap();
    assert!(ok, "额度内 clear 应 true（k0 已被清除）");
    // 第 3 次（超 2/s）：clear 一个不存在的键也被限流返 false（护栏先于幂等语义）。
    let limited: bool = lua
        .load("local ok, err = ctx:clear_event('k9'); return ok == false and err == nil")
        .eval_async()
        .await
        .expect("第 3 次调用");
    assert!(limited, "clear 超限应返 false（非 nil,err）");
    assert_eq!(
        alarm.rate_limited_count(),
        1,
        "clear 超限计入共用 rate_limited"
    );
    let items = crate::db::alarms::list(&ctx_host.lux).await.unwrap();
    assert!(items.is_empty(), "限流 clear 不应有任何清单副作用");

    let _ = crate::db::shutdown_lux(handle).await;
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// spec §3 同 key 抖动：1s 窗口内重复 fire 仍计数（lux count=2——风暴可见性）
/// 但只发一次 Alarm Fire 帧（前端不被同键闪屏）；帧 source 是 inst_id（宿主填，
/// 脚本不可伪造——API 无 source 参数）。
#[tokio::test]
async fn fire_event_dedupes_same_key_sse() {
    let (lua, mut rx, ctx_host, _alarm, handle, data_dir) =
        alarm_vm("alarm-dedupe", Role::Collector, None).await;
    // 复审 P3-4：两次 fire 合入同一 chunk——间隔缩到微秒级（此前两次 eval_async
    // 间隔满载可超 1s 抖动窗，第二次正常发帧致 second.is_err() 假红）。
    let oks: Vec<bool> = lua
        .load(
            "return { ctx:fire_event('dev1.temp_high', 'warn', '温度过高'), ctx:fire_event('dev1.temp_high', 'warn', '温度过高') }",
        )
        .eval_async()
        .await
        .expect("fire 应可调用");
    assert_eq!(oks, vec![true, true], "额度内两次 fire 都应 true");
    // lux count 累计 2（抖动只吞 SSE 不吞计数）。
    let item = crate::db::alarms::get(&ctx_host.lux, "dev1.temp_high")
        .await
        .unwrap()
        .expect("条目应存在");
    assert_eq!(item.count, 2, "同 key 重复 fire count 应累计");
    // SSE 只 1 帧 Fire（第二次被 1s 抖动窗口吞）。
    let f1 = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("超时")
        .expect("应收到首帧");
    match f1 {
        crate::event_bus::EventPayload::Alarm {
            action,
            key,
            level,
            source,
            message,
            ..
        } => {
            assert!(
                matches!(action, crate::event_bus::AlarmAction::Fire),
                "应为 Fire 帧"
            );
            assert_eq!(key, "dev1.temp_high");
            assert_eq!(level, "warn");
            assert_eq!(message, "温度过高");
            assert_eq!(source, "it", "source 由宿主填 inst_id（脚本不可伪造）");
        }
        other => panic!("应收到 Alarm 帧，实得 {other:?}"),
    }
    let second = tokio::time::timeout(std::time::Duration::from_millis(150), rx.recv()).await;
    assert!(second.is_err(), "1s 抖动窗口内第二次 fire 不应再发 SSE");

    let _ = crate::db::shutdown_lux(handle).await;
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// spec §2 clear 幂等恒发：无条目也发 Clear 帧（level/message 空串）；有条目
/// 沿用条目 level/message；clear 顺带清抖动表——同 key 立即重新 fire 是新事件
///（再发 Fire 帧，非抖动吞掉）。
#[tokio::test]
async fn clear_event_idempotent_always_publishes() {
    let (lua, mut rx, ctx_host, _alarm, handle, data_dir) =
        alarm_vm("alarm-clear", Role::Collector, None).await;

    // 1) 无条目 clear → true + Clear 帧（level/message 空串——多客户端视图收敛）。
    let ok: bool = lua
        .load("return ctx:clear_event('ghost')")
        .eval_async()
        .await
        .expect("clear ghost");
    assert!(ok, "clear 幂等：无条目也返回 true");
    let f = recv_alarm(&mut rx).await;
    assert!(
        matches!(f.action, crate::event_bus::AlarmAction::Clear),
        "clear 应恒发 Clear 帧"
    );
    assert_eq!(f.key, "ghost");
    assert_eq!(f.level, "", "无条目时 level 空串");
    assert_eq!(f.message, "", "无条目时 message 空串");
    assert_eq!(f.source, "it");

    // 2) 有条目 clear → level/message 沿用条目值 + lux 删除。
    fire_and_recv(&lua, &mut rx).await;
    let ok: bool = lua
        .load("return ctx:clear_event('dev1.x')")
        .eval_async()
        .await
        .expect("clear dev1.x");
    assert!(ok);
    let f = recv_alarm(&mut rx).await;
    assert!(matches!(f.action, crate::event_bus::AlarmAction::Clear));
    assert_eq!(f.key, "dev1.x");
    assert_eq!(f.level, "error", "clear 沿用清除时条目的 level");
    assert_eq!(f.message, "急停按下", "clear 沿用条目的 message");
    assert!(
        crate::db::alarms::get(&ctx_host.lux, "dev1.x")
            .await
            .unwrap()
            .is_none(),
        "clear 后条目应删除"
    );

    // 3) clear 后立即再 fire 同 key = 新事件（抖动表已清，正常发 Fire）。
    let ok: bool = lua
        .load("return ctx:fire_event('dev1.x', 'warn', '再次触发')")
        .eval_async()
        .await
        .expect("再次 fire");
    assert!(ok);
    let f = recv_alarm(&mut rx).await;
    assert!(
        matches!(f.action, crate::event_bus::AlarmAction::Fire),
        "clear 清抖动表后重新 fire 应发新 Fire 帧"
    );

    // 4) 幂等：再 clear 已清除的 key 仍 true + 仍发 Clear 帧。
    let ok: bool = lua
        .load("return ctx:clear_event('dev1.x')")
        .eval_async()
        .await
        .expect("重复 clear");
    assert!(ok, "clear 幂等：重复清除仍 true");
    let f = recv_alarm(&mut rx).await;
    assert!(matches!(f.action, crate::event_bus::AlarmAction::Clear));

    let _ = crate::db::shutdown_lux(handle).await;
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// 告警帧分离形态（recv_alarm 返回，测试断言用）。
struct AlarmFrame {
    action: crate::event_bus::AlarmAction,
    key: String,
    level: String,
    source: String,
    message: String,
    #[allow(dead_code)]
    ts: String,
}

/// 告警测试辅助：收一帧 Alarm（非 Alarm 帧或超时即 panic，分离字段供断言）。
async fn recv_alarm(
    rx: &mut tokio::sync::broadcast::Receiver<crate::event_bus::EventPayload>,
) -> AlarmFrame {
    let f = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("超时（应收告警帧）")
        .expect("通道不应关闭");
    match f {
        crate::event_bus::EventPayload::Alarm {
            action,
            key,
            level,
            source,
            message,
            ts,
        } => AlarmFrame {
            action,
            key,
            level,
            source,
            message,
            ts,
        },
        other => panic!("应收到 Alarm 帧，实得 {other:?}"),
    }
}

/// fire 一条（dev1.x/error）并收掉对应 Fire 帧（clear 场景的前置）。
async fn fire_and_recv(
    lua: &Lua,
    rx: &mut tokio::sync::broadcast::Receiver<crate::event_bus::EventPayload>,
) {
    let ok: bool = lua
        .load("return ctx:fire_event('dev1.x', 'error', '急停按下')")
        .eval_async()
        .await
        .expect("fire dev1.x");
    assert!(ok);
    let f = recv_alarm(rx).await;
    assert!(
        matches!(f.action, crate::event_bus::AlarmAction::Fire),
        "前置 fire 应发 Fire 帧"
    );
}
