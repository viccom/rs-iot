//! luaNorth（mqtt-north 角色薄壳）配置：`parse_north_cfg` 解析 `app_config`
//!（照 `lua_core/config.rs` 的 parse 模式——必填显式 Err、可选项缺省就地收敛）。
//!
//! 与 collector 的 `LuaCfg` 差异：**无传输项**（kind/endpoint/serial_*）——MQTT
//! 连接层在宿主（T7 连接序列，rumqttc），配置只描述 broker 与批量节奏（Q5
//! 混合批量：满 `batch_max_count` 条或 `batch_max_secs` 秒先到先触发）。

/// mqtt-north 实例配置（`app_config` 解析产物）。
///
/// 连接序列与回调契约（T7）已消费全部连接/批量字段；`secrets` 例外——register 期
/// `ctx:secret_get` 经 `app_config` 原文消费（T5 接线），本字段仅保留契约可见性。
use crate::plugin::lua_core as core_;

#[derive(Debug)]
pub(crate) struct NorthCfg {
    /// 脚本目录名（→ `scripts/<script>/main.lua`，与 collector 同校验规则）。
    pub(crate) script: String,
    /// broker 主机（连接序列消费；parse 校验非空）。
    pub(crate) host: String,
    /// broker 端口（默认 1883）。
    pub(crate) port: u16,
    /// 用户名（可选）。
    pub(crate) username: Option<String>,
    /// 密码（可选；支持 `secret:<ref>` 引用——连接序列经
    /// [`super::contract::resolve_password_opt`] 解析 lux secrets 表）。
    pub(crate) password: Option<String>,
    /// MQTT client id（缺省 `hwid-<inst_id>`——缺省值运行期才知道，见
    /// [`NorthCfg::client_id_or_default`]）。
    pub(crate) client_id: Option<String>,
    /// 批量上限条数（满 N 条触发 flush；Q5 混合批量，默认 50）。
    pub(crate) batch_max_count: usize,
    /// 批量上限秒数（T 秒先到先触发；缺省取 `period`）。
    pub(crate) batch_max_secs: u64,
    /// 收拢周期秒（默认 5）。
    pub(crate) period: u64,
    /// down() 订阅 topic 列表（默认空；ConnAck 后订阅）。
    pub(crate) subscribe: Vec<String>,
    /// secret 授权清单（R3；register 期 `ctx:secret_get` 消费的是 `app_config`
    /// 原文——本字段无运行时读者，保留契约可见性与 parse 校验）。
    #[allow(dead_code)]
    pub(crate) secrets: Vec<String>,
    /// 单回调时长预算毫秒（Q3/§4.5-1，照 lua_client 缺省 10s；auth/up/down 等回调消费）。
    pub(crate) callback_timeout_ms: u64,
    /// VM 内存限额 MB（沙箱参数，照 collector 缺省 32）。
    pub(crate) mem_limit_mb: usize,
    /// 指令 hook 阈值（沙箱参数，照 collector 缺省 1M）。
    pub(crate) hook_instr_limit: u32,
}

