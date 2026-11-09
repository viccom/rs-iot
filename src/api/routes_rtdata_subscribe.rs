//! 实时数据订阅端点（OPC UA 式订阅语义；设计见
//! docs/designs/2026-08-23-rtdata-subscribe-plan.md）。
//!
//! `GET /api/v1/rtdatas/subscribe?dev_id=a,b&tag=x&interval=1000` → SSE：
//! - **snapshot**：连接建立即逐设备推全量快照（HGETALL `rtdata:<dev>`，一设备一帧）；
//! - **datachange**：之后每 `interval`（默认 1s，clamp [100ms,60s]）重读订阅范围，
//!   与上次已推值比对（仅比 cell[2] 值串——ts 变化值不变不算），有变化/删除才推一帧。
//!
//! 机制取舍：**每连接读侧 poll-diff，不走 EventBus**——绕过 pipeline 聚合的
//! `RTDATA_NOTIFY_MAX_DEVS=50` 截断与 broadcast 容量 256 的 Lagged 丢帧（订阅定位
//! 外部客户端可靠数据面）；interval 天然 per-connection（对标 OPC UA publishing
//! interval）；断线重连即重新 snapshot 全量重同步。pipeline / event_bus 零改动
//! ——现有 `/api/v1/events` 与 lua_transform 订阅面不受影响。

use crate::api::{
    hgetall,
    routes_rtdata::tag_of,
    rtdata_watch::{Dirty, RtdataWatchService, WatchCursor},
    AppState,
};
use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::time::Duration;
use tokio_stream::wrappers::ReceiverStream;

/// 订阅周期下限 ms（防高频滥用——每连接每 tick 全量 HGETALL 的成本护栏）。
const MIN_INTERVAL_MS: u64 = 100;
/// 订阅周期上限 ms。
const MAX_INTERVAL_MS: u64 = 60_000;
/// 默认订阅周期 ms。
const DEFAULT_INTERVAL_MS: u64 = 1_000;

/// query 参数（均可选）。
#[derive(Deserialize)]
pub struct SubQuery {
    /// 设备 ID 逗号列表（精确匹配；缺省 = 全部设备，每 tick 重查 KEYS，新设备自动纳入）。
    pub dev_id: Option<String>,
    /// tag 模糊过滤（field 末段 contains；语义同 `GET /api/v1/rtdatas?tag=`）。
    pub tag: Option<String>,
    /// 订阅周期 ms（clamp [100, 60000]，缺省 1000）。
    pub interval: Option<u64>,
}

// ---------- 纯函数（Task 2，单测覆盖） ----------

/// interval clamp：缺省 DEFAULT，夹到 [MIN, MAX]。纯函数便于单测。
pub fn clamp_interval(raw: Option<u64>) -> u64 {
    raw.unwrap_or(DEFAULT_INTERVAL_MS)
        .clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS)
}

/// dev_id 逗号列表 → 去重集合。None/空白/全空段 → None（= 全部设备）。
pub fn parse_dev_ids(raw: Option<&str>) -> Option<HashSet<String>> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    let set: HashSet<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    if set.is_empty() {
        None
    } else {
        Some(set)
    }
}

/// cell（`[ts, localTime, value]`）→ 值串（diff 比较键）。非数组/越界/非串 → 整体
/// JSON 串兜底（防畸形 cell 误判恒等）。
pub fn value_str(cell: &serde_json::Value) -> String {
    cell.get(2)
        .and_then(serde_json::Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| cell.to_string())
}

/// 全订阅范围 diff：`current` = 本 tick 过滤后的 field → (值串, cell)；
/// `last_sent` = 已推基线 field → 值串。返回 (变更 field → cell, 消失 field 列表)。
///
/// **值串比较**——ts/localTime 变化值不变不算变更（对标 OPC UA DataChangeFilter
/// 的值触发语义；采集插件周期写回同值不产生通知）。
pub fn diff_scope(
    current: HashMap<String, (String, serde_json::Value)>,
    last_sent: &HashMap<String, String>,
) -> (serde_json::Map<String, serde_json::Value>, Vec<String>) {
    let mut now: HashSet<&str> = HashSet::with_capacity(current.len());
    let mut changes = serde_json::Map::new();
    for (field, (val, cell)) in &current {
        now.insert(field.as_str());
        if last_sent.get(field) != Some(val) {
            changes.insert(field.clone(), cell.clone());
        }
    }
    let removed: Vec<String> = last_sent
        .keys()
        .filter(|f| !now.contains(f.as_str()))
        .cloned()
        .collect();
    (changes, removed)
}

