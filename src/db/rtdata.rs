//! 实时数据缓存：把 Sample 最新值写入 lux Hash `rtdata:<devid>`。
//!
//! 对标 Lite_IOT `bDB.HandleSyncDataQueue → GlobalDBMap[devid][key]=[ts,localTime,value]`。
//! 区别：rs-iot 不再维护内存 map + sync 队列双套，直接落 lux Hash——
//! 持久化（Tiered WAL）、REST `/rtdatas` 直接 HGETALL、并发交给 lux。

use crate::error::RsIotError;
use crate::types::SampleValue;
use lux::EmbeddedClient;

/// 写入一条 Sample 的所有变量到 `rtdata:<devid>`。
/// field = `<devid>.<tag>`（若 tag 已含 devid 则不重复），value = JSON `[ts, localTime, value]`。
///
/// **test-only 存量入口**（`lua_client/api/tests.rs` 用；REST `/rtdatas` 是 HGETALL 读路径，
/// 不调本函数）。内部委托 `write_batch`（单 Sample 切片）。`hwid` 不参与 rtdata 写入
/// （rtdata key 仅按 devid），构造空 Hwid 仅为填 Sample 结构（`write_batch` 不读 hwid）。
pub async fn write(
    c: &EmbeddedClient,
    hwid: &str,
    devid: &str,
    values: &std::collections::BTreeMap<String, SampleValue>,
    timestamp: i64,
) -> Result<(), RsIotError> {
    let _ = hwid;
    let sample = crate::types::Sample {
        hwid: crate::types::Hwid::from(String::new()),
        devid: devid.to_string(),
        values: values.clone(),
        timestamp,
    };
    write_batch(c, std::slice::from_ref(&sample))
        .await
        .map(|_| ())
}

/// 批量写入多个 Sample 到 lux rtdata（收益①：同 devid 多点合成一条 HSET 多对）。
///
/// **bench 修正（阶段0）**：实测 B 形态（逐 devid `execute("HSET", 多对)`，~1.65M 点/秒）
/// 优于 C 形态（PreparedPipeline，~1.38M 点/秒）——lux 的 pipeline 路径未比 N×execute 更优。
/// 故弃 PreparedPipeline，用 B 形态：聚合后逐 devid 一次 execute HSET 多对。
///
/// 同 devid 跨 Sample → 后入覆盖（HSET 多对后值覆盖前值，靠 field 名相同）。
/// 空批直接返回 `Ok(WriteBatchOutcome::default())`。local_time 批内取一次。
///
/// **per-devid 容错（I1）**：单个 devid execute 失败不丢整批剩余——记入 `failed_devids`
/// + continue 后续 devid。prep 错（serde_json cell 编码）仍 `?` 传播。
///
/// **失败集合而非计数**：调用方的旁路消费者（discovery / history / SSE notify）必须
/// 只消费**真正入库**的样本，否则未写入 lux 的值会进发现队列、归档进 tsink、推给 SSE
/// （历史与实时分叉，违反"lux rtdata 是唯一数据交汇点"）。故返回失败 devid 集合，
/// 由 [`WriteBatchOutcome::successful`] 过滤原始样本。
pub async fn write_batch(
    c: &EmbeddedClient,
    samples: &[crate::types::Sample],
) -> Result<WriteBatchOutcome, RsIotError> {
    if samples.is_empty() {
        return Ok(WriteBatchOutcome::default());
    }
    use std::collections::HashMap;
    let local_time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    // devid -> Vec<(field, cell)>；HashMap O(1) 聚合(N1:各 devid 独立 execute 无序依赖,
    // 原 BTreeMap 保序非必需)。Vec 保序(同 devid 多 Sample 后入覆盖靠 field 名相同,HSET
    // 多对后值覆盖前值)。
    let mut by_devid: HashMap<&str, Vec<(String, String)>> = HashMap::new();
    for s in samples {
        let entry = by_devid.entry(s.devid.as_str()).or_default();
        for (tag, val) in &s.values {
            // F-3/F-4：坏质量标记（Null）与非有限浮点（NaN/±Inf）不覆盖实时值——
            // 保留上次有效值与它的时间戳（ts 不前进即"陈旧"，可据此判活），
            // 不把 "NaN" 写进实时面冒充新鲜数据。历史侧另有 nonfinite 防线。
            let val_str = match val {
                crate::types::SampleValue::Float(f) => {
                    if !f.is_finite() {
                        continue;
                    }
                    f.to_string()
                }
                crate::types::SampleValue::Int(i) => i.to_string(),
                crate::types::SampleValue::UInt(u) => u.to_string(),
                crate::types::SampleValue::Bool(b) => b.to_string(),
                crate::types::SampleValue::Str(st) => st.clone(),
                crate::types::SampleValue::Null => continue,
            };
            let field = crate::types::tag_key(&s.devid, tag);
            let cell =
                serde_json::to_string(&serde_json::json!([s.timestamp, local_time, val_str]))?;
            entry.push((field, cell));
        }
    }
    // B 形态（bench 证明最快）：聚合后逐 devid 一次 execute("HSET", 多对 argv)。
    // per-devid 容错(I1):单个 devid execute 失败不丢整批,记入失败集 + continue。
    let mut outcome = WriteBatchOutcome::default();
    for (devid, pairs) in &by_devid {
        // F-9：整批点都是坏质量时 pairs 为空——lux `HSET key`（0 field）会报
        // `ERR wrong number of arguments`，把"本拍没有有效值"伪装成存储失败。
        // 空批跳过（不建空 entry，不改上次有效值）。
        if pairs.is_empty() {
            continue;
        }
        let key = format!("rtdata:{devid}");
        let mut argv: Vec<String> = Vec::with_capacity(1 + pairs.len() * 2);
        argv.push(key);
        for (f, v) in pairs {
            argv.push(f.clone());
            argv.push(v.clone());
        }
        let argv_ref: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
        if c.execute("HSET", &argv_ref[..]).await.is_err() {
            outcome.failed_devids.insert((*devid).to_string());
        }
    }
    Ok(outcome)
}

