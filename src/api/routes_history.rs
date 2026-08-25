//! 历史读端点（Phase 2 Task 1 骨架）：`GET /api/v1/history`（默认 JWT 保护）。
//!
//! - 参数（P3）：`dev_id`+`tag` 必填（单 series）；`start`/`end` 必填**秒**、
//!   半开 [start,end)（P2：REST 出入秒，内部 ×1000 ms）；`agg` ∈
//!   min/max/avg/sum/count/last 六值；`interval` 秒，与 agg 联动（有 agg 必有
//!   interval、有 interval 必有 agg，否则 400）；`limit` 缺省 10000、上限
//!   100000（取最旧 n，tsink 语义）；降采样桶数 `(end-start)/interval` 上限
//!   100000（超限 400——downsample 全窗聚合的读放大护栏，raw 有 limit 早停
//!   不受限）；
//! - 503 矩阵（P5）：reader 未注入（`AppState.history_reader == None`）时按
//!   engine_state 映射 reason（feature_not_compiled / history_disabled /
//!   init_failed / history_unavailable 兜底），body
//!   `{error:"history unavailable", reason}`；
//! - 200：`{dev_id, tag, points:[{ts(秒), value}...], count}`（ts = ts_ms/1000）；
//!   空窗/不存在 series 是 `Ok(vec![])` 而非错误（tsink 语义）；
//! - select 经 `spawn_blocking` 包裹（P6：长窗 raw 扫描可能 >1ms，不占
//!   runtime worker）；reader 的 `Err(String)` 分层映射（P12）：串含
//!   `bad request: ` 前缀（engine 侧对参数类 tsink 错误构造，见
//!   engine.rs `classify_err`）→ 400 去前缀返 `{error}`，其余 → 500。

use crate::api::AppState;
use crate::history::{HistoryAgg, HistoryQuery};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

/// `GET /api/v1/history` query（P3；时间参数一律秒）。
#[derive(Deserialize)]
pub struct HistoryParams {
    /// 设备 ID（必填，series 定位之一）。
    pub dev_id: Option<String>,
    /// tag 名（必填，作 metric 名）。
    pub tag: Option<String>,
    /// 窗口起点（秒，必填，含）。
    pub start: Option<i64>,
    /// 窗口终点（秒，必填，不含）。
    pub end: Option<i64>,
    /// 降采样聚合（六值，见 [`parse_agg`]；与 interval 联动）。
    pub agg: Option<String>,
    /// 降采样桶宽（秒，>0；与 agg 联动）。
    pub interval: Option<u64>,
    /// 返回点数上限（缺省 10000，上限 100000，取最旧 n）。
    pub limit: Option<usize>,
}

/// `limit` 缺省值（P3）。
const DEFAULT_LIMIT: usize = 10_000;
/// `limit` 上限（P3）。
const MAX_LIMIT: usize = 100_000;
/// 降采样桶数上限（R6 读放大护栏）：`(end-start)/interval` 超过即 400——
/// downsample 聚合输出规模与引擎扫描粒度的上界，与 `MAX_LIMIT` 同量级。
const MAX_BUCKETS: u64 = 100_000;

/// agg 参数解析：六值（P3，对齐 tsink 聚合推荐面），未知值 `None` → 400。
fn parse_agg(s: &str) -> Option<HistoryAgg> {
    match s {
        "min" => Some(HistoryAgg::Min),
        "max" => Some(HistoryAgg::Max),
        "avg" => Some(HistoryAgg::Avg),
        "sum" => Some(HistoryAgg::Sum),
        "count" => Some(HistoryAgg::Count),
        "last" => Some(HistoryAgg::Last),
        _ => None,
    }
}

/// 秒 → 毫秒换算（P2）：`|秒| > i64::MAX/1000` 时 `None`（checked 防溢出
/// panic——荒谬大时间戳是可构造请求，不打崩进程）。
fn secs_to_ms(secs: i64) -> Option<i64> {
    secs.checked_mul(1000)
}

/// 503 reason 映射（P5）：feature 未编译（`cfg!` 编译期求值，feature on 构建
/// 下该分支不可达）→ `feature_not_compiled`；engine_state "disabled" →
/// `history_disabled`；"init_failed" → `init_failed`；其余（理论不可达）兜底
/// `history_unavailable`。
fn history_reason(engine_state: &str) -> &'static str {
    if !cfg!(feature = "history") {
        "feature_not_compiled"
    } else {
        match engine_state {
            "disabled" => "history_disabled",
            "init_failed" => "init_failed",
            _ => "history_unavailable",
        }
    }
}

/// 400 响应（纯文本，照 routes_tag::err 范本）。
fn bad_request(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, msg.to_string()).into_response()
}

