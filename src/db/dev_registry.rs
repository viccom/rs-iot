//! 设备归属注册表（决策 D-OWN-2，spec `2026-08-21-device-ownership-design.md` §2）：
//! lux Hash `dev2inst`。
//!
//! - field = devid，value = 宿主 inst_id 逗号分隔列表（如 `"a,b"`，单宿主无分隔符）；
//! - **多宿主并记**：跨实例同 devid 共写是既有遗留，本表只记账不治理（spec §9
//!   明确不做写合并语义），同 devid 被多实例写时全部归属在册；
//! - **rtdata 删除规则 = 最后宿主移除才清**：`remove_owner` 移除后列表空 →
//!   HDEL 该 field 并返回 `true`（"列表已空"信号），调用方（cascade_delete）
//!   据此决定是否 `DEL rtdata:<devid>`——共写设备删一个宿主不清数据；
//! - `devices_of` 反查宿主名下全部 devid（cascade_delete 闭环入口）；
//! - `clear` 整表清除（purge scope=all/rtdata 用——rtdata 没了归属无意义）。
//!
//! RMW 的 HGET→HSET 非事务原子（同 `alarms` 模块口径，EmbeddedClient Clone =
//! 独立 session）——归属是管理面记账数据，最终一致可接受（启动预登记兜底）。

use crate::error::RsIotError;
use lux::EmbeddedClient;

/// lux hash 名（全局单表）。
const HASH: &str = "dev2inst";

/// 登记归属：HGET 现值 → 不含该宿主则追加 → HSET（含则 no-op，防每 tick 重复写）。
pub async fn note(c: &EmbeddedClient, devid: &str, inst_id: &str) -> Result<(), RsIotError> {
    let mut list = owners(c, devid).await?;
    if list.iter().any(|s| s == inst_id) {
        return Ok(());
    }
    list.push(inst_id.to_string());
    c.execute("HSET", &[HASH, devid, &list.join(",")]).await?;
    Ok(())
}

/// 读 devid 的宿主列表（空/缺键 → `vec![]`）。
pub async fn owners(c: &EmbeddedClient, devid: &str) -> Result<Vec<String>, RsIotError> {
    let v = c.execute_value("HGET", &[HASH, devid]).await?;
    Ok(match embedded_to_string(v) {
        Some(raw) => raw
            .split(',')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
        None => vec![],
    })
}

/// 移除一个宿主：从列表移除后为空 → HDEL 该 field 并返回 `true`（列表已空，
/// 调用方据此决定是否删 rtdata）；非空 → HSET 回写返 `false`；devid 或宿主
/// 不存在 → 返 `false`（幂等）。
pub async fn remove_owner(
    c: &EmbeddedClient,
    devid: &str,
    inst_id: &str,
) -> Result<bool, RsIotError> {
    let cur = owners(c, devid).await?;
    let next: Vec<String> = cur.iter().filter(|s| *s != inst_id).cloned().collect();
    if next.len() == cur.len() {
        return Ok(false);
    }
    if next.is_empty() {
        c.execute("HDEL", &[HASH, devid]).await?;
        Ok(true)
    } else {
        c.execute("HSET", &[HASH, devid, &next.join(",")]).await?;
        Ok(false)
    }
}

/// 反查宿主名下全部 devid（HGETALL 解析每 field 的宿主列表；cascade 用）。
pub async fn devices_of(c: &EmbeddedClient, inst_id: &str) -> Result<Vec<String>, RsIotError> {
    let v = c.execute_value("HGETALL", &[HASH]).await?;
    let mut out = Vec::new();
    if let lux::EmbeddedValue::Array(arr) = v {
        // RESP 扁平序列：k1, v1, k2, v2, ...（同 alarms::list 解析形态）。
        let mut i = 0;
        while i + 1 < arr.len() {
            let devid = embedded_to_string(arr[i].clone()).unwrap_or_default();
            let raw = embedded_to_string(arr[i + 1].clone()).unwrap_or_default();
            if !devid.is_empty() && raw.split(',').any(|s| s == inst_id) {
                out.push(devid);
            }
            i += 2;
        }
    }
    Ok(out)
}

