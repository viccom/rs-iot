//! 通用反代：REST + SSE 共用一条 `forward()` 路径。
//!
//! 关键设计（plan-gateway-tech-selection.md §四悬念2 + §五）：
//! - **SSE 裸字节流透传**：`Body::from_stream(reqwest.bytes_stream())`，axum 官方钦定模式
//!   （examples/reqwest-response）。**不用** `axum::response::sse::{Sse, Event}`——
//!   serve 的 SSE 无 `event:` 头（kind 在 JSON body 内），`Event::data` 会按 `\n` 拆行插 `data:`
//!   前缀，破坏 reasonix 前端的裸 JSON 解析（报告 §四悬念1 实测）。
//! - **不做帧边界状态机**：gateway 是字节管道，前端 EventSource 自解 `\n\n`。
//!   两条上游（rs-iot KeepAlive + serve `: ping`）自带心跳，gateway 无需自加 KeepAlive。
//! - **层3 cookie 注入**：转发 serve 时加 `Cookie: reasonix_token=<token>`（serve 只认 cookie）。
//! - **reqwest 不设全局 timeout**：防截断 SSE 长连接（报告 §七风险表）。连接超时在 Client 构造时限定。

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::TryStreamExt;

use crate::config::GwConfig;

/// Gateway 共享状态（注入 axum Router）。
///
/// `client` 为全连接池复用——REST 与 SSE 共用同一 reqwest::Client，连接复用率高。
/// `serve_token` 用 `Arc<String>` 便于未来 token 轮换（P1 token-file watch）替换原子地换。
#[derive(Clone)]
pub struct GwState {
    pub client: reqwest::Client,
    pub rsiot_base: String,
    pub serve_base: String,
    pub serve_token: Arc<String>,
    /// 层1 JWT secret（与 rs-iot 同值）。auth_middleware 验前端 JWT 用。
    pub jwt_secret: Arc<String>,
    /// 供应商管理段（home_dir/warden；空 home_dir = 未启用，端点返 503）。
    pub reasonix: crate::config::ReasonixCfg,
}

impl GwState {
    /// 从配置构造。reqwest::Client **不设全局 timeout**（报告 §七风险表：截断 SSE），
    /// 仅设连接超时（loopback/同机，短即可）。
    pub fn from_config(cfg: &GwConfig) -> Result<Self, std::io::Error> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            // 不设 .timeout()——SSE 长连接会被全局 timeout 截断。
            .build()
            .map_err(std::io::Error::other)?;
        Ok(Self {
            client,
            rsiot_base: cfg.rsiot_base.clone(),
            serve_base: cfg.serve_base.clone(),
            serve_token: Arc::new(cfg.serve_token.clone()),
            jwt_secret: Arc::new(cfg.jwt_secret.clone()),
            reasonix: cfg.reasonix.clone(),
        })
    }
}

/// 上游目标（决定 forward 走 rs-iot 还是 serve，以及是否注入 cookie）。
#[derive(Clone, Copy)]
pub enum Upstream {
    /// rs-iot 主进程：透传 Authorization（层2），不注入 cookie。
    Rsiot,
    /// reasonix serve：注入 `Cookie: reasonix_token`（层3）。
    Serve,
}

impl Upstream {
    fn base<'a>(&'a self, st: &'a GwState) -> &'a str {
        match self {
            Upstream::Rsiot => &st.rsiot_base,
            Upstream::Serve => &st.serve_base,
        }
    }

    fn cookie<'a>(&'a self, st: &'a GwState) -> Option<&'a str> {
        match self {
            Upstream::Rsiot => None,
            Upstream::Serve => Some(st.serve_token.as_str()),
        }
    }
}

