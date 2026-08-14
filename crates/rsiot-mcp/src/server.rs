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
    schemars, tool, tool_handler, tool_router,
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
    #[schemars(
        description = "Command payload as JSON text, e.g. {\"address\":\"holding:0:f32\",\"value\":12.34}"
    )]
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
    #[schemars(
        description = "Script directory name (e.g. \"devA\"), written to scripts/<name>/main.lua. Only [A-Za-z0-9_-]."
    )]
    pub name: String,
    #[schemars(description = "Content of main.lua")]
    pub content: String,
}

/// `rsiot_start_packet_trace`：激活报文 trace。对标 `routes_trace::TraceReq`。
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StartPacketTraceRequest {
    #[schemars(description = "Instance must be Running, or rs-iot returns 404")]
    pub inst_id: String,
    #[schemars(description = "Trace duration in seconds (default 60)")]
    pub duration_secs: Option<u64>,
}

/// `rsiot_stop_packet_trace`：关报文 trace。对标 `routes_trace::IdQuery`（query 参数）。
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StopPacketTraceRequest {
    #[schemars(description = "Instance ID to stop tracing")]
    pub inst_id: String,
}

/// `rsiot_get_instance_stats`：实例指标。对标 `routes_stat::InstQuery`。
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetInstanceStatsRequest {
    #[schemars(description = "Omit to return stats for all instances")]
    pub inst_id: Option<String>,
}

/// `rsiot_reload_lua_instance`：重载 lua 实例（stop+start）。对标 `routes_lua::InstQuery`。
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReloadLuaRequest {
    #[schemars(description = "luaClient instance ID (non-lua instance returns 400)")]
    pub inst_id: String,
}

/// `rsiot_get_instance_config`：读实例完整配置。对标 `routes_inst::IdQuery`。
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetInstanceConfigRequest {
    #[schemars(description = "Instance ID")]
    pub inst_id: String,
}

/// `rsiot_set_instance`：建/改实例。对标 `routes_inst::SetReq`（inst_id + 原样配置对象）。
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetInstanceRequest {
    #[schemars(description = "Instance ID (unique, routing key)")]
    pub inst_id: String,
    #[schemars(
        description = "Full InstConfig JSON: inst_name/app_code(plugin name)/app_type(south|north)/auto_start/app_config(arbitrary plugin config)/devices/host_kind(builtin|external). rs-iot validates on deserialize."
    )]
    pub object: serde_json::Value,
}

/// `rsiot_delete_instance`：删实例。对标 `routes_inst::DelReq`。
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeleteInstanceRequest {
    #[schemars(description = "Instance ID to delete")]
    pub inst_id: String,
}

/// `rsiot_set_device`：单点 upsert 设备到指定实例点表。对标 `routes_device::SetReq`
/// （inst_id + dev_id + DeviceSpec object——设备挂在实例下，无实例不成设备）。
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetDeviceRequest {
    #[schemars(description = "Owning instance ID (device belongs to an instance)")]
    pub inst_id: String,
    #[schemars(description = "Device ID (becomes DeviceSpec.value)")]
    pub dev_id: String,
    #[schemars(
        description = "DeviceSpec JSON: label/children[{label,value(tag),type}]/slave_id(optional, modbusClient only); value is overridden by dev_id"
    )]
    pub object: serde_json::Value,
}

/// `rsiot_delete_device`：从指定实例点表移除设备。对标 `routes_device::DelReq`。
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeleteDeviceRequest {
    #[schemars(description = "Owning instance ID")]
    pub inst_id: String,
    #[schemars(description = "Device ID to remove from the instance's device list")]
    pub dev_id: String,
}

// ============ query 拼接辅助 ============

/// percent-encode query 参数值（输入边界防护，对标规则5「系统边界处验证输入」）。
///
/// `dev_id`/`level`/`target` 来自 agent（自由文本），直接 `format!` 拼 query 会因 `&`/`=`/`+`/
/// 空格/`#`/非 ASCII 等字符破坏 URL 或注入额外参数。本 helper 把这些字符转 `%XX`。
///
/// 保留 unreserved（`A-Za-z0-9-_.~`，RFC 3986）原样，其余一律 `%XX` 编码（保守，宁可多编）。
fn encode_query_value(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

// ============ risk 框架 ============

/// 工具风险三档（rs-iot 独有，对标 dbx-mcp `sql_risk` 但绑定工具名）。
///
/// - `ReadOnly`：查询类，agent 自动放行（对标 rmcp `readOnlyHint`，但运行时判定更灵活）。
/// - `Write`：写设备/启停实例/写脚本，工具描述声明 "requires approval"，由 reasonix Ask 模式拦。
/// - `CriticalWrite`：建/删实例与设备点表，受 `RiskPolicy` 总开关管制（默认拦，见 `check_risk`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolRisk {
    ReadOnly,
    Write,
    CriticalWrite,
}

/// 工具名 → risk 档静态查表（纯函数，零运行时分配；漏登名字会在拦截测试暴露）。
///
/// 未知名字按 `CriticalWrite` 保守对待（**fail-closed**：新增工具漏登记进表时，
/// 默认被 risk 拦截而非静默放行——"默认最严"对表外名字同样成立）。完整性由
/// `all_tools_registered_in_risk_table` 测试锁定（23 工具逐一断言非 default）。
fn tool_risk(name: &str) -> ToolRisk {
    use ToolRisk::*;
    match name {
        // 只读（12）
        "rsiot_list_devices"
        | "rsiot_read_tags"
        | "rsiot_get_status"
        | "rsiot_list_instances"
        | "rsiot_get_recent_logs"
        | "rsiot_get_config"
        | "rsiot_list_packet_trace"
        | "rsiot_get_instance_stats"
        | "rsiot_list_lua_scripts"
        | "rsiot_get_instance_config"
        | "rsiot_list_plugins"
        | "rsiot_get_sysinfo" => ReadOnly,
        // 普通写（描述声明 + annotations，放行）
        "rsiot_send_device_command"
        | "rsiot_start_instance"
        | "rsiot_stop_instance"
        | "rsiot_save_lua_script"
        | "rsiot_start_packet_trace"
        | "rsiot_stop_packet_trace"
        | "rsiot_reload_lua_instance" => Write,
        // 关键写（受 RiskPolicy 总开关管制）
        "rsiot_set_instance"
        | "rsiot_delete_instance"
        | "rsiot_set_device"
        | "rsiot_delete_device" => CriticalWrite,
        // fail-closed：未登记（含未来新增漏登记）按最严拦，不放行。
        _ => CriticalWrite,
    }
}

