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
pub mod routes_alarm;
pub mod routes_command;
pub mod routes_device;
pub mod routes_devreg;
pub mod routes_discovery;
pub mod routes_events;
pub mod routes_group;
pub mod routes_history;
pub mod routes_inst;
pub mod routes_logs;
pub mod routes_lua;
pub mod routes_metrics;
pub mod routes_model;
pub mod routes_rtdata;
pub mod routes_rtdata_subscribe;
pub mod routes_secret;
pub mod routes_stat;
pub mod routes_sys;
pub mod routes_tag;
pub mod routes_trace;
pub mod routes_user;
pub mod rtdata_watch;

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
    /// 历史引擎旁路计数（Phase 3 前置可见性）：None = 引擎未运行
    /// （disabled/init_failed），/metrics 据此省略 history family。
    /// `rows.rs` 无条件编译（P6），本字段随之一致无条件编译。
    pub history_stats: Option<Arc<crate::history::rows::HistoryStats>>,
    /// 历史引擎运行态（init 后不变）："ok" / "disabled" / "init_failed"；
    /// feature off 构建恒 "disabled"（feature 布尔在 handler 内 `cfg!` 求值，
    /// 不进 AppState）。
    pub history_engine_state: std::sync::Arc<str>,
    /// 历史读句柄（Phase 2 P1 无条件编译）：None = 读路径不可用，/api/v1/history
    /// 据 503 矩阵按 `history_engine_state` 映射 reason。`HistoryReader` trait
    /// 无条件编译（`history/mod.rs`），本字段零 cfg 散布；feature off 构建
    /// 恒 None。
    pub history_reader: Option<std::sync::Arc<dyn crate::history::HistoryReader>>,
    /// 历史治理观测句柄（Phase 3 P4 无条件编译）：None = 引擎未运行
    /// （disabled/init_failed/feature off），/metrics 与 /sysstatus 的治理
    /// family 据此省略。`HistoryTelemetry` trait 无条件编译
    /// （`history/mod.rs`），本字段零 cfg 散布；feature off 构建恒 None。
    pub history_telemetry: Option<std::sync::Arc<dyn crate::history::HistoryTelemetry>>,
    /// 实时数据订阅 watcher（订阅升级 Phase 2，P7/P9）：ksub 模式下消费
    /// `ksubscribe("rtdata:*")` 维护 per-devid 版本表；poll 模式常驻构造但不
    /// 启动消费循环（零开销承诺，P8）。无条件编译（T5 将接 subscribe producer
    /// 消费本服务做精准 HGETALL）。
    pub rtdata_watch: Arc<rtdata_watch::RtdataWatchService>,
}

/// 白名单路径（无需 JWT）。
pub const PUBLIC_PATHS: &[&str] = &["/api/v1/user/login", "/api/v1/user/register", "/metrics"];

pub fn is_public(path: &str) -> bool {
    PUBLIC_PATHS.contains(&path) || path.starts_with("/apitest") || path.starts_with("/scada")
}

// ---------- 历史治理快照 helper（供 routes_metrics/routes_sys 复用） ----------

/// 采集历史治理 telemetry 三元组（Phase 3 P4/P5）：`series_count()` 为
/// O(series) 分配 → `spawn_blocking` 不占 runtime worker；JoinErr（阻塞任务
/// panic/取消）→ `Default` 零快照防御降级。telemetry None 分支在调用方
/// `history_stats` Some 前置下理论不可达（lib.rs 与引擎 ok 成对注入），
/// 防御取零快照。
pub(crate) async fn history_telemetry_snapshot(
    st: &AppState,
) -> routes_metrics::render::TelemetrySnapshot {
    match st.history_telemetry.clone() {
        Some(t) => tokio::task::spawn_blocking(move || routes_metrics::render::TelemetrySnapshot {
            memory_used_bytes: t.memory_used_bytes(),
            memory_budget_bytes: t.memory_budget_bytes(),
            series: t.series_count(),
        })
        .await
        .unwrap_or_default(),
        None => routes_metrics::render::TelemetrySnapshot::default(),
    }
}

// ---------- lux Hash 读写 helper（审计 B4 下沉 db 层） ----------
// str_from_value/hgetall/hget 原定义于此——model::migrate 与 lua_north::compat
// 曾为此反向 use crate::api（全仓仅有的 2 个循环依赖环根因）。api/routes 内
// 继续经 re-export 使用（pub(crate)：编译期阻止 api 之外新增消费点）。
pub(crate) use crate::db::{hget, hgetall, str_from_value};

