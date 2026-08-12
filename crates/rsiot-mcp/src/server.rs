//! MCP server 工具声明 + 三档 risk 校验。
//!
//! 对标 dbx-mcp `server.rs`，砍掉 dbx 的 SQL/Mongo/Redis 大量工具与 6 层 sql_risk 校验。
//! rs-iot 工具是薄翻译层（调主进程 REST），risk 绑定**工具名**（工具本身就是 read/write 语义），
//! 故 6 层校验砍到 3 档（read_only / write / 关键设备禁写）。
//!
//! 本骨架先实现 1 个工具 `rsiot_list_devices`（P0-A A3：跑通宏链路），后续工具
//! （read_tags/write_tags/get_logs/send_command 等）下一会话扩。

use std::sync::Arc;

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars,
    tool, tool_handler, tool_router,
};
use serde::Deserialize;

use crate::backend::RsiotBackend;

/// `rsiot_list_devices` 工具：列 rs-iot 已注册设备（含子采集点 children）。
///
/// 映射 `GET /api/v1/devices`（rs-iot `src/api/mod.rs:111`）。只读，agent 调用无需批准。
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListDevicesRequest {
    #[schemars(description = "Optional device ID filter; omit to list all devices")]
    pub dev_id: Option<String>,
}

/// 工具风险三档（rs-iot 独有，对标 dbx-mcp `sql_risk` 但绑定工具名）。
///
/// - `ReadOnly`：查询类，agent 自动放行（对标 rmcp `readOnlyHint`，但运行时判定更灵活）。
/// - `Write`：写设备/启停实例/写脚本，走 reasonix Ask 权限卡片。
/// - `CriticalWrite`：关键设备写/删实例，P1 加生产环境保护（关键设备 ID 列表禁写）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolRisk {
    ReadOnly,
    Write,
    CriticalWrite,
}

/// MCP server 主体。
///
/// 持有 `Arc<dyn RsiotBackend>`（WebBackend 注入，测试可换 FakeBackend）。
/// `#[tool_router]` 注册所有 `#[tool]` 方法；`#[tool_handler]` 自动生成
/// `call_tool`/`list_tools`/`get_tool`/`get_info`（rmcp 官方模式，见 docs.rs/rmcp）。
pub struct RsiotMcpServer {
    backend: Arc<dyn RsiotBackend>,
}

impl RsiotMcpServer {
    /// 构造 server，注入 backend。
    pub fn new(backend: Arc<dyn RsiotBackend>) -> Self {
        Self { backend }
    }
}

#[tool_router]
impl RsiotMcpServer {
    /// `rsiot_list_devices`：列设备。
    ///
    /// 返回 `CallToolResult`（rmcp 工具统一返回类型）。成功把 rs-iot `/devices` 响应 JSON
    /// 包成 `ContentBlock::Text`（agent 友好的可读文本，非裸 JSON struct）。
    #[tool(description = "List devices registered in the rs-iot gateway, optionally filtered by dev_id. Returns device list with collection points (children). Read-only.")]
    async fn rsiot_list_devices(
        &self,
        Parameters(req): Parameters<ListDevicesRequest>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let path = match &req.dev_id {
            Some(id) => format!("/api/v1/device/get?dev_id={id}"),
            None => "/api/v1/devices".to_string(),
        };
        match self.backend.get(&path).await {
            Ok(value) => {
                // 序列化为紧凑 JSON 文本（agent 可直接读，无需结构化 schema）。
                let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
                Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
            }
            // 工具层错误：rs-iot 不可达/返错时，以文本 ContentBlock 回 agent（is_error=true），
            // 不走协议级 Err（那是协议错，如参数解析失败）。
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "rsiot_list_devices failed: {e}"
            ))])),
        }
    }
}

#[tool_handler]
impl rmcp::ServerHandler for RsiotMcpServer {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendError;
    use async_trait::async_trait;
    use serde_json::{json, Value};

    /// 假 backend：测试工具逻辑不依赖真实 rs-iot。
    struct FakeBackend {
        devices: Value,
    }

    #[async_trait]
    impl RsiotBackend for FakeBackend {
        async fn get(&self, _path: &str) -> Result<Value, BackendError> {
            Ok(self.devices.clone())
        }
        async fn post(&self, _path: &str, _body: &Value) -> Result<Value, BackendError> {
            Ok(Value::Null)
        }
    }

    /// `rsiot_list_devices` 成功路径：backend 返设备数组，工具包成 Text。
    ///
    /// 验证意图：工具层是薄翻译——不解析 rs-iot 响应结构，原样透传 JSON 文本给 agent。
    #[tokio::test]
    async fn list_devices_returns_backend_json_as_text() {
        let backend = Arc::new(FakeBackend {
            devices: json!([{ "dev_id": "devA", "children": [] }]),
        });
        let server = RsiotMcpServer::new(backend);
        let req = ListDevicesRequest { dev_id: None };
        let result = server.rsiot_list_devices(Parameters(req)).await.unwrap();

        use rmcp::model::ContentBlock;
        assert_ne!(result.is_error, Some(true), "should be success");
        let content = &result.content[0];
        match content {
            ContentBlock::Text(t) => {
                assert!(t.text.contains("devA"), "text should contain device id");
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    /// `rsiot_list_devices` 失败路径：backend 不可达，工具返 error（非 panic）。
    ///
    /// 验证意图：rs-iot 主进程离线时 agent 收到可读错误而非协议级失败。
    #[tokio::test]
    async fn list_devices_returns_error_when_backend_unreachable() {
        struct ErrBackend;
        #[async_trait]
        impl RsiotBackend for ErrBackend {
            async fn get(&self, _: &str) -> Result<Value, BackendError> {
                Err(BackendError::Unreachable("connection refused".into()))
            }
            async fn post(&self, _: &str, _: &Value) -> Result<Value, BackendError> {
                Ok(Value::Null)
            }
        }
        let server = RsiotMcpServer::new(Arc::new(ErrBackend));
        let req = ListDevicesRequest { dev_id: None };
        let result = server.rsiot_list_devices(Parameters(req)).await.unwrap();

        assert_eq!(result.is_error, Some(true), "should be error result");
    }
}
