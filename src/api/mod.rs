//! REST 管理 API（axum）。
//!
//! 端口默认 :8780。JWT 中间件 + 白名单（login/register/静态资源）+ X-Access-Key 直通。
//! 路由对标 Lite_IOT（见各 routes_*.rs）。

// REST 管理层为 rs-iot 内部实现（非库公开 API）。
// DTO/handler 的 missing_docs / missing_debug_implementations 以 expect 放行
//（若未来补全 doc/Debug，编译器会提示移除），pub(crate) 收紧留后续治理。
#![expect(missing_docs, missing_debug_implementations)]

use crate::plugin::instance::InstanceManager;
use lux::EmbeddedClient;
use std::sync::Arc;

pub mod auth;
pub mod frontend;
pub mod routes_command;
pub mod routes_device;
pub mod routes_events;
pub mod routes_group;
pub mod routes_inst;
pub mod routes_logs;
pub mod routes_lua;
pub mod routes_metrics;
pub mod routes_rtdata;
pub mod routes_stat;
pub mod routes_sys;
pub mod routes_trace;
pub mod routes_user;

/// axum 共享状态。
#[derive(Clone)]
pub struct AppState {
    pub lux: EmbeddedClient,
    pub mgr: Arc<InstanceManager>,
    pub hwid: crate::types::Hwid,
    pub jwt_secret: String,
    pub version: &'static str,
    pub metrics: std::sync::Arc<crate::process_metrics::ProcessMetrics>,
    /// 报文 trace 开关注册表（REST 端点激活/查询/关闭）。
    pub trace_registry: crate::io_trace::TraceRegistry,
    /// 日志 ring buffer（GET /api/v1/logs/recent 读）。
    pub log_ringbuffer: std::sync::Arc<crate::log_ringbuffer::LogRingBuffer>,
    /// SSE 事件总线（GET /api/v1/events 订阅；与 HostContext/TraceRegistry 共享同一 bus）。
    pub event_bus: crate::event_bus::EventBus,
    /// 应用配置快照（GET /api/v1/config 脱敏读；只读 Arc 共享零成本）。
    pub config: std::sync::Arc<crate::config::AppConfig>,
}

/// 白名单路径（无需 JWT）。
pub const PUBLIC_PATHS: &[&str] = &["/api/v1/user/login", "/api/v1/user/register", "/metrics"];

pub fn is_public(path: &str) -> bool {
    PUBLIC_PATHS.contains(&path) || path.starts_with("/apitest") || path.starts_with("/scada")
}

// ---------- lux Hash 读写 helper（供各 routes 复用） ----------

pub fn str_from_value(v: &lux::EmbeddedValue) -> String {
    match v {
        lux::EmbeddedValue::Bulk(b) => String::from_utf8_lossy(b).to_string(),
        lux::EmbeddedValue::Simple(s) => s.clone(),
        lux::EmbeddedValue::Int(i) => i.to_string(),
        _ => String::new(),
    }
}

/// `HGETALL key` → JSON 对象（每个 value 尝试解析为 JSON，失败保留原始字符串）。
pub async fn hgetall(lux: &lux::EmbeddedClient, key: &str) -> serde_json::Value {
    match lux.execute_value("HGETALL", &[key]).await {
        Ok(lux::EmbeddedValue::Array(arr)) => {
            let mut map = serde_json::Map::new();
            let mut i = 0;
            while i + 1 < arr.len() {
                let k = str_from_value(&arr[i]);
                let raw = str_from_value(&arr[i + 1]);
                let parsed = serde_json::from_str(&raw).unwrap_or(serde_json::Value::String(raw));
                map.insert(k, parsed);
                i += 2;
            }
            serde_json::Value::Object(map)
        }
        _ => serde_json::Value::Object(serde_json::Map::new()),
    }
}

pub async fn hget(lux: &lux::EmbeddedClient, key: &str, field: &str) -> Option<String> {
    match lux.execute_value("HGET", &[key, field]).await {
        Ok(v) if !matches!(v, lux::EmbeddedValue::Nil) => Some(str_from_value(&v)),
        _ => None,
    }
}

