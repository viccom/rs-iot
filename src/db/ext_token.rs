//! 外部插件 bridge 的 token 签发/校验/吊销。
//!
//! lux Hash `ext_tokens`：field=`inst_id`，value=JSON`{token_hash, created_at, enabled}`。
//! `token_hash = hmac_sha256(jwt_secret, token)` 的 hex。
//!
//! 为何不用 bcrypt：acceptor 对每个外部连接校验一次，bcrypt 故意慢（~100ms）会成为
//! 连接风暴下的瓶颈；token 本身是 256bit 高熵随机串、不怕离线爆破，hmac 足够快且
//! 绑定 `jwt_secret`——即使 lux 数据泄露（hash 暴露），没有 `jwt_secret` 也无法伪造。

use crate::error::RsIotError;
use hmac::{Hmac, Mac};
use lux::{EmbeddedClient, EmbeddedValue};
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};

type HmacSha256 = Hmac<sha2::Sha256>;

/// token 随机字节数（256bit 高熵）。
const TOKEN_BYTES: usize = 32;

/// 一条 token 记录（存 lux Hash value，JSON）。
#[derive(Debug, Serialize, Deserialize)]
struct TokenRecord {
    /// `hmac_sha256(jwt_secret, token)` 的 hex。
    token_hash: String,
    /// 签发时间（Unix 秒）。
    created_at: i64,
    /// 是否启用（false = 已吊销）。
    enabled: bool,
}

/// 字节 → 小写 hex 字符串（零依赖手写，避免引入 hex crate）。
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = vec![0u8; bytes.len() * 2];
    for (i, b) in bytes.iter().enumerate() {
        out[i * 2] = HEX[(b >> 4) as usize];
        out[i * 2 + 1] = HEX[(b & 0x0f) as usize];
    }
    String::from_utf8(out).expect("hex chars are valid utf-8")
}

/// 单个 hex 字符 → 半字节值。
fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// hex 字符串 → 字节（损坏/奇数长度返回 `None`）。
fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    let bytes = hex.as_bytes();
    if bytes.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for i in (0..bytes.len()).step_by(2) {
        let hi = hex_nibble(bytes[i])?;
        let lo = hex_nibble(bytes[i + 1])?;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

/// 算 `hmac_sha256(jwt_secret, token)` 的 hex。
fn compute_hash(jwt_secret: &str, token: &str) -> Result<String, RsIotError> {
    let mut mac = HmacSha256::new_from_slice(jwt_secret.as_bytes())
        .map_err(|e| RsIotError::Config(format!("hmac key error: {e}")))?;
    mac.update(token.as_bytes());
    Ok(hex_encode(&mac.finalize().into_bytes()))
}

/// 签发：生成 256bit 随机 token，存 hmac hash，返回明文（仅此一次，调用方负责交付外部插件）。
///
/// 幂等性：若该 inst_id 已有 token 则**覆盖**（旧 token 失效）。通常配合外部插件实例创建调用。
pub async fn issue(
    c: &EmbeddedClient,
    jwt_secret: &str,
    inst_id: &str,
) -> Result<String, RsIotError> {
    let mut buf = [0u8; TOKEN_BYTES];
    OsRng.fill_bytes(&mut buf);
    let token = hex_encode(&buf);
    let rec = TokenRecord {
        token_hash: compute_hash(jwt_secret, &token)?,
        created_at: chrono::Local::now().timestamp(),
        enabled: true,
    };
    let v = serde_json::to_string(&rec)?;
    c.execute("HSET", &["ext_tokens", inst_id, &v]).await?;
    Ok(token)
}

/// 校验：token 与 lux 存的 hmac hash 比对，且 `enabled=true`。
///
/// 返回 `false` 的情形：inst_id 无记录、`enabled=false`（已吊销）、hash 不匹配（错误 token）。
pub async fn verify(
    c: &EmbeddedClient,
    jwt_secret: &str,
    inst_id: &str,
    token: &str,
) -> Result<bool, RsIotError> {
    let Some(rec) = get(c, inst_id).await? else {
        return Ok(false);
    };
    if !rec.enabled {
        return Ok(false);
    }
    // 常量时间比较（`mac.verify_slice`），避免 `==` 短路时序泄漏。
    // stored 损坏（非合法 hex）视为校验失败而非错误（拒绝接入）。
    let Some(stored) = hex_decode(&rec.token_hash) else {
        return Ok(false);
    };
    let mut mac = HmacSha256::new_from_slice(jwt_secret.as_bytes())
        .map_err(|e| RsIotError::Config(format!("hmac key error: {e}")))?;
    mac.update(token.as_bytes());
    Ok(mac.verify_slice(&stored).is_ok())
}

/// 吊销：置 `enabled=false`（保留记录便于审计/重放追溯）。
pub async fn revoke(c: &EmbeddedClient, inst_id: &str) -> Result<(), RsIotError> {
    if let Some(mut rec) = get(c, inst_id).await? {
        rec.enabled = false;
        let v = serde_json::to_string(&rec)?;
        c.execute("HSET", &["ext_tokens", inst_id, &v]).await?;
    }
    Ok(())
}

/// 删除实例时彻底清除 token 记录（实例没了，token 必失效——安全底线）。
pub async fn remove(c: &EmbeddedClient, inst_id: &str) -> Result<(), RsIotError> {
    c.execute("HDEL", &["ext_tokens", inst_id]).await?;
    Ok(())
}

/// 取一条记录（无则 `None`）。
async fn get(c: &EmbeddedClient, inst_id: &str) -> Result<Option<TokenRecord>, RsIotError> {
    let v = c.execute_value("HGET", &["ext_tokens", inst_id]).await?;
    match v {
        EmbeddedValue::Bulk(b) => {
            let s = String::from_utf8_lossy(&b).into_owned();
            if s.is_empty() {
                Ok(None)
            } else {
                Ok(Some(serde_json::from_str(&s)?))
            }
        }
        EmbeddedValue::Simple(s) if !s.is_empty() => Ok(Some(serde_json::from_str(&s)?)),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_encode_basic() {
        assert_eq!(hex_encode(&[]), "");
        assert_eq!(hex_encode(&[0x00]), "00");
        assert_eq!(hex_encode(&[0xff]), "ff");
        assert_eq!(hex_encode(&[0x4a, 0x3b]), "4a3b");
    }

    #[test]
    fn compute_hash_deterministic_and_key_bound() {
        let h1 = compute_hash("secret", "tokenA").unwrap();
        let h2 = compute_hash("secret", "tokenA").unwrap();
        assert_eq!(h1, h2, "相同 key+token → 相同 hash");
        let h3 = compute_hash("secret", "tokenB").unwrap();
        assert_ne!(h1, h3, "不同 token → 不同 hash");
        let h4 = compute_hash("other", "tokenA").unwrap();
        assert_ne!(h1, h4, "不同 key → 不同 hash（绑 jwt_secret）");
        // hmac-sha256 输出 32 字节 = 64 hex 字符
        assert_eq!(h1.len(), 64);
    }
}
