pub mod ext_token;
pub mod identity;
pub mod luakv;
pub mod rtdata;
pub mod stat;

use crate::config::AppConfig;
use lux::{ServerConfig, StorageConfig, StorageMode};
use std::time::Duration;

/// 构造 lux `ServerConfig`：
///
/// - RESP 监听 `bind_host:port`（默认 127.0.0.1:6390），仅 loopback
/// - 禁用 lux 自带 HTTP（`http_port = 0`），管理面用我们自己的 axum :8780
/// - `storage.mode = Tiered`：每写刷 WAL，真正持久化
/// - `save_interval` 保留周期快照（截断 WAL，避免无限增长）
/// - `auth.enabled = false`：JWT 由 rs-iot 自管，不用 lux 的 /auth/v1/*
///
/// `ServerConfig` 字段众多且常增减，按字段逐一赋值比维护完整字面量更稳，
/// 故以 `expect` 放行 field_reassign（若 lux 提供 Builder，可移除）。
#[expect(clippy::field_reassign_with_default)]
pub fn build_lux_config(cfg: &AppConfig) -> ServerConfig {
    let mut sc = ServerConfig::default();
    sc.bind_host = cfg.lux.bind_host.clone();
    sc.port = cfg.lux.port;
    sc.http_port = 0;
    sc.enable_resp = true;
    sc.require_auth = false; // loopback 无需密码；如需远程暴露再开启 password + require_auth
    sc.password = String::new();
    sc.auth = lux::AuthConfig::default();
    sc.data_dir = cfg.lux.data_dir.clone();
    sc.storage = StorageConfig {
        mode: StorageMode::Tiered,
        dir: cfg.lux.data_dir.clone(),
    };
    // Tiered 下 save_interval 不能为 0（否则 WAL 不被截断、无限增长）
    sc.save_interval = Duration::from_secs(cfg.lux.save_interval_secs.max(1));
    sc
}

/// 优雅关闭 lux：先 `SAVE` 刷盘，再 `shutdown_and_wait`。
///
/// 关键陷阱：lux 的 `shutdown_and_wait()` 默认**不落盘**——
/// Memory 模式会丢自上次快照以来的写入，Tiered 模式 WAL 不截断。
/// 因此关闭前必须显式 `SAVE`。
pub async fn shutdown_lux(handle: lux::ServerHandle) -> Result<(), crate::error::RsIotError> {
    let client = handle.client();
    match client.execute("SAVE", &[]).await {
        Ok(_) => tracing::info!("lux SAVE ok (snapshot flushed)"),
        Err(e) => tracing::warn!("lux SAVE failed during shutdown: {e}"),
    }
    drop(client);
    handle.shutdown_and_wait().await?;
    Ok(())
}
