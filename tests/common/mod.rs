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

/// 优雅关闭 lux（内部先 SAVE 再 shutdown），并清理临时 data_dir。
/// 清理失败忽略（Windows 上文件句柄释放可能有延迟）。
pub async fn stop_lux(handle: lux::ServerHandle) {
    let _ = rs_iot::db::shutdown_lux(handle).await;
}
