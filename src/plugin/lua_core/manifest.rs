//! 脚本 manifest 头部解析 + dry-run 契约校验（设计 §2.1-2.2/§4.4，计划 T11）。
//!
//! main.lua 顶部注释块声明脚本契约（§2.1 语法），行格式 `-- @key value`：
//!
//! ```lua
//! -- @role collector
//! -- @api_ver 1
//! -- @stdlib >=1.0
//! -- @param period number 可选 上报周期秒 默认5 范围:1-3600
//! -- @source local
//! ```
//!
//! # dry_run 校验（Q1/§2.2/§4.4）
//!
//! 1. **API_VER 协商**：`@api_ver` > [`CUR_API_VER`]（capability.rs 单一事实源）
//!    拒载（请升级宿主）；< [`MIN_API_VER`] 拒载（请升级脚本）。
//! 2. **role 契约**（§5.1，B 阶段 T6 起多角色）：显式 `@role collector` → 全局
//!    `collect` 必须存在；`@role mqtt-north` → `auth`+`up` 必须存在；`@role
//!    transform` → `on_timer`/`on_rtdata` 至少其一（any-of，§2.2 C 阶段 T5）。
//! 3. **stdlib 范围**：`@stdlib >=X.Y <Z`（空格组合，全部满足）vs 本地 index
//!    版本（`stdlib_sync::read_index`；None = 种子兜底态，跳过版本比对），
//!    不满足给可行动提示（请 sync）。
//!
//! # 兼容决策：显式声明 `@role` 才启用 collect 契约校验
//!
//! 缺省 role 语义为 collector（v1 唯一角色），但 **仅当脚本显式声明 `@role`
//! 时才强制 `collect` 存在性**：存量生产脚本与测试/工具脚本常无 manifest 头
//! （可能只定义 on_data），若缺省也强制会让它们静默拒载。取舍：
//! 显式声明 = 作者签署契约 → 严格校验；无声明 → 向后兼容放行。
//! api_ver / stdlib 校验不受此豁免（api_ver 缺省 1 天然通过；stdlib 缺省不限）。
//!
//! 未知 `@key` 忽略不报错（向前兼容：宿主旧版本读新脚本头部）。

use crate::plugin::lua_core::capability::{CUR_API_VER, MIN_API_VER};
use mlua::Lua;

/// 脚本 manifest（§2.1 头部块解析结果）。
///
/// `role` 存原始声明（None = 未声明；缺省 collector 语义由 [`dry_run`] 提供，
/// 以区分"显式签署契约"与"存量无头脚本"——见模块文档兼容决策）。
/// `api_ver` 解析期就给缺省 1（u32，非法值直接 Err）。
#[allow(dead_code)]
// source/params 阶段 A 无运行时读者（T12 文档/B 阶段参数面板消费），对标 capability.rs 先例
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Manifest {
    /// 脚本角色（§2.2；v1 唯一 collector）。None = 未声明。
    pub(crate) role: Option<String>,
    /// 脚本要求的宿主 API 版本（缺省 1）。
    pub(crate) api_ver: u32,
    /// stdlib 版本范围（如 ">=1.0" / ">=1.0 <2"）。None = 不限。
    pub(crate) stdlib: Option<String>,
    /// 脚本来源徽标（local / market 等，§2.1）。
    pub(crate) source: Option<String>,
    /// 参数声明（§2.1 `@param`）。
    pub(crate) params: Vec<ParamDecl>,
}

/// 单个 `@param` 声明（§2.1：`-- @param <name> <type> [可选] [描述] [默认X] [枚举:..|..] [范围:a-b]`）。
#[allow(dead_code)] // 同 Manifest：阶段 A 仅 parse/tests 消费，B 阶段参数面板接线
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParamDecl {
    /// 参数名。
    pub(crate) name: String,
    /// 类型名（number/string/boolean）。
    pub(crate) ty: String,
    /// 是否必填（出现"可选"token → false；缺省 true）。
    pub(crate) required: bool,
    /// 默认值（`默认X` 后缀，存 X）。
    pub(crate) default: Option<String>,
    /// 约束原文（首个 `枚举:..` / `范围:a-b` token，含前缀整体保存）。
    pub(crate) constraints: Option<String>,
}

