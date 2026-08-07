//! Lua 实例状态快照（spec §13）：lux string key `stat:<inst_id>` 存 JSON。
//!
//! luaClient `run()` 内采点（collect 成败/耗时 + 热加载次数 + 脚本版本），每次 collect/reload/
//! 启停写一次。REST `GET /api/v1/lua/stats?inst_id=<inst>` 读回（`routes_lua::stats`）。
//!
//! v1 不含 `io_connected`/`last_io_err`（io_engine 未暴露连接状态，后置 Plan 7/M8）。

use crate::error::RsIotError;
use lux::{EmbeddedClient, EmbeddedValue};

/// 实例状态快照（写 lux `stat:<inst_id>`）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LuaStat {
    /// `running` / `degraded` / `stopped`。
    pub status: String,
    /// 累计 collect 成功周期数。
    pub collect_count: u64,
    /// 累计 collect 失败周期数。
    pub collect_fail_count: u64,
    /// 最近一次 collect 的墙上时间（unix 秒）。
    pub last_collect_ts: i64,
    /// 最近一次 collect 耗时（毫秒）。
    pub last_collect_duration_ms: u64,
    /// VM 重建次数（热加载/崩溃恢复）。
    pub vm_restart_count: u64,
    /// 脚本版本（main.lua sha256 前 8 hex）。
    pub script_version: String,
    /// 最近一次失败原因（成功时清空）。
    pub last_err: String,
}

/// key 名 `stat:<inst_id>`。
fn key(inst_id: &str) -> String {
    format!("stat:{inst_id}")
}

/// 写状态快照（JSON，`SET stat:<inst>`）。
pub async fn write(c: &EmbeddedClient, inst_id: &str, stat: &LuaStat) -> Result<(), RsIotError> {
    let json = serde_json::to_string(stat)
        .map_err(|e| RsIotError::Config(format!("LuaStat 序列化失败：{e}")))?;
    c.execute("SET", &[&key(inst_id), &json]).await?;
    Ok(())
}

/// 读状态快照（无则 `None`）。
pub async fn read(c: &EmbeddedClient, inst_id: &str) -> Result<Option<LuaStat>, RsIotError> {
    let v = c.execute_value("GET", &[&key(inst_id)]).await?;
    match v {
        EmbeddedValue::Bulk(b) => {
            let s = String::from_utf8_lossy(&b);
            if s.is_empty() {
                return Ok(None);
            }
            let stat: LuaStat = serde_json::from_str(&s)
                .map_err(|e| RsIotError::Config(format!("LuaStat 反序列化失败：{e}")))?;
            Ok(Some(stat))
        }
        EmbeddedValue::Simple(s) if !s.is_empty() => {
            let stat: LuaStat = serde_json::from_str(&s)
                .map_err(|e| RsIotError::Config(format!("LuaStat 反序列化失败：{e}")))?;
            Ok(Some(stat))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn client() -> EmbeddedClient {
        let cfg = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: std::env::temp_dir()
                    .join(format!("luastat-{}", std::process::id()))
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

    /// 构造一个非空快照（覆盖全部字段）。
    fn sample() -> LuaStat {
        LuaStat {
            status: "running".into(),
            collect_count: 5,
            collect_fail_count: 1,
            last_collect_ts: 123,
            last_collect_duration_ms: 42,
            vm_restart_count: 2,
            script_version: "abcdef12".into(),
            last_err: "timeout".into(),
        }
    }

    #[tokio::test]
    async fn write_read_roundtrip() {
        let c = client().await;
        write(&c, "inst1", &sample()).await.unwrap();
        let got = read(&c, "inst1").await.unwrap().expect("stat 应存在");
        assert_eq!(got.status, "running");
        assert_eq!(got.collect_count, 5);
        assert_eq!(got.collect_fail_count, 1);
        assert_eq!(got.vm_restart_count, 2);
        assert_eq!(got.script_version, "abcdef12");
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
}
