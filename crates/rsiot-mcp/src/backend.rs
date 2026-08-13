//! rs-iot REST 后端抽象。
//!
//! 对标 dbx-mcp `backend.rs` 的 `DbxBackend` trait + `WebBackend`/`LocalBackend` 双实现。
//! rs-iot 简化：只有 `WebBackend`（调主进程 loopback REST），无 LocalBackend
//! （rs-iot 是独立常驻进程，MCP 子进程无法直接持 lux 连接，只能走 REST）。
//!
//! 鉴权：预签发 JWT（`Authorization: Bearer`），比 dbx 的 cookie session 流程更简单。
//! token 来源：环境变量 `RSIOT_JWT`（部署时 reasonix.toml 的 env 注入，或主进程签发一次写入）。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde_json::Value;
use thiserror::Error;

/// MCP 调主进程 REST 失败的错误。
///
/// 对外统一 `String`（MCP 工具返回友好文本），内部用本类型区分错误来源便于日志。
#[derive(Debug, Error)]
pub enum BackendError {
    /// 主进程不可达（rs-iot 未启动 / 端口错 / loopback 断）。
    #[error("rs-iot unreachable: {0}")]
    Unreachable(String),
    /// 主进程返非 2xx（含 401 鉴权失败、404 端点不存在、500 内部错）。
    #[error("rs-iot returned {status}: {body}")]
    HttpStatus { status: u16, body: String },
    /// 响应体非合法 JSON（rs-iot 返了非 JSON 内容，理论上不会发生）。
    #[error("invalid JSON response: {0}")]
    Json(String),
}

/// rs-iot REST 后端契约。
///
/// 所有工具方法返 `Result<Value, String>`——成功返 rs-iot REST 的原始 JSON（工具层按需整形），
/// 失败返人类可读字符串（MCP 工具错误以文本回 agent，不走协议级 Err）。
#[async_trait]
pub trait RsiotBackend: Send + Sync {
    /// `GET <base>/api/v1/<path>`，返响应 JSON。
    async fn get(&self, path: &str) -> Result<Value, BackendError>;

    /// `POST <base>/api/v1/<path>` 带 JSON body，返响应 JSON。
    async fn post(&self, path: &str, body: &Value) -> Result<Value, BackendError>;

    /// `DELETE <base>/api/v1/<path>`（rsiot_stop_packet_trace 关报文 trace 用，query 在 path 拼）。
    async fn delete(&self, path: &str) -> Result<Value, BackendError>;
}

/// 调 rs-iot 主进程 loopback REST 的 WebBackend。
///
/// - `base`：rs-iot 基址，如 `"http://127.0.0.1:8790"`（loopback，MCP 子进程与主进程同机）。
/// - `jwt`：预签发 JWT token（`Authorization: Bearer`）。rs-iot `auth_middleware` 验证。
/// - `client`：reqwest 连接池（复用）。connect_timeout 只覆盖 TCP 握手；**总请求
///   timeout 30s** 兜底进程级 stall（rs-iot 事件循环被 lua 脚本阻塞 / RSIOT_BASE
///   误配为 accept 后不响应的黑洞）——否则工具调用 future 永久挂起、agent 会话卡死。
///
/// 对标 dbx-mcp `WebBackend`，但鉴权从 cookie 改 JWT（更简单，rs-iot 原生支持）。
#[derive(Clone)]
pub struct WebBackend {
    base: String,
    jwt: Arc<String>,
    client: reqwest::Client,
}

/// 总请求超时（秒）。覆盖「连接成功但响应永不到来」的 stall 场景
/// （rs-iot runtime 阻塞 / 中间设备黑洞），超时后返 `Unreachable` 而非挂死。
const REQUEST_TIMEOUT_SECS: u64 = 30;

impl WebBackend {
    /// 构造 WebBackend。
    ///
    /// `base` 不带尾 `/`（拼路径时统一加）。`jwt` 为空串则不带 Authorization 头
    /// （仅 rs-iot PUBLIC_PATHS 端点如 `/metrics` 可匿名，其余会 401）。
    pub fn new(base: impl Into<String>, jwt: impl Into<String>) -> Result<Self, BackendError> {
        let base = base.into();
        let jwt = jwt.into();
        let client = reqwest::Client::builder()
            // loopback 同机，连接超时短即可；总 timeout 兜底 stall（见 WebBackend doc）。
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|e| BackendError::Unreachable(format!("build reqwest client: {e}")))?;
        Ok(Self {
            base,
            jwt: Arc::new(jwt),
            client,
        })
    }

    /// 拼带 JWT 的请求头。
    fn auth_headers(&self) -> HeaderMap {
        let mut h = HeaderMap::new();
        if !self.jwt.is_empty() {
            // unwrap 安全：Bearer <token> 是合法 HeaderValue（JWT 是 base64url+点，ASCII 可见）。
            let hv = HeaderValue::from_str(&format!("Bearer {}", self.jwt))
                .expect("jwt contains invalid header chars");
            h.insert(AUTHORIZATION, hv);
        }
        h
    }

    /// 统一发请求 + 解析响应。
    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, BackendError> {
        let url = format!("{}{}", self.base, path);
        let mut req = self
            .client
            .request(method, &url)
            .headers(self.auth_headers());
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| BackendError::Unreachable(format!("{url}: {e}")))?;
        let status = resp.status().as_u16();
        let text = resp
            .text()
            .await
            .map_err(|e| BackendError::Json(format!("read body: {e}")))?;
        if !(200..300).contains(&status) {
            return Err(BackendError::HttpStatus { status, body: text });
        }
        // 空体（204 或空 200）返 Null，避免 serde_json 解析空串报错。
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| BackendError::Json(format!("parse body: {e}")))
    }
}

#[async_trait]
impl RsiotBackend for WebBackend {
    async fn get(&self, path: &str) -> Result<Value, BackendError> {
        self.request(reqwest::Method::GET, path, None).await
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value, BackendError> {
        self.request(reqwest::Method::POST, path, Some(body)).await
    }

    async fn delete(&self, path: &str) -> Result<Value, BackendError> {
        self.request(reqwest::Method::DELETE, path, None).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 鉴权头构造：空 JWT 不带 Authorization，非空带 Bearer。
    /// 不依赖网络，纯逻辑单测。
    #[test]
    fn auth_headers_omits_when_jwt_empty() {
        let b = WebBackend::new("http://127.0.0.1:8790", "").unwrap();
        assert!(b.auth_headers().get(AUTHORIZATION).is_none());
    }

    #[test]
    fn auth_headers_sets_bearer_when_jwt_present() {
        let b = WebBackend::new("http://127.0.0.1:8790", "abc.def.ghi").unwrap();
        let headers = b.auth_headers();
        let v = headers.get(AUTHORIZATION).unwrap();
        assert_eq!(v.to_str().unwrap(), "Bearer abc.def.ghi");
    }
}
