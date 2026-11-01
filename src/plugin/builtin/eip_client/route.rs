//! 点位静态分类（配置期一次判定）：读路径分流 / CIP 类型支持表 / 位路径校验 /
//! 数组范围后缀解析。
//!
//! 台账（`docs/designs/2026-11-01-protocol-semantics-audit.md` §6）对应关系：
//! - **E-2** `0xD3` 兼作 `BOOL_ARRAY_DWORD` 与 DWORD 类型码：dword 数组元素配置期拒绝；
//! - **E-3** WORD/LWORD/BYTE/STRING2 在库解码层无分支：配置期拒绝；
//! - **E-4** 位读仅接受 BOOL/DINT 父字、bit 必须 <32（≥32 被 TagPath 编成成员名）：
//!   配置期拒绝；
//! - **E-5** 分流口径：只有 `TagPath::Bit` 必须单点，`Program:` 作用域 / UDT 成员 /
//!   单元素数组都可进批量；
//! - **E-6** STRING 数组元素写缺结构 handle（库私有路径无法绕开）：配置期拒绝；
//! - **E-14** 点位地址尾部 `[a:b]` → 一次范围读；
//! - **E-16** 含 `:` 的 I/O tag 库不认（只认 `Program:` 前缀）：配置期拒绝。
//!
//! 上游证据根 `<REG>` = `/root/.cargo/registry/src/rsproxy.cn-e3de039b2554c837/`。

/// 点位读路径（配置期定案，运行期不再判断）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ReadRoute {
    /// 批量读（`read_tags_batch`）：标量、`Program:` 作用域、UDT 成员、多维数组。
    Batch,
    /// 位读：库批量路径只编码父字、位在客户端解析，必须单点读（E-5）。
    Bit {
        /// 父字 tag 路径。
        base: String,
        /// 位号（已校验 0..=31）。
        bit: u8,
    },
    /// 数组元素 / 范围读（E-13/E-14：按 base 归并走 `read_array_range`）。
    Array(ArraySpan),
    /// STRING 标量：走 `read_string_tag`（自定义 STRING 类型批量路径解不了）。
    StringTag,
}

/// 数组元素范围（单维；`count` = 1 即单元素 `Arr[3]`）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct ArraySpan {
    /// 数组基名（不含下标，如 `Program:Main.Arr`）。
    pub(super) base: String,
    /// 起始下标。
    pub(super) start: u32,
    /// 元素个数（≥1）。
    pub(super) count: u32,
}

/// 一次范围读的元素数量上限（防误配 `Arr[0:999999]` 打爆内存/PLC）。
pub(super) const MAX_RANGE_ELEMS: u32 = 4096;

