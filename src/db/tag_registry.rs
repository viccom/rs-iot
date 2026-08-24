//! 标签属性注册表（lux Hash `tags`；设计见 docs/designs/2026-08-24-tag-registry-plan.md）。
//!
//! - field = `tag_key(devid, tag)`（与 rtdata field 同构的 `<devid>.<tag>`），
//!   value = [`TagProps`] JSON——标签身份单一规范化源（`types::tag_key`）；
//! - **叠加层**：采集地址语义仍在各插件 `ChildSpec`，本表只存跨插件共享属性
//!   （history/deadband/desc/unit/owner_inst）；
//! - **三路径登记、只补缺不覆盖**：配置声明式（`declared` 初值）→ 出口自动
//!   （`note_once`，pipeline flush 咽喉）→ API 人工（`set`/`batch_set`）。
//!   自动路径对已有记录 no-op——人工修改在实例重启后不丢；
//! - **进程级 once-set 去重**（`SEEN`）：首见 tag 才探测 lux，稳态零读零写。
//!   `delete` 同步移除 seen——删除后再出数重新占位登记；
//! - RMW 的 HGET→HSET 非事务原子（同 `dev_registry` 口径）——管理面属性数据，
//!   最终一致可接受（出口登记 next tick 兜底重试，seen 语义见 `note_once` 注释）。
//!
//! `owner_inst` 取 `dev2inst` 宿主列表首个（多宿主共写是遗留形态，本表只记账
//! 不治理——同 dev2inst 哲学）。

use crate::error::RsIotError;
use crate::types::{tag_key, Sample, TagProps};
use lux::EmbeddedClient;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

/// lux hash 名（全局单表）。
const HASH: &str = "tags";

/// 进程级"已确认写入 lux"集合——稳态命中零 lux 交互。
static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn seen() -> &'static Mutex<HashSet<String>> {
    SEEN.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 当前声明式初值快照（Arc clone，廉价）。
///
/// M-2 起数据源 = **effective 物化缓存**（模型模板默认 ∪ devreg 覆盖，
/// `model::cache`）——内联 `ChildSpec.props` 已随实例绑定化消失，模板默认成为
/// 声明式初值的唯一权威（比旧双候选登记更准：三态合并后的最终意图）。
pub fn declared() -> Arc<HashMap<String, TagProps>> {
    crate::model::cache::effective()
}

/// 刷新声明式初值（= effective 缓存全量重建；启动 + startinst 后刷新）。
pub async fn refresh_declared(c: &EmbeddedClient) -> Result<usize, RsIotError> {
    let m = crate::model::cache::reload(c).await?;
    Ok(m.len())
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
    #[serde(default)]
    pub history: Option<bool>,
    #[serde(default)]
    pub deadband: Option<f64>,
    #[serde(default)]
    pub desc: Option<String>,
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

/// 出口登记（pipeline flush 咽喉调用）：batch 内首见 `(devid, tag)` 探测 lux，
/// 缺 → 占位写入（declared 初值或默认 + owner_inst 取 dev2inst 首宿主）。
///
/// - once-set **先加后查**：本批内去重 + 失败下批重试（探测/写入失败时从
///   seen 移除，next flush 兜底——最终一致）；
/// - 表中已有 → 不写（"只补缺不覆盖"：API 人工修改不被实例重启冲掉）；
/// - 本函数 fail-soft 由调用方决定（pipeline 侧 warn+计数，不影响写路径）。
pub async fn note_once(
    c: &EmbeddedClient,
    samples: &[Sample],
    declared: &HashMap<String, TagProps>,
) -> Result<(), RsIotError> {
    // 本批候选：去重后的 (key, devid)（devid 供 owner 反查）。
    // **热路径零分配**：key 复用同一 scratch String 组装 + `contains(&str)` 借用查找
    //（50万点/秒管线门槛——逐点 String 分配实测 -13% 端到端吞吐，见 plan 文档）；
    // 仅真正的新 tag（罕见）才 clone 入候选。
    let mut candidates: Vec<(String, String)> = Vec::new();
    {
        let seen = seen().lock().unwrap();
        let mut key = String::new();
        for s in samples {
            for tag in s.values.keys() {
                key.clear();
                if tag.contains(s.devid.as_str()) {
                    key.push_str(tag);
                } else {
                    key.push_str(&s.devid);
                    key.push('.');
                    key.push_str(tag);
                }
                if !seen.contains(key.as_str()) {
                    candidates.push((key.clone(), s.devid.clone()));
                }
            }
        }
    }
    if candidates.is_empty() {
        return Ok(());
    }
    let mut failed: Vec<String> = Vec::new();
    for (key, devid) in &candidates {
        seen().lock().unwrap().insert(key.clone());
        let existing = c.execute_value("HGET", &[HASH, key]).await;
        match existing {
            Ok(v) if embedded_to_string(v.clone()).is_some() => {} // 已有记录：只补缺，不覆盖
            _ => {
                // miss（或 HGET 失败重试路径）：占位写。declared 初值或默认。
                let mut props = declared.get(key).cloned().unwrap_or_default();
                if props.owner_inst.is_empty() {
                    if let Ok(first_owner) = first_owner(c, devid).await {
                        props.owner_inst = first_owner;
                    }
                }
                let raw = serde_json::to_string(&props)?;
                if c.execute("HSET", &[HASH, key, &raw]).await.is_err() {
                    failed.push(key.clone());
                }
            }
        }
    }
    // 失败的从 seen 移除——next flush 重试（最终一致）。
    if !failed.is_empty() {
        let mut seen = seen().lock().unwrap();
        for k in failed {
            seen.remove(&k);
        }
    }
    Ok(())
}

/// dev2inst 宿主列表首个（owner_inst 展示语义；查不到返回空串）。
async fn first_owner(c: &EmbeddedClient, devid: &str) -> Result<String, RsIotError> {
    let v = c.execute_value("HGET", &["dev2inst", devid]).await?;
    Ok(embedded_to_string(v).unwrap_or_default())
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