/// risk 拦截策略。
///
/// P0 实现：仅 `allow_critical_writes` 总开关——`false`（默认）所有 `CriticalWrite` 返 error
/// （安全默认，生产想开才开）；`true` 放行（开发/受控环境）。
///
/// P1 扩展点（留接口不实现，避 dead_code）：加 `critical_device_ids: HashSet<String>`，
/// `set/delete_device` 的 dev_id 命中即拒（即使总开关放开）——在 `check_risk` 内读参数实现。
#[derive(Debug, Clone)]
pub struct RiskPolicy {
    /// 硬闸开关：`true`=所有 CriticalWrite 被拦截（生产双保险）；`false`=放行给
    /// serve 审批层（默认——写工具带 destructive annotation，serve Ask 模式触发
    /// approval_request，用户在 UI 决策）。
    deny_critical_writes: bool,
}

impl Default for RiskPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl RiskPolicy {
    /// 默认放行 CriticalWrite 到 serve 审批层。
    ///
    /// 历史设计默认最严（全拦）——现场实测暴露误伤：AI 建实例是常规需求（用户明确
    /// 要求），被 MCP 层硬闸拦在 serve 审批**之前**，审批弹窗永不出现、用户无决策
    /// 入口（AI 卡"思考中"）。serve 的 Ask 审批已是用户可见的决策机制，MCP 层硬闸
    /// 是重复防线且误伤——改为默认放行，`RSIOT_MCP_DENY_CRITICAL_WRITES=1` 时硬拦。
    pub fn new() -> Self {
        Self {
            deny_critical_writes: false,
        }
    }

    /// 硬拦全部 CriticalWrite（生产双保险 / 集成测试用）。
    pub fn deny_all() -> Self {
        Self {
            deny_critical_writes: true,
        }
    }

    /// 从环境变量构造：`RSIOT_MCP_DENY_CRITICAL_WRITES=1|true|yes|on` 硬拦
    /// CriticalWrite（大小写不敏感，生产部署可设此双保险；默认不设 = 放行给审批）。
    pub fn from_env() -> Self {
        let deny = std::env::var("RSIOT_MCP_DENY_CRITICAL_WRITES")
            .map(|v| {
                let v = v.trim();
                v == "1"
                    || v.eq_ignore_ascii_case("true")
                    || v.eq_ignore_ascii_case("yes")
                    || v.eq_ignore_ascii_case("on")
            })
            .unwrap_or(false);
        Self {
            deny_critical_writes: deny,
        }
    }

    /// 判定工具是否被 risk 策略拦截。返 `Some(error CallToolResult)` = 拦截，`None` = 放行。
    ///
    /// 拦截返 `CallToolResult::error`（非协议 Err）——工具级失败，agent 看可读文本自行决策，
    /// 与 `backend_to_result` 既有错误约定一致。
    fn check(&self, name: &str) -> Option<CallToolResult> {
        if matches!(tool_risk(name), ToolRisk::CriticalWrite) && self.deny_critical_writes {
            // P1 扩展点：此处可读 request.arguments 取 dev_id/inst_id，
            // 命中 critical_device_ids 即拒（即使总开关放开）。
            Some(CallToolResult::error(vec![ContentBlock::text(format!(
                "{name} blocked: CriticalWrite tool (creates/deletes instance or device). \
                 Set RSIOT_MCP_DENY_CRITICAL_WRITES=1 to enforce; \
                 production: prefer explicit per-device policy."
            ))]))
        } else {
            None
        }
    }
}

// ============ server 主体 ============

/// MCP server 主体。
///
/// 持有 `Arc<dyn RsiotBackend>`（WebBackend 注入，测试可换 FakeBackend）。
/// `#[tool_router]` 注册所有 `#[tool]` 方法；`#[tool_handler]` 自动生成
/// `call_tool`/`list_tools`/`get_tool`/`get_info`（rmcp 官方模式，见 docs.rs/rmcp）。
pub struct RsiotMcpServer {
    backend: Arc<dyn RsiotBackend>,
    /// CriticalWrite 硬闸（默认放行给 serve 审批；main 从 `RSIOT_MCP_DENY_CRITICAL_WRITES` 构造）。
    risk_policy: RiskPolicy,
}

