//! 发现引擎（M-4 T1；决策 D-M3——出口登记转型发现队列）。
//!
//! 数据流中未建模标签进 lux Hash `discoveries`（field = `tag_key`），**不自动
//! 入正式册**——发现是建模工作流的入口（人工确认后 adopt 一键建模），不是
//! 兼容手段。Phase 1a `note_once` 的"首见占位写 tags 表"语义随 tags 表退役
//! （M-4 T3），本模块是其换壳演进：
//!
//! - **O(1) 比对**：effective 物化缓存（[`crate::model::cache::effective()`]）
//!   查 `tag_key`——已建模 → once-set 记忆（稳态纯内存零 lux 交互，热路径
//!   scratch 复用零分配，50万点/秒门槛不变）；未建模 → 内存计数缓冲；
//! - **per-entry 60s 节流**：每条目独立窗口（首见立即落盘尽快可见，其后
//!   窗口内纯内存聚合 count/last_ts/sample_value，到期一批 HSET——同 key
//!   不新增条目只累计，风险登记 §七 count 聚合）；
//! - **上限 5000 + LRU 淘汰**：超限逐出 `last_ts` 最旧条目（内存 + lux 同步
//!   HDEL，风险登记 §七防脏数据灌爆）；
//! - **重启接续**：启动 [`load`] 装载 lux 表恢复内存计数（count 不清零）。
//!
//! 与 store.rs 的分工：devmodels/devreg CRUD 在 store；discoveries 的 CRUD
//! （clear/sweep）与引擎内存态强耦合（须同步逐出），故同置本模块防状态漂移。

use crate::error::RsIotError;
use crate::types::{Sample, TagProps};
use lux::EmbeddedClient;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// lux hash 名（发现队列单表）。
const HASH: &str = "discoveries";

/// 节流窗口（生产默认；[`note_once`] 包装）。
pub const FLUSH_WINDOW: Duration = Duration::from_secs(60);

/// 队列上限（LRU 淘汰最旧；风险登记 §七）。
pub const MAX_ENTRIES: usize = 5000;

/// 发现队列条目（计划 §二；value = 本结构 JSON）。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Discovery {
    /// 设备 id。
    pub devid: String,
    /// 标签名。
    pub tag: String,
    /// 首次发现时间戳（秒）。
    pub first_ts: i64,
    /// 最近发现时间戳（秒）。
    pub last_ts: i64,
    /// 聚合计数（同 key 不新增条目，只累计）。
    pub count: u64,
    /// 最近样本值（JSON 编码字符串，建模参考）。
    pub sample_value: String,
}

/// 内存缓冲条目。
#[derive(Debug, Clone)]
struct Pending {
    disc: Discovery,
    /// 最近样本值（原始枚举；due 落盘时才序列化——稳态零序列化开销）。
    last_val: crate::types::SampleValue,
    /// 上次落盘时刻；None = 未落过盘（首见立即落）。
    last_flush: Option<Instant>,
}

/// 引擎进程态（once-set 记忆 + 未建模计数缓冲）。
struct Engine {
    /// 已确认建模的 key（effective 命中过一次——稳态跳过全部处理）。
    seen_modeled: HashSet<String>,
    /// 未建模条目（key = tag_key）。
    pending: HashMap<String, Pending>,
}

static ENGINE: OnceLock<Mutex<Engine>> = OnceLock::new();

fn engine() -> &'static Mutex<Engine> {
    ENGINE.get_or_init(|| {
        Mutex::new(Engine {
            seen_modeled: HashSet::new(),
            pending: HashMap::new(),
        })
    })
}

/// 出口发现（pipeline flush 咽喉；生产包装：60s 窗口 / 5000 上限）。
pub async fn note_once(
    c: &EmbeddedClient,
    samples: &[Sample],
    effective: &HashMap<String, TagProps>,
) -> Result<(), RsIotError> {
    note_with_window(c, samples, effective, FLUSH_WINDOW, MAX_ENTRIES).await
}

