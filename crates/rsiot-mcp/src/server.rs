//! MCP server 工具声明 + risk 框架。
//!
//! 对标 dbx-mcp `server.rs`，砍掉 dbx 的 SQL/Mongo/Redis 大量工具与 6 层 sql_risk 校验。
//! rs-iot 工具是薄翻译层（调主进程 REST），risk 绑定**工具名**（工具本身就是 read/write 语义），
//! 故 6 层校验砍到 3 档（read_only / write / 关键设备禁写）。
//!
//! 所有工具共用 `backend_to_result` helper（成功包 pretty JSON Text / 失败包 error Text），
//! 工具方法体压到 2-3 行，避免 7 个工具写 7 遍 match。

use std::sync::Arc;

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars,
    tool, tool_handler, tool_router,
};
use serde::Deserialize;

use crate::backend::{BackendError, RsiotBackend};

// ============ 工具参数 structs ============

/// `rsiot_list_devices`：列设备。
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListDevicesRequest {
    #[schemars(description = "Optional device ID filter; omit to list all devices")]
    pub dev_id: Option<String>,
}

/// `rsiot_read_tags`：读实时数据。
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadTagsRequest {
    #[schemars(description = "Optional device ID; omit to read all devices' latest values")]
    pub dev_id: Option<String>,
}

/// `rsiot_get_recent_logs`：取最近日志。
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetRecentLogsRequest {
    #[schemars(description = "Filter by minimum level, e.g. \"warn\" returns error+warn")]
    pub level: Option<String>,
    #[schemars(description = "Filter by exact tracing target, e.g. \"io_trace\"")]
    pub target: Option<String>,
    #[schemars(description = "Max entries to return (default 200)")]
    pub limit: Option<usize>,
}

/// 无参工具的占位 struct（rmcp `Parameters<T>` 要求一个具体类型）。
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NoParams {}

/// `rsiot_send_device_command`：下发写命令到设备（高危写）。
///
/// 对标 rs-iot `CommandReq`（`src/api/routes_command.rs:17`）：`data` 是嵌套 JSON 文本，
/// 由具体南向插件 `handle_write` 解析（opcua `{node_id,value,opc_type}` / modbus `{address,value}`）。
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SendCommandRequest {
    #[schemars(description = "Target instance ID (routing key for the write bus)")]
    pub instance: String,
    #[schemars(description = "Target device ID (business validation, not routing)")]
    pub devid: String,
    #[schemars(description = "Command type, e.g. \"write\". Plugin dispatches on this.")]
    pub cmd: String,
    #[schemars(description = "Command payload as JSON text, e.g. {\"address\":\"holding:0:f32:ABCD\",\"value\":12.34}")]
    pub data: String,
}

/// `rsiot_start_instance` / `rsiot_stop_instance`：启停实例。
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InstanceRequest {
    #[schemars(description = "Instance ID to start or stop")]
    pub inst_id: String,
}

/// `rsiot_save_lua_script`：写 lua 脚本。映射 `POST /api/v1/lua/script/save`。
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SaveScriptRequest {
    #[schemars(description = "Script directory name (e.g. \"devA\"), written to scripts/<name>/main.lua. Only [A-Za-z0-9_-].")]
    pub name: String,
    #[schemars(description = "Content of main.lua")]
    pub content: String,
}

// ============ risk 框架 ============

/// 工具风险三档（rs-iot 独有，对标 dbx-mcp `sql_risk` 但绑定工具名）。
///
/// - `ReadOnly`：查询类，agent 自动放行（对标 rmcp `readOnlyHint`，但运行时判定更灵活）。
/// - `Write`：写设备/启停实例/写脚本，走 reasonix Ask 权限卡片（工具描述声明 "requires approval"）。
/// - `CriticalWrite`：关键设备写/删实例，P1 加生产环境保护（关键设备 ID 列表禁写）。
///
/// P0 实现：risk 绑定在工具描述（"Read-only" / "requires approval"），由 reasonix 侧 Ask 模式
/// 拦截。MCP server 侧不自己拦截写操作——对标 dbx（dbx 的运行时校验在 backend 层，rs-iot 后置）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolRisk {
    ReadOnly,
    Write,
    CriticalWrite,
}

