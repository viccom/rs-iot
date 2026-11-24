//! 层1 鉴权：gateway JWT 中间件（复用 rs-iot jwt_secret）。
//!
//! 设计（plan-gateway-tech-selection.md §四层1）：
//! - 前端登录 gateway 拿 JWT，gateway 验 JWT（`verify_token`）。
//! - gateway 与 rs-iot 共享 `jwt_secret`，故 gateway 颁的 JWT 也能直接调 rs-iot（层2 自动满足）。
//! - 对标 rs-iot `src/api/auth.rs`，但 gateway 不签发（登录端点 P0-B2 后置），只验证。
//!
//! PUBLIC_PATHS：gateway 自有的 `/api/gw/health` 免鉴权（健康探针用）。
//! 此外，**所有非 `/api/` 开头的路径**（静态前端 SPA）一律免鉴权——见 `is_public`。

use std::sync::{Arc, Once};

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use jsonwebtoken::{decode, DecodingKey, Validation};
use serde::{Deserialize, Serialize};

use crate::proxy::GwState;

/// 显式选定 jsonwebtoken 的进程级 `CryptoProvider`（幂等）。
///
/// 全工作区（lux ≥0.37.1 / rs-iot 主 crate / 本 crate）的 `jsonwebtoken` feature
/// 已统一为 `aws_lc_rs`——单一 provider 时 jsonwebtoken 10.4 可自动判定，此调用
/// 是幂等兜底（防御未来某侧再引回第二 provider 导致的双提供方运行时 panic）。
/// `pub` 供不经过 `verify_token` 的调用方（集成测试自行签发 JWT）调用。
static CRYPTO_PROVIDER: Once = Once::new();

pub fn install_jwt_crypto_provider() {
    CRYPTO_PROVIDER.call_once(|| {
        let _ = jsonwebtoken::crypto::aws_lc::DEFAULT_PROVIDER.install_default();
    });
}

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
    install_jwt_crypto_provider();
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
/// 2. URL query `?token=<jwt>`（EventSource 兼容通道——标准 API 不能设 header）。
/// 3. `Cookie: token=<jwt>`（统一会话主通道：rs-iot login 下发 HttpOnly cookie，
///    浏览器对同源 REST 与 EventSource 自动携带，两套 SPA 共享登录态）。
///
/// 状态类型 `Arc<GwState>`（与 build_router 的 with_state 对齐）。
pub async fn auth_middleware(State(st): State<Arc<GwState>>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    if is_public(&path) {
        return next.run(req).await;
    }
    // 候选链 Bearer → query → Cookie；空值候选一律跳过落到下一通道（升级部署后
    // 未刷新的旧 bundle 会拼出 `&token=` 空值——空值不得遮蔽同时携带的有效 cookie，
    // 否则旧标签页 SSE 401 死循环重连）。JWT 非空，跳过空值无语义损失。
    let token = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .or_else(|| {
            // EventSource 不能设 header，支持 ?token=<jwt> query 参数。
            req.uri().query().and_then(|q| {
                q.split('&').find_map(|kv| {
                    kv.strip_prefix("token=")
                        .filter(|t| !t.is_empty())
                        .map(str::to_string)
                })
            })
        })
        .or_else(|| {
            // Cookie 精确名解析：`;` 分割后逐对 trim 再匹配 `token=` 前缀，
            // `reasonix_token=` 等后缀同名的 cookie 不得误当凭证。
            req.headers()
                .get_all(axum::http::header::COOKIE)
                .iter()
                .find_map(|v| {
                    let v = v.to_str().ok()?;
                    v.split(';')
                        .map(str::trim)
                        .find_map(|pair| pair.strip_prefix("token=").filter(|t| !t.is_empty()))
                })
                .map(str::to_string)
        });
    match token {
        Some(t) => match verify_token(&t, st.jwt_secret.as_str()) {
            Some(_) => next.run(req).await,
            None => (StatusCode::UNAUTHORIZED, "invalid token").into_response(),
        },
        None => (StatusCode::UNAUTHORIZED, "missing or malformed token").into_response(),
    }
}
