//! S7 地址严格校验（S-12 / S-13，2026-11-01 协议语义审查）。
//!
//! tokio-s7 0.1.7 的地址解析器 `RequestItem::extract_number` 只保留 ASCII 数字
//! （`<REG>tokio-s7-0.1.7/src/item.rs:211-222`），尺寸字母 `X/B/W/D` 与 TIA 的
//! `%` 前缀纯装饰/不可解析，且把 T/C 区按 `<<2` 编址后按 BCD 语义取数。结果是
//! 一串**能连上但读错地址**的静默错值：
//!
//! - `DB1.DBX0.5` + `u16` → 位段被丢弃，实际读 `DB1.DBW0`；
//! - `%DB1.DBX0.0`（TIA 导出格式）→ 库报 `unknown area prefix: '%'`；
//! - `T5` / `C3` → 库按定时器/计数器 BCD 语义取数，值无意义；
//! - `PIW256` / `AIW0` / `SM0.0` → 库无解析入口（`S7Area::from_first_char`
//!   只认 `I/Q/M/D/V/T/C`），运行期报 `unknown area prefix` 之类的怪错。
//!
//! 因此插件在**配置期**（装配点表 / 解析写命令）就做严格校验：坏形态当场拒绝并
//! 给出可操作的提示，不给运行期留"每拍失败但实例看着健康"的坑。
//!
//! 支持形态（大小写不敏感，首尾空白忽略）：
//!
//! | 形态 | 适用 kind |
//! |---|---|
//! | `DB<n>.DBX<b>.<bit>` | 仅 `bool` |
//! | `DB<n>.DBB<b>` / `DB<n>.DBW<b>` / `DB<n>.DBD<b>` | 任意非位 kind（宽度由 kind 决定，前缀只是可读性） |
//! | `I/Q/M/V` + `<b>` 或 `<B\|W\|D><b>` | 任意非位 kind |
//! | `I/Q/M/V` + `<b>.<bit>` | 仅 `bool` |
//!
//! `V` 区经库固定映射到 `DB1`（S7-200 V 存储区），保留支持。DB 前缀的尺寸字母不
//! 与 kind 交叉校验——协议里变量的实际宽度由请求项的 `count`（= kind 字节数）决定，
//! `DB1.DBW0` 配 `i32` 是合法的"从字地址起读 4 字节"写法（既有文档语义不变）。

use super::Kind;

/// DB 号上限（S7 协议字段 u16）。
const MAX_DB_NUMBER: u32 = u16::MAX as u32;
/// DB 内字节地址上限（请求项里是 24 位字段，`byte_address << 3 + bit`）。
const MAX_DB_BYTE: u32 = 0x7F_FFFF;
/// I/Q/M/V 区字节地址上限（库接受 u32，实际 PLC 地址空间远小于此，取 16 位）。
const MAX_AREA_BYTE: u32 = u16::MAX as u32;

