pub mod alarms;
pub mod dev_registry;
pub mod ext_token;
pub mod identity;
pub mod luakv;
pub mod rtdata;
pub mod secrets;
pub mod stat;

use crate::config::AppConfig;
use lux::{ServerConfig, StorageConfig, StorageMode};
use std::path::Path;
use std::time::Duration;

/// 单元测试用的 lux client 包装。
///
/// lux `3f75c17`（shutdown durability barrier）起，丢弃 `ServerHandle` 会让
/// `shutdown_rx` 的 Sender 全部释放 → server 判定关闭 → `accepting_work=false`，
/// 之后所有命令返回 `SERVER shutting down; new work is not accepted`。官方
/// embedded 用法（README）要求把 handle 持有到显式 `shutdown_and_wait()`。
///
/// 旧测试辅助函数只返回 `handle.client()` 并把 handle 当场丢弃；部分辅助函数
/// （`lua_host_ctx` 等）只把 clone 出的 client 塞进 `HostContext`，包装本体同样
/// 活不过函数返回。故 `new` 把 handle 注册到进程级表中持有到测试进程结束——
/// 无论包装本体是否被丢弃，server 都保持接单。测试进程退出时表随进程回收。
/// `Deref<Target = EmbeddedClient>` 保持所有既有调用点零改动。仅 `#[cfg(test)]`
/// 构建存在，不进 release 产物。
#[cfg(test)]
pub struct TestLux {
    client: lux::EmbeddedClient,
}

#[cfg(test)]
static TEST_LUX_HANDLES: std::sync::Mutex<Vec<lux::ServerHandle>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
impl std::fmt::Debug for TestLux {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TestLux { .. }")
    }
}

#[cfg(test)]
impl TestLux {
    /// 从 `ServerHandle` 构造：把 handle 注册进进程级表持有到测试结束，
    /// 返回只暴露 client 的包装（避免 handle 被回收后 server 停止接单）。
    pub fn new(handle: lux::ServerHandle) -> Self {
        let client = handle.client();
        TEST_LUX_HANDLES
            .lock()
            .expect("test lux handle registry poisoned")
            .push(handle);
        Self { client }
    }

    /// 供需要按值拥有 `EmbeddedClient` 的调用点（如 `HostContext.lux`）取克隆。
    /// 克隆出的 client 与本体共享同一 runtime（lux `EmbeddedClient::clone` 语义）。
    pub fn client(&self) -> lux::EmbeddedClient {
        self.client.clone()
    }
}

#[cfg(test)]
impl std::ops::Deref for TestLux {
    type Target = lux::EmbeddedClient;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

/// 为 lux 启动准备 state 目录：预建根目录并收紧到 owner-only。
///
/// lux 自 `827d1dd` 起在 `ensure_dir` 中校验 state 路径，`mode & 0o022 != 0` 即拒
/// （`unsafe state path: directory is writable by another user`）。本地 lux 补丁
/// （`src/file_security.rs::create_dir_all_private`）已改为用 `DirBuilder::mode(0o700)`
/// 创建缺失目录（含 `shard_*/journal/storage` 子目录），不再受进程 umask 影响。
///
/// 本函数保留为**调用侧防御**：预建根目录并显式 chmod 0o700，使旧版 lux
/// （无补丁）在根目录一层也能通过，同时避免在进程级改 umask（旧实现曾
/// `umask(0o077)`，会影响本进程后续所有文件创建，副作用过大）。
pub fn prepare_lux_data_dir(path: impl AsRef<Path>) -> std::io::Result<()> {
    let path = path.as_ref();
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

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
/// - 禁用 lux 自带 HTTP（`http_port = 0`），管理面用我们自己的 axum :8790
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
    // lux 827d1dd 起校验 state 目录必须 owner-only（详见 prepare_lux_data_dir）。
    // run_app 生产路径已 create_dir_all，此处补齐权限收紧；测试辅助函数直接调
    // 本函数时亦一并满足前置条件。
    prepare_lux_data_dir(&cfg.lux.data_dir).unwrap_or_else(|e| {
        panic!("prepare lux data_dir {}: {e}", cfg.lux.data_dir);
    });
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

// ---------- lux Hash 读写 helper（审计 B4 自 api/mod.rs 下沉） ----------
// 原定义于 api 层导致 model::migrate / lua_north::compat 反向 use crate::api
//（全仓仅有的 2 个循环依赖环根因）——db 是 lux 值转换的应许之家。

/// RESP 单值 → String（Bulk/Simple/Int 直映，其余空串）。
pub fn str_from_value(v: &lux::EmbeddedValue) -> String {
    match v {
        lux::EmbeddedValue::Bulk(b) => String::from_utf8_lossy(b).to_string(),
        lux::EmbeddedValue::Simple(s) => s.clone(),
        lux::EmbeddedValue::Int(i) => i.to_string(),
        _ => String::new(),
    }
}

/// `HGETALL key` → JSON 对象（每个 value 尝试解析为 JSON，失败保留原始字符串）。
pub async fn hgetall(lux: &lux::EmbeddedClient, key: &str) -> serde_json::Value {
    match lux.execute_value("HGETALL", &[key]).await {
        Ok(lux::EmbeddedValue::Array(arr)) => {
            // 奇数长度 = field/value 对不完整（lux HGETALL 正常恒偶数）——尾部
            // 元素将被丢弃，静默吞不如留痕（审查 Phase2-L4）。
            if arr.len() % 2 != 0 {
                tracing::warn!(
                    "[db] HGETALL {key} 返回奇数长度 {}，尾部元素丢弃",
                    arr.len()
                );
            }
            let mut map = serde_json::Map::new();
            let mut i = 0;
            while i + 1 < arr.len() {
                let k = str_from_value(&arr[i]);
                let raw = str_from_value(&arr[i + 1]);
                let parsed = serde_json::from_str(&raw).unwrap_or(serde_json::Value::String(raw));
                map.insert(k, parsed);
                i += 2;
            }
            serde_json::Value::Object(map)
        }
        _ => serde_json::Value::Object(serde_json::Map::new()),
    }
}

/// `HGET key field` → Option<String>（Nil → None）。
pub async fn hget(lux: &lux::EmbeddedClient, key: &str, field: &str) -> Option<String> {
    match lux.execute_value("HGET", &[key, field]).await {
        Ok(v) if !matches!(v, lux::EmbeddedValue::Nil) => Some(str_from_value(&v)),
        _ => None,
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// lux 827d1dd 起要求 state 目录 owner-only；helper 必须建出 0o700 根目录。
    /// 子目录（`shard_*/journal/storage`）由 lux 本地补丁
    /// `create_dir_all_private` 以 0o700 创建（见 Cargo.toml 注释），本测不重复断言。
    #[test]
    fn prepare_lux_data_dir_is_owner_only() {
        let dir =
            std::env::temp_dir().join(format!("rsiot-prepare-lux-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        prepare_lux_data_dir(&dir).expect("prepare");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o022,
                0,
                "state dir must not be group/other writable"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

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