/// 通用透传：拼 `upstream_base + path + query`，转发。
///
/// REST 与 SSE 走同一路径——上游返 `text/event-stream` 时，headers clone 保留 Content-Type，
/// `Body::from_stream(bytes_stream())` 原样吐字节，前端 EventSource 自解 `\n\n`。
///
/// **路径前缀由 axum `nest_service` 已剥离**（见 axum nest 语义：nested route handler 收到的
/// path 不含前缀），forward 直接用剥后的 path 拼 base，不再二次剥离。
///
/// `upstream` 决定基址 + cookie 注入。
///
/// **rsiot 凭证转换**（C1 修复）：前端 EventSource 不能设 header，只能 `?token=<jwt>`。
/// gateway `auth_middleware` 验 query token 通过，但 rs-iot `auth_middleware` 只认
/// `Authorization`/`Cookie` 不认 query token。故转发 rsiot 时，forward 从入站请求取 JWT
/// 注入 `Authorization: Bearer <jwt>`，并从 upstream URL 摘掉 `token=`（防泄漏到上游日志）。
pub async fn forward(State(st): State<Arc<GwState>>, req: Request, upstream: Upstream) -> Response {
    let base = upstream.base(&st);
    let cookie = upstream.cookie(&st);

    // 1. 拼上游 URL：path 已被 nest_service 剥前缀，直接用。
    //    rsiot 路径：从 query 摘 token=（防泄漏）+ 独立提取 JWT 注入 Authorization（C1）。
    let (parts, body) = req.into_parts();
    let raw_path = parts.uri.path();
    let raw_query = parts.uri.query().unwrap_or("");

    // 提取入站 JWT（query 优先——EventSource 场景；退回 Authorization header——REST 场景）。
    // 注：auth_middleware 已验过有效性，此处仅取值不做密码学校验。
    let inbound_jwt = extract_query_token(raw_query)
        .or_else(|| extract_bearer(&parts.headers))
        .filter(|_| matches!(upstream, Upstream::Rsiot));

    // 摘掉 query 里的 token=（rsiot 路径防泄漏；serve 路径 token 对 serve 零价值同样摘）。
    let clean_query = strip_token_from_query(raw_query);
    let upstream_url = if clean_query.is_empty() {
        format!("{base}{raw_path}")
    } else {
        format!("{base}{raw_path}?{clean_query}")
    };

    // 2. 构造出站请求：保 method，透传原 headers（Authorization 层2 自动满足），按需注入凭证。
    let mut req_builder = st.client.request(parts.method.clone(), &upstream_url);
    // hop-by-hop 头不透传（标准反代约定，Host 由 reqwest 按上游 URL 重设）。
    for (name, value) in parts.headers.iter() {
        if is_hop_by_hop(name) {
            continue;
        }
        req_builder = req_builder.header(name, value);
    }
    // rsiot 凭证注入（C1）：query token → Authorization: Bearer。
    if let Some(jwt) = inbound_jwt {
        let hv =
            HeaderValue::from_str(&format!("Bearer {jwt}")).expect("jwt contains invalid chars");
        req_builder = req_builder.header(axum::http::header::AUTHORIZATION, hv);
    }
    // 层3：serve cookie 注入。
    if let Some(tok) = cookie {
        // unwrap 安全：reasonix_token=<token> 是合法 HeaderValue（token 为 ASCII）。
        let hv = HeaderValue::from_str(&format!("reasonix_token={tok}"))
            .expect("serve token contains invalid header chars");
        req_builder = req_builder.header(reqwest::header::COOKIE, hv);
    }
    // 3. 请求体：axum Body → reqwest Body（POST submit 等用；GET/SSE 无 body）。
    let body_stream = body.into_data_stream();
    let req_body = reqwest::Body::wrap_stream(body_stream.map_err(std::io::Error::other));
    let upstream_req = req_builder.body(req_body);

    // 4. 发送。RequestBuilder.send() 内部 build + execute（省一次 .build()?）。
    let upstream_resp = match upstream_req.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(%e, %upstream_url, "upstream request failed");
            return (StatusCode::BAD_GATEWAY, "upstream unreachable").into_response();
        }
    };

    // 5. 透传响应：status + headers + 流式 Body。
    //    SSE 上游 Content-Type: text/event-stream 经 headers clone 原样保留。
    //    Body::from_stream(bytes_stream()) 是 axum 官方钦定模式（报告 §四悬念2）。
    let status = upstream_resp.status();
    // mut：headers_mut() 需 &mut self（不是 out_headers 要 mut，是 resp_builder 要 mut）。
    let mut resp_builder = Response::builder().status(status);
    let out_headers = resp_builder.headers_mut().expect("headers_mut ok");
    for (name, value) in upstream_resp.headers().iter() {
        if is_hop_by_hop(name) {
            continue;
        }
        out_headers.insert(name, value.clone());
    }
    // 防 nginx 等中间代理缓冲 SSE（报告 §七风险表）。
    if out_headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/event-stream"))
    {
        out_headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    }
    resp_builder
        .body(Body::from_stream(upstream_resp.bytes_stream()))
        .unwrap_or_else(|e| {
            tracing::error!(%e, "build response failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "gateway error").into_response()
        })
}

