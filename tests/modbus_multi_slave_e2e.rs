//! modbus 多 slave 单实例采集 端到端（CI 内，mock TCP modbus server）。
//!
//! 起 lux + InstanceManager + modbusClient 实例，直接从 hub_rx 收 Sample（不经
//! rtdata 落盘，更直接验证采集循环产出）。验四点：
//! ① 多 slave 各自入库（m1/m3 均采到）
//! ② 故障隔离（unit2 不响应，unit1/3 正常——per-request 超时跳过不阻塞其他 slave）
//! ③ 两策略均采到（RoundRobin 默认 + Sequential）
//! ④ 旧单 slave 配置兼容（devices 无 slave_id，回退 app_config.slave_id）。
//!
//! 编排照搬 `tests/sidecar_e2e.rs` / `tests/dataflow.rs`（已验证可用）。

mod common;

use rs_iot::host::{BridgeRegistry, CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::types::{
    AppType, ChildSpec, DeviceSpec, HostKind, Hwid, InstConfig, PluginName, Sample,
};
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// mock TCP modbus 从站：按请求的 unit id 回不同数据。
/// - unit 1：holding[0..] 回 [1000, 2000, 3000, 4000]
/// - unit 2：不响应（模拟故障/超时，验故障隔离）
/// - unit 3：holding[0..] 回 [500, 600]
///
/// 仅处理 FC03（读保持寄存器）。MBAP 解析：7B 头 [tid(2)+pid(2)+len(2)+uid(1)]
/// + PDU [fc(1)+addr(2)+qty(2)]。unit 2 收到请求但不回包，触发客户端 per-request 超时。
async fn spawn_mock_server() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("mock bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        loop {
            if let Ok((sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut sock = sock;
                    let mut hdr = [0u8; 7];
                    loop {
                        if sock.read_exact(&mut hdr).await.is_err() {
                            return;
                        }
                        let tid = u16::from_be_bytes([hdr[0], hdr[1]]);
                        let uid = hdr[6];
                        let pdu_len =
                            (u16::from_be_bytes([hdr[4], hdr[5]]) as usize).saturating_sub(1);
                        let mut pdu = vec![0u8; pdu_len];
                        if pdu_len > 0 && sock.read_exact(&mut pdu).await.is_err() {
                            return;
                        }
                        // 仅处理 FC03（holding 读）；其它 FC 忽略。
                        if pdu.is_empty() || pdu[0] != 3 {
                            continue;
                        }
                        let resp_regs: Vec<u16> = match uid {
                            1 => vec![1000, 2000, 3000, 4000],
                            3 => vec![500, 600],
                            _ => continue, // unit 2（及其它）：不响应，触发客户端超时。
                        };
                        let qty = u16::from_be_bytes([pdu[3], pdu[4]]) as usize;
                        let data: Vec<u16> = resp_regs.into_iter().take(qty).collect();
                        let mut resp = Vec::new();
                        resp.extend_from_slice(&tid.to_be_bytes());
                        resp.extend_from_slice(&0u16.to_be_bytes()); // pid=0
                        let len = 3 + data.len() * 2;
                        resp.extend_from_slice(&(len as u16).to_be_bytes());
                        resp.push(uid);
                        resp.push(3); // fc echo
                        resp.push((data.len() * 2) as u8); // byte count
                        for r in &data {
                            resp.extend_from_slice(&r.to_be_bytes());
                        }
                        let _ = sock.write_all(&resp).await;
                    }
                });
            }
        }
    });
    addr
}

