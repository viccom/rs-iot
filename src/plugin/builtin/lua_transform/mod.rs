//! `luaTransform`：transform 角色 Lua 薄壳（Q19 三薄插件之三，Source 订阅型）。
//!
//! C 阶段 T6 先落配置层（[`config`]：TransformCfg + subscribe 模式匹配，Q6 宿主
//! 过滤）；T7 填 Source 主循环（EventBus 订阅 → 模式过滤 → `on_rtdata` 逐点回调 +
//! `on_timer` 周期回调 + 热加载 + stat）。

mod config;
