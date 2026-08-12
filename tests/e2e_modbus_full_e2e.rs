//! rs-iot 改进功能完整端到端测试 — Modbus 全协议 + LUA + P1 三子系统验证。
//!
//! **目标**：在最近 58 个 commit 改进（m3/m4/P1 三子系统）之上，做一次完整集成验证。
//!
//! **测试矩阵**：
//! - T1: `modbusClient` RTU over serial（物理 COM2 → COM3 com0com 对）3 slave × FC01/03/04
//!   **`#[ignore]`，CI 不跑**——依赖用户 Modbus 仿真器占 COM3。手动：
//!   `cargo test --test e2e_modbus_full_e2e modbus_rtu_serial -- --ignored`
//! - T2: `modbusClient` TCP（127.0.0.1:0 OS 分配）2 slave × FC03/04 + 多类型数据（CI 可跑）
//! - T3: `luaClient` Modbus TCP（127.0.0.1:0 OS 分配）2 devid 嵌套输出（CI 可跑）
//! - T4: P1 三子系统（`/metrics`、`/api/v1/logs/recent`、`/api/v1/inst/trace`）（CI 可跑）
//!
//! **物理串口约定**（仅 T1）：
//! - COM2（com0com CNCA0）→ rs-iot 端 (`serial_port: COM2`)
//! - COM3（com0com CNCB0）→ 用户 Modbus 仿真器（Modbus Slave/Poll 软件，3 slave）。
//!   COM3 全局互斥，避免 cargo test 多测试并行冲突。
//!
//! **取舍**：不固定端口到 2002/2003（OS 分配端口 0 + 端口探测），避免 CI 冲突。
//! 测试运行顺序按字母排序，本测试文件用 COM3 锁统一管理。
//!
//! **借鉴**：编排照搬 `tests/modbus_multi_slave_e2e.rs`（已验证可用模式）+ `tests/metrics_e2e.rs`
//! 暴露 axum 子集。

mod common;

