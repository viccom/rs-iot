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
pub async fn set(
    State(st): State<AppState>,
    auth: Option<axum::Extension<crate::api::auth::AuthedUser>>,
    Json(req): Json<SetSecretReq>,
) -> Response {
    match secrets::set(&st.lux, &req.r#ref, &req.value).await {
        Ok(()) => {
            // Q4 审计：ref 级不落值——日志绝不出现 value（D6）。
            tracing::info!(
                "[secret][audit] set ref={} by={}（via {}）",
                req.r#ref,
                auth.as_ref()
                    .map(|a| a.user_id.as_str())
                    .unwrap_or("accesskey"),
                auth.as_ref().map(|a| a.via).unwrap_or("accesskey")
            );
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
pub async fn delete(
    State(st): State<AppState>,
    auth: Option<axum::Extension<crate::api::auth::AuthedUser>>,
    Query(q): Query<RefQuery>,
) -> Response {
    match secrets::del(&st.lux, &q.r#ref).await {
        Ok(()) => {
            tracing::info!(
                "[secret][audit] delete ref={} by={}（via {}）",
                q.r#ref,
                auth.as_ref()
                    .map(|a| a.user_id.as_str())
                    .unwrap_or("accesskey"),
                auth.as_ref().map(|a| a.via).unwrap_or("accesskey")
            );
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
        let data_dir =
            std::env::temp_dir().join(format!("routes-secret-{tag}-{}", std::process::id()));
        // 满载 flaky 加固（终审计杂项登记）：data_dir 键含 pid，pid 回收复用时上一轮
        // 残留 WAL 会带出旧 secret 键 → 精确相等断言假红。起跑先整删（照
        // lua_transform_e2e test_app 复审 P2-5 同款修法，不改被测行为）。
        let _ = std::fs::remove_dir_all(&data_dir);
        let cfg = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: data_dir.to_str().unwrap().to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let handle = lux::run_with_config(crate::db::build_lux_config(&cfg))
            .await
            .expect("lux");
        // lux ≥0.37.1：ServerHandle drop 即发起优雅停机——测试 helper 只回
        // client，绑定保活至函数尾（进程退出统一回收）。
        let client = handle.client();
        std::mem::forget(handle); // lux ≥0.37.1：drop=停机——async helper 栈帧随 future 完成销毁，绑定保活无效，须 forget
        client
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
        // 注意：5 字符明文 "short" 的掩码 "****hort" 含子串 "hort" 但不含 "short"——
        // 断言用完整明文（D6 绝不回显的语义单元）。
        assert!(!raw.contains("short"), "短明文同样绝不回显");

        assert_eq!(
            serde_json::to_value(&items).unwrap(),
            serde_json::json!([
                {"ref": "secret:mqtt_pwd", "masked": "****hort"},
                {"ref": "secret:onenet_key", "masked": "****wxyz"},
            ]),
            "字典序稳定 + 掩码规则（复审 M1：只显尾4，≤4 全掩）"
        );
    }
}