/// 带参核心（e2e/单测可调小窗口与上限）。语义见模块注释。
///
/// 热路径（50万点/秒门槛）：scratch 复用组装 key + `contains(&str)` 借用查找
/// （同 Phase 1a note_once 的零分配模式）；已建模 key 首见 clone 一次进
/// seen_modeled 后纯内存跳过，稳态零 lux 交互。落盘只写**本批涉及**的 key
/// （per-entry 窗口判定），不触碰无关条目——多实例 lux 场景（测试）互不搬运。
pub async fn note_with_window(
    c: &EmbeddedClient,
    samples: &[Sample],
    effective: &HashMap<String, TagProps>,
    window: Duration,
    max_entries: usize,
) -> Result<(), RsIotError> {
    // 本批涉及且窗口到期的落盘快照；LRU 逐出目标（锁外执行 lux IO）。
    let mut to_flush: Vec<(String, String)> = Vec::new();
    let mut to_evict: Vec<String> = Vec::new();
    {
        let mut g = engine().lock().unwrap();
        let mut key = String::new();
        let mut now = None::<Instant>; // 惰性取一次时钟（本批有未建模才取）
        for s in samples {
            for (tag, val) in &s.values {
                key.clear();
                if tag.contains(s.devid.as_str()) {
                    key.push_str(tag);
                } else {
                    key.push_str(&s.devid);
                    key.push('.');
                    key.push_str(tag);
                }
                // 已建模判定。热路径顺序：seen_modeled 短路（稳态每点仅此一次
                // 借用查找，与 Phase 1a 同构）；effective 命中首见才记入 once-set。
                if g.seen_modeled.contains(key.as_str()) {
                    continue;
                }
                if effective.contains_key(key.as_str()) {
                    g.seen_modeled.insert(key.clone());
                    continue;
                }
                let ts = s.timestamp;
                let n = *now.get_or_insert_with(Instant::now);
                if let Some(p) = g.pending.get_mut(key.as_str()) {
                    // 同 key 聚合：count 累计 + 最近值/时间（窗口内纯内存，
                    // 零序列化——sample_value 在 due 落盘时才从 last_val 生成）。
                    p.disc.count += 1;
                    p.disc.last_ts = ts;
                    p.last_val = val.clone();
                    let due = p.last_flush.is_none_or(|t| n.duration_since(t) >= window);
                    if due {
                        p.last_flush = Some(n);
                        p.disc.sample_value =
                            serde_json::to_string(&p.last_val).unwrap_or_default();
                        to_flush.push((
                            key.clone(),
                            serde_json::to_string(&p.disc).unwrap_or_default(),
                        ));
                    }
                } else {
                    let disc = Discovery {
                        devid: s.devid.clone(),
                        tag: tag.clone(),
                        first_ts: ts,
                        last_ts: ts,
                        count: 1,
                        sample_value: serde_json::to_string(val).unwrap_or_default(),
                    };
                    g.pending.insert(
                        key.clone(),
                        Pending {
                            disc,
                            last_val: val.clone(),
                            last_flush: None,
                        },
                    );
                    // LRU 上限：超限批量逐出 last_ts 最旧**至 90% 水位线**（摊还：
                    // 每轮 O(n) 扫描换 n/10 次插入免扫——持续灌爆下均摊 O(1)）。
                    let mut evicted_self = false;
                    if g.pending.len() > max_entries {
                        let watermark = max_entries - max_entries / 10;
                        while g.pending.len() > watermark {
                            let Some(oldest) = g
                                .pending
                                .iter()
                                .min_by_key(|(_, p)| p.disc.last_ts)
                                .map(|(k, _)| k.clone())
                            else {
                                break;
                            };
                            if oldest == key {
                                evicted_self = true;
                            }
                            g.pending.remove(&oldest);
                            to_evict.push(oldest);
                        }
                    }
                    // 首见立即落盘（尽快可见）；被 LRU 逐出（含刚插入即最旧）则不写。
                    if !evicted_self {
                        if let Some(p) = g.pending.get_mut(key.as_str()) {
                            p.last_flush = Some(n);
                            to_flush.push((
                                key.clone(),
                                serde_json::to_string(&p.disc).unwrap_or_default(),
                            ));
                        }
                    }
                }
            }
        }
    }
    for k in &to_evict {
        if let Err(e) = c.execute("HDEL", &[HASH, k]).await {
            tracing::warn!("[discovery] LRU 逐出 HDEL {k} 失败（遗留脏条目，clear 兜底）: {e}");
        }
    }
    for (k, raw) in &to_flush {
        if let Err(e) = c.execute("HSET", &[HASH, k, raw]).await {
            // 落盘失败回滚本批落盘标记（审查修复）：last_flush 已在锁内置位，
            // 不回滚则 60s 窗口内不重试——回滚后下一批立即重新到期落盘。
            tracing::warn!("[discovery] 落盘失败（回滚标记，下批重试）: {e}");
            let keys: Vec<String> = to_flush.iter().map(|(k, _)| k.clone()).collect();
            unmark_flushed(&keys);
            return Err(e.into());
        }
    }
    Ok(())
}

