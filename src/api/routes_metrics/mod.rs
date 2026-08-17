//! `/metrics` 端点 handler(Prometheus 文本格式,免 JWT)。spec §3/§6/§8。

use crate::api::AppState;
use axum::extract::State;
use axum::response::IntoResponse;

pub(crate) mod render;

use render::{render_instances, render_process, InstanceRow, METRICS_CONTENT_TYPE};

/// `GET /metrics`(Prometheus 文本格式,免 JWT)。
///
/// 组合进程级与 per-instance 指标。per-instance 部分遍历 `mgr.status()`(进程级
/// state)、`config_snapshot`(分类)、lux `stat:<inst>`(health/计数)三者组装。
/// 单实例 stat 读失败时跳过该实例的 per-instance 计数行(tick/fail/duration/ts),
/// info 行照常输出(health 空串),进程级指标照常输出(失败降级,见 spec §6)。
pub async fn metrics(State(st): State<AppState>) -> axum::response::Response {
    let m = &st.metrics;
    let (cpu, mem) = crate::process_metrics::sample_process_resource();

    let mut body = render_process(m, st.hwid.as_ref(), st.version, cpu, mem);

    // per-instance:遍历实例 + 读 lux stat。
    let states = st.mgr.status();
    let mut rows: Vec<InstanceRow> = Vec::with_capacity(states.len());
    for (inst_id, state) in states {
        let class = st.mgr.config_snapshot(&inst_id);
        let (app_code, host_kind) = class
            .as_ref()
            .map(|c| (c.app_code.to_string(), c.host_kind.as_str()))
            .unwrap_or((String::new(), ""));
        // stat 读失败 → None(该实例 per-instance 计数缺,但 info 行仍输出)。
        let health = crate::db::stat::read(&st.lux, &inst_id)
            .await
            .ok()
            .flatten();
        rows.push(InstanceRow {
            inst_id,
            state,
            app_code,
            host_kind,
            health,
        });
    }
    body.push_str(&render_instances(&rows));

    (
        [(axum::http::header::CONTENT_TYPE, METRICS_CONTENT_TYPE)],
        body,
    )
        .into_response()
}
