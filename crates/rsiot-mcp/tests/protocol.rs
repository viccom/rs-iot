//! A7 rsiot-mcp 协议层端到端集成测（dbx `tests/protocol.rs` 范式）。
//!
//! 之前的单测都**直接调工具方法**（`server.rsiot_read_tags(...)`），没走 rmcp 的
//! JSON-RPC over stdio 协议层。本测试用 `tokio::io::duplex` + rmcp client 真实握手，
//! 验证 `#[tool_router]`/`#[tool_handler]` 宏生成的协议层正确：
//! - `list_tools` 暴露全部 10 个工具
//! - `call_tool` 经 JSON-RPC 往返正确（参数反序列化 + 结果序列化）
//!
//! 这是 reasonix `[[plugins]]` spawn rsiot-mcp 后真实经历的路径。

use std::sync::Arc;

use async_trait::async_trait;
use rmcp::model::CallToolRequestParams;
use rmcp::ServiceExt;
use rsiot_mcp::{RsiotBackend, RsiotMcpServer};
use serde_json::{json, Value};

/// 假 backend：所有 get 返预设 JSON，所有 post 返 {status:ok}。
/// 不依赖真实 rs-iot——本测试验证协议层，不验证 backend。
struct FakeBackend {
    get_value: Value,
}

#[async_trait]
impl RsiotBackend for FakeBackend {
    async fn get(&self, _path: &str) -> Result<Value, rsiot_mcp::BackendError> {
        Ok(self.get_value.clone())
    }
    async fn post(&self, _path: &str, _body: &Value) -> Result<Value, rsiot_mcp::BackendError> {
        Ok(json!({"status": "ok"}))
    }
    async fn delete(&self, _path: &str) -> Result<Value, rsiot_mcp::BackendError> {
        Ok(json!({"status": "ok"}))
    }
}

/// 辅助：起 server + client 经 duplex 握手，返回 client（用完 cancel 即可）。
///
/// `().serve(transport)` 返回 `RunningService<RoleClient, ()>`（rmcp 2.2 API），
/// 其 `.peer()` 取 `&Peer` 调工具，`.cancel()` 关连接。对标 dbx protocol.rs。
async fn spawn_handshake(
    get_value: Value,
) -> (
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tokio::task::JoinHandle<()>,
) {
    spawn_handshake_with_policy(get_value, rsiot_mcp::RiskPolicy::default()).await
}

/// 同 spawn_handshake，但注入硬闸策略（RSIOT_MCP_DENY_CRITICAL_WRITES=1 场景）。
async fn spawn_handshake_deny(
    get_value: Value,
) -> (
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tokio::task::JoinHandle<()>,
) {
    spawn_handshake_with_policy(get_value, rsiot_mcp::RiskPolicy::deny_all()).await
}

/// 握手公共实现（默认/deny 两种策略共用）。
async fn spawn_handshake_with_policy(
    get_value: Value,
    policy: rsiot_mcp::RiskPolicy,
) -> (
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tokio::task::JoinHandle<()>,
) {
    let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
    let server = RsiotMcpServer::new(
        Arc::new(FakeBackend { get_value }) as Arc<dyn RsiotBackend>,
        policy,
    );
    let server_task = tokio::spawn(async move {
        // serve().await 建立 server 返回 RunningService；必须接着 .waiting().await
        // 保持运行（否则 serve 返回即 drop service → client 握手 BrokenPipe）。
        match server.serve(server_transport).await {
            Ok(service) => {
                let _ = service.waiting().await;
            }
            Err(e) => eprintln!("server.serve failed: {e:?}"),
        }
    });
    let client = ().serve(client_transport).await.expect("MCP handshake");
    (client, server_task)
}

/// 协议握手 + list_tools：23 个工具全部暴露。
///
/// 验证意图（规则7）：`#[tool_router]` 宏注册的工具必须在 list_tools 出现——
/// 漏一个说明宏配置错或工具方法签名错（reasonix 会看不到该工具）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handshake_lists_all_tools() {
    let (client, server_task) = spawn_handshake(json!([])).await;
    let tools = client.peer().list_tools(None).await.expect("list_tools");
    let names: Vec<&str> = tools.tools.iter().map(|t| t.name.as_ref()).collect();

    // 23 个工具全在（漏一个 = 宏没注册，agent 调不到）。
    for expected in [
        // 既有 10 个
        "rsiot_list_devices",
        "rsiot_read_tags",
        "rsiot_get_status",
        "rsiot_list_instances",
        "rsiot_get_recent_logs",
        "rsiot_send_device_command",
        "rsiot_start_instance",
        "rsiot_stop_instance",
        "rsiot_save_lua_script",
        "rsiot_get_config",
        // 新增 13 个
        "rsiot_start_packet_trace",
        "rsiot_list_packet_trace",
        "rsiot_stop_packet_trace",
        "rsiot_get_instance_stats",
        "rsiot_reload_lua_instance",
        "rsiot_list_lua_scripts",
        "rsiot_get_instance_config",
        "rsiot_set_instance",
        "rsiot_delete_instance",
        "rsiot_set_device",
        "rsiot_delete_device",
        "rsiot_list_plugins",
        "rsiot_get_sysinfo",
    ] {
        assert!(
            names.contains(&expected),
            "tool {expected} must be listed; got {names:?}"
        );
    }
    assert_eq!(names.len(), 23, "exactly 23 tools; got {names:?}");

    let _ = client.cancel().await;
    server_task.abort();
}