/// 落盘失败回滚：把 keys 的 `last_flush` 重置为 None（下一批重新视为到期，
/// 立即重试落盘）。幂等——不在缓冲的键跳过；并发批次已重置/推进则维持其值。
fn unmark_flushed(keys: &[String]) {
    let mut g = engine().lock().unwrap();
    for k in keys {
        if let Some(p) = g.pending.get_mut(k) {
            p.last_flush = None;
        }
    }
}

/// 闸拒登记（决策 D-M4"不过闸进 discoveries"）：lua `set_value/set_values/collect`
/// 与 sidecar DataUp 被校验闸拒绝的未建模 (devid, tag, 值) 进发现队列——**数据
/// 本身不放行**（严格闸语义不变），元数据可见供人工 adopt 建模。fail-soft。
///
/// 内部走 [`note_once`]（已建模 key 跳过 + 60s 节流 + LRU 上限同口径）；
/// 空集零开销直接返回。
pub async fn note_rejected(
    c: &EmbeddedClient,
    hwid: &crate::types::Hwid,
    devid: &str,
    rejected: std::collections::BTreeMap<String, crate::types::SampleValue>,
) {
    if rejected.is_empty() {
        return;
    }
    let s = Sample {
        hwid: hwid.clone(),
        devid: devid.to_string(),
        values: rejected,
        timestamp: chrono::Utc::now().timestamp(),
    };
    if let Err(e) = note_once(c, std::slice::from_ref(&s), &crate::model::cache::effective()).await
    {
        tracing::warn!("[discovery] 闸拒登记失败(忽略): {e}");
    }
}

/// 队列全表（HGETALL；坏值跳过）。
pub async fn list(c: &EmbeddedClient) -> Result<HashMap<String, Discovery>, RsIotError> {
    let v = c.execute_value("HGETALL", &[HASH]).await?;
    let mut out = HashMap::new();
    if let lux::EmbeddedValue::Array(arr) = v {
        let mut i = 0;
        while i + 1 < arr.len() {
            let key = embedded_to_string(arr[i].clone()).unwrap_or_default();
            let raw = embedded_to_string(arr[i + 1].clone()).unwrap_or_default();
            if !key.is_empty() {
                if let Ok(d) = serde_json::from_str::<Discovery>(&raw) {
                    out.insert(key, d);
                }
            }
            i += 2;
        }
    }
    Ok(out)
}

/// 清除队列（`dev_id` = None 全清；Some 按设备前缀清）。返回清除条数。
/// 内存缓冲同步逐出（clear 后同 key 再出数重新计数——发现语义重开）。
/// 逐出范围以**本 lux 现存条目**为准（不动其他连接实例的缓冲——测试多 lux
/// 并行互不干扰；生产单进程单 lux 语义等价于全清）。
pub async fn clear(c: &EmbeddedClient, dev_id: Option<&str>) -> Result<usize, RsIotError> {
    let keys: Vec<String> = match dev_id {
        None => list(c).await?.into_keys().collect(),
        Some(d) => {
            let prefix = format!("{d}.");
            // 谓词与 routes_discovery::list 同口径：tag_key 短路形态（tag 名含
            // devid 子串 → key 无 `<devid>.` 前缀）按 devid 字段兜底命中。
            list(c)
                .await?
                .into_iter()
                .filter(|(k, disc)| k.starts_with(&prefix) || disc.devid == d)
                .map(|(k, _)| k)
                .collect()
        }
    };
    if dev_id.is_none() {
        c.execute("DEL", &[HASH]).await?;
    } else {
        for k in &keys {
            c.execute("HDEL", &[HASH, k]).await?;
        }
    }
    let mut g = engine().lock().unwrap();
    for k in &keys {
        g.pending.remove(k);
    }
    Ok(keys.len())
}

