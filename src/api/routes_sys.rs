//! 系统/插件实例相关路由。

use crate::api::{history_telemetry_snapshot, str_from_value, AppState};
use crate::history::rows::WriteErrorKind;
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
    // sysinfo 现采进程 CPU%/mem（process_metrics 公共函数，/metrics 复用）。
    let (cpu, mem) = crate::process_metrics::sample_process_resource();
    // 历史引擎可见性 + Phase 3 治理字段（P6）：stats 仅 engine=="ok"
    // （Some）时非 null；水位/series/archived_tags/deadbands 与 stats 的
    // deadband_evaluated/errors_by_kind 仅 ok 态输出——非 ok 整体省略
    // （省略语义而非 null，与 stats 的 null 语义区分）。ratio 口径与
    // /metrics 一致（P2：suppressed/evaluated，分母 0 时 0）。
    let history = match &st.history_stats {
        Some(h) => {
            let snap = history_telemetry_snapshot(&st).await;
            let archived_tags = crate::model::cache::effective()
                .values()
                .filter(|p| p.history)
                .count();
            let evaluated = h
                .deadband_evaluated
                .load(std::sync::atomic::Ordering::Relaxed);
            let ratio = if evaluated > 0 {
                h.deadbanded.load(std::sync::atomic::Ordering::Relaxed) as f64 / evaluated as f64
            } else {
                0.0
            };
            let mut errors_by_kind = serde_json::Map::new();
            for kind in WriteErrorKind::ALL {
                errors_by_kind.insert(
                    kind.as_str().to_string(),
                    serde_json::json!(h.error_rows_of(kind)),
                );
            }
            serde_json::json!({
                "feature": cfg!(feature = "history"),
                "enabled": st.config.history.enabled,
                "engine": st.history_engine_state.as_ref(),
                "memory_used_bytes": snap.memory_used_bytes,
                "memory_budget_bytes": snap.memory_budget_bytes,
                "series": snap.series,
                "archived_tags": archived_tags,
                "deadbands": {
                    "evaluated": evaluated,
                    "ratio": ratio,
                },
                "stats": {
                    "sent": h.sent.load(std::sync::atomic::Ordering::Relaxed),
                    "written": h.written.load(std::sync::atomic::Ordering::Relaxed),
                    "dropped": h.dropped.load(std::sync::atomic::Ordering::Relaxed),
                    "deadbanded": h.deadbanded.load(std::sync::atomic::Ordering::Relaxed),
                    "deadband_evaluated": evaluated,
                    "skipped": h.skipped.load(std::sync::atomic::Ordering::Relaxed),
                    "lane_mismatch": h.lane_mismatch.load(std::sync::atomic::Ordering::Relaxed),
                    "nonfinite_skipped": h
                        .nonfinite_skipped
                        .load(std::sync::atomic::Ordering::Relaxed),
                    "dup_ts_skipped": h
                        .dup_ts_skipped
                        .load(std::sync::atomic::Ordering::Relaxed),
                    "ts_rejected": h
                        .ts_rejected
                        .load(std::sync::atomic::Ordering::Relaxed),
                    "errors": h.errors.load(std::sync::atomic::Ordering::Relaxed),
                    "errors_by_kind": serde_json::Value::Object(errors_by_kind),
                },
            })
        }
        None => serde_json::json!({
            "feature": cfg!(feature = "history"),
            "enabled": st.config.history.enabled,
            "engine": st.history_engine_state.as_ref(),
            "stats": serde_json::Value::Null,
        }),
    };
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
            "pipeline_batch_count": m.pipeline_batch_count.load(std::sync::atomic::Ordering::Relaxed),
            "pipeline_workers_active": m.pipeline_workers_active.load(std::sync::atomic::Ordering::Relaxed),
            "pipeline_worker_deaths": m.pipeline_worker_deaths_count.load(std::sync::atomic::Ordering::Relaxed),
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
        },
        // 历史引擎可见性（Phase 3 前置 + P6 治理扩展，构建见上方 history）。
        "history": history,
    }))
}

pub async fn plugins(State(_st): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "sources": crate::plugin::registry::source_names(),
        "sinks": crate::plugin::registry::sink_names(),
    }))
}

/// `GET /api/v1/config`：返回脱敏后的配置快照。
///
/// 供 AI agent（MCP `get_config` 工具）和运维读配置——**敏感字段遮蔽**
/// （`jwt_secret` / `mqtt.password` / `mqtt.username` / `lux.password`），
/// 结构保留便于 agent 理解配置形态。
/// 对标 v1 §六安全边界"遮 jwt_secret/password/mqtt.password"。
pub async fn config(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(redact_config(&st.config))
}

/// 配置脱敏：深拷贝后遮蔽敏感字段（保留键、替换值为 `"***"`）。
///
/// 纯函数便于单测。遮蔽点：`jwt_secret`（顶层）、`mqtt.password`、`mqtt.username`、
/// `lux.password`（D-LUX-1：RESP 密码同属凭据，绝不回显明文）。
/// 其余字段原样（agent 需看 lux 端口/data_dir、bridge bind、log 级别等做诊断）。
pub fn redact_config(cfg: &crate::config::AppConfig) -> serde_json::Value {
    let mut v = serde_json::to_value(cfg).expect("AppConfig serializable");
    if let Some(obj) = v.as_object_mut() {
        if obj.contains_key("jwt_secret") {
            obj.insert("jwt_secret".into(), serde_json::json!("***"));
        }
        if let Some(serde_json::Value::Object(lux)) = obj.get_mut("lux") {
            if lux.contains_key("password") {
                lux.insert("password".into(), serde_json::json!("***"));
            }
        }
        if let Some(serde_json::Value::Object(mqtt)) = obj.get_mut("mqtt") {
            if mqtt.contains_key("username") {
                mqtt.insert("username".into(), serde_json::json!("***"));
            }
            if mqtt.contains_key("password") {
                mqtt.insert("password".into(), serde_json::json!("***"));
            }
        }
    }
    v
}

