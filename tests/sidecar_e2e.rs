//! sidecarClient bridge 端到端：起 lux + bridge acceptor + sidecarClient 实例，
//! 进程内模拟外部客户端（Tokio TcpStream）验证：
//! Hello/HelloAck/ConfigPush、DataUp→rtdata、写命令→Command→Ack、错误 token 拒绝。

mod common;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use rs_iot::db::ext_token;
use rs_iot::host::{BridgeRegistry, CtlRegistry, HostContext};
use rs_iot::plugin::builtin::sidecar_client;
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::plugin::traits::Processor;
use rs_iot::transform::passthrough::Passthrough;
use rs_iot::types::{
    AppType, CtlMessage, DeviceSpec, HostKind, InstConfig, InstId, PluginName, Sample,
};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tokio_util::sync::CancellationToken;

const T_HELLO: u8 = 1;
const T_HELLO_ACK: u8 = 2;
const T_CONFIG_PUSH: u8 = 3;
const T_DATA_UP: u8 = 4;
const T_COMMAND: u8 = 5;
const T_COMMAND_ACK: u8 = 6;

/// 测试用 hmac 密钥（任意长度，hmac 接受）。
const SECRET: &str = "it-sidecar-test-secret";

fn enc(typ: u8, body: &impl serde::Serialize) -> Bytes {
    let mut v = vec![typ];
    let _ = serde_json::to_writer(&mut v, body);
    Bytes::from(v)
}

/// 读一帧 → (type, json)。stream 结束或错误返回 None。
async fn read_frame<R>(reader: &mut R) -> Option<(u8, serde_json::Value)>
where
    R: futures_util::Stream<Item = Result<bytes::BytesMut, std::io::Error>> + Unpin,
{
    let bytes = reader.next().await?.ok()?;
    if bytes.is_empty() {
        return None;
    }
    let typ = bytes[0];
    let val: serde_json::Value =
        serde_json::from_slice(&bytes[1..]).unwrap_or(serde_json::Value::Null);
    Some((typ, val))
}

async fn hget_cell(client: &lux::EmbeddedClient, key: &str, field: &str) -> String {
    use lux::EmbeddedValue;
    match client.execute_value("HGET", &[key, field]).await {
        Ok(EmbeddedValue::Bulk(b)) => String::from_utf8_lossy(&b).to_string(),
        Ok(EmbeddedValue::Simple(s)) => s,
        _ => String::new(),
    }
}

struct Env {
    handle: lux::ServerHandle,
    client: lux::EmbeddedClient,
    shutdown: CancellationToken,
    mgr: Arc<InstanceManager>,
    port: u16,
    token: String,
}

/// 起全套：lux + identity + ctx + InstanceManager + pipeline + bridge acceptor + sidecarClient 实例。
async fn setup(inst_id: &str, hb_secs: u64) -> Env {
    let data_dir = common::test_data_dir(&format!("sidecar-{inst_id}"));
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    let shutdown = CancellationToken::new();
    let bridge_registry = BridgeRegistry::new();
    let ctx = HostContext {
        lux: client.clone(),
        hwid: rs_iot::types::Hwid::new("it-hwid"),
        shutdown: shutdown.clone(),
        ctl_registry: CtlRegistry::new(std::sync::Arc::new(
            rs_iot::process_metrics::ProcessMetrics::new(),
        )),
        bridge_registry: bridge_registry.clone(),
        metrics: std::sync::Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        event_tx: rs_iot::event_bus::EventBus::dummy(),
    };
    let (hub_tx, hub_rx) = mpsc::channel::<Sample>(64);
    let mgr = Arc::new(InstanceManager::new(ctx.clone(), hub_tx));
    let proc: Arc<dyn Processor> = Arc::new(Passthrough);
    let pt = shutdown.child_token();
    let _pipeline = tokio::spawn(rs_iot::pipeline::run(ctx, hub_rx, proc, 1, pt));

    // bridge acceptor（127.0.0.1:0，OS 分配端口）。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bridge bind");
    let port = listener.local_addr().expect("local_addr").port();
    let bt = shutdown.child_token();
    tokio::spawn(sidecar_client::acceptor(
        listener,
        Duration::from_secs(2),
        64,
        client.clone(),
        std::sync::Arc::<str>::from(SECRET),
        bridge_registry,
        bt,
    ));

    // M-2 模型驱动：devS 注册隐式模型+devreg（纯软），实例持绑定。
    let bindings = common::register_devices_as_models(
        &client,
        inst_id,
        "sidecarClient",
        vec![DeviceSpec {
            label: "d".into(),
            value: "devS".into(),
            children: vec![rs_iot::types::ChildSpec {
                label: "温度".into(),
                value: "temp".into(),
                r#type: "float".into(),
                props: None,
            }],
            slave_id: None,
        }],
    )
    .await;
    // 创建 sidecarClient External 实例 + 签发 token + start。
    let inst = InstConfig {
        inst_id: inst_id.into(),
        inst_name: "it sidecar".into(),
        app_code: PluginName::new("sidecarClient"),
        app_type: AppType::South,
        auto_start: false,
        // 测试心跳超时（hb_secs 参数化）：默认长超时避免误判，心跳超时用例传小值。
        app_config: json!({"heartbeat_timeout_secs": hb_secs}),
        devices: bindings,
        host_kind: HostKind::External,
    };
    let token = ext_token::issue(&client, SECRET, inst_id)
        .await
        .expect("issue token");
    mgr.start_source(inst).await.expect("start sidecar");
    // 等 sidecarClient task 注册 slot 到 bridge_registry。
    tokio::time::sleep(Duration::from_millis(200)).await;

    Env {
        handle,
        client,
        shutdown,
        mgr,
        port,
        token,
    }
}

