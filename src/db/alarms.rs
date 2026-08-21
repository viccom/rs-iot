//! 活跃告警清单（Q12 告警最小闭环，spec `2026-08-21-alarm-minimal-loop-design.md` §1.2）：
//! lux Hash `alarms`。
//!
//! - field = key（脚本自定义告警键，如 "dev1.temp_high"）；
//! - value = JSON `{level, source, message, ts, count}`（key 即 field，不重复存）；
//! - fire 同 key：覆盖写 + `count += 1`（重复报警次数——风暴可见性）。RMW 的
//!   HGET→HSET 非事务原子（EmbeddedClient Clone = 独立 session，跨实例并发同 key
//!   fire 可能丢 count 更新——count 仅风暴可见性指标，非正确性数据，接受；复审
//!   P3-1 修正原「无并发竞争」的不实注释）；
//! - clear：HDEL（不存在也 `Ok`，幂等）；
//! - 重启后活跃清单天然保留（lux 持久）——告警不因网关重启而消失，运维语义正确。
//!
//! 历史时序表明确不做（spec §0「最小」语义），本表只存**当前活跃**。

use crate::error::RsIotError;
use lux::EmbeddedClient;

/// lux hash 名（全局单表）。
const HASH: &str = "alarms";

/// 全局活跃告警键上限（复审 P1-1）：频控限速率不限基数——失控脚本若不设上限，
/// 20/s × 86400s ≈ 172 万键/天（~250MB/天持久写入 + 面板全量 HGETALL/渲染/排序
/// 同步占住 lux session）。达上限后新键拒（返回 [`FireOutcome::Capped`]），
/// 既有键更新不受限（运维清除后自动恢复容量）。
pub const ALARM_MAX_KEYS: usize = 1000;

/// fire 结果（api 层据此返 true/false 并计数）。
#[derive(Debug, PartialEq, Eq)]
pub enum FireOutcome {
    /// 已存储，携带更新后的 count。
    Stored(u64),
    /// 新键被基数上限拒绝（`alarm_hash_capped` 计数）。
    Capped,
}

/// 单条活跃告警（REST list / 前端面板形状；key 来自 Hash field，spec §1.2/§4）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AlarmItem {
    /// 告警键。
    pub key: String,
    /// 级别（"info"/"warn"/"error"）。
    pub level: String,
    /// 来源实例 ID（宿主填，脚本不可伪造）。
    pub source: String,
    /// 人类可读描述。
    pub message: String,
    /// 最后一次 fire 的 ISO8601 时间戳。
    pub ts: String,
    /// 累计触发次数（同 key 重复 fire 递增）。
    pub count: u64,
}

/// 存储记录（value JSON；key 即 field 不重复存，spec §1.2）。
#[derive(serde::Serialize, serde::Deserialize)]
struct AlarmRecord {
    level: String,
    source: String,
    message: String,
    ts: String,
    count: u64,
}

/// 触发/更新一条告警：HGET 现值 → `count+1`（无则 1）→ HSET 覆盖
/// （level/source/message/ts 均取本次值）。新键且基数达 [`ALARM_MAX_KEYS`] →
/// 拒存返回 [`FireOutcome::Capped`]（复审 P1-1；HLEN 与 HSET 间无事务，近似
/// 上限对护栏语义足够）；既有键更新恒放行。
pub async fn fire(
    c: &EmbeddedClient,
    key: &str,
    level: &str,
    source: &str,
    message: &str,
    ts: &str,
) -> Result<FireOutcome, RsIotError> {
    let count = match get(c, key).await? {
        Some(item) => item.count + 1,
        None => {
            // 新键：基数护栏（复审 P1-1）。HLEN 回 RESP 整数 ":N"，去前缀解析；
            // 解析失败按 0（fail-open——护栏失效好过告警全拒，量级看 alarm_hash_capped）。
            let len: u64 = String::from_utf8_lossy(&c.execute("HLEN", &[HASH]).await?)
                .trim()
                .trim_start_matches(':')
                .parse()
                .unwrap_or(0);
            if len as usize >= ALARM_MAX_KEYS {
                return Ok(FireOutcome::Capped);
            }
            1
        }
    };
    let rec = AlarmRecord {
        level: level.to_string(),
        source: source.to_string(),
        message: message.to_string(),
        ts: ts.to_string(),
        count,
    };
    c.execute("HSET", &[HASH, key, &serde_json::to_string(&rec)?])
        .await?;
    Ok(FireOutcome::Stored(count))
}

