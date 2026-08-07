//! 内置插件实现。通过 `inventory` 静态注册到 `plugin::registry`。

pub mod data_sim;
#[cfg(feature = "lua")]
pub mod lua_client;
#[cfg(feature = "modbus")]
pub mod modbus_client;
pub mod mqtt_pub;
#[cfg(feature = "opcua")]
pub mod opcua_client;
#[cfg(feature = "s7")]
pub mod s7_client;
pub mod sidecar_client;