impl RsiotMcpServer {
    /// 构造 server，注入 backend + risk 策略（`RiskPolicy::default()` = 最严）。
    pub fn new(backend: Arc<dyn RsiotBackend>, risk_policy: RiskPolicy) -> Self {
        Self {
            backend,
            risk_policy,
        }
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

    /// `rsiot_list_devices`：列设备。映射 `GET /api/v1/devices`。
    ///
    /// 后端该端点已从实例配置聚合（单一数据源 = InstConfig.devices）——历史独立
    /// 设备注册表（lux `devices` Hash）是孤立死表，建实例不同步写、REST 客户端
    /// 读到空表（现场 bug 根因），已废弃。MCP 保持薄翻译层，不复制业务逻辑。
    #[tool(
        description = "List devices aggregated from instance configs, optionally filtered by dev_id. Each device includes collection points (children). Read-only.",
        annotations(read_only_hint = true)
    )]
    async fn rsiot_list_devices(
        &self,
        Parameters(req): Parameters<ListDevicesRequest>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let path = match &req.dev_id {
            Some(id) => format!("/api/v1/device/get?dev_id={}", encode_query_value(id)),
            None => "/api/v1/devices".to_string(),
        };
        backend_to_result(self.backend.get(&path).await, "rsiot_list_devices")
    }

    /// `rsiot_read_tags`：读设备实时数据。映射 `GET /api/v1/rtdatas?dev_id=`。
    #[tool(
        description = "Read latest values of device tags from the gateway. Pass dev_id for a single device, or omit for all devices. Read-only.",
        annotations(read_only_hint = true)
    )]
    async fn rsiot_read_tags(
        &self,
        Parameters(req): Parameters<ReadTagsRequest>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let path = match &req.dev_id {
            Some(id) => format!("/api/v1/rtdatas?dev_id={}", encode_query_value(id)),
            None => "/api/v1/rtdatas".to_string(),
        };
        backend_to_result(self.backend.get(&path).await, "rsiot_read_tags")
    }

    /// `rsiot_get_status`：网关综合状态（实例/计数器/CPU/mem）。映射 `GET /api/v1/sysstatus`。
    #[tool(
        description = "Get gateway overall status: running instances, pipeline/hub/mqtt counters, process CPU/memory. Read-only.",
        annotations(read_only_hint = true)
    )]
    async fn rsiot_get_status(
        &self,
        Parameters(_): Parameters<NoParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        backend_to_result(
            self.backend.get("/api/v1/sysstatus").await,
            "rsiot_get_status",
        )
    }

    /// `rsiot_list_instances`：列实例名册（配置全集 ∪ 运行态）。映射 `GET /api/v1/inst/roster`。
    #[tool(
        description = "List ALL gateway plugin instances (configured + running state), including auto_start=false ones never started. Each entry carries its state (running/stopped/failed etc). Read-only.",
        annotations(read_only_hint = true)
    )]
    async fn rsiot_list_instances(
        &self,
        Parameters(_): Parameters<NoParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        backend_to_result(
            self.backend.get("/api/v1/inst/roster").await,
            "rsiot_list_instances",
        )
    }

    /// `rsiot_get_recent_logs`：取最近日志。映射 `GET /api/v1/logs/recent`。
    #[tool(
        description = "Get recent structured log entries from the gateway ring buffer. Filter by level (e.g. \"warn\" = error+warn), tracing target (e.g. \"io_trace\" for packet trace), and limit. Read-only.",
        annotations(read_only_hint = true)
    )]
    async fn rsiot_get_recent_logs(
        &self,
        Parameters(req): Parameters<GetRecentLogsRequest>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        // 拼 query：只拼非空参数，避免尾随 &；level/target 经 percent-encode 防注入。
        let mut qs: Vec<String> = Vec::new();
        if let Some(l) = &req.level {
            qs.push(format!("level={}", encode_query_value(l)));
        }
        if let Some(t) = &req.target {
            qs.push(format!("target={}", encode_query_value(t)));
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
    #[tool(
        description = "Send a write command to a device via its running instance (e.g. write a Modbus register or OPC UA node). HIGH RISK: modifies device state. The `data` field is plugin-specific JSON (e.g. {\"address\":\"holding:0:f32\",\"value\":12.5}). Requires approval.",
        annotations(destructive_hint = true)
    )]
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
        backend_to_result(
            self.backend.post("/api/v1/command/send", &body).await,
            "rsiot_send_device_command",
        )
    }

    /// `rsiot_start_instance`：启动实例。映射 `POST /api/v1/startinst`。
    #[tool(
        description = "Start a south (source) instance by inst_id. The instance begins collecting from its configured devices. Requires approval."
    )]
    async fn rsiot_start_instance(
        &self,
        Parameters(req): Parameters<InstanceRequest>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let body = serde_json::json!({ "inst_id": req.inst_id });
        backend_to_result(
            self.backend.post("/api/v1/startinst", &body).await,
            "rsiot_start_instance",
        )
    }

    /// `rsiot_stop_instance`：停止实例。映射 `POST /api/v1/stopinst`。
    #[tool(
        description = "Stop a running instance by inst_id. Its collection tasks are cancelled. Requires approval."
    )]
    async fn rsiot_stop_instance(
        &self,
        Parameters(req): Parameters<InstanceRequest>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let body = serde_json::json!({ "inst_id": req.inst_id });
        backend_to_result(
            self.backend.post("/api/v1/stopinst", &body).await,
            "rsiot_stop_instance",
        )
    }

    /// `rsiot_save_lua_script`：写 lua 脚本。映射 `POST /api/v1/lua/script/save`。
    ///
    /// **写操作**：修改 scripts/ 文件树（路径白名单由 rs-iot 主进程强制）。
    /// 写后对**运行中**实例调 `rsiot_reload_lua_instance`（stop+start）生效——不要用
    /// `rsiot_start_instance`（对运行中实例是错误路径）。
    #[tool(
        description = "Write a lua collection script to scripts/<name>/main.lua. After saving, call rsiot_reload_lua_instance to apply to a running luaClient. Requires approval."
    )]
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
    #[tool(
        description = "Read the gateway configuration with secrets masked (jwt_secret and mqtt credentials shown as \"***\"). Read-only.",
        annotations(read_only_hint = true)
    )]
    async fn rsiot_get_config(
        &self,
        Parameters(_): Parameters<NoParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        backend_to_result(self.backend.get("/api/v1/config").await, "rsiot_get_config")
    }

    // ---- 报文 trace（开关语义与实例生命周期解耦；激活后收发字节经 io_trace target 进日志 ring buffer）----

    /// `rsiot_start_packet_trace`：激活实例报文 trace。映射 `POST /api/v1/inst/trace`。
    ///
    /// 实例须 Running（未运行 → 404）。返回 deadline_ms（trace 到时自动失效）。
    /// 激活后配合 `rsiot_get_recent_logs(target="io_trace")` 拉报文 hex。
    #[tool(
        description = "Start packet-level trace for a RUNNING instance (raw TX/RX bytes go to io_trace log target). Returns deadline_ms. Instance must be Running or rs-iot returns 404. Requires approval."
    )]
    async fn rsiot_start_packet_trace(
        &self,
        Parameters(req): Parameters<StartPacketTraceRequest>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        // duration_secs 上界 86400(1 天):rs-iot 侧 `(x as i64)*1000` 在 debug 构建
        // 对超大 u64 溢出 panic(审查 finding)。1 天已是超长调试窗口。
        let dur = req.duration_secs.unwrap_or(60).min(86400);
        let body = serde_json::json!({
            "inst_id": req.inst_id,
            "duration_secs": dur,
        });
        backend_to_result(
            self.backend.post("/api/v1/inst/trace", &body).await,
            "rsiot_start_packet_trace",
        )
    }

    /// `rsiot_list_packet_trace`：当前激活 trace 的实例列表。映射 `GET /api/v1/inst/trace`。
    #[tool(
        description = "List currently active packet-trace instance IDs (lazy-expired). Read-only.",
        annotations(read_only_hint = true)
    )]
    async fn rsiot_list_packet_trace(
        &self,
        Parameters(_): Parameters<NoParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        backend_to_result(
            self.backend.get("/api/v1/inst/trace").await,
            "rsiot_list_packet_trace",
        )
    }

    /// `rsiot_stop_packet_trace`：关闭实例报文 trace。映射 `DELETE /api/v1/inst/trace?inst_id=`。
    #[tool(
        description = "Stop packet trace for an instance (DELETE /api/v1/inst/trace). Requires approval."
    )]
    async fn rsiot_stop_packet_trace(
        &self,
        Parameters(req): Parameters<StopPacketTraceRequest>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let path = format!(
            "/api/v1/inst/trace?inst_id={}",
            encode_query_value(&req.inst_id)
        );
        backend_to_result(self.backend.delete(&path).await, "rsiot_stop_packet_trace")
    }

    // ---- 诊断（比 list_instances 更富的实例健康视图）----

    /// `rsiot_get_instance_stats`：实例指标（state + plugin health JSON）。映射 `GET /api/v1/inst/stats`。
    ///
    /// 不带 inst_id 返全实例数组；带则返单实例 `{inst_id, app_code, host_kind, state, health}`。
    #[tool(
        description = "Get per-instance stats: process state + plugin health JSON. Pass inst_id for one instance, omit for all. Read-only.",
        annotations(read_only_hint = true)
    )]
    async fn rsiot_get_instance_stats(
        &self,
        Parameters(req): Parameters<GetInstanceStatsRequest>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let path = match &req.inst_id {
            Some(id) => format!("/api/v1/inst/stats?inst_id={}", encode_query_value(id)),
            None => "/api/v1/inst/stats".to_string(),
        };
        backend_to_result(self.backend.get(&path).await, "rsiot_get_instance_stats")
    }

    // ---- lua 闭环（save_script → reload 生效）----

    /// `rsiot_reload_lua_instance`：重载 lua 实例。映射 `POST /api/v1/lua/reload?inst_id=`。
    ///
    /// 仅 `app_code=="luaClient"` 实例生效（非 lua → 400，不存在 → 404）。
    /// 对**运行中**实例做 stop+start 使新脚本生效——这是 save_script 后正确的生效路径。
    #[tool(
        description = "Reload a luaClient instance (stop+start, applies new scripts without full process restart). Only works for app_code=='luaClient' instances. Requires approval."
    )]
    async fn rsiot_reload_lua_instance(
        &self,
        Parameters(req): Parameters<ReloadLuaRequest>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        // reload 走 query 参数（routes_lua.rs:41 Query<InstQuery>），非 body。
        let path = format!(
            "/api/v1/lua/reload?inst_id={}",
            encode_query_value(&req.inst_id)
        );
        backend_to_result(
            self.backend.post(&path, &serde_json::Value::Null).await,
            "rsiot_reload_lua_instance",
        )
    }

    /// `rsiot_list_lua_scripts`：列脚本目录。映射 `GET /api/v1/lua/scripts`。
    #[tool(
        description = "List script directories under scripts/ that have a main.lua. Read-only.",
        annotations(read_only_hint = true)
    )]
    async fn rsiot_list_lua_scripts(
        &self,
        Parameters(_): Parameters<NoParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        backend_to_result(
            self.backend.get("/api/v1/lua/scripts").await,
            "rsiot_list_lua_scripts",
        )
    }

    // ---- 实例/设备 CRUD（CriticalWrite：受 RiskPolicy 总开关管制，默认拦截）----

    /// `rsiot_get_instance_config`：读实例完整配置。映射 `GET /api/v1/inst/get?inst_id=`。
    ///
    /// 注意：返回的配置**未脱敏**（External 实例可能含连接串/token 相关字段）。
    #[tool(
        description = "Read full instance config by inst_id. WARNING: not redacted — may contain credentials for External instances. Read-only.",
        annotations(read_only_hint = true)
    )]
    async fn rsiot_get_instance_config(
        &self,
        Parameters(req): Parameters<GetInstanceConfigRequest>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let path = format!(
            "/api/v1/inst/get?inst_id={}",
            encode_query_value(&req.inst_id)
        );
        backend_to_result(self.backend.get(&path).await, "rsiot_get_instance_config")
    }

    /// `rsiot_set_instance`：建/改实例配置。映射 `POST /api/v1/inst/set`。
    ///
    /// **CriticalWrite**：建采集实例的写入口，错配可致采集失败。默认被 risk 策略拦截
    /// （审批放行（RSIOT_MCP_DENY_CRITICAL_WRITES=1 可硬拦））。External 实例返回一次性 plugin_token。
    #[tool(
        description = "Create/update an instance config (full InstConfig JSON). HIGH RISK: CRITICAL WRITE — approval-gated (serve Ask); set RSIOT_MCP_DENY_CRITICAL_WRITES=1 to hard-block. For External host_kind, returns a one-time plugin_token.",
        annotations(destructive_hint = true)
    )]
    async fn rsiot_set_instance(
        &self,
        Parameters(req): Parameters<SetInstanceRequest>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let body = serde_json::json!({ "inst_id": req.inst_id, "object": req.object });
        backend_to_result(
            self.backend.post("/api/v1/inst/set", &body).await,
            "rsiot_set_instance",
        )
    }

    /// `rsiot_delete_instance`：删实例。映射 `POST /api/v1/inst/delete`。
    ///
    /// **CriticalWrite**：连带清 ext_token + luakv。默认被 risk 策略拦截。
    #[tool(
        description = "Delete an instance and its External token + lua kv state. HIGH RISK: CRITICAL WRITE — approval-gated (serve Ask); set RSIOT_MCP_DENY_CRITICAL_WRITES=1 to hard-block.",
        annotations(destructive_hint = true)
    )]
    async fn rsiot_delete_instance(
        &self,
        Parameters(req): Parameters<DeleteInstanceRequest>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let body = serde_json::json!({ "inst_id": req.inst_id });
        backend_to_result(
            self.backend.post("/api/v1/inst/delete", &body).await,
            "rsiot_delete_instance",
        )
    }

    /// `rsiot_set_device`：建/改设备点表。映射 `POST /api/v1/device/set`。
    ///
    /// **CriticalWrite**：建设备点表写入口。默认被 risk 策略拦截。
    #[tool(
        description = "Create/update a device spec (full DeviceSpec JSON: label/value/children/slave_id). HIGH RISK: CRITICAL WRITE — approval-gated (serve Ask); set RSIOT_MCP_DENY_CRITICAL_WRITES=1 to hard-block.",
        annotations(destructive_hint = true)
    )]
    async fn rsiot_set_device(
        &self,
        Parameters(req): Parameters<SetDeviceRequest>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let body = serde_json::json!({ "inst_id": req.inst_id, "dev_id": req.dev_id, "object": req.object });
        backend_to_result(
            self.backend.post("/api/v1/device/set", &body).await,
            "rsiot_set_device",
        )
    }

    /// `rsiot_delete_device`：删设备。映射 `POST /api/v1/device/delete`。
    ///
    /// **CriticalWrite**：默认被 risk 策略拦截。
    #[tool(
        description = "Delete a device spec. HIGH RISK: CRITICAL WRITE — approval-gated (serve Ask); set RSIOT_MCP_DENY_CRITICAL_WRITES=1 to hard-block.",
        annotations(destructive_hint = true)
    )]
    async fn rsiot_delete_device(
        &self,
        Parameters(req): Parameters<DeleteDeviceRequest>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let body = serde_json::json!({ "inst_id": req.inst_id, "dev_id": req.dev_id });
        backend_to_result(
            self.backend.post("/api/v1/device/delete", &body).await,
            "rsiot_delete_device",
        )
    }

    // ---- 配套只读（set_instance 前查合法 app_code；MQTT topic 根 hwid）----

    /// `rsiot_list_plugins`：列注册表合法插件名。映射 `GET /api/v1/plugins`。
    ///
    /// `set_instance` 的 `app_code` 必须在此列表中，否则 rs-iot 校验失败。
    #[tool(
        description = "List registered source and sink plugin names (valid app_code values for rsiot_set_instance). Read-only.",
        annotations(read_only_hint = true)
    )]
    async fn rsiot_list_plugins(
        &self,
        Parameters(_): Parameters<NoParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        backend_to_result(
            self.backend.get("/api/v1/plugins").await,
            "rsiot_list_plugins",
        )
    }

    /// `rsiot_get_sysinfo`：网关硬件 ID + 版本。映射 `GET /api/v1/sysinfo`。
    #[tool(
        description = "Get gateway hardware id (hwid, MQTT topic root) and version. Read-only.",
        annotations(read_only_hint = true)
    )]
    async fn rsiot_get_sysinfo(
        &self,
        Parameters(_): Parameters<NoParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        backend_to_result(
            self.backend.get("/api/v1/sysinfo").await,
            "rsiot_get_sysinfo",
        )
    }
}