/// tag contains 过滤（field 末段匹配，复用 `GET /rtdatas` 的 tag_of 语义）。None 全保留。
pub fn filter_by_tag(
    m: serde_json::Map<String, serde_json::Value>,
    tag: Option<&str>,
) -> serde_json::Map<String, serde_json::Value> {
    match tag {
        None => m,
        Some(t) => m
            .into_iter()
            .filter(|(k, _)| tag_of(k).contains(t))
            .collect(),
    }
}

// ---------- producer（Task 4/5 组装纯函数） ----------

/// 读订阅范围内全部设备 → (dev, field→cell) 列表。
///
/// dev_filter=None → 每 tick `KEYS rtdata:*` 重查（新设备自动纳入订阅）；
/// Some → 固定设备列表（设备已删时 HGETALL 返回空 map → 其点位进 removed）。
async fn read_scope(
    lux: &lux::EmbeddedClient,
    dev_filter: &Option<HashSet<String>>,
) -> Vec<(String, serde_json::Map<String, serde_json::Value>)> {
    let devs: Vec<String> = match dev_filter {
        Some(set) => set.iter().cloned().collect(),
        None => match lux.execute_value("KEYS", &["rtdata:*"]).await {
            Ok(lux::EmbeddedValue::Array(keys)) => keys
                .iter()
                .filter_map(|k| match k {
                    lux::EmbeddedValue::Bulk(b) => Some(String::from_utf8_lossy(b).to_string()),
                    lux::EmbeddedValue::Simple(s) => Some(s.clone()),
                    _ => None,
                })
                .map(|k| k.strip_prefix("rtdata:").unwrap_or(&k).to_string())
                .collect(),
            _ => Vec::new(),
        },
    };
    let mut out = Vec::with_capacity(devs.len());
    for dev in devs {
        let obj = hgetall(lux, &format!("rtdata:{dev}")).await;
        if let serde_json::Value::Object(m) = obj {
            out.push((dev, m));
        }
    }
    out
}

// ---------- ksub 读源（Task 5，D8/P7：dirty-set 精准读） ----------

/// ksub tick 读源结果（与 `read_scope` 返回同构；P7）。
#[derive(Debug)]
enum ReadOutcome {
    /// epoch 变更（Lagged 重同步）：全量重读 + 重建后的新 epoch（游标已在
    /// `read_delta` 内重建——先快照后全量读，之后的 mark 版本 > 游标不丢帧）。
    Resync(
        u64,
        Vec<(String, serde_json::Map<String, serde_json::Value>)>,
    ),
    /// 仅脏设备（设备消失 = 空 map 保留键，`diff_scope` 自然出 removed，P9）。
    Partial(Vec<(String, serde_json::Map<String, serde_json::Value>)>),
}