pub async fn inststatus(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!(st.mgr.status()))
}

#[derive(Deserialize)]
pub struct IdReq {
    pub inst_id: crate::types::InstId,
}

pub async fn startinst(State(st): State<AppState>, Json(req): Json<IdReq>) -> Response {
    // 配置双源：lux instances（REST 建的实例）；miss 回退 mgr 运行态快照——
    // runtime-only 实例（selfMonitor、config.toml/代码启动）stop 后经此重启
    // （照 cascade_delete 回退先例；stop 保留快照，remove_state/删除才清）。
    let inst: InstConfig = match st
        .lux
        .execute_value("HGET", &["instances", req.inst_id.as_ref()])
        .await
    {
        Ok(v) if !matches!(v, EmbeddedValue::Nil) => {
            match serde_json::from_str(&str_from_value(&v)) {
                Ok(c) => c,
                Err(e) => return err(StatusCode::BAD_REQUEST, &format!("invalid config: {e}")),
            }
        }
        _ => match st.mgr.config_snapshot(req.inst_id.as_ref()) {
            Some(c) => c,
            None => return err(StatusCode::NOT_FOUND, "instance not found"),
        },
    };
    // M-2 绑定事实校验（南向 Source）：每条绑定的设备须在 devreg 且模型可展开
    // （params 齐/地址可解析）——fail-fast 拒启动带指引，不带病运行（插件侧
    // 装配期也会再展开一次，此处给 REST 调用方即时反馈）。
    if inst.app_type == AppType::South {
        for b in &inst.devices {
            if let Err(e) = crate::model::expand::expand_binding(&st.lux, b).await {
                return err(
                    StatusCode::NOT_FOUND,
                    &format!(
                        "绑定设备 {} 展开失败: {e}（先 POST /api/v1/devreg/set 建设备\
                         或 POST /api/v1/migrate/inline-devices 迁移旧配置）",
                        b.devid
                    ),
                );
            }
        }
    }
    // 按方向分发（R4）：South→Source、North→Sink（此前 North 被 400 拒绝）。
    let result = match inst.app_type {
        AppType::South => st.mgr.start_source(inst).await,
        AppType::North => st.mgr.start_sink(inst).await,
    };
    match result {
        // 唯一启动守卫：已运行实例二次 startinst → 409（幂等与否交调用方决定）
        Err(e @ crate::error::RsIotError::InstanceAlreadyRunning(_)) => {
            err(StatusCode::CONFLICT, &e.to_string())
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Ok(()) => {
            // effective 物化缓存增量刷新（实例启动可能伴随模型/设备注册变更；
            // fail-soft：失败不影响启动结果——下个写路径兜底）。
            if let Err(e) = crate::model::cache::reload(&st.lux).await {
                tracing::warn!("[model] startinst 后 effective 缓存刷新失败: {e}");
            }
            Json(serde_json::json!({"status": "ok"})).into_response()
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    /// 敏感字段遮蔽，非敏感保留（规则7：业务逻辑变化时测试要失败——这里锁"绝不泄露密钥"）。
    #[test]
    fn redact_masks_secrets_keeps_structure() {
        // 用 toml 构造带敏感值的配置（避免 field_reassign_with_default lint）。
        let toml_str = r#"
jwt_secret = "real-secret-32-bytes-aaaaaaaaaaaaa"
[lux]
password = "resp-pass"
port = 6390
[mqtt]
username = "broker-user"
password = "broker-pass"
host = "broker.example.com"
port = 1883
"#;
        let cfg: AppConfig = toml::from_str(toml_str).expect("parse");

        let v = redact_config(&cfg);
        // 敏感字段必须遮蔽（绝不泄露）。
        assert_eq!(v["jwt_secret"], "***", "jwt_secret must be masked");
        assert_eq!(v["mqtt"]["username"], "***", "mqtt username must be masked");
        assert_eq!(v["mqtt"]["password"], "***", "mqtt password must be masked");
        assert_eq!(
            v["lux"]["password"], "***",
            "lux RESP password must be masked"
        );
        // 非敏感字段保留（agent 诊断需看端口/周期）。
        assert_eq!(v["mqtt"]["port"], 1883, "mqtt port preserved");
        assert_eq!(
            v["mqtt"]["host"], "broker.example.com",
            "mqtt host preserved"
        );
        assert_eq!(v["lux"]["port"], 6390, "lux port preserved");
    }

    /// 无密码的 broker（匿名）也不应泄露 username 结构。
    #[test]
    fn redact_handles_none_credentials() {
        let cfg = AppConfig::default(); // mqtt.username/password 默认 None
        let v = redact_config(&cfg);
        assert_eq!(v["jwt_secret"], "***");
    }
}
