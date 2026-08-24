pub mod alarms;
pub mod dev_registry;
pub mod ext_token;
pub mod identity;
pub mod luakv;
pub mod rtdata;
pub mod secrets;
pub mod stat;
pub mod tag_registry;

use crate::config::AppConfig;
use lux::{ServerConfig, StorageConfig, StorageMode};
use std::time::Duration;

/// D-LUX-1：非 loopback 绑定强制认证的启动校验（纯函数便于单测）。
///
/// - loopback 白名单（`127.0.0.1` / `::1` / `localhost`，大小写不敏感）→ 放行
///   （无密码单机信任模型，维持现状）
/// - 非 loopback 且 password 空 → 拒绝，错误信息含两条出路：
///   设 `[lux] password` 或改回 loopback 绑定
/// - 非 loopback 且 password 非空 → 放行（RESP 强制 AUTH）
pub fn validate_lux_bind(bind_host: &str, password: &str) -> Result<(), String> {
    let loopback = matches!(
        bind_host.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "::1" | "localhost"
    );
    if loopback || !password.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "lux RESP binds non-loopback ({bind_host}) without a password; \
             set [lux] password in config, or bind back to loopback \
             (127.0.0.1 / ::1 / localhost)"
        ))
    }
}

/// 构造 lux `ServerConfig`：
///
/// - RESP 监听 `bind_host:port`（默认 127.0.0.1:6390）；非 loopback 绑定须带
///   `[lux] password`（D-LUX-1：`validate_lux_bind` 拒绝无密码配置，主启动路径
///   `run_app` 以 Err 前置拦截，此处 panic 兜住绕过 run_app 的调用方；lux 自身
///   对非 loopback 无认证另有 `allow_insecure_no_auth` 拒绝兜底，双保险）
/// - 禁用 lux 自带 HTTP（`http_port = 0`），管理面用我们自己的 axum :8780
/// - `storage.mode = Tiered`：每写刷 WAL，真正持久化
/// - `save_interval` 保留周期快照（截断 WAL，避免无限增长）
/// - `auth.enabled = false`：JWT 由 rs-iot 自管，不用 lux 的 /auth/v1/*
///
/// `ServerConfig` 字段众多且常增减，按字段逐一赋值比维护完整字面量更稳，
/// 故以 `expect` 放行 field_reassign（若 lux 提供 Builder，可移除）。
#[expect(clippy::field_reassign_with_default)]
pub fn build_lux_config(cfg: &AppConfig) -> ServerConfig {
    if let Err(msg) = validate_lux_bind(&cfg.lux.bind_host, &cfg.lux.password) {
        panic!("lux RESP bind rejected: {msg}");
    }
    let non_loopback = !cfg.lux.password.is_empty();
    let mut sc = ServerConfig::default();
    sc.bind_host = cfg.lux.bind_host.clone();
    sc.port = cfg.lux.port;
    sc.http_port = 0;
    sc.enable_resp = true;
    // D-LUX-1：非 loopback（配置了 password）强制 AUTH；loopback 维持无密码单机信任
    sc.require_auth = non_loopback;
    sc.password = cfg.lux.password.clone();
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 照 routes_sys.rs 测试惯例：用 toml 构造配置（避免 field_reassign_with_default lint）。
    fn cfg_with(lux_toml: &str) -> crate::config::AppConfig {
        toml::from_str(lux_toml).expect("parse")
    }

    /// D-LUX-1：loopback 三形态（大小写不敏感）无密码放行。
    #[test]
    fn validate_lux_bind_loopback_forms_ok_without_password() {
        for host in ["127.0.0.1", "::1", "localhost", "LOCALHOST", "LocalHost"] {
            assert!(
                validate_lux_bind(host, "").is_ok(),
                "loopback 形态 {host} 无密码应放行"
            );
        }
    }

    /// D-LUX-1：非 loopback 无密码 → 拒绝，错误信息含两条出路（设密码/回 loopback）。
    #[test]
    fn validate_lux_bind_non_loopback_without_password_rejected() {
        for host in ["0.0.0.0", "192.168.1.10"] {
            let err = validate_lux_bind(host, "").unwrap_err();
            let msg = err.to_lowercase();
            assert!(msg.contains("password"), "错误信息须指引设置密码：{err}");
            assert!(
                msg.contains("loopback"),
                "错误信息须指引改回 loopback：{err}"
            );
        }
    }

    /// D-LUX-1：非 loopback 带密码 → 放行；loopback 带密码（显式加固）同样放行。
    #[test]
    fn validate_lux_bind_with_password_ok() {
        assert!(validate_lux_bind("0.0.0.0", "s3cret").is_ok());
        assert!(validate_lux_bind("127.0.0.1", "s3cret").is_ok());
    }

    /// D-LUX-1：非 loopback + password → 产出 ServerConfig 强制认证且密码传入。
    #[test]
    fn build_lux_config_non_loopback_requires_auth() {
        let cfg = cfg_with(
            r#"[lux]
bind_host = "0.0.0.0"
password = "s3cret""#,
        );
        let sc = build_lux_config(&cfg);
        assert!(sc.require_auth, "非 loopback 绑定必须强制认证");
        assert_eq!(sc.password, "s3cret");
    }

    /// loopback 维持现状：require_auth=false、无密码（单机信任模型）。
    #[test]
    fn build_lux_config_loopback_keeps_no_auth() {
        let sc = build_lux_config(&cfg_with(""));
        assert!(!sc.require_auth);
        assert_eq!(sc.password, "");
    }

    /// 非 loopback 无密码直接走 build_lux_config → panic（启动期兜底；
    /// 主启动路径 run_app 以 Err 提前拦截，此处兜住绕过 run_app 的调用方）。
    #[test]
    #[should_panic(expected = "lux RESP bind")]
    fn build_lux_config_non_loopback_without_password_panics() {
        let cfg = cfg_with(
            r#"[lux]
bind_host = "0.0.0.0""#,
        );
        let _ = build_lux_config(&cfg);
    }
}