/// ksub tick 读源：`dirty_since` 增量查询 → 脏设备精准 HGETALL（替代每 tick
/// 全量 `read_scope`；tick 节奏与 `diff_scope` 冻结不动，P7）。
///
/// - `Dirty::Delta` → 脏集 ∩ dev_filter（None = 全部脏设备）逐设备 HGETALL →
///   `Partial`（设备已删时 HGETALL 得空 map，保留键让 diff_scope 出 removed）；
/// - `Dirty::Full`（epoch 变更）→ **先**重建游标（`cursor()` 三元组快照）**再**
///   全量重读 → `Resync`（新 epoch 随结果返回，调用方更新游标）。
async fn read_delta(
    lux: &lux::EmbeddedClient,
    watch: &RtdataWatchService,
    dev_filter: &Option<HashSet<String>>,
    cursor: &mut WatchCursor,
) -> ReadOutcome {
    match watch.dirty_since(cursor) {
        Dirty::Full => {
            // 游标重建先于全量读：快照之后的 mark 版本 > 游标，下次 tick 报脏
            // 重读——重同步窗口内的写不丢帧（P7）。
            *cursor = watch.cursor();
            let epoch = cursor.epoch();
            ReadOutcome::Resync(epoch, read_scope(lux, dev_filter).await)
        }
        Dirty::Delta(devs) => {
            let selected: Vec<String> = match dev_filter {
                Some(set) => devs.into_iter().filter(|d| set.contains(d)).collect(),
                None => devs,
            };
            let mut out = Vec::with_capacity(selected.len());
            for dev in selected {
                let obj = hgetall(lux, &format!("rtdata:{dev}")).await;
                let m = match obj {
                    serde_json::Value::Object(m) => m,
                    // 设备消失/异常：空 map 保留键（diff_scope 出 removed，P9）。
                    _ => serde_json::Map::new(),
                };
                out.push((dev, m));
            }
            ReadOutcome::Partial(out)
        }
    }
}

/// field（`<devid>.<tag>`，tag 为末段，与 `tag_of` 互补）→ devid 段。
/// 无点 field 原样返回（`tag_key` 的 contains 分支无前缀，反解析不可得——
/// 保守归为自身，最多漏判归属，不误报）。
fn dev_segment(field: &str) -> &str {
    match field.rsplit_once('.') {
        Some((dev, _)) => dev,
        None => field,
    }
}

/// ksub `Partial` 基线投影：`last_sent` 限制到脏设备域（field 的 devid 段 ∈
/// 脏集）。非脏设备数据未变，不参与本轮变更/删除判定——若直接喂全量基线，
/// 非脏域 field 会全部误判 removed（R3 等价漂移）。
fn project_baseline(
    last_sent: &HashMap<String, String>,
    scope: &[(String, serde_json::Map<String, serde_json::Value>)],
) -> HashMap<String, String> {
    let dirty: HashSet<&str> = scope.iter().map(|(d, _)| d.as_str()).collect();
    last_sent
        .iter()
        .filter(|(f, _)| dirty.contains(dev_segment(f)))
        .map(|(f, v)| (f.clone(), v.clone()))
        .collect()
}

/// 组装喂 `diff_scope` 的 current：(dev, points) 列表 → tag 过滤 →
/// field → (值串, cell)。poll / ksub 两读源共用（R3：diff 输入同构）。
fn collect_current(
    scope: Vec<(String, serde_json::Map<String, serde_json::Value>)>,
    tag: Option<&str>,
) -> HashMap<String, (String, serde_json::Value)> {
    let mut current: HashMap<String, (String, serde_json::Value)> = HashMap::new();
    for (_dev, points) in scope {
        for (field, cell) in filter_by_tag(points, tag) {
            let v = value_str(&cell);
            current.insert(field, (v, cell));
        }
    }
    current
}

/// `GET /api/v1/rtdatas/subscribe`：OPC UA 式实时数据订阅（SSE）。
///
/// 连接建立：snapshot 逐设备全量推送 + 建基线；之后每 interval 读侧 diff，
/// 变化才推 datachange。客户端断开 → send 失败 → producer task 退出（资源回收）。
pub async fn subscribe(State(st): State<AppState>, Query(q): Query<SubQuery>) -> impl IntoResponse {
    let dev_filter = parse_dev_ids(q.dev_id.as_deref());
    let tag_filter = q.tag.clone();
    let interval = Duration::from_millis(clamp_interval(q.interval));
    tracing::debug!(
        "[rtdata-subscribe] client connected: dev={:?} tag={:?} interval={:?}",
        dev_filter,
        tag_filter,
        interval
    );
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(8);
    let lux = st.lux.clone();
    let watch = st.rtdata_watch.clone();
    let sub_impl = st.config.rtdata.sub_impl;
    tokio::spawn(async move {
        run(&lux, &watch, sub_impl, dev_filter, tag_filter, interval, tx).await;
        tracing::debug!("[rtdata-subscribe] client disconnected");
    });
    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}

