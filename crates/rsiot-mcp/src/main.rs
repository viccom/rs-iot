//! rsiot-mcp 二进制入口。
//!
//! 被 reasonix 经 `[[plugins]]` spawn（stdio NDJSON），或本地集成测试手动起。
//! 环境变量配置（reasonix.toml 的 `[[plugins]].env` 注入）：
//! - `RSIOT_BASE`：rs-iot 主进程基址，缺省 `http://127.0.0.1:8790`。
//! - `RSIOT_JWT`：预签发 JWT（`Authorization: Bearer`）。部署时主进程签发一次写入。

use std::sync::Arc;

use rmcp::ServiceExt;
use rsiot_mcp::{RiskPolicy, RsiotMcpServer, WebBackend};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = std::env::var("RSIOT_BASE").unwrap_or_else(|_| "http://127.0.0.1:8790".into());
    let jwt = std::env::var("RSIOT_JWT").unwrap_or_default();
    let backend =
        Arc::new(WebBackend::new(base, jwt).map_err(|e| std::io::Error::other(e.to_string()))?);
    // CriticalWrite 硬闸从环境变量构造（缺省放行给 serve 审批；RSIOT_MCP_DENY_CRITICAL_WRITES=1 硬拦）。
    let service = RsiotMcpServer::new(backend, RiskPolicy::from_env())
        .serve(rmcp::transport::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}
