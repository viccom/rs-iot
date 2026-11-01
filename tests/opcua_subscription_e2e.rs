//! opcuaClient OPC-9（Subscription）端到端：进程内真实 OPC UA server 联调。
//!
//! server = async-opcua-server 0.19（`ServerBuilder::new_anonymous` + 内存节点空间
//! `SimpleNodeManager`，自定义命名空间挂 v1..v4 变量节点）；client = opcuaClient
//! 实例（`acquisition_mode=subscription`）经真实 UA TCP 二进制协议交互。
//!
//! 验证：
//! ① 订阅建立后**首值**经 publish 通知入库（不再靠周期 Read）；
//! ② server 侧改写变量值 → 订阅通知 **2s 内**到达 hub（变化推送语义）；
//! ③ 写命令（`{"tag":..,"value":..}`）回执：成功经 `CtlRegistry::finish_write`
//!    回流 oneshot，且写入后订阅收到新值（写→notify 闭环）。
//!
//! 门控：`opcua-e2e`（dev-only）+ `opcua`（插件本体）；feature 说明见 Cargo.toml。
#![cfg(all(feature = "opcua", feature = "opcua-e2e"))]

mod common;

use opcua::types::{DataTypeId, DataValue, NodeId, Variant};
use opcua_server::address_space::{Variable, VariableBuilder};
use opcua_server::diagnostics::NamespaceMetadata;
use opcua_server::node_manager::memory::{simple_node_manager, SimpleNodeManager};
use opcua_server::{ServerBuilder, ServerHandle};
use rs_iot::host::{BridgeRegistry, CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::types::{
    AppType, ChildSpec, CtlMessage, DeviceSpec, HostKind, Hwid, InstConfig, PluginName, Sample,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const NS_URI: &str = "urn:rs-iot:opcua-e2e";

// ── 进程内 UA server ────────────────────────────────────────────────────

struct TestServer {
    addr: std::net::SocketAddr,
    mgr: Arc<SimpleNodeManager>,
    handle: ServerHandle,
    ns: u16,
}

fn var(ns: u16, name: &str, value: Variant, dt: DataTypeId, writable: bool) -> Variable {
    let b = VariableBuilder::new(&NodeId::new(ns, name), name, name)
        .value(value)
        .data_type(dt);
    if writable {
        b.writable().build()
    } else {
        b.build()
    }
}

/// 起 server（端口 0 = OS 分配；PKI 目录隔离到临时目录防并行互踩）。
async fn spawn_server(inst: &str) -> TestServer {
    let pki = common::test_data_dir(&format!("opcua-e2e-server-{inst}"));
    let (server, handle) = ServerBuilder::new_anonymous("rs-iot opcua-e2e")
        .pki_dir(pki)
        .with_node_manager(simple_node_manager(
            NamespaceMetadata {
                namespace_uri: NS_URI.to_string(),
                ..Default::default()
            },
            "e2e-vars",
        ))
        .build()
        .expect("build opcua server");
    let mgr = handle
        .node_managers()
        .get_by_name::<SimpleNodeManager>("e2e-vars")
        .expect("simple node manager registered");
    let ns = handle
        .get_namespace_index(NS_URI)
        .expect("namespace registered");
    // v1/v4 可写（v4 是写命令目标）；v3 用 u64::MAX 验证 F-5 UInt 车道端到端
    let vars = vec![
        var(ns, "v1", Variant::Double(1.5), DataTypeId::Double, true),
        var(
            ns,
            "v2",
            Variant::String("hello-sub".into()),
            DataTypeId::String,
            false,
        ),
        var(
            ns,
            "v3",
            Variant::UInt64(u64::MAX),
            DataTypeId::UInt64,
            false,
        ),
        var(ns, "v4", Variant::Double(0.0), DataTypeId::Double, true),
    ];
    {
        let mut space = mgr.address_space().write();
        let ok = space.add_variables(vars, &NodeId::objects_folder_id());
        assert!(ok.iter().all(|&b| b), "变量节点全部插入成功: {ok:?}");
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind server port 0");
    let addr = listener.local_addr().expect("server addr");
    tokio::spawn(server.run_with(listener));
    TestServer {
        addr,
        mgr,
        handle,
        ns,
    }
}

/// server 侧改写变量值并通知订阅（`set_value` 内部走 `maybe_notify`）。
fn server_set_value(srv: &TestServer, name: &str, value: Variant) {
    srv.mgr
        .set_value(
            srv.handle.subscriptions(),
            &NodeId::new(srv.ns, name),
            None,
            DataValue::new_now(value),
        )
        .expect("server set_value");
}

// ── opcuaClient 实例（Harness 提炼自 eip_client_e2e，同款模型装配） ──────

struct Harness {
    mgr: Arc<InstanceManager>,
    hub_rx: tokio::sync::mpsc::Receiver<Sample>,
    shutdown: CancellationToken,
    lux: lux::ServerHandle,
    ctl: Arc<CtlRegistry>,
}

async fn start_instance(
    inst_id: &str,
    app: serde_json::Value,
    devices: Vec<DeviceSpec>,
) -> Harness {
    let data_dir = common::test_data_dir(&format!("opcua-sub-{inst_id}"));
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");
    let bindings =
        common::register_devices_as_models(&client, inst_id, "opcuaClient", devices).await;

    let shutdown = CancellationToken::new();
    let ctl = Arc::new(CtlRegistry::new(Arc::new(
        rs_iot::process_metrics::ProcessMetrics::new(),
    )));
    let ctx = HostContext {
        lux: client,
        hwid: Hwid::new("it-hwid"),
        shutdown: shutdown.clone(),
        ctl_registry: (*ctl).clone(),
        bridge_registry: BridgeRegistry::new(),
        metrics: Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        event_tx: rs_iot::event_bus::EventBus::dummy(),
        history: None,
    };
    let (hub_tx, hub_rx) = tokio::sync::mpsc::channel::<Sample>(64);
    let mgr = Arc::new(InstanceManager::new(ctx, hub_tx));
    let inst = InstConfig {
        inst_id: inst_id.into(),
        inst_name: "it opcua sub".into(),
        app_code: PluginName::new("opcuaClient"),
        app_type: AppType::South,
        auto_start: false,
        app_config: app,
        devices: bindings,
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start opcuaClient");
    Harness {
        mgr,
        hub_rx,
        shutdown,
        lux: handle,
        ctl,
    }
}

fn mk_child(field: &str, node: &str, ty: &str) -> ChildSpec {
    ChildSpec {
        label: field.into(),
        value: node.into(),
        r#type: ty.into(),
        props: None,
    }
}

/// 收 Sample 到 `want` 全出现或 deadline。
async fn collect_until(
    hub_rx: &mut tokio::sync::mpsc::Receiver<Sample>,
    want: &HashMap<String, String>,
    deadline_secs: u64,
) -> HashMap<String, String> {
    let mut got: HashMap<String, String> = HashMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(deadline_secs);
    loop {
        if want.iter().all(|(k, v)| got.get(k) == Some(v)) {
            break;
        }
        match tokio::time::timeout_at(deadline, hub_rx.recv()).await {
            Ok(Some(s)) => {
                for (k, v) in &s.values {
                    got.insert(format!("{}.{}", s.devid, k), format!("{v:?}"));
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    got
}

/// 等单点出现指定值（变化推送的 2s 判据用）。
async fn wait_value(
    hub_rx: &mut tokio::sync::mpsc::Receiver<Sample>,
    devid: &str,
    field: &str,
    expect: &str,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    let key = format!("{devid}.{field}");
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, hub_rx.recv()).await {
            Ok(Some(s)) => {
                for (k, v) in &s.values {
                    if format!("{devid}.{k}") == key && format!("{v:?}") == expect {
                        return true;
                    }
                }
            }
            Ok(None) | Err(_) => return false,
        }
    }
    false
}

/// 订阅配置：200ms 发布、看门狗 10s、请求超时 5s（连接/建订阅各环节预算）。
fn sub_app(endpoint: &str) -> serde_json::Value {
    serde_json::json!({
        "endpoint": endpoint,
        "acquisition_mode": "subscription",
        "period": 1,
        "publishing_interval_ms": 200,
        "subscription_watchdog_secs": 10,
        "request_timeout_ms": 5000,
    })
}

// ── 用例 ────────────────────────────────────────────────────────────────

/// ①+② 订阅首值入库 + server 侧变化 2s 内推送（无周期 Read 参与）。
#[tokio::test]
async fn opcua_subscription_delivers_initial_and_changed_values() {
    let srv = spawn_server("sub1").await;
    let endpoint = format!("opc.tcp://{}", srv.addr);
    let devices = vec![DeviceSpec {
        label: "plc".into(),
        value: "plc".into(),
        children: vec![
            mk_child("temp", &format!("ns={};s=v1", srv.ns), "f64"),
            mk_child("msg", &format!("ns={};s=v2", srv.ns), "string"),
            mk_child("big", &format!("ns={};s=v3", srv.ns), "u64"),
        ],
        slave_id: None,
    }];
    let mut h = start_instance("opcsub1", sub_app(&endpoint), devices).await;

    // 首值：订阅建立后的第一次 publish 携带全部 monitored item 当前值
    let want: HashMap<String, String> = HashMap::from([
        ("plc.temp".to_string(), "Float(1.5)".to_string()),
        ("plc.msg".to_string(), "Str(\"hello-sub\")".to_string()),
        // F-5：UInt64 不走 i64 符号翻转，u64::MAX 端到端保真
        ("plc.big".to_string(), format!("UInt({})", u64::MAX)),
    ]);
    let got = collect_until(&mut h.hub_rx, &want, 15).await;
    for (k, v) in &want {
        assert_eq!(got.get(k), Some(v), "订阅首值未按预期入库: got {got:?}");
    }

    // server 侧改写 → 订阅通知 2s 内到达 hub（复用同一连接，无需重建）
    server_set_value(&srv, "v1", Variant::Double(42.5));
    let in_2s = wait_value(
        &mut h.hub_rx,
        "plc",
        "temp",
        "Float(42.5)",
        Duration::from_secs(2),
    )
    .await;
    h.shutdown.cancel();
    h.mgr.stop_all().await;
    common::stop_lux(h.lux).await;
    srv.handle.cancel();
    assert!(
        in_2s,
        "server 侧变化后 2s 内未收到订阅推送（期望 Float(42.5)）"
    );
}

/// ③ 写命令回执（F-2 通道在订阅模式下同样生效）+ 写入值经订阅回推。
#[tokio::test]
async fn opcua_subscription_write_command_receipt() {
    let srv = spawn_server("sub2").await;
    let endpoint = format!("opc.tcp://{}", srv.addr);
    let devices = vec![DeviceSpec {
        label: "plc".into(),
        value: "plc".into(),
        children: vec![
            mk_child("temp", &format!("ns={};s=v1", srv.ns), "f64"),
            mk_child("sp", &format!("ns={};s=v4", srv.ns), "f64"),
        ],
        slave_id: None,
    }];
    let mut h = start_instance("opcsub2", sub_app(&endpoint), devices).await;

    // 等首值（确认订阅已建立再发写命令）
    let want0: HashMap<String, String> = HashMap::from([
        ("plc.temp".to_string(), "Float(1.5)".to_string()),
        ("plc.sp".to_string(), "Float(0.0)".to_string()),
    ]);
    let got0 = collect_until(&mut h.hub_rx, &want0, 15).await;
    assert_eq!(
        got0.get("plc.sp"),
        Some(&"Float(0.0)".to_string()),
        "首值未到，订阅未建立？got {got0:?}"
    );

    // 写命令 + 回执（REST 写入口同款 oneshot 通道）
    let ack_rx = h.ctl.write_acks().begin("req-sub-e2e-1");
    let ok = h.mgr.send_ctl(
        "opcsub2",
        CtlMessage {
            instance: "opcsub2".into(),
            devid: "plc".into(),
            cmd: "write".into(),
            data: r#"{"tag":"sp","value":7.5}"#.into(),
            request_id: Some("req-sub-e2e-1".into()),
        },
    );
    assert!(ok, "send_ctl 应投递成功（实例运行中）");
    let ack = tokio::time::timeout(Duration::from_secs(12), ack_rx)
        .await
        .expect("12s 内应收到写回执")
        .expect("回执通道不应关闭");
    assert!(ack.is_ok(), "写回执应为成功: {ack:?}");

    // 写入成功后 server 通知订阅者 → hub 收到 sp=7.5（写→notify 闭环）
    let notified = wait_value(
        &mut h.hub_rx,
        "plc",
        "sp",
        "Float(7.5)",
        Duration::from_secs(5),
    )
    .await;
    h.shutdown.cancel();
    h.mgr.stop_all().await;
    common::stop_lux(h.lux).await;
    srv.handle.cancel();
    assert!(notified, "写入后订阅未回推新值 sp=7.5");
}
