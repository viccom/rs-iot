//! 系统/插件实例相关路由。

use crate::api::{str_from_value, AppState};
use crate::types::{AppType, InstConfig};
use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use lux::EmbeddedValue;
use serde::Deserialize;

pub async fn sysinfo(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({"hwid": st.hwid, "version": st.version}))
}

pub async fn sysstatus(State(st): State<AppState>) -> Json<serde_json::Value> {
    let dbsize = match st.lux.execute_value("DBSIZE", &[]).await {
        Ok(EmbeddedValue::Int(n)) => n,
        _ => 0,
    };
    let m = &st.metrics;
    // sysinfo 0.32 现采：进程 CPU% 与内存字节。
    // System::new_all() 构造并刷新；get_current_pid 返回 Result，失败兜底 Pid 0。
    let sys = sysinfo::System::new_all();
    let pid = sysinfo::get_current_pid().unwrap_or_else(|_| sysinfo::Pid::from(0_usize));
    let (cpu, mem) = sys
        .process(pid)
        .map(|p| (p.cpu_usage(), p.memory()))
        .unwrap_or((0.0, 0));
    Json(serde_json::json!({
        "status": "ok",
        "dbsize": dbsize,
        "instances": st.mgr.status(),
        "metrics": {
            "ctl_send_count": m.ctl_send_count.load(std::sync::atomic::Ordering::Relaxed),
            "ctl_dropped_count": m.ctl_dropped_count.load(std::sync::atomic::Ordering::Relaxed),
            "ctl_not_found_count": m.ctl_not_found_count.load(std::sync::atomic::Ordering::Relaxed),
            "pipeline_processed_count": m.pipeline_processed_count.load(std::sync::atomic::Ordering::Relaxed),
            "pipeline_write_fail_count": m.pipeline_write_fail_count.load(std::sync::atomic::Ordering::Relaxed),
            "hub": {
                "depth_last": m.hub_depth_last.load(std::sync::atomic::Ordering::Relaxed),
                "depth_max": m.hub_depth_max.load(std::sync::atomic::Ordering::Relaxed),
            },
            "mqtt": {
                "publish_count": m.mqtt_publish_count.load(std::sync::atomic::Ordering::Relaxed),
                "publish_fail_count": m.mqtt_publish_fail_count.load(std::sync::atomic::Ordering::Relaxed),
                "connected": m.mqtt_connected.load(std::sync::atomic::Ordering::Relaxed),
            },
        },
        "process": {
            "uptime_secs": m.started.elapsed().as_secs(),
            "cpu_pct": cpu,
            "mem_bytes": mem,
        }
    }))
}

pub async fn plugins(State(_st): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "sources": crate::plugin::registry::source_names(),
        "sinks": crate::plugin::registry::sink_names(),
    }))
}

pub async fn inststatus(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!(st.mgr.status()))
}

#[derive(Deserialize)]
pub struct IdReq {
    pub inst_id: crate::types::InstId,
}

pub async fn startinst(State(st): State<AppState>, Json(req): Json<IdReq>) -> Response {
    let raw = match st
        .lux
        .execute_value("HGET", &["instances", req.inst_id.as_ref()])
        .await
    {
        Ok(v) if !matches!(v, EmbeddedValue::Nil) => str_from_value(&v),
        _ => return err(StatusCode::NOT_FOUND, "instance not found"),
    };
    let inst: InstConfig = match serde_json::from_str(&raw) {
        Ok(c) => c,
        Err(e) => return err(StatusCode::BAD_REQUEST, &format!("invalid config: {e}")),
    };
    if inst.app_type != AppType::South {
        return err(
            StatusCode::BAD_REQUEST,
            "only south(source) instances can be started via REST",
        );
    }
    if let Err(e) = st.mgr.start_source(inst).await {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    Json(serde_json::json!({"status": "ok"})).into_response()
}

pub async fn stopinst(State(st): State<AppState>, Json(req): Json<IdReq>) -> Response {
    let _ = st.mgr.stop(req.inst_id.as_ref()).await;
    Json(serde_json::json!({"status": "ok"})).into_response()
}

// ---- 局部类型/工具 ----
type StatusCode = axum::http::StatusCode;
type Response = axum::response::Response;

fn err(code: StatusCode, msg: &str) -> Response {
    (code, msg.to_string()).into_response()
}