/// 读一条告警（REST clear 取清除前条目用：level/message 沿用条目值；无则 `None`）。
///
/// value 损坏（非法 JSON）同样返回 `None`——与 [`list`] 跳过坏记录一致；clear 对其
/// 走幂等路径仍 HDEL（运维兜底恰要能清掉坏记录，只是无条目信息可发 Clear 事件）。
pub async fn get(c: &EmbeddedClient, key: &str) -> Result<Option<AlarmItem>, RsIotError> {
    let v = c.execute_value("HGET", &[HASH, key]).await?;
    let Some(raw) = embedded_to_string(v) else {
        return Ok(None);
    };
    let rec: AlarmRecord = match serde_json::from_str(&raw) {
        Ok(r) => r,
        Err(_) => return Ok(None),
    };
    Ok(Some(AlarmItem {
        key: key.to_string(),
        level: rec.level,
        source: rec.source,
        message: rec.message,
        ts: rec.ts,
        count: rec.count,
    }))
}

/// 清除一条告警（幂等：不存在仍 `Ok`——HDEL 对缺失 field 天然无错）。
pub async fn clear(c: &EmbeddedClient, key: &str) -> Result<(), RsIotError> {
    c.execute("HDEL", &[HASH, key]).await?;
    Ok(())
}

/// 列全部活跃告警，按 ts 倒序（最新在前；spec §4）。
///
/// 排序按 ts 字符串比较：宿主统一 `to_rfc3339()` 生成（同格式 UTC），字典序 == 时间序。
/// value 损坏（非法 JSON/缺字段）跳过不阻断清单——告警展示不该被一条坏记录拖垮。
pub async fn list(c: &EmbeddedClient) -> Result<Vec<AlarmItem>, RsIotError> {
    let v = c.execute_value("HGETALL", &[HASH]).await?;
    let mut out = Vec::new();
    if let lux::EmbeddedValue::Array(arr) = v {
        // RESP 扁平序列：k1, v1, k2, v2, ...（同 secrets::list 解析形态）。
        let mut i = 0;
        while i + 1 < arr.len() {
            let k = embedded_to_string(arr[i].clone()).unwrap_or_default();
            let raw = embedded_to_string(arr[i + 1].clone()).unwrap_or_default();
            if !k.is_empty() {
                if let Ok(rec) = serde_json::from_str::<AlarmRecord>(&raw) {
                    out.push(AlarmItem {
                        key: k,
                        level: rec.level,
                        source: rec.source,
                        message: rec.message,
                        ts: rec.ts,
                        count: rec.count,
                    });
                }
            }
            i += 2;
        }
    }
    out.sort_by(|a, b| b.ts.cmp(&a.ts));
    Ok(out)
}