/// 严格校验一个 S7 地址串是否属"插件支持的形态"，并与 `kind` 的位/非位语义对齐。
///
/// 返回 `Err(原因)` 即表示该点位/写命令**必须被拒绝**（装配期 warn 跳过该点、
/// 写命令回 422），不允许带病走到库的宽松解析器。
pub(super) fn validate_address(addr: &str, kind: Kind) -> Result<(), String> {
    let raw = addr.trim();
    if raw.is_empty() {
        return Err("地址为空".to_string());
    }
    // TIA 导出格式（%DB1.DBX0.0）：库不认 '%'，且现场复制粘贴高频，给出直接可用的提示。
    if let Some(rest) = raw.strip_prefix('%') {
        return Err(format!(
            "地址 '{raw}' 带 TIA 导出前缀 '%'（tokio-s7 不识别），请去掉前缀写成 '{rest}'"
        ));
    }
    let up = raw.to_ascii_uppercase();
    // ---- S-13：库无解析入口的区域，配置期就明确拒绝（否则运行期报怪错） ----
    if up.starts_with('T') || up.starts_with('C') {
        return Err(format!(
            "地址 '{raw}' 属 T/C（定时器/计数器）区：库按 BCD 语义按 ×4 编址取值，读出来不是工程量，暂不支持；请改用 DB 区镜像"
        ));
    }
    if up.starts_with('P') {
        return Err(format!(
            "地址 '{raw}' 属 P（外设直接访问 PI/PQ）区：tokio-s7 定义了 DirectPeripheralAccess 但无地址解析入口，不可达；请改用 I/Q 区"
        ));
    }
    if up.starts_with("AI") || up.starts_with("AQ") {
        return Err(format!(
            "地址 '{raw}' 属 S7-200 AI/AQ（模拟量）区：tokio-s7 定义了区域码但无地址解析入口，不可达；请改用 I/Q 或 DB 区镜像"
        ));
    }
    if up.starts_with("SM") {
        return Err(format!(
            "地址 '{raw}' 属 S7-200 SM（特殊存储器）区：tokio-s7 无地址解析入口，不可达"
        ));
    }
    if up.starts_with("DI") {
        return Err(format!(
            "地址 '{raw}' 属 DI（背景数据块）区：库会把 'DI' 当成 'D' 解析成普通 DB 号，语义不符；请写成对应的 DB<n>"
        ));
    }

    let (_byte, bit) = if up.starts_with("DB") {
        parse_db_form(&up, raw)?
    } else if up.starts_with('D') {
        return Err(format!(
            "地址 '{raw}' 缺少 DB 前缀或尺寸字母：请写成 DB<n>.DBB<b> / DB<n>.DBW<b> / DB<n>.DBD<b> / DB<n>.DBX<b>.<bit>"
        ));
    } else {
        parse_area_form(&up, raw)?
    };

    match (bit, matches!(kind, Kind::Bool)) {
        (Some(_), true) | (None, false) => Ok(()),
        (Some(b), false) => Err(format!(
            "地址 '{raw}' 是位地址（bit {b}），但 kind 不是 bool：库会丢弃位段按字节读，静默读错地址；请把 kind 改成 bool"
        )),
        (None, true) => Err(format!(
            "地址 '{raw}' 不是位地址，但 kind=bool：bool 需要位形态（DB<n>.DBX<b>.<bit> / M<b>.<bit>）"
        )),
    }
}

/// 解析 `DB<n>.DBX<b>.<bit>` / `DB<n>.DBB|DBW|DBD<b>`，返回 `(byte, Option<bit>)`。
fn parse_db_form(up: &str, raw: &str) -> Result<(u32, Option<u32>), String> {
    let parts: Vec<&str> = up.split('.').collect();
    if parts.len() < 2 {
        return Err(format!(
            "地址 '{raw}' 不完整：请写成 DB<n>.DBB<b> / DB<n>.DBW<b> / DB<n>.DBD<b> / DB<n>.DBX<b>.<bit>"
        ));
    }
    let db = parse_num(&parts[0][2..]).map_err(|e| format!("地址 '{raw}' 的 DB 号非法: {e}"))?;
    if db > MAX_DB_NUMBER {
        return Err(format!(
            "地址 '{raw}' 的 DB 号 {db} 超上限 {MAX_DB_NUMBER}（协议字段 u16）"
        ));
    }
    // 第二段必须是 `DB<尺寸字母><数字>`（DBX/DBB/DBW/DBD），否则是库里"纯装饰前缀
    // 被忽略"的老写法（`DB1.0` / `DB1.X0.0` 都会被库按字节地址静默解析）。
    let seg = parts[1].strip_prefix("DB").ok_or_else(|| {
        format!(
            "地址 '{raw}' 第二段 '{0}' 缺少 DB 尺寸字母前缀：请写成 DB{n}.DBB<b> / DB{n}.DBW<b> / DB{n}.DBD<b> / DB{n}.DBX<b>.<bit>（{0} 会被库静默当成字节地址）",
            parts[1],
            n = db
        )
    })?;
    let size = seg
        .chars()
        .next()
        .filter(|c| matches!(c, 'X' | 'B' | 'W' | 'D'))
        .ok_or_else(|| {
            format!(
                "地址 '{raw}' 第二段 '{0}' 的尺寸字母非法：只支持 DBX/DBB/DBW/DBD（如 DB{n}.DBD0）",
                parts[1],
                n = db
            )
        })?;
    let byte = parse_num(&seg[size.len_utf8()..])
        .map_err(|e| format!("地址 '{raw}' 的字节地址非法: {e}"))?;
    if byte > MAX_DB_BYTE {
        return Err(format!(
            "地址 '{raw}' 的字节地址 {byte} 超上限 {MAX_DB_BYTE}（请求项 24 位地址字段）"
        ));
    }
    if size == 'X' {
        let bit_part = parts.get(2).ok_or_else(|| {
            format!("地址 '{raw}' 是 DBX 形态但缺位号：请写成 DB{db}.DBX{byte}.<bit>")
        })?;
        if parts.len() > 3 {
            return Err(format!(
                "地址 '{raw}' 段数过多：DBX 形态应为 DB<n>.DBX<b>.<bit>"
            ));
        }
        let bit = parse_bit(bit_part).map_err(|e| format!("地址 '{raw}' 的位号非法: {e}"))?;
        return Ok((byte, Some(bit)));
    }
    if parts.len() > 2 {
        return Err(format!(
            "地址 '{raw}' 段数过多：DBB/DBW/DBD 形态不带位号（位地址请用 DBX，如 DB{db}.DBX{byte}.0）"
        ));
    }
    Ok((byte, None))
}

