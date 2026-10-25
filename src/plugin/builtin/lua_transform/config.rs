//! luaTransform（transform 角色薄壳）配置：`parse_transform_cfg` 解析 `app_config`
//!（照 `lua_north/config.rs` 的 parse 模式——必填显式 Err、可选项缺省就地收敛）。
//!
//! 与 collector 的 `LuaCfg` / north 的 `NorthCfg` 差异：**无传输项**——transform 是
//! EventBus 订阅型实例（Q6），输入面是 `subscribe` 清单（宿主侧模式过滤），不是
//! socket/串口；`subscribe` 空 = 纯定时（on_timer 周期加工，不订 EventBus）。
//!
//! 订阅模式语法（Q6 宿主过滤）：
//! - `"dev1"`：dev1 的全部 tag；
//! - `"dev2.temp*"`：dev2 + tag glob（`*` 匹配任意序列，无 `?`）；
//! - `"*"`：全部设备（dev 段整段通配）。
//!
//! 非法 pattern（空串 / 含 `..` / 超 2 段 / 空段 / dev 段部分通配如 `"dev*"`）→
//! 解析报错——坏声明比静默缺省更危险（订阅写错 = 数据面静默丢失，必须可见）。

use crate::plugin::lua_core as core_;
use crate::plugin::lua_core::loader::SCRIPTS_ROOT;

/// transform 实例配置（`app_config` 解析产物）。
///
/// 回调契约（on_timer/on_rtdata any-of）与能力包（Role::Transform）见
/// manifest/api 层；本结构只承载壳层节奏与订阅面。
#[derive(Debug)]
pub(crate) struct TransformCfg {
    /// 脚本目录名（→ `scripts/<script>/main.lua`，与 collector 同校验规则）。
    pub(crate) script: String,
    /// on_timer 周期秒（默认 3，对齐 lua_client；0 = 纯订阅，不调 on_timer）。
    pub(crate) period: u64,
    /// 订阅模式清单（Q6 宿主过滤）：空 = 纯定时（不订 EventBus）。
    pub(crate) subscribe: Vec<String>,
    /// 脚本根目录（缺省 `scripts`，per-instance 可覆盖——照 lua_north 解析模式）。
    pub(crate) scripts_root: String,
    /// 单回调时长预算毫秒（Q3/§4.5-1，照 lua_core config 缺省 10s；on_init/
    /// on_timer/on_rtdata 每次调用消费）。
    pub(crate) callback_timeout_ms: u64,
    /// 连续失败降级阈值（Q3「连续超限走 fail_threshold → degraded」，照 collector
    /// 可配缺省 10——复审 P2-2：on_rtdata 批级记账消费）。
    pub(crate) fail_threshold: u32,
    /// VM 内存限额 MB（沙箱参数，照 collector/north 缺省 32）。
    pub(crate) mem_limit_mb: usize,
    /// 指令 hook 阈值（沙箱参数，照 collector/north 缺省 1M）。
    pub(crate) hook_instr_limit: u32,
}