async fn teardown(env: Env) {
    env.shutdown.cancel();
    env.mgr.stop_all().await;
    common::stop_lux(env.handle).await;
}

#[tokio::test]
async fn sidecar_dataup_and_command_roundtrip() {
    let env = setup("sc1", 60).await;

    // 模拟外部客户端：连接 + Hello。
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", env.port))
        .await
        .expect("connect");
    let (r, w) = tokio::io::split(stream);
    let mut reader = tokio_util::codec::FramedRead::new(r, LengthDelimitedCodec::new());
    let mut writer = tokio_util::codec::FramedWrite::new(w, LengthDelimitedCodec::new());

    writer
        .send(enc(
            T_HELLO,
            &json!({"inst_id":"sc1","token":env.token,"ver":"2"}),
        ))
        .await
        .expect("send hello");

    // 收 HelloAck{ok:true}。
    let (t, v) = read_frame(&mut reader).await.expect("helloack");
    assert_eq!(t, T_HELLO_ACK);
    assert_eq!(v["ok"], json!(true), "helloack should be ok, got {v}");

    // 收 ConfigPush（含 devices）。
    let (t, v) = read_frame(&mut reader).await.expect("configpush");
    assert_eq!(t, T_CONFIG_PUSH);
    assert_eq!(v["inst_id"], "sc1");
    assert!(v["devices"].is_array(), "configpush should carry devices");

    // 发 DataUp → 等 pipeline 落库 → 验 rtdata。
    writer
        .send(enc(
            T_DATA_UP,
            &json!({"devid":"devS","samples":[{"tag":"temp","value":42.5,"ts":1000}]}),
        ))
        .await
        .expect("send dataup");
    tokio::time::sleep(Duration::from_millis(500)).await;

    let cell = hget_cell(&env.client, "rtdata:devS", "devS.temp").await;
    assert!(
        cell.contains("42.5"),
        "rtdata devS.temp should contain 42.5, got '{cell}'"
    );

    // 写命令（mgr.send_ctl）→ 外部收 Command → 回 Ack。
    let msg = CtlMessage {
        instance: InstId::new("sc1"),
        devid: "devS".into(),
        cmd: "write".into(),
        data: r#"{"address":"x","value":1}"#.into(),
    };
    assert!(
        env.mgr.send_ctl("sc1", msg),
        "send_ctl should find running instance"
    );

    let (t, v) = read_frame(&mut reader).await.expect("command frame");
    assert_eq!(t, T_COMMAND);
    assert_eq!(v["cmd"], "write");
    let cmd_id = v["cmd_id"].as_u64().expect("cmd_id");
    writer
        .send(enc(T_COMMAND_ACK, &json!({"cmd_id":cmd_id,"ok":true})))
        .await
        .expect("send ack");

    // observed stat：DataUp 后 bridge 周期 flush（ticker 5s 错峰检查 + 距上次落盘
    // ≥ FLUSH_MAX_INTERVAL_SECS=30s 才写，到期时刻最坏 相位+30s≈35s——见 stat_tracker）。
    // 连接瞬间的初始 stat（connected/reconnect_count 已含，tick=0）由连接建立后的
    // 显式 flush 落；observed tick（tick_count>=1）须等到期落盘，故显式等 37s。
    tokio::time::sleep(Duration::from_secs(37)).await;
    let stat_v = env
        .client
        .execute_value("GET", &["stat:sc1"])
        .await
        .expect("stat GET");
    use lux::EmbeddedValue;
    let stat_json = match stat_v {
        EmbeddedValue::Bulk(b) => String::from_utf8_lossy(&b).to_string(),
        EmbeddedValue::Simple(s) => s,
        _ => String::new(),
    };
    assert!(!stat_json.is_empty(), "sidecar observed stat 应存在");
    let stat: serde_json::Value = serde_json::from_str(&stat_json).expect("stat json");
    assert!(
        stat["tick_count"].as_u64() >= Some(1),
        "observed stat 应计 DataUp，got {stat_json}"
    );
    assert_eq!(
        stat["connected"],
        json!(true),
        "connected metadata, got {stat_json}"
    );
    assert!(
        stat["reconnect_count"].as_u64() >= Some(1),
        "reconnect_count metadata, got {stat_json}"
    );

    teardown(env).await;
}