pub fn build_router(state: AppState) -> axum::Router {
    use axum::routing::{get, post};
    let app = axum::Router::new()
        // Prometheus 抓取端点（免 JWT，机器出口；spec §3）
        .route("/metrics", get(routes_metrics::metrics))
        // 系统信息与插件实例
        .route("/api/v1/sysinfo", get(routes_sys::sysinfo))
        .route("/api/v1/sysstatus", get(routes_sys::sysstatus))
        .route("/api/v1/config", get(routes_sys::config))
        .route("/api/v1/plugins", get(routes_sys::plugins))
        .route("/api/v1/inststatus", get(routes_sys::inststatus))
        .route("/api/v1/startinst", post(routes_sys::startinst))
        .route("/api/v1/stopinst", post(routes_sys::stopinst))
        // instance CRUD
        .route("/api/v1/insts", get(routes_inst::list))
        .route("/api/v1/inst/roster", get(routes_inst::roster))
        .route("/api/v1/inst/get", get(routes_inst::get))
        .route("/api/v1/inst/set", post(routes_inst::set))
        .route("/api/v1/inst/delete", post(routes_inst::delete))
        .route("/api/v1/purge", post(routes_inst::purge))
        .route("/api/v1/inst/stats", get(routes_stat::stats))
        // device CRUD
        .route("/api/v1/devices", get(routes_device::list))
        .route("/api/v1/device/get", get(routes_device::get))
        .route("/api/v1/device/set", post(routes_device::set))
        .route("/api/v1/device/delete", post(routes_device::delete))
        // user
        .route("/api/v1/users", get(routes_user::list))
        .route("/api/v1/user/get", get(routes_user::get))
        .route("/api/v1/user/set", post(routes_user::set))
        .route("/api/v1/user/delete", post(routes_user::delete))
        .route("/api/v1/user/verify", post(routes_user::verify))
        .route("/api/v1/user/changepass", post(routes_user::changepass))
        .route("/api/v1/user/checktoken", get(routes_user::checktoken))
        .route("/api/v1/user/login", post(routes_user::login))
        .route("/api/v1/user/logout", post(routes_user::logout))
        // group + accesskey
        .route("/api/v1/usergroups", get(routes_group::list))
        .route("/api/v1/usergroup/add", post(routes_group::add))
        .route("/api/v1/usergroup/remove", post(routes_group::remove))
        .route("/api/v1/accesskey/set", post(routes_group::set_accesskey))
        // Lua 实例重载/脚本（spec §13，默认 JWT 保护；实例指标走 /api/v1/inst/stats）
        .route("/api/v1/lua/reload", post(routes_lua::reload))
        .route("/api/v1/lua/scripts", get(routes_lua::list_scripts))
        // Lua 脚本写入（AI 写脚本入口：MCP save_script 工具 / 浮动面板；路径白名单 scripts/<name>）
        .route("/api/v1/lua/script/save", post(routes_lua::save_script))
        // Lua 脚本回读 / 版本管理（前端脚本编辑器：读内容 / 列历史版本 / 回滚）
        .route("/api/v1/lua/script", get(routes_lua::get_script))
        .route(
            "/api/v1/lua/script/versions",
            get(routes_lua::list_versions),
        )
        .route(
            "/api/v1/lua/script/rollback",
            post(routes_lua::rollback_script),
        )
        // 命令下发（写命令经 CtlRegistry 路由到实例 cmd_rx；默认 JWT 保护）
        .route("/api/v1/command/send", post(routes_command::send))
        // 报文 trace 开关（per-instance 运行时激活/查询/关闭；默认 JWT 保护）
        .route(
            "/api/v1/inst/trace",
            get(routes_trace::list)
                .post(routes_trace::start)
                .delete(routes_trace::stop),
        )
        // 实时数据查询
        .route("/api/v1/rtdatas", get(routes_rtdata::rtdatas))
        // 日志 ring buffer 快照(最近 N 条结构化日志;默认 JWT 保护)
        .route("/api/v1/logs/recent", get(routes_logs::recent))
        // SSE 实时推送(日志/报文/实例状态;默认 JWT 保护,走 cookie)
        .route("/api/v1/events", get(routes_events::events))
        // 静态前端
        .route("/apitest", get(frontend::index_redirect))
        .route("/scada", get(frontend::scada_terminal))
        .route("/", get(frontend::root_redirect));

    app.layer(axum::middleware::from_fn_with_state(
        state.clone(),
        auth::auth_middleware,
    ))
    .layer(tower_http::trace::TraceLayer::new_for_http())
    .with_state(state)
}
