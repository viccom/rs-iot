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
//!
//! D-SEC-1：value AEAD 加密存储（设计 §6）——算法 chacha20poly1305，密钥 =
//! HKDF-SHA256(jwt_secret, salt="rs-iot-secrets-v1", info="lux-secrets") 派生
//! 32B。存储格式 `enc:v1:<nonce_b64url>:<ct_b64url>`：RESP 直读 HGETALL 只见
//! 密文；进程内经本模块透明解密。无前缀 = 旧明文，读兼容（懒迁移：读明文不
//! 回写，下次 set 才加密）。

use crate::error::RsIotError;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chacha20poly1305::aead::{Aead, Generate, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use lux::{EmbeddedClient, EmbeddedValue};
use sha2::Sha256;

/// lux hash 名（全局单表）。
const HASH: &str = "secrets";

/// ref 强制前缀（D6 受限前缀）。
pub const REF_PREFIX: &str = "secret:";

/// 密文存储前缀（D-SEC-1）：`enc:v1:<nonce_b64url>:<ct_b64url>`；无前缀 = 旧明文。
const ENC_PREFIX: &str = "enc:v1:";

/// HKDF 域分离参数（设计 §6 定案，勿改——改即全员密文不可解）。
const HKDF_SALT: &[u8] = b"rs-iot-secrets-v1";
const HKDF_INFO: &[u8] = b"lux-secrets";

/// 进程级派生密钥（计划 Task 6 定案 `OnceLock<ChaChaKey>`——jwt_secret 进程
/// 恒定，run_app 启动 init 一次；全进程（REST/脚本/北向）共享同一键）。
static KEY: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();

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

/// 掩码 value（REST list 回显用；D6 绝不回显明文）：只显**尾 4 字符** + `****` 前缀
/// （≤4 全掩码）。复审 M1：旧规则（首4+尾4）对 9 字符值泄漏 8/9——数字 PIN 场景
/// 暴力空间从 10^n 缩到 10，形同虚设；尾 4 供人工核对足够，泄漏面恒为 4 字符。
///
/// 按字符（非字节）截取，多字节字符不产生非法 UTF-8 边界。
pub fn mask_value(v: &str) -> String {
    const MASK: &str = "****";
    let n = v.chars().count();
    if n <= 4 {
        return MASK.to_string();
    }
    let tail: String = v.chars().skip(n - 4).collect();
    format!("{MASK}{tail}")
}

/// 初始化进程级加密密钥（run_app 启动时调用一次，先于任何 secrets 读写）。
/// 幂等：同派生键静默；不同派生键 warn 并保持首值（jwt_secret 进程恒定，多值
/// init 属配置错乱——大声可见，不静默换键导致旧密文不可解）。
pub fn init_key(jwt_secret: &str) {
    let k = derive_key(jwt_secret);
    if let Err(first) = KEY.set(k) {
        if first != k {
            tracing::warn!("secrets 密钥重复初始化且派生值不同——保持首值（jwt_secret 应进程恒定）");
        }
    }
}

/// 取进程级密钥；未初始化（测试二进制/库直嵌未走 run_app）回退空串派生键——
/// 加解密同键自洽（往返不坏），强度同默认配置 jwt_secret（公开常量），warn 可见。
fn key() -> [u8; 32] {
    KEY.get().copied().unwrap_or_else(|| {
        tracing::warn!("secrets 加密密钥未初始化（非 run_app 路径）——回退空串派生键");
        derive_key("")
    })
}

/// HKDF-SHA256(jwt_secret) → 32B ChaCha20Poly1305 密钥（salt/info 见常量，设计 §6）。
fn derive_key(jwt_secret: &str) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(Some(HKDF_SALT), jwt_secret.as_bytes());
    let mut okm = [0u8; 32];
    hk.expand(HKDF_INFO, &mut okm)
        .expect("HKDF-SHA256 32B OKM 恒可扩展");
    okm
}

/// 明文 → `enc:v1:<nonce_b64url>:<ct_b64url>`。nonce 随机（同值两次 set 密文
/// 不同）；b64 用 URL_SAFE_NO_PAD（不含 `:`，与格式分隔符无歧义）。
fn encrypt_value(k: &[u8; 32], plain: &str) -> String {
    let cipher = ChaCha20Poly1305::new(&Key::from(*k));
    let nonce = Nonce::generate();
    // encrypt 仅在明文超长时报错（2^32 量级）——secret 是短字符串，不可达。
    let ct = cipher
        .encrypt(&nonce, plain.as_bytes())
        .expect("chacha20poly1305 加密");
    format!(
        "{ENC_PREFIX}{}:{}",
        URL_SAFE_NO_PAD.encode(nonce.as_slice()),
        URL_SAFE_NO_PAD.encode(&ct)
    )
}

