//! api 模块测试（register + ctx API）。Plan 8 Task 1 拆分自原 api.rs。
//!
//! 覆盖：register 成功 + ctx 暴露 request/log/now/time（冒烟）；kv_set/get round-trip；
//! send_command 经 CtlRegistry 路由；http_get 返回 {status, body}（feature lua-http）。

use super::*;
use crate::host::{BridgeRegistry, CtlRegistry, HostContext};
use crate::plugin::builtin::lua_client::io_engine::{spawn_io_task, IoEngine, MockIoTask};
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
    )
}

/// 冒烟测：register 成功 + ctx 暴露 request/log_info/now/time（且都是 function）。
#[tokio::test]
async fn register_exposes_ctx_api() {
    let lua = Lua::new();
    let io = spawn_mock_io();
    // 起一个测试 lux 拿 EmbeddedClient（HostContext 需要；端口 0 由 OS 分配，独立
    // 临时 data_dir 防并行冲突——同 tests/common 模式）。
    let data_dir = std::env::temp_dir()
        .join(format!("lua-api-{}", std::process::id()))
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
    let lux_cfg = crate::db::build_lux_config(&cfg);
    let handle = lux::run_with_config(lux_cfg).await.expect("lux");
    let ctx_host = HostContext {
        lux: handle.client(),
        hwid: crate::types::Hwid::new("it"),
        shutdown: CancellationToken::new(),
        ctl_registry: CtlRegistry::new(),
        bridge_registry: BridgeRegistry::new(),
    };
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
        }],
        host_kind: HostKind::Builtin,
    };
    register(&lua, ctx_host, &inst, io).expect("register");
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
    let data_dir = std::env::temp_dir()
        .join(format!("lua-kv-{}", std::process::id()))
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
        ctl_registry: CtlRegistry::new(),
        bridge_registry: BridgeRegistry::new(),
    };
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
        }],
        host_kind: HostKind::Builtin,
    };
    register(&lua, ctx_host, &inst, io).expect("register");

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
    let ctl = CtlRegistry::new();
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
        }],
        host_kind: HostKind::Builtin,
    };
    register(&lua, ctx_host, &inst, io).expect("register");

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
    let data_dir = std::env::temp_dir()
        .join(format!("lua-http-{}", std::process::id()))
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
        ctl_registry: CtlRegistry::new(),
        bridge_registry: BridgeRegistry::new(),
    };
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
        }],
        host_kind: HostKind::Builtin,
    };
    register(&lua, ctx_host, &inst, io).expect("register");

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