/// 读全表 `(devid, 宿主列表)` 对（`GET /devices` 运行时补集入口；空表 → `vec![]`）。
///
/// 宿主保持登记顺序（多宿主逗号列表原序拆分）。
pub async fn all(c: &EmbeddedClient) -> Result<Vec<(String, Vec<String>)>, RsIotError> {
    let v = c.execute_value("HGETALL", &[HASH]).await?;
    let mut out = Vec::new();
    if let lux::EmbeddedValue::Array(arr) = v {
        // RESP 扁平序列：k1, v1, k2, v2, ...（同 devices_of 解析形态）。
        let mut i = 0;
        while i + 1 < arr.len() {
            let devid = embedded_to_string(arr[i].clone()).unwrap_or_default();
            let raw = embedded_to_string(arr[i + 1].clone()).unwrap_or_default();
            if !devid.is_empty() {
                let owners: Vec<String> = raw
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
                out.push((devid, owners));
            }
            i += 2;
        }
    }
    Ok(out)
}

/// 整表清除（`DEL dev2inst`，purge 用）。
pub async fn clear(c: &EmbeddedClient) -> Result<(), RsIotError> {
    c.execute("DEL", &[HASH]).await?;
    Ok(())
}

/// HGET 值 → 字符串（Nil/空 → `None`，语义同 `alarms::embedded_to_string`）。
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

    /// 唯一临时 data_dir（tag 区分用例，防并行测试互扰；照 alarms::tests 模式）。
    async fn test_client(tag: &str) -> lux::EmbeddedClient {
        let cfg = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: std::env::temp_dir()
                    .join(format!("devreg-{tag}-{}", std::process::id()))
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

    /// 新 devid 首次 note：field 建立，单宿主无分隔符。
    #[tokio::test]
    async fn note_new_devid_creates() {
        let c = test_client("note-new").await;
        note(&c, "dev1", "inst-a").await.unwrap();
        assert_eq!(owners(&c, "dev1").await.unwrap(), vec!["inst-a"]);
    }

    /// 同宿主二次 note：不重复（去重 no-op）。
    #[tokio::test]
    async fn note_dedupes() {
        let c = test_client("note-dedup").await;
        note(&c, "dev1", "inst-a").await.unwrap();
        note(&c, "dev1", "inst-a").await.unwrap();
        assert_eq!(
            owners(&c, "dev1").await.unwrap(),
            vec!["inst-a"],
            "同宿主重复 note 不应产生第二个条目"
        );
    }

    /// 第二宿主 note：追加为逗号列表 "a,b"（D-OWN-2 多宿主并记）。
    #[tokio::test]
    async fn note_second_owner_appends() {
        let c = test_client("note-append").await;
        note(&c, "dev1", "inst-a").await.unwrap();
        note(&c, "dev1", "inst-b").await.unwrap();
        assert_eq!(
            owners(&c, "dev1").await.unwrap(),
            vec!["inst-a", "inst-b"],
            "多宿主按登记顺序并记"
        );
        // 线格式锚定：value 是逗号分隔列表。
        let raw = c.execute_value("HGET", &[HASH, "dev1"]).await.unwrap();
        assert_eq!(embedded_to_string(raw).as_deref(), Some("inst-a,inst-b"));
    }

    /// 单宿主移除后列表空：HDEL field + 返回 true（"列表已空"信号）。
    #[tokio::test]
    async fn remove_owner_to_empty_deletes_and_signals() {
        let c = test_client("rm-empty").await;
        note(&c, "dev1", "inst-a").await.unwrap();
        let emptied = remove_owner(&c, "dev1", "inst-a").await.unwrap();
        assert!(emptied, "移除唯一宿主应返回 true（列表已空）");
        assert_eq!(owners(&c, "dev1").await.unwrap(), Vec::<String>::new());
        let v = c.execute_value("HGET", &[HASH, "dev1"]).await.unwrap();
        assert!(matches!(v, lux::EmbeddedValue::Nil), "field 应被 HDEL");
    }

    /// 双宿主删其一：返 false + 剩余宿主保留（共写设备删一个宿主不清数据）。
    #[tokio::test]
    async fn remove_owner_keeps_others() {
        let c = test_client("rm-keep").await;
        note(&c, "dev1", "inst-a").await.unwrap();
        note(&c, "dev1", "inst-b").await.unwrap();
        let emptied = remove_owner(&c, "dev1", "inst-a").await.unwrap();
        assert!(!emptied, "仍有宿主 inst-b，不应报列表已空");
        assert_eq!(owners(&c, "dev1").await.unwrap(), vec!["inst-b"]);
    }

    /// devid 或宿主不存在：幂等返 false，不建 field 不报错。
    #[tokio::test]
    async fn remove_owner_missing_is_noop_false() {
        let c = test_client("rm-miss").await;
        assert!(
            !remove_owner(&c, "nope", "inst-a").await.unwrap(),
            "缺 devid 应返 false"
        );
        note(&c, "dev1", "inst-a").await.unwrap();
        assert!(
            !remove_owner(&c, "dev1", "inst-x").await.unwrap(),
            "devid 在册但宿主不在列表应返 false"
        );
        assert_eq!(owners(&c, "dev1").await.unwrap(), vec!["inst-a"]);
    }

    /// 反查：跨多个 devid（含多宿主）找出宿主名下全部设备。
    #[tokio::test]
    async fn devices_of_reverse_lookup() {
        let c = test_client("rev").await;
        note(&c, "dev1", "inst-a").await.unwrap();
        note(&c, "dev2", "inst-a").await.unwrap();
        note(&c, "dev2", "inst-b").await.unwrap();
        note(&c, "dev3", "inst-b").await.unwrap();

        let mut devs_a = devices_of(&c, "inst-a").await.unwrap();
        devs_a.sort();
        assert_eq!(devs_a, vec!["dev1", "dev2"]);
        let mut devs_b = devices_of(&c, "inst-b").await.unwrap();
        devs_b.sort();
        assert_eq!(devs_b, vec!["dev2", "dev3"]);
        assert!(
            devices_of(&c, "inst-none").await.unwrap().is_empty(),
            "无归属宿主应返空"
        );
    }

    /// 全表读取（GET /devices 运行时补集入口）：每条目 (devid, 宿主列表)，
    /// 宿主保持登记顺序；空表返空 vec。
    #[tokio::test]
    async fn all_lists_entries_with_owner_lists() {
        let c = test_client("all").await;
        assert!(
            all(&c).await.unwrap().is_empty(),
            "空表应返空 vec（GET /devices 退化为纯配置聚合）"
        );
        note(&c, "dev1", "inst-a").await.unwrap();
        note(&c, "dev1", "inst-b").await.unwrap();
        note(&c, "dev2", "inst-b").await.unwrap();
        let mut entries = all(&c).await.unwrap();
        entries.sort();
        assert_eq!(
            entries,
            vec![
                (
                    "dev1".to_string(),
                    vec!["inst-a".to_string(), "inst-b".to_string()]
                ),
                ("dev2".to_string(), vec!["inst-b".to_string()]),
            ],
            "每 devid 带全部宿主，保持登记顺序"
        );
    }

    /// clear 整表清除（purge 路径）。
    #[tokio::test]
    async fn clear_wipes() {
        let c = test_client("clear").await;
        note(&c, "dev1", "inst-a").await.unwrap();
        note(&c, "dev2", "inst-b").await.unwrap();
        clear(&c).await.unwrap();
        assert_eq!(owners(&c, "dev1").await.unwrap(), Vec::<String>::new());
        assert!(
            devices_of(&c, "inst-a").await.unwrap().is_empty(),
            "清表后反查应空"
        );
        // 幂等：空表再 clear 仍 Ok。
        clear(&c).await.unwrap();
    }
}