pub fn build_router(state: AppState) -> axum::Router {
    use axum::routing::{delete, get, post};
    let app = axum::Router::new()
        // Prometheus 抓取端点（免 JWT，机器出口；spec §3）
        .route("/metrics", get(routes_metrics::metrics))
        // secret 管理（阶段 B Q4/D6：lux secrets 表写入/掩码读/删除；默认 JWT
        // 保护，list 绝不回显明文、审计 ref 级不落值；脚本侧读取是 T5）
        .route("/api/v1/secret/set", post(routes_secret::set))
        .route("/api/v1/secret/list", get(routes_secret::list))
        .route("/api/v1/secret", delete(routes_secret::delete))
        // 活跃告警查询/清除（Q12 告警最小闭环，spec §4：默认 JWT 保护；clear 发
        // Clear 事件让 SSE 前端收敛；触发面是脚本 ctx:fire_event——T4，无 REST 端点）
        .route(
            "/api/v1/alarms",
            get(routes_alarm::list).delete(routes_alarm::delete),
        )
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
        .route("/api/v1/tags", get(routes_tag::list))
        .route("/api/v1/tags/legacy-export", get(routes_tag::legacy_export))
        .route("/api/v1/tag/set", post(routes_tag::set))
        .route("/api/v1/tag/batch", post(routes_tag::batch))
        .route("/api/v1/tag/delete", post(routes_tag::delete))
        // 历史时序读（Phase 2：单 series raw/降采样查询；默认 JWT 保护，
        // 503/400 矩阵见 routes_history）
        .route("/api/v1/history", get(routes_history::history))
        // 设备模型注册表（M-1；devreg 为过渡路径，M-2 接管 /api/v1/devices 系）
        .route("/api/v1/devmodels", get(routes_model::list))
        .route("/api/v1/devmodel/set", post(routes_model::set))
        .route("/api/v1/devmodel/update", post(routes_model::update))
        .route("/api/v1/devmodel/delete", post(routes_model::delete))
        .route("/api/v1/devreg", get(routes_devreg::list))
        .route("/api/v1/devreg/set", post(routes_devreg::set))
        .route("/api/v1/devreg/delete", post(routes_devreg::delete))
        .route(
            "/api/v1/migrate/inline-devices",
            post(routes_devreg::migrate_inline),
        )
        // 发现队列（M-4 T2；发现是建模工作流的入口，D-M3）
        .route("/api/v1/discoveries", get(routes_discovery::list))
        .route("/api/v1/discovery/clear", post(routes_discovery::clear))
        .route("/api/v1/discovery/adopt", post(routes_discovery::adopt))
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
        // /lua/reload 的 handler 依赖 lua_core::reload（T10 移入）→ lua 门控，
        // 注册移下方 cfg 块；scripts/save 等纯 fs 端点不门控。
        // Lua 脚本列表/回读（脚本编辑器 + MCP；?kind=lib → 共享库视图，T10/Q13）
        .route("/api/v1/lua/scripts", get(routes_lua::list_scripts))
        // Lua 脚本写入（AI 写脚本入口：MCP save_script 工具 / 浮动面板；路径白名单
        // scripts/<name>；?kind=lib → scripts/_lib/<org>/<name>.lua，T10）
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
        // 实时数据订阅（OPC UA 式：snapshot 全量 + 周期 datachange；
        // 设计 docs/designs/2026-08-23-rtdata-subscribe-plan.md）
        .route(
            "/api/v1/rtdatas/subscribe",
            get(routes_rtdata_subscribe::subscribe),
        )
        // 日志 ring buffer 快照(最近 N 条结构化日志;默认 JWT 保护)
        .route("/api/v1/logs/recent", get(routes_logs::recent))
        // SSE 实时推送(日志/报文/实例状态;默认 JWT 保护,走 cookie)
        .route("/api/v1/events", get(routes_events::events))
        // 静态前端
        .route("/apitest", get(frontend::index_redirect))
        .route("/scada", get(frontend::scada_terminal))
        .route("/", get(frontend::root_redirect));

    // Lua stdlib 清单/同步（设计 §3.5/§3.6 D1）：handler 依赖 lua_core → lua 门控；
    // sync 另需 reqwest → lua-http 门控。裁剪 feature 时端点随编译缺席（handler 同名
    // cfg 门控，路由注册须对应缺席，否则链不到 handler）。/lua/reload 同理（T10：
    // 核心移 lua_core::reload 后 handler 依赖 lua_core）。
    #[cfg(feature = "lua")]
    let app = app
        .route("/api/v1/lua/reload", post(routes_lua::reload))
        .route("/api/v1/lua/stdlib", get(routes_lua::stdlib_list));
    #[cfg(feature = "lua-http")]
    let app = app.route("/api/v1/lua/stdlib/sync", post(routes_lua::stdlib_sync));
    #[cfg(not(feature = "lua-http"))]
    let app = app;

    app.layer(axum::middleware::from_fn_with_state(
        state.clone(),
        auth::auth_middleware,
    ))
    .layer(tower_http::trace::TraceLayer::new_for_http())
    .with_state(state)
}
