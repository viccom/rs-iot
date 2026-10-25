//! 能力卡（设计 §4.2/Q2）：宿主赋予脚本能力的元数据登记——单一事实源，
//! 驱动一致性测试、护栏参数与文档生成。
//!
//! 现登记 22 张卡：21 个 ctx 方法（与 [`crate::plugin::lua_core::api::register`] +
//! `api/http.rs` + `api/alarm.rs` 的 ctx.set 清单逐一对齐；设计 §4.1 表写的"17 项"
//! 实测 +T5 `secret_get` +D 阶段 T4 告警双函数为 21——`set_values`/`log_*` 按方法名
//! 单计数）+ builtin `json` 模块（T8 实现，`api::tests::json_roundtrip_and_limit`
//! 锁行为）。
//! 一致性锁：本文件 tests（卡名格式/去重/数量）+ `api::tests::capability_cards_match_registered_ctx`
//! （每张 "ctx:" 卡必须真注册在 mock VM 的 ctx 表上）。

/// 能力分级（设计 §4.1）：C0 纯函数 / C1 链路通讯 / C2 数据读写 / C3 控制联动 /
/// C4 持久状态 / C5 网络出站 / C6 可观测 / C7 高性能算法（Rust 加速）。
#[allow(dead_code)] // C7 为规划层（sparkplug/FFT 等，§4.1），阶段 A 无卡片
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Layer {
    /// 纯函数（无副作用）：now/time/json。
    C0,
    /// 链路通讯：request/transport。
    C1,
    /// 数据读写：set_value(s)/get_*/list_devices。
    C2,
    /// 控制联动：send_command。
    C3,
    /// 持久状态：kv_set/kv_get。
    C4,
    /// 网络出站：http_get/http_post（feature lua-http）。
    C5,
    /// 可观测：log_*。
    C6,
    /// 高性能算法（规划：sparkplug 编解码/FFT/摘要加密，§4.1）。
    C7,
}

/// 风险分级（设计 §4.2）：R0 纯函数 / R1 受控副作用 / R2 网络出站 / R3 权限敏感。
#[allow(dead_code)] // 同 Layer：全变体已用，防御后续增删变体时的 lint 噪音
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Risk {
    /// 纯函数：无护栏要求，仅输入大小上限。
    R0,
    /// 受控副作用（log/kv/set_value）：现有护栏不变。
    R1,
    /// 网络出站（http/mqtt pub）：白名单 + 大小限额 + 频控。
    R2,
    /// 权限敏感（send_command/secret 读取，§4.2）。Q4 定案：secret_get 恒注入 +
    /// 实例配置 secrets 清单运行时校验（越权可感知报错，非静默）。
    R3,
}

/// 一张能力卡的元数据（设计 §4.2 卡片示例的代码化）。
///
/// struct 级 `#[allow(dead_code)]`（对标 `config::LuaCfg` 先例）：部分字段
/// （layer/risk/feature/api_ver/desc）阶段 A 消费方是一致性测试与文档生成；
/// `timeout_cap_ms`/`rate_per_sec` 已被 clamp/频控运行时读取（单一事实源）。
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct CapabilityCard {
    /// 能力名："ctx:<方法名>" 或 builtin 模块名（如 "json"）。
    pub(crate) name: &'static str,
    /// 能力分级（§4.1）。
    pub(crate) layer: Layer,
    /// 风险分级（§4.2）。
    pub(crate) risk: Risk,
    /// 门控 feature 名（如 "lua-http"）；None = 无门控。
    pub(crate) feature: Option<&'static str>,
    /// 脚本可传超时参数的宿主上限（毫秒；0 = 该能力无超时参数）。
    pub(crate) timeout_cap_ms: u64,
    /// 频控上限（次/秒/实例；0 = 不限）。
    pub(crate) rate_per_sec: u32,
    /// 能力 API 版本（§4.4 API_VER）。
    pub(crate) api_ver: u32,
    /// 一句话中文描述（文档/前端能力表展示）。
    pub(crate) desc: &'static str,
}

