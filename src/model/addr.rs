//! 参数化地址解析（纯字符串→字符串；计划 §一.2 最小集语法）。
//!
//! 模板语法两要素：
//! - `{name}` 占位符：替换为设备实例化参数值；
//! - `+N` 数字后缀偏移：仅当**紧随占位符替换结果之后**且替换值为整数时生效
//!   （`holding:{base}+2:f32` + `{base:100}` → `holding:102:f32`）。
//!
//! resolve 是模型层唯一地址加工点：各插件把结果喂给**现有**地址解析器
//! （modbus `PointSpec` 等）——插件地址解析器零改动，模型层不感知协议。

use std::collections::BTreeMap;
use std::fmt;

/// 地址模板解析错误。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AddrError {
    /// 模板占位符在参数表中不存在（缺参/未知参数同源）。
    MissingParam(String),
    /// 模板格式非法（未闭合 `{`、空占位符名、嵌套 `{`、偏移溢出）。
    Malformed(String),
    /// 占位符替换值非整数却携带 `+N` 偏移。
    NonNumericOffset {
        /// 占位符名。
        name: String,
        /// 替换值原样。
        value: String,
    },
}

impl fmt::Display for AddrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingParam(name) => write!(f, "地址模板引用了未提供的参数 {{{name}}}"),
            Self::Malformed(reason) => write!(f, "地址模板格式非法：{reason}"),
            Self::NonNumericOffset { name, value } => {
                write!(f, "参数 {{{name}}} 的值 {value:?} 非整数，不能施加 +N 偏移")
            }
        }
    }
}

impl std::error::Error for AddrError {}

