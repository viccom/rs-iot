//! Gateway 自有业务端点（H7，Pingora 的硬伤处，axum 原生 handler）。
//!
//! `GET /api/gw/health`：聚合探 rs-iot + serve 在线状态（免 JWT，PUBLIC_PATHS）。
//! MCP 连接状态难从 gateway 探（gateway 不 spawn rsiot-mcp，是 reasonix spawn），
//! P0 占位 "unknown"，P1 经 serve `/status` 间接判断。

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::proxy::GwState;

/// 健康响应。
#[derive(Serialize)]
pub struct HealthResp {
    /// rs-iot 主进程在线状态：`"online"` / `"offline"`。
    pub rsiot: &'static str,
    /// reasonix serve 在线状态。
    pub serve: &'static str,
    /// MCP 连接状态（gateway 视角不可见，P0 占位）。
    pub mcp: &'static str,
}

/// `GET /api/gw/health`：并发探 rs-iot + serve。
///
/// 探针语义：HTTP 响应成功（含 401 等客户端错误——说明端口活着，鉴权另算）算 online，
/// 连接失败（拒绝/超时）算 offline。
pub async fn health(State(st): State<Arc<GwState>>) -> Json<HealthResp> {
    // URL 先存 let 绑定，避免 tokio::join! 宏内 format! 临时值在 probe 借用期间释放。
    let rsiot_url = format!("{}/api/v1/sysstatus", st.rsiot_base);
    let serve_url = format!("{}/status", st.serve_base);
    let (rsiot, serve) = tokio::join!(
        probe(&st.client, &rsiot_url),
        probe(&st.client, &serve_url),
    );
    Json(HealthResp {
        rsiot: if rsiot { "online" } else { "offline" },
        serve: if serve { "online" } else { "offline" },
        mcp: "unknown",
    })
}

/// 探针：2s 超时，任何 HTTP 响应（含 4xx）算在线，连接错算离线。
async fn probe(client: &reqwest::Client, url: &str) -> bool {
    client
        .get(url)
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .is_ok()
}
