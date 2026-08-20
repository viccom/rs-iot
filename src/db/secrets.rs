//! 平台凭证本地存储（阶段 B 设计 §3.7 Q4/D6）：lux Hash `secrets`。
//!
//! mqtt-north 等平台凭证（OneNET access_key 等）不进脚本源码与 manifest
//! （D6：密钥永不进脚本源码与日志），经 REST 管理面（`routes_secret`）写入本表；
//! 脚本侧 `ctx:secret_get`（`api::register`）按实例授权清单读取。
//!
//! - field = ref：强制 `secret:` 前缀 + `[a-z0-9_-]`（D6 受限前缀——前缀强制
//!   防脚本未来读到任意 lux key；ref 非机密可入审计日志，value 绝不入日志/回显）；
//! - value = 凭证明文（纯字符串不 JSON 包装——lux 是本机嵌入式存储，与 luakv
//!   同信任域）。

use crate::error::RsIotError;
use lux::{EmbeddedClient, EmbeddedValue};

/// lux hash 名（全局单表）。
const HASH: &str = "secrets";

/// ref 强制前缀（D6 受限前缀）。
pub const REF_PREFIX: &str = "secret:";

/// 校验 ref 合法：`secret:` 前缀 + 前缀后非空且仅 `[a-z0-9_-]`（小写）。
///
/// ref 非机密，错误消息可带 ref 本身（可行动）；value 绝不出现在任何错误/日志。
pub fn validate_ref(r: &str) -> Result<(), RsIotError> {
    let Some(name) = r.strip_prefix(REF_PREFIX) else {
        return Err(RsIotError::Config(format!(
            "secret ref 必须以 '{REF_PREFIX}' 前缀开头（D6 受限前缀）：'{r}'"
        )));
    };
    if name.is_empty() {
        return Err(RsIotError::Config(format!(
            "secret ref 前缀 '{REF_PREFIX}' 后不能为空：'{r}'"
        )));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        return Err(RsIotError::Config(format!(
            "secret ref 仅允许 [a-z0-9_-]（小写）：'{r}'"
        )));
    }
    Ok(())
}

/// 掩码 value（REST list 回显用；D6 绝不回显明文）：
/// 字符数 >8 → 前 4 + `****` + 后 4；≤8 → 全 `****`。
///
/// 按字符（非字节）截取，多字节字符不产生非法 UTF-8 边界。
pub fn mask_value(v: &str) -> String {
    const MASK: &str = "****";
    let n = v.chars().count();
    if n <= 8 {
        return MASK.to_string();
    }
    let head: String = v.chars().take(4).collect();
    let tail: String = v.chars().skip(n - 4).collect();
    format!("{head}{MASK}{tail}")
}

/// 写入一个 ref（覆盖旧值）。ref 非法返回 Err（拒绝的不落库）。
pub(crate) async fn set(c: &EmbeddedClient, r: &str, value: &str) -> Result<(), RsIotError> {
    validate_ref(r)?;
    c.execute("HSET", &[HASH, r, value]).await?;
    Ok(())
}

/// 读一个 ref 的明文（脚本侧 `ctx:secret_get` 经 `api::register` 消费，T5 接线；
/// 无则 `None`——在授权清单但未写入时脚本据此判空给默认）。
pub(crate) async fn get(c: &EmbeddedClient, r: &str) -> Result<Option<String>, RsIotError> {
    validate_ref(r)?;
    let v = c.execute_value("HGET", &[HASH, r]).await?;
    Ok(embedded_to_string(v))
}

/// 列全部 `(ref, value)` 对（REST list 掩码消费；排序在展示层做）。
pub(crate) async fn list(c: &EmbeddedClient) -> Result<Vec<(String, String)>, RsIotError> {
    let v = c.execute_value("HGETALL", &[HASH]).await?;
    let mut out = Vec::new();
    if let EmbeddedValue::Array(arr) = v {
        // RESP 扁平序列：k1, v1, k2, v2, ...（同 api::hgetall 解析形态）。
        let mut i = 0;
        while i + 1 < arr.len() {
            let k = embedded_to_string(arr[i].clone()).unwrap_or_default();
            let val = embedded_to_string(arr[i + 1].clone()).unwrap_or_default();
            if !k.is_empty() {
                out.push((k, val));
            }
            i += 2;
        }
    }
    Ok(out)
}

/// 删除一个 ref（幂等：不存在仍 `Ok`——HDEL 对缺失 field 天然无错）。
pub(crate) async fn del(c: &EmbeddedClient, r: &str) -> Result<(), RsIotError> {
    validate_ref(r)?;
    c.execute("HDEL", &[HASH, r]).await?;
    Ok(())
}