/// 存储值 → 明文：无 `enc:v1:` 前缀 = 旧明文原样（懒迁移读兼容）；有前缀 =
/// 解密，失败（篡改/损坏/jwt_secret 已变）warn + None——密文对调用方无用，
/// 按「值不存在」语义处理（D6：日志绝不含 value/密文，只报事实）。
fn maybe_decrypt(k: &[u8; 32], stored: &str) -> Option<String> {
    let Some(rest) = stored.strip_prefix(ENC_PREFIX) else {
        return Some(stored.to_string());
    };
    let decrypted = (|| {
        let (n_b64, c_b64) = rest.split_once(':')?;
        let nonce: [u8; 12] = URL_SAFE_NO_PAD.decode(n_b64).ok()?.try_into().ok()?;
        let ct = URL_SAFE_NO_PAD.decode(c_b64).ok()?;
        ChaCha20Poly1305::new(&Key::from(*k))
            .decrypt(&Nonce::from(nonce), ct.as_slice())
            .ok()
            .map(|pt| String::from_utf8_lossy(&pt).into_owned())
    })();
    if decrypted.is_none() {
        tracing::warn!("secrets 密文解密失败（篡改/损坏或 jwt_secret 已变）——按值不存在处理");
    }
    decrypted
}

/// 写入一个 ref（覆盖旧值）。ref 非法返回 Err（拒绝的不落库）。
/// D-SEC-1：一律写密文（旧明文在下次 set 覆盖时即完成迁移）。空 value 不加密
/// ——保持「存储空 = 读 None」原语义（加密空串会读回 Some("")，语义漂移）。
pub(crate) async fn set(c: &EmbeddedClient, r: &str, value: &str) -> Result<(), RsIotError> {
    validate_ref(r)?;
    let stored = if value.is_empty() {
        String::new()
    } else {
        let k = key();
        encrypt_value(&k, value)
    };
    c.execute("HSET", &[HASH, r, &stored]).await?;
    Ok(())
}

/// 读一个 ref 的明文（脚本侧 `ctx:secret_get` 经 `api::register` 消费，T5 接线；
/// 无则 `None`——在授权清单但未写入时脚本据此判空给默认）。
/// D-SEC-1：透明解密（密文失败按不存在；旧明文原样）。
pub(crate) async fn get(c: &EmbeddedClient, r: &str) -> Result<Option<String>, RsIotError> {
    validate_ref(r)?;
    let v = c.execute_value("HGET", &[HASH, r]).await?;
    Ok(embedded_to_string(v).and_then(|s| {
        let k = key();
        maybe_decrypt(&k, &s)
    }))
}