/// 解析 `I/Q/M/V` 区：`<A>[B|W|D]<b>`（非位）或 `<A><b>.<bit>`（位），返回 `(byte, Option<bit>)`。
fn parse_area_form(up: &str, raw: &str) -> Result<(u32, Option<u32>), String> {
    let parts: Vec<&str> = up.split('.').collect();
    let head = parts[0];
    let mut chars = head.chars();
    let area = chars.next().unwrap_or('\0');
    if !matches!(area, 'I' | 'Q' | 'M' | 'V') {
        return Err(format!(
            "地址 '{raw}' 的区域前缀 '{area}' 不受支持：插件只支持 DB 区与 I/Q/M/V 区（位形态 <area><byte>.<bit>，其余 <area>[BWD]<byte>）"
        ));
    }
    let rest = &head[area.len_utf8()..];
    let num = match rest.chars().next() {
        Some(c @ ('B' | 'W' | 'D')) => &rest[c.len_utf8()..],
        _ => rest,
    };
    let byte = parse_num(num).map_err(|e| format!("地址 '{raw}' 的字节地址非法: {e}"))?;
    if byte > MAX_AREA_BYTE {
        return Err(format!(
            "地址 '{raw}' 的字节地址 {byte} 超上限 {MAX_AREA_BYTE}"
        ));
    }
    if parts.len() > 2 {
        return Err(format!(
            "地址 '{raw}' 段数过多：位形态应为 <area><byte>.<bit>"
        ));
    }
    match parts.get(1) {
        Some(b) => {
            let bit = parse_bit(b).map_err(|e| format!("地址 '{raw}' 的位号非法: {e}"))?;
            Ok((byte, Some(bit)))
        }
        None => Ok((byte, None)),
    }
}

/// 解析纯十进制数字段（空串/含非数字 → Err）。
fn parse_num(s: &str) -> Result<u32, String> {
    if s.is_empty() {
        return Err("缺数字段".to_string());
    }
    if !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("'{s}' 含非数字字符"));
    }
    s.parse::<u32>().map_err(|e| format!("'{s}' 溢出: {e}"))
}

