//! JWT 签发/验证（HS256，24h）+ 全局鉴权中间件。
//!
//! 对标 Lite_IOT `bRedka/jwt_util.go` + `auth_middleware.go`。
//! 白名单（login/register/静态资源）放行；其余要 `Authorization`/Cookie token，
//! 或 `X-Access-Key` == lux `accesskey`。

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use std::sync::Once;

use crate::api::{is_public, AppState};
use crate::error::TResult;

/// 显式选定 jsonwebtoken 的进程级 `CryptoProvider`（幂等，可重复调用）。
///
/// lux 自 v0.37.1（更新后的 mine/main）把 `jsonwebtoken` feature 从
/// `rust_crypto` 改成 `aws_lc_rs`，而 rs-iot 自己声明 `rust_crypto`；Cargo
/// feature 统一后两个 provider 同时启用，jsonwebtoken 10.4 无法自动判定，
/// 任何 encode/decode 都会 panic（`Could not automatically determine the
/// process-level CryptoProvider`）。官方给宿主的解法即
/// `CryptoProvider::install_default()`（进程内至多成功一次）。
///
/// 选 `rust_crypto`：与 rs-iot 自身依赖声明及旧 lux 一致，HS256 签发/校验
/// 语义不变。`pub` 供不经过本模块 `encode_token`/`verify_token` 的调用方
/// （如集成测试里自行签发 JWT 的 `make_jwt`）在 encode 前调用。
static CRYPTO_PROVIDER: Once = Once::new();

pub fn install_jwt_crypto_provider() {
    CRYPTO_PROVIDER.call_once(|| {
        let _ = jsonwebtoken::crypto::rust_crypto::DEFAULT_PROVIDER.install_default();
    });
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// subject = userId
    pub sub: String,
    /// 过期时间（Unix 秒）
    pub exp: usize,
}

pub fn encode_token(user_id: &str, secret: &str) -> TResult<String> {
    install_jwt_crypto_provider();
    let exp = chrono::Local::now().timestamp() as usize + 24 * 3600;
    let claims = Claims {
        sub: user_id.to_string(),
        exp,
    };
    Ok(encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )?)
}

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

/// axum 全局鉴权中间件。
pub async fn auth_middleware(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    if is_public(&path) {
        return next.run(req).await;
    }

    // X-Access-Key 直通（对标 Lite_IOT globalAccessKey）
    if let Some(ak) = req
        .headers()
        .get("x-access-key")
        .and_then(|v| v.to_str().ok())
    {
        if let Ok(lux::EmbeddedValue::Simple(stored)) =
            st.lux.execute_value("GET", &["accesskey"]).await
        {
            if !stored.is_empty() && stored == ak {
                return next.run(req).await;
            }
        }
    }

    // Authorization header（"Bearer <jwt>" 或裸 token）或 Cookie token
    let token = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.strip_prefix("Bearer ").unwrap_or(s).to_string())
        .or_else(|| {
            req.headers()
                .get(axum::http::header::COOKIE)
                .and_then(|v| v.to_str().ok())
                .and_then(|c| c.split("token=").nth(1).and_then(|s| s.split(';').next()))
                .map(|s| s.to_string())
        });

    let token = match token {
        Some(t) => t,
        None => return (StatusCode::UNAUTHORIZED, "missing token").into_response(),
    };
    match verify_token(&token, &st.jwt_secret) {
        Some(user_id) => {
            // 复审 M2：操作者身份注入 request extensions——审计面（secret set/delete
            // 等）经 Extension<AuthedUser> 取 who，不再依赖访问日志侧猜。
            let mut req = req;
            req.extensions_mut().insert(AuthedUser {
                user_id: user_id.clone(),
                via: "jwt",
            });
            next.run(req).await
        }
        None => (StatusCode::UNAUTHORIZED, "invalid token").into_response(),
    }
}

/// 已认证操作者（auth_middleware 注入；审计 who 的唯一来源）。
#[derive(Debug, Clone)]
pub struct AuthedUser {
    pub user_id: String,
    /// 认证通道（jwt；accesskey 直通路径不注入本类型）。
    pub via: &'static str,
}
