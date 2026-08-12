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

/// 通用透传：剥 `strip_prefix` 后拼 `upstream_base + 剩余 path + query`，转发。
///
/// REST 与 SSE 走同一路径——上游返 `text/event-stream` 时，headers clone 保留 Content-Type，
/// `Body::from_stream(bytes_stream())` 原样吐字节，前端 EventSource 自解 `\n\n`。
///
/// `upstream` 决定基址 + cookie 注入。`strip_prefix` 是 axum `nest` 挂载的前缀（如 `/api/rsiot`）。
pub async fn forward(
    State(st): State<Arc<GwState>>,
    req: Request,
    upstream: Upstream,
    strip_prefix: &str,
) -> Response {
    let base = upstream.base(&st);
    let cookie = upstream.cookie(&st);

    // 1. 拼上游 URL：剥 nest 前缀，保 query。
    let (parts, body) = req.into_parts();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let remaining = path_and_query
        .strip_prefix(strip_prefix)
        .filter(|s| !s.is_empty())
        .unwrap_or("/");
    let upstream_url = format!("{base}{remaining}");

    // 2. 构造出站请求：保 method，透传原 headers（Authorization 层2 自动满足），按需注入 cookie。
    let mut req_builder = st.client.request(parts.method.clone(), &upstream_url);
    // hop-by-hop 头不透传（标准反代约定，Host 由 reqwest 按上游 URL 重设）。
    for (name, value) in parts.headers.iter() {
        if is_hop_by_hop(name) {
            continue;
        }
        req_builder = req_builder.header(name, value);
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

/// hop-by-hop 头清单（RFC 7230 §6.1 +常见反代不透传项）。
///
/// 这些头是单跳语义，反代必须剥离，否则下游混淆。
fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "host" | "connection" | "proxy-authorization" | "proxy-authenticate"
            | "te" | "trailers" | "transfer-encoding" | "upgrade"
    )
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
}