// ============ server 主体 ============

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

/// 统一包装 backend 调用结果为 `CallToolResult`（所有薄翻译工具共用）。
///
/// - 成功：JSON pretty-print 成 Text 给 agent（可读，无需结构化 schema）。
/// - 失败：错误包成 error ContentBlock（`is_error=true`），不走协议级 Err
///   （协议 Err 是参数解析失败等；backend 不可达是工具层错误，agent 应看到可读文本）。
fn backend_to_result(
    result: Result<serde_json::Value, BackendError>,
    tool_name: &str,
) -> Result<CallToolResult, rmcp::ErrorData> {
    match result {
        Ok(value) => {
            let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
            Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
        }
        Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
            "{tool_name} failed: {e}"
        ))])),
    }
}

#[tool_router]
impl RsiotMcpServer {
    // ---- 只读工具（agent 自动放行，对标 readOnlyHint）----

    /// `rsiot_list_devices`：列设备（含子采集点 children）。映射 `GET /api/v1/devices`。
    #[tool(description = "List devices registered in the rs-iot gateway, optionally filtered by dev_id. Returns device list with collection points (children). Read-only.")]
    async fn rsiot_list_devices(
        &self,
        Parameters(req): Parameters<ListDevicesRequest>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let path = match &req.dev_id {
            Some(id) => format!("/api/v1/device/get?dev_id={id}"),
            None => "/api/v1/devices".to_string(),
        };
        backend_to_result(self.backend.get(&path).await, "rsiot_list_devices")
    }

    /// `rsiot_read_tags`：读设备实时数据。映射 `GET /api/v1/rtdatas?dev_id=`。
    #[tool(description = "Read latest values of device tags from the gateway. Pass dev_id for a single device, or omit for all devices. Read-only.")]
    async fn rsiot_read_tags(
        &self,
        Parameters(req): Parameters<ReadTagsRequest>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let path = match &req.dev_id {
            Some(id) => format!("/api/v1/rtdatas?dev_id={id}"),
            None => "/api/v1/rtdatas".to_string(),
        };
        backend_to_result(self.backend.get(&path).await, "rsiot_read_tags")
    }

    /// `rsiot_get_status`：网关综合状态（实例/计数器/CPU/mem）。映射 `GET /api/v1/sysstatus`。
    #[tool(description = "Get gateway overall status: running instances, pipeline/hub/mqtt counters, process CPU/memory. Read-only.")]
    async fn rsiot_get_status(
        &self,
        Parameters(_): Parameters<NoParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        backend_to_result(self.backend.get("/api/v1/sysstatus").await, "rsiot_get_status")
    }

    /// `rsiot_list_instances`：列实例状态。映射 `GET /api/v1/inststatus`。
    #[tool(description = "List all gateway plugin instances with their running state (Running/Stopped/Failed etc). Read-only.")]
    async fn rsiot_list_instances(
        &self,
        Parameters(_): Parameters<NoParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        backend_to_result(self.backend.get("/api/v1/inststatus").await, "rsiot_list_instances")
    }

    /// `rsiot_get_recent_logs`：取最近日志。映射 `GET /api/v1/logs/recent`。
    #[tool(description = "Get recent structured log entries from the gateway ring buffer. Filter by level (e.g. \"warn\" = error+warn), tracing target (e.g. \"io_trace\" for packet trace), and limit. Read-only.")]
    async fn rsiot_get_recent_logs(
        &self,
        Parameters(req): Parameters<GetRecentLogsRequest>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        // 拼 query：只拼非空参数，避免尾随 &。
        let mut qs: Vec<String> = Vec::new();
        if let Some(l) = &req.level {
            qs.push(format!("level={l}"));
        }
        if let Some(t) = &req.target {
            qs.push(format!("target={t}"));
        }
        if let Some(n) = req.limit {
            qs.push(format!("limit={n}"));
        }
        let path = if qs.is_empty() {
            "/api/v1/logs/recent".to_string()
        } else {
            format!("/api/v1/logs/recent?{}", qs.join("&"))
        };
        backend_to_result(self.backend.get(&path).await, "rsiot_get_recent_logs")
    }