/// call_tool 经 JSON-RPC 往返：只读工具（read_tags）无参调用成功。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_tool_round_trip_read_tags() {
    let (client, server_task) = spawn_handshake(json!({"devA": {"v1": [1, 2, 3.5]}})).await;
    let result = client
        .peer()
        .call_tool(CallToolRequestParams::new("rsiot_read_tags"))
        .await
        .expect("call_tool");
    assert_ne!(result.is_error, Some(true), "should succeed");
    let text = result.content[0].as_text().expect("text").text.clone();
    assert!(
        text.contains("devA"),
        "result should contain devA; got {text}"
    );

    let _ = client.cancel().await;
    server_task.abort();
}

/// call_tool 带参数往返：read_tags 带 dev_id 参数正确反序列化（验证 schema 路径）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_tool_with_arguments_round_trip() {
    let (client, server_task) = spawn_handshake(json!({"devX": {"temp": 1}})).await;
    let result = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("rsiot_read_tags").with_arguments(
                json!({ "dev_id": "devX" })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
        )
        .await
        .expect("call_tool");
    assert_ne!(result.is_error, Some(true));
    let text = result.content[0].as_text().expect("text").text.clone();
    assert!(
        text.contains("devX"),
        "argument reached backend; got {text}"
    );

    let _ = client.cancel().await;
    server_task.abort();
}

/// call_tool 写工具往返：send_device_command 带 4 参数，backend 返 {status:ok}。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_tool_write_command_round_trip() {
    let (client, server_task) = spawn_handshake(json!(null)).await;
    let result = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("rsiot_send_device_command").with_arguments(
                json!({
                    "instance": "inst1",
                    "devid": "devA",
                    "cmd": "write",
                    "data": "{\"address\":\"holding:0:f32\",\"value\":12.5}"
                })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            ),
        )
        .await
        .expect("call_tool");
    assert_ne!(result.is_error, Some(true));
    let text = result.content[0].as_text().expect("text").text.clone();
    assert!(text.contains("ok"), "write post returned ok; got {text}");

    let _ = client.cancel().await;
    server_task.abort();
}

/// risk 拦截经 JSON-RPC 往返生效：默认 RiskPolicy 下 CriticalWrite（set_instance）被拦。
///
/// 验证意图（规则7）：硬闸必须在协议层生效——只验 `check()` 纯函数不够，reasonix 实际走
/// `call_tool` 往返路径。默认策略**放行**（serve 审批兜底），设 deny 后 set_instance 应返
/// error CallToolResult（is_error=true + text 含 blocked），且不触达 backend。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_tool_criticalwrite_blocked_over_wire_when_deny_set() {
    let (client, server_task) = spawn_handshake_deny(json!(null)).await;
    let result = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("rsiot_set_instance").with_arguments(
                json!({
                    "inst_id": "i1",
                    "object": { "app_code": "dataSim", "app_type": "south" }
                })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            ),
        )
        .await
        .expect("call_tool");
    assert_eq!(
        result.is_error,
        Some(true),
        "CriticalWrite must be blocked when deny policy set"
    );
    let text = result.content[0].as_text().expect("text").text.clone();
    assert!(
        text.contains("blocked"),
        "blocked message must be readable; got {text}"
    );

    let _ = client.cancel().await;
    server_task.abort();
}

/// call_tool 往返：默认策略放行 CriticalWrite（serve 审批兜底），set_instance 正常触达
/// backend 返回结果。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_tool_criticalwrite_passes_over_wire_by_default() {
    let (client, server_task) = spawn_handshake(json!({"status": "ok"})).await;
    let result = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("rsiot_set_instance").with_arguments(
                json!({
                    "inst_id": "i1",
                    "object": { "app_code": "dataSim", "app_type": "south" }
                })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            ),
        )
        .await
        .expect("call_tool");
    assert_ne!(
        result.is_error,
        Some(true),
        "default policy passes CriticalWrite to serve approval"
    );

    let _ = client.cancel().await;
    server_task.abort();
}
