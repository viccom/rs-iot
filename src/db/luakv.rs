//! Lua 脚本本地持久化（spec §11）：lux Hash `luakv:<inst_id>`。
//!
//! 每实例一个独立 hash：field = key（限 `[A-Za-z0-9_.-]`，≤128 字节），value = JSON
//! 文本（number/string/bool/table，≤16KB 防滥用）。实例删除时 `remove` 清空整个 hash
//! （`routes_inst::delete` 调，与 `ext_token::remove` 同位）。

use crate::error::RsIotError;
use lux::{EmbeddedClient, EmbeddedValue};

/// key 最大字节数。
const MAX_KEY_LEN: usize = 128;
/// 单值最大字节数（JSON 文本）。
const MAX_VALUE_LEN: usize = 16 * 1024;

/// 校验 key 合法（`[A-Za-z0-9_.-]`，非空，≤128）。
fn validate_key(key: &str) -> Result<(), RsIotError> {
    if key.is_empty() || key.len() > MAX_KEY_LEN {
        return Err(RsIotError::Config(format!(
            "luakv key 长度非法（1..={MAX_KEY_LEN}）：'{key}'"
        )));
    }
    if !key
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
    {
        return Err(RsIotError::Config(format!(
            "luakv key 含非法字符（仅 [A-Za-z0-9_.-]）：'{key}'"
        )));
    }
    Ok(())
}

/// hash 名 `luakv:<inst_id>`（每实例一个独立 hash）。
fn hash_name(inst_id: &str) -> String {
    format!("luakv:{inst_id}")
}

/// 写一个 key（JSON 文本）。key 非法或 value 超限返回 Err。
pub async fn set(
    c: &EmbeddedClient,
    inst_id: &str,
    key: &str,
    json: &str,
) -> Result<(), RsIotError> {
    validate_key(key)?;
    if json.len() > MAX_VALUE_LEN {
        return Err(RsIotError::Config(format!(
            "luakv value 超限（>{MAX_VALUE_LEN} 字节）：key '{key}'"
        )));
    }
    c.execute("HSET", &[&hash_name(inst_id), key, json]).await?;
    Ok(())
}

/// 读一个 key 的 JSON 文本（无则 `None`）。
pub async fn get(
    c: &EmbeddedClient,
    inst_id: &str,
    key: &str,
) -> Result<Option<String>, RsIotError> {
    validate_key(key)?;
    let v = c.execute_value("HGET", &[&hash_name(inst_id), key]).await?;
    match v {
        EmbeddedValue::Bulk(b) => {
            let s = String::from_utf8_lossy(&b).into_owned();
            if s.is_empty() {
                Ok(None)
            } else {
                Ok(Some(s))
            }
        }
        EmbeddedValue::Simple(s) if !s.is_empty() => Ok(Some(s)),
        _ => Ok(None),
    }
}

/// 删除实例全部 kv（实例删除时调，DEL 删整个 hash）。
pub async fn remove(c: &EmbeddedClient, inst_id: &str) -> Result<(), RsIotError> {
    c.execute("DEL", &[&hash_name(inst_id)]).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_validation() {
        assert!(validate_key("counter").is_ok());
        assert!(validate_key("a_b.c-1").is_ok());
        assert!(validate_key("").is_err());
        assert!(validate_key("a/b").is_err(), "非法字符应拒");
        assert!(validate_key(&"x".repeat(129)).is_err(), "超长应拒");
    }

    async fn test_client() -> lux::EmbeddedClient {
        let cfg = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: std::env::temp_dir()
                    .join(format!("luakv-{}", std::process::id()))
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

    #[tokio::test]
    async fn set_get_roundtrip() {
        let c = test_client().await;
        set(&c, "inst1", "k", "42").await.unwrap();
        assert_eq!(get(&c, "inst1", "k").await.unwrap().as_deref(), Some("42"));
        assert_eq!(get(&c, "inst1", "missing").await.unwrap(), None);
        assert_eq!(get(&c, "other", "k").await.unwrap(), None, "实例间隔离");
    }

    #[tokio::test]
    async fn set_rejects_bad_key_and_oversize() {
        let c = test_client().await;
        assert!(set(&c, "i", "a/b", "1").await.is_err());
        let big = "x".repeat(MAX_VALUE_LEN + 1);
        assert!(set(&c, "i", "k", &big).await.is_err());
    }

    #[tokio::test]
    async fn remove_clears_instance_kv() {
        let c = test_client().await;
        set(&c, "i", "k1", "1").await.unwrap();
        set(&c, "i", "k2", "2").await.unwrap();
        remove(&c, "i").await.unwrap();
        assert_eq!(get(&c, "i", "k1").await.unwrap(), None);
        assert_eq!(get(&c, "i", "k2").await.unwrap(), None);
    }
}
