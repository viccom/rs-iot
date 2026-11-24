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
use std::sync::atomic::{AtomicU64, Ordering};

/// 进程内唯一序号：`SystemTime` 分辨率不足时（同微秒内并发调用）仍保证目录名不撞。
/// 曾致 `events_e2e` 偶发 `Resource temporarily unavailable`（同 binary 内多测试
/// 同时起 lux；2026-11-01 修复）。
static DIR_SEQ: AtomicU64 = AtomicU64::new(0);

/// 为测试创建独立 data_dir（临时目录，按 name 隔离）。
///
/// lux ≥0.37.1 对 data_dir 持排他锁——同名目录并发打开直接 AlreadyExists 拒启。
/// 因此每次调用都拼 pid + 纳秒时钟 + 进程内序号保证唯一（时钟分辨率不足时序号兜底）；
/// 同测试内跨 start/stop 重启复用**同一 PathBuf 变量**（kill 恢复/持久化类用例的既有
/// 惯例），不重复调用本函数。
pub fn test_data_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "rsiot-it-{name}-{}-{nanos}-{seq}",
        std::process::id()
    ))
}
/// [`test_data_dir`] 的别名（master 线测试如 `lua_api_e2e` 的调用名；
/// ours 实现本就每次调用唯一，语义等价直接转发）。
pub fn unique_test_data_dir(name: &str) -> PathBuf {
    test_data_dir(name)
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
