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
    register(&lua, ctx_host, &inst, io, mock_hub_tx(), "tcp").expect("register");
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
    register(&lua, ctx_host, &inst, io, mock_hub_tx(), "tcp").expect("register");

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
    register(&lua, ctx_host, &inst, io, mock_hub_tx(), "tcp").expect("register");

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
    register(&lua, ctx_host, &inst, io, mock_hub_tx(), "tcp").expect("register");

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
    register(&lua, ctx_host, &inst, io, tx, "tcp").expect("register");

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
    register(&lua, ctx_host, &inst, io, tx, "tcp").expect("register");

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
    register(&lua, ctx_host, &inst, io, tx, "tcp").expect("register");

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
    register(&lua, ctx_host, &inst, io, mock_hub_tx(), "tcp").expect("register");

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
    register(&lua, ctx_host, &inst, io, mock_hub_tx(), "tcp").expect("register");

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
    register(&lua, ctx_host, &inst, io, mock_hub_tx(), "tcp").expect("register");

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
        register(&lua, ctx_host, &inst, io, mock_hub_tx(), kind).expect("register");
        let got: String = lua
            .load("return ctx:transport()")
            .eval()
            .expect("ctx:transport eval");
        assert_eq!(got, expected, "ctx:transport() 应返回 register 传入的 kind");

        let _ = crate::db::shutdown_lux(handle).await;
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}