/// serve 内联斜杠命令前缀清单——这些命令走 submit handler 的前缀拦截分支
/// （serve.go: `/model ` / `/effort `，返回 204 不进 turn）。gateway 的 HINT 若
/// 注入到这些 input 前面，会破坏 serve 的前缀匹配（HINT 后 input 不再以 `/model ` 开头），
/// 导致模型/思考深度切换静默失效。故这些 input 必须原样透传、跳过 HINT 注入。
///
/// 带尾随空格（`/model ` 非 `/model`）对标 serve 的 `HasPrefix(trimmed, "/model ")`，
/// 避免误伤普通查询 `/models`（GET 端点，不会进 submit，但防御性区分）。
const SLASH_COMMAND_PREFIXES: &[&str] = &["/model ", "/effort "];

/// 判断 submit input 是否应注入 AGENT_TOOL_HINT。
///
/// 注入条件：input 存在且（去前导空白后）**不以** serve 内联斜杠命令前缀开头。
/// 斜杠命令（`/model X`、`/effort X`）原样透传，保持 serve 前缀拦截可识别。
/// 纯函数便于单测（input_should_get_hint / slash_command_skips_hint）。
fn input_should_get_hint(input: &str) -> bool {
    let trimmed = input.trim_start();
    !SLASH_COMMAND_PREFIXES
        .iter()
        .any(|p| trimmed.starts_with(p))
}

/// agent submit 工具提示（冷启动盲区缓解）。
///
/// 根因（记忆 serve-coldstart-blindspot）：模型首轮看不到/不愿查 MCP 工具
/// （capability list 284KB 被截断），退回 bash 直连 redis-cli。本提示注入在
/// `POST /api/agent/submit` 的 `input` 前缀——模型每轮被告知 rsiot 工具存在与
/// 用途，把"查数据走 MCP"变成默认路径。克制、只列最常用工具映射。
///
/// "经 use_capability 调用"经真实链路实测确认（SSE 流 notice：
/// `capability proxy: use_capability → mcp__rsiot__rsiot_read_tags`），非假设。
pub const AGENT_TOOL_HINT: &str = "\
【网关工具提示】本网关提供 rsiot MCP 工具(经 use_capability 调用):\
查设备实时数据用 rsiot_read_tags(传 dev_id)、列设备 rsiot_list_devices、\
实例状态 rsiot_list_instances、实例健康 rsiot_get_instance_stats、\
最近日志 rsiot_get_recent_logs(报文 trace 查 target=io_trace)、\
激活报文 trace 用 rsiot_start_packet_trace。\
涉及网关设备/实例/数据/日志/报文的任务,优先调用对应 MCP 工具,不要用 bash 直连 redis-cli/lux。";

/// agent submit 专用转发：`input` 字段前缀注入 `AGENT_TOOL_HINT`，其余原样透传。
///
/// 只挂在 `POST /api/agent/submit`（app.rs 的 /api/agent `nest` 子路由显式 `/submit`，
/// 剥前缀后其余 fallback 裸透传）。body 非 JSON/无 `input` 字段 → 原样透传（降级
/// 不破坏）；**超 1MB → 413 快速失败**（to_bytes 已消费原 body 无法透传）。
/// 注入后重构 Request 复用 `forward`（Serve cookie 注入 + SSE 透传逻辑零重复）。
pub async fn forward_agent_submit(State(st): State<Arc<GwState>>, req: Request) -> Response {
    let (mut parts, body) = req.into_parts();
    // submit body 是 JSON 文本（前端 agent.ts `{input: <text>}`），1MB 上限绰绰有余。
    let bytes = match axum::body::to_bytes(body, 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            // 超限（>1MB）或流错误：to_bytes 已消费原 body 无法原样透传，
            // 若带 stale Content-Length 转发会让上游收截断 body（502 + 误导日志）。
            // 直接 413 快速失败——语义清晰，前端 agent.ts 把状态码带给用户。
            tracing::warn!(%e, "agent submit body too large or stream error");
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                "submit body too large (max 1MB)",
            )
                .into_response();
        }
    };

    // 注入：仅当 body 是 JSON object 且 input 是字符串。
    let mut injected = false;
    let out_bytes = match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(mut v) => {
            let input = v
                .get("input")
                .and_then(|i| i.as_str())
                .map(|s| s.to_string());
            if let Some(original) = input {
                // 斜杠命令（/model /effort）跳过 HINT 注入：serve 用 input 前缀拦截
                // 这两条命令（serve.go），HINT 前置会破坏前缀匹配致命令失效。
                if !input_should_get_hint(&original) {
                    bytes.to_vec()
                } else {
                    v["input"] =
                        serde_json::Value::String(format!("{AGENT_TOOL_HINT}\n\n{original}"));
                    injected = true;
                    serde_json::to_vec(&v).unwrap_or_else(|_| {
                        injected = false;
                        bytes.to_vec()
                    })
                }
            } else {
                bytes.to_vec() // 无 input 字段 → 原样
            }
        }
        Err(_) => bytes.to_vec(), // 非 JSON → 原样
    };
    // 仅注入发生（body 长度变化）才删 Content-Length；未注入时保留原头（reqwest 对
    // 已知大小 Vec body 会重算，语义无差，但删头是多余写）。
    if injected {
        parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    }
    let new_req = Request::from_parts(parts, Body::from(out_bytes));
    forward(State(st), new_req, Upstream::Serve).await
}