/// 解析位号（0-7）。
fn parse_bit(s: &str) -> Result<u32, String> {
    let b = parse_num(s)?;
    if b > 7 {
        return Err(format!("{b} 超 0-7"));
    }
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_db_forms() {
        // 位形态只配 bool
        assert!(validate_address("DB1.DBX0.0", Kind::Bool).is_ok());
        assert!(validate_address("db100.dbx8191.7", Kind::Bool).is_ok());
        // 数值/字符串/raw 用 DBB/DBW/DBD（尺寸字母不与 kind 交叉校验，宽度由 kind 决定）
        for k in [
            Kind::Byte,
            Kind::UByte,
            Kind::I16,
            Kind::U64,
            Kind::F32,
            Kind::F64,
            Kind::String,
            Kind::WString,
            Kind::Raw(4),
        ] {
            assert!(validate_address("DB1.DBB0", k).is_ok(), "DBB0 + {k:?}");
            assert!(validate_address("DB1.DBW2", k).is_ok(), "DBW2 + {k:?}");
            assert!(validate_address("DB1.DBD4", k).is_ok(), "DBD4 + {k:?}");
        }
    }

    #[test]
    fn accepts_area_forms() {
        // I/Q/M/V：位形态
        for a in ["I0.0", "Q1.7", "M10.5", "V100.3"] {
            assert!(validate_address(a, Kind::Bool).is_ok(), "{a}");
        }
        // I/Q/M/V：非位形态（可带 B/W/D 尺寸字母）
        for a in ["I0", "QW4", "M10", "MW10", "MD10", "MB10", "VW100", "VD100"] {
            assert!(validate_address(a, Kind::U16).is_ok(), "{a}");
        }
    }

    #[test]
    fn rejects_tia_percent_prefix_with_hint() {
        let e = validate_address("%DB1.DBX0.0", Kind::Bool).unwrap_err();
        assert!(e.contains("去掉前缀"), "应提示去掉 % (得到: {e})");
        assert!(e.contains("DB1.DBX0.0"), "应给出可用形态 (得到: {e})");
    }

    #[test]
    fn rejects_bit_address_with_numeric_kind() {
        // S-12 主诉：DB1.DBX0.5 + u16 库会静默读 DB1.DBW0
        let e = validate_address("DB1.DBX0.5", Kind::U16).unwrap_err();
        assert!(e.contains("位地址"), "应指出是位地址 (得到: {e})");
        assert!(e.contains("bool"), "应提示改 bool (得到: {e})");
        assert!(validate_address("M10.5", Kind::I32).is_err());
        assert!(validate_address("I0.0", Kind::Raw(2)).is_err());
    }

    #[test]
    fn rejects_bool_kind_without_bit() {
        assert!(validate_address("DB1.DBB0", Kind::Bool).is_err());
        assert!(validate_address("M10", Kind::Bool).is_err());
    }

    #[test]
    fn rejects_timer_counter_areas() {
        for a in ["T5", "C3", "T0.1"] {
            let e = validate_address(a, Kind::Bool).unwrap_err();
            assert!(e.contains("T/C"), "{a} 应报 T/C 区 (得到: {e})");
        }
    }

    #[test]
    fn rejects_unsupported_areas_with_explicit_reason() {
        for (a, needle) in [
            ("PIW256", "P（外设"),
            ("PQW0", "P（外设"),
            ("PB10", "P（外设"),
            ("AIW0", "AI/AQ"),
            ("AQW4", "AI/AQ"),
            ("SM0.0", "SM"),
            ("DI5.DBX0.0", "背景数据块"),
        ] {
            let e = validate_address(a, Kind::U16).unwrap_err();
            assert!(e.contains(needle), "{a} 应含 '{needle}' (得到: {e})");
        }
    }

    #[test]
    fn rejects_missing_size_letters_and_bad_numbers() {
        // 老写法 DB1.0（无尺寸字母）：库把数字过滤后当字节地址 → 拒绝并给提示
        let e = validate_address("DB1.0", Kind::U16).unwrap_err();
        assert!(e.contains("尺寸字母"), "得到: {e}");
        // DB1.X0.0（非 DBX）同样拒
        assert!(validate_address("DB1.X0.0", Kind::Bool).is_err());
        // 位号越界 / DB 号超限 / 字节地址超限
        assert!(validate_address("DB1.DBX0.8", Kind::Bool).is_err());
        assert!(validate_address("DB65536.DBB0", Kind::Byte).is_err());
        assert!(validate_address("DB1.DBB8388608", Kind::Byte).is_err());
        // 段数过多 / 空地址 / 未知前缀
        assert!(validate_address("DB1.DBB0.1", Kind::Byte).is_err());
        assert!(validate_address("", Kind::Byte).is_err());
        assert!(validate_address("   ", Kind::Byte).is_err());
        assert!(validate_address("XYZ1", Kind::Byte).is_err());
        // DB 号缺失 / 非数字
        assert!(validate_address("DB.DBB0", Kind::Byte).is_err());
        assert!(validate_address("DBx.DBB0", Kind::Byte).is_err());
    }
}
