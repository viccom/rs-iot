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

use crate::api::{is_public, AppState};
use crate::error::TResult;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// subject = userId
    pub sub: String,
    /// 过期时间（Unix 秒）
    pub exp: usize,
}

pub fn encode_token(user_id: &str, secret: &str) -> TResult<String> {
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

    // Authorization header（"Bearer <jwt>" 或裸 token）或 Cookie token。
    // 空值候选跳过（与 gateway auth.rs 同约定）：空 Bearer/空 Authorization 不得
    // 遮蔽同时携带的有效 Cookie（gateway 反代旧 bundle 场景，2026-10-28 实测）。
    let token = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.strip_prefix("Bearer ").unwrap_or(s))
        .filter(|t| !t.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            req.headers()
                .get(axum::http::header::COOKIE)
                .and_then(|v| v.to_str().ok())
                .and_then(cookie_token)
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

/// 从 Cookie 头字符串里取 `token=` 对的值（精确名解析）。
///
/// 多对 cookie 以 `;` 分隔；名字必须恰为 `token`——`reasonix_token=` 等
/// 后缀同名的 cookie 不得误当凭证（gateway 反代场景浏览器可能同时带两者）。
fn cookie_token(cookie_header: &str) -> Option<&str> {
    cookie_header.split(';').map(str::trim).find_map(|pair| {
        pair.strip_prefix("token=")
            .filter(|v| !v.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::cookie_token;

    /// 基本形态：单对 token cookie。
    #[test]
    fn single_token_pair() {
        assert_eq!(cookie_token("token=jwt-abc"), Some("jwt-abc"));
    }

    /// token 在多对 cookie 中间。
    #[test]
    fn token_among_other_pairs() {
        assert_eq!(
            cookie_token("theme=dark; token=jwt-abc; density=compact"),
            Some("jwt-abc")
        );
    }

    /// 后缀同名 cookie（如 gateway 注入的 reasonix_token）不得误命中。
    /// 复现旧实现 `split("token=").nth(1)` 的缺陷：会把 `reasonix_token=x`
    /// 里的 x 当凭证值。
    #[test]
    fn suffix_named_cookie_is_rejected() {
        assert_eq!(cookie_token("reasonix_token=jwt-abc"), None);
        assert_eq!(cookie_token("a=1; reasonix_token=jwt-abc; b=2"), None);
    }

    /// 无 token 对。
    #[test]
    fn no_token_pair() {
        assert_eq!(cookie_token("theme=dark"), None);
        assert_eq!(cookie_token(""), None);
    }
}
