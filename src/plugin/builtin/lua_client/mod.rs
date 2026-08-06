//! `luaClient`：Lua 5.4 动态脚本采集 Source（MVP：最小采集闭环）。
//!
//! 实例加载 `scripts/<name>/main.lua`，脚本定义 `collect(ctx)`/`on_write(ctx,cmd)`。
//! 详见 `docs/superpowers/specs/2026-08-06-lua-script-collector-design.md`。

mod config;

use crate::host::HostContext;
use crate::plugin::traits::Source;
use crate::types::{CtlMessage, InstConfig, Sample};
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Lua 脚本采集 Source（`luaClient`）。
#[derive(Debug, Default)]
pub struct LuaClient;

#[async_trait]
impl Source for LuaClient {
    fn name(&self) -> &'static str {
        "luaClient"
    }

    async fn run(
        &self,
        _ctx: HostContext,
        inst: InstConfig,
        _tx: mpsc::Sender<Sample>,
        mut cmd_rx: mpsc::Receiver<CtlMessage>,
        token: CancellationToken,
    ) {
        // 骨架：Task 6 填充主循环。先保证注册可被 find_source 命中。
        tracing::info!("[luaClient] inst {} stub run", inst.inst_id);
        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                _ = cmd_rx.recv() => {}
            }
        }
    }
}

// 静态注册到 inventory（编译期，编进单一二进制）
inventory::submit! {
    crate::plugin::registry::SourceEntry {
        name: "luaClient",
        create: || Box::new(LuaClient),
    }
}

#[cfg(test)]
mod tests {
    use crate::plugin::registry::find_source;

    #[test]
    fn lua_client_registered() {
        assert!(find_source("luaClient").is_some(), "luaClient 应静态注册");
        assert!(find_source("nope").is_none());
    }
}