#[tool_handler]
impl rmcp::ServerHandler for RsiotMcpServer {
    /// 手写 call_tool：risk 拦截 + 透传。
    ///
    /// `#[tool_handler]` 宏检测到 `call_tool` 已手写即跳过生成（rmcp tool_handler.rs:44
    /// `has_method` 检测），`list_tools`/`get_tool`/`get_info` 仍由宏生成。
    /// CriticalWrite 被策略拦时返 error CallToolResult（agent 看可读文本，非协议 Err）。
    ///
    /// **注意**：risk 闸门只在 call_tool 这一层——工具方法与 `tool_router()` 当前均为
    /// 私有，无现实绕过路径；若未来 library 化公开这些入口，须把 risk 闸门一并前置，
    /// 否则绕过陷阱（审查 Minor）。
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let name = request.name.as_ref();
        if let Some(blocked) = self.risk_policy.check(name) {
            return Ok(blocked);
        }
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        Self::tool_router().call(tcc).await
    }
}

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
        deletes: Mutex<Vec<String>>,
    }

    impl FakeBackend {
        fn new(get_value: Value) -> Self {
            Self {
                get_value,
                post_value: Value::Null,
                paths: Mutex::new(Vec::new()),
                posts: Mutex::new(Vec::new()),
                deletes: Mutex::new(Vec::new()),
            }
        }
        /// 取出收到的所有 path（get + post + delete）。
        fn recorded_paths(&self) -> Vec<String> {
            self.paths.lock().expect("paths lock").clone()
        }
        /// 取出收到的所有 (path, body) post 调用（验证写工具拼的 body）。
        fn recorded_posts(&self) -> Vec<(String, Value)> {
            self.posts.lock().expect("posts lock").clone()
        }
        /// 取出收到的所有 delete path（验证 stop_packet_trace 拼的 query）。
        fn recorded_deletes(&self) -> Vec<String> {
            self.deletes.lock().expect("deletes lock").clone()
        }
    }

    #[async_trait]
    impl RsiotBackend for FakeBackend {
        async fn get(&self, path: &str) -> Result<Value, BackendError> {
            self.paths
                .lock()
                .expect("paths lock")
                .push(path.to_string());
            Ok(self.get_value.clone())
        }
        async fn post(&self, path: &str, body: &Value) -> Result<Value, BackendError> {
            self.paths
                .lock()
                .expect("paths lock")
                .push(path.to_string());
            self.posts
                .lock()
                .expect("posts lock")
                .push((path.to_string(), body.clone()));
            Ok(self.post_value.clone())
        }
        async fn delete(&self, path: &str) -> Result<Value, BackendError> {
            self.paths
                .lock()
                .expect("paths lock")
                .push(path.to_string());
            self.deletes
                .lock()
                .expect("deletes lock")
                .push(path.to_string());
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
        async fn delete(&self, _: &str) -> Result<Value, BackendError> {
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

    // ---- encode_query_value（输入边界防护，规则5）----

    #[test]
    fn encode_keeps_unreserved_as_is() {
        // RFC 3986 unreserved 原样。
        assert_eq!(encode_query_value("devA_1-2.~"), "devA_1-2.~");
    }

    #[test]
    fn encode_escapes_special_chars() {
        // 空格 → %20，& → %26，= → %3D，+ → %2B，# → %23。
        assert_eq!(encode_query_value("a b"), "a%20b");
        assert_eq!(encode_query_value("a&b=c"), "a%26b%3Dc");
        assert_eq!(encode_query_value("a+b"), "a%2Bb");
        assert_eq!(encode_query_value("a#b"), "a%23b");
    }

    #[test]
    fn encode_escapes_non_ascii() {
        // 中文每字节 %XX（UTF-8 三字节）。
        assert_eq!(encode_query_value("设备"), "%E8%AE%BE%E5%A4%87");
    }

    /// 注入防护：dev_id 含 `&cmd=write` 不能注入额外 query 参数。
    #[tokio::test]
    async fn read_tags_dev_id_with_ampersand_does_not_inject_param() {
        let backend = Arc::new(FakeBackend::new(json!({"v1": 1})));
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
        server
            .rsiot_read_tags(Parameters(ReadTagsRequest {
                dev_id: Some("devA&cmd=write".into()),
            }))
            .await
            .unwrap();
        let path = &backend.recorded_paths()[0];
        // 验证意图（规则7）：& 被 encode 成 %26，不会拆出第二个 query 参数。
        assert!(
            path.contains("dev_id=devA%26cmd%3Dwrite"),
            "ampersand/equals must be encoded, not injected; got {path}"
        );
        assert!(
            !path.contains("&cmd=write"),
            "raw injection must not survive; got {path}"
        );
    }

    // ---- list_devices（薄翻译层：后端 /api/v1/devices 已从实例配置聚合）----

    #[tokio::test]
    async fn list_devices_hits_devices_endpoint() {
        let backend = Arc::new(FakeBackend::new(json!({"devA": {"value": "devA"}})));
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
        let result = server
            .rsiot_list_devices(Parameters(ListDevicesRequest { dev_id: None }))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        assert!(text_of(&result).contains("devA"));
        // 验证意图：MCP 是薄翻译层——数据源是后端 /api/v1/devices
        // （后端已从实例配置聚合，单一数据源在主 crate，MCP 不复制业务逻辑）。
        assert_eq!(backend.recorded_paths()[0], "/api/v1/devices");
    }

    #[tokio::test]
    async fn list_devices_with_dev_id_hits_device_get() {
        let backend = Arc::new(FakeBackend::new(json!({"dev_id": "devA"})));
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
        server
            .rsiot_list_devices(Parameters(ListDevicesRequest {
                dev_id: Some("devA".into()),
            }))
            .await
            .unwrap();
        let paths = backend.recorded_paths();
        assert!(
            paths
                .iter()
                .any(|p| p.contains("/api/v1/device/get?dev_id=devA")),
            "dev_id filter should hit device/get; got {paths:?}"
        );
    }

    // ---- read_tags ----

    #[tokio::test]
    async fn read_tags_all_devices_hits_rtdatas_no_query() {
        let backend = Arc::new(FakeBackend::new(json!({"devA": {"v1": 1}})));
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
        let result = server
            .rsiot_read_tags(Parameters(ReadTagsRequest { dev_id: None }))
            .await
            .unwrap();
        assert!(text_of(&result).contains("devA"));
        // 验证意图：无 dev_id 时不带 query（避免尾随 ?）。
        assert_eq!(
            backend.recorded_paths()[0],
            "/api/v1/rtdatas",
            "no trailing ? when no params"
        );
    }

    #[tokio::test]
    async fn read_tags_with_dev_id_appends_query() {
        let backend = Arc::new(FakeBackend::new(json!({"v1": 1})));
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
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
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
        let result = server
            .rsiot_get_status(Parameters(NoParams {}))
            .await
            .unwrap();
        assert!(text_of(&result).contains("ok"));
        assert_eq!(backend.recorded_paths()[0], "/api/v1/sysstatus");
    }

    #[tokio::test]
    async fn list_instances_hits_inst_roster() {
        let backend = Arc::new(FakeBackend::new(json!([{"inst_id": "i1"}])));
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
        server
            .rsiot_list_instances(Parameters(NoParams {}))
            .await
            .unwrap();
        assert_eq!(backend.recorded_paths()[0], "/api/v1/inst/roster");
    }

    // ---- get_recent_logs（query 拼接逻辑）----

    #[tokio::test]
    async fn recent_logs_no_params_hits_bare_path() {
        let backend = Arc::new(FakeBackend::new(json!([])));
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
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
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
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
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
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
            body["data"], r#"{"address":"holding:0:f32:ABCD","value":12.5}"#,
            "data must stay as JSON text string, not parsed object"
        );
    }

    // ---- start/stop_instance（写工具，inst_id body）----

    #[tokio::test]
    async fn start_instance_posts_inst_id() {
        let backend = Arc::new(FakeBackend::new(json!(null)));
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
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
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
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
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
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
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
        let result = server
            .rsiot_get_config(Parameters(NoParams {}))
            .await
            .unwrap();
        assert!(text_of(&result).contains("***"));
        assert_eq!(backend.recorded_paths()[0], "/api/v1/config");
    }

    // ---- 新增 13 工具（path/body 拼对 + risk 拦截）----

    #[tokio::test]
    async fn start_packet_trace_posts_body_with_default_duration() {
        let backend = Arc::new(FakeBackend::new(json!(null)));
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
        server
            .rsiot_start_packet_trace(Parameters(StartPacketTraceRequest {
                inst_id: "i1".into(),
                duration_secs: None,
            }))
            .await
            .unwrap();
        let (path, body) = &backend.recorded_posts()[0];
        assert_eq!(path, "/api/v1/inst/trace");
        assert_eq!(body["inst_id"], "i1");
        assert_eq!(body["duration_secs"], 60, "default duration is 60");
    }

    #[tokio::test]
    async fn start_packet_trace_posts_explicit_duration() {
        let backend = Arc::new(FakeBackend::new(json!(null)));
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
        server
            .rsiot_start_packet_trace(Parameters(StartPacketTraceRequest {
                inst_id: "i1".into(),
                duration_secs: Some(120),
            }))
            .await
            .unwrap();
        let (_, body) = &backend.recorded_posts()[0];
        assert_eq!(body["duration_secs"], 120);
    }

    #[tokio::test]
    async fn list_packet_trace_hits_get() {
        let backend = Arc::new(FakeBackend::new(json!({"active": []})));
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
        server
            .rsiot_list_packet_trace(Parameters(NoParams {}))
            .await
            .unwrap();
        assert_eq!(backend.recorded_paths()[0], "/api/v1/inst/trace");
    }

    #[tokio::test]
    async fn stop_packet_trace_hits_delete_with_query() {
        let backend = Arc::new(FakeBackend::new(json!({"status": "ok"})));
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
        server
            .rsiot_stop_packet_trace(Parameters(StopPacketTraceRequest {
                inst_id: "i1&x=y".into(),
            }))
            .await
            .unwrap();
        // 验证意图：DELETE 方法 + query 参数经 encode（& 不被当分隔符）。
        assert_eq!(
            backend.recorded_deletes()[0],
            "/api/v1/inst/trace?inst_id=i1%26x%3Dy"
        );
    }

    #[tokio::test]
    async fn get_instance_stats_no_param_hits_bare() {
        let backend = Arc::new(FakeBackend::new(json!([])));
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
        server
            .rsiot_get_instance_stats(Parameters(GetInstanceStatsRequest { inst_id: None }))
            .await
            .unwrap();
        assert_eq!(backend.recorded_paths()[0], "/api/v1/inst/stats");
    }

    #[tokio::test]
    async fn get_instance_stats_with_inst_id_appends_query() {
        let backend = Arc::new(FakeBackend::new(json!({"state": "Running"})));
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
        server
            .rsiot_get_instance_stats(Parameters(GetInstanceStatsRequest {
                inst_id: Some("i1".into()),
            }))
            .await
            .unwrap();
        assert_eq!(backend.recorded_paths()[0], "/api/v1/inst/stats?inst_id=i1");
    }

    #[tokio::test]
    async fn reload_lua_posts_with_query_not_body() {
        let backend = Arc::new(FakeBackend::new(json!(null)));
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
        server
            .rsiot_reload_lua_instance(Parameters(ReloadLuaRequest {
                inst_id: "i1".into(),
            }))
            .await
            .unwrap();
        // 验证意图：reload 走 query 参数（对标 routes_lua Query<InstQuery>），body 是 Null。
        let posts = backend.recorded_posts();
        assert_eq!(posts[0].0, "/api/v1/lua/reload?inst_id=i1");
        assert_eq!(posts[0].1, serde_json::Value::Null);
    }

    #[tokio::test]
    async fn list_lua_scripts_hits_get() {
        let backend = Arc::new(FakeBackend::new(json!({"scripts": ["devA"]})));
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
        server
            .rsiot_list_lua_scripts(Parameters(NoParams {}))
            .await
            .unwrap();
        assert_eq!(backend.recorded_paths()[0], "/api/v1/lua/scripts");
    }

    #[tokio::test]
    async fn get_instance_config_hits_get() {
        let backend = Arc::new(FakeBackend::new(json!({"i1": {"app_code": "dataSim"}})));
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
        server
            .rsiot_get_instance_config(Parameters(GetInstanceConfigRequest {
                inst_id: "i1".into(),
            }))
            .await
            .unwrap();
        assert_eq!(backend.recorded_paths()[0], "/api/v1/inst/get?inst_id=i1");
    }

    #[tokio::test]
    async fn set_instance_posts_inst_id_and_object() {
        let backend = Arc::new(FakeBackend::new(json!(null)));
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
        server
            .rsiot_set_instance(Parameters(SetInstanceRequest {
                inst_id: "i1".into(),
                object: json!({"app_code": "dataSim", "app_type": "south"}),
            }))
            .await
            .unwrap();
        let (path, body) = &backend.recorded_posts()[0];
        assert_eq!(path, "/api/v1/inst/set");
        assert_eq!(body["inst_id"], "i1");
        // object 原样透传（rs-iot 侧反序列化校验，MCP 不二次解析）。
        assert_eq!(body["object"]["app_code"], "dataSim");
    }

    #[tokio::test]
    async fn delete_instance_posts_body() {
        let backend = Arc::new(FakeBackend::new(json!(null)));
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
        server
            .rsiot_delete_instance(Parameters(DeleteInstanceRequest {
                inst_id: "i1".into(),
            }))
            .await
            .unwrap();
        let (path, body) = &backend.recorded_posts()[0];
        // 验证意图：rs-iot 的 inst/delete 走 POST+body（非 HTTP DELETE）。
        assert_eq!(path, "/api/v1/inst/delete");
        assert_eq!(body["inst_id"], "i1");
    }

    #[tokio::test]
    async fn set_device_posts_inst_dev_and_object() {
        let backend = Arc::new(FakeBackend::new(json!(null)));
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
        server
            .rsiot_set_device(Parameters(SetDeviceRequest {
                inst_id: "mb-1".into(),
                dev_id: "devA".into(),
                object: json!({"label": "devA", "value": "devA"}),
            }))
            .await
            .unwrap();
        let (path, body) = &backend.recorded_posts()[0];
        assert_eq!(path, "/api/v1/device/set");
        // 验证意图：设备单点操作必须携带 inst_id（设备挂在实例下，无实例不成设备）。
        assert_eq!(body["inst_id"], "mb-1");
        assert_eq!(body["dev_id"], "devA");
        assert_eq!(body["object"]["label"], "devA");
    }

    #[tokio::test]
    async fn delete_device_posts_body() {
        let backend = Arc::new(FakeBackend::new(json!(null)));
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
        server
            .rsiot_delete_device(Parameters(DeleteDeviceRequest {
                inst_id: "mb-1".into(),
                dev_id: "devA".into(),
            }))
            .await
            .unwrap();
        let (path, body) = &backend.recorded_posts()[0];
        assert_eq!(path, "/api/v1/device/delete");
        assert_eq!(body["inst_id"], "mb-1");
        assert_eq!(body["dev_id"], "devA");
    }

    #[tokio::test]
    async fn list_plugins_hits_get() {
        let backend = Arc::new(FakeBackend::new(json!({"sources": ["dataSim"]})));
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
        server
            .rsiot_list_plugins(Parameters(NoParams {}))
            .await
            .unwrap();
        assert_eq!(backend.recorded_paths()[0], "/api/v1/plugins");
    }

    #[tokio::test]
    async fn get_sysinfo_hits_get() {
        let backend = Arc::new(FakeBackend::new(json!({"hwid": "eiot123"})));
        let server = RsiotMcpServer::new(
            backend.clone() as Arc<dyn RsiotBackend>,
            RiskPolicy::default(),
        );
        server
            .rsiot_get_sysinfo(Parameters(NoParams {}))
            .await
            .unwrap();
        assert_eq!(backend.recorded_paths()[0], "/api/v1/sysinfo");
    }

    // ---- risk 硬闸（RiskPolicy::check 纯函数路径）----

    #[test]
    fn check_passes_criticalwrite_by_default() {
        let policy = RiskPolicy::new();
        // 验证意图：默认放行 CriticalWrite 到 serve 审批层（审批是用户可见的决策
        // 机制；MCP 层硬闸默认拦会误伤——AI 建实例被拦在审批之前，用户无决策入口）。
        for name in [
            "rsiot_set_instance",
            "rsiot_delete_instance",
            "rsiot_set_device",
            "rsiot_delete_device",
        ] {
            assert!(
                policy.check(name).is_none(),
                "{name} must pass by default (serve approval gates it)"
            );
        }
        assert!(policy.check("rsiot_send_device_command").is_none());
        assert!(policy.check("rsiot_read_tags").is_none());
    }

    #[test]
    fn check_blocks_criticalwrite_when_deny_set() {
        let policy = RiskPolicy {
            deny_critical_writes: true,
        };
        // 验证意图：RSIOT_MCP_DENY_CRITICAL_WRITES=1 时 4 个 CriticalWrite 硬拦
        // （生产双保险），Write/ReadOnly 仍放行。
        for name in [
            "rsiot_set_instance",
            "rsiot_delete_instance",
            "rsiot_set_device",
            "rsiot_delete_device",
        ] {
            let blocked = policy.check(name);
            assert!(blocked.is_some(), "{name} must be blocked when deny set");
            assert!(
                blocked.unwrap().is_error == Some(true),
                "{name} block must be error result"
            );
        }
        assert!(policy.check("rsiot_send_device_command").is_none());
        assert!(policy.check("rsiot_read_tags").is_none());
    }

    #[test]
    fn tool_risk_table_maps_all_tools() {
        use ToolRisk::*;
        assert_eq!(tool_risk("rsiot_read_tags"), ReadOnly);
        assert_eq!(tool_risk("rsiot_get_sysinfo"), ReadOnly);
        assert_eq!(tool_risk("rsiot_start_packet_trace"), Write);
        assert_eq!(tool_risk("rsiot_send_device_command"), Write);
        assert_eq!(tool_risk("rsiot_set_instance"), CriticalWrite);
        assert_eq!(tool_risk("rsiot_delete_device"), CriticalWrite);
        // 未知名字 fail-closed 按最严（新增工具漏登记时被拦，而非静默放行）。
        assert_eq!(tool_risk("rsiot_unknown_tool"), CriticalWrite);
    }

    /// 完整性测试（fail-closed 保障）：23 个工具逐一断言登记档位与期望一致。
    ///
    /// 验证意图（规则7）：`_ => CriticalWrite` 的 fail-closed 默认只在"漏登记"时生效，
    /// 而本测试把 23 个名字的期望档位全枚举——新增工具若漏登记进 risk 表，
    /// 这里会因档位不符而失败（而非靠 default 静默放行）。与 protocol.rs 的
    /// `handshake_lists_all_tools`(list_tools 注册完整性)互补:那边锁"注册",这边锁"risk 登记"。
    #[test]
    fn all_tools_registered_in_risk_table() {
        use ToolRisk::*;
        // (工具名, 期望档位) 全量 23 个。
        let expected: &[(&str, ToolRisk)] = &[
            ("rsiot_list_devices", ReadOnly),
            ("rsiot_read_tags", ReadOnly),
            ("rsiot_get_status", ReadOnly),
            ("rsiot_list_instances", ReadOnly),
            ("rsiot_get_recent_logs", ReadOnly),
            ("rsiot_get_config", ReadOnly),
            ("rsiot_list_packet_trace", ReadOnly),
            ("rsiot_get_instance_stats", ReadOnly),
            ("rsiot_list_lua_scripts", ReadOnly),
            ("rsiot_get_instance_config", ReadOnly),
            ("rsiot_list_plugins", ReadOnly),
            ("rsiot_get_sysinfo", ReadOnly),
            ("rsiot_send_device_command", Write),
            ("rsiot_start_instance", Write),
            ("rsiot_stop_instance", Write),
            ("rsiot_save_lua_script", Write),
            ("rsiot_start_packet_trace", Write),
            ("rsiot_stop_packet_trace", Write),
            ("rsiot_reload_lua_instance", Write),
            ("rsiot_set_instance", CriticalWrite),
            ("rsiot_delete_instance", CriticalWrite),
            ("rsiot_set_device", CriticalWrite),
            ("rsiot_delete_device", CriticalWrite),
        ];
        for (name, want) in expected {
            assert_eq!(
                tool_risk(name),
                *want,
                "tool {name} must be registered in risk table as {want:?}"
            );
        }
    }

    // ---- 错误路径（所有工具共用 backend_to_result）----

    #[tokio::test]
    async fn any_tool_returns_error_when_backend_unreachable() {
        // 用 read_tags 代表——所有工具走同一 backend_to_result，错误路径一致。
        let server = RsiotMcpServer::new(Arc::new(ErrBackend), RiskPolicy::default());
        let result = server
            .rsiot_read_tags(Parameters(ReadTagsRequest { dev_id: None }))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true), "should be error result");
        assert!(text_of(&result).contains("rsiot_read_tags failed"));
    }
}
