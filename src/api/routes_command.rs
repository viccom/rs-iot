//! 命令下发路由：把外部写命令经 `CtlRegistry` 路由到运行中实例的 `cmd_rx`。
//!
//! `POST /api/v1/command/send`：投递 `CtlMessage` 到指定实例。默认受 JWT 保护（写命令需鉴权）。
//! 实例未运行返回 404；投递成功返回 `{"status":"ok"}`。
//!
//! 这是 rs-iot **自身的写入口**（非 MQTT），与 MQTT `<hwid>/command` 订阅、未来北向
//! UA Server/DA/Modbus slave/104 等写回调共用同一条 `CtlRegistry` 写总线。

use crate::api::AppState;
use crate::types::CtlMessage;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

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
}

/// `POST /api/v1/command/send`：投递写命令到运行中实例。
pub async fn send(State(st): State<AppState>, Json(req): Json<CommandReq>) -> Response {
    let inst = req.instance.to_string();
    let msg = CtlMessage {
        instance: req.instance,
        devid: req.devid,
        cmd: req.cmd,
        data: req.data,
    };
    if st.mgr.send_ctl(&inst, msg) {
        Json(serde_json::json!({"status": "ok"})).into_response()
    } else {
        err(StatusCode::NOT_FOUND, "instance not running")
    }
}

// ---- 局部类型/工具 ----
type StatusCode = axum::http::StatusCode;
type Response = axum::response::Response;

fn err(code: StatusCode, msg: &str) -> Response {
    (code, msg.to_string()).into_response()
}