/// hop-by-hop 头清单（RFC 7230 §6.1 +常见反代不透传项）。
///
/// 这些头是单跳语义，反代必须剥离，否则下游混淆。
fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "host"
            | "connection"
            | "proxy-authorization"
            | "proxy-authenticate"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}

/// 从 query 串提取 `token=<jwt>` 的值（不含 `token=` 前缀）。
///
/// EventSource 不能设 header，前端 JWT 经 `?token=<jwt>` 传入。auth_middleware 已校验；
/// 此处仅取值供 forward 注入 `Authorization: Bearer` 转发 rsiot（C1）。
fn extract_query_token(query: &str) -> Option<String> {
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix("token=").map(|s| s.to_string()))
}

/// 从 `Authorization: Bearer <jwt>` header 提取 JWT（REST 场景退路）。
fn extract_bearer(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer ").map(|t| t.to_string()))
}

/// 从 query 串摘除 `token=<...>`（防 JWT 泄漏到上游 URL/日志），保留其余业务 query。
///
/// 例如 `types=log&token=<jwt>&limit=200` → `types=log&limit=200`。
fn strip_token_from_query(query: &str) -> String {
    query
        .split('&')
        .filter(|kv| !kv.is_empty() && !kv.starts_with("token="))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// hop-by-hop 判定：Host/Connection 等不透传，Authorization/Content-Type 透传。
    #[test]
    fn hop_by_hop_detection() {
        assert!(is_hop_by_hop(&HeaderName::from_static("host")));
        assert!(is_hop_by_hop(&HeaderName::from_static("connection")));
        assert!(!is_hop_by_hop(&HeaderName::from_static("authorization")));
        assert!(!is_hop_by_hop(&HeaderName::from_static("content-type")));
    }

    /// query token 提取（C1）：EventSource 的 `?token=<jwt>` 能取出 JWT。
    #[test]
    fn extract_query_token_picks_token_param() {
        assert_eq!(
            extract_query_token("types=log&token=abc.def.ghi"),
            Some("abc.def.ghi".into())
        );
        assert_eq!(extract_query_token("types=log"), None);
        assert_eq!(extract_query_token(""), None);
    }

    /// query token 摘除（C1 token-leak）：保留业务 query，去掉 token=。
    #[test]
    fn strip_token_keeps_business_query() {
        assert_eq!(
            strip_token_from_query("types=log&token=secret&limit=200"),
            "types=log&limit=200"
        );
        assert_eq!(strip_token_from_query("token=secret"), "");
        assert_eq!(strip_token_from_query("types=log"), "types=log");
        assert_eq!(strip_token_from_query(""), "");
    }

    /// submit HINT 注入判断：serve 内联斜杠命令（/model /effort）跳过 HINT，
    /// 普通输入注入。对标 serve.go 的 `HasPrefix(trimmed, "/model ")` 前缀拦截——
    /// HINT 前置会破坏前缀匹配致模型/effort 切换失效。
    #[test]
    fn slash_command_input_skips_hint() {
        // 斜杠命令（带尾随空格，对标 serve 前缀）→ 不注入
        assert!(!input_should_get_hint("/model anthropic/claude"));
        assert!(!input_should_get_hint("/effort high"));
        // 前导空白不影响判定（trim_start）
        assert!(!input_should_get_hint("  /model x"));
        assert!(!input_should_get_hint("\t/effort max"));
    }

    /// submit HINT 注入判断回归：普通用户输入仍注入（回归保护，防误伤）。
    #[test]
    fn normal_input_still_gets_hint() {
        // 普通自然语言 → 注入
        assert!(input_should_get_hint("读 devA 的实时数据"));
        assert!(input_should_get_hint("帮我建一个 modbus 实例"));
        // /models 是 GET 端点查询（无尾随空格），不进 submit 命令拦截 → 仍注入
        assert!(input_should_get_hint("/models"));
        // 裸 /model 无参数（无尾随空格）serve 不拦截 → 仍注入（罕见，无害）
        assert!(input_should_get_hint("/model"));
        // 空 input → 注入分支不触发（caller 已判 Some），此处仅判定无前缀
        assert!(input_should_get_hint(""));
    }
}