/// producer 主体：snapshot（Phase 1）+ 周期 diff（Phase 2）。tx 断（客户端断开）即返回。
///
/// Phase 2 读源双 impl（P8）：ksub → `read_delta` dirty-set 精准读（P7/D8）；
/// poll → 原 `read_scope` 轮询路径（逐字保留，回退开关）。两分支组装出同构
/// current 喂同一 `diff_scope`（一行不动），帧逻辑等价（R3/P11）。
async fn run(
    lux: &lux::EmbeddedClient,
    watch: &RtdataWatchService,
    sub_impl: crate::config::SubImpl,
    dev_filter: Option<HashSet<String>>,
    tag_filter: Option<String>,
    interval: Duration,
    tx: tokio::sync::mpsc::Sender<Result<Event, Infallible>>,
) {
    let tag = tag_filter.as_deref();
    let mut last_sent: HashMap<String, String> = HashMap::new();

    // ---- ksub 游标基线（P7）：必须在 snapshot **之前**取（`cursor` 文档语义：
    // "先取快照再全量读"）——snapshot 期间发生的 mark 版本 > 游标，首 tick 报
    // 脏重读，不丢帧（值与基线相同则 diff 静默无帧，无代价）。若后置到
    // snapshot 之后，半程中已推设备的 mark 会被游标吸收，该变更永不推送
    // （ksub_change_during_snapshot_not_lost 反压构造锚定）。poll 档不消费
    // 版本表，保持原语义。----
    let mut cursor = watch.cursor();

    // ---- Phase 1: snapshot（逐设备一帧，建基线）----
    for (dev, points) in read_scope(lux, &dev_filter).await {
        let filtered = filter_by_tag(points, tag);
        if filtered.is_empty() {
            continue; // tag 过滤后无点（或空 hash）不推空帧
        }
        for (field, cell) in &filtered {
            last_sent.insert(field.clone(), value_str(cell));
        }
        if let Ok(ev) = Event::default()
            .event("snapshot")
            .json_data(json!({ "dev": dev, "points": filtered }))
        {
            if tx.send(Ok(ev)).await.is_err() {
                return;
            }
        }
    }

    // ---- Phase 2: 周期 diff（值变/删除才推）----
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await; // interval 首拍立即完成——快照刚读，消费掉跳过
    loop {
        ticker.tick().await;
        let (changes, removed) = match sub_impl {
            // poll：原轮询路径逐字保留（P8 回退开关）。
            crate::config::SubImpl::Poll => {
                let current = collect_current(read_scope(lux, &dev_filter).await, tag);
                diff_scope(current, &last_sent)
            }
            // ksub：dirty-set 精准读（D8/P7）——只 HGETALL 落后设备。
            crate::config::SubImpl::Ksub => {
                match read_delta(lux, watch, &dev_filter, &mut cursor).await {
                    ReadOutcome::Resync(_epoch, scope) => {
                        // Lagged 重同步：全量重读；游标已在 read_delta 内重建
                        // （新 epoch 含于游标，返回值仅供日志/测试观察）。
                        let current = collect_current(scope, tag);
                        diff_scope(current, &last_sent)
                    }
                    ReadOutcome::Partial(scope) => {
                        // current 只含脏设备 field：基线投影到脏设备域（非脏设备
                        // 数据未变不参与判定；直接喂全量基线会整域误报 removed）。
                        let proj = project_baseline(&last_sent, &scope);
                        let current = collect_current(scope, tag);
                        diff_scope(current, &proj)
                    }
                }
            }
        };
        // 基线更新（变更覆盖 + 删除移除），与推帧解耦——推帧失败即退出无需回滚。
        for (field, cell) in &changes {
            last_sent.insert(field.clone(), value_str(cell));
        }
        for field in &removed {
            last_sent.remove(field);
        }
        if changes.is_empty() && removed.is_empty() {
            continue;
        }
        if let Ok(ev) = Event::default().event("datachange").json_data(json!({
            "changes": changes,
            "removed": removed,
            "ts": crate::event_bus::now_iso8601(),
        })) {
            if tx.send(Ok(ev)).await.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn clamp_interval_defaults_and_bounds() {
        assert_eq!(clamp_interval(None), DEFAULT_INTERVAL_MS);
        assert_eq!(clamp_interval(Some(0)), MIN_INTERVAL_MS);
        assert_eq!(clamp_interval(Some(50)), MIN_INTERVAL_MS);
        assert_eq!(clamp_interval(Some(2000)), 2000);
        assert_eq!(clamp_interval(Some(u64::MAX)), MAX_INTERVAL_MS);
    }

    #[test]
    fn parse_dev_ids_none_empty_or_all_blank() {
        assert!(parse_dev_ids(None).is_none());
        assert!(parse_dev_ids(Some("")).is_none());
        assert!(parse_dev_ids(Some("  ")).is_none());
        assert!(
            parse_dev_ids(Some(",,")).is_none(),
            "全空段 = None（全部设备）"
        );
    }

    #[test]
    fn parse_dev_ids_splits_trims_and_dedups() {
        let s = parse_dev_ids(Some("devA, devB ,devA")).unwrap();
        assert_eq!(s.len(), 2, "去重");
        assert!(s.contains("devA") && s.contains("devB"), "trim 生效");
    }

    #[test]
    fn value_str_takes_third_element() {
        assert_eq!(value_str(&json!([1, "t", "1.5"])), "1.5");
        assert_eq!(value_str(&json!([1, "t", "true"])), "true");
    }

    #[test]
    fn value_str_malformed_falls_back_to_json() {
        // 越界（无第三元素）→ 整体 JSON 串兜底，而非空串误判恒等。
        assert_eq!(value_str(&json!([1, "t"])), json!([1, "t"]).to_string());
        // 非数组 → 同上。
        assert_eq!(value_str(&json!("x")), "\"x\"");
    }

    /// 构造 diff 输入：field → (值串, cell)。
    fn scope(pairs: &[(&str, &str)]) -> HashMap<String, (String, serde_json::Value)> {
        pairs
            .iter()
            .map(|(f, v)| ((*f).to_string(), ((*v).to_string(), json!([1, "lt", v]))))
            .collect()
    }

    /// 构造已推基线：field → 值串。
    fn baseline(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(f, v)| ((*f).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn diff_detects_value_change_and_new_field() {
        let last = baseline(&[("d.t1", "1"), ("d.t2", "2")]);
        let cur = scope(&[("d.t1", "9"), ("d.t2", "2"), ("d.t3", "3")]);
        let (changes, removed) = diff_scope(cur, &last);
        assert_eq!(changes.len(), 2, "t1 值变 + t3 新增");
        assert!(changes.contains_key("d.t1") && changes.contains_key("d.t3"));
        assert!(removed.is_empty());
    }

    #[test]
    fn diff_same_value_new_ts_not_a_change() {
        // OPC UA 语义核心：值同 ts 变（周期写回同值）不产生通知。
        let last = baseline(&[("d.t1", "1")]);
        let cur: HashMap<String, (String, serde_json::Value)> = [(
            "d.t1".to_string(),
            ("1".to_string(), json!([999, "lt", "1"])),
        )]
        .into_iter()
        .collect();
        let (changes, removed) = diff_scope(cur, &last);
        assert!(
            changes.is_empty() && removed.is_empty(),
            "值未变不推: {changes:?}"
        );
    }

    #[test]
    fn diff_reports_removed_fields() {
        let last = baseline(&[("d.t1", "1"), ("d.t2", "2")]);
        let cur = scope(&[("d.t1", "1")]);
        let (changes, removed) = diff_scope(cur, &last);
        assert!(changes.is_empty());
        assert_eq!(removed, vec!["d.t2".to_string()]);
    }

    #[test]
    fn diff_empty_current_removes_all() {
        // 设备删除后其全部点位进 removed（e2e removed_on_device_delete 的纯函数面）。
        let last = baseline(&[("d.t1", "1")]);
        let (changes, removed) = diff_scope(HashMap::new(), &last);
        assert!(changes.is_empty());
        assert_eq!(removed.len(), 1);
    }

    #[test]
    fn filter_by_tag_keeps_matching_tail() {
        let m: serde_json::Map<String, serde_json::Value> = [
            ("devA.temp".to_string(), json!([0, "t", "1"])),
            ("devA.hum".to_string(), json!([0, "t", "2"])),
        ]
        .into_iter()
        .collect();
        let got = filter_by_tag(m, Some("temp"));
        assert_eq!(got.len(), 1);
        assert!(got.contains_key("devA.temp"));
        // None → 全保留。
        let m2: serde_json::Map<String, serde_json::Value> =
            [("a.b".to_string(), json!(1))].into_iter().collect();
        assert_eq!(filter_by_tag(m2, None).len(), 1);
    }

    // ---------- ksub 读源 read_delta（Task 5 单元层红门，真 lux） ----------

    use crate::api::rtdata_watch::RtdataWatchService;

    /// 单测 lux 基建（照 src/db/rtdata.rs tests 的 start_lux 形态：独立临时
    /// 目录 + 端口 0，handle RAII 退出）。
    async fn start_lux(name: &str) -> (lux::ServerHandle, lux::EmbeddedClient) {
        let dir =
            std::env::temp_dir().join(format!("sub-delta-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        crate::db::prepare_lux_data_dir(&dir).unwrap();
        let mut sc = lux::ServerConfig::default();
        sc.port = 0; // OS 分配空闲端口，避免与运行中 rs-iot/redis 冲突
        sc.data_dir = dir.to_str().unwrap().to_string();
        sc.storage = lux::StorageConfig {
            mode: lux::StorageMode::Tiered,
            dir: sc.data_dir.clone(),
        };
        sc.save_interval = std::time::Duration::from_secs(60);
        let h = lux::run_with_config(sc).await.unwrap();
        let c = h.client();
        (h, c)
    }

    /// 铺一设备点位：HSET rtdata:<dev>，field 形如 <dev>.<tag>，cell 为
    /// `[ts, localTime, value]` JSON 串（与 db::rtdata::write_batch 同构）。
    async fn seed_rtdata(lux: &lux::EmbeddedClient, dev: &str, pts: &[(&str, &str)]) {
        let key = format!("rtdata:{dev}");
        let mut args: Vec<String> = vec![key];
        for (t, v) in pts {
            args.push(format!("{dev}.{t}"));
            args.push(json!([1, "lt", v]).to_string());
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        lux.execute("HSET", &refs).await.unwrap();
    }

    /// Partial 只读脏设备：3 设备铺数 → mark 仅 devA → read_delta 返回只含
    /// devA；再 mark devB → 只含 devB（devA 游标已被上次查询推进，P7）。
    #[tokio::test]
    async fn read_delta_only_reads_dirty_devices() {
        let (_handle, lux) = start_lux("dirty").await;
        let watch = RtdataWatchService::new();
        for dev in ["devA", "devB", "devC"] {
            seed_rtdata(&lux, dev, &[("t1", "1")]).await;
        }

        // 游标先于事件建立(真实序:连接建游标在前,事件在后)——mark 后游标
        // 即落后,首查报脏。
        let mut cur = watch.cursor();
        watch.mark("devA");
        match read_delta(&lux, &watch, &None, &mut cur).await {
            ReadOutcome::Partial(scope) => {
                assert_eq!(scope.len(), 1, "只读脏设备 devA: {scope:?}");
                assert_eq!(scope[0].0, "devA");
                assert!(scope[0].1.contains_key("devA.t1"), "{scope:?}");
            }
            other => panic!("无 epoch 变更应为 Partial: {other:?}"),
        }

        // devA 游标已推进：mark devB 后本轮只读 devB。
        watch.mark("devB");
        match read_delta(&lux, &watch, &None, &mut cur).await {
            ReadOutcome::Partial(scope) => {
                assert_eq!(scope.len(), 1, "devA 已追平，只读新脏 devB: {scope:?}");
                assert_eq!(scope[0].0, "devB");
            }
            other => panic!("无 epoch 变更应为 Partial: {other:?}"),
        }
    }

    /// epoch 变更（Lagged 重同步，P7）→ 全量重读 Resync：含全部设备 + 新 epoch；
    /// 游标已重建——同 epoch 再查无脏。
    #[tokio::test]
    async fn read_delta_epoch_bump_resync() {
        let (_handle, lux) = start_lux("resync").await;
        let watch = RtdataWatchService::new();
        for dev in ["devA", "devB", "devC"] {
            seed_rtdata(&lux, dev, &[("t1", "1")]).await;
        }

        // 游标先建(epoch=0),再 bump——持有旧 epoch 的游标转 Full(真实序)。
        let mut cur = watch.cursor();
        watch.bump_epoch();
        let new_epoch = match read_delta(&lux, &watch, &None, &mut cur).await {
            ReadOutcome::Resync(epoch, scope) => {
                assert!(epoch >= 1, "新 epoch 随 Resync 返回: {epoch}");
                let devs: Vec<&str> = scope.iter().map(|(d, _)| d.as_str()).collect();
                assert_eq!(devs.len(), 3, "全量重读含全部设备: {devs:?}");
                for dev in ["devA", "devB", "devC"] {
                    assert!(devs.contains(&dev), "Resync 应含 {dev}: {devs:?}");
                }
                epoch
            }
            other => panic!("epoch 变更应为 Resync: {other:?}"),
        };

        // 游标已重建为当前快照：同 epoch 再查 → Partial 空集。
        assert_eq!(cur.epoch(), new_epoch, "游标含新 epoch");
        match read_delta(&lux, &watch, &None, &mut cur).await {
            ReadOutcome::Partial(scope) => {
                assert!(scope.is_empty(), "游标已重建应无脏: {scope:?}")
            }
            other => panic!("epoch 一致应为 Partial: {other:?}"),
        }
    }

    /// M5 守门①：ksub 独有的 project_baseline——3 设备基线、脏域仅 devA 且值
    /// 未变时，投影后 diff 不得把非脏域（devB/devC）整域误报 removed。删除
    /// 投影直接喂全量基线的形态下 removed 会含 devB.t1/devC.t1，本测即红
    /// ——防该防误报守门被未来重构无声移除（对 scada 前端是灾难性闪烁）。
    #[test]
    fn project_baseline_limits_diff_to_dirty_domain() {
        let last = baseline(&[("devA.t1", "1"), ("devB.t1", "2"), ("devC.t1", "3")]);
        let dirty = || {
            let points: serde_json::Map<String, serde_json::Value> =
                [("devA.t1".to_string(), json!([0, "lt", "1"]))]
                    .into_iter()
                    .collect();
            vec![("devA".to_string(), points)]
        };
        // ksub Partial 路径逐字复刻（主循环 ReadOutcome::Partial 分支）。
        let proj = project_baseline(&last, &dirty());
        let (changes, removed) = diff_scope(collect_current(dirty(), None), &proj);
        assert!(
            changes.is_empty() && removed.is_empty(),
            "脏域值未变且非脏域不得误报: changes={changes:?} removed={removed:?}"
        );
        // 对照（论证投影必要性 = 守门意义）：直接喂全量基线（删投影形态），
        // devB/devC 整域必误报 removed。
        let (_, removed_no_proj) = diff_scope(collect_current(dirty(), None), &last);
        assert_eq!(
            removed_no_proj.len(),
            2,
            "无投影时非脏域必误报（devB.t1/devC.t1）: {removed_no_proj:?}"
        );
        assert!(
            removed_no_proj.contains(&"devB.t1".to_string())
                && removed_no_proj.contains(&"devC.t1".to_string())
        );
    }

    /// M5 守门②：dev_segment 归域语义——field `<devid>.<tag>` 的 devid 段
    /// 取 rsplit_once（倒数第一段是 tag，与 tag_of 互补）；无点 field 原样
    /// 返回（tag_key contains 分支无前缀、反解析不可得——保守归自身，最多
    /// 漏判归属不误报，R12 边界）。
    #[test]
    fn dev_segment_splits_last_dot_as_tag() {
        assert_eq!(
            dev_segment("a.b.c"),
            "a.b",
            "倒数第一段是 tag，前缀是 devid"
        );
        assert_eq!(dev_segment("line1.temp"), "line1");
        assert_eq!(dev_segment("nodot"), "nodot", "无点原样返回（保守归自身）");
    }
}