/// DataUp 校验闸（M-3）+ 闸拒进发现队列（审查修复，决策 D-M4"不过闸进
/// discoveries"）：未建模 tag 不落库但进 discoveries（元数据可见供 adopt
/// 建模）；建模 tag 正常落库且不进队列。
#[tokio::test]
async fn sidecar_dataup_gate_rejects_unmodeled_to_discovery() {
    let env = setup("sc-gate", 60).await;

    let stream = tokio::net::TcpStream::connect(("127.0.0.1", env.port))
        .await
        .expect("connect");
    let (r, w) = tokio::io::split(stream);
    let mut reader = tokio_util::codec::FramedRead::new(r, LengthDelimitedCodec::new());
    let mut writer = tokio_util::codec::FramedWrite::new(w, LengthDelimitedCodec::new());

    writer
        .send(enc(
            T_HELLO,
            &json!({"inst_id":"sc-gate","token":env.token,"ver":"2"}),
        ))
        .await
        .expect("send hello");
    let (t, _) = read_frame(&mut reader).await.expect("helloack");
    assert_eq!(t, T_HELLO_ACK);
    let (t, _) = read_frame(&mut reader).await.expect("configpush");
    assert_eq!(t, T_CONFIG_PUSH);

    // DataUp：temp（建模）+ ghost（未建模）同帧。
    writer
        .send(enc(
            T_DATA_UP,
            &json!({"devid":"devS","samples":[
                {"tag":"temp","value":25.5,"ts":1000},
                {"tag":"ghost","value":9.9,"ts":1000}]}),
        ))
        .await
        .expect("send dataup");
    tokio::time::sleep(Duration::from_millis(500)).await;

    let cell = hget_cell(&env.client, "rtdata:devS", "devS.temp").await;
    assert!(cell.contains("25.5"), "建模 tag 落库：'{cell}'");
    let ghost = env
        .client
        .execute_value("HGET", &["rtdata:devS", "devS.ghost"])
        .await
        .unwrap();
    assert!(
        matches!(ghost, lux::EmbeddedValue::Nil),
        "未建模 tag 不落库"
    );

    // 闸拒 tag 进发现队列（含样本值供建模参考）。
    let disc = env
        .client
        .execute_value("HGET", &["discoveries", "devS.ghost"])
        .await
        .unwrap();
    let raw = match disc {
        lux::EmbeddedValue::Bulk(b) => String::from_utf8_lossy(&b).to_string(),
        lux::EmbeddedValue::Simple(s) => s,
        _ => String::new(),
    };
    assert!(!raw.is_empty(), "ghost 应进发现队列");
    let d: serde_json::Value = serde_json::from_str(&raw).expect("discovery json");
    assert_eq!(d["devid"], "devS");
    assert_eq!(d["tag"], "ghost");
    assert!(
        d["sample_value"].as_str().unwrap().contains("9.9"),
        "样本值供建模参考：{d}"
    );
    // 建模 tag 不进队列。
    let temp = env
        .client
        .execute_value("HGET", &["discoveries", "devS.temp"])
        .await
        .unwrap();
    assert!(
        matches!(temp, lux::EmbeddedValue::Nil),
        "建模 tag 不进发现队列"
    );

    teardown(env).await;
}

