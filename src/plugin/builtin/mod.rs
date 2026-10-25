//! 内置插件实现。通过 `inventory` 静态注册到 `plugin::registry`。

pub mod data_sim;
#[cfg(feature = "eip")]
pub mod eip_client;
#[cfg(feature = "lua")]
pub mod lua_client;
#[cfg(feature = "lua")]
pub mod lua_north;
#[cfg(feature = "lua")]
pub mod lua_transform;
#[cfg(feature = "modbus")]
pub mod modbus_client;
#[cfg(feature = "opcua")]
pub mod opcua_client;
#[cfg(feature = "s7")]
pub mod s7_client;
pub mod self_monitor;
pub mod sidecar_client;