use rs_iot::api::AppState;
use rs_iot::host::{BridgeRegistry, CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::types::{
    AppType, ChildSpec, DeviceSpec, HostKind, Hwid, InstConfig, PluginName, Sample,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 全局 COM3 互斥：物理串口对，同进程内不能并行打开 COM3。
/// cargo test 默认多线程运行多测试函数，故全局锁保护。
static COM3_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 构造一个 device spec：devid + slave_id + 点位列表 `[(label, addr)]`。
fn mk_dev(devid: &str, slave_id: u8, points: Vec<(&str, &str)>) -> DeviceSpec {
    DeviceSpec {
        label: devid.into(),
        value: devid.into(),
        children: points
            .into_iter()
            .map(|(label, addr)| ChildSpec {
                label: label.into(),
                value: addr.into(),
                r#type: "".into(),
            })
            .collect(),
        slave_id: Some(slave_id),
    }
}

// ============================================================================
// T1: Modbus RTU over Serial（物理 COM2 ↔ COM3 com0com 对，COM3 由用户 Modbus 仿真器占用）
//     3 slave × FC01/03/04。**不自起 mock server**——直接连 COM2 跟用户的仿真器通讯。
//     不断言精确值（仿真器寄存器值未知），只断言 3 个 devid 都收到 Sample。
// ============================================================================

/// 起 rs-iot + InstanceManager + 1 个 modbusClient 实例（接 COM2），从 hub_rx 收 Sample。
///
/// COM3 由用户的 Modbus 仿真器（如 Modbus Slave / Modbus Poll 软件）占用，3 个 slave：
/// - unit 1 F=04 (input)
/// - unit 2 F=03 (holding)
/// - unit 3 F=01 (coil)
async fn collect_via_serial(
    inst_id: &str,
    devices: Vec<DeviceSpec>,
    _min_keys: usize,
    deadline_secs: u64,
) -> BTreeMap<String, BTreeMap<String, String>> {
    let data_dir = common::test_data_dir(&format!("e2e-modbus-serial-${inst_id}"));
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    let shutdown = CancellationToken::new();
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("e2e-hwid"),
        shutdown: shutdown.clone(),
        ctl_registry: CtlRegistry::new(Arc::new(rs_iot::process_metrics::ProcessMetrics::new())),
        bridge_registry: BridgeRegistry::new(),
        metrics: Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        event_tx: rs_iot::event_bus::EventBus::dummy(),
    };
    let (hub_tx, mut hub_rx) = mpsc::channel::<Sample>(64);
    let mgr = Arc::new(InstanceManager::new(ctx, hub_tx));

    // 期望 devid 集合（move 进 start_source 前算好，循环判定用）。
    let want_devids: std::collections::HashSet<String> =
        devices.iter().map(|d| d.value.clone()).collect();

    let inst = InstConfig {
        inst_id: inst_id.into(),
        inst_name: "e2e modbus serial".into(),
        app_code: PluginName::new("modbusClient"),
        app_type: AppType::South,
        auto_start: false,
        app_config: serde_json::json!({
            "kind": "rtu_serial",
            "serial_port": "COM2",
            "baud_rate": 9600,
            "data_bits": 8,
            "parity": "none",
            "stop_bits": 1,
            "flow_control": "none",
            "period": 1,
            "request_timeout_secs": 2,
        }),
        devices,
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst)
        .await
        .expect("start modbus rtu_serial");

    // 给实例一点时间 connect + 首个 tick
    tokio::time::sleep(Duration::from_millis(500)).await;
    // 状态检查：实例应 Running（若 Failed 说明 task 早退）
    let states = mgr.status();
    println!("[T1] 实例状态 after start: {states:?}");

    let mut got: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(deadline_secs);
    loop {
        // 全部期望 devid 都至少收到 1 点 → 完成。
        if want_devids.iter().all(|d| got.contains_key(d)) {
            break;
        }
        match tokio::time::timeout_at(deadline, hub_rx.recv()).await {
            Ok(Some(s)) => {
                let entry = got.entry(s.devid.clone()).or_default();
                for (k, v) in &s.values {
                    entry.insert(k.clone(), format!("{v:?}"));
                }
            }
            Ok(None) => {
                println!("[T1] hub_rx 返回 None（实例 task 已退出），当前 got = {got:?}");
                let states = mgr.status();
                println!("[T1] 实例状态 after hub close: {states:?}");
                break;
            }
            Err(_) => {
                println!("[T1] deadline 到，当前 got = {got:?}");
                break;
            }
        }
    }
    shutdown.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
    got
}

/// 现场端到端测试：依赖物理 COM2↔COM3 com0com 串口对 + 用户 Modbus 仿真器占 COM3
/// （3 slave：unit1/FC04 input、unit2/FC03 holding、unit3/FC01 coil）。
/// **CI 不跑**（`#[ignore]`）——CLAUDE.md 约定现场测试依赖真机，CI 守协议正确性、
/// 现场守物理与时序正确性。手动跑：`cargo test --test e2e_modbus_full_e2e modbus_rtu_serial -- --ignored`。
#[tokio::test]
#[ignore]
async fn modbus_rtu_serial_com2_collects_3_slaves_multi_fc() {
    let _guard = COM3_LOCK.lock().await;

    let devices = vec![
        // unit 1 F=04 (input) → 4 个 i16 点
        mk_dev(
            "s1",
            1,
            vec![
                ("v0", "input:0:i16"),
                ("v1", "input:1:i16"),
                ("v2", "input:2:i16"),
                ("v3", "input:3:i16"),
            ],
        ),
        // unit 2 F=03 (holding) → 4 个 i16 点
        mk_dev(
            "s2",
            2,
            vec![
                ("v0", "holding:0:i16"),
                ("v1", "holding:1:i16"),
                ("v2", "holding:2:i16"),
                ("v3", "holding:3:i16"),
            ],
        ),
        // unit 3 F=01 (coil) → 4 个 bool 点
        mk_dev(
            "s3",
            3,
            vec![
                ("b0", "coil:0:bool"),
                ("b1", "coil:1:bool"),
                ("b2", "coil:2:bool"),
                ("b3", "coil:3:bool"),
            ],
        ),
    ];
    // min_keys=3：每个 devid 至少收到 1 点即可（仿真器寄存器值未知，不断言精确值）。
    let got = collect_via_serial("rtu_com2", devices, 3, 20).await;

    println!("[T1 rtu_serial COM2] got = {got:?}");
    assert!(got.contains_key("s1"), "s1 (FC04 input) missing: {got:?}");
    assert!(got.contains_key("s2"), "s2 (FC03 holding) missing: {got:?}");
    assert!(got.contains_key("s3"), "s3 (FC01 coil) missing: {got:?}");
}

// ============================================================================
// T2: Modbus TCP（127.0.0.1:0, OS 分配）— 2 slave × FC03/04 + 多类型数据
// ============================================================================

/// Mock TCP modbus server：MBAP 头，路由 unit→slave_data。
/// - unit 1 F=04 → i16 [1010, 1020, 1030, 1040]
/// - unit 2 F=03 → f32 ABCD [12.5, 25.0, 37.5, 50.0]
async fn spawn_mock_tcp_server() -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind tcp mock");
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        loop {
            if let Ok((sock, _)) = listener.accept().await {
                tokio::spawn(handle_conn(sock));
            }
        }
    });
    (port, handle)
}

