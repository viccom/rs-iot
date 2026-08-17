//! rhai 脚本转换 Processor。
//!
//! 把 `sample.values` 注入为 rhai 变量（变量名取 tag 末段或整体，非法字符替换为 `_`），
//! 运行用户脚本，再把变量回读为新的 `SampleValue`。
//!
//! 示例脚本（温度华氏→摄氏）：
//! ```text
//! temp = (temp - 32) * 5 / 9
//! ```
//! 说明：脚本中可任意读写这些变量；未在 values 中出现的新变量被忽略。

use crate::plugin::traits::Processor;
use crate::types::{Sample, SampleValue};
use async_trait::async_trait;
use rhai::{Dynamic, Engine, Scope};

/// rhai 脚本 Processor：用用户脚本转换 `sample.values`。
#[derive(Debug)]
pub struct RhaiProcessor {
    script: String,
}

impl RhaiProcessor {
    /// 用脚本字符串构造 Processor。
    pub fn new(script: impl Into<String>) -> Self {
        Self {
            script: script.into(),
        }
    }
}

fn var_name(tag: &str) -> String {
    // rhai 标识符：字母/下划线开头，字母数字下划线。取末段并替换非法字符。
    let last = tag.rsplit(['.', '/', ':', '-']).next().unwrap_or(tag);
    let mut s = String::with_capacity(last.len());
    for c in last.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            s.push(c);
        } else {
            s.push('_');
        }
    }
    if s.is_empty() || s.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(true) {
        s.insert(0, 'v');
    }
    s
}

fn dynamic_to_value(d: Dynamic) -> SampleValue {
    if let Ok(f) = d.as_float() {
        return SampleValue::Float(f);
    }
    if let Ok(i) = d.as_int() {
        return SampleValue::Int(i);
    }
    if let Ok(b) = d.as_bool() {
        return SampleValue::Bool(b);
    }
    if d.is_string() {
        return SampleValue::Str(d.into_string().unwrap_or_default());
    }
    SampleValue::Str(format!("{d}"))
}

#[async_trait]
impl Processor for RhaiProcessor {
    async fn process(&self, mut sample: Sample) -> Option<Sample> {
        let engine = Engine::new();
        let mut scope = Scope::new();

        // 注入：每个 value 作为 rhai 变量
        let names: Vec<String> = sample.values.keys().map(|k| var_name(k)).collect();
        for (name, v) in names.iter().zip(sample.values.values()) {
            match v {
                SampleValue::Float(f) => {
                    scope.push(name, *f);
                }
                SampleValue::Int(i) => {
                    scope.push(name, *i);
                }
                SampleValue::Bool(b) => {
                    scope.push(name, *b);
                }
                SampleValue::Str(s) => {
                    scope.push(name, s.clone());
                }
            }
        }

        // eval：返回值丢弃（用户用赋值改变量）；出错则原样返回
        let _: Dynamic = match engine.eval_with_scope(&mut scope, &self.script) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("[rhai] script error: {e}");
                return Some(sample);
            }
        };

        // 回读
        let mut new_values = std::collections::BTreeMap::new();
        for (k, v) in &sample.values {
            let name = var_name(k);
            match scope.get_value::<Dynamic>(&name) {
                Some(d) => new_values.insert(k.clone(), dynamic_to_value(d)),
                None => new_values.insert(k.clone(), v.clone()),
            };
        }
        sample.values = new_values;
        Some(sample)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn rhai_converts_fahrenheit_to_celsius() {
        let p = RhaiProcessor::new("temp = (temp - 32) * 5 / 9");
        let mut values = BTreeMap::new();
        values.insert("temp".to_string(), SampleValue::Float(212.0));
        let s = Sample {
            hwid: "h".into(),
            devid: "d".into(),
            values,
            timestamp: 0,
        };
        let out = p.process(s).await.unwrap();
        match out.values.get("temp") {
            Some(SampleValue::Float(f)) => assert!((f - 100.0).abs() < 0.1, "got {f}"),
            other => panic!("expected float, got {other:?}"),
        }
    }
}