/// `GET /api/v1/history?dev_id=&tag=&start=&end=[&agg=&interval=][&limit=]`：
/// 单 series 历史读（raw 或降采样）。
pub async fn history(State(st): State<AppState>, Query(q): Query<HistoryParams>) -> Response {
    // ---- 参数校验（P3/P12；全在解析层拦截，不打到 reader） ----
    // 1) 必填四项
    let (Some(dev_id), Some(tag), Some(start), Some(end)) = (&q.dev_id, &q.tag, q.start, q.end)
    else {
        return bad_request("dev_id, tag, start, end 均为必填");
    };
    // 2) 半开窗：start < end
    if start >= end {
        return bad_request("start must < end");
    }
    // 3) agg/interval 联动（P3：整窗聚合坍缩 1 点的怪形态直接拒）
    if q.agg.is_some() && q.interval.is_none() {
        return bad_request("agg requires interval");
    }
    if q.interval.is_some() && q.agg.is_none() {
        return bad_request("interval requires agg");
    }
    // 4) agg 六值解析
    let agg = match q.agg.as_deref() {
        Some(s) => match parse_agg(s) {
            Some(a) => Some(a),
            None => {
                return bad_request("agg 须为 min/max/avg/sum/count/last 之一");
            }
        },
        None => None,
    };
    // 5) interval > 0
    if q.interval == Some(0) {
        return bad_request("interval must > 0");
    }
    // 6) 降采样桶数上限（R6 读放大护栏）：桶数 = (end-start)/interval 是聚合
    //    输出规模与引擎扫描粒度的上界——downsample 路径 tsink 侧全窗读入后
    //    聚合（rollup 物化仅部分加速），无护栏时大窗+窄桶构成重查询 DoS 面。
    //    raw 路径有 limit 分页早停（select_raw_series_page），不受此限。
    if let Some(iv) = q.interval {
        // start < end 已证（差为正），i64 正数 as u64 无损。
        if (end - start) as u64 / iv > MAX_BUCKETS {
            return bad_request("降采样桶数 (end-start)/interval 不得超过 100000");
        }
    }
    // 7) limit ≤ 100000（缺省 10000）
    let limit = match q.limit {
        Some(l) if l > MAX_LIMIT => {
            return bad_request("limit 不得超过 100000");
        }
        Some(l) => l,
        None => DEFAULT_LIMIT,
    };

    // ---- 503 矩阵（P5）：reader 未注入 → 按 engine_state 区分 reason ----
    let Some(reader) = st.history_reader.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "history unavailable",
                "reason": history_reason(&st.history_engine_state),
            })),
        )
            .into_response();
    };

    // ---- 读路径（P2 秒↔ms；P6 spawn_blocking；参数序 metric, devid, q） ----
    // 窗口秒 → ms（checked 防溢出）。
    let (Some(start_ms), Some(end_ms)) = (secs_to_ms(start), secs_to_ms(end)) else {
        return bad_request("start/end 秒值换算毫秒溢出");
    };
    // 联动校验后 agg 与 interval 同有无：agg 有则 interval 必在，一并换算 ms
    // （u64→i64 用 try_from——`as` 截断位模式会让 ≥2^63 的值变负绕过溢出检查，
    // 如 u64::MAX → -1 → -1000ms 负桶宽下推，只能靠引擎层兜底改文案）；
    // raw 查询（agg=None）downsample 直接 None。
    let downsample = match agg {
        Some(a) => {
            let Some(ms) = q
                .interval
                .and_then(|s| i64::try_from(s).ok())
                .and_then(secs_to_ms)
            else {
                return bad_request("interval 秒值换算毫秒溢出");
            };
            Some((ms, a))
        }
        None => None,
    };
    let query = HistoryQuery {
        start_ms,
        end_ms,
        downsample,
        limit,
    };
    let metric = tag.clone();
    let devid = dev_id.clone();
    let result = tokio::task::spawn_blocking(move || reader.select(&metric, &devid, &query))
        .await
        .unwrap_or_else(|e| Err(format!("select blocking 任务失败: {e}")));

    match result {
        Ok(points) => {
            // ts 秒基线（P2）：ts_ms / 1000；value 经 SampleValue untagged serde。
            let out: Vec<_> = points
                .iter()
                .map(|p| json!({"ts": p.ts_ms / 1000, "value": p.value}))
                .collect();
            let count = out.len();
            Json(json!({
                "dev_id": dev_id,
                "tag": tag,
                "points": out,
                "count": count,
            }))
            .into_response()
        }
        // P12 分层：`bad request: ` 前缀（engine 侧 classify_err 对参数类
        // tsink 错误构造）→ 400 去前缀返 {error}；其余（`history engine: `
        // 前缀的引擎类 / spawn_blocking 任务失败）→ 500。body 形与 500 对齐
        // （Json {error}；解析层 400 是纯文本，二者面不同源不混同）。
        Err(e) => {
            if let Some(msg) = e.strip_prefix("bad request: ") {
                (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response()
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": e })),
                )
                    .into_response()
            }
        }
    }
}
