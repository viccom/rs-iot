//! 集成测试公共 helper。
//!
//! 所有测试用**独立 data_dir + 端口 0（OS 自动分配）**启动 lux，
//! 保证测试并行不冲突、不污染仓库 `./data/`。
//!
//! 本模块是全部测试 binary 的共享工具箱：各 binary 按需引用部分函数，未引用
//! 方在 dead_code 检测下呈"未使用"（每 binary 独立编译判定——expect 会在
//! 使用方 binary 误报 unused expectation，故文件级 allow；生产代码不适用本
//! 惯例，见 CLAUDE.md 编码约定）。
#![allow(dead_code)]

use rs_iot::config::{AppConfig, LuxConfig};
use std::path::PathBuf;

/// 为测试创建独立 data_dir（临时目录，按 name 隔离）。
///
/// 注意：返回**固定名**目录，供需要跨调用复用同一目录的套件使用（real_chain 等）。
/// 若同一测试二进制内多个测试各起 lux 且共用此目录，lux v0.37.1 起会撞持久化
/// 目录锁（already in use or cannot be locked）——这类场景改用 [`unique_test_data_dir`]。
pub fn test_data_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rsiot-it-{name}"))
}

/// 唯一 data_dir（pid + 纳秒）：同一二进制内多测试并行各起 lux 时用，避免撞
/// lux 的持久化目录锁。照 alarm_e2e / history_visibility_e2e 既有惯例。
pub fn unique_test_data_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    test_data_dir(&format!("{name}-{}-{nanos}", std::process::id()))
}

/// 启动测试 lux 实例：端口 0 由 OS 分配（见 lux `ServerConfig::port` 文档），
/// data_dir 指向临时目录。返回 (handle, client)。
pub async fn start_lux(data_dir: &str) -> (lux::ServerHandle, lux::EmbeddedClient) {
    let cfg = AppConfig {
        lux: LuxConfig {
            port: 0,
            data_dir: data_dir.to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    let lux_cfg = rs_iot::db::build_lux_config(&cfg);
    let handle = lux::run_with_config(lux_cfg)
        .await
        .expect("test lux should start");
    let client = handle.client();
    (handle, client)
}

/// 起进程内 rumqttd（明文 TCP v4，127.0.0.1 随机端口），返回监听端口。
///
/// 复制自 `lua_mqtt_pass_e2e`（tests/ 无法复用 src 内 `cfg(test)` helper；
/// rumqttd 是 dev-dep，仅测试构建可见）。端口先 bind `:0` 取空闲口再释放交给
/// rumqttd，就绪探测 TCP 连通即返回（5s 上限，失败 panic）。
/// rumqttd 0.20 的 `Broker::start` 无停机口，broker 线程随测试进程结束回收。
pub fn spawn_inproc_broker(tag: &str) -> u16 {
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
                    name: tag.to_string(),
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
    let tag = tag.to_string();
    std::thread::Builder::new()
        .name(format!("rumqttd-{tag}"))
        .spawn(move || {
            if let Err(e) = broker.start() {
                eprintln!("[{tag}] rumqttd start 失败：{e:?}");
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

/// 优雅关闭 lux（内部先 SAVE 再 shutdown）。**不清理 data_dir**——临时目录的清理
/// 责任在调用方：复用目录的套件（real_chain 模式）起跑前先整删（见
/// lua_transform_e2e `real_chain` 先例）；且 Windows 上 lux 句柄释放有延迟，关闭后
/// 立即删除也可能失败，故不在本 helper 内顺带删。
pub async fn stop_lux(handle: lux::ServerHandle) {
    let _ = rs_iot::db::shutdown_lux(handle).await;
}

/// M-2 模型驱动建实例 helper：旧形态点表（DeviceSpec）注册为隐式模型 + devreg
/// 设备，返回绑定列表（实例 `devices` 字段直接使用）。
///
/// 内部复用 `model::migrate::convert`（tag 名/地址语义与生产迁移完全一致：
/// 地址类插件[modbus/opcua/s7/eip] addr 原样 + tag 名=label；纯软 addr=None）。
/// 注册后 effective 缓存自动刷新（store 写路径）。
pub async fn register_devices_as_models(
    client: &lux::EmbeddedClient,
    inst_id: &str,
    app_code: &str,
    devices: Vec<rs_iot::types::DeviceSpec>,
) -> Vec<rs_iot::types::DeviceBinding> {
    use rs_iot::model::migrate::{convert, LegacyDevice, LegacyInst};
    let legacy = LegacyInst {
        inst_id: inst_id.to_string(),
        app_code: app_code.to_string(),
        devices: devices
            .into_iter()
            .map(|d| LegacyDevice {
                value: d.value,
                children: d.children,
                slave_id: d.slave_id,
            })
            .collect(),
    };
    let outcome = convert(&legacy);
    for m in &outcome.models {
        rs_iot::model::store::set_model(client, m)
            .await
            .expect("set_model");
    }
    for d in &outcome.devices {
        rs_iot::model::store::set_device(client, d)
            .await
            .expect("set_device");
    }
    outcome.bindings
}