/// 起 lux + InstanceManager + modbusClient 实例，从 hub_rx 收 Sample 到
/// `got.len() >= min_keys` 或 deadline（先到为准），返回收到的 `{devid.field: value}` 映射。
///
/// 直接消费 hub_rx（不 spawn pipeline），更直接验证 modbusClient 采集循环产出。
async fn collect_samples(
    inst_id: &str,
    app_config: serde_json::Value,
    devices: Vec<DeviceSpec>,
    min_keys: usize,
    deadline_secs: u64,
) -> HashMap<String, String> {
    let data_dir = common::test_data_dir(&format!("modbus-ms-{inst_id}"));
    let (handle, _client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&_client)
        .await
        .expect("bootstrap");

    let shutdown = CancellationToken::new();
    let ctx = HostContext {
        lux: _client,
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
    let (hub_tx, mut hub_rx) = tokio::sync::mpsc::channel::<Sample>(64);
    let mgr = std::sync::Arc::new(InstanceManager::new(ctx, hub_tx));

    let inst = InstConfig {
        inst_id: inst_id.into(),
        inst_name: "it modbus multi-slave".into(),
        app_code: PluginName::new("modbusClient"),
        app_type: AppType::South,
        auto_start: false,
        app_config,
        devices,
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start modbus");

    // 收 Sample 到 min_keys 或 deadline（先到为准）。
    let mut got: HashMap<String, String> = HashMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(deadline_secs);
    loop {
        if got.len() >= min_keys {
            break;
        }
        match tokio::time::timeout_at(deadline, hub_rx.recv()).await {
            Ok(Some(s)) => {
                for (k, v) in &s.values {
                    got.insert(format!("{}.{}", s.devid, k), format!("{v:?}"));
                }
            }
            Ok(None) => break, // hub_tx 全 drop（实例退出）
            Err(_) => break,   // deadline 到
        }
    }

    // 优雅关闭：cancel → stop 实例 → lux SAVE。
    shutdown.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
    got
}

/// 构造单点 holding:0:i16 设备（slave_id 指定 unit）。
fn mk_dev(devid: &str, slave_id: u8) -> DeviceSpec {
    DeviceSpec {
        label: devid.into(),
        value: devid.into(),
        children: vec![ChildSpec {
            label: "v0".into(),
            value: "holding:0:i16".into(),
            r#type: "i16".into(),
        }],
        slave_id: Some(slave_id),
    }
}

#[tokio::test]
async fn multi_slave_collects_each_devid() {
    let addr = spawn_mock_server().await;
    let app = serde_json::json!({
        "kind": "tcp",
        "endpoint": addr.to_string(),
        "period": 1,
        "request_timeout_secs": 1,
    });
    let devices = vec![mk_dev("m1", 1), mk_dev("m3", 3)];
    let got = collect_samples("ms1", app, devices, 2, 8).await;
    assert!(got.contains_key("m1.v0"), "slave1 应入库: {got:?}");
    assert!(got.contains_key("m3.v0"), "slave3 应入库: {got:?}");
}

#[tokio::test]
async fn fault_isolation_unresponsive_slave() {
    let addr = spawn_mock_server().await;
    let app = serde_json::json!({
        "kind": "tcp",
        "endpoint": addr.to_string(),
        "period": 1,
        "request_timeout_secs": 1,
    });
    let devices = vec![mk_dev("m1", 1), mk_dev("m2", 2), mk_dev("m3", 3)];
    // unit2 不响应 → 每 tick 多耗 ~1s（超时），给足 deadline。
    let got = collect_samples("ms2", app, devices, 2, 12).await;
    assert!(
        got.contains_key("m1.v0"),
        "故障隔离：slave1 仍应入库: {got:?}"
    );
    assert!(
        got.contains_key("m3.v0"),
        "故障隔离：slave3 仍应入库: {got:?}"
    );
    assert!(!got.contains_key("m2.v0"), "故障 slave2 不应入库: {got:?}");
}

#[tokio::test]
async fn backward_compat_legacy_single_slave_config() {
    let addr = spawn_mock_server().await;
    // 旧配置：无 device.slave_id，实例级 app_config.slave_id=1（向后兼容回退）。
    let app = serde_json::json!({
        "kind": "tcp",
        "endpoint": addr.to_string(),
        "slave_id": 1,
        "period": 1,
    });
    let devices = vec![DeviceSpec {
        label: "m1".into(),
        value: "m1".into(),
        children: vec![ChildSpec {
            label: "v0".into(),
            value: "holding:0:i16".into(),
            r#type: "i16".into(),
        }],
        slave_id: None,
    }];
    let got = collect_samples("ms3", app, devices, 1, 8).await;
    assert!(
        got.contains_key("m1.v0"),
        "旧单 slave 配置应兼容入库: {got:?}"
    );
}

#[tokio::test]
async fn sequential_strategy_also_collects() {
    let addr = spawn_mock_server().await;
    let app = serde_json::json!({
        "kind": "tcp",
        "endpoint": addr.to_string(),
        "period": 1,
        "poll_strategy": "sequential",
        "request_timeout_secs": 1,
    });
    let devices = vec![mk_dev("m1", 1), mk_dev("m3", 3)];
    let got = collect_samples("ms4", app, devices, 2, 8).await;
    assert!(
        got.contains_key("m1.v0") && got.contains_key("m3.v0"),
        "sequential 两 slave 均应入库: {got:?}"
    );
}

/// Sequential + 死从站(unit2)多 region：超时后跳过 unit2 剩余 region（spec §3.2 break-on-timeout）。
/// 验证隔离不退化（m1/m3 入库、m2 不入库）；break 让 m2 只付 1×timeout（时间性留 M3 真机）。
#[tokio::test]
async fn sequential_breaks_on_timeout_skipping_dead_slave_remaining_regions() {
    let addr = spawn_mock_server().await;
    let app = serde_json::json!({
        "kind": "tcp",
        "endpoint": addr.to_string(),
        "period": 1,
        "poll_strategy": "sequential",
        "request_timeout_secs": 1,
    });
    // m2 两个 holding 点 gap=50 > 默认 gap_threshold(16) → merge 成 2 region。
    let m2 = DeviceSpec {
        label: "m2".into(),
        value: "m2".into(),
        children: vec![
            ChildSpec {
                label: "v0".into(),
                value: "holding:0:i16".into(),
                r#type: "i16".into(),
            },
            ChildSpec {
                label: "v1".into(),
                value: "holding:50:i16".into(),
                r#type: "i16".into(),
            },
        ],
        slave_id: Some(2),
    };
    let devices = vec![mk_dev("m1", 1), m2, mk_dev("m3", 3)];
    let got = collect_samples("ms5", app, devices, 2, 8).await;
    assert!(got.contains_key("m1.v0"), "slave1 入库: {got:?}");
    assert!(got.contains_key("m3.v0"), "slave3 入库: {got:?}");
    assert!(
        !got.contains_key("m2.v0") && !got.contains_key("m2.v1"),
        "死从站 m2 不应入库: {got:?}"
    );
}