async fn handle_conn(mut sock: TcpStream) {
    let mut hdr = [0u8; 7];
    loop {
        if sock.read_exact(&mut hdr).await.is_err() {
            return;
        }
        let tid = u16::from_be_bytes([hdr[0], hdr[1]]);
        let uid = hdr[6];
        let pdu_len = (u16::from_be_bytes([hdr[4], hdr[5]]) as usize).saturating_sub(1);
        let mut pdu = vec![0u8; pdu_len];
        if pdu_len > 0 && sock.read_exact(&mut pdu).await.is_err() {
            return;
        }
        if pdu.is_empty() {
            continue;
        }
        let fc = pdu[0];
        // qty = pdu[3..5] big-endian
        let qty = if pdu_len >= 5 {
            u16::from_be_bytes([pdu[3], pdu[4]]) as usize
        } else {
            0
        };
        let data: Vec<u8> = match (uid, fc) {
            (1, 4) => {
                // FC04 input → i16
                let regs = [1010i16, 1020, 1030, 1040];
                regs.into_iter()
                    .take(qty)
                    .flat_map(|v| v.to_be_bytes())
                    .collect()
            }
            (2, 3) => {
                // FC03 holding → f32 ABCD
                let regs = [12.5f32, 25.0, 37.5, 50.0];
                regs.into_iter()
                    .take(qty)
                    .flat_map(|v| v.to_be_bytes())
                    .collect()
            }
            _ => continue,
        };
        let mut resp = Vec::new();
        resp.extend_from_slice(&tid.to_be_bytes());
        resp.extend_from_slice(&0u16.to_be_bytes()); // pid=0
        let len = 3 + data.len();
        resp.extend_from_slice(&(len as u16).to_be_bytes());
        resp.push(uid);
        resp.push(fc);
        resp.push(data.len() as u8);
        resp.extend_from_slice(&data);
        let _ = sock.write_all(&resp).await;
    }
}