/// 解析 main.lua 源文本顶部注释块的 manifest 头。
///
/// 逐行扫描 `-- @key value`；遇到首个非注释非空行（代码行）即停（头部块结束）。
/// 未知 `@key` 忽略；`@api_ver` 非整数 → Err（坏声明比静默缺省更危险）。
pub(crate) fn parse(src: &str) -> Result<Manifest, String> {
    let mut m = Manifest {
        role: None,
        api_ver: 1,
        stdlib: None,
        source: None,
        params: Vec::new(),
    };
    for line in src.lines() {
        let trimmed = line.trim_start();
        let comment = match trimmed.strip_prefix("--") {
            Some(c) => c,
            // 空行 = 头部块内排版留白，继续扫；代码行 = 头部块结束
            None if trimmed.is_empty() => continue,
            None => break,
        };
        let body = comment.trim_start();
        let Some(rest) = body.strip_prefix('@') else {
            continue; // 普通 -- 注释行
        };
        let (key, value) = match rest.split_once(char::is_whitespace) {
            Some((k, v)) => (k, v.trim()),
            None => (rest, ""),
        };
        match key {
            "role" => m.role = Some(value.to_string()),
            "api_ver" => {
                m.api_ver = value.parse::<u32>().map_err(|_| {
                    format!("@api_ver 非法：{value:?}（应为整数，当前宿主 {CUR_API_VER}）")
                })?;
            }
            "stdlib" => m.stdlib = Some(value.to_string()),
            "source" => m.source = Some(value.to_string()),
            "param" => {
                if let Some(p) = parse_param(value) {
                    m.params.push(p);
                }
            }
            _ => {} // 未知 @key 忽略（向前兼容）
        }
    }
    Ok(m)
}

/// 解析单条 `@param` 值：`<name> <type> [可选] [描述...] [默认X] [枚举:a|b] [范围:a-b]`。
/// name/ty 不足两 token → None（不完整声明，整行忽略）。
fn parse_param(value: &str) -> Option<ParamDecl> {
    let mut it = value.split_whitespace();
    let name = it.next()?.to_string();
    let ty = it.next()?.to_string();
    let mut p = ParamDecl {
        name,
        ty,
        required: true,
        default: None,
        constraints: None,
    };
    for tok in it {
        if tok == "可选" {
            p.required = false;
        } else if let Some(d) = tok.strip_prefix("默认") {
            p.default = Some(d.to_string());
        } else if tok.starts_with("枚举:") || tok.starts_with("范围:") {
            // 首个约束 token 整体存（含前缀；§2.1 两形式，B 阶段参数面板再细分）
            if p.constraints.is_none() {
                p.constraints = Some(tok.to_string());
            }
        }
        // 其余 token = 人类可读描述，不参与契约
    }
    Some(p)
}

/// 角色必需回调契约形态（§2.2）：清单内全部必须（All）或至少其一（Any）。
enum Required {
    /// 清单内全部必须（collector→`collect`、mqtt-north→`auth`+`up`；缺哪个点名哪个）。
    All(&'static [&'static str]),
    /// 至少声明其一（transform→`on_timer`/`on_rtdata`，C 阶段 T5——纯定时/纯订阅
    /// 两形态都合法；`on_init` 恒可选不参与契约）。
    Any(&'static [&'static str]),
}

