//! Gateway 配置（TOML）。
//!
//! 部署时与 rs-iot 的 config.toml 共存于运行目录。`jwt_secret` 必须与 rs-iot
//! `[api] jwt_secret` 同值（层1 复用 JWT，见 plan-gateway-tech-selection.md §四层1）。
//!
//! ```toml
//! listen = "0.0.0.0:8080"
//! rsiot_base = "http://127.0.0.1:8790"
//! serve_base = "http://127.0.0.1:8787"
//! jwt_secret = "<与 rs-iot 同值>"
//! serve_token = "<reasonix serve --auth token>"
//! ```

use serde::Deserialize;

/// Gateway 配置。
#[derive(Debug, Deserialize, Clone)]
pub struct GwConfig {
    /// 监听地址，如 `"0.0.0.0:8080"`。
    pub listen: String,
    /// rs-iot 主进程基址（loopback），如 `"http://127.0.0.1:8790"`。
    pub rsiot_base: String,
    /// reasonix serve 基址，如 `"http://127.0.0.1:8787"`。
    pub serve_base: String,
    /// JWT secret（与 rs-iot `[api] jwt_secret` 同值）。层1 复用 rs-iot JWT。
    pub jwt_secret: String,
    /// reasonix serve token（伪造 Cookie: reasonix_token 注入，层3）。
    /// 来自 serve 启动 `--auth token --token <值>` 或 `--token-file`。
    pub serve_token: String,
}

impl GwConfig {
    /// 从 TOML 字符串解析。
    pub fn parse(toml_str: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(toml_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 配置解析：所有字段必填（缺失字段启动失败，符合"显式配置"约定）。
    #[test]
    fn parses_full_config() {
        let s = r#"
listen = "0.0.0.0:8080"
rsiot_base = "http://127.0.0.1:8790"
serve_base = "http://127.0.0.1:8787"
jwt_secret = "secret"
serve_token = "tok"
"#;
        let c = GwConfig::parse(s).unwrap();
        assert_eq!(c.listen, "0.0.0.0:8080");
        assert_eq!(c.rsiot_base, "http://127.0.0.1:8790");
        assert_eq!(c.serve_token, "tok");
    }

    /// 缺失字段报错（不静默 Default——gateway 配置错不能隐式放行）。
    #[test]
    fn rejects_missing_field() {
        let s = r#"listen = "0.0.0.0:8080""#;
        assert!(GwConfig::parse(s).is_err());
    }
}