/// HGET/HGETALL 值 → 字符串（Nil/空 → `None`，语义同 `luakv::get`）。
fn embedded_to_string(v: EmbeddedValue) -> Option<String> {
    match v {
        EmbeddedValue::Bulk(b) => {
            let s = String::from_utf8_lossy(&b).into_owned();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        }
        EmbeddedValue::Simple(s) if !s.is_empty() => Some(s),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 唯一临时 data_dir（tag 区分用例，防并行测试互扰；照 luakv::tests 模式）。
    async fn test_client(tag: &str) -> lux::EmbeddedClient {
        let cfg = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: std::env::temp_dir()
                    .join(format!("secrets-{tag}-{}", std::process::id()))
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

    // ---- 掩码规则（D6：list 回显的唯一形态）----

    #[test]
    fn mask_long_value_keeps_head_and_tail_4() {
        assert_eq!(mask_value("abcd12345678wxyz"), "abcd****wxyz");
    }

    #[test]
    fn mask_short_value_fully_masked() {
        assert_eq!(mask_value("short"), "****");
        assert_eq!(mask_value(""), "****");
    }

    /// 边界：≤8 全掩码；>8 前后各 4（9 字符是「>8」最小情形）。
    #[test]
    fn mask_boundary_at_8_and_9_chars() {
        assert_eq!(mask_value("12345678"), "****");
        assert_eq!(mask_value("123456789"), "1234****6789");
    }

    /// 多字节字符按字符截取，不产生非法 UTF-8（panic 即失败）。
    #[test]
    fn mask_multibyte_by_chars() {
        let masked = mask_value("密钥凭证明文八个字以上");
        assert!(masked.starts_with("密钥凭") && masked.ends_with("字以上"));
        assert!(masked.contains("****"));
    }

    // ---- ref 校验（D6 受限前缀；错误消息可行动）----

    #[test]
    fn ref_accepts_prefixed_lowercase() {
        assert!(validate_ref("secret:onenet_key").is_ok());
        assert!(validate_ref("secret:a-b_2").is_ok());
    }

    #[test]
    fn ref_rejects_missing_prefix() {
        for bad in ["", "onenet_key", "SECRET:onenet_key", "mysecret:key"] {
            assert!(
                validate_ref(bad).is_err(),
                "无（或错误的）secret: 前缀应拒：{bad:?}"
            );
        }
    }

    #[test]
    fn ref_rejects_empty_after_prefix() {
        assert!(validate_ref("secret:").is_err(), "前缀后为空应拒");
    }

    /// 非法字符：大写、符号、空格、路径分隔——错误消息可行动（含 ref 与合法字符集）。
    #[test]
    fn ref_rejects_bad_charset_with_actionable_message() {
        for bad in [
            "secret:OneNet",
            "secret:a.b",
            "secret:a/b",
            "secret:a b",
            "secret:a:b",
        ] {
            let e = validate_ref(bad).unwrap_err().to_string();
            assert!(
                e.contains("a-z0-9_-"),
                "错误消息应给出合法字符集（{bad:?}）：{e}"
            );
        }
    }

    // ---- lux roundtrip（真实 EmbeddedClient）----

    #[tokio::test]
    async fn set_get_list_del_roundtrip() {
        let c = test_client("roundtrip").await;
        set(&c, "secret:onenet_key", "k1-plain").await.unwrap();
        set(&c, "secret:mqtt_pwd", "k2-plain").await.unwrap();

        assert_eq!(
            get(&c, "secret:onenet_key").await.unwrap().as_deref(),
            Some("k1-plain")
        );
        assert_eq!(get(&c, "secret:missing").await.unwrap(), None);

        let mut all = list(&c).await.unwrap();
        all.sort();
        assert_eq!(
            all,
            vec![
                ("secret:mqtt_pwd".to_string(), "k2-plain".to_string()),
                ("secret:onenet_key".to_string(), "k1-plain".to_string()),
            ]
        );

        del(&c, "secret:onenet_key").await.unwrap();
        assert_eq!(get(&c, "secret:onenet_key").await.unwrap(), None);
        // 幂等：删不存在的 ref 仍 Ok
        del(&c, "secret:onenet_key").await.unwrap();
    }

    /// 覆盖写：同 ref 二次 set 取新值。
    #[tokio::test]
    async fn set_overwrites_existing_ref() {
        let c = test_client("overwrite").await;
        set(&c, "secret:token", "OLD").await.unwrap();
        set(&c, "secret:token", "NEW").await.unwrap();
        assert_eq!(
            get(&c, "secret:token").await.unwrap().as_deref(),
            Some("NEW")
        );
    }

    /// 非法 ref 在存储层同门被拒，且拒绝的不落库。
    #[tokio::test]
    async fn set_rejects_invalid_ref_and_stores_nothing() {
        let c = test_client("reject").await;
        assert!(set(&c, "onenet_key", "v").await.is_err(), "无前缀应拒");
        assert!(set(&c, "secret:BAD", "v").await.is_err(), "大写应拒");
        assert!(list(&c).await.unwrap().is_empty(), "被拒的写入不应落库");
    }
}
