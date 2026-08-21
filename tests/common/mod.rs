//! 集成测试公共 helper。
//!
//! 所有测试用**独立 data_dir + 端口 0（OS 自动分配）**启动 lux，
//! 保证测试并行不冲突、不污染仓库 `./data/`。

use rs_iot::config::{AppConfig, LuxConfig};
use std::path::PathBuf;

/// 为测试创建独立 data_dir（临时目录，按 name 隔离）。
pub fn test_data_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rsiot-it-{name}"))
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