/// 点位声明的 `type` 归类（E-2/E-3）。
///
/// 注意本仓 `ChildSpec.type` 是**通用值类型标签**（`"float"/"int"/"bool"` 等，供各插件
/// 自行解释），只有 `"string"` 与 CIP 类型名对 eipClient 有语义；其余标签不参与解码
/// （库本来就按标签读回的类型码解码），故归入 [`TypeClass::Inferred`] 而不是报错。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TypeClass {
    /// 未声明 / 通用值类型标签 → 按读回类型码解码。
    Inferred,
    /// CIP 类型名（白名单内，小写）。
    Cip(&'static str),
    /// DWORD：标量读回 0xD3 → UDINT 解码正确；数组元素与 BOOL 数组同码 → 歧义（E-2）。
    Dword,
    /// 库解码层无分支的类型名（E-3）。
    Unsupported(&'static str),
}

/// 支持的 CIP 类型名白名单（E-2/E-3；库 `values.rs` 的解码分支集合）。
const CIP_TYPES: [&str; 12] = [
    "bool", "sint", "int", "dint", "lint", "usint", "uint", "udint", "ulint", "real", "lreal",
    "string",
];

/// 声明类型归类。
pub(super) fn declared_type_class(ty: &str) -> TypeClass {
    let t = ty.trim().to_ascii_lowercase();
    if t.is_empty() {
        return TypeClass::Inferred;
    }
    if let Some(name) = CIP_TYPES.iter().copied().find(|s| *s == t) {
        return TypeClass::Cip(name);
    }
    match t.as_str() {
        // AB 类型表 0xD3 = DWORD，与 BOOL_ARRAY_DWORD 同码（<REG>rust-ethernet-ip-protocol-1.2.0/src/values.rs:19）。
        "dword" => TypeClass::Dword,
        // 以下类型码在 values.rs:74-174 无解码分支（落到 "Unsupported data type"）。
        "word" => TypeClass::Unsupported("WORD 0x00D2"),
        "lword" => TypeClass::Unsupported("LWORD 0x00D4"),
        "byte" => TypeClass::Unsupported("BYTE 0x00D1"),
        "string2" => TypeClass::Unsupported("STRING2 0x00D5"),
        // 通用值类型标签（float/int/double/...）不参与 eip 解码 → 按读回类型。
        _ => TypeClass::Inferred,
    }
}

/// 位号上限（E-4）：`TagPath` 把 ≥32 的数字段编成**成员名**
/// （<REG>rust-ethernet-ip-tag-path-1.2.0/src/lib.rs:496-514），PLC 回 `0x04 路径错误`。
const BIT_MAX_EXCLUSIVE: u8 = 32;

/// 位读必需的父字类型（<REG>rust-ethernet-ip-1.2.0/src/client.rs:1252-1266：
/// 只接受 `PlcValue::Bool` / `PlcValue::Dint`，其余 `DataTypeMismatch`）。
const BIT_BASE_TYPES: [&str; 2] = ["bool", "dint"];

/// 末段是纯数字位号 → `(父字路径, 位号)`。
///
/// 与库 `TagPath` 的口径对齐但**不做** <32 截断：≥32 也识别为"位号形态"，
/// 由 [`plan_point`] 报可读错误（否则 `MyLint.40` 会退化成一个成员名去读，
/// 现场只会看到 0x04 路径错误）。
fn parse_bit_path(path: &str) -> Option<(String, u32)> {
    let (base, last) = path.rsplit_once('.')?;
    if base.is_empty() || last.is_empty() {
        return None;
    }
    if !last.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let bit: u32 = last.parse().ok()?;
    Some((base.to_string(), bit))
}

/// 解析数组后缀：`Arr[3]` / `Arr[0:9]`（单维、方括号后无成员）。
///
/// 返回 `Ok(None)` = 不是数组路径（交库 `TagPath` 处理，如 `Arr[0][1]` 多维、
/// `Arr[0].Member` 成员）；`Err` = 后缀形态非法（越界/倒序/超上限）。
fn parse_array_path(path: &str) -> Result<Option<ArraySpan>, String> {
    let Some(close) = path.rfind(']') else {
        return Ok(None);
    };
    if close + 1 != path.len() {
        // 方括号后还有成员（`Arr[0].Member`）：复杂路径交库 TagPath（批量合法）。
        return Ok(None);
    }
    let Some(open) = path.rfind('[') else {
        return Ok(None);
    };
    if open > close {
        return Ok(None);
    }
    let base = &path[..open];
    let inner = &path[open + 1..close];
    if base.is_empty() || base.contains('[') {
        // 多维数组（`Arr[0][1]`）插件不解析，交库 TagPath。
        return Ok(None);
    }
    let (start, count) = parse_index_span(path, inner)?;
    Ok(Some(ArraySpan {
        base: base.to_string(),
        start,
        count,
    }))
}

/// `[N]` → `(N, 1)`；`[a:b]` → `(a, b-a+1)`。
fn parse_index_span(path: &str, inner: &str) -> Result<(u32, u32), String> {
    let (start_raw, count) = match inner.split_once(':') {
        None => (inner, 1),
        Some((a, b)) => {
            let start: u32 = a
                .trim()
                .parse()
                .map_err(|_| format!("点位 '{path}' 数组起始下标 '{a}' 非无符号整数"))?;
            let end: u32 = b
                .trim()
                .parse()
                .map_err(|_| format!("点位 '{path}' 数组结束下标 '{b}' 非无符号整数"))?;
            if end < start {
                return Err(format!(
                    "点位 '{path}' 数组范围倒序（{start}:{end}）：`[a:b]` 要求 a ≤ b"
                ));
            }
            let count = end - start + 1;
            return finalize_span(path, start, count);
        }
    };
    let start: u32 = start_raw
        .trim()
        .parse()
        .map_err(|_| format!("点位 '{path}' 数组下标 '{start_raw}' 非无符号整数"))?;
    finalize_span(path, start, count)
}

/// 上限校验（抽出来供两个分支复用）。
fn finalize_span(path: &str, start: u32, count: u32) -> Result<(u32, u32), String> {
    if count > MAX_RANGE_ELEMS {
        return Err(format!(
            "点位 '{path}' 范围读 {count} 个元素超过上限 {MAX_RANGE_ELEMS}（E-14），请拆分点位"
        ));
    }
    Ok((start, count))
}

/// E-5：只有位读必须单点。
///
/// `Program:` 作用域、UDT 成员（末段成员名）、单元素数组都能进批量——
/// 库批量对成员/程序路径编码正确（<REG>rust-ethernet-ip-1.2.0/src/client/batch_exec.rs:556-569），
/// 只有 `TagPath::Bit` 在批量里只编码父字、位掩码缺失（静默错数据）。
pub(super) fn needs_single_read(path: &str) -> bool {
    parse_bit_path(path).is_some()
}

/// 库"数组元素 workaround"命中条件（写路径动态判定用）。
///
/// 单维 `base[index]` 且方括号后无成员时，`read_tag`/`write_tag` 会走
/// `read_array_element_workaround` / `write_array_element_workaround`
/// （<REG>rust-ethernet-ip-1.2.0/src/client.rs:1326-1357、3282-3300）——
/// 该路径对 STRING 缺 2B 结构 handle（E-6），对 `0xD3` 会当 BOOL 数组（E-2）。
pub(super) fn hits_array_element_workaround(path: &str) -> bool {
    matches!(parse_array_path(path), Ok(Some(_)))
}

/// 去掉尾部数组后缀后的符号路径（`Program:Main.Arr[0:4]` → `Program:Main.Arr`）。
///
/// E-16 的 ':' 检查必须先剔除数组范围语法（`[a:b]` 里的冒号不是 I/O tag 分隔符）。
fn strip_array_suffix(path: &str) -> &str {
    if let Some(close) = path.rfind(']') {
        if close + 1 == path.len() {
            if let Some(open) = path.rfind('[') {
                return &path[..open];
            }
        }
    }
    path
}

/// 配置期点位定案：路径 + 声明类型 → 读路径；不合法直接 Err（实例启动失败）。
///
/// 一条点位只判一次：运行期不再做字符串/类型判断，也不再对单点重复校验。
pub(super) fn plan_point(path: &str, ty: &str) -> Result<ReadRoute, String> {
    if path.is_empty() {
        return Err("点位 tag 为空".to_string());
    }
    // E-16：库只认 `Program:` 前缀，其余含 ':' 的 I/O tag 会被整串当符号名编码
    //（<REG>rust-ethernet-ip-tag-path-1.2.0/src/lib.rs:414-435、517-534）。
    // 数组范围后缀 `[a:b]` 的冒号不算（先剔除）。
    let symbolic = strip_array_suffix(path);
    if symbolic.contains(':') && !symbolic.starts_with("Program:") {
        return Err(format!(
            "点位 '{symbolic}' 含 ':' 且非 Program: 前缀：rust-ethernet-ip 1.2.0 不支持 I/O tag 路径编码，\
             会退化成单符号段被 PLC 回 0x04（台账 E-16）；请改用控制器作用域 tag 或先在 PLC 侧建别名"
        ));
    }
    let class = declared_type_class(ty);
    if let TypeClass::Unsupported(name) = class {
        return Err(format!(
            "点位 '{path}' 的 type '{ty}' = {name}：库解码层无该类型分支（values.rs 只解 \
             C1-CB/CE/DA/A0/2A0/D3），接收后永久无值（台账 E-3）；请改声明为 udint/dint 或等上游修复"
        ));
    }
    let is_string = matches!(class, TypeClass::Cip("string"));
    let array = parse_array_path(path)?;
    // 命中库"数组元素 workaround"（单维、方括号后无成员）才受 E-2/E-6 影响：
    // `Arr[0].Member` 走 TagPath 元素+成员段编码，不经过 workaround。
    let array_workaround = array.is_some();

    // E-2：DWORD 数组元素与 BOOL 数组同用 0xD3，库会按"BOOL 数组取位"解 → 静默错值。
    if matches!(class, TypeClass::Dword) && array_workaround {
        return Err(format!(
            "点位 '{path}' 声明 dword 且是数组元素：0xD3 同时表示 DWORD 与 BOOL_ARRAY_DWORD，\
             库按 BOOL 数组取位解码（台账 E-2，上游处理中）；DWORD 数组元素暂不支持，\
             标量 DWORD 点位不受影响"
        ));
    }
    // E-6：STRING 数组元素写走库的数组 workaround，缺 2B 结构 handle（写失败/写坏）。
    if is_string && array_workaround {
        return Err(format!(
            "点位 '{path}' 是 STRING 数组元素：库的数组元素写路径用 `to_bytes()`（无类型前缀/无 \
             0x0FCE 结构 handle）且无公开正确路径（台账 E-6，等上游修复）；标量 STRING 不受影响"
        ));
    }

    if needs_single_read(path) {
        let Some((base, bit)) = parse_bit_path(path) else {
            // 不可达（needs_single_read 与 parse_bit_path 同源）；防御性退回批量
            return Ok(ReadRoute::Batch);
        };
        if bit >= u32::from(BIT_MAX_EXCLUSIVE) {
            return Err(format!(
                "点位 '{path}' 位号 {bit} ≥32：TagPath 会把它编成成员名，PLC 回 0x04 路径错误\
                 （台账 E-4）"
            ));
        }
        // E-4：库位读只解 BOOL/DINT 父字；AlarmWord.3（INT）会永久 DataTypeMismatch。
        match class {
            TypeClass::Cip(t) if BIT_BASE_TYPES.contains(&t) => {}
            TypeClass::Inferred => {}
            TypeClass::Cip(t) => {
                return Err(format!(
                    "点位 '{path}' 位读的父字声明为 '{t}'：库位读只接受 BOOL/DINT，\
                     INT/SINT/REAL 等父字会永久类型不匹配（台账 E-4）；请改走成员/整体读或声明 dint"
                ));
            }
            other => {
                return Err(format!(
                    "点位 '{path}' 位读的父字类型 {other:?} 不受支持（E-4，仅 bool/dint）"
                ));
            }
        }
        return Ok(ReadRoute::Bit {
            base,
            bit: bit as u8,
        });
    }

    if let Some(span) = array {
        return Ok(ReadRoute::Array(span));
    }
    if is_string {
        return Ok(ReadRoute::StringTag);
    }
    Ok(ReadRoute::Batch)
}

/// `read_array_range` 可解的元素类型码 → 元素字节数
/// （<REG>rust-ethernet-ip-1.2.0/src/client.rs:1713-1730 `array_element_size`）。
pub(super) fn array_element_size(code: u16) -> Option<usize> {
    match code {
        0x00C1 => Some(1), // BOOL
        0x00C2 => Some(1), // SINT
        0x00C3 => Some(2), // INT
        0x00C4 => Some(4), // DINT
        0x00C5 => Some(8), // LINT
        0x00C6 => Some(1), // USINT
        0x00C7 => Some(2), // UINT
        0x00C8 => Some(4), // UDINT
        0x00C9 => Some(8), // ULINT
        0x00CA => Some(4), // REAL
        0x00CB => Some(8), // LREAL
        _ => None,
    }
}

/// 数组类型码支持判定（E-2/E-3，运行期按读回/属性类型码决策）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ArrayKind {
    /// 基础标量数组：可 `read_array_range` 一次读 N 元素。
    Scalar,
    /// `0xD3`：BOOL 数组 / DWORD 数组同码——只能按声明类型区分，声明非 bool 时拒绝。
    AmbiguousDword,
    /// 逐元素可读（UDT / STRING 数组）：范围读无解码分支，退回 `read_tag`。
    PerElement,
    /// 库无解码分支的类型码（E-3）：该 base 全部点位跳过（Null）。
    Unsupported,
}