/// builtin 单段模块名（require 路由的 Builtin 层名单，§3.1；T6 loader 消费）。
#[allow(dead_code)] // T6 loader 接线前仅本文件测试消费
pub(crate) const BUILTIN_MODULES: &[&str] = &["json"];

/// 宿主当前 ctx API 版本（设计 §4.4 API_VER 协商）。
///
/// 单一事实源：T9 stdlib sync 的上游 index `min_api_ver` 预检（超前版本拒装，§3.3
/// 兼容矩阵）与 T11 manifest dry-run（脚本 `@api_ver` 上界校验）共用。B/C 阶段破坏性
/// 变更时 +1。
pub(crate) const CUR_API_VER: u32 = 1;

/// 宿主接受的最低脚本 API 版本（§4.4 API_VER 协商下界；T11 manifest dry-run 消费）。
///
/// 脚本 `@api_ver` 低于此值 → 拒载（提示升级脚本）。B/C 阶段弃用旧 API 时才上调；
/// 与 [`CUR_API_VER`] 同处（版本协商常量单一事实源）。
pub(crate) const MIN_API_VER: u32 = 1;

/// 全部能力卡（单一事实源）。顺序与 `api/mod.rs` + `api/http.rs` 的注册顺序一致。
#[allow(dead_code)] // 阶段 A 消费方为一致性测试与文档生成；T4/T5 护栏接线后转运行时读
pub(crate) static CAPABILITIES: &[CapabilityCard] = &[
    // --- L1 链路（api/mod.rs）---
    CapabilityCard {
        name: "ctx:request",
        layer: Layer::C1,
        risk: Risk::R1,
        feature: None,
        timeout_cap_ms: 30_000, // Q3：request 超时宿主 clamp ≤30s（T4 接线）
        rate_per_sec: 0,
        api_ver: 1,
        desc: "链路一问一答",
    },
    // --- L6 可观测（api/mod.rs register_log）---
    CapabilityCard {
        name: "ctx:log_info",
        layer: Layer::C6,
        risk: Risk::R1,
        feature: None,
        timeout_cap_ms: 0,
        rate_per_sec: 0,
        api_ver: 1,
        desc: "信息级日志",
    },
    CapabilityCard {
        name: "ctx:log_warn",
        layer: Layer::C6,
        risk: Risk::R1,
        feature: None,
        timeout_cap_ms: 0,
        rate_per_sec: 0,
        api_ver: 1,
        desc: "警告级日志",
    },
    CapabilityCard {
        name: "ctx:log_err",
        layer: Layer::C6,
        risk: Risk::R1,
        feature: None,
        timeout_cap_ms: 0,
        rate_per_sec: 0,
        api_ver: 1,
        desc: "错误级日志",
    },
    CapabilityCard {
        name: "ctx:log_debug",
        layer: Layer::C6,
        risk: Risk::R1,
        feature: None,
        timeout_cap_ms: 0,
        rate_per_sec: 0,
        api_ver: 1,
        desc: "调试级日志",
    },
    // --- L6→C0 纯函数：now/time（§4.1 归 C0；api/mod.rs 注册紧随 log）---
    CapabilityCard {
        name: "ctx:now",
        layer: Layer::C0,
        risk: Risk::R0,
        feature: None,
        timeout_cap_ms: 0,
        rate_per_sec: 0,
        api_ver: 1,
        desc: "当前毫秒时间戳",
    },
    CapabilityCard {
        name: "ctx:time",
        layer: Layer::C0,
        risk: Risk::R0,
        feature: None,
        timeout_cap_ms: 0,
        rate_per_sec: 0,
        api_ver: 1,
        desc: "当前秒时间戳",
    },
    // --- L4 持久状态（api/mod.rs）---
    CapabilityCard {
        name: "ctx:kv_set",
        layer: Layer::C4,
        risk: Risk::R1,
        feature: None,
        timeout_cap_ms: 0,
        rate_per_sec: 0,
        api_ver: 1,
        desc: "实例持久状态写（lux luakv）",
    },
    CapabilityCard {
        name: "ctx:kv_get",
        layer: Layer::C4,
        risk: Risk::R1,
        feature: None,
        timeout_cap_ms: 0,
        rate_per_sec: 0,
        api_ver: 1,
        desc: "实例持久状态读（lux luakv）",
    },
    CapabilityCard {
        name: "ctx:secret_get",
        layer: Layer::C4,
        risk: Risk::R3, // Q4/R3：恒注入 + 实例配置 secrets 清单运行时校验（越权可感知）
        feature: None,
        timeout_cap_ms: 0,
        rate_per_sec: 0,
        api_ver: 1,
        desc: "凭证读取（实例配置 secrets 清单显式授权）",
    },
    // --- L3 控制联动（api/mod.rs）---
    CapabilityCard {
        name: "ctx:send_command",
        layer: Layer::C3,
        risk: Risk::R3, // §4.2 权限敏感。B 阶段 T6 起 collector/north 均恒注入（Q5 定案：
        // mqtt-north down() 下行依赖 send_command；T8 兼容脚本同依赖）——
        // 「实例配置显式授权才注入」的收紧排后续 task。
        feature: None,
        timeout_cap_ms: 0,
        rate_per_sec: 0,
        api_ver: 1,
        desc: "跨实例控制指令投递",
    },
    // --- L2 数据读写（api/mod.rs）---
    CapabilityCard {
        name: "ctx:set_value",
        layer: Layer::C2,
        risk: Risk::R1,
        feature: None,
        timeout_cap_ms: 0,
        rate_per_sec: 0,
        api_ver: 1,
        desc: "单数据点上数（推 hub）",
    },
    CapabilityCard {
        name: "ctx:set_values",
        layer: Layer::C2,
        risk: Risk::R1,
        feature: None,
        timeout_cap_ms: 0,
        rate_per_sec: 0,
        api_ver: 1,
        desc: "单设备批量上数（推 hub）",
    },
    CapabilityCard {
        name: "ctx:get_value",
        layer: Layer::C2,
        risk: Risk::R0,
        feature: None,
        timeout_cap_ms: 0,
        rate_per_sec: 0,
        api_ver: 1,
        desc: "读设备单点实时值",
    },
    CapabilityCard {
        name: "ctx:get_device",
        layer: Layer::C2,
        risk: Risk::R0,
        feature: None,
        timeout_cap_ms: 0,
        rate_per_sec: 0,
        api_ver: 1,
        desc: "读设备全部实时值",
    },
    CapabilityCard {
        name: "ctx:list_devices",
        layer: Layer::C2,
        risk: Risk::R0,
        feature: None,
        timeout_cap_ms: 0,
        rate_per_sec: 0,
        api_ver: 1,
        desc: "实例设备清单",
    },
    // --- L1 链路元信息（api/mod.rs）---
    CapabilityCard {
        name: "ctx:transport",
        layer: Layer::C1,
        risk: Risk::R0,
        feature: None,
        timeout_cap_ms: 0,
        rate_per_sec: 0,
        api_ver: 1,
        desc: "底层链路类型（tcp/serial/udp）",
    },
    // --- C6 告警（Q12 T4，api/alarm.rs；跨角色全注入 + fire 频控/抖动护栏）---
    CapabilityCard {
        name: "ctx:fire_event",
        layer: Layer::C6,
        risk: Risk::R1,
        feature: None,
        timeout_cap_ms: 0,
        rate_per_sec: 20, // spec alarm §3：每实例 20/s，超限返 false + alarm_rate_limited 计数
        api_ver: 1,
        desc: "触发告警（20/s 频控 + 同 key 1s 抖动）",
    },
    CapabilityCard {
        name: "ctx:clear_event",
        layer: Layer::C6,
        risk: Risk::R1,
        feature: None,
        timeout_cap_ms: 0,
        // 复审 P1-2：clear 与 fire 同频控限值（单一事实源经 alarm_rate_per_sec 查
        // ctx:fire_event 卡——此处 20 为文档值，消费见 api/alarm.rs）。
        rate_per_sec: 20,
        api_ver: 1,
        desc: "清除告警（幂等，恒发 Clear 事件）",
    },
    // --- L5 网络出站（api/http.rs，feature lua-http；Q3 频控 20/s）---
    CapabilityCard {
        name: "ctx:http_get",
        layer: Layer::C5,
        risk: Risk::R2,
        feature: Some("lua-http"),
        timeout_cap_ms: 30_000,
        rate_per_sec: 20,
        api_ver: 1,
        desc: "HTTP GET 出站",
    },
    CapabilityCard {
        name: "ctx:http_post",
        layer: Layer::C5,
        risk: Risk::R2,
        feature: Some("lua-http"),
        timeout_cap_ms: 30_000,
        rate_per_sec: 20,
        api_ver: 1,
        desc: "HTTP POST 出站",
    },
    // --- C0 builtin 模块（T8 实现，§4.1/§3.1/D5：serde_json 桥 + 1MB 输入上限）---
    CapabilityCard {
        name: "json",
        layer: Layer::C0,
        risk: Risk::R0,
        feature: None,
        timeout_cap_ms: 0,
        rate_per_sec: 0,
        api_ver: 1,
        desc: "JSON 编解码（decode 输入 ≤1MB）",
    },
];

