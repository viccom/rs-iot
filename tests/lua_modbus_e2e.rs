//! luaClient 端到端：lux + mock Modbus TCP server + luaClient 实例（scripts/it_modbus）
//! + Lua 脚本采集 → rtdata + 写命令 round-trip。复用 s7_e2e 范式。
//!
//! 验证 MVP：Lua 脚本（标准 Modbus FC03/FC06）经 luaClient 全链路闭环。
//! mock server 裸字节解析 MBAP+FC03/FC06，累积读防 TCP 拆包（FC03/FC06 请求固定 12B）。

mod common;

use common::test_data_dir;
use rs_iot::host::{BridgeRegistry, CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::plugin::traits::Processor;
use rs_iot::transform::passthrough::Passthrough;
use rs_iot::types::{
    AppType, CtlMessage, DeviceSpec, HostKind, Hwid, InstConfig, InstId, PluginName, Sample,
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

/// mock server 共享状态：寄存器内存 + 写记录。
struct ServerState {
    regs: Mutex<HashMap<u16, u16>>,
    writes: Mutex<Vec<(u16, u16)>>,
}

#[tokio::test]
async fn lua_modbus_collect_and_write_roundtrip() {
    let data_dir = test_data_dir("lua-modbus");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    // 预播种寄存器：reg0=125（→ temp=12.5）、reg1=32000（→ speed=32000）。
    let state = Arc::new(ServerState {
        regs: Mutex::new(HashMap::from([(0u16, 125u16), (1u16, 32000u16)])),
        writes: Mutex::new(vec![]),
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let sv_state = state.clone();
    let sv_token = CancellationToken::new();
    let t2 = sv_token.clone();
    tokio::spawn(async move { serve_modbus(listener, sv_state, t2).await });

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

    // scripts_root 指向仓库 scripts/ 目录（CARGO_MANIFEST_DIR）。
    let scripts_root = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts");
    let inst = InstConfig {
        inst_id: "lua-it-modbus".into(),
        inst_name: "it modbus".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: true,
        app_config: json!({
            "script": "it_modbus",
            "kind": "tcp",
            "endpoint": format!("127.0.0.1:{port}"),
            "period": 1,
            "recv_idle_ms": 80,
            "scripts_root": scripts_root,
        }),
        devices: vec![DeviceSpec {
            label: "m".into(),
            value: "devM".into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start lua");

    // period=1s，等 2.5s 保证至少两轮采集 + pipeline 落库。
    tokio::time::sleep(Duration::from_millis(2500)).await;

    let temp = hget_cell(&client, "rtdata:devM", "devM.temp").await;
    assert!(temp.contains("12.5"), "reg0=125 → temp=12.5，得到 '{temp}'");
    let speed = hget_cell(&client, "rtdata:devM", "devM.speed").await;
    assert!(
        speed.contains("32000"),
        "reg1=32000 → speed=32000，得到 '{speed}'"
    );

    // 写命令：经 InstanceManager 投递（与 REST/MQTT 同通道）。
    let msg = CtlMessage {
        instance: InstId::new("lua-it-modbus"),
        devid: "devM".into(),
        cmd: "write".into(),
        data: json!({"addr": 0, "value": 1}).to_string(),
    };
    assert!(
        mgr.send_ctl("lua-it-modbus", msg),
        "send_ctl 应命中运行中的实例"
    );
    // 等写 round-trip 完成。
    tokio::time::sleep(Duration::from_millis(800)).await;
    let writes = state.writes.lock().expect("writes lock").clone();
    assert_eq!(writes.len(), 1, "mock 应收到 1 条写，实际 {writes:?}");
    assert_eq!(writes[0], (0, 1), "addr=0 value=1");

    shutdown.cancel();
    sv_token.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
}

/// mock modbus server acceptor：每连接 spawn 一个 `serve_one` task。
async fn serve_modbus(listener: TcpListener, state: Arc<ServerState>, token: CancellationToken) {
    loop {
        tokio::select! {
            biased;
            _ = token.cancelled() => break,
            res = listener.accept() => {
                if let Ok((stream, _)) = res {
                    let st = state.clone();
                    let t = token.child_token();
                    tokio::spawn(async move { serve_one(stream, st, t).await });
                }
            }
        }
    }
}

/// 单连接服务循环：裸字节解析 MBAP+FC03/FC06。
///
/// 累积读直到 ≥12 字节再解析（FC03/FC06 请求固定 12 字节：MBAP 7 + PDU 5），
/// 防 TCP 拆包导致单次 read 读不全。
async fn serve_one(mut stream: TcpStream, state: Arc<ServerState>, token: CancellationToken) {
    use tokio::io::AsyncWriteExt;
    let mut acc: Vec<u8> = Vec::with_capacity(32);
    let mut tmp = [0u8; 256];
    loop {
        while acc.len() < 12 {
            tokio::select! {
                biased;
                _ = token.cancelled() => return,
                res = stream.read(&mut tmp) => match res {
                    Ok(0) | Err(_) => return,
                    Ok(n) => acc.extend_from_slice(&tmp[..n]),
                },
            }
        }
        let fc = acc[7];
        let resp: Vec<u8> = match fc {
            // FC03 读保持寄存器：回 MBAP + fc=3 + bytecount + data。
            3 => {
                let addr = u16::from_be_bytes([acc[8], acc[9]]) as usize;
                let qty = u16::from_be_bytes([acc[10], acc[11]]) as usize;
                let regs = state.regs.lock().expect("regs lock");
                let mut data = Vec::with_capacity(qty * 2);
                for i in 0..qty {
                    let v = regs.get(&(addr as u16 + i as u16)).copied().unwrap_or(0);
                    data.extend_from_slice(&v.to_be_bytes());
                }
                drop(regs);
                let mut r = Vec::with_capacity(7 + 3 + qty * 2);
                r.extend_from_slice(&acc[0..4]); // tid + pid（echo MBAP 前 4 字节）
                let len: u16 = 3 + (qty * 2) as u16;
                r.extend_from_slice(&len.to_be_bytes());
                r.push(acc[6]); // uid（echo）
                r.push(3); // fc
                r.push((qty * 2) as u8); // byte count
                r.extend_from_slice(&data);
                r
            }
            // FC06 写单寄存器：记录写 + echo 请求（12 字节）。
            6 => {
                let addr = u16::from_be_bytes([acc[8], acc[9]]);
                let value = u16::from_be_bytes([acc[10], acc[11]]);
                state
                    .writes
                    .lock()
                    .expect("writes lock")
                    .push((addr, value));
                acc[..12].to_vec()
            }
            _ => {
                acc.clear();
                continue;
            }
        };
        let _ = stream.write_all(&resp).await;
        acc.clear();
    }
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
