//! MQTT 透传端到端（T11，Q10/spike-2 方案 a）：进程内 rumqttd broker + 测试侧
//! 「设备」（订 req_topic 回 resp_topic + 推被动帧）+ luaClient kind=mqtt 实例。
//!
//! 覆盖：
//! 1. [`mqtt_pass_request_and_on_data`]：`ctx:request` 闭环出数——collect 发 req →
//!    设备回 echo+0xAA → rtdata 落 v=170；on_data 被动帧——设备推 passive_topic
//!    （无在飞 request 时）→ on_data kv_set 落 43。
//! 2. [`mqtt_dead_broker_errors_not_panics`]：broker 不可达（连接必失败端口）——
//!    collect 的 ctx:request 每轮 Err（连接错误经 pump poll 快速冒出），脚本容错
//!    计数不 panic，实例存活（降级断言：报错不 panic 即可）。
//!
//! 「断 broker→恢复自愈」在进程内 rumqttd 上无法确定性编排：0.20 的 `Broker::start`
//! 无停机口（T10 已证），MqttIo 的 client_id 进程内生成（`mqtt-io-<pid>-<seq>`）
//! 无法被测试 takeover 抢占；晚起 broker 则撞「实例首发与设备订阅」竞态（首发丢给
//! 无订阅者 topic → 该轮 backend 等满 30s 兜底，独占 IO task）——自愈语义留真机
//! 验证（pump 重连循环 + ConnAck 重订阅路径与单测 mqtt_dead_broker_errors_fast
//! 的「poll 错误后 pump 存活持续重连」断言共同覆盖其机制面）。
//!
//! broker helper 为 T10 `lua_north/contract.rs` spawn_inproc_broker 的复制
//! （tests/ 无法复用 src 内 cfg(test) helper；rumqttd 为 dev-dep 仅测试可用）。

mod common;

use common::test_data_dir;
use rs_iot::host::{BridgeRegistry, CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::plugin::traits::Processor;
use rs_iot::transform::passthrough::Passthrough;
use rs_iot::types::{AppType, DeviceSpec, HostKind, Hwid, InstConfig, PluginName, Sample};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const REQ_TOPIC: &str = "mqtt-e2e/req";
const RESP_TOPIC: &str = "mqtt-e2e/resp";
const PASSIVE_TOPIC: &str = "mqtt-e2e/passive";

/// 起进程内 rumqttd（明文 TCP v4，127.0.0.1 随机端口；T10 同款）。返回端口。
fn spawn_inproc_broker() -> u16 {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind :0 选空闲端口");
        l.local_addr().expect("local_addr").port()
    };
    // RouterConfig 需显式非零（derive Default 全 0——max_connections=0 会拒一切连接）
    let config = rumqttd::Config {
        router: rumqttd::RouterConfig {
            max_connections: 64,
            max_outgoing_packet_count: 200,
            max_segment_size: 10 * 1024 * 1024,
            max_segment_count: 10,
            custom_segment: None,
            initialized_filters: None,
            shared_subscriptions_strategy: Default::default(),
        },
        v4: Some(
            [(
                "test".to_string(),
                rumqttd::ServerSettings {
                    name: "mqtt-pass-e2e".to_string(),
                    listen: format!("127.0.0.1:{port}").parse().expect("SocketAddr"),
                    tls: None,
                    next_connection_delay_ms: 1,
                    connections: rumqttd::ConnectionSettings {
                        connection_timeout_ms: 60_000,
                        max_payload_size: 1024 * 1024,
                        max_inflight_count: 100,
                        auth: None, // 匿名放行（ConnectionSettings 无 Default derive）
                        external_auth: None,
                        dynamic_filters: false,
                    },
                },
            )]
            .into_iter()
            .collect(),
        ),
        ..Default::default()
    };
    let mut broker = rumqttd::Broker::new(config);
    std::thread::Builder::new()
        .name("rumqttd-mqtt-pass-e2e".to_string())
        .spawn(move || {
            if let Err(e) = broker.start() {
                eprintln!("[mqtt-pass-e2e] rumqttd start 失败：{e:?}");
            }
        })
        .expect("spawn rumqttd 线程");
    // 就绪探测：TCP 连通即 listener up（5s 上限）
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return port;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("rumqttd 5s 未就绪（port {port}）");
}