/// 参数化地址模板 → 绝对地址串（纯函数）。
///
/// - `{name}` 替换；`+N` 仅对紧邻占位符的整数替换结果生效；
/// - 无占位符 → 原样返回；多余参数忽略（模型级参数可被多个 tag 模板部分使用）；
/// - 占位符缺参 / 非整数值配偏移 / 模板括号非法 → [`AddrError`]。
pub fn resolve(template: &str, params: &BTreeMap<String, String>) -> Result<String, AddrError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let close = after
            .find('}')
            .ok_or_else(|| AddrError::Malformed("占位符未闭合（缺少 '}'）".into()))?;
        let name = &after[..close];
        if name.is_empty() {
            return Err(AddrError::Malformed("占位符名为空".into()));
        }
        if name.contains('{') {
            return Err(AddrError::Malformed("占位符嵌套 '{'".into()));
        }
        let value = params
            .get(name)
            .ok_or_else(|| AddrError::MissingParam(name.to_string()))?;
        let tail = &after[close + 1..];
        match offset_suffix(tail) {
            Some(digits) => {
                let offset: i64 = digits
                    .parse()
                    .map_err(|_| AddrError::Malformed(format!("偏移量 +{digits} 超出整数范围")))?;
                let base: i64 = value.parse().map_err(|_| AddrError::NonNumericOffset {
                    name: name.to_string(),
                    value: value.to_string(),
                })?;
                let summed = base
                    .checked_add(offset)
                    .ok_or_else(|| AddrError::Malformed("偏移结果溢出整数范围".into()))?;
                out.push_str(&summed.to_string());
                rest = &tail[1 + digits.len()..];
            }
            None => {
                out.push_str(value);
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// 提取模板全部占位符名（保持出现顺序，不去重；括号合法性校验与 [`resolve`]
/// 同口径）。`validate` 用其检查 addr 占位符 ⊆ 模型 params。
pub(crate) fn placeholders(template: &str) -> Result<Vec<String>, AddrError> {
    let mut names = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let close = after
            .find('}')
            .ok_or_else(|| AddrError::Malformed("占位符未闭合（缺少 '}'）".into()))?;
        let name = &after[..close];
        if name.is_empty() {
            return Err(AddrError::Malformed("占位符名为空".into()));
        }
        if name.contains('{') {
            return Err(AddrError::Malformed("占位符嵌套 '{'".into()));
        }
        names.push(name.to_string());
        rest = &after[close + 1..];
    }
    Ok(names)
}

/// `tail` 是否以 `+<digits>` 开头：是则返回数字段（不含 `+`），否则 `None`
/// （`+` 后无数字 = 字面量加号，不构成偏移语法）。
fn offset_suffix(tail: &str) -> Option<&str> {
    let bytes = tail.as_bytes();
    if bytes.first() != Some(&b'+') {
        return None;
    }
    let mut end = 1;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == 1 {
        None
    } else {
        Some(&tail[1..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn replaces_placeholder_with_param_value() {
        let p = params(&[("base", "100")]);
        assert_eq!(
            resolve("holding:{base}:f32", &p).unwrap(),
            "holding:100:f32"
        );
    }

    #[test]
    fn applies_numeric_offset_suffix() {
        // 计划 §一.2 标准例：holding:{base}+2:f32 + base=100。
        let p = params(&[("base", "100")]);
        assert_eq!(
            resolve("holding:{base}+2:f32", &p).unwrap(),
            "holding:102:f32"
        );
    }

    #[test]
    fn multi_digit_offset() {
        let p = params(&[("base", "5")]);
        assert_eq!(resolve("{base}+10", &p).unwrap(), "15");
    }

    #[test]
    fn negative_base_value_offsets() {
        let p = params(&[("base", "-5")]);
        assert_eq!(resolve("{base}+2", &p).unwrap(), "-3");
    }

    #[test]
    fn missing_param_is_err() {
        let p = params(&[]);
        assert_eq!(
            resolve("holding:{base}:f32", &p),
            Err(AddrError::MissingParam("base".into()))
        );
    }

    #[test]
    fn unknown_placeholder_is_err() {
        // 占位符名不在参数表（与缺参同错误源，测试各自锁定）。
        let p = params(&[("base", "1")]);
        assert_eq!(
            resolve("{foo}", &p),
            Err(AddrError::MissingParam("foo".into()))
        );
    }

    #[test]
    fn non_numeric_value_with_offset_is_err() {
        let p = params(&[("p", "temp")]);
        assert_eq!(
            resolve("ns=2;s={p}+2", &p),
            Err(AddrError::NonNumericOffset {
                name: "p".into(),
                value: "temp".into()
            })
        );
    }

    #[test]
    fn template_without_placeholder_returned_as_is() {
        let p = params(&[("base", "100")]);
        assert_eq!(resolve("holding:0:f32", &p).unwrap(), "holding:0:f32");
    }

    #[test]
    fn mixed_multiple_placeholders() {
        // S7 场景：DB 号替换 + 数据区基地址偏移。
        let p = params(&[("db", "1"), ("base", "100")]);
        assert_eq!(resolve("DB{db}.DBD{base}+4", &p).unwrap(), "DB1.DBD104");
    }

    #[test]
    fn opcua_ns_prefix_pattern() {
        let p = params(&[("p", "temp")]);
        assert_eq!(resolve("ns=2;s={p}ua", &p).unwrap(), "ns=2;s=tempua");
    }

    #[test]
    fn extra_unused_params_allowed() {
        // 设备 params ⊇ 模板占位符集合（同模型参数被不同 tag 模板部分使用）。
        let p = params(&[("db", "1"), ("base", "100")]);
        assert_eq!(resolve("{base}", &p).unwrap(), "100");
    }

    #[test]
    fn unclosed_brace_is_err() {
        let p = params(&[]);
        assert!(matches!(resolve("DB{db", &p), Err(AddrError::Malformed(_))));
    }

    #[test]
    fn empty_placeholder_name_is_err() {
        let p = params(&[]);
        assert!(matches!(resolve("{}", &p), Err(AddrError::Malformed(_))));
    }

    #[test]
    fn lone_closing_brace_is_literal() {
        // 不构成占位符的 `}` 原样保留（防御地址串天然含 `}` 的场景）。
        let p = params(&[]);
        assert_eq!(resolve("a}b", &p).unwrap(), "a}b");
    }

    #[test]
    fn plus_without_digits_is_literal() {
        // `+` 后非数字：不构成偏移语法，原样保留（替换仍发生）。
        let p = params(&[("p", "x")]);
        assert_eq!(resolve("{p}+ua", &p).unwrap(), "x+ua");
    }

    #[test]
    fn empty_template_resolves_empty() {
        let p = params(&[]);
        assert_eq!(resolve("", &p).unwrap(), "");
    }

    #[test]
    fn placeholders_extracts_names_in_order_with_offsets() {
        assert_eq!(
            placeholders("DB{db}.DBD{base}+4 and {db} again").unwrap(),
            vec!["db".to_string(), "base".to_string(), "db".to_string()],
            "偏移语法不进占位符名，重复保留出现顺序"
        );
        assert_eq!(
            placeholders("no placeholder").unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn placeholders_malformed_is_err() {
        assert!(placeholders("holding:{base").is_err());
        assert!(placeholders("{}").is_err());
    }
}