/// HGET/HGETALL 值 → 字符串（Nil/空 → `None`，语义同 `secrets::embedded_to_string`）。
fn embedded_to_string(v: lux::EmbeddedValue) -> Option<String> {
    match v {
        lux::EmbeddedValue::Bulk(b) => {
            let s = String::from_utf8_lossy(&b).into_owned();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        }
        lux::EmbeddedValue::Simple(s) if !s.is_empty() => Some(s),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 唯一临时 data_dir（tag 区分用例，防并行测试互扰；照 secrets::tests 模式）。
    async fn test_client(tag: &str) -> lux::EmbeddedClient {
        let cfg = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: std::env::temp_dir()
                    .join(format!("alarms-{tag}-{}", std::process::id()))
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

    /// 新 key 首次 fire：count=1，字段原样落库。
    #[tokio::test]
    async fn fire_new_key_starts_count_1() {
        let c = test_client("fire-new").await;
        let n = fire(
            &c,
            "dev1.temp_high",
            "warn",
            "inst-a",
            "温度过高",
            "2026-08-21T01:00:00+00:00",
        )
        .await
        .unwrap();
        assert_eq!(n, FireOutcome::Stored(1), "首次 fire 应 Stored(1)");

        let item = get(&c, "dev1.temp_high").await.unwrap().expect("应存在");
        assert_eq!(item.count, 1);
        assert_eq!(item.level, "warn");
        assert_eq!(item.source, "inst-a");
        assert_eq!(item.message, "温度过高");
        assert_eq!(item.ts, "2026-08-21T01:00:00+00:00");
    }

    /// 同 key 重复 fire：count 累计 + level/message/ts 覆盖为最新（spec §1.2 覆盖写）。
    #[tokio::test]
    async fn fire_same_key_increments_and_overwrites() {
        let c = test_client("fire-inc").await;
        fire(&c, "k", "info", "s1", "m1", "2026-08-21T01:00:00+00:00")
            .await
            .unwrap();
        let n = fire(&c, "k", "error", "s1", "m2", "2026-08-21T02:00:00+00:00")
            .await
            .unwrap();
        assert_eq!(n, FireOutcome::Stored(2), "二次 fire 应 Stored(2)");

        let item = get(&c, "k").await.unwrap().unwrap();
        assert_eq!(item.count, 2);
        assert_eq!(item.level, "error", "level 取最新");
        assert_eq!(item.message, "m2", "message 取最新");
        assert_eq!(item.ts, "2026-08-21T02:00:00+00:00", "ts 取最新");
    }

    /// clear 幂等：不存在 → Ok；清除后清单空；再清仍 Ok（spec §1.2）。
    /// 复审 P1-1：全局基数上限——新键达 [`ALARM_MAX_KEYS`] 拒存（Capped），
    /// 既有键更新不受限；HDEL 释放容量后恢复。
    #[tokio::test]
    async fn fire_new_key_capped_at_limit() {
        let c = test_client("alarms-cap").await;
        for i in 0..ALARM_MAX_KEYS {
            let r = fire(&c, &format!("k{i}"), "warn", "s", "m", "t")
                .await
                .unwrap();
            assert_eq!(r, FireOutcome::Stored(1), "达上限前新键应 Stored：k{i}");
        }
        // 第 1001 个新键：Capped。
        let r = fire(&c, "k_new", "warn", "s", "m", "t").await.unwrap();
        assert_eq!(r, FireOutcome::Capped, "达上限后新键应 Capped");
        // 既有键更新不受限（count 递增）。
        let r = fire(&c, "k0", "warn", "s", "m2", "t2").await.unwrap();
        assert_eq!(r, FireOutcome::Stored(2), "既有键更新恒放行");
        // 清除一个键释放容量后新键恢复。
        clear(&c, "k0").await.unwrap();
        let r = fire(&c, "k_new", "warn", "s", "m", "t").await.unwrap();
        assert_eq!(r, FireOutcome::Stored(1), "容量释放后新键恢复 Stored");
    }

    #[tokio::test]
    async fn clear_is_idempotent() {
        let c = test_client("clear").await;
        clear(&c, "missing").await.unwrap();

        fire(&c, "k", "warn", "s", "m", "2026-08-21T01:00:00+00:00")
            .await
            .unwrap();
        clear(&c, "k").await.unwrap();
        assert!(list(&c).await.unwrap().is_empty(), "清除后清单应空");
        clear(&c, "k").await.unwrap();
    }

    /// list 按 ts 倒序（最新在前，spec §4）；不同 key 各自独立。
    #[tokio::test]
    async fn list_orders_by_ts_desc() {
        let c = test_client("list-desc").await;
        fire(&c, "old", "info", "s", "m", "2026-08-21T01:00:00+00:00")
            .await
            .unwrap();
        fire(&c, "mid", "warn", "s", "m", "2026-08-21T02:00:00+00:00")
            .await
            .unwrap();
        fire(&c, "new", "error", "s", "m", "2026-08-21T03:00:00+00:00")
            .await
            .unwrap();

        let items = list(&c).await.unwrap();
        let keys: Vec<&str> = items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, vec!["new", "mid", "old"], "ts 倒序");
    }

    /// lux 持久语义（spec §1.2）：shutdown + 同 data_dir 重启 → 活跃清单与 count 保留。
    #[tokio::test]
    async fn list_persists_across_lux_restart() {
        let data_dir = std::env::temp_dir().join(format!("alarms-restart-{}", std::process::id()));
        let cfg = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: data_dir.to_str().unwrap().to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let handle = lux::run_with_config(crate::db::build_lux_config(&cfg))
            .await
            .expect("lux");
        let c = handle.client();
        fire(
            &c,
            "dev1.temp_high",
            "warn",
            "inst-a",
            "m",
            "2026-08-21T01:00:00+00:00",
        )
        .await
        .unwrap();
        fire(
            &c,
            "dev1.temp_high",
            "warn",
            "inst-a",
            "m",
            "2026-08-21T01:01:00+00:00",
        )
        .await
        .unwrap();
        crate::db::shutdown_lux(handle).await.unwrap();

        // 重启（同 data_dir）：清单保留、count 不丢——告警不因网关重启而消失。
        let handle = lux::run_with_config(crate::db::build_lux_config(&cfg))
            .await
            .expect("lux restart");
        let c = handle.client();
        let items = list(&c).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].key, "dev1.temp_high");
        assert_eq!(items[0].count, 2, "count 跨重启保留");
    }
}