/// 测试侧「设备」：连 broker，每次 ConnAck（重）订 REQ_TOPIC，收 req 帧 → 回显 +
/// 0xAA 到 RESP_TOPIC；断连期持续重连（poll Err 退避 200ms）。返回 client 句柄
/// （测试侧推被动帧用，poll 由内部 task 常驻驱动）。
async fn spawn_device(port: u16) -> rumqttc::AsyncClient {
    let mut opts = rumqttc::MqttOptions::new("mqtt-pass-e2e-device", "127.0.0.1", port);
    opts.set_clean_session(true);
    let (client, mut eventloop) = rumqttc::AsyncClient::new(opts, 16);
    let poll_client = client.clone();
    tokio::spawn(async move {
        loop {
            match eventloop.poll().await {
                Ok(rumqttc::Event::Incoming(rumqttc::Packet::ConnAck(_))) => {
                    let _ = poll_client
                        .subscribe(REQ_TOPIC, rumqttc::QoS::AtLeastOnce)
                        .await;
                }
                Ok(rumqttc::Event::Incoming(rumqttc::Packet::Publish(p))) => {
                    let mut r = p.payload.to_vec();
                    r.push(0xAA); // collect 侧解析 byte(2)=0xAA=170
                    let _ = poll_client
                        .publish(RESP_TOPIC, rumqttc::QoS::AtLeastOnce, false, r)
                        .await;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
                _ => {}
            }
        }
    });
    client
}

/// 起全套 host 环境（lux + pipeline + InstanceManager），返回 (mgr, client, token)。
async fn start_host(
    data_dir: &str,
) -> (
    Arc<InstanceManager>,
    lux::EmbeddedClient,
    CancellationToken,
    lux::ServerHandle,
) {
    let (handle, client) = common::start_lux(data_dir).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");
    let shutdown = CancellationToken::new();
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown: shutdown.clone(),
        ctl_registry: CtlRegistry::new(std::sync::Arc::new(
            rs_iot::process_metrics::ProcessMetrics::new(),
        )),
        bridge_registry: BridgeRegistry::new(),
        metrics: std::sync::Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        event_tx: rs_iot::event_bus::EventBus::dummy(),
    };
    let (hub_tx, hub_rx) = tokio::sync::mpsc::channel::<Sample>(64);
    let mgr = Arc::new(InstanceManager::new(ctx.clone(), hub_tx));
    let proc: Arc<dyn Processor> = Arc::new(Passthrough);
    let pt = shutdown.child_token();
    let _pipeline = tokio::spawn(rs_iot::pipeline::run(ctx, hub_rx, proc, 1, pt));
    (mgr, client, shutdown, handle)
}