/// 启动装载：lux 表 → 内存缓冲（count 接续；adopt/clear 之前调）。
pub async fn load(c: &EmbeddedClient) -> Result<usize, RsIotError> {
    let all = list(c).await?;
    let n = all.len();
    let mut g = engine().lock().unwrap();
    for (key, disc) in all {
        // 已在缓冲的条目不动（本批新计数优先——load 与 flush 的竞态下新数据准）。
        let last_val = serde_json::from_str(&disc.sample_value)
            .unwrap_or(crate::types::SampleValue::Float(0.0));
        g.pending.entry(key).or_insert(Pending {
            disc,
            last_val,
            last_flush: None,
        });
    }
    Ok(n)
}

/// effective 快照变更后修剪 seen_modeled（cache::reload 尾部调用）：不在新
/// 快照中的 key（模型删 tag / 设备删除）不再被短路——同 tag 数据重新走发现
/// 路径。仍在快照中的 key 保留（稳态短路零开销不变）。
pub(crate) fn retain_modeled(effective: &HashMap<String, TagProps>) {
    let mut g = engine().lock().unwrap();
    g.seen_modeled.retain(|k| effective.contains_key(k));
}

/// 对账清除：队列中已进 effective 缓存的条目删除（adopt 建模成功后调用；
/// 同步逐出内存缓冲）。返回清除条数。
pub async fn sweep_modeled(
    c: &EmbeddedClient,
    effective: &HashMap<String, TagProps>,
) -> Result<usize, RsIotError> {
    let keys: Vec<String> = list(c)
        .await?
        .into_keys()
        .filter(|k| effective.contains_key(k))
        .collect();
    for k in &keys {
        c.execute("HDEL", &[HASH, k]).await?;
    }
    let mut g = engine().lock().unwrap();
    for k in &keys {
        g.pending.remove(k);
    }
    Ok(keys.len())
}

/// 极简 glob：`*` 通配任意串（含空）。pattern 无 `*` = 精确匹配。
///
/// 分段算法（无回溯）：按 `*` 切段，首段必须前缀、末段必须后缀、中间段依序
/// `contains` 且不重叠。`*furnace*.temp*` 形态覆盖 dev 前缀 + tag 后缀场景。
/// Phase 1a `db::tag_registry` 同名实现迁移（tags 表退役，批量建模/清理复用）。
pub fn glob_match(pattern: &str, key: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == key;
    }
    let mut rest = key;
    if !rest.starts_with(parts[0]) {
        return false;
    }
    rest = &rest[parts[0].len()..];
    for part in &parts[1..parts.len() - 1] {
        match rest.find(part) {
            Some(pos) => rest = &rest[pos + part.len()..],
            None => return false,
        }
    }
    rest.ends_with(parts[parts.len() - 1])
}