/// 解析 `app_config`（transform 键集）。`script` 必填；`period`/`callback_timeout_ms`/
/// 沙箱参数可配缺省；`subscribe` 逐条模式校验（非法 pattern → Err）。
pub(crate) fn parse_transform_cfg(app: &serde_json::Value) -> Result<TransformCfg, String> {
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

    let period = app.get("period").and_then(|v| v.as_u64()).unwrap_or(3);
    // Q3/§4.5-1：缺省 10s；max(1) 钳下限（0 会把每个回调打成必超时）。
    let callback_timeout_ms = app
        .get("callback_timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(core_::budget::DEFAULT_CALLBACK_TIMEOUT_MS)
        .max(1);
    // 复审 P2-2：连续失败降级阈值可配（照 collector lua_core config 缺省 10）。
    let fail_threshold = app
        .get("fail_threshold")
        .and_then(|v| v.as_u64())
        .unwrap_or(10)
        .max(1) as u32;
    let mem_limit_mb = app
        .get("mem_limit_mb")
        .and_then(|v| v.as_u64())
        .unwrap_or(core_::budget::DEFAULT_MEM_LIMIT_MB as u64) as usize;
    let hook_instr_limit =
        app.get("hook_instr_limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(core_::budget::DEFAULT_HOOK_INSTR_LIMIT as u64) as u32;
    let scripts_root = app
        .get("scripts_root")
        .and_then(|v| v.as_str())
        .unwrap_or(SCRIPTS_ROOT)
        .to_string();

    // 字符串数组键（照 lua_north str_vec 模式）：非数组 → Err（坏声明比静默缺省
    // 更危险——订阅是 transform 的数据面入口，漏配 = 数据静默丢失，必须可见）。
    let subscribe = match app.get("subscribe") {
        None => Vec::new(),
        // 复审 P3-4：元素级非字符串 → Err（此前 filter_map 静默丢——坏声明可见，
        // 与模块头「订阅是数据面入口」原则一致；north str_vec 同步收紧）。
        Some(serde_json::Value::Array(arr)) => {
            if let Some(bad) = arr.iter().find(|v| !v.is_string()) {
                return Err(format!("app_config.subscribe 含非字符串元素（实 {bad}）"));
            }
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        }
        Some(other) => {
            return Err(format!(
                "app_config.subscribe 类型不符（期望字符串数组，实 {other}）"
            ))
        }
    };
    // 逐条模式校验（非法 pattern 在此点名，不拖到运行期静默不匹配）。
    for p in &subscribe {
        parse_pattern(p)?;
    }

    Ok(TransformCfg {
        script,
        period,
        subscribe,
        scripts_root,
        callback_timeout_ms,
        fail_threshold,
        mem_limit_mb,
        hook_instr_limit,
    })
}

/// dev 段形态：精确名或整段 `*`（全设备）。
#[derive(Debug)]
enum DevPat {
    /// `*`——匹配任意 devid。
    All,
    /// 精确 devid。
    Exact(String),
}

/// 单条订阅模式（"dev" / "dev.tagpat" 两段，Q6）。
///
/// - dev 段：精确名或整段 `*`；部分通配（如 `"dev*"`）解析期拒绝；
/// - tag 段（可省）：省略 = 该 dev 全部 tag；否则 glob（仅 `*` 通配）。
#[derive(Debug)]
pub(crate) struct SubPattern {
    dev: DevPat,
    tag: Option<String>,
}

/// 解析单条订阅模式（`parse_transform_cfg` 逐条调用；独立 pub(crate) 供测试直测）。
pub(crate) fn parse_pattern(raw: &str) -> Result<SubPattern, String> {
    if raw.is_empty() {
        return Err("订阅模式为空串（合法形态：\"dev\" / \"dev.tagpat\" / \"*\"）".into());
    }
    if raw.contains("..") {
        return Err(format!("订阅模式 '{raw}' 含 '..'（不允许）"));
    }
    let mut segs = raw.split('.');
    let dev = segs.next().unwrap_or_default();
    let tag = segs.next();
    if segs.next().is_some() {
        return Err(format!(
            "订阅模式 '{raw}' 超 2 段（合法形态：\"dev\" / \"dev.tagpat\"）"
        ));
    }
    if dev.is_empty() {
        return Err(format!("订阅模式 '{raw}' dev 段为空（\".tag\" 非法）"));
    }
    let dev = if dev == "*" {
        DevPat::All
    } else if dev.contains('*') {
        // dev 段只支持精确名或整段 `*`：部分通配静默不匹配的坑在解析期拦下。
        return Err(format!(
            "订阅模式 '{raw}' dev 段只支持精确名或整段 '*'（tag 通配请写第二段）"
        ));
    } else {
        DevPat::Exact(dev.to_string())
    };
    let tag = match tag {
        None => None,
        Some("") => {
            return Err(format!(
                "订阅模式 '{raw}' tag 段为空（全 tag 订阅请省略第二段，写 \"dev\"）"
            ))
        }
        Some(t) => Some(t.to_string()),
    };
    Ok(SubPattern { dev, tag })
}

impl SubPattern {
    /// 该模式是否匹配 `(devid, tag)` 数据点。
    pub(crate) fn matches(&self, devid: &str, tag: &str) -> bool {
        match &self.dev {
            DevPat::All => {}
            DevPat::Exact(d) if d == devid => {}
            _ => return false,
        }
        match &self.tag {
            None => true,
            Some(p) => glob_match(p, tag),
        }
    }
}

/// 仅 `*` 通配的 glob 匹配（无 `?`、无新依赖）：`*` 匹配任意序列（含空）。
///
/// 双指针回溯经典实现（char 粒度，tag 含非 ASCII 也正确）。
pub(crate) fn glob_match(pat: &str, s: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = s.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    // 最近一次 `*` 的位置 + 其已吞到的文本下标（回溯点）。
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            // 回溯：`*` 多吞一个字符重来。
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    // 尾部剩余须全为 `*`（可匹配空）。
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// 宿主侧订阅匹配器：订阅清单整体（多模式 any-of）。
///
/// 构造重新走 [`parse_pattern`]（parse 期已校验过——双查不绕过，坏数据 panic
/// 防线前移到可见 Err）；`is_empty` = 纯定时模式（不订 EventBus 的判定依据）。
pub(crate) struct SubMatcher {
    pats: Vec<SubPattern>,
}

impl SubMatcher {
    /// 从订阅清单构造（清单已过 parse 校验；防御性双查返回 Err）。
    pub(crate) fn new(pats: &[String]) -> Result<Self, String> {
        Ok(Self {
            pats: pats
                .iter()
                .map(|p| parse_pattern(p))
                .collect::<Result<_, _>>()?,
        })
    }

    /// 任一模式命中即真（any-of）。
    pub(crate) fn matches(&self, devid: &str, tag: &str) -> bool {
        self.pats.iter().any(|p| p.matches(devid, tag))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ===== parse_transform_cfg =====

    /// 缺省值：period 3（对齐 lua_client）、subscribe 空（纯定时）、scripts_root
    /// "scripts"、callback_timeout_ms 10s（照 lua_core）、沙箱参数照 collector/north。
    #[test]
    fn parse_defaults() {
        let cfg = parse_transform_cfg(&json!({"script":"agg1"})).unwrap();
        assert_eq!(cfg.script, "agg1");
        assert_eq!(cfg.period, 3, "period 缺省 3");
        assert!(cfg.subscribe.is_empty(), "subscribe 缺省空 = 纯定时");
        assert_eq!(cfg.scripts_root, "scripts");
        assert_eq!(cfg.callback_timeout_ms, 10_000, "照 lua_core 缺省 10s");
        assert_eq!(cfg.mem_limit_mb, 32);
        assert_eq!(cfg.hook_instr_limit, 1_000_000);
    }

    /// 显式覆盖全字段。
    #[test]
    fn parse_overrides() {
        let cfg = parse_transform_cfg(&json!({
            "script":"agg1","period":10,
            "subscribe":["dev1","dev2.temp*"],
            "scripts_root":"/tmp/x","callback_timeout_ms":500,
            "mem_limit_mb":64,"hook_instr_limit":2000
        }))
        .unwrap();
        assert_eq!(cfg.period, 10);
        assert_eq!(cfg.subscribe, vec!["dev1", "dev2.temp*"]);
        assert_eq!(cfg.scripts_root, "/tmp/x");
        assert_eq!(cfg.callback_timeout_ms, 500);
        assert_eq!(cfg.mem_limit_mb, 64);
        assert_eq!(cfg.hook_instr_limit, 2000);
    }

    /// script 必填 + 防路径穿越 + 拒空名（与 collector/north 同规则）。
    #[test]
    fn parse_rejects_bad_script() {
        assert!(parse_transform_cfg(&json!({})).is_err(), "缺 script");
        assert!(
            parse_transform_cfg(&json!({"script":"../etc"})).is_err(),
            "路径穿越"
        );
        assert!(parse_transform_cfg(&json!({"script":""})).is_err(), "空名");
    }

    /// subscribe 类型不符 → Err（坏声明可见，不静默降级空清单）。
    #[test]
    fn parse_rejects_bad_subscribe_type() {
        assert!(parse_transform_cfg(&json!({"script":"a", "subscribe":"dev1"})).is_err());
        assert!(parse_transform_cfg(&json!({"script":"a", "subscribe":42})).is_err());
    }

    /// 非法 pattern → 解析报错（坏声明可见：订阅写错 = 数据面静默丢失）。
    #[test]
    fn parse_rejects_bad_patterns() {
        for bad in ["", "..", "a.b.c", ".temp", "dev.", "dev*"] {
            assert!(
                parse_transform_cfg(&json!({"script":"a", "subscribe":[bad]})).is_err(),
                "pattern '{bad}' 应被拒"
            );
        }
    }

    // ===== 模式解析 + 匹配语义 =====

    fn m(p: &str) -> SubPattern {
        parse_pattern(p).expect("合法 pattern")
    }

    /// "dev1" = dev1 全部 tag（其它 dev 不匹配）。
    #[test]
    fn match_exact_dev_all_tags() {
        let p = m("dev1");
        assert!(p.matches("dev1", "temp"));
        assert!(p.matches("dev1", "hum"));
        assert!(!p.matches("dev2", "temp"));
    }

    /// "dev2.temp*" = dev2 + tag 前缀通配（其它 tag / 其它 dev 不匹配）。
    #[test]
    fn match_dev_tag_glob() {
        let p = m("dev2.temp*");
        assert!(p.matches("dev2", "temp"));
        assert!(p.matches("dev2", "temp_out"));
        assert!(!p.matches("dev2", "hum"));
        assert!(!p.matches("dev1", "temp"));
    }

    /// "*" = 全部设备全部 tag（dev 段整段通配）。
    #[test]
    fn match_star_all_devs() {
        let p = m("*");
        assert!(p.matches("anydev", "anytag"));
        assert!(p.matches("x", "y"));
    }

    /// "dev.*" = 该 dev 全部 tag；中缀 `*` 也支持（任意序列，含空）。
    #[test]
    fn match_tag_star_and_mid_glob() {
        let p = m("dev.*");
        assert!(p.matches("dev", "a"));
        assert!(p.matches("dev", "b"));
        let p = m("dev.t*p");
        assert!(p.matches("dev", "temp"));
        assert!(p.matches("dev", "tp"), "* 可匹配空序列");
        assert!(!p.matches("dev", "te"));
    }

    /// SubMatcher 多模式 any-of（清单整体语义）。
    #[test]
    fn matcher_any_of() {
        let sm = SubMatcher::new(&["dev1".into(), "dev2.h*".into()]).expect("合法清单");
        assert!(sm.matches("dev1", "x"), "dev1 全 tag");
        assert!(sm.matches("dev2", "hum"));
        assert!(!sm.matches("dev2", "temp"), "dev2 仅 h* tag");
        assert!(!sm.matches("dev3", "hum"));
    }

    /// 单点入口：合法 pattern 过、非法拒（空 / `.` / `..` / 空段 / 超 2 段 / dev 部分
    /// 通配）。
    #[test]
    fn pattern_parse_ok_and_err() {
        for ok in ["a", "a.b", "*", "*.*", "dev2.temp*"] {
            assert!(parse_pattern(ok).is_ok(), "'{ok}' 应合法");
        }
        for bad in ["", ".", "..", "a..b", "a.b.c", ".b", "a.", "a*b"] {
            assert!(parse_pattern(bad).is_err(), "'{bad}' 应拒");
        }
    }

    /// 复审 P3-4：subscribe 数组含非字符串元素 → Err（此前 filter_map 静默丢）。
    #[test]
    fn parse_subscribe_non_string_element_errors() {
        let r = parse_transform_cfg(&serde_json::json!({
            "script": "t", "subscribe": ["dev1", 42]
        }));
        assert!(r.is_err(), "非字符串元素应拒: {r:?}");
        let msg = r.unwrap_err();
        assert!(msg.contains("非字符串元素"), "错误应点名元素问题：{msg}");
    }
}
