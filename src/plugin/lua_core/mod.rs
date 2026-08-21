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
/// R2 频控（lua-http 门控对齐，阶段 A 审查修复）：RateLimiter 的唯一消费者是
/// `api/http.rs`（http_get/post），裸 lua 构建下编译它会成 dead_code 告警群。
#[cfg(feature = "lua-http")]
pub(crate) mod ratelimit;
pub(crate) mod reload;
pub(crate) mod script;
pub(crate) mod stdlib;
pub(crate) mod stdlib_sync;
