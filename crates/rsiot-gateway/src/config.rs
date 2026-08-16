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

/// 供应商管理段（可选）：指向 reasonix 的 REASONIX_HOME 与 warden。
/// 缺省（空 home_dir）时 /api/agent/providers* 返 503（现场未启用该能力）。
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(default)]
pub struct ReasonixCfg {
    /// REASONIX_HOME 目录（其下 config.toml + .env 由本网关外科手术式编辑）。
    pub home_dir: String,
    /// warden REST 基址（改配置后重启 reasonix 服务，serve 启动时快照配置）。
    pub warden_url: String,
    /// warden 中的 reasonix 服务名。
    pub service_name: String,
    /// reasonix 工作区根（@ 引用解析锚点，即 serve 的 workspaceRoot）。
    /// 附件上传落 `<workspace_dir>/.reasonix/attachments/`，工作区浏览以此为沙箱。
    /// 须与 warden 里 reasonix 服务的 working_dir 一致；若其上是 git 仓库则应填
    /// git 根（reasonix 取最近 git 根为 workspaceRoot，boot.go resolveWorkspaceRoot）。
    /// 缺省（空）时 /api/agent/attachments、/api/agent/workspace 返 503。
    pub workspace_dir: String,
}

impl ReasonixCfg {
    /// 服务名缺省值。
    pub fn service_name(&self) -> &str {
        if self.service_name.is_empty() {
            "reasonix"
        } else {
            &self.service_name
        }
    }
}

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
    /// 供应商管理段（可选）。
    #[serde(default)]
    pub reasonix: ReasonixCfg,
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