/// MQTT 透传主闭环：request 出数（v=170）+ on_data 被动帧（43）。
/// 时序：broker → 设备（连+订）先就绪，实例后起——规避首发与设备订阅竞态
/// （首发丢给无订阅者 topic 会占满 30s 兜底，见文件头注释）。
#[tokio::test]
async fn mqtt_pass_request_and_on_data() {
    let data_dir = test_data_dir("lua-mqtt-pass");
    let (mgr, client, shutdown, handle) = start_host(data_dir.to_str().expect("utf8")).await;

    let port = spawn_inproc_broker();
    let device = spawn_device(port).await;
    // 设备连接 + SubAck 落定（连接失败由内部重连兜底，300ms 容余）。
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 脚本：collect 发 "\x01" 取响应 byte(2)（设备回显+0xAA=170）；
    // on_data 把被动帧首字节（0x2B=43）kv_set。
    let scripts = std::env::temp_dir().join(format!("lua-mq-{}", std::process::id()));
    std::fs::create_dir_all(scripts.join("mq")).expect("mkdir");
    std::fs::write(
        scripts.join("mq/main.lua"),
        "function collect()\n\
         local r = ctx:request(\"\\x01\", 500)\n\
         if not r or #r < 2 then return nil end\n\
         return { v = r:byte(2) }\n\
         end\n\
         function on_data(frame)\n\
         ctx:kv_set('passive', frame:byte(1))\n\
         end\n",
    )
    .expect("write");

    let inst = InstConfig {
        inst_id: "lua-mq".into(),
        inst_name: "mq".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: true,
        app_config: json!({
            "script": "mq",
            "kind": "mqtt",
            "endpoint": format!("127.0.0.1:{port}"),
            "req_topic": REQ_TOPIC,
            "resp_topic": RESP_TOPIC,
            "passive_topics": [PASSIVE_TOPIC],
            "period": 1,
            "timeout_ms": 500,
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "devMQ".into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start lua");

    // request 闭环出数（10s 上限：容实例启动 + 首周期）。
    let ok = wait_cell_contains(&client, "rtdata:devMQ", "devMQ.v", "170", 10).await;
    assert!(ok, "collect 应经 request 闭环出 v=170（设备回显+0xAA）");

    // 被动帧 → on_data kv_set（推帧与 collect 周期可能撞在飞——分路把帧投给
    // request，故周期性重推直至 kv 命中，10s 上限）。
    let mut got = false;
    for _ in 0..20 {
        let _ = device
            .publish(PASSIVE_TOPIC, rumqttc::QoS::AtLeastOnce, false, &[0x2B])
            .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let s = hget_cell(&client, "luakv:lua-mq", "passive").await;
        if s.contains("43") {
            got = true;
            break;
        }
    }
    assert!(got, "被动帧 0x2B=43 应经 on_data kv_set passive");

    shutdown.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
    let _ = std::fs::remove_dir_all(&scripts);
}

/// broker 不可达：request 报错不 panic（降级断言——自愈真机验证，见文件头）。
/// 证据链：collect 的 request Err → 脚本 kv_set err 计数（脚本存活 + 回调持续
/// 被调 = 实例未 panic）；rtdata 恒空（请求从未闭环）。
#[tokio::test]
async fn mqtt_dead_broker_errors_not_panics() {
    let data_dir = test_data_dir("lua-mqtt-dead");
    let (mgr, client, shutdown, handle) = start_host(data_dir.to_str().expect("utf8")).await;

    // 脚本：request Err 时 kv 计数（容错路径），成功时返回值（恒不应发生）。
    let scripts = std::env::temp_dir().join(format!("lua-mqd-{}", std::process::id()));
    std::fs::create_dir_all(scripts.join("mqd")).expect("mkdir");
    std::fs::write(
        scripts.join("mqd/main.lua"),
        "function collect()\n\
         local r = ctx:request(\"\\x01\", 500)\n\
         if not r then\n\
         ctx:kv_set('err', (ctx:kv_get('err') or 0) + 1)\n\
         return nil\n\
         end\n\
         return { v = r:byte(2) }\n\
         end\n",
    )
    .expect("write");

    let inst = InstConfig {
        inst_id: "lua-mqd".into(),
        inst_name: "mqd".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: true,
        app_config: json!({
            "script": "mqd",
            "kind": "mqtt",
            // 端口 1 无监听（连接必失败；单测 mqtt_dead_broker_errors_fast 同款）
            "endpoint": "127.0.0.1:1",
            "req_topic": REQ_TOPIC,
            "resp_topic": RESP_TOPIC,
            "period": 1,
            "timeout_ms": 500,
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "devMQd".into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start lua");

    // 实例启动 + ≥1 轮 collect（每轮 request Err 后 kv 计数；并行负载下轮数不定，
    // 只断言 ≥1——证明 request 走了 Err 且脚本存活持续被调）。
    tokio::time::sleep(Duration::from_millis(4000)).await;

    let err_raw = hget_cell(&client, "luakv:lua-mqd", "err").await;
    let err_cnt: u64 = err_raw.trim_matches('"').parse().unwrap_or(0);
    assert!(
        err_cnt >= 1,
        "request 应报错且脚本存活计数（err='{err_raw}'；0=实例未跑或 request 挂死）"
    );
    let v = hget_cell(&client, "rtdata:devMQd", "devMQd.v").await;
    assert!(
        v.is_empty(),
        "broker 不可达时 collect 不应闭环出数（'{v}'）"
    );

    // 实例存活可正常停（panic 的任务/卡死 IO 会拖垮 stop_all）。
    shutdown.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
    let _ = std::fs::remove_dir_all(&scripts);
}

/// 轮询 HGET 直到 value 包含 expect（上限 secs 秒），返回是否命中。
async fn wait_cell_contains(
    client: &lux::EmbeddedClient,
    key: &str,
    field: &str,
    expect: &str,
    secs: u64,
) -> bool {
    for _ in 0..(secs * 4) {
        let v = hget_cell(client, key, field).await;
        if v.contains(expect) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

/// 取 lux Hash 单 field 的字符串值（HGET）。
async fn hget_cell(client: &lux::EmbeddedClient, key: &str, field: &str) -> String {
    use lux::EmbeddedValue;
    match client.execute_value("HGET", &[key, field]).await {
        Ok(EmbeddedValue::Bulk(b)) => String::from_utf8_lossy(&b).to_string(),
        Ok(EmbeddedValue::Simple(s)) => s,
        _ => String::new(),
    }
}
