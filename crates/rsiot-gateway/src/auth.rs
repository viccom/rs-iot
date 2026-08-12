//! 层1 鉴权：gateway JWT 中间件（复用 rs-iot jwt_secret）。
//!
//! 设计（plan-gateway-tech-selection.md §四层1）：
//! - 前端登录 gateway 拿 JWT，gateway 验 JWT（`verify_token`）。
//! - gateway 与 rs-iot 共享 `jwt_secret`，故 gateway 颁的 JWT 也能直接调 rs-iot（层2 自动满足）。
//! - 对标 rs-iot `src/api/auth.rs`，但 gateway 不签发（登录端点 P0-B2 后置），只验证。
//!
//! PUBLIC_PATHS：gateway 自有的 `/api/gw/health` 免鉴权（健康探针用）。
//! 此外，**所有非 `/api/` 开头的路径**（静态前端 SPA）一律免鉴权——见 `is_public`。

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use jsonwebtoken::{decode, DecodingKey, Validation};
use serde::{Deserialize, Serialize};

use crate::proxy::GwState;

/// JWT claims（与 rs-iot `src/api/auth.rs:17` 同结构，sub=userId）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub exp: usize,
}

/// 免鉴权路径（gateway 自有端点）。
///
/// `/api/gw/health`：健康探针（无需凭证即可探活）。
/// `/api/gw/login`：登录端点（前端登录页提交凭证换 JWT，登录前无 JWT 可用）。
const PUBLIC_PATHS: &[&str] = &["/api/gw/health", "/api/gw/login"];

/// 路径是否免鉴权。
///
/// 鉴权边界是 `/api/*`：API 调用必须带 JWT；非 `/api/` 开头的路径（静态前端 SPA
/// 经 `static_assets::serve` fallback 提供，含登录页）一律公开——前端登录页本身要先加载
/// 才能提交凭证。`/api/gw/health` 等已在 PUBLIC_PATHS 内，保留显式清单便于扩展。
fn is_public(path: &str) -> bool {
    PUBLIC_PATHS.iter().any(|p| path.starts_with(p)) || !path.starts_with("/api/")
}

/// 验证 JWT，返回 sub（userId）。失败返 None。
///
/// 与 rs-iot `verify_token` 同逻辑（HS256 + 默认 Validation），共享 secret。
pub fn verify_token(token: &str, secret: &str) -> Option<String> {
    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )
    .ok()?;
    Some(data.claims.sub)
}

/// axum 鉴权中间件（对标 rs-iot `auth_middleware`）。
///
/// 放行 PUBLIC_PATHS；其余按以下顺序取 token 验证，失败 401：
/// 1. `Authorization: Bearer <jwt>`（REST / fetch 用）。
/// 2. URL query `?token=<jwt>`（EventSource 用——标准 EventSource API 不能设 header，
///    监控 SSE / AI 面板 SSE 必须经 query 传凭证）。
///
/// 状态类型 `Arc<GwState>`（与 build_router 的 with_state 对齐）。
pub async fn auth_middleware(State(st): State<Arc<GwState>>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    if is_public(&path) {
        return next.run(req).await;
    }
    let token = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer ").map(|t| t.to_string()))
        .or_else(|| {
            // EventSource 不能设 header，支持 ?token=<jwt> query 参数。
            req.uri().query().and_then(|q| {
                q.split('&')
                    .find_map(|kv| kv.strip_prefix("token=").map(|s| s.to_string()))
            })
        });
    match token {
        Some(t) => match verify_token(&t, st.jwt_secret.as_str()) {
            Some(_) => next.run(req).await,
            None => (StatusCode::UNAUTHORIZED, "invalid token").into_response(),
        },
        None => (StatusCode::UNAUTHORIZED, "missing or malformed token").into_response(),
    }
}