/// lux EmbeddedValue → String（Bulk/Simple；同 store 口径）。
fn embedded_to_string(v: lux::EmbeddedValue) -> Option<String> {
    match v {
        lux::EmbeddedValue::Bulk(b) => Some(String::from_utf8_lossy(&b).to_string()),
        lux::EmbeddedValue::Simple(s) => Some(s),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Hwid, SampleValue};
    use std::collections::BTreeMap;

    /// 全局 ENGINE 进程级共享且并行测试多 lux 实例——本套件内**串行**执行
    /// （每测试持锁到结束；tokio Mutex——guard 可跨 await 点）+ 起点重置，
    /// 使 LRU/节流断言只依赖自己的 key（毒化防御：`unwrap_or_else(into_inner)`）。
    static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn serialized_and_reset() -> tokio::sync::MutexGuard<'static, ()> {
        let g = TEST_LOCK.lock().await;
        let mut e = engine().lock().unwrap_or_else(|e| e.into_inner());
        e.pending.clear();
        e.seen_modeled.clear();
        g
    }

    /// 唯一临时 data_dir（tag 区分用例防并行互扰；store::tests 模式）。
    async fn test_client(tag: &str) -> lux::EmbeddedClient {
        let cfg = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: std::env::temp_dir()
                    .join(format!("discovery-{tag}-{}", std::process::id()))
                    .to_str()
                    .unwrap()
                    .to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let handle = lux::run_with_config(crate::db::build_lux_config(&cfg))
            .await
            .expect("lux");
        handle.client()
    }

    fn sample(devid: &str, tag: &str, val: f64, ts: i64) -> Sample {
        let mut values = BTreeMap::new();
        values.insert(tag.to_string(), SampleValue::Float(val));
        Sample {
            hwid: Hwid::new("it-hwid"),
            devid: devid.to_string(),
            values,
            timestamp: ts,
        }
    }

    fn eff(keys: &[&str]) -> HashMap<String, TagProps> {
        keys.iter()
            .map(|k| (k.to_string(), TagProps::default()))
            .collect()
    }

    /// 全局 ENGINE 进程级共享——各用例用互不重叠 devid（Phase 1a SEEN 约定）。
    const D: &str = "discdev1";
    const D2: &str = "discdev2";

    #[test]
    fn glob_exact_match_without_star() {
        assert!(glob_match("dev1.temp", "dev1.temp"));
        assert!(!glob_match("dev1.temp", "dev1.temp2"));
    }

    #[test]
    fn glob_star_prefix_and_suffix() {
        assert!(glob_match("furnace*", "furnace_02.zone1.temp"));
        assert!(glob_match("*.temp", "furnace_02.temp"));
        assert!(!glob_match("*.temp", "furnace_02.pressure"));
    }

    #[test]
    fn glob_middle_segment_ordered_no_overlap() {
        assert!(glob_match("*zone*.temp*", "furnace_02.zone1.temp"));
        assert!(
            !glob_match("*temp*.zone*", "furnace_02.zone1.temp"),
            "中间段须依序"
        );
        assert!(!glob_match("a*a*a", "aa"));
        assert!(glob_match("a*a*a", "aaa"));
    }

    #[test]
    fn glob_single_star_matches_all() {
        assert!(glob_match("*", "anything.at.all"));
        assert!(glob_match("*", ""));
    }

    #[tokio::test]
    async fn modeled_tag_skipped_entirely() {
        let _serial = serialized_and_reset().await;
        let c = test_client("modeled").await;
        let e = eff(&["discdev1.temp"]);
        note_with_window(&c, &[sample(D, "temp", 1.0, 100)], &e, Duration::ZERO, 100)
            .await
            .unwrap();
        assert!(list(&c).await.unwrap().is_empty(), "已建模不进队列");
        // 断言数据锁内收集、锁外断言（panic 不毒化全局锁）。
        let (in_pending, in_seen) = {
            let g = engine().lock().unwrap();
            (
                g.pending.contains_key("discdev1.temp"),
                g.seen_modeled.contains("discdev1.temp"),
            )
        };
        assert!(!in_pending, "已建模只进 seen_modeled，不占缓冲");
        assert!(in_seen);
    }

    #[tokio::test]
    async fn unmodeled_tag_enters_queue_with_sample() {
        let _serial = serialized_and_reset().await;
        let c = test_client("unmodeled").await;
        note_with_window(
            &c,
            &[sample(D, "ghost", 42.5, 1700)],
            &eff(&[]),
            Duration::ZERO,
            100,
        )
        .await
        .unwrap();
        let m = list(&c).await.unwrap();
        let d = m.get("discdev1.ghost").expect("进队列");
        assert_eq!((d.devid.as_str(), d.tag.as_str()), (D, "ghost"));
        assert_eq!(d.count, 1);
        assert_eq!(d.first_ts, 1700);
        assert_eq!(d.last_ts, 1700);
        assert_eq!(d.sample_value, "42.5");
    }

    #[tokio::test]
    async fn count_accumulates_same_key_no_new_entry() {
        let _serial = serialized_and_reset().await;
        let c = test_client("count").await;
        let e = eff(&[]);
        note_with_window(&c, &[sample(D, "c1", 1.0, 100)], &e, Duration::ZERO, 100)
            .await
            .unwrap();
        note_with_window(&c, &[sample(D, "c1", 2.0, 200)], &e, Duration::ZERO, 100)
            .await
            .unwrap();
        let m = list(&c).await.unwrap();
        assert_eq!(m.len(), 1, "同 key 不新增条目");
        let d = &m["discdev1.c1"];
        assert_eq!(d.count, 2, "count 聚合");
        assert_eq!(d.last_ts, 200);
        assert_eq!(d.sample_value, "2.0", "样本取最近值（JSON 编码）");
    }

    #[tokio::test]
    async fn throttle_window_defers_flush() {
        let _serial = serialized_and_reset().await;
        let c = test_client("throttle").await;
        // 大窗口：首见立即落一次（尽快可见），其后窗口内纯内存。
        note_with_window(
            &c,
            &[sample(D, "slow", 1.0, 100)],
            &eff(&[]),
            Duration::from_secs(3600),
            100,
        )
        .await
        .unwrap();
        let m = list(&c).await.unwrap();
        assert_eq!(m["discdev1.slow"].count, 1, "首见立即落一次");
        // 窗口内再计数：不落盘（lux count 不变），内存聚合。
        note_with_window(
            &c,
            &[sample(D, "slow", 2.0, 200)],
            &eff(&[]),
            Duration::from_secs(3600),
            100,
        )
        .await
        .unwrap();
        assert_eq!(list(&c).await.unwrap()["discdev1.slow"].count, 1);
        // 窗口归零（模拟到期）：同 key 再来 → 聚合值落盘。
        note_with_window(
            &c,
            &[sample(D, "slow", 3.0, 300)],
            &eff(&[]),
            Duration::ZERO,
            100,
        )
        .await
        .unwrap();
        let m = list(&c).await.unwrap();
        let d = &m["discdev1.slow"];
        assert_eq!(d.count, 3, "到期落聚合值");
        assert_eq!(d.last_ts, 300);
    }

    #[tokio::test]
    async fn lru_cap_evicts_oldest_by_last_ts() {
        let _serial = serialized_and_reset().await;
        let c = test_client("lru").await;
        let e = eff(&[]);
        note_with_window(&c, &[sample(D2, "a", 1.0, 100)], &e, Duration::ZERO, 2)
            .await
            .unwrap();
        note_with_window(&c, &[sample(D2, "b", 1.0, 200)], &e, Duration::ZERO, 2)
            .await
            .unwrap();
        note_with_window(&c, &[sample(D2, "c", 1.0, 300)], &e, Duration::ZERO, 2)
            .await
            .unwrap();
        let m = list(&c).await.unwrap();
        assert!(!m.contains_key("discdev2.a"), "最旧被 LRU 逐出");
        assert!(m.contains_key("discdev2.b") && m.contains_key("discdev2.c"));
        let a_in_pending = {
            let g = engine().lock().unwrap();
            g.pending.contains_key("discdev2.a")
        };
        assert!(!a_in_pending, "内存缓冲同步逐出");
    }

    #[tokio::test]
    async fn load_resumes_counts_across_restart() {
        let _serial = serialized_and_reset().await;
        let c = test_client("load").await;
        // 模拟前次运行留下的队列条目。
        let seed = Discovery {
            devid: D.into(),
            tag: "resume".into(),
            first_ts: 50,
            last_ts: 60,
            count: 7,
            sample_value: "1".into(),
        };
        c.execute(
            "HSET",
            &[
                HASH,
                "discdev1.resume",
                &serde_json::to_string(&seed).unwrap(),
            ],
        )
        .await
        .unwrap();
        assert_eq!(load(&c).await.unwrap(), 1);
        note_with_window(
            &c,
            &[sample(D, "resume", 9.0, 80)],
            &eff(&[]),
            Duration::ZERO,
            100,
        )
        .await
        .unwrap();
        let d = &list(&c).await.unwrap()["discdev1.resume"];
        assert_eq!(d.count, 8, "count 接续（7+1）");
        assert_eq!(d.first_ts, 50, "首见时间保留");
        assert_eq!(d.last_ts, 80);
    }

    #[tokio::test]
    async fn clear_all_and_by_device() {
        let _serial = serialized_and_reset().await;
        let c = test_client("clear").await;
        let e = eff(&[]);
        note_with_window(&c, &[sample(D, "x", 1.0, 100)], &e, Duration::ZERO, 100)
            .await
            .unwrap();
        note_with_window(&c, &[sample(D2, "y", 1.0, 100)], &e, Duration::ZERO, 100)
            .await
            .unwrap();
        assert_eq!(clear(&c, Some(D)).await.unwrap(), 1, "按设备清 1 条");
        let m = list(&c).await.unwrap();
        assert!(m.contains_key("discdev2.y") && !m.contains_key("discdev1.x"));
        assert_eq!(clear(&c, None).await.unwrap(), 1, "全清余量");
        assert!(list(&c).await.unwrap().is_empty());
        let (x_in_pending, y_in_pending) = {
            let g = engine().lock().unwrap();
            (
                g.pending.contains_key("discdev1.x"),
                g.pending.contains_key("discdev2.y"),
            )
        };
        assert!(!x_in_pending && !y_in_pending, "内存缓冲同步清");
    }

    /// clear(dev_id) 与 list 同谓词（审查修复）：tag_key 短路形态（tag 名含
    /// devid 子串 → key=tag 原样、无 `<devid>.` 前缀）的条目也按 devid 字段
    /// 命中清除——此前前缀过滤漏清。
    #[tokio::test]
    async fn clear_by_device_matches_key_without_prefix() {
        let _serial = serialized_and_reset().await;
        let c = test_client("clear-key").await;
        // 种无前缀形态：tag "discdev1x" 含 devid "discdev1" 子串 → tag_key 短路。
        let d = Discovery {
            devid: D.into(),
            tag: format!("{D}x"),
            first_ts: 1,
            last_ts: 1,
            count: 1,
            sample_value: "1".into(),
        };
        let key = crate::types::tag_key(D, &d.tag);
        assert!(
            !key.starts_with(&format!("{D}.")),
            "测试前提：tag_key 短路形态（key={key}）"
        );
        c.execute(
            "HSET",
            &[HASH, &key, &serde_json::to_string(&d).unwrap()],
        )
        .await
        .unwrap();
        // 常规前缀形态对照（不应被误清）。
        let other = Discovery {
            devid: D2.into(),
            tag: "y".into(),
            first_ts: 1,
            last_ts: 1,
            count: 1,
            sample_value: "1".into(),
        };
        c.execute(
            "HSET",
            &[HASH, &format!("{D2}.y"), &serde_json::to_string(&other).unwrap()],
        )
        .await
        .unwrap();
        assert_eq!(
            clear(&c, Some(D)).await.unwrap(),
            1,
            "无前缀形态按 devid 字段命中清除"
        );
        let m = list(&c).await.unwrap();
        assert!(
            !m.contains_key(&key) && m.contains_key("discdev2.y"),
            "目标清、其余留：{m:?}"
        );
    }

    #[tokio::test]
    async fn sweep_modeled_removes_adopted_entries() {
        let _serial = serialized_and_reset().await;
        let c = test_client("sweep").await;
        note_with_window(
            &c,
            &[sample(D, "adopted", 1.0, 100)],
            &eff(&[]),
            Duration::ZERO,
            100,
        )
        .await
        .unwrap();
        // adopt 后 effective 含该 key → 对账清除。
        let e = eff(&["discdev1.adopted"]);
        assert_eq!(sweep_modeled(&c, &e).await.unwrap(), 1);
        assert!(list(&c).await.unwrap().is_empty());
        let still_pending = {
            let g = engine().lock().unwrap();
            g.pending.contains_key("discdev1.adopted")
        };
        assert!(!still_pending);
    }

    /// seen_modeled 修剪：模型删 tag / 设备删除后，effective 快照不再含的
    /// key 不再被短路——同 tag 数据重新进发现队列（此前需进程重启才自愈）。
    #[tokio::test]
    async fn retain_modeled_prunes_stale_seen_keys() {
        let _serial = serialized_and_reset().await;
        let c = test_client("retain").await;
        // 曾建模：首见进 seen_modeled（纯内存跳过，不进队列）。
        let e1 = eff(&["discdev1.was_modeled"]);
        note_with_window(
            &c,
            &[sample(D, "was_modeled", 1.0, 100)],
            &e1,
            Duration::from_secs(3600),
            100,
        )
        .await
        .unwrap();
        assert!(list(&c).await.unwrap().is_empty());
        // 模型编辑删除该 tag：新 effective 快照修剪 seen_modeled。
        let e2 = eff(&[]);
        retain_modeled(&e2);
        // 同 tag 再出数：不被短路，重新可发现（首见立即落盘）。
        note_with_window(
            &c,
            &[sample(D, "was_modeled", 2.0, 200)],
            &e2,
            Duration::from_secs(3600),
            100,
        )
        .await
        .unwrap();
        let m = list(&c).await.unwrap();
        assert!(
            m.contains_key("discdev1.was_modeled"),
            "修剪后重新可发现：{m:?}"
        );
        // 仍在 effective 的 key 不受修剪影响（继续短路零开销）。
        let e3 = eff(&["discdev1.still"]);
        note_with_window(&c, &[sample(D, "still", 1.0, 100)], &e3, Duration::ZERO, 100)
            .await
            .unwrap();
        retain_modeled(&e3);
        note_with_window(&c, &[sample(D, "still", 2.0, 200)], &e3, Duration::ZERO, 100)
            .await
            .unwrap();
        assert!(
            !list(&c).await.unwrap().contains_key("discdev1.still"),
            "有效 key 保留短路"
        );
    }

    /// 闸拒登记（决策 D-M4"不过闸进 discoveries"）：lua/sidecar 校验闸拒绝的
    /// 未建模 (devid, tag, 值) 进队列——数据本身不放行，元数据可见供 adopt。
    #[tokio::test]
    async fn note_rejected_records_unmodeled_with_sample() {
        let _serial = serialized_and_reset().await;
        let c = test_client("rejected").await;
        let mut vals = std::collections::BTreeMap::new();
        vals.insert("ghost".to_string(), SampleValue::Float(9.9));
        note_rejected(&c, &Hwid::new("it-hwid"), D, vals).await;
        let m = list(&c).await.unwrap();
        let d = m.get("discdev1.ghost").expect("闸拒进队列");
        assert_eq!((d.devid.as_str(), d.tag.as_str()), (D, "ghost"));
        assert_eq!(d.sample_value, "9.9", "样本值供建模参考");
        assert_eq!(d.count, 1);
        // 空集零开销跳过（不产生条目、不触 lux）。
        note_rejected(&c, &Hwid::new("it-hwid"), D, Default::default()).await;
        assert_eq!(list(&c).await.unwrap().len(), 1);
    }

    /// 落盘失败回滚（审查修复）：unmark_flushed 重置 last_flush 后，同节流窗口
    /// 内下一批重新视为到期落盘（HSET 失败不再吞掉整窗口的队列更新）。
    #[tokio::test]
    async fn unmark_flushed_retries_within_window() {
        let _serial = serialized_and_reset().await;
        let c = test_client("unmark").await;
        let w = Duration::from_secs(3600);
        // 首见立即落盘 count=1；窗口内第二笔只内存累计不落盘。
        note_with_window(&c, &[sample(D, "fl", 1.0, 100)], &eff(&[]), w, 100)
            .await
            .unwrap();
        note_with_window(&c, &[sample(D, "fl", 2.0, 200)], &eff(&[]), w, 100)
            .await
            .unwrap();
        assert_eq!(
            list(&c).await.unwrap()["discdev1.fl"].count,
            1,
            "窗口内不落盘"
        );
        // 模拟 HSET 失败后的标记回滚。
        unmark_flushed(&["discdev1.fl".to_string()]);
        // 同窗口下一批：重新到期，聚合值落盘。
        note_with_window(&c, &[sample(D, "fl", 3.0, 300)], &eff(&[]), w, 100)
            .await
            .unwrap();
        let d = &list(&c).await.unwrap()["discdev1.fl"];
        assert_eq!(d.count, 3, "回滚后窗口内重试落盘");
        assert_eq!(d.last_ts, 300);
    }
}