/// 宿主侧超时 clamp（Q3/附录 B 冻结面②）：脚本可传超时封顶 30s、下限 1ms。
///
/// 上限查能力卡 `ctx:request` 的 `timeout_cap_ms`——单一事实源（阶段 A 审查修复：
/// 原先硬编码 30_000 与卡片靠约定同步，有漂移风险；`api/http.rs` 的秒级 clamp 同理
/// 读 http 卡）。卡缺失或 0（=无超时参数）时回落保守默认 30s。
pub(crate) fn clamp_timeout_ms(v: u64) -> u64 {
    v.clamp(1, request_timeout_cap_ms())
}

/// `ctx:request` 卡的宿主超时上限（ms）；缺省回落 30s。
pub(crate) fn request_timeout_cap_ms() -> u64 {
    CAPABILITIES
        .iter()
        .find(|c| c.name == "ctx:request")
        .map(|c| c.timeout_cap_ms)
        .filter(|&t| t > 0)
        .unwrap_or(30_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Q2 一致性锁（卡表层）：卡名格式（"ctx:<方法名>" 或 builtin 模块名）、无重复、
    /// ≥17 项全登记（真注册断言在 `api::tests::capability_cards_match_registered_ctx`）。
    #[test]
    fn capability_cards_wellformed_and_no_dup() {
        let mut seen = std::collections::HashSet::new();
        for c in CAPABILITIES {
            assert!(
                c.name.starts_with("ctx:")
                    || (c.layer == Layer::C0 && BUILTIN_MODULES.contains(&c.name)),
                "能力卡 name 非法：{}",
                c.name
            );
            assert!(seen.insert(c.name), "能力卡重复：{}", c.name);
        }
        assert!(
            CAPABILITIES.len() >= 17,
            "现有 17+ 个 ctx API 应全部登记（实 {}）",
            CAPABILITIES.len()
        );
    }

    /// 附录 B 冻结面② / Q3：宿主侧超时 clamp 上限 30s、下限 1ms（T4 接线调用点）。
    #[test]
    fn clamp_timeout_ms_bounds() {
        assert_eq!(clamp_timeout_ms(0), 1);
        assert_eq!(clamp_timeout_ms(1), 1);
        assert_eq!(clamp_timeout_ms(200), 200);
        assert_eq!(clamp_timeout_ms(30_000), 30_000);
        assert_eq!(clamp_timeout_ms(3_600_000), 30_000);
    }
}