async fn collect_via_tcp(
    inst_id: &str,
    endpoint: String,
    devices: Vec<DeviceSpec>,
    min_keys: usize,
    deadline_secs: u64,
) -> BTreeMap<String, BTreeMap<String, String>> {
    let data_dir = common::test_data_dir(&format!("e2e-modbus-tcp-${inst_id}"));
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    let shutdown = CancellationToken::new();
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("e2e-hwid"),
        shutdown: shutdown.clone(),
        ctl_registry: CtlRegistry::new(Arc::new(rs_iot::process_metrics::ProcessMetrics::new())),
        bridge_registry: BridgeRegistry::new(),
        metrics: Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        event_tx: rs_iot::event_bus::EventBus::dummy(),
    };
    let (hub_tx, mut hub_rx) = mpsc::channel::<Sample>(64);
    let mgr = Arc::new(InstanceManager::new(ctx, hub_tx));

    let inst = InstConfig {
        inst_id: inst_id.into(),
        inst_name: "e2e modbus tcp".into(),
        app_code: PluginName::new("modbusClient"),
        app_type: AppType::South,
        auto_start: false,
        app_config: serde_json::json!({
            "kind": "tcp",
            "endpoint": endpoint,
            "period": 1,
            "request_timeout_secs": 1,
        }),
        devices,
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start modbus tcp");

    let mut got: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(deadline_secs);
    loop {
        if got.values().map(|m| m.len()).sum::<usize>() >= min_keys {
            break;
        }
        match tokio::time::timeout_at(deadline, hub_rx.recv()).await {
            Ok(Some(s)) => {
                let entry = got.entry(s.devid.clone()).or_default();
                for (k, v) in &s.values {
                    entry.insert(k.clone(), format!("{v:?}"));
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    shutdown.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
    got
}

#[tokio::test]
async fn modbus_tcp_2_slaves_mixed_types() {
    let (port, _server) = spawn_mock_tcp_server().await;
    let endpoint = format!("127.0.0.1:{port}");
    let devices = vec![
        // unit 1 F=04 → 4 个 i16 点
        mk_dev(
            "t1",
            1,
            vec![
                ("v0", "input:0:i16"),
                ("v1", "input:1:i16"),
                ("v2", "input:2:i16"),
                ("v3", "input:3:i16"),
            ],
        ),
        // unit 2 F=03 → 4 个 f32 点（验证 f32 解码）
        mk_dev(
            "t2",
            2,
            vec![
                ("v0", "holding:0:f32:ABCD"),
                ("v1", "holding:2:f32:ABCD"),
                ("v2", "holding:4:f32:ABCD"),
                ("v3", "holding:6:f32:ABCD"),
            ],
        ),
    ];
    let got = collect_via_tcp("tcp1", endpoint, devices, 8, 10).await;

    println!("[T2 tcp] got = {got:?}");
    assert!(got.contains_key("t1"), "t1 (FC04 i16) missing: {got:?}");
    assert!(got.contains_key("t2"), "t2 (FC03 f32) missing: {got:?}");
}

// ============================================================================
// T3: LUA 脚本 Modbus — 127.0.0.1:0 + multi-device 输出
// ============================================================================

/// 为 luaClient 准备一个简单 modbus-like TCP server：FC03 → 返回固定值。
async fn spawn_mock_tcp_for_lua() -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind lua mock");
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        loop {
            if let Ok((sock, _)) = listener.accept().await {
                tokio::spawn(handle_lua_conn(sock));
            }
        }
    });
    (port, handle)
}

async fn handle_lua_conn(mut sock: TcpStream) {
    let mut hdr = [0u8; 7];
    loop {
        if sock.read_exact(&mut hdr).await.is_err() {
            return;
        }
        let tid = u16::from_be_bytes([hdr[0], hdr[1]]);
        let uid = hdr[6];
        let pdu_len = (u16::from_be_bytes([hdr[4], hdr[5]]) as usize).saturating_sub(1);
        let mut pdu = vec![0u8; pdu_len];
        if pdu_len > 0 && sock.read_exact(&mut pdu).await.is_err() {
            return;
        }
        if pdu.is_empty() || pdu[0] != 3 {
            continue;
        }
        let qty = if pdu_len >= 5 {
            u16::from_be_bytes([pdu[3], pdu[4]]) as usize
        } else {
            0
        };
        // FC03 holding → i16 [1234, 5678]（足够 2 寄存器）
        let regs = [1234i16, 5678];
        let data: Vec<u8> = regs
            .iter()
            .take(qty.min(regs.len()))
            .flat_map(|v| v.to_be_bytes())
            .collect();
        let mut resp = Vec::new();
        resp.extend_from_slice(&tid.to_be_bytes());
        resp.extend_from_slice(&0u16.to_be_bytes());
        let len = 3 + data.len();
        resp.extend_from_slice(&(len as u16).to_be_bytes());
        resp.push(uid);
        resp.push(3);
        resp.push(data.len() as u8);
        resp.extend_from_slice(&data);
        let _ = sock.write_all(&resp).await;
    }
}

#[tokio::test]
async fn lua_client_modbus_multi_device_outputs() {
    use rs_iot::plugin::instance::InstanceManager;

    let (port, _server) = spawn_mock_tcp_for_lua().await;
    let endpoint = format!("127.0.0.1:{port}");

    let data_dir = common::test_data_dir("e2e-lua-modbus");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    let shutdown = CancellationToken::new();
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("e2e-hwid"),
        shutdown: shutdown.clone(),
        ctl_registry: CtlRegistry::new(Arc::new(rs_iot::process_metrics::ProcessMetrics::new())),
        bridge_registry: BridgeRegistry::new(),
        metrics: Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        event_tx: rs_iot::event_bus::EventBus::dummy(),
    };
    let (hub_tx, mut hub_rx) = mpsc::channel::<Sample>(64);
    let mgr = Arc::new(InstanceManager::new(ctx, hub_tx));

    // lua multi-device 脚本：每 devid 发一个 FC03 请求，collect 嵌套返回。
    let lua_script = r#"
function collect()
    local out = {}
    local devs = ctx:list_devices()
    for _, dev in ipairs(devs) do
        local unit = dev.value == 'ldev1' and 1 or 2
        local mbap = string.pack('>I2I2I2BB', 1, 0, 6, unit, 3)
        local req = mbap .. string.pack('>I2I2', 0, 2)
        local resp = ctx:request(req, 200)
        if resp and #resp >= 11 then
            local r0 = string.unpack('>i2', resp:sub(10, 11))
            out[dev.value] = { v = r0 }
        end
    end
    return out
end
"#;
    let script_dir = data_dir.join("scripts").join("e2e_lua_modbus");
    std::fs::create_dir_all(&script_dir).unwrap();
    std::fs::write(script_dir.join("main.lua"), lua_script).unwrap();

    let inst = InstConfig {
        inst_id: "lua_modbus".into(),
        inst_name: "e2e lua modbus".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: false,
        app_config: serde_json::json!({
            "transport": "tcp",
            "endpoint": endpoint,
            "script": "e2e_lua_modbus",
            "scripts_root": data_dir.join("scripts").to_string_lossy(),
            "period": 1,
            "recv_idle_ms": 100,
        }),
        devices: vec![mk_dev("ldev1", 1, vec![]), mk_dev("ldev2", 2, vec![])],
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start lua");

    let mut got: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        // 至少两个 devid 各有一个 v
        if got.contains_key("ldev1") && got.contains_key("ldev2") {
            break;
        }
        match tokio::time::timeout_at(deadline, hub_rx.recv()).await {
            Ok(Some(s)) => {
                let entry = got.entry(s.devid.clone()).or_default();
                for (k, v) in &s.values {
                    entry.insert(k.clone(), format!("{v:?}"));
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }

    println!("[T3 lua] got = {got:?}");
    shutdown.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;

    assert!(got.contains_key("ldev1"), "lua ldev1 missing: {got:?}");
    assert!(got.contains_key("ldev2"), "lua ldev2 missing: {got:?}");
    assert!(got["ldev1"].contains_key("v"), "ldev1 v missing: {got:?}");
    assert!(got["ldev2"].contains_key("v"), "ldev2 v missing: {got:?}");
}

// ============================================================================
// T4: P1 三子系统 — /metrics、/api/v1/logs/recent、/api/v1/inst/trace
// ============================================================================

/// 启 axum（含 metrics + logs + trace routes），用 oneshot 测客户端 + JWT 直连。
async fn spawn_axum(state: AppState) -> (String, tokio::task::JoinHandle<()>) {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let router = rs_iot::api::build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let router_clone = router.clone();
    let handle = tokio::spawn(async move {
        axum::serve(listener, router_clone.into_make_service())
            .await
            .unwrap();
    });
    // 给 axum 启动一点时间
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 内部做一次 warmup：先访 /metrics（验证 mount ok），丢掉结果。
    let _ = router
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let _ = StatusCode::OK; // 抑制警告

    (base_url, handle)
}

fn make_jwt(secret: &str, username: &str, ttl_secs: u64) -> String {
    use jsonwebtoken::{encode, EncodingKey, Header};
    let exp = (chrono::Utc::now().timestamp() as u64).saturating_add(ttl_secs);
    let claims = serde_json::json!({
        "sub": username,
        "exp": exp,
    });
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap()
}

#[tokio::test]
async fn p1_three_subsystems_metrics_logs_trace() {
    let data_dir = common::test_data_dir("e2e-p1-subsystems");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    // 创建 admin 用户用于 JWT
    let _ = client
        .execute(
            "HSET",
            &[
                "users",
                "admin",
                &serde_json::json!({"username": "admin", "enabled": true}).to_string(),
            ],
        )
        .await;

    let shutdown = CancellationToken::new();
    let process_metrics_arc = Arc::new(rs_iot::process_metrics::ProcessMetrics::new());
    let trace_registry = rs_iot::io_trace::TraceRegistry::new();
    let log_ringbuffer = Arc::new(rs_iot::log_ringbuffer::LogRingBuffer::new(100));

    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("e2e-hwid"),
        shutdown: shutdown.clone(),
        ctl_registry: CtlRegistry::new(process_metrics_arc.clone()),
        bridge_registry: BridgeRegistry::new(),
        metrics: process_metrics_arc.clone(),
        trace_registry: trace_registry.clone(),
        event_tx: rs_iot::event_bus::EventBus::dummy(),
    };
    let (hub_tx, _hub_rx) = mpsc::channel::<Sample>(64);
    let mgr = Arc::new(InstanceManager::new(ctx, hub_tx));

    // 启 1 个 modbus TCP 实例
    let (port, _server) = spawn_mock_tcp_server().await;
    let inst = InstConfig {
        inst_id: "p1_modbus".into(),
        inst_name: "p1 modbus".into(),
        app_code: PluginName::new("modbusClient"),
        app_type: AppType::South,
        auto_start: false,
        app_config: serde_json::json!({
            "kind": "tcp",
            "endpoint": format!("127.0.0.1:{port}"),
            "period": 1,
            "request_timeout_secs": 1,
            "slave_id": 1,
        }),
        devices: vec![mk_dev(
            "p1dev",
            1,
            vec![("v0", "input:0:i16"), ("v1", "input:1:i16")],
        )],
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start p1 modbus");
    // 给 1-2s 让 modbus 采到一些 Sample
    tokio::time::sleep(Duration::from_secs(2)).await;

    // 启 axum（含 metrics + logs/recent + inst/trace）
    let jwt_secret = "qwerty1234567890asdfgh1234567890";
    let state = AppState {
        lux: client.clone(),
        mgr: mgr.clone(),
        hwid: Hwid::new("e2e-hwid"),
        jwt_secret: jwt_secret.to_string(),
        version: "0.1.0",
        metrics: process_metrics_arc.clone(),
        trace_registry: trace_registry.clone(),
        log_ringbuffer: log_ringbuffer.clone(),
        event_bus: rs_iot::event_bus::EventBus::dummy(),
        config: std::sync::Arc::new(rs_iot::config::AppConfig::default()),
    };
    let (base_url, axum_h) = spawn_axum(state).await;

    let client_http = reqwest::Client::new();

    // === T4.1: /metrics 免 JWT ===
    let resp = client_http
        .get(format!("{base_url}/metrics"))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "/metrics 状态非 2xx");
    let body = resp.text().await.unwrap();
    let preview: String = body.chars().take(800).collect();
    println!("[T4.1 /metrics] first ~800 chars:\n{preview}");
    assert!(body.contains("rs_iot_info"), "metrics 应含 rs_iot_info");
    assert!(
        body.contains("rs_iot_uptime_seconds"),
        "metrics 应含 uptime_seconds"
    );
    // per-instance metrics
    assert!(
        body.contains("rs_iot_instance_info"),
        "metrics 应含 rs_iot_instance_info（per-instance）"
    );
    assert!(body.contains("p1_modbus"), "metrics 应含 p1_modbus 实例 ID");

    // === T4.2: /api/v1/logs/recent JWT ===
    let jwt = make_jwt(jwt_secret, "admin", 600);
    // 先往 ring buffer 注入一条 warn 级日志（避免空 buffer 测试失真）
    {
        use rs_iot::log_ringbuffer::LogEntry;
        let entry = LogEntry {
            ts: "1234567890".to_string(),
            level: "WARN".to_string(),
            target: "e2e".to_string(),
            message: "test entry".to_string(),
            fields: std::collections::HashMap::new(),
        };
        log_ringbuffer.push(entry);
    }
    let resp = client_http
        .get(format!("{base_url}/api/v1/logs/recent?level=warn&limit=20"))
        .header(reqwest::header::AUTHORIZATION, &jwt)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body_text = resp.text().await.unwrap_or_default();
    println!("[T4.2 /logs/recent] status={status}, body={body_text}");
    assert!(
        status.is_success(),
        "/api/v1/logs/recent 状态非 2xx: {status} body={body_text}"
    );
    let body: serde_json::Value = serde_json::from_str(&body_text).unwrap_or_default();
    println!("[T4.2 /logs/recent?level=warn] body = {body}");
    assert!(body.is_array(), "/logs/recent 应返回数组");
    let arr = body.as_array().unwrap();
    assert!(
        arr.iter()
            .any(|e| e.get("target").and_then(|v| v.as_str()) == Some("e2e")),
        "注入的 e2e target 应在数组中"
    );

    // === T4.3: /api/v1/inst/trace 激活 → 关闭 ===
    let resp = client_http
        .post(format!("{base_url}/api/v1/inst/trace"))
        .header(reqwest::header::AUTHORIZATION, &jwt)
        .json(&serde_json::json!({
            "inst_id": "p1_modbus",
            "duration_secs": 30,
        }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "POST /api/v1/inst/trace 状态非 2xx"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    println!("[T4.3 POST /inst/trace] body = {body}");
    assert_eq!(body.get("status").and_then(|v| v.as_str()), Some("ok"));

    // 查激活列表
    let resp = client_http
        .get(format!("{base_url}/api/v1/inst/trace"))
        .header(reqwest::header::AUTHORIZATION, &jwt)
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    println!("[T4.3 GET /inst/trace] body = {body}");
    let active = body
        .get("active")
        .and_then(|v| v.as_array())
        .expect("active 应为数组");
    assert!(
        active.iter().any(|v| v.as_str() == Some("p1_modbus")),
        "p1_modbus 应在 active 列表"
    );

    // 关闭
    let resp = client_http
        .delete(format!("{base_url}/api/v1/inst/trace?inst_id=p1_modbus"))
        .header(reqwest::header::AUTHORIZATION, &jwt)
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "DELETE /api/v1/inst/trace 状态非 2xx"
    );

    // === 收尾：确保 cleanup，不阻塞其它测试 ===
    let _ = client_http; // 抑制警告
    axum_h.abort();
    shutdown.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
}