#[tokio::test]
async fn sidecar_wrong_token_rejected() {
    let env = setup("sc2", 60).await;

    let stream = tokio::net::TcpStream::connect(("127.0.0.1", env.port))
        .await
        .expect("connect");
    let (r, w) = tokio::io::split(stream);
    let mut reader = tokio_util::codec::FramedRead::new(r, LengthDelimitedCodec::new());
    let mut writer = tokio_util::codec::FramedWrite::new(w, LengthDelimitedCodec::new());

    writer
        .send(enc(
            T_HELLO,
            &json!({"inst_id":"sc2","token":"WRONG","ver":"2"}),
        ))
        .await
        .expect("send hello");

    let (t, v) = read_frame(&mut reader).await.expect("helloack");
    assert_eq!(t, T_HELLO_ACK);
    assert_eq!(v["ok"], json!(false), "wrong token → ack ok:false, got {v}");

    teardown(env).await;
}

#[tokio::test]
async fn sidecar_reconnect_resends_config() {
    let env = setup("sc3", 60).await;

    // 第一次连接 + 握手 + ConfigPush。
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", env.port))
        .await
        .expect("connect");
    let (r, w) = tokio::io::split(stream);
    let mut reader = FramedRead::new(r, LengthDelimitedCodec::new());
    let mut writer = FramedWrite::new(w, LengthDelimitedCodec::new());
    writer
        .send(enc(
            T_HELLO,
            &json!({"inst_id":"sc3","token":env.token,"ver":"2"}),
        ))
        .await
        .expect("hello");
    let (t, _) = read_frame(&mut reader).await.expect("helloack");
    assert_eq!(t, T_HELLO_ACK);
    let (t, _) = read_frame(&mut reader).await.expect("configpush");
    assert_eq!(t, T_CONFIG_PUSH);

    // drop 客户端 → 断连；task 回 'outer 等 slot（支持外部重连）。
    drop(reader);
    drop(writer);
    tokio::time::sleep(Duration::from_millis(400)).await;

    // 重连 → 应重新收 HelloAck + ConfigPush（验证 slot 支持重连 + 配置重发）。
    let stream2 = tokio::net::TcpStream::connect(("127.0.0.1", env.port))
        .await
        .expect("reconnect");
    let (r2, w2) = tokio::io::split(stream2);
    let mut reader2 = FramedRead::new(r2, LengthDelimitedCodec::new());
    let mut writer2 = FramedWrite::new(w2, LengthDelimitedCodec::new());
    writer2
        .send(enc(
            T_HELLO,
            &json!({"inst_id":"sc3","token":env.token,"ver":"2"}),
        ))
        .await
        .expect("hello2");
    let (t, v) = read_frame(&mut reader2).await.expect("helloack2");
    assert_eq!(t, T_HELLO_ACK);
    assert_eq!(v["ok"], json!(true), "reconnect helloack should be ok");
    let (t, v) = read_frame(&mut reader2).await.expect("configpush2");
    assert_eq!(t, T_CONFIG_PUSH);
    assert_eq!(
        v["inst_id"], "sc3",
        "config should be re-pushed on reconnect"
    );

    drop(writer2);
    teardown(env).await;
}

#[tokio::test]
async fn sidecar_heartbeat_timeout_disconnects() {
    // hb_timeout=2s：不发心跳 → task 在 ~2s + hb_check(1s) 内判失联断连。
    let env = setup("sc4", 2).await;

    let stream = tokio::net::TcpStream::connect(("127.0.0.1", env.port))
        .await
        .expect("connect");
    let (r, w) = tokio::io::split(stream);
    let mut reader = FramedRead::new(r, LengthDelimitedCodec::new());
    let mut writer = FramedWrite::new(w, LengthDelimitedCodec::new());

    writer
        .send(enc(
            T_HELLO,
            &json!({"inst_id":"sc4","token":env.token,"ver":"2"}),
        ))
        .await
        .expect("hello");
    let (t, _) = read_frame(&mut reader).await.expect("helloack");
    assert_eq!(t, T_HELLO_ACK);
    let (t, _) = read_frame(&mut reader).await.expect("configpush");
    assert_eq!(t, T_CONFIG_PUSH);

    // 不发心跳、不发数据 → 等 task 超时断连（hb_timeout 2s + 容差）。
    tokio::time::sleep(Duration::from_secs(4)).await;
    // reader 应收到 EOF（None）——证明 task 主动断连；若仍阻塞说明 task 未超时。
    let res = tokio::time::timeout(Duration::from_secs(2), reader.next()).await;
    assert!(
        res.is_ok(),
        "task 应在心跳超时后断连；reader 仍阻塞说明 task 未超时断连"
    );

    drop(writer);
    teardown(env).await;
}
