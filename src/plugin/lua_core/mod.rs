//! Lua 脚本系统公共层（设计 §5.0）：沙箱引擎 / 受控加载 / 命名空间路由 / ctx 能力注册 / IO 引擎。
//! 薄壳插件（builtin/lua_client 等）按角色组装本层。
pub(crate) mod api;
pub(crate) mod budget;
pub(crate) mod capability;
pub(crate) mod config;
pub(crate) mod engine;
pub(crate) mod hotload;
pub(crate) mod io_engine;
pub(crate) mod loader;
pub(crate) mod manifest;
/// R2 频控（通用件，阶段 D 起双消费者）：http 能力（`api/http.rs`，lua-http 门控）
/// 与告警 fire_event（`api/alarm.rs`，lua 门控 20/s）共用——纯 std 实现（原子计数
/// + Mutex 窗口），不随任一上层 feature 门控（阶段 D T4 复用时解除原 lua-http
/// 门控，否则 minimal 组合编译失败，E0432 实证）。
pub(crate) mod ratelimit;
pub(crate) mod reload;
pub(crate) mod script;
pub(crate) mod stdlib;
pub(crate) mod stdlib_sync;
