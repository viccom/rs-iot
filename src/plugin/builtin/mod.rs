//! 内置插件实现。通过 `inventory` 静态注册到 `plugin::registry`。

pub mod data_sim;
#[cfg(feature = "lua")]
pub mod lua_client;
pub mod modbus_client;
pub mod mqtt_pub;
pub mod opcua_client;
pub mod s7_client;
pub mod sidecar_client;