    // ---- 写工具（reasonix Ask 权限模式拦截，工具描述声明 requires approval）----
    // P0 risk 实现：MCP server 侧不拦截写——由 reasonix 侧 Ask 模式按工具描述决定是否弹审批。
    // CriticalWrite 框架（关键设备禁写）P1 在 backend 层加，对标 dbx production_safety。

    /// `rsiot_send_device_command`：下发写命令。映射 `POST /api/v1/command/send`。
    ///
    /// **高危**：直接写设备寄存器/节点。data 由插件解析（modbus `{address,value}` / opcua
    /// `{node_id,value,opc_type}`）。实例未运行返回 404。
    #[tool(description = "Send a write command to a device via its running instance (e.g. write a Modbus register or OPC UA node). HIGH RISK: modifies device state. The `data` field is plugin-specific JSON (e.g. {\"address\":\"holding:0:f32\",\"value\":12.5}). Requires approval.")]
    async fn rsiot_send_device_command(
        &self,
        Parameters(req): Parameters<SendCommandRequest>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        // 对标 rs-iot CommandReq：data 是 JSON 文本字符串（嵌套），非内联 object。
        let body = serde_json::json!({
            "instance": req.instance,
            "devid": req.devid,
            "cmd": req.cmd,
            "data": req.data,
        });
        backend_to_result(self.backend.post("/api/v1/command/send", &body).await, "rsiot_send_device_command")
    }

    /// `rsiot_start_instance`：启动实例。映射 `POST /api/v1/startinst`。
    #[tool(description = "Start a south (source) instance by inst_id. The instance begins collecting from its configured devices. Requires approval.")]
    async fn rsiot_start_instance(
        &self,
        Parameters(req): Parameters<InstanceRequest>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let body = serde_json::json!({ "inst_id": req.inst_id });
        backend_to_result(self.backend.post("/api/v1/startinst", &body).await, "rsiot_start_instance")
    }

    /// `rsiot_stop_instance`：停止实例。映射 `POST /api/v1/stopinst`。
    #[tool(description = "Stop a running instance by inst_id. Its collection tasks are cancelled. Requires approval.")]
    async fn rsiot_stop_instance(
        &self,
        Parameters(req): Parameters<InstanceRequest>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let body = serde_json::json!({ "inst_id": req.inst_id });
        backend_to_result(self.backend.post("/api/v1/stopinst", &body).await, "rsiot_stop_instance")
    }

    /// `rsiot_save_lua_script`：写 lua 脚本。映射 `POST /api/v1/lua/script/save`。
    ///
    /// **写操作**：修改 scripts/ 文件树（路径白名单由 rs-iot 主进程强制）。
    /// 写后如需生效，调 `rsiot_start_instance` 或靠 luaClient notify 热加载。
    #[tool(description = "Write a lua collection script to scripts/<name>/main.lua. After saving, start/reload the luaClient instance to apply. Requires approval.")]
    async fn rsiot_save_lua_script(
        &self,
        Parameters(req): Parameters<SaveScriptRequest>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let body = serde_json::json!({ "name": req.name, "content": req.content });
        backend_to_result(
            self.backend.post("/api/v1/lua/script/save", &body).await,
            "rsiot_save_lua_script",
        )
    }

    /// `rsiot_get_config`：读脱敏配置。映射 `GET /api/v1/config`。
    ///
    /// 返回的配置已脱敏（jwt_secret/mqtt 凭据遮蔽为 "***"），agent 可安全读取做诊断。
    #[tool(description = "Read the gateway configuration with secrets masked (jwt_secret and mqtt credentials shown as \"***\"). Read-only.")]
    async fn rsiot_get_config(
        &self,
        Parameters(_): Parameters<NoParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        backend_to_result(self.backend.get("/api/v1/config").await, "rsiot_get_config")
    }
}

