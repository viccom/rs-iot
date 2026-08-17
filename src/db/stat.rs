//! 通用实例指标（M4）：lux string key `stat:<inst_id>` 存 JSON。
//!
//! 核心 6 字段所有实例一致；插件扩展走 `extra`（serde flatten 透传）。
//! 替代 M2 的 `luastat`（lua 专属）——lua 迁移后此模块是全部实例的 stat 承载。

use crate::error::RsIotError;
use lux::{EmbeddedClient, EmbeddedValue};

/// 实例状态快照（写 lux `stat:<inst_id>`）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InstanceStat {
    /// health 维度：`running` / `degraded` / `stopped`。死活看进程级 `InstState`。
    pub status: String,
    /// 累计活动成功次数（counter）。
    pub tick_count: u64,
    /// 累计活动失败次数（counter）。
    pub fail_count: u64,
    /// 最近一次活动墙上时间（unix 秒）。
    pub last_activity_ts: i64,
    /// 最近一次活动耗时（毫秒）。
    pub last_activity_duration_ms: u64,
    /// 最近一次失败原因（成功时清空）。
    pub last_err: String,
    /// 插件扩展字段（flatten 透传，如 opcua session_reconnect_count、lua vm_restart_count）。
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// key 名 `stat:<inst_id>`。
fn key(inst_id: &str) -> String {
    format!("stat:{inst_id}")
}

/// 写状态快照（JSON，`SET stat:<inst>`）。
pub async fn write(
    c: &EmbeddedClient,
    inst_id: &str,
    stat: &InstanceStat,
) -> Result<(), RsIotError> {
    let json = serde_json::to_string(stat)
        .map_err(|e| RsIotError::Config(format!("InstanceStat 序列化失败：{e}")))?;
    c.execute("SET", &[&key(inst_id), &json]).await?;
    Ok(())
}

/// 读状态快照（无则 `None`）。
pub async fn read(c: &EmbeddedClient, inst_id: &str) -> Result<Option<InstanceStat>, RsIotError> {
    let v = c.execute_value("GET", &[&key(inst_id)]).await?;
    match v {
        EmbeddedValue::Bulk(b) => {
            let s = String::from_utf8_lossy(&b);
            if s.is_empty() {
                return Ok(None);
            }
            let stat: InstanceStat = serde_json::from_str(&s)
                .map_err(|e| RsIotError::Config(format!("InstanceStat 反序列化失败：{e}")))?;
            Ok(Some(stat))
        }
        EmbeddedValue::Simple(s) if !s.is_empty() => {
            let stat: InstanceStat = serde_json::from_str(&s)
                .map_err(|e| RsIotError::Config(format!("InstanceStat 反序列化失败：{e}")))?;
            Ok(Some(stat))
        }
        _ => Ok(None),
    }
}

/// 删除实例状态快照（`DEL stat:<inst_id>`）。
///
/// 实例删除时调，避免残留 stat 被误读为存活实例的指标。DEL 模式对标 `luakv::remove`。
pub async fn delete(c: &EmbeddedClient, inst_id: &str) -> Result<(), RsIotError> {
    c.execute("DEL", &[&key(inst_id)]).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn client() -> EmbeddedClient {
        let cfg = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: std::env::temp_dir()
                    .join(format!("inststat-{}", std::process::id()))
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

    fn sample() -> InstanceStat {
        InstanceStat {
            status: "running".into(),
            tick_count: 5,
            fail_count: 1,
            last_activity_ts: 123,
            last_activity_duration_ms: 42,
            last_err: "timeout".into(),
            extra: serde_json::Map::new(),
        }
    }

    #[tokio::test]
    async fn write_read_roundtrip() {
        let c = client().await;
        write(&c, "inst1", &sample()).await.unwrap();
        let got = read(&c, "inst1").await.unwrap().expect("stat 应存在");
        assert_eq!(got.status, "running");
        assert_eq!(got.tick_count, 5);
        assert_eq!(got.fail_count, 1);
        assert_eq!(got.last_err, "timeout");
    }

    #[tokio::test]
    async fn read_missing_is_none() {
        let c = client().await;
        assert!(read(&c, "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn instances_are_isolated() {
        let c = client().await;
        write(&c, "a", &sample()).await.unwrap();
        assert!(read(&c, "b").await.unwrap().is_none(), "实例间隔离");
    }

    /// 删除实例 stat（B 修复）：delete 后 read 应为 None，其他实例不受影响。
    #[tokio::test]
    async fn delete_removes_only_target_inst() {
        let c = client().await;
        write(&c, "a", &sample()).await.unwrap();
        write(&c, "b", &sample()).await.unwrap();
        delete(&c, "a").await.unwrap();
        assert!(read(&c, "a").await.unwrap().is_none(), "a 的 stat 应被删除");
        assert!(read(&c, "b").await.unwrap().is_some(), "b 不受影响");
    }

    #[test]
    fn extra_flatten_preserves_extension_fields() {
        let mut s = sample();
        s.extra
            .insert("vm_restart_count".into(), serde_json::json!(3));
        let json = serde_json::to_string(&s).unwrap();
        assert!(
            json.contains("\"vm_restart_count\":3"),
            "extra 应 flatten 到顶层"
        );
        let back: InstanceStat = serde_json::from_str(&json).unwrap();
        assert_eq!(back.extra["vm_restart_count"], 3, "未知字段回到 extra");
        assert_eq!(back.tick_count, 5, "核心字段正常");
    }
}