/// dry-run 契约校验（Q1/§2.2/§4.4）：api_ver 协商 + role 契约 + stdlib 范围。
///
/// `lua` 为已加载 main.lua 的 VM（globals 含脚本定义的函数）；
/// `stdlib_ver` 为本地 stdlib index 版本（None = 未 sync 种子态，跳过版本比对）。
/// 不满足给可行动错误消息（含"请升级宿主/升级脚本/请 sync"）。
///
/// role 契约按角色查必需回调（§5.1；B 阶段 T6 起多角色）：collector→`collect`、
/// mqtt-north→`auth`+`up`（down/will/on_connack/on_disconnect 可选）、transform→
/// `on_timer`/`on_rtdata` any-of（§2.2，C 阶段 T5）。仅显式声明 `@role` 时启用
///（存量无头脚本豁免，见模块文档兼容决策）。
pub(crate) fn dry_run(m: &Manifest, lua: &Lua, stdlib_ver: Option<&str>) -> Result<(), String> {
    // 1) API_VER 协商（§4.4；CUR/MIN 单一事实源在 capability.rs，与 stdlib sync 预检共用）
    if m.api_ver > CUR_API_VER {
        return Err(format!(
            "脚本 api_ver {} > 宿主 {CUR_API_VER}，请升级宿主",
            m.api_ver
        ));
    }
    if m.api_ver < MIN_API_VER {
        return Err(format!(
            "脚本 api_ver {} < 宿主最低 {MIN_API_VER}，请升级脚本",
            m.api_ver
        ));
    }
    // 2) role 契约（§5.1/§5.2）：角色 → 必需全局回调契约（All/Any，C 阶段 T5）；
    //    仅显式声明时校验。
    let role = m.role.as_deref();
    let required = match role {
        Some("collector") => Required::All(&["collect"]),
        Some("mqtt-north") => Required::All(&["auth", "up"]),
        Some("transform") => Required::Any(&["on_timer", "on_rtdata"]),
        Some(other) => {
            return Err(format!(
                "@role {other} 未知（本宿主支持：collector、mqtt-north、transform）"
            ));
        }
        None => Required::All(&[]),
    };
    let has = |f: &str| lua.globals().get::<mlua::Function>(f).is_ok();
    match required {
        Required::All(list) => {
            for f in list {
                if !has(f) {
                    // required 非空 ⟺ role 是 Some（None 分支清单为空，循环不进）。
                    return Err(format!(
                        "{} 角色缺少 {f}()：显式 @role 需定义全局 {f}",
                        role.unwrap_or_default()
                    ));
                }
            }
        }
        Required::Any(list) => {
            if !list.iter().any(|f| has(f)) {
                let names = list.join(" 或 ");
                return Err(format!(
                    "{} 角色需至少声明 {names} 其一：显式 @role 需定义对应全局回调",
                    role.unwrap_or_default()
                ));
            }
        }
    }
    // 3) @stdlib 范围 vs 本地 index 版本（§3.5；None = 种子兜底态跳过——冷启动不拒载）
    if let (Some(spec), Some(local)) = (&m.stdlib, stdlib_ver) {
        if !stdlib_range_ok(spec, local) {
            return Err(format!(
                "本机 stdlib {local} / 脚本要求 {spec}，请 sync（POST /api/v1/lua/stdlib/sync）"
            ));
        }
    }
    Ok(())
}

/// 读 main.lua → parse → [`dry_run`]（薄壳共用：luaClient 启动/reload 与 luaNorth
/// 同一入口，B 阶段 T6 自 lua_client 下沉）。
///
/// Ok 返回一行摘要（role/api_ver/stdlib 协商结果，含本机 stdlib 版本——现场诊断
/// "为何拒载/放行"一眼可见）；Err 为可行动错误消息。源读取失败按空源处理
/// （全缺省 manifest → 通过）：调用方 `load_main` 刚读过同一文件，此处仅竞态
/// 兜底，拒载语义由 `load_main` 承担。
pub(crate) fn dry_run_main(
    lua: &Lua,
    scripts_root: &std::path::Path,
    script_name: &str,
    stdlib_ver: Option<&str>,
) -> Result<String, String> {
    let path = crate::plugin::lua_core::loader::main_lua_path(scripts_root, script_name);
    let src = std::fs::read_to_string(&path).unwrap_or_default();
    let m = parse(&src)?;
    dry_run(&m, lua, stdlib_ver)?;
    let stdlib_desc = match (&m.stdlib, stdlib_ver) {
        (Some(spec), Some(local)) => format!("{spec}（本机 {local}）"),
        (Some(spec), None) => format!("{spec}（本机无 index，种子态未比对）"),
        (None, Some(local)) => format!("不限（本机 {local}）"),
        (None, None) => "不限".into(),
    };
    Ok(format!(
        "role={} api_ver={} stdlib={stdlib_desc}",
        m.role.as_deref().unwrap_or("collector(缺省)"),
        m.api_ver
    ))
}