/// 解析 `app_config`（mqtt-north 键集）。`script`/`host` 必填；`port`/`period`/
/// `batch_*`/`callback_timeout_ms`/沙箱参数可配缺省；`batch_max_secs` 缺省取
/// `period`（Q5：批量节奏与收拢周期天然对齐）。
pub(crate) fn parse_north_cfg(app: &serde_json::Value) -> Result<NorthCfg, String> {
    let script = app
        .get("script")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "app_config.script missing（脚本目录名）".to_string())?
        .to_string();
    if !script
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        || script.is_empty()
    {
        return Err(format!(
            "非法 script 名 '{script}'（仅 [A-Za-z0-9_-]，防路径穿越）"
        ));
    }

    let host = app
        .get("host")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "app_config.host missing（broker 地址）".to_string())?
        .to_string();
    if host.trim().is_empty() {
        return Err("app_config.host 不得为空".into());
    }

    let port_v = app.get("port").and_then(|v| v.as_u64()).unwrap_or(1883);
    if port_v == 0 || port_v > u16::MAX as u64 {
        return Err(format!("非法 port {port_v}（1-65535）"));
    }

    let period = app.get("period").and_then(|v| v.as_u64()).unwrap_or(5);
    let batch_max_secs = app
        .get("batch_max_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(period);
    let batch_max_count = app
        .get("batch_max_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(50)
        .max(1) // 复审 P2-1：0 会使首个样本在空 Vec 上 remove(0) panic
        as usize;
    let callback_timeout_ms = app
        .get("callback_timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(core_::budget::DEFAULT_CALLBACK_TIMEOUT_MS)
        .max(1);
    let mem_limit_mb = app
        .get("mem_limit_mb")
        .and_then(|v| v.as_u64())
        .unwrap_or(core_::budget::DEFAULT_MEM_LIMIT_MB as u64) as usize;
    let hook_instr_limit =
        app.get("hook_instr_limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(core_::budget::DEFAULT_HOOK_INSTR_LIMIT as u64) as u32;

    // 字符串数组键（subscribe/secrets）：非数组 → Err（坏声明比静默缺省更危险，
    // 与 secret 授权清单语义对齐——漏授权即拒绝，配置错误必须可见）。
    let str_vec = |key: &str| -> Result<Vec<String>, String> {
        match app.get(key) {
            None => Ok(Vec::new()),
            // 复审 P3-4：元素级非字符串 → Err（此前 filter_map 静默丢——subscribe/
            // secrets 都是语义关键清单，坏元素必须可见；transform 同步收紧）。
            Some(serde_json::Value::Array(arr)) => {
                if let Some(bad) = arr.iter().find(|v| !v.is_string()) {
                    return Err(format!("app_config.{key} 含非字符串元素（实 {bad}）"));
                }
                Ok(arr
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect())
            }
            Some(other) => Err(format!(
                "app_config.{key} 类型不符（期望字符串数组，实 {other}）"
            )),
        }
    };

    Ok(NorthCfg {
        script,
        host,
        port: port_v as u16,
        username: app
            .get("username")
            .and_then(|v| v.as_str())
            .map(String::from),
        password: app
            .get("password")
            .and_then(|v| v.as_str())
            .map(String::from),
        client_id: app
            .get("client_id")
            .and_then(|v| v.as_str())
            .map(String::from),
        batch_max_count,
        batch_max_secs,
        period,
        subscribe: str_vec("subscribe")?,
        secrets: str_vec("secrets")?,
        callback_timeout_ms,
        mem_limit_mb,
        hook_instr_limit,
    })
}

impl NorthCfg {
    /// 有效 client id：显式配置优先，缺省 `hwid-<inst_id>`（缺省值含运行期 hwid，
    /// parse 时不知道——方法形态在注入 cfg 表/连接序列处物化）。
    pub(crate) fn client_id_or_default(&self, hwid: &str, inst_id: &str) -> String {
        self.client_id
            .clone()
            .unwrap_or_else(|| format!("{hwid}-{inst_id}"))
    }

    /// cfg 全局表数据（globals.cfg / `auth(cfg)` 参数，Q5「宿主另注入全局 cfg
    /// 表」）：脚本业务相关的有效配置子集（client_id 缺省已物化；沙箱参数/
    /// 授权清单等宿主内部项不暴露）。password 保持原始形态——`secret:` 引用
    /// 在连接前一刻经 contract::resolve_password_opt 解析（解析后明文不长期驻留
    /// VM globals）。`hwid`（T8）供 mqttPub 兼容脚本组 `<hwid>/<devid>/datas`
    /// topic 与 payload。
    pub(crate) fn to_script_cfg(&self, hwid: &str, inst_id: &str) -> serde_json::Value {
        serde_json::json!({
            "hwid": hwid,
            "script": self.script,
            "host": self.host,
            "port": self.port,
            "username": self.username,
            "password": self.password,
            "client_id": self.client_id_or_default(hwid, inst_id),
            "batch_max_count": self.batch_max_count,
            "batch_max_secs": self.batch_max_secs,
            "period": self.period,
            "subscribe": self.subscribe,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_defaults() {
        let cfg = parse_north_cfg(&json!({"script":"cloud1","host":"127.0.0.1"})).unwrap();
        assert_eq!(cfg.script, "cloud1");
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.port, 1883, "port 缺省 1883");
        assert_eq!(cfg.period, 5, "period 缺省 5");
        assert_eq!(cfg.batch_max_secs, 5, "batch_max_secs 缺省取 period");
        assert_eq!(cfg.batch_max_count, 50, "batch_max_count 缺省 50");
        assert!(cfg.subscribe.is_empty());
        assert!(cfg.secrets.is_empty());
        assert_eq!(cfg.callback_timeout_ms, 10_000, "照 lua_client 缺省 10s");
        assert_eq!(cfg.mem_limit_mb, 32);
        assert_eq!(cfg.hook_instr_limit, 1_000_000);
        assert!(cfg.username.is_none() && cfg.password.is_none() && cfg.client_id.is_none());
    }

    #[test]
    fn parse_overrides() {
        let cfg = parse_north_cfg(&json!({
            "script":"cloud1","host":"b.example.com","port":8883,
            "username":"u","password":"p","client_id":"cid",
            "period":2,"batch_max_secs":7,"batch_max_count":100,
            "subscribe":["t/1","t/2"],"secrets":["secret:k1"],
            "callback_timeout_ms":500
        }))
        .unwrap();
        assert_eq!(cfg.port, 8883);
        assert_eq!(cfg.username.as_deref(), Some("u"));
        assert_eq!(cfg.password.as_deref(), Some("p"));
        assert_eq!(cfg.client_id.as_deref(), Some("cid"));
        assert_eq!(cfg.period, 2);
        assert_eq!(cfg.batch_max_secs, 7, "显式覆盖不再取 period");
        assert_eq!(cfg.batch_max_count, 100);
        assert_eq!(cfg.subscribe, vec!["t/1", "t/2"]);
        assert_eq!(cfg.secrets, vec!["secret:k1"]);
        assert_eq!(cfg.callback_timeout_ms, 500);
    }

    #[test]
    fn parse_missing_required_errors() {
        assert!(parse_north_cfg(&json!({"host":"h"})).is_err(), "缺 script");
        assert!(parse_north_cfg(&json!({"script":"s"})).is_err(), "缺 host");
        assert!(
            parse_north_cfg(&json!({"script":"s","host":"  "})).is_err(),
            "host 空白串无效"
        );
    }

    /// 防路径穿越（与 collector 同规则）+ 非法 port。
    #[test]
    fn parse_rejects_bad_script_and_port() {
        assert!(parse_north_cfg(&json!({"script":"../etc","host":"h"})).is_err());
        assert!(parse_north_cfg(&json!({"script":"","host":"h"})).is_err());
        assert!(parse_north_cfg(&json!({"script":"s","host":"h","port":0})).is_err());
        assert!(parse_north_cfg(&json!({"script":"s","host":"h","port":65536})).is_err());
    }

    /// 字符串数组键类型不符 → Err（坏声明可见，不静默降级空清单）。
    #[test]
    fn parse_rejects_bad_str_vec_type() {
        assert!(parse_north_cfg(&json!({"script":"s","host":"h","subscribe":"t/1"})).is_err());
        assert!(parse_north_cfg(&json!({"script":"s","host":"h","secrets":42})).is_err());
    }

    /// client_id 缺省 `hwid-<inst_id>`，显式配置优先。
    #[test]
    fn client_id_default_materialized() {
        let cfg = parse_north_cfg(&json!({"script":"s","host":"h"})).unwrap();
        assert_eq!(
            cfg.client_id_or_default("hw-1", "north-1"),
            "hw-1-north-1",
            "缺省 client_id = hwid-<inst_id>"
        );
        let cfg = parse_north_cfg(&json!({"script":"s","host":"h","client_id":"fixed"})).unwrap();
        assert_eq!(cfg.client_id_or_default("hw-1", "north-1"), "fixed");
    }

    /// cfg 全局表数据：有效子集 + client_id 物化 + password 原始形态（连接前才解析）。
    #[test]
    fn to_script_cfg_shape() {
        let cfg = parse_north_cfg(&json!({
            "script":"cloud1","host":"127.0.0.1","password":"secret:k1","period":1
        }))
        .unwrap();
        let v = cfg.to_script_cfg("hw-9", "north-9");
        assert_eq!(v["hwid"], "hw-9", "T8：兼容脚本组 topic/payload 需 hwid");
        assert_eq!(v["host"], "127.0.0.1");
        assert_eq!(v["port"], 1883);
        assert_eq!(v["client_id"], "hw-9-north-9", "缺省已物化");
        assert_eq!(v["password"], "secret:k1", "引用原样保留（连接前解析）");
        assert_eq!(v["batch_max_count"], 50);
        assert_eq!(v["batch_max_secs"], 1);
        assert!(
            v.get("secrets").is_none() && v.get("callback_timeout_ms").is_none(),
            "宿主内部项（授权清单/沙箱参数）不进脚本 cfg"
        );
    }
    /// 复审 P1-1/P2-1：batch_max_count 解析下限——0（panic 诱因）被抬到 1；
    /// 缺省 50；显式合法值原样。满批即发的行为断言在 contract.rs e2e
    /// （batch 缓冲驱动路径），此处锁配置面。
    #[test]
    fn batch_max_count_floor() {
        let c = parse_north_cfg(&json!({"script":"t","host":"h"})).unwrap();
        assert_eq!(c.batch_max_count, 50, "缺省 50");
        let c = parse_north_cfg(&json!({"script":"t","host":"h","batch_max_count":0})).unwrap();
        assert_eq!(c.batch_max_count, 1, "0 抬到 1（空 Vec remove panic 预防）");
        let c = parse_north_cfg(&json!({"script":"t","host":"h","batch_max_count":7})).unwrap();
        assert_eq!(c.batch_max_count, 7, "合法值原样");
    }

    /// 复审 P3-4：字符串数组键（subscribe/secrets）含非字符串元素 → Err（此前静默丢）。
    #[test]
    fn parse_str_vec_non_string_element_errors() {
        let r = parse_north_cfg(&serde_json::json!({
            "script": "t", "host": "h", "port": 1883,
            "subscribe": ["a/topic", true]
        }));
        assert!(r.is_err(), "非字符串元素应拒: {r:?}");
        assert!(r.unwrap_err().contains("非字符串元素"));
    }
}
