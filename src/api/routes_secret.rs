//! 平台凭证 secret REST 管理面（阶段 B 设计 §3.7 Q4/D6）。默认 JWT 保护。
//!
//! - `POST   /api/v1/secret/set`  body `{ref, value}` → 写 lux `secrets` 表
//! - `GET    /api/v1/secret/list` → `[{ref, masked}]`（**绝不回显明文**，D6）
//! - `DELETE /api/v1/secret?ref=`  → 删除（幂等：不存在仍 200 ok）
//!
//! ref 规则（D6 受限前缀）：`secret:` + `[a-z0-9_-]`，见 [`crate::db::secrets::validate_ref`]。
//! 审计（Q4 ref 级不落值）：set/delete 各落一条 `[secret][audit]` 日志——who 由
//! JWT 访问日志侧承载（同 `routes_lua` stdlib sync 惯例），**日志里绝不出现 value**。
//! 脚本侧读取接口（`ctx:secret_get` + 实例授权清单）是 Task 5，不在本层。

use crate::api::AppState;
use crate::db::secrets;
use crate::error::RsIotError;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

/// `POST /api/v1/secret/set` 请求体。
#[derive(Deserialize)]
pub struct SetSecretReq {
    /// secret ref（`secret:` 前缀 + `[a-z0-9_-]`）。
    pub r#ref: String,
    /// 凭证明文（仅入 lux 存储；绝不入日志/响应——D6）。
    pub value: String,
}

/// `?ref=<r>` 查询参数（DELETE）。
#[derive(Deserialize)]
pub struct RefQuery {
    /// secret ref（校验同 set）。
    pub r#ref: String,
}

/// `POST /api/v1/secret/set` → 写入（覆盖旧值）。
///
/// ref 校验失败（无前缀/非法字符/空）→ 400（消息含合法字符集，可行动）；
/// lux 故障 → 500。成功 → `{"status":"ok","ref":...}`（不回显 value）。
pub async fn set(State(st): State<AppState>, Json(req): Json<SetSecretReq>) -> Response {
    match secrets::set(&st.lux, &req.r#ref, &req.value).await {
        Ok(()) => {
            // Q4 审计：ref 级不落值——日志绝不出现 value（D6）。
            tracing::info!("[secret][audit] set ref={}", req.r#ref);
            Json(serde_json::json!({"status": "ok", "ref": req.r#ref})).into_response()
        }
        Err(RsIotError::Config(m)) => err(StatusCode::BAD_REQUEST, &m),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// `GET /api/v1/secret/list` → `[{ref, masked}]`（按 ref 字典序，空表 → `[]`）。
///
/// 掩码规则见 [`secrets::mask_value`]；明文只存在于 lux，响应与日志均不出（D6）。
pub async fn list(State(st): State<AppState>) -> Response {
    match list_masked(&st.lux).await {
        Ok(items) => Json(items).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// `DELETE /api/v1/secret?ref=` → 删除（幂等：不存在仍 200 ok）。
///
/// ref 校验失败 → 400；lux 故障 → 500。
pub async fn delete(State(st): State<AppState>, Query(q): Query<RefQuery>) -> Response {
    match secrets::del(&st.lux, &q.r#ref).await {
        Ok(()) => {
            tracing::info!("[secret][audit] delete ref={}", q.r#ref);
            Json(serde_json::json!({"status": "ok", "ref": q.r#ref})).into_response()
        }
        Err(RsIotError::Config(m)) => err(StatusCode::BAD_REQUEST, &m),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// list 端点主体（可注入 lux 的小函数，函数级测试用——本仓无 REST router 级
/// 测试基建，`routes_lua::stdlib_list_payload` 先例）：HGETALL → 掩码 → 按 ref
/// 字典序稳定排序。**组装结果绝不包含明文**（D6）。
async fn list_masked(c: &lux::EmbeddedClient) -> Result<Vec<serde_json::Value>, RsIotError> {
    let mut pairs = secrets::list(c).await?;
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(pairs
        .iter()
        .map(|(r, v)| serde_json::json!({ "ref": r, "masked": secrets::mask_value(v) }))
        .collect())
}

// ---- 局部类型/工具（对标 routes_trace.rs / routes_lua.rs） ----
type StatusCode = axum::http::StatusCode;
type Response = axum::response::Response;

fn err(code: StatusCode, msg: &str) -> Response {
    (code, msg.to_string()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_client(tag: &str) -> lux::EmbeddedClient {
        let cfg = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: std::env::temp_dir()
                    .join(format!("routes-secret-{tag}-{}", std::process::id()))
                    .to_str()
                    .unwrap()
                    .to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let handle = lux::run_with_config(crate::db::build_lux_config(&cfg))
            .await
            .expect("lux");
        handle.client()
    }

    /// 验证意图（D6）：list 只返 `{ref, masked}`——明文绝不出现在组装结果；
    /// 掩码规则生效；多 ref 字典序稳定；空表 → `[]`。
    #[tokio::test]
    async fn list_masked_never_echoes_plaintext() {
        let c = test_client("masked").await;

        // 空表 → []
        assert_eq!(
            serde_json::to_value(list_masked(&c).await.unwrap()).unwrap(),
            serde_json::json!([]),
            "空表应返回空数组"
        );

        secrets::set(&c, "secret:onenet_key", "abcd12345678wxyz")
            .await
            .unwrap();
        secrets::set(&c, "secret:mqtt_pwd", "short").await.unwrap();

        let items = list_masked(&c).await.unwrap();
        let raw = serde_json::to_string(&items).unwrap();
        assert!(!raw.contains("abcd12345678wxyz"), "明文绝不回显（D6）");
        assert!(!raw.contains("short"), "短明文同样绝不回显");

        assert_eq!(
            serde_json::to_value(&items).unwrap(),
            serde_json::json!([
                {"ref": "secret:mqtt_pwd", "masked": "****"},
                {"ref": "secret:onenet_key", "masked": "abcd****wxyz"},
            ]),
            "字典序稳定 + 掩码规则（>8 前后4 / ≤8 全掩）"
        );
    }
}
