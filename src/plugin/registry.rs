//! 插件注册表：基于 `inventory` 在编译期静态注册（对标 Go 的匿名 import +
//! `init()`）。最终插件编进单一二进制，零运行时加载开销。

use super::traits::{Sink, Source};
use inventory::{collect, iter};

/// 一个南向插件（Source）的注册项。
#[derive(Debug)]
pub struct SourceEntry {
    /// 插件名（`app_code`）。
    pub name: &'static str,
    /// 工厂函数：构造插件实例。
    pub create: fn() -> Box<dyn Source>,
}
collect!(SourceEntry);

/// 一个北向插件（Sink）的注册项。
#[derive(Debug)]
pub struct SinkEntry {
    /// 插件名（`app_code`）。
    pub name: &'static str,
    /// 工厂函数：构造插件实例。
    pub create: fn() -> Box<dyn Sink>,
}
collect!(SinkEntry);

/// 按 `app_code` 查找 Source 工厂。
#[must_use]
pub fn find_source(name: &str) -> Option<fn() -> Box<dyn Source>> {
    iter::<SourceEntry>()
        .find(|e| e.name == name)
        .map(|e| e.create)
}

/// 按 `app_code` 查找 Sink 工厂。
#[must_use]
pub fn find_sink(name: &str) -> Option<fn() -> Box<dyn Sink>> {
    iter::<SinkEntry>()
        .find(|e| e.name == name)
        .map(|e| e.create)
}

/// 所有已注册 Source 名（供 `/api/v1/plugins` 展示）。
#[must_use]
pub fn source_names() -> Vec<&'static str> {
    iter::<SourceEntry>().map(|e| e.name).collect()
}

/// 所有已注册 Sink 名。
#[must_use]
pub fn sink_names() -> Vec<&'static str> {
    iter::<SinkEntry>().map(|e| e.name).collect()
}
