//! 标签属性注册表（lux Hash `tags`；设计见 docs/designs/2026-08-24-tag-registry-plan.md）。
//!
//! **M-4 退役中**：出口登记已转型发现引擎（`model::discovery`，决策 D-M3——
//! tags 表不再是运行时登记目标，未建模数据进 discoveries 队列）；本模块仅余
//! tags 表 CRUD 供过渡读取（M-4 T3 随 routes_tag 改造——effective 视图 +
//! devreg.overrides 写入——一并退役）。`glob_match` 复用（批量建模/清理）。

use crate::error::RsIotError;
use crate::types::{tag_key, TagProps};
use lux::EmbeddedClient;
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

/// lux hash 名（全局单表）。
const HASH: &str = "tags";

/// 进程级"已确认"集合（API set/delete 共享；发现引擎不再用）。
static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn seen() -> &'static Mutex<HashSet<String>> {
    SEEN.get_or_init(|| Mutex::new(HashSet::new()))
}

// ---------- 纯函数 ----------

/// 极简 glob：`*` 通配任意串（含空）。pattern 无 `*` = 精确匹配。
///
/// 分段算法（无回溯）：按 `*` 切段，首段必须前缀、末段必须后缀、中间段依序
/// `contains` 且不重叠。`*furnace*.temp*` 形态覆盖 dev 前缀 + tag 后缀场景。
pub fn glob_match(pattern: &str, key: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == key;
    }
    let mut rest = key;
    // 首段（`*` 前）：前缀锚定。
    if !rest.starts_with(parts[0]) {
        return false;
    }
    rest = &rest[parts[0].len()..];
    // 中间段：依序找子串，找完消耗前缀（不重叠）。
    for part in &parts[1..parts.len() - 1] {
        match rest.find(part) {
            Some(pos) => rest = &rest[pos + part.len()..],
            None => return false,
        }
    }
    // 末段（最后一个 `*` 后）：后缀锚定。
    rest.ends_with(parts[parts.len() - 1])
}

/// API set/batch 的可写字段（owner_inst 系统管理，不接受修改）。
/// 全量覆盖语义：`None` = 清除该属性。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct TagSetReq {
    /// 历史归档白名单。
    #[serde(default)]
    pub history: Option<bool>,
    /// 绝对死区。
    #[serde(default)]
    pub deadband: Option<f64>,
    /// 描述。
    #[serde(default)]
    pub desc: Option<String>,
    /// 工程单位。
    #[serde(default)]
    pub unit: Option<String>,
}

/// 把请求字段覆盖到 props（owner_inst 不动）。
pub fn apply_set(props: &mut TagProps, req: &TagSetReq) {
    props.history = req.history.unwrap_or(false);
    props.deadband = req.deadband;
    props.desc = req.desc.clone();
    props.unit = req.unit.clone();
}

/// glob 批量改属性到已存在记录。返回 (matched, updated)。
pub async fn batch_set(
    c: &EmbeddedClient,
    pattern: &str,
    req: &TagSetReq,
) -> Result<(usize, usize), RsIotError> {
    let all = list(c).await?;
    let mut matched = 0usize;
    let mut updated = 0usize;
    for key in all.keys() {
        if !glob_match(pattern, key) {
            continue;
        }
        matched += 1;
        let mut props = all[key].clone();
        apply_set(&mut props, req);
        let raw = serde_json::to_string(&props)?;
        c.execute("HSET", &[HASH, key, &raw]).await?;
        updated += 1;
    }
    Ok((matched, updated))
}

/// 读单条（缺 → None）。
pub async fn get(c: &EmbeddedClient, dev: &str, tag: &str) -> Result<Option<TagProps>, RsIotError> {
    let key = tag_key(dev, tag);
    let v = c.execute_value("HGET", &[HASH, &key]).await?;
    match embedded_to_string(v) {
        Some(raw) => Ok(Some(serde_json::from_str(&raw)?)),
        None => Ok(None),
    }
}

/// 全表（HGETALL 扁平序列解析，同 dev_registry 口径）。key = `<devid>.<tag>`。
pub async fn list(c: &EmbeddedClient) -> Result<HashMap<String, TagProps>, RsIotError> {
    let v = c.execute_value("HGETALL", &[HASH]).await?;
    let mut out = HashMap::new();
    if let lux::EmbeddedValue::Array(arr) = v {
        let mut i = 0;
        while i + 1 < arr.len() {
            let key = embedded_to_string(arr[i].clone()).unwrap_or_default();
            let raw = embedded_to_string(arr[i + 1].clone()).unwrap_or_default();
            if !key.is_empty() {
                if let Ok(props) = serde_json::from_str::<TagProps>(&raw) {
                    out.insert(key, props);
                }
            }
            i += 2;
        }
    }
    Ok(out)
}

/// API set：RMW（HGET 现值 → apply_set 覆盖可写字段 → HSET）。
/// 记录不存在 → 以默认值建卡（owner_inst 空——API 建卡无宿主可查）。
pub async fn set(
    c: &EmbeddedClient,
    dev: &str,
    tag: &str,
    req: &TagSetReq,
) -> Result<TagProps, RsIotError> {
    let key = tag_key(dev, tag);
    let mut props = get(c, dev, tag).await?.unwrap_or_default();
    apply_set(&mut props, req);
    let raw = serde_json::to_string(&props)?;
    c.execute("HSET", &[HASH, &key, &raw]).await?;
    seen().lock().unwrap().insert(key);
    Ok(props)
}

/// 删除属性记录（rtdata 值/后续历史不动）。同步移除 seen——再出数重新占位。
pub async fn delete(c: &EmbeddedClient, dev: &str, tag: &str) -> Result<(), RsIotError> {
    let key = tag_key(dev, tag);
    c.execute("HDEL", &[HASH, &key]).await?;
    seen().lock().unwrap().remove(&key);
    Ok(())
}

/// lux EmbeddedValue → String（Bulk/Simple；同 dev_registry 口径）。
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
        // 不重叠：消费过的子串不复用。
        assert!(!glob_match("a*a*a", "aa"));
        assert!(glob_match("a*a*a", "aaa"));
    }

    #[test]
    fn glob_single_star_matches_all() {
        assert!(glob_match("*", "anything.at.all"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn apply_set_overwrites_all_writable_fields_and_keeps_owner() {
        let mut props = TagProps {
            history: true,
            deadband: Some(1.0),
            desc: Some("旧".into()),
            unit: Some("kPa".into()),
            owner_inst: "sim1".into(),
        };
        apply_set(
            &mut props,
            &TagSetReq {
                history: Some(false),
                deadband: None,
                desc: None,
                unit: Some("°C".into()),
            },
        );
        assert!(!props.history);
        assert_eq!(props.deadband, None, "None = 清除");
        assert_eq!(props.desc, None);
        assert_eq!(props.unit.as_deref(), Some("°C"));
        assert_eq!(props.owner_inst, "sim1", "owner_inst 不受 API 修改");
    }

    #[test]
    fn apply_set_default_req_resets_to_defaults() {
        let mut props = TagProps {
            history: true,
            owner_inst: "x".into(),
            ..Default::default()
        };
        apply_set(&mut props, &TagSetReq::default());
        assert_eq!(
            props,
            TagProps {
                owner_inst: "x".into(),
                ..Default::default()
            }
        );
    }
}