/// [`write_batch`] 的结果：失败的 devid 集合（per-devid 容错）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WriteBatchOutcome {
    /// lux HSET 失败的 devid（这些设备的样本**未入库**）。
    pub failed_devids: std::collections::HashSet<String>,
}

impl WriteBatchOutcome {
    /// 失败 devid 数（既有 `n_failed` 口径，供 metrics/warn）。
    pub fn n_failed(&self) -> usize {
        self.failed_devids.len()
    }

    /// 是否全部成功。
    pub fn all_ok(&self) -> bool {
        self.failed_devids.is_empty()
    }

    /// 从原始样本中筛出**确认入库**的部分（所有权数组）：旁路消费者
    /// （discovery/history/SSE）只能消费它——未入库的值不得进发现队列/历史归档/推送
    /// （否则历史与实时分叉）。全部成功时返回完整克隆；调用方可在 `all_ok()` 时
    /// 走零拷贝借用路径完全跳过本函数。
    pub fn successful(&self, samples: &[crate::types::Sample]) -> Vec<crate::types::Sample> {
        if self.failed_devids.is_empty() {
            return samples.to_vec();
        }
        samples
            .iter()
            .filter(|s| !self.failed_devids.contains(s.devid.as_str()))
            .cloned()
            .collect()
    }
}

/// 删除某设备的全部实时数据（`DEL rtdata:<dev_id>` 整个 hash）。
///
/// 实例/设备删除时调，清孤儿数据——否则北向发布器（默认 luaNorth 实例）仍会周期 `KEYS rtdata:*`
/// 把已移除设备的点位发布到 broker。DEL 模式对标 `luakv::remove`。
pub async fn delete_device(c: &EmbeddedClient, dev_id: &str) -> Result<(), RsIotError> {
    c.execute("DEL", &[&format!("rtdata:{dev_id}")]).await?;
    Ok(())
}

#[cfg(test)]
mod batch_tests {
    use super::*;
    use crate::types::{Hwid, Sample, SampleValue};
    use std::collections::BTreeMap;