/// 数值段版本比较（"1.10" > "1.9"；短侧补 0："1.0" == "1.0.0"；非数值段按 0）。
fn ver_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let seg = |v: &str| -> Vec<u64> {
        v.split('.')
            .map(|s| s.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (mut sa, mut sb) = (seg(a), seg(b));
    let n = sa.len().max(sb.len());
    sa.resize(n, 0);
    sb.resize(n, 0);
    sa.cmp(&sb)
}

/// @stdlib 范围判断：">=X.Y" 下界 + "<X" 上界，空格组合（全部满足才过）；
/// 未知形式 token 忽略（向前兼容新语法）。
fn stdlib_range_ok(spec: &str, local: &str) -> bool {
    use std::cmp::Ordering;
    spec.split_whitespace().all(|tok| {
        if let Some(v) = tok.strip_prefix(">=") {
            ver_cmp(local, v) != Ordering::Less
        } else if let Some(v) = tok.strip_prefix('<') {
            ver_cmp(local, v) == Ordering::Less
        } else {
            true
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 已定义全局 collect 的裸 VM（dry_run role 校验的最小通过态）。
    fn vm_with_collect() -> Lua {
        let lua = Lua::new();
        lua.load("function collect() return { v = 1 } end")
            .exec()
            .unwrap();
        lua
    }

    /// 计划 T11 Step 1：头部块全键解析 + @param 后缀（可选/默认X/范围:a-b）。
    #[test]
    fn parses_header_block() {
        let src = "-- @role collector\n-- @api_ver 1\n-- @stdlib >=1.0\n\
                   -- @param period number 可选 上报周期秒 默认5 范围:1-3600\n\
                   -- @source local\nlocal x = 1";
        let m = parse(src).unwrap();
        assert_eq!(m.role.as_deref(), Some("collector"));
        assert_eq!(m.api_ver, 1);
        assert_eq!(m.stdlib.as_deref(), Some(">=1.0"));
        assert_eq!(m.source.as_deref(), Some("local"));
        assert_eq!(m.params.len(), 1);
        assert_eq!(m.params[0].name, "period");
        assert_eq!(m.params[0].ty, "number");
        assert!(!m.params[0].required, "出现「可选」token → 非必填");
        assert_eq!(m.params[0].default.as_deref(), Some("5"));
        assert_eq!(m.params[0].constraints.as_deref(), Some("范围:1-3600"));
    }

    /// 计划 T11 Step 1：无 manifest 头 → 全缺省（api_ver=1；role 存原始 None，
    /// 缺省 collector 语义在 dry_run 给——兼容决策见模块文档）。
    #[test]
    fn defaults_when_absent() {
        let m = parse("-- hi").unwrap();
        assert_eq!(m.api_ver, 1, "缺省 1");
        assert!(
            m.role.is_none(),
            "Option 存原始；缺省 collector 语义由 dry_run 提供"
        );
        assert!(m.stdlib.is_none());
        assert!(m.source.is_none());
        assert!(m.params.is_empty());
    }

    /// 未知 @key 忽略不报错（向前兼容：新版本脚本头部带宿主不认识的键）。
    #[test]
    fn unknown_keys_ignored() {
        let m = parse("-- @future_key x\n-- @role collector\nlocal x = 1").unwrap();
        assert_eq!(m.role.as_deref(), Some("collector"));
    }

    /// 头部块在首个代码行终止：代码行后的 `-- @api_ver 99` 不读（防脚本体偶发匹配）。
    #[test]
    fn header_block_ends_at_first_code_line() {
        let m = parse("-- @role collector\nlocal s = \"-- @api_ver 99\"\n-- @api_ver 99").unwrap();
        assert_eq!(m.api_ver, 1, "代码行后的 @key 不应生效");
    }

    /// @api_ver 非整数 → Err（坏声明显式报错，不静默按缺省 1 放行）。
    #[test]
    fn parse_rejects_non_numeric_api_ver() {
        let e = parse("-- @api_ver abc").unwrap_err();
        assert!(e.contains("api_ver"), "错误应指明键名：{e}");
    }

    /// @param 枚举约束 + 缺省必填（无「可选」token）。
    #[test]
    fn parses_param_enum_and_required_default() {
        let m = parse("-- @param mode string 枚举:a|b").unwrap();
        assert_eq!(m.params.len(), 1);
        assert!(m.params[0].required, "无「可选」token → 必填");
        assert_eq!(m.params[0].constraints.as_deref(), Some("枚举:a|b"));
        assert!(m.params[0].default.is_none());
    }

    /// dry_run 拒：api_ver 超宿主（动态取 CUR+1，防宿主升版后测试漂移）。
    #[test]
    fn dry_run_rejects_future_api_ver() {
        let lua = vm_with_collect();
        let m = parse(&format!("-- @api_ver {}", CUR_API_VER + 1)).unwrap();
        let e = dry_run(&m, &lua, None).unwrap_err();
        assert!(e.contains("升级宿主"), "应提示升级宿主：{e}");
        assert!(e.contains("api_ver"), "应含键名：{e}");
    }

    /// dry_run 拒：api_ver 低于宿主下界（0 < MIN_API_VER=1）。
    #[test]
    fn dry_run_rejects_api_ver_below_min() {
        let lua = vm_with_collect();
        let m = parse("-- @api_ver 0").unwrap();
        let e = dry_run(&m, &lua, None).unwrap_err();
        assert!(e.contains("升级脚本"), "应提示升级脚本：{e}");
    }

    /// dry_run 拒：显式 @role collector 但全局无 collect（契约未满足）。
    #[test]
    fn dry_run_rejects_missing_collect_when_role_declared() {
        let lua = Lua::new(); // 无 collect
        let m = parse("-- @role collector").unwrap();
        let e = dry_run(&m, &lua, None).unwrap_err();
        assert!(e.contains("collect"), "应指明缺 collect()：{e}");
    }

    /// 兼容决策锁：无 manifest 头的存量脚本（可能只定义 on_data）不做 collect 强制。
    #[test]
    fn dry_run_allows_absent_collect_without_manifest() {
        let lua = Lua::new(); // 只有 on_data，无 collect
        lua.load("function on_data(f) end").exec().unwrap();
        let m = parse("function on_data(f) end").unwrap();
        assert!(dry_run(&m, &lua, None).is_ok(), "无声明 → 放行（向后兼容）");
    }

    /// dry_run 拒：显式未知角色（v1 仅 collector；宿主不认识的角色应显式拒）。
    #[test]
    fn dry_run_rejects_unknown_role() {
        let lua = vm_with_collect();
        let m = parse("-- @role processor").unwrap();
        let e = dry_run(&m, &lua, None).unwrap_err();
        assert!(e.contains("processor"), "应指明未知角色名：{e}");
    }

    /// dry_run 拒：@stdlib >=9.9 vs 本机 1.0（可行动提示含双方版本与 sync）。
    #[test]
    fn dry_run_rejects_stdlib_range_mismatch() {
        let lua = vm_with_collect();
        let m = parse("-- @stdlib >=9.9").unwrap();
        let e = dry_run(&m, &lua, Some("1.0")).unwrap_err();
        assert!(e.contains("9.9") && e.contains("1.0"), "应含双方版本：{e}");
        assert!(e.contains("sync"), "应提示 sync：{e}");
    }

    /// stdlib_ver=None（未 sync 种子态）：跳过版本比对（脚本声明高版本也放行，
    /// 由接线层摘要记录；冷启动内网场景不拒载）。
    #[test]
    fn dry_run_stdlib_none_index_skips_check() {
        let lua = vm_with_collect();
        let m = parse("-- @stdlib >=9.9").unwrap();
        assert!(
            dry_run(&m, &lua, None).is_ok(),
            "无本地 index → 跳过版本比对"
        );
    }

    /// 组合范围 ">=1.0 <2"：区间内通过、触上界拒（空格分隔全部满足）。
    #[test]
    fn dry_run_stdlib_combined_range() {
        let lua = vm_with_collect();
        let m = parse("-- @stdlib >=1.0 <2").unwrap();
        assert!(dry_run(&m, &lua, Some("1.5")).is_ok(), "1.5 在 [1.0, 2) 内");
        assert!(dry_run(&m, &lua, Some("2.0")).is_err(), "2.0 触上界");
        assert!(dry_run(&m, &lua, Some("0.9")).is_err(), "0.9 低于下界");
    }

    /// 版本比较语义锁：数值段比较（"1.10" > "1.9"）+ 尾零等价（"1.0" == "1.0.0"）。
    #[test]
    fn version_compare_numeric_segments() {
        use std::cmp::Ordering::*;
        assert_eq!(ver_cmp("1.10", "1.9"), Greater);
        assert_eq!(ver_cmp("1.0", "1.0.0"), Equal);
        assert_eq!(ver_cmp("2", "1.99"), Greater);
        assert_eq!(ver_cmp("1.2", "1.10"), Less);
    }

    /// 好契约全通过：显式 @role collector + collect 存在 + api_ver=1 + stdlib 满足。
    #[test]
    fn dry_run_passes_good_contract() {
        let lua = vm_with_collect();
        let m = parse("-- @role collector\n-- @api_ver 1\n-- @stdlib >=1.0").unwrap();
        assert!(dry_run(&m, &lua, Some("1.2")).is_ok());
    }

    /// B 阶段 T6 多角色契约：显式 @role mqtt-north + auth+up 都在 → 通过
    ///（§5.1 回调契约：auth(cfg)/up(batch) 必需，down/will/on_connack/on_disconnect 可选）。
    #[test]
    fn dry_run_passes_mqtt_north_with_auth_and_up() {
        let lua = Lua::new();
        lua.load("function auth(cfg) return {} end\nfunction up(batch) return {} end")
            .exec()
            .unwrap();
        let m = parse("-- @role mqtt-north").unwrap();
        assert!(dry_run(&m, &lua, None).is_ok(), "auth+up 齐备应通过");
    }

    /// mqtt-north 缺 up → 拒（错误指明缺的回调名，可行动）。
    #[test]
    fn dry_run_rejects_mqtt_north_missing_up() {
        let lua = Lua::new();
        lua.load("function auth(cfg) return {} end").exec().unwrap();
        let m = parse("-- @role mqtt-north").unwrap();
        let e = dry_run(&m, &lua, None).unwrap_err();
        assert!(e.contains("up"), "应指明缺 up()：{e}");
        assert!(e.contains("mqtt-north"), "错误应含角色名：{e}");
    }

    /// mqtt-north 缺 auth → 拒（重连必重调 auth 的前提是 auth 存在，Q5）。
    #[test]
    fn dry_run_rejects_mqtt_north_missing_auth() {
        let lua = Lua::new();
        lua.load("function up(batch) return {} end").exec().unwrap();
        let m = parse("-- @role mqtt-north").unwrap();
        let e = dry_run(&m, &lua, None).unwrap_err();
        assert!(e.contains("auth"), "应指明缺 auth()：{e}");
    }

    /// mqtt-north 只定义 collect（collector 契约）不算满足 north 契约：auth+up 必需。
    #[test]
    fn dry_run_rejects_mqtt_north_with_only_collect() {
        let lua = vm_with_collect();
        let m = parse("-- @role mqtt-north").unwrap();
        let e = dry_run(&m, &lua, None).unwrap_err();
        assert!(e.contains("auth"), "collect 不能替代 auth：{e}");
    }

    /// C 阶段 T5 transform 契约（§2.2 any-of）：on_timer/on_rtdata 至少声明其一
    ///（纯定时 / 纯订阅两形态都合法）；on_init 恒可选（不参与契约，也不能顶替）。
    #[test]
    fn dry_run_transform_any_of_on_timer_or_on_rtdata() {
        // 只 on_timer（纯定时形态）→ 过。
        let lua = Lua::new();
        lua.load("function on_timer() end").exec().unwrap();
        let m = parse("-- @role transform").unwrap();
        assert!(dry_run(&m, &lua, None).is_ok(), "只 on_timer 应通过（纯定时）");

        // 只 on_rtdata（纯订阅形态）→ 过。
        let lua = Lua::new();
        lua.load("function on_rtdata(devid, tag, value) end").exec().unwrap();
        let m = parse("-- @role transform").unwrap();
        assert!(dry_run(&m, &lua, None).is_ok(), "只 on_rtdata 应通过（纯订阅）");

        // 两者皆无（即使有 on_init）→ 拒，错误可行动：点名角色与两个回调 + 其一语义。
        let lua = Lua::new();
        lua.load("function on_init() end").exec().unwrap();
        let m = parse("-- @role transform").unwrap();
        let e = dry_run(&m, &lua, None).unwrap_err();
        assert!(e.contains("transform"), "错误应含角色名：{e}");
        assert!(
            e.contains("on_timer") && e.contains("on_rtdata"),
            "应点名两个候选回调：{e}"
        );
        assert!(e.contains("其一"), "应表达 any-of 语义：{e}");

        // 两者皆有 + on_init → 过（on_init 可选，双回调叠加合法形态）。
        let lua = Lua::new();
        lua.load("function on_init() end\nfunction on_timer() end\nfunction on_rtdata(d, t, v) end")
            .exec()
            .unwrap();
        let m = parse("-- @role transform").unwrap();
        assert!(dry_run(&m, &lua, None).is_ok(), "双回调 + on_init 应通过");
    }
}