/// 列全部 `(ref, 明文)` 对（REST list 掩码基于**解密值**算；排序在展示层做）。
/// 解密失败的值降级为空串（掩码层全掩可见，不阻塞整表列举）。
pub(crate) async fn list(c: &EmbeddedClient) -> Result<Vec<(String, String)>, RsIotError> {
    let v = c.execute_value("HGETALL", &[HASH]).await?;
    let k = key();
    let mut out = Vec::new();
    if let EmbeddedValue::Array(arr) = v {
        // RESP 扁平序列：k1, v1, k2, v2, ...（同 api::hgetall 解析形态）。
        let mut i = 0;
        while i + 1 < arr.len() {
            let key_s = embedded_to_string(arr[i].clone()).unwrap_or_default();
            let val = embedded_to_string(arr[i + 1].clone())
                .and_then(|s| maybe_decrypt(&k, &s))
                .unwrap_or_default();
            if !key_s.is_empty() {
                out.push((key_s, val));
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

/// 解析宿主侧连接凭证（`secret:<ref>` → 明文；字面值原样；None → None）。
///
/// 供**宿主内部**消费（luaNorth 的 broker password、sidecarClient 下发给外部插件的
/// 连接参数）：与脚本侧 `ctx:secret_get` 的授权清单是两条独立通路——清单管脚本
/// 读什么，本函数管宿主连接用什么，不受 `app_config.secrets` 约束。
///
/// 语义（调用方须按此处理）：
/// - `None` → `Ok(None)`：调用方自行决定无凭证是否合法（north 按认证失败处理，
///   **不静默无密码连接**）；
/// - 字面值 → `Ok(Some(字面值))`；
/// - `secret:<ref>` 且已写入 → `Ok(Some(明文))`；
/// - `secret:<ref>` 但 lux 无此 ref → `Ok(None)`（配置了但没写值）；
/// - ref 非法 / lux 读失败 → `Err`。
///
/// 明文只在调用方需要建立连接的生命周期内存在，不落日志。
pub async fn resolve_ref(pw: Option<&str>, lux: &EmbeddedClient) -> Result<Option<String>, String> {
    let Some(p) = pw else {
        return Ok(None);
    };
    let Some(r) = p.strip_prefix(REF_PREFIX) else {
        return Ok(Some(p.to_string()));
    };
    let full = format!("{REF_PREFIX}{r}");
    validate_ref(&full).map_err(|e| e.to_string())?;
    get(lux, &full)
        .await
        .map_err(|e| format!("password secret 读取失败：{e}"))
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
    fn mask_long_value_shows_tail_4_only() {
        // 复审 M1：只显尾 4（旧规则首4+尾4 对 9 字符值泄漏 8/9）
        assert_eq!(mask_value("abcd12345678wxyz"), "****wxyz");
    }

    #[test]
    fn mask_short_value_fully_masked() {
        assert_eq!(mask_value("short"), "****hort"); // 5 字符显尾 4（>4 即显）
        assert_eq!(mask_value(""), "****");
    }

    /// 边界：≤4 全掩码；>4 只显尾 4（5 字符是「>4」最小情形——泄漏面恒为 4）。
    #[test]
    fn mask_boundary_at_4_and_5_chars() {
        assert_eq!(mask_value("1234"), "****");
        assert_eq!(mask_value("12345"), "****2345");
        // 数字 PIN 场景（复审 PoC）：9 位数字旧规则泄漏 8/9，新规则恒 4
        assert_eq!(mask_value("123456789"), "****6789");
    }

    /// 多字节字符按字符截取，不产生非法 UTF-8（panic 即失败）。
    #[test]
    fn mask_multibyte_by_chars() {
        let masked = mask_value("密钥凭证明文八个字以上");
        assert!(masked.starts_with("****") && masked.ends_with("字以上"));
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

    // ---- D-SEC-1：值 AEAD 加密存储（chacha20poly1305 + HKDF(jwt_secret)）----

    /// 测试侧直读原始存储值（RESP 视角，绕过 db::secrets 解密语义）。
    async fn hget_raw(c: &lux::EmbeddedClient, r: &str) -> String {
        let v = c.execute_value("HGET", &[HASH, r]).await.unwrap();
        embedded_to_string(v).unwrap_or_default()
    }

    /// 往返：set 后 RESP 直读只见密文（`enc:v1:` 前缀且不含明文）；get 返明文。
    #[tokio::test]
    async fn set_stores_ciphertext_get_decrypts() {
        let c = test_client("aead-roundtrip").await;
        set(&c, "secret:onenet_key", "plain-secret-value")
            .await
            .unwrap();

        let raw = hget_raw(&c, "secret:onenet_key").await;
        assert!(
            raw.starts_with("enc:v1:"),
            "RESP 直读应只见密文（enc:v1: 前缀），实得：{raw:?}"
        );
        assert!(!raw.contains("plain-secret-value"), "密文不得含明文");

        assert_eq!(
            get(&c, "secret:onenet_key").await.unwrap().as_deref(),
            Some("plain-secret-value"),
            "进程内 get 应透明解密"
        );
    }

    /// 旧明文兼容（懒迁移）：直接 HSET 明文 → get 原样返回；且不回写（原始值仍明文，
    /// 下次 set 才加密）。
    #[tokio::test]
    async fn legacy_plaintext_readable_and_not_rewritten() {
        let c = test_client("aead-legacy").await;
        c.execute("HSET", &[HASH, "secret:old_key", "legacy-plain"])
            .await
            .unwrap();

        assert_eq!(
            get(&c, "secret:old_key").await.unwrap().as_deref(),
            Some("legacy-plain"),
            "旧明文值应原样可读（读兼容）"
        );
        assert_eq!(
            hget_raw(&c, "secret:old_key").await,
            "legacy-plain",
            "懒迁移：读明文不回写"
        );
    }

    /// 篡改密文 / 格式损坏 → 解密失败按「值不存在」（None）处理，不 panic
    ///（密文对调用方无用；损坏数据等同丢失，warn 可见）。
    #[tokio::test]
    async fn tampered_or_malformed_ciphertext_reads_as_missing() {
        let c = test_client("aead-tamper").await;
        set(&c, "secret:k", "v-tamper-target").await.unwrap();
        let raw = hget_raw(&c, "secret:k").await;
        // enc:v1:<nonce_b64>:<ct_b64>——翻转 ct 段首字符（b64 字符集内仍合法，
        // 解密走到 AEAD 认证失败而非格式解析失败）。
        let mut chars: Vec<char> = raw.chars().collect();
        let last = chars.len() - 1;
        chars[last] = if chars[last] == 'A' { 'B' } else { 'A' };
        let tampered: String = chars.into_iter().collect();
        c.execute("HSET", &[HASH, "secret:k", &tampered])
            .await
            .unwrap();
        assert_eq!(
            get(&c, "secret:k").await.unwrap(),
            None,
            "篡改 ct → AEAD 认证失败 → None"
        );

        // 非 b64 字符的损坏段同样按不存在处理。
        c.execute("HSET", &[HASH, "secret:k", "enc:v1:!!!not-b64!!!:####"])
            .await
            .unwrap();
        assert_eq!(get(&c, "secret:k").await.unwrap(), None, "格式损坏 → None");
    }

    /// 随机 nonce：同值两次 set 密文不同（nonce 唯一），但都解密回同一明文。
    #[tokio::test]
    async fn same_value_two_sets_yield_unique_ciphertext() {
        let c = test_client("aead-nonce").await;
        set(&c, "secret:n", "same-value").await.unwrap();
        let raw1 = hget_raw(&c, "secret:n").await;
        set(&c, "secret:n", "same-value").await.unwrap();
        let raw2 = hget_raw(&c, "secret:n").await;
        assert_ne!(raw1, raw2, "随机 nonce：同值两次 set 密文应不同");
        assert_eq!(
            get(&c, "secret:n").await.unwrap().as_deref(),
            Some("same-value"),
            "两份密文都应解密回同一明文"
        );
    }

    /// list 基于**解密值**（掩码消费方 list_masked 拿明文算掩码；存储层只密文）。
    #[tokio::test]
    async fn list_yields_plaintext_pairs_for_masking() {
        let c = test_client("aead-list").await;
        set(&c, "secret:b_key", "plain-b").await.unwrap();
        set(&c, "secret:a_key", "plain-a").await.unwrap();

        let mut all = list(&c).await.unwrap();
        all.sort();
        assert_eq!(
            all,
            vec![
                ("secret:a_key".to_string(), "plain-a".to_string()),
                ("secret:b_key".to_string(), "plain-b".to_string()),
            ],
            "list 应返回解密后的明文对"
        );

        // RESP 直读 HGETALL 同样只见密文。
        let v = c.execute_value("HGETALL", &[HASH]).await.unwrap();
        if let EmbeddedValue::Array(arr) = v {
            for item in arr {
                let s = embedded_to_string(item).unwrap_or_default();
                assert!(
                    !s.contains("plain-a") && !s.contains("plain-b"),
                    "HGETALL 原始值不得含明文"
                );
            }
        }
    }

    /// 空 value 语义保持：set 空 → 读 None（未加密存储层等价「不存在」）。
    #[tokio::test]
    async fn empty_value_still_reads_missing() {
        let c = test_client("aead-empty").await;
        set(&c, "secret:empty", "").await.unwrap();
        assert_eq!(get(&c, "secret:empty").await.unwrap(), None);
    }

    /// 纯函数：派生键确定性 + 绑 jwt_secret（不同 secret → 不同键）。
    #[test]
    fn derive_key_deterministic_and_secret_bound() {
        assert_eq!(derive_key("jwt-a"), derive_key("jwt-a"));
        assert_ne!(derive_key("jwt-a"), derive_key("jwt-b"));
        assert!(derive_key("").iter().any(|&b| b != 0), "空串也派生非平凡键");
    }

    /// 纯函数：异键解密 → None（jwt_secret 变更后旧密文按「值不存在」处理）。
    #[test]
    fn wrong_key_decrypts_to_none() {
        let k1 = derive_key("jwt-old");
        let k2 = derive_key("jwt-new");
        let stored = encrypt_value(&k1, "plain-x");
        assert_eq!(maybe_decrypt(&k1, &stored).as_deref(), Some("plain-x"));
        assert_eq!(
            maybe_decrypt(&k2, &stored),
            None,
            "异键 → AEAD 认证失败 → None"
        );
    }
}