    /// 启动测试 lux（Tiered + save 60s + 端口 0 OS 分配 + 按 name+pid 唯一 data_dir）。
    /// 注：ServerHandle 非 Clone（lux/src/lib.rs:505），client(&self) 借用返回 owned client，
    /// 故返回 `(handle, handle.client())` —— 对标 tests/common::start_lux 与 examples/bench_rtdata。
    async fn start_lux(name: &str) -> (lux::ServerHandle, lux::EmbeddedClient) {
        let dir = std::env::temp_dir().join(format!("rtdata-test-{name}-{}", std::process::id()));
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

    fn sample(devid: &str, pts: &[(&str, f64)]) -> Sample {
        let mut values = BTreeMap::new();
        for (t, v) in pts {
            values.insert((*t).to_string(), SampleValue::Float(*v));
        }
        Sample {
            hwid: Hwid::from("h"),
            devid: devid.into(),
            values,
            timestamp: 1_700_000_000,
        }
    }

    /// 从 HGETALL 结果(Array)抽取所有 field 名(偶数下标)。
    fn extract_fields(arr: &lux::EmbeddedValue) -> Vec<String> {
        match arr {
            lux::EmbeddedValue::Array(a) => a
                .iter()
                .step_by(2)
                .filter_map(|v| match v {
                    lux::EmbeddedValue::Bulk(b) => Some(String::from_utf8_lossy(b).to_string()),
                    lux::EmbeddedValue::Simple(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => vec![],
        }
    }

    #[tokio::test]
    async fn write_batch_same_devid_multi_points_one_hset() {
        let (_h, c) = start_lux("same_devid").await;
        let s = sample("d1", &[("t1", 1.0), ("t2", 2.0), ("t3", 3.0)]);
        write_batch(&c, std::slice::from_ref(&s)).await.unwrap();
        // 验证三个 field 都写入
        let arr = c.execute_value("HGETALL", &["rtdata:d1"]).await.unwrap();
        let fields = extract_fields(&arr);
        assert!(fields.contains(&"d1.t1".to_string()));
        assert!(fields.contains(&"d1.t2".to_string()));
        assert!(fields.contains(&"d1.t3".to_string()));
    }

    #[tokio::test]
    async fn write_batch_cross_devid_same_pipeline() {
        let (_h, c) = start_lux("cross_devid").await;
        let s1 = sample("d1", &[("t1", 1.0)]);
        let s2 = sample("d2", &[("t1", 2.0)]);
        write_batch(&c, &[s1, s2]).await.unwrap();
        assert!(
            extract_fields(&c.execute_value("HGETALL", &["rtdata:d1"]).await.unwrap())
                .contains(&"d1.t1".to_string())
        );
        assert!(
            extract_fields(&c.execute_value("HGETALL", &["rtdata:d2"]).await.unwrap())
                .contains(&"d2.t1".to_string())
        );
    }

    #[tokio::test]
    async fn write_batch_same_devid_later_sample_overrides() {
        let (_h, c) = start_lux("override").await;
        let s1 = sample("d1", &[("t1", 1.0)]);
        let mut s2 = sample("d1", &[("t1", 99.0)]);
        s2.timestamp = 1_700_000_010; // 后到
        write_batch(&c, &[s1, s2]).await.unwrap();
        // 最终值应是 99（后入覆盖）；取 value（field d1.t1 对应 cell [ts,localTime,valStr]）。
        // cell = [ts, localTime, valStr]，解析后精确断言 valStr == "99"
        // （非子串匹配：避免 "199"/"990" 假阳性通过）。
        let arr = c.execute_value("HGETALL", &["rtdata:d1"]).await.unwrap();
        let cells: Vec<String> = match &arr {
            lux::EmbeddedValue::Array(a) => a
                .iter()
                .skip(1)
                .step_by(2)
                .filter_map(|v| match v {
                    lux::EmbeddedValue::Bulk(b) => Some(String::from_utf8_lossy(b).to_string()),
                    lux::EmbeddedValue::Simple(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => vec![],
        };
        let cell_json: serde_json::Value =
            serde_json::from_str(cells.first().expect("至少一个 cell")).expect("cell 是合法 JSON");
        assert_eq!(
            cell_json[2],
            serde_json::json!("99"),
            "后入应覆盖，最终值应是 99，实际 cells: {cells:?}"
        );
    }

    #[tokio::test]
    async fn write_batch_empty_noop() {
        let (_h, c) = start_lux("empty").await;
        let o = write_batch(&c, &[]).await.unwrap(); // 不 panic 不报错
        assert!(o.all_ok() && o.n_failed() == 0);
    }

    /// P1 回归锁：失败集合的过滤语义——旁路消费者只能拿到确认入库的样本。
    /// 构造方式：`successful()` 直接对合成 outcome 断言（lux 无法按 devid 定点注入故障，
    /// 故在此锁纯函数契约；pipeline 的三个旁路均已改为消费 `written`）。
    #[test]
    fn outcome_successful_filters_failed_devids() {
        let samples = vec![
            sample("d1", &[("t1", 1.0)]),
            sample("d2", &[("t1", 2.0)]),
            sample("d3", &[("t1", 3.0)]),
        ];
        let mut o = WriteBatchOutcome::default();
        o.failed_devids.insert("d2".to_string());
        assert!(!o.all_ok());
        assert_eq!(o.n_failed(), 1);
        let kept_owned = o.successful(&samples);
        let kept: Vec<&str> = kept_owned.iter().map(|s| s.devid.as_str()).collect();
        assert_eq!(
            kept,
            vec!["d1", "d3"],
            "失败 devid 的样本必须被过滤（不得进发现/历史/SSE）"
        );
        // 全成功：原样全收
        let ok = WriteBatchOutcome::default();
        assert!(ok.all_ok());
        assert_eq!(ok.successful(&samples).len(), 3);
    }

    /// 删除设备实时数据（B 修复）：写后 delete_device 应清掉整个 rtdata:<dev> hash，
    /// 其他设备的 rtdata 不受影响。
    #[tokio::test]
    async fn delete_device_removes_only_target_dev() {
        let (_h, c) = start_lux("deldev").await;
        write_batch(
            &c,
            &[sample("d1", &[("t1", 1.0)]), sample("d2", &[("t2", 2.0)])],
        )
        .await
        .unwrap();
        delete_device(&c, "d1").await.unwrap();
        // d1 已清空（HGETALL 返回空数组）。
        let arr = c.execute_value("HGETALL", &["rtdata:d1"]).await.unwrap();
        assert!(
            matches!(&arr, lux::EmbeddedValue::Array(a) if a.is_empty()),
            "d1 rtdata 应被删除，实际: {arr:?}"
        );
        // d2 保留。
        assert!(
            extract_fields(&c.execute_value("HGETALL", &["rtdata:d2"]).await.unwrap())
                .contains(&"d2.t2".to_string()),
            "d2 rtdata 不受影响"
        );
    }

    /// HGETALL → (field, cell JSON) 映射（测试断言用）。
    async fn cells(
        c: &lux::EmbeddedClient,
        devid: &str,
    ) -> std::collections::BTreeMap<String, serde_json::Value> {
        let arr = c
            .execute_value("HGETALL", &[&format!("rtdata:{devid}")])
            .await
            .unwrap();
        let mut out = std::collections::BTreeMap::new();
        if let lux::EmbeddedValue::Array(a) = &arr {
            for pair in a.chunks(2) {
                let to_s = |v: &lux::EmbeddedValue| match v {
                    lux::EmbeddedValue::Bulk(b) => Some(String::from_utf8_lossy(b).to_string()),
                    lux::EmbeddedValue::Simple(s) => Some(s.clone()),
                    _ => None,
                };
                if let (Some(f), Some(v)) =
                    (pair.first().and_then(to_s), pair.get(1).and_then(to_s))
                {
                    out.insert(
                        f,
                        serde_json::from_str(&v).unwrap_or(serde_json::Value::Null),
                    );
                }
            }
        }
        out
    }

    /// F-9 回归锁：空 values 的 Sample（全坏质量）不得触发 `HSET key`（0 field）。
    ///
    /// 旧实现 `by_devid` 里留下空 Vec → `execute("HSET", ["rtdata:d1"])` →
    /// lux `ERR wrong number of arguments` → 计入 failed_devids → 监控表现为
    /// "采集正常但存储一直报错"。
    #[tokio::test]
    async fn write_batch_empty_values_noop_no_hset() {
        let (_h, c) = start_lux("empty_vals").await;
        let s = sample("d_empty", &[]);
        let o = write_batch(&c, std::slice::from_ref(&s)).await.unwrap();
        assert!(o.all_ok(), "空 values 不应被记为写失败: {o:?}");
        let arr = c
            .execute_value("HGETALL", &["rtdata:d_empty"])
            .await
            .unwrap();
        assert!(
            matches!(&arr, lux::EmbeddedValue::Array(a) if a.is_empty()),
            "空 values 不应创建 rtdata entry，实际: {arr:?}"
        );
    }

    /// F-3/F-4 回归锁：坏质量（Null）与非有限 Float 不覆盖上次有效值——
    /// 保留原值与**原时间戳**（ts 不前进 = 陈旧，可据此判活），实时面不出现 "NaN"。
    #[tokio::test]
    async fn write_batch_bad_values_keep_last_good() {
        let (_h, c) = start_lux("bad_quality").await;
        let good = sample("d1", &[("t1", 1.0)]);
        write_batch(&c, std::slice::from_ref(&good)).await.unwrap();
        let mut bad = sample("d1", &[]);
        bad.timestamp = 1_700_000_050;
        bad.values
            .insert("t1".to_string(), SampleValue::Float(f64::NAN));
        bad.values.insert("t2".to_string(), SampleValue::Null);
        bad.values
            .insert("t3".to_string(), SampleValue::Float(f64::INFINITY));
        write_batch(&c, std::slice::from_ref(&bad)).await.unwrap();
        let cells = cells(&c, "d1").await;
        let t1 = cells.get("d1.t1").expect("t1 保留上次有效值");
        assert_eq!(t1[2], serde_json::json!("1"), "NaN 不得覆盖有效值");
        assert_eq!(
            t1[0],
            serde_json::json!(1_700_000_000),
            "ts 不前进（陈旧可判）"
        );
        assert!(!cells.contains_key("d1.t2"), "Null 点不落 rtdata");
        assert!(!cells.contains_key("d1.t3"), "Inf 点不落 rtdata");
    }
}