/// 类型码 → [`ArrayKind`]。
pub(super) fn array_kind(code: u16) -> ArrayKind {
    if array_element_size(code).is_some() {
        return ArrayKind::Scalar;
    }
    match code {
        0x00D3 => ArrayKind::AmbiguousDword,
        // UDT（0x00A0）/ AB_UDT（0x02A0）：逐元素读可以拿到原始字节。
        0x00A0 | 0x02A0 => ArrayKind::PerElement,
        // STRING（0x00CE）/ ALT_STRING（0x00DA）：范围读无解码分支，逐元素 read_tag 可解。
        0x00CE | 0x00DA => ArrayKind::PerElement,
        _ => ArrayKind::Unsupported,
    }
}

/// 类型码可读名（日志用）。
pub(super) fn type_code_name(code: u16) -> &'static str {
    match code {
        0x00C1 => "BOOL(0xC1)",
        0x00C2 => "SINT(0xC2)",
        0x00C3 => "INT(0xC3)",
        0x00C4 => "DINT(0xC4)",
        0x00C5 => "LINT(0xC5)",
        0x00C6 => "USINT(0xC6)",
        0x00C7 => "UINT(0xC7)",
        0x00C8 => "UDINT(0xC8)",
        0x00C9 => "ULINT(0xC9)",
        0x00CA => "REAL(0xCA)",
        0x00CB => "LREAL(0xCB)",
        0x00CE => "STRING(0xCE)",
        0x00DA => "ALT_STRING(0xDA)",
        0x00D1 => "BYTE(0xD1)",
        0x00D2 => "WORD(0xD2)",
        0x00D3 => "DWORD/BOOL_ARRAY(0xD3)",
        0x00D4 => "LWORD(0xD4)",
        0x00D5 => "STRING2(0xD5)",
        0x00A0 => "UDT(0xA0)",
        0x02A0 => "AB_UDT(0x2A0)",
        _ => "UNKNOWN",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// E-5：分流口径——只有位读单点，其余（Program/成员/单元素数组）都进批量。
    #[test]
    fn needs_single_read_routes_only_bit_paths() {
        assert!(
            !needs_single_read("Program:MainProgram.Tag"),
            "Program: 作用域进批量（E-5）"
        );
        assert!(!needs_single_read("MyUdt.Member"), "UDT 成员进批量（E-5）");
        assert!(!needs_single_read("Arr[3]"), "单元素数组进批量（E-5）");
        assert!(needs_single_read("Word.3"), "位读必须单点（E-5）");
        // I/O tag 形态：末段数字 → 位读口径；但配置期更早按 E-16 拒绝（见下条测试）。
        assert!(
            needs_single_read("Local:1:I.Data.0"),
            "末段纯数字按位读口径（E-5；E-16 会在启动期先拒）"
        );
        assert!(!needs_single_read("PlainTag"));
        assert!(!needs_single_read("Tag.1abc"), "非纯数字末段是成员名");
    }

    /// E-16：含 `:` 且非 `Program:` 前缀 → 配置期 Err，不得静默退化成单符号段。
    #[test]
    fn plan_point_rejects_io_tag_paths() {
        let err = plan_point("Local:1:I.Data.0", "").expect_err("I/O tag 必须拒绝");
        assert!(err.contains("E-16"), "错误需指向台账条目: {err}");
        // Program: 前缀是库唯一认的 ':' 形态 → 允许
        assert!(plan_point("Program:MainProgram.Tag", "").is_ok());
    }

    /// E-2/E-3：类型支持表——白名单外无解码分支的类型配置期 Err。
    #[test]
    fn plan_point_type_whitelist() {
        for t in [
            "bool", "sint", "int", "dint", "lint", "usint", "uint", "udint", "ulint", "real",
            "lreal", "",
        ] {
            assert!(
                plan_point("Tag", t).is_ok(),
                "type '{t}' 应在支持表内（E-2/E-3）"
            );
        }
        assert_eq!(plan_point("Tag", "string"), Ok(ReadRoute::StringTag));
        for (t, tag_id) in [
            ("word", "E-3"),
            ("LWORD", "E-3"),
            ("byte", "E-3"),
            ("string2", "E-3"),
        ] {
            let err = plan_point("Tag", t).expect_err("无解码分支的类型必须拒绝");
            assert!(
                err.contains(tag_id),
                "type '{t}' 的错误需指向台账 {tag_id}: {err}"
            );
        }
        // 通用值类型标签（其它插件惯用）不参与 eip 解码 → 放行，按读回类型解码
        assert!(plan_point("Tag", "float").is_ok());
    }

    /// E-2：dword 标量放行（0xD3 → UDINT 解码正确），dword 数组元素拒绝。
    #[test]
    fn plan_point_rejects_dword_array_elements() {
        assert_eq!(
            plan_point("DwTag", "dword"),
            Ok(ReadRoute::Batch),
            "标量 DWORD 解码为 UDINT，正确"
        );
        let err = plan_point("DwArr[3]", "dword").expect_err("dword 数组元素必须拒绝");
        assert!(err.contains("0xD3") && err.contains("E-2"), "{err}");
    }

    /// E-4：位号 0..=31 + 父字仅 bool/dint。
    #[test]
    fn plan_point_validates_bit_paths() {
        assert_eq!(
            plan_point("Word.3", "dint"),
            Ok(ReadRoute::Bit {
                base: "Word".into(),
                bit: 3
            })
        );
        // 未声明类型：无法判断父字，放行（运行期库会报具体类型不符）
        assert!(matches!(
            plan_point("Word.3", ""),
            Ok(ReadRoute::Bit { bit: 3, .. })
        ));
        let err = plan_point("Word.32", "dint").expect_err("bit≥32 必须拒绝");
        assert!(err.contains("E-4"), "{err}");
        let err = plan_point("AlarmWord.3", "int").expect_err("INT 父字位读必须拒绝");
        assert!(err.contains("E-4"), "{err}");
        let err = plan_point("AlarmWord.3", "sint").expect_err("SINT 父字位读必须拒绝");
        assert!(err.contains("E-4"), "{err}");
    }

    /// E-6：STRING 数组元素配置期拒绝；标量 STRING 不受影响。
    #[test]
    fn plan_point_rejects_string_array_elements() {
        let err = plan_point("StrArr[0]", "string").expect_err("STRING 数组元素必须拒绝");
        assert!(err.contains("E-6"), "{err}");
        assert_eq!(
            plan_point("StrTag", "string"),
            Ok(ReadRoute::StringTag),
            "标量 STRING 走专用路径（E-6 不影响）"
        );
    }

    /// E-14：`[a:b]` 范围 + `[N]` 单元素解析。
    #[test]
    fn plan_point_parses_array_spans() {
        assert_eq!(
            plan_point("Arr[3]", "dint"),
            Ok(ReadRoute::Array(ArraySpan {
                base: "Arr".into(),
                start: 3,
                count: 1
            }))
        );
        assert_eq!(
            plan_point("Program:Main.Arr[0:9]", "real"),
            Ok(ReadRoute::Array(ArraySpan {
                base: "Program:Main.Arr".into(),
                start: 0,
                count: 10
            }))
        );
        // 成员后缀 / 多维数组不归范围读（交库 TagPath 走批量）
        assert_eq!(
            plan_point("Arr[0].Member", ""),
            Ok(ReadRoute::Batch),
            "数组元素成员路径交库"
        );
        assert_eq!(
            plan_point("Arr[0][1]", ""),
            Ok(ReadRoute::Batch),
            "多维数组交库"
        );
        let err = plan_point("Arr[9:0]", "").expect_err("倒序范围必须拒绝");
        assert!(err.contains("倒序"), "{err}");
        let err = plan_point("Arr[0:99999]", "").expect_err("超上限范围必须拒绝");
        assert!(err.contains("上限"), "{err}");
        let err = plan_point("Arr[x]", "").expect_err("非数字下标必须拒绝");
        assert!(err.contains("非无符号整数"), "{err}");
    }

    /// E-13/E-14 的运行期类型码分类（0xD3 歧义 / 结构 / 无解码分支）。
    #[test]
    fn array_kind_classification() {
        assert_eq!(array_kind(0x00C4), ArrayKind::Scalar);
        assert_eq!(array_kind(0x00D3), ArrayKind::AmbiguousDword);
        assert_eq!(array_kind(0x02A0), ArrayKind::PerElement);
        assert_eq!(array_kind(0x00CE), ArrayKind::PerElement);
        assert_eq!(array_kind(0x00D2), ArrayKind::Unsupported);
        assert_eq!(array_kind(0x00D5), ArrayKind::Unsupported);
        assert_eq!(array_element_size(0x00C5), Some(8));
        assert_eq!(array_element_size(0x00D3), None);
        assert!(type_code_name(0x00D3).contains("0xD3"));
    }
}