#[tool_handler]
impl rmcp::ServerHandler for RsiotMcpServer {}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::{json, Value};
    use std::sync::Mutex;

    /// 假 backend：可预设 get 返回值 + 记录收到的 path/body（验证工具拼的 URL 与 body 正确）。
    struct FakeBackend {
        get_value: Value,
        post_value: Value,
        paths: Mutex<Vec<String>>,
        posts: Mutex<Vec<(String, Value)>>,
    }

    impl FakeBackend {
        fn new(get_value: Value) -> Self {
            Self {
                get_value,
                post_value: Value::Null,
                paths: Mutex::new(Vec::new()),
                posts: Mutex::new(Vec::new()),
            }
        }
        /// 取出收到的所有 path（get + post）。
        fn recorded_paths(&self) -> Vec<String> {
            self.paths.lock().expect("paths lock").clone()
        }
        /// 取出收到的所有 (path, body) post 调用（验证写工具拼的 body）。
        fn recorded_posts(&self) -> Vec<(String, Value)> {
            self.posts.lock().expect("posts lock").clone()
        }
    }

    #[async_trait]
    impl RsiotBackend for FakeBackend {
        async fn get(&self, path: &str) -> Result<Value, BackendError> {
            self.paths.lock().expect("paths lock").push(path.to_string());
            Ok(self.get_value.clone())
        }
        async fn post(&self, path: &str, body: &Value) -> Result<Value, BackendError> {
            self.paths.lock().expect("paths lock").push(path.to_string());
            self.posts.lock().expect("posts lock").push((path.to_string(), body.clone()));
            Ok(self.post_value.clone())
        }
    }

    /// 始终失败的 backend（测错误路径）。
    struct ErrBackend;
    #[async_trait]
    impl RsiotBackend for ErrBackend {
        async fn get(&self, _: &str) -> Result<Value, BackendError> {
            Err(BackendError::Unreachable("connection refused".into()))
        }
        async fn post(&self, _: &str, _: &Value) -> Result<Value, BackendError> {
            Err(BackendError::Unreachable("connection refused".into()))
        }
    }

    /// 辅助：从 CallToolResult 取 Text 内容（成功路径断言用）。
    fn text_of(result: &CallToolResult) -> &str {
        match &result.content[0] {
            ContentBlock::Text(t) => &t.text,
            other => panic!("expected Text, got {other:?}"),
        }
    }

    // ---- list_devices（原工具，重构后用 helper，回归测试）----

    #[tokio::test]
    async fn list_devices_returns_backend_json_as_text() {
        let backend = Arc::new(FakeBackend::new(json!([{ "dev_id": "devA" }])));
        let server = RsiotMcpServer::new(backend);
        let result = server
            .rsiot_list_devices(Parameters(ListDevicesRequest { dev_id: None }))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        assert!(text_of(&result).contains("devA"));
    }

    #[tokio::test]
    async fn list_devices_with_dev_id_hits_device_get() {
        let backend = Arc::new(FakeBackend::new(json!({"dev_id": "devA"})));
        let server = RsiotMcpServer::new(backend.clone() as Arc<dyn RsiotBackend>);
        server
            .rsiot_list_devices(Parameters(ListDevicesRequest {
                dev_id: Some("devA".into()),
            }))
            .await
            .unwrap();
        // 验证意图：带 dev_id 走单设备端点 device/get，不带走列表端点 devices。
        // 测试侧保留 Arc<FakeBackend>（server 持 trait object 看不到具体方法）。
        let paths = backend.recorded_paths();
        assert!(
            paths.iter().any(|p| p.contains("/api/v1/device/get?dev_id=devA")),
            "dev_id filter should hit device/get; got {paths:?}"
        );
    }

    // ---- read_tags ----

    #[tokio::test]
    async fn read_tags_all_devices_hits_rtdatas_no_query() {
        let backend = Arc::new(FakeBackend::new(json!({"devA": {"v1": 1}})));
        let server = RsiotMcpServer::new(backend.clone() as Arc<dyn RsiotBackend>);
        let result = server
            .rsiot_read_tags(Parameters(ReadTagsRequest { dev_id: None }))
            .await
            .unwrap();
        assert!(text_of(&result).contains("devA"));
        // 验证意图：无 dev_id 时不带 query（避免尾随 ?）。
        assert_eq!(backend.recorded_paths()[0], "/api/v1/rtdatas", "no trailing ? when no params");
    }

    #[tokio::test]
    async fn read_tags_with_dev_id_appends_query() {
        let backend = Arc::new(FakeBackend::new(json!({"v1": 1})));
        let server = RsiotMcpServer::new(backend.clone() as Arc<dyn RsiotBackend>);
        server
            .rsiot_read_tags(Parameters(ReadTagsRequest {
                dev_id: Some("devB".into()),
            }))
            .await
            .unwrap();
        assert_eq!(backend.recorded_paths()[0], "/api/v1/rtdatas?dev_id=devB");
    }

    // ---- get_status / list_instances（无参工具）----

    #[tokio::test]
    async fn get_status_hits_sysstatus() {
        let backend = Arc::new(FakeBackend::new(json!({"status": "ok"})));
        let server = RsiotMcpServer::new(backend.clone() as Arc<dyn RsiotBackend>);
        let result = server
            .rsiot_get_status(Parameters(NoParams {}))
            .await
            .unwrap();
        assert!(text_of(&result).contains("ok"));
        assert_eq!(backend.recorded_paths()[0], "/api/v1/sysstatus");
    }

    #[tokio::test]
    async fn list_instances_hits_inststatus() {
        let backend = Arc::new(FakeBackend::new(json!([{"inst_id": "i1"}])));
        let server = RsiotMcpServer::new(backend.clone() as Arc<dyn RsiotBackend>);
        server
            .rsiot_list_instances(Parameters(NoParams {}))
            .await
            .unwrap();
        assert_eq!(backend.recorded_paths()[0], "/api/v1/inststatus");
    }

    // ---- get_recent_logs（query 拼接逻辑）----

    #[tokio::test]
    async fn recent_logs_no_params_hits_bare_path() {
        let backend = Arc::new(FakeBackend::new(json!([])));
        let server = RsiotMcpServer::new(backend.clone() as Arc<dyn RsiotBackend>);
        server
            .rsiot_get_recent_logs(Parameters(GetRecentLogsRequest {
                level: None,
                target: None,
                limit: None,
            }))
            .await
            .unwrap();
        assert_eq!(
            backend.recorded_paths()[0],
            "/api/v1/logs/recent",
            "no params = no query string"
        );
    }

    #[tokio::test]
    async fn recent_logs_assembles_query_with_amp_separators() {
        let backend = Arc::new(FakeBackend::new(json!([])));
        let server = RsiotMcpServer::new(backend.clone() as Arc<dyn RsiotBackend>);
        server
            .rsiot_get_recent_logs(Parameters(GetRecentLogsRequest {
                level: Some("warn".into()),
                target: Some("io_trace".into()),
                limit: Some(50),
            }))
            .await
            .unwrap();
        let path = &backend.recorded_paths()[0];
        // 验证意图：三参数都拼上，& 分隔，无尾随 &。
        assert!(path.starts_with("/api/v1/logs/recent?"), "got {path}");
        assert!(path.contains("level=warn"), "got {path}");
        assert!(path.contains("target=io_trace"), "got {path}");
        assert!(path.contains("limit=50"), "got {path}");
        assert!(!path.contains("&&"), "no double ampersand; got {path}");
    }

    // ---- send_device_command（写工具，body 拼接 + data 嵌套保留）----

    #[tokio::test]
    async fn send_command_posts_correct_body() {
        let backend = Arc::new(FakeBackend::new(json!(null)));
        let server = RsiotMcpServer::new(backend.clone() as Arc<dyn RsiotBackend>);
        server
            .rsiot_send_device_command(Parameters(SendCommandRequest {
                instance: "inst1".into(),
                devid: "devA".into(),
                cmd: "write".into(),
                data: r#"{"address":"holding:0:f32:ABCD","value":12.5}"#.into(),
            }))
            .await
            .unwrap();
        let posts = backend.recorded_posts();
        assert_eq!(posts.len(), 1);
        let (path, body) = &posts[0];
        assert_eq!(path, "/api/v1/command/send");
        // 验证意图（规则7）：data 是 JSON 文本字符串（对标 rs-iot CommandReq.data），
        // 不被二次反序列化——agent 传啥原样透传，插件层自解。
        assert_eq!(body["instance"], "inst1");
        assert_eq!(body["devid"], "devA");
        assert_eq!(body["cmd"], "write");
        assert_eq!(
            body["data"],
            r#"{"address":"holding:0:f32:ABCD","value":12.5}"#,
            "data must stay as JSON text string, not parsed object"
        );
    }

    // ---- start/stop_instance（写工具，inst_id body）----

    #[tokio::test]
    async fn start_instance_posts_inst_id() {
        let backend = Arc::new(FakeBackend::new(json!(null)));
        let server = RsiotMcpServer::new(backend.clone() as Arc<dyn RsiotBackend>);
        server
            .rsiot_start_instance(Parameters(InstanceRequest {
                inst_id: "i9".into(),
            }))
            .await
            .unwrap();
        let (path, body) = &backend.recorded_posts()[0];
        assert_eq!(path, "/api/v1/startinst");
        assert_eq!(body["inst_id"], "i9");
    }

    #[tokio::test]
    async fn stop_instance_posts_inst_id() {
        let backend = Arc::new(FakeBackend::new(json!(null)));
        let server = RsiotMcpServer::new(backend.clone() as Arc<dyn RsiotBackend>);
        server
            .rsiot_stop_instance(Parameters(InstanceRequest {
                inst_id: "i9".into(),
            }))
            .await
            .unwrap();
        let (path, body) = &backend.recorded_posts()[0];
        assert_eq!(path, "/api/v1/stopinst");
        assert_eq!(body["inst_id"], "i9");
    }

    // ---- save_lua_script / get_config ----

    #[tokio::test]
    async fn save_script_posts_name_and_content() {
        let backend = Arc::new(FakeBackend::new(json!(null)));
        let server = RsiotMcpServer::new(backend.clone() as Arc<dyn RsiotBackend>);
        server
            .rsiot_save_lua_script(Parameters(SaveScriptRequest {
                name: "devA".into(),
                content: "function collect() end".into(),
            }))
            .await
            .unwrap();
        let (path, body) = &backend.recorded_posts()[0];
        assert_eq!(path, "/api/v1/lua/script/save");
        assert_eq!(body["name"], "devA");
        assert_eq!(body["content"], "function collect() end");
    }

    #[tokio::test]
    async fn get_config_hits_config_endpoint() {
        let backend = Arc::new(FakeBackend::new(json!({"jwt_secret": "***"})));
        let server = RsiotMcpServer::new(backend.clone() as Arc<dyn RsiotBackend>);
        let result = server
            .rsiot_get_config(Parameters(NoParams {}))
            .await
            .unwrap();
        assert!(text_of(&result).contains("***"));
        assert_eq!(backend.recorded_paths()[0], "/api/v1/config");
    }

    // ---- 错误路径（所有工具共用 backend_to_result）----

    #[tokio::test]
    async fn any_tool_returns_error_when_backend_unreachable() {
        // 用 read_tags 代表——所有工具走同一 backend_to_result，错误路径一致。
        let server = RsiotMcpServer::new(Arc::new(ErrBackend));
        let result = server
            .rsiot_read_tags(Parameters(ReadTagsRequest { dev_id: None }))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true), "should be error result");
        assert!(text_of(&result).contains("rsiot_read_tags failed"));
    }
}
