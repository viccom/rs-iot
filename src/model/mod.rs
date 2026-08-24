//! 设备模型注册表域（物模型；决策见 docs/decisions/2026-08-25-device-model-registry.md）。
//!
//! 高内聚域模块：参数化地址（addr）/ 属性合并（effective）/ 类型与校验
//! （types）/ 引用完整性（validate）/ lux 存储（store）/ 绑定展开（expand，
//! M-2 装配期）/ 一次性迁移（migrate）。
//! 依赖方向单向向下：types ← addr/effective/validate ← store/expand/migrate。
//!
//! 数据面（pipeline/rtdata/`tag_key`/dev2inst）与本域正交——决策 §5 不破坏性
//! 声明：模型化只发生在配置与元数据面。

pub mod addr;
pub mod cache;
pub mod effective;
pub mod expand;
pub mod migrate;
pub mod store;
pub mod types;
pub mod validate;
