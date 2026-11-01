//! 命令下发路由：把外部写命令经 `CtlRegistry` 路由到运行中实例的 `cmd_rx`。
//!
//! `POST /api/v1/command/send`：投递 `CtlMessage` 到指定实例。默认受 JWT 保护（写命令需鉴权）。
//! 实例未运行返回 404。
//!
//! **写回执（F-2，2026-11-01 协议语义审查）**：插件声明了写回执能力时
//! （`CtlRegistry::declare_write_ack`），本入口在 [`WRITE_ACK_TIMEOUT`] 内等待
//! `handle_write` 的业务结果：
//! - 成功 → `200 {"status":"ok","request_id":…,"acked":true}`
//! - 插件明确失败（编码拒绝/从站异常/未连接…）→ `422 {"status":"error",…}`
//! - 窗口内无回报 → `504 {"status":"timeout",…}`（投递成功但业务结果未知）
//!
//! 未声明回执能力的实例（外部插件/MQTT 旁路）保持原"投递即 200"契约，不引入等待。
//! 客户端可用 `"ack": false` 显式跳过等待。
//!
//! 这是 rs-iot **自身的写入口**（非 MQTT），与 MQTT `<hwid>/command` 订阅、未来北向
//! UA Server/DA/Modbus slave/104 等写回调共用同一条 `CtlRegistry` 写总线。

use crate::api::AppState;
use crate::types::CtlMessage;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// 写回执等待窗口：覆盖四家插件最长的单次写超时（EIP 库硬编码 10s），
/// 超出即 504——投递已发生，业务结果未知。
const WRITE_ACK_TIMEOUT: Duration = Duration::from_secs(12);

/// 写命令请求体。`data` 为嵌套 JSON 文本，由具体插件（opcua/modbus…）自行解析。
#[derive(Deserialize)]
pub struct CommandReq {
    /// 目标实例 ID（路由 key）。
    pub instance: crate::types::InstId,
    /// 目标设备 ID（业务校验用，不参与命令路由）。
    pub devid: String,
    /// 命令类型（如 `"write"`），插件按此分发。
    pub cmd: String,
    /// 命令载荷（JSON 文本），插件 `handle_write` 解析。
    pub data: String,
    /// 是否等待写回执（缺省 true）。仅在实例声明了回执能力时生效；
    /// `false` = 保持旧"投递即 200"行为（批量脚本/不等结果的场景）。
    #[serde(default)]
    pub ack: Option<bool>,
}

/// 进程内请求序号（`request_id` 唯一性只需进程内 + 时间维度）。
static REQ_SEQ: AtomicU64 = AtomicU64::new(1);

fn next_request_id(inst: &str) -> String {
    format!("{inst}-{}", REQ_SEQ.fetch_add(1, Ordering::Relaxed))
}

/// `POST /api/v1/command/send`：投递写命令到运行中实例。
pub async fn send(State(st): State<AppState>, Json(req): Json<CommandReq>) -> Response {
    let inst = req.instance.to_string();
    let wait_ack = req.ack.unwrap_or(true) && st.mgr.write_ack_supported(&inst);
    let request_id = wait_ack.then(|| next_request_id(&inst));
    let ack_rx = request_id.as_deref().map(|id| st.mgr.begin_write_ack(id));
    let msg = CtlMessage {
        instance: req.instance,
        devid: req.devid,
        cmd: req.cmd,
        data: req.data,
        request_id: request_id.clone(),
    };
    if !st.mgr.send_ctl(&inst, msg) {
        if let Some(id) = request_id.as_deref() {
            st.mgr.cancel_write_ack(id);
        }
        return err(StatusCode::NOT_FOUND, "instance not running");
    }
    let (Some(id), Some(rx)) = (request_id, ack_rx) else {
        // 未声明回执能力的实例：保持原契约（投递即 200）。
        return Json(serde_json::json!({"status": "ok"})).into_response();
    };
    match tokio::time::timeout(WRITE_ACK_TIMEOUT, rx).await {
        Ok(Ok(Ok(()))) => Json(serde_json::json!({
            "status": "ok",
            "request_id": id,
            "acked": true,
        }))
        .into_response(),
        Ok(Ok(Err(e))) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "status": "error",
                "request_id": id,
                "error": e,
            })),
        )
            .into_response(),
        Ok(Err(_)) => {
            // 回执槽被取消（实例退出/重启）：业务结果未知。
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "status": "unavailable",
                    "request_id": id,
                    "error": "write ack channel closed (instance stopped?)",
                })),
            )
                .into_response()
        }
        Err(_) => {
            st.mgr.cancel_write_ack(&id);
            (
                StatusCode::GATEWAY_TIMEOUT,
                Json(serde_json::json!({
                    "status": "timeout",
                    "request_id": id,
                    "error": format!("no write result within {WRITE_ACK_TIMEOUT:?}"),
                })),
            )
                .into_response()
        }
    }
}

// ---- 局部类型/工具 ----
type StatusCode = axum::http::StatusCode;
type Response = axum::response::Response;

fn err(code: StatusCode, msg: &str) -> Response {
    (code, msg.to_string()).into_response()
}
