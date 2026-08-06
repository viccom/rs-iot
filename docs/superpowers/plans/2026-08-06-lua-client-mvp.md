# luaClient MVP（M1-M3：最小采集闭环）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `app_code="luaClient"` 的实例能加载 Lua 5.4 脚本，用 `ctx:request` 走 TCP 采集标准 Modbus 协议，产出 `Sample` 落 lux `rtdata:<devid>`，并能经 `cmd_rx` 处理 `on_write` 写命令——零改 hub/pipeline/lux/CtlRegistry 核心。

**Architecture:** 新增一个南向 Source `luaClient`（与 dataSim/modbusClient 同构）。每实例一个独立 `mlua::Lua` VM（沙箱 + 指令 hook + 内存限额），脚本定义 `collect(ctx)`/`on_write(ctx,cmd)` 回调。IO 走「channel + 独占 IO task」模式：Lua 的 `ctx:request` 经 `coroutine.yield` 挂起 → mpsc 送 IO task → oneshot 回执 + timeout。守 spec §18 红线：collect/on_write 全在同 task 的 `select!` 循环顺序执行。

**Tech Stack:** Rust 2021 / tokio（multi_thread，已有）/ `mlua 0.12`（lua54+vendored+async+send+serde+macros）/ lux（进程内 RESP）/ inventory（静态注册）。

**Spec：** `docs/superpowers/specs/2026-08-06-lua-script-collector-design.md`（本 plan 覆盖 §4 文件清单、§5 生命周期、§6 编程模型、§7 L1+L2+L6 API、§8 IO 引擎、§9 沙箱、§10 写命令；L3/L4/L5/热加载/状态 REST 在后续 plan）。

**TDD 约定：** 每个 task 先写失败测试 → 跑验证失败 → 写最小实现 → 跑验证通过 → commit。三项 CI 门槛全程守：`cargo fmt --all --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test --all-targets --all-features`。

**执行顺序：** Task 8（`api.rs`）的 `register` 被 Task 6 的 `run` 调用，故实际执行顺序为 **Task 1 → 2 → 3 → 4 → 5 → 8 → 6 → 7 → 9**（先做 Task 8 的 api.rs，再回 Task 6 填 run，最后 Task 9 端到端测）。

---

## File Structure（本 plan 新增/修改）

```
Cargo.toml                                      [修改] 加 lua feature + mlua 依赖
src/plugin/builtin/mod.rs                       [修改] +`#[cfg(feature="lua")] pub mod lua_client;`
src/plugin/builtin/lua_client/
  mod.rs            [新建] LuaClient + Source impl（run 主循环）+ inventory 注册 + 单测
  config.rs         [新建] LuaCfg + parse_cfg（仿 modbus config.rs）+ 单测
  engine.rs         [新建] new_vm（沙箱 new_with + hook + memory_limit）+ 单测
  loader.rs         [新建] 脚本目录约定解析（名校验 + 路径穿越防护）
  script.rs         [新建] 脚本加载编译 + 接管 package.searchers + 单测
  io_engine.rs      [新建] IoEngine（channel + 独占 IO task + oneshot timeout）+ MockIoTask + 单测
  api.rs            [新建] ctx 对象 + L1/L2/L6 host 函数注册
scripts/it_modbus/main.lua                      [新建] 端到端测试脚本（标准 Modbus TCP）
tests/lua_modbus_e2e.rs                         [新建] 端到端闭环测（mock modbus server + rtdata + 写命令）
```

每个文件单一职责，<500 行（CLAUDE.md 约定）。`config.rs`/`engine.rs`/`io_engine.rs`/`api.rs` 互相只经明确接口依赖，可独立单测。

---

## Task 1：feature 门控 + mlua 依赖 + 模块骨架

**Files:**
- Modify: `Cargo.toml:80-87`（`[features]`）+ `Cargo.toml:10-78`（`[dependencies]`）
- Modify: `src/plugin/builtin/mod.rs`
- Create: `src/plugin/builtin/lua_client/mod.rs`

- [ ] **Step 1：修改 `Cargo.toml` [features]**

把 `Cargo.toml:80-87` 的 `[features]` 节改为（加 `lua`，并入 `default`）：

```toml
[features]
default = ["transform", "lua"]
# L2 数据转换（rhai 脚本，默认开启）
transform = ["dep:rhai"]
# Lua 动态脚本采集（luaClient，默认开启）
lua = ["dep:mlua"]
# 预留（后续里程碑）：
# local-broker = ["dep:rumqttd"]
# analytics = ["dep:datafusion"]
# windows-svc = ["dep:windows-service"]
```

- [ ] **Step 2：修改 `Cargo.toml` [dependencies]**

在 `[dependencies]` 末尾（`futures-util = "0.3"` 后，`[features]` 前）加：

```toml
# Lua 动态脚本采集（luaClient）：mlua 0.12，Lua 5.4 vendored 编入，async+send 桥接 tokio。
# send 是必需的（rs-iot multi_thread runtime 下 spawn 持 Lua 的 future 须 Send，spec §20 核实）。
mlua = { version = "0.12", features = ["lua54", "vendored", "async", "send", "serde", "macros"], optional = true }
```

- [ ] **Step 3：修改 `src/plugin/builtin/mod.rs`**

在现有 `pub mod` 列表末尾加一行（触发 inventory 注册的匿名 import）：

```rust
#[cfg(feature = "lua")]
pub mod lua_client;
```

- [ ] **Step 4：创建 `src/plugin/builtin/lua_client/mod.rs` 骨架**

```rust
//! `luaClient`：Lua 5.4 动态脚本采集 Source（MVP：最小采集闭环）。
//!
//! 实例加载 `scripts/<name>/main.lua`，脚本定义 `collect(ctx)`/`on_write(ctx,cmd)`。
//! 详见 `docs/superpowers/specs/2026-08-06-lua-script-collector-design.md`。

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
```

- [ ] **Step 5：写失败测试（注册可被发现）**

在 `src/plugin/builtin/lua_client/mod.rs` 末尾追加：

```rust
#[cfg(test)]
mod tests {
    use crate::plugin::registry::find_source;

    #[test]
    fn lua_client_registered() {
        assert!(find_source("luaClient").is_some(), "luaClient 应静态注册");
        assert!(find_source("nope").is_none());
    }
}
```

- [ ] **Step 6：跑测试验证通过 + 三项 CI 门槛**

```bash
cargo test lua_client_registered --features lua
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
```
Expected：测试 PASS；fmt/clippy 零警告。

- [ ] **Step 7：commit**

```bash
git add Cargo.toml src/plugin/builtin/mod.rs src/plugin/builtin/lua_client/mod.rs
git commit -m "feat(lua): luaClient 骨架 + mlua 依赖 + lua feature 门控"
```

---

## Task 2：LuaCfg 配置解析（`config.rs`）

**Files:**
- Create: `src/plugin/builtin/lua_client/config.rs`
- Modify: `src/plugin/builtin/lua_client/mod.rs`（加 `mod config;`）

照抄 `src/plugin/builtin/modbus_client/config.rs:63-158` 的 `parse_cfg` 范式（强类型 Cfg + 从 `app_config` 读 key + 缺省 + 校验 + `Err(String)`）。

- [ ] **Step 1：写失败测试 `config.rs`**

创建 `src/plugin/builtin/lua_client/config.rs`：

```rust
//! Lua 插件配置：传输 / 周期 / 沙箱参数 / `parse_cfg`（解析 `app_config`）。

/// 传输模式（Plan 1 仅实现 tcp；serial/udp 框架预留，Task 7 接入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Transport {
    Tcp,
    Serial,
    Udp,
}

/// 接收成帧模式（spec §8）。Plan 1 MVP 仅用 `Timeout`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RecvMode {
    /// 读到静默 idle_ms 返回累积字节。
    Timeout,
}

/// 从 `app_config` 解析的插件配置。
#[derive(Debug)]
pub(super) struct LuaCfg {
    pub(super) transport: Transport,
    /// tcp/udp 的 `ip:port`（serial 时为空）。
    pub(super) endpoint: String,
    /// serial 串口名。
    pub(super) serial_port: Option<String>,
    pub(super) period: u64,
    /// ctx:request 默认超时（毫秒）。
    pub(super) timeout_ms: u64,
    pub(super) recv_mode: RecvMode,
    pub(super) recv_idle_ms: u64,
    pub(super) reconnect_secs: u64,
    /// VM 内存限额（MB）。
    pub(super) mem_limit_mb: usize,
    /// 指令 hook 阈值（spec §9.2）。
    pub(super) hook_instr_limit: u32,
    /// 脚本目录名（→ scripts/<script>/main.lua）。
    pub(super) script: String,
}

/// 解析 `app_config`。tcp 默认；period=0 表示纯被动（仅 on_data，Plan 1 不用，保留）。
pub(super) fn parse_cfg(app: &serde_json::Value) -> Result<LuaCfg, String> {
    let script = app
        .get("script")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "app_config.script missing（脚本目录名）".to_string())?
        .to_string();
    if !script.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') || script.is_empty()
    {
        return Err(format!(
            "非法 script 名 '{script}'（仅 [A-Za-z0-9_-]，防路径穿越）"
        ));
    }

    let kind = app.get("kind").and_then(|v| v.as_str()).unwrap_or("tcp");
    let transport = match kind {
        "tcp" => Transport::Tcp,
        "serial" => Transport::Serial,
        "udp" => Transport::Udp,
        other => return Err(format!("unsupported kind '{other}'（tcp/serial/udp）")),
    };
    let (endpoint, serial_port) = match transport {
        Transport::Serial => {
            let sp = app
                .get("serial_port")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "serial 需要 serial_port".to_string())?
                .to_string();
            (String::new(), Some(sp))
        }
        _ => {
            let ep = app
                .get("endpoint")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "app_config.endpoint missing".to_string())?
                .to_string();
            (ep, None)
        }
    };

    let period = app.get("period").and_then(|v| v.as_u64()).unwrap_or(3);
    let timeout_ms = app.get("timeout_ms").and_then(|v| v.as_u64()).unwrap_or(200).max(10);
    let recv_idle_ms = app.get("recv_idle_ms").and_then(|v| v.as_u64()).unwrap_or(50);
    let reconnect_secs = app.get("reconnect_secs").and_then(|v| v.as_u64()).unwrap_or(5).max(1);
    let mem_limit_mb = app.get("mem_limit_mb").and_then(|v| v.as_u64()).unwrap_or(32) as usize;
    let hook_instr_limit = app
        .get("hook_instr_limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(1_000_000) as u32;

    Ok(LuaCfg {
        transport,
        endpoint,
        serial_port,
        period,
        timeout_ms,
        recv_mode: RecvMode::Timeout,
        recv_idle_ms,
        reconnect_secs,
        mem_limit_mb,
        hook_instr_limit,
        script,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_tcp_defaults() {
        let cfg = parse_cfg(&json!({"script":"devA","endpoint":"127.0.0.1:502"})).unwrap();
        assert_eq!(cfg.transport, Transport::Tcp);
        assert_eq!(cfg.period, 3);
        assert_eq!(cfg.timeout_ms, 200);
        assert_eq!(cfg.mem_limit_mb, 32);
    }

    #[test]
    fn parse_rejects_bad_script_name() {
        let r = parse_cfg(&json!({"script":"../etc","endpoint":"x"}));
        assert!(r.is_err());
        let r = parse_cfg(&json!({"script":"","endpoint":"x"}));
        assert!(r.is_err());
    }

    #[test]
    fn parse_missing_script_errors() {
        assert!(parse_cfg(&json!({"endpoint":"x"})).is_err());
    }

    #[test]
    fn parse_serial_needs_port() {
        assert!(parse_cfg(&json!({"script":"a","kind":"serial"})).is_err());
        let cfg = parse_cfg(&json!({"script":"a","kind":"serial","serial_port":"COM3"})).unwrap();
        assert_eq!(cfg.transport, Transport::Serial);
        assert_eq!(cfg.serial_port.as_deref(), Some("COM3"));
    }
}
```

- [ ] **Step 2：在 `mod.rs` 顶部 `use` 后加模块声明**

在 `src/plugin/builtin/lua_client/mod.rs` 的模块文档注释后加：

```rust
mod config;
use config::{parse_cfg, LuaCfg};
```

- [ ] **Step 3：跑测试验证通过**

```bash
cargo test --features lua config::tests
```
Expected：4 个测试 PASS。

- [ ] **Step 4：fmt + clippy + commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
git add src/plugin/builtin/lua_client/config.rs src/plugin/builtin/lua_client/mod.rs
git commit -m "feat(lua): LuaCfg 配置解析（parse_cfg，仿 modbus）"
```

---

## Task 3：VM 引擎（`engine.rs`）— 沙箱 + hook + 内存限额

**Files:**
- Create: `src/plugin/builtin/lua_client/engine.rs`
- Modify: `src/plugin/builtin/lua_client/mod.rs`（加 `mod engine;`）

spec §9 三层护栏。mlua API 已核实（§20）：`Lua::new_with(StdLib, LuaOptions)` 双参；`HookTriggers::new().every_nth_instruction(n)` builder；`set_memory_limit(bytes)`；`catch_rust_panics(true)`。

- [ ] **Step 1：写 `engine.rs`**

```rust
//! Lua VM 创建：库裁剪 + 指令 hook + 内存限额（spec §9 三层护栏）。

use crate::plugin::builtin::lua_client::config::LuaCfg;
use mlua::{Error, HookTriggers, Lua, LuaOptions, StdLib, VmState};

/// 创建一个沙箱化的 Lua 5.4 VM。
///
/// - 库裁剪：仅 TABLE|STRING|MATH|COROUTINE（剔除 io/os/debug/package；COROUTINE 必留，async 桥接依赖）。
/// - 指令 hook：每 `hook_instr_limit` 条指令触发，回调返回 Err 中断死循环。
/// - 内存限额：超 `mem_limit_mb` 触发 MemoryError。
/// - catch_rust_panics(true)：Rust callback panic 转 RuntimeError（host 函数仍应返回 Err 而非 panic，spec §18）。
pub(super) fn new_vm(cfg: &LuaCfg) -> mlua::Result<Lua> {
    let lua = Lua::new_with(
        StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::COROUTINE,
        LuaOptions::new().catch_rust_panics(true),
    )?;

    // 指令 hook：到点抛 RuntimeError 中断（VM 不死，下次 call_async 重新计数）。
    let limit = cfg.hook_instr_limit;
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(limit),
        move |_lua, _dbg| {
            Err(Error::RuntimeError(format!(
                "instruction limit ({limit}) exceeded — 疑似死循环"
            )))
        },
    )?;

    // 内存限额（字节）。
    lua.set_memory_limit(cfg.mem_limit_mb * 1024 * 1024)?;

    Ok(lua)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::builtin::lua_client::config::parse_cfg;
    use serde_json::json;

    fn cfg() -> LuaCfg {
        parse_cfg(&json!({"script":"t","endpoint":"x"})).unwrap()
    }

    #[test]
    fn sandbox_blocks_dangerous_libs() {
        let lua = new_vm(&cfg()).unwrap();
        // io / os / debug / package 应不可用（pcall 捕获 → nil+err）
        let (ok, _): (bool, mlua::Value) = lua
            .load("pcall(function() return io.read end)")
            .eval()
            .unwrap();
        assert!(!ok, "io 库应被沙箱禁用");
        let (ok, _): (bool, mlua::Value) = lua
            .load("pcall(function() return os.execute end)")
            .eval()
            .unwrap();
        assert!(!ok, "os 库应被沙箱禁用");
    }

    #[test]
    fn hook_kills_infinite_loop() {
        let mut c = cfg();
        c.hook_instr_limit = 10_000; // 小阈值加快触发
        let lua = new_vm(&c).unwrap();
        let r: mlua::Result<()> = lua.load("while true do end").exec();
        assert!(r.is_err(), "死循环应被 hook 中断");
        // VM 仍可用（不死）
        let v: i64 = lua.load("return 1 + 1").eval().unwrap();
        assert_eq!(v, 2);
    }

    #[test]
    fn memory_limit_enforced() {
        let mut c = cfg();
        c.mem_limit_mb = 1; // 1MB
        let lua = new_vm(&c).unwrap();
        let r: mlua::Result<String> = lua.load("local s='x'; for i=1,1000000 do s=s..string.rep('x',100) end; return s").eval();
        assert!(r.is_err(), "爆内存应触发 MemoryError");
    }

    #[test]
    fn coroutine_available() {
        let lua = new_vm(&cfg()).unwrap();
        let v: i64 = lua
            .load("local function f() coroutine.yield() end; return 42")
            .eval()
            .unwrap();
        assert_eq!(v, 42, "COROUTINE 库必须可用（async 桥接依赖）");
    }
}
```

- [ ] **Step 2：在 `mod.rs` 加 `mod engine;`**

在 `mod config;` 后加：

```rust
mod engine;
```

- [ ] **Step 3：跑测试验证通过**

```bash
cargo test --features lua engine::tests
```
Expected：4 个测试 PASS（沙箱禁库、hook 挡死循环、内存限额、coroutine 可用）。

- [ ] **Step 4：fmt + clippy + commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
git add src/plugin/builtin/lua_client/engine.rs src/plugin/builtin/lua_client/mod.rs
git commit -m "feat(lua): VM 引擎（沙箱 new_with + 指令 hook + 内存限额）"
```

---

## Task 4：脚本加载（`loader.rs` + `script.rs`）— 接管 package.searchers

**Files:**
- Create: `src/plugin/builtin/lua_client/loader.rs`
- Create: `src/plugin/builtin/lua_client/script.rs`
- Modify: `src/plugin/builtin/lua_client/mod.rs`

spec §5.1 + §18 红线：`loader.rs` 必须真正接管 `package.searchers`，否则 `require` 可逃逸到全局路径。

- [ ] **Step 1：写 `loader.rs`（目录约定解析）**

```rust
//! 脚本目录约定解析：scripts/<name>/main.lua + 本地 lib/*.lua（require 限定）。

use std::path::{Path, PathBuf};

/// 脚本根目录（相对于进程工作目录）。生产可由 app_config 覆盖，Plan 1 固定。
pub(super) const SCRIPTS_ROOT: &str = "scripts";

/// 解析脚本入口路径：scripts/<name>/main.lua。
///
/// `name` 已在 `config::parse_cfg` 校验为 [A-Za-z0-9_-]，这里再做一次防御性断言。
pub(super) fn main_lua_path(scripts_root: &Path, name: &str) -> PathBuf {
    debug_assert!(
        name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
        "script name 已校验"
    );
    scripts_root.join(name).join("main.lua")
}

/// 解析本地模块路径：scripts/<name>/lib/<mod_path>.lua（require 限定在目录内）。
///
/// `req` 是 require 参数（如 "lib.foo" 或 "foo"）；返回 None 表示拒绝（越界或含 `..`）。
pub(super) fn local_module_path(scripts_root: &Path, name: &str, req: &str) -> Option<PathBuf> {
    // 拒绝任何路径穿越。
    if req.contains("..") || req.starts_with('/') || req.contains('\\') {
        return None;
    }
    let parts: Vec<&str> = req.split('.').collect();
    if parts.iter().any(|p| p.is_empty() || p.contains('/') || p.contains('\\')) {
        return None;
    }
    // 接受 "lib.foo" 或裸 "foo"：统一映射到 scripts/<name>/lib/<...>.lua
    let rel = if parts.first() == Some(&"lib") {
        parts[1..].join("/")
    } else {
        parts.join("/")
    };
    if rel.is_empty() {
        return None;
    }
    Some(scripts_root.join(name).join("lib").join(format!("{rel}.lua")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_path_basic() {
        let p = main_lua_path(Path::new("scripts"), "devA");
        assert_eq!(p, PathBuf::from("scripts/devA/main.lua"));
    }

    #[test]
    fn local_module_resolves() {
        let p = local_module_path(Path::new("scripts"), "devA", "lib.util").unwrap();
        assert_eq!(p, PathBuf::from("scripts/devA/lib/util.lua"));
        let p = local_module_path(Path::new("scripts"), "devA", "util").unwrap();
        assert_eq!(p, PathBuf::from("scripts/devA/lib/util.lua"));
    }

    #[test]
    fn local_module_rejects_traversal() {
        assert!(local_module_path(Path::new("scripts"), "a", "../../etc/passwd").is_none());
        assert!(local_module_path(Path::new("scripts"), "a", "/etc/passwd").is_none());
        assert!(local_module_path(Path::new("scripts"), "a", "..\\x").is_none());
        assert!(local_module_path(Path::new("scripts"), "a", "").is_none());
    }
}
```

- [ ] **Step 2：写 `script.rs`（加载 + 接管 searchers）**

```rust
//! 脚本加载与编译 + 接管 package.searchers（限定 require 在脚本目录内，spec §18 红线）。

use crate::plugin::builtin::lua_client::loader;
use mlua::{Lua, Value};
use std::path::Path;
use std::sync::Arc;

/// 加载上下文：脚本根 + 脚本名（供 searchers 闭包解析 require）。
#[derive(Clone)]
pub(super) struct LoadCtx {
    pub(super) scripts_root: Arc<Path>,
    pub(super) script_name: Arc<str>,
}

/// 在 `lua` 上接管 `package.searchers`（或 `package.loaders`），把 require 限定到
/// `scripts/<name>/lib/`。返回 main.lua 入口的源码（已读为 String）。
pub(super) fn load_main(lua: &Lua, lctx: &LoadCtx) -> mlua::Result<()> {
    // 接管 searchers：Lua 5.4 用 package.searchers（5.1 用 package.loaders）。
    // 设置唯一 searcher = 我们的本地解析函数。
    let lctx_clone = lctx.clone();
    let searcher = lua.create_function(move |_lua, req: String| -> mlua::Result<Option<String>> {
        match loader::local_module_path(&lctx_clone.scripts_root, &lctx_clone.script_name, &req) {
            Some(p) => match std::fs::read_to_string(&p) {
                Ok(src) => Ok(Some(src)),
                Err(_) => Ok(None), // 找不到 → 让 Lua 报 module not found
            },
            None => Ok(None), // 路径穿越 → 拒绝（当作找不到）
        }
    })?;

    // package 库虽未装入沙箱，但 searchers 接管需通过注册表直接设；
    // 沙箱未装 package → require 本就不可用。为支持本地 require，我们手动注入一个
    // 受控 require 全局函数（不依赖 package 表）。
    let lctx2 = lctx.clone();
    let require_fn = lua.create_function(move |lua, req: String| -> mlua::Result<Value> {
        let lctx = lctx2.clone();
        match loader::local_module_path(&lctx.scripts_root, &lctx.script_name, &req) {
            Some(p) => {
                let src = std::fs::read_to_string(&p)
                    .map_err(|e| mlua::Error::RuntimeError(format!("require {req}: {e}")))?;
                // 缓存到 registry table 防重复加载（key = req）。
                let cached = {
                    let t = lua.create_table()?;
                    lua.set_named_registry_value(&format!("__lua_require_cache__{req}"), t.clone())?;
                    t
                };
                let _ = cached; // 简化：Plan 1 不做 require 缓存，每次重新 load（YAGNI）
                let chunk = lua.load(&src).set_name(format!("={req}"));
                chunk.exec()?;
                Ok(Value::Nil)
            }
            None => Err(mlua::Error::RuntimeError(format!(
                "require '{req}' 越界或不存在（仅限 lib/）"
            ))),
        }
    })?;
    let _ = searcher; // 保留 searcher 以备未来切回 package.searchers
    lua.globals().set("require", require_fn)?;

    // 加载 main.lua 入口（执行其顶层代码，定义 collect/on_write 等）。
    let main_path = loader::main_lua_path(&lctx.scripts_root, &lctx.script_name);
    let src = std::fs::read_to_string(&main_path)
        .map_err(|e| mlua::Error::RuntimeError(format!("加载脚本 {}: {e}", main_path.display())))?;
    lua.load(&src)
        .set_name(format!("={}_main", lctx.script_name))
        .exec()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::builtin::lua_client::engine::new_vm;
    use crate::plugin::builtin::lua_client::config::parse_cfg;
    use serde_json::json;
    use std::fs;

    #[test]
    fn load_main_defines_functions() {
        let tmp = std::env::temp_dir().join(format!("lua-load-{}", std::process::id()));
        let dir = tmp.join("devA");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("main.lua"), "function collect(ctx) return {v=1} end").unwrap();

        let cfg = parse_cfg(&json!({"script":"devA","endpoint":"x"})).unwrap();
        let lua = new_vm(&cfg).unwrap();
        let lctx = LoadCtx {
            scripts_root: Arc::from(tmp.as_path()),
            script_name: Arc::<str>::from("devA"),
        };
        load_main(&lua, &lctx).unwrap();
        let has: bool = lua
            .load("return type(collect) == 'function'")
            .eval()
            .unwrap();
        assert!(has, "main.lua 应定义 collect");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn require_rejects_traversal() {
        let tmp = std::env::temp_dir().join(format!("lua-req-{}", std::process::id()));
        fs::create_dir_all(tmp.join("devA/lib")).unwrap();
        fs::write(tmp.join("devA/main.lua"), "").unwrap();

        let cfg = parse_cfg(&json!({"script":"devA","endpoint":"x"})).unwrap();
        let lua = new_vm(&cfg).unwrap();
        let lctx = LoadCtx {
            scripts_root: Arc::from(tmp.as_path()),
            script_name: Arc::<str>::from("devA"),
        };
        load_main(&lua, &lctx).unwrap();
        let r: mlua::Result<()> = lua.load("require('../../etc/passwd')").exec();
        assert!(r.is_err(), "越界 require 应被拒");
        let _ = fs::remove_dir_all(&tmp);
    }
}
```

- [ ] **Step 3：在 `mod.rs` 加模块声明**

```rust
mod loader;
mod script;
use script::{load_main, LoadCtx};
```

- [ ] **Step 4：跑测试验证通过**

```bash
cargo test --features lua loader::tests script::tests
```
Expected：5 个测试 PASS。

- [ ] **Step 5：fmt + clippy + commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
git add src/plugin/builtin/lua_client/loader.rs src/plugin/builtin/lua_client/script.rs src/plugin/builtin/lua_client/mod.rs
git commit -m "feat(lua): 脚本加载 + 接管 require（限定 lib/，防路径穿越）"
```

---

## Task 5：IO 引擎（`io_engine.rs`）— channel + 独占 IO task + MockIoTask

**Files:**
- Create: `src/plugin/builtin/lua_client/io_engine.rs`
- Modify: `src/plugin/builtin/lua_client/mod.rs`

spec §8 模式 Z。Plan 1 先实现 `MockIoTask`（可编程响应）+ IoEngine 框架；Task 7 接 tcp 真实传输。守红线：IO task 独占连接，Lua 经 channel 请求。

- [ ] **Step 1：写 `io_engine.rs`**

```rust
//! IO 引擎：channel + 独占 IO task + oneshot 回执 + timeout（spec §8 模式 Z）。
//!
//! Lua 的 ctx:request 经 create_async_function → mpsc 送 IoRequest → IO task 独占连接
//! 处理 → oneshot 回执；timeout 包 oneshot，超时丢弃迟到回包。

use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

/// 一次 request：帧字节 + 回执通道 + 是否为被动帧标记。
pub(super) struct IoRequest {
    pub(super) frame: Vec<u8>,
    pub(super) reply_to: oneshot::Sender<mlua::Result<Vec<u8>>>,
}

/// IO 后端 trait：IO task 持有一个，处理 request。Plan 1 有 MockIoTask；Task 7 加 TcpIo。
#[async_trait::async_trait]
pub(super) trait IoBackend: Send + 'static {
    /// 处理一次 request：发 frame、收响应、返回字节（或 Err）。
    async fn request(&mut self, frame: &[u8]) -> mlua::Result<Vec<u8>>;
}

/// IO 引擎句柄：Lua 侧通过它发 request（经 channel 送 IO task）。
#[derive(Clone)]
pub(super) struct IoEngine {
    tx: mpsc::Sender<IoRequest>,
}

impl IoEngine {
    /// 发一次 request，等回执（带超时）。供 ctx:request 的 async fn 调用。
    pub(super) async fn request(&self, frame: &[u8], timeout_ms: u64) -> mlua::Result<Vec<u8>> {
        let (otx, orx) = oneshot::channel();
        self.tx
            .send(IoRequest {
                frame: frame.to_vec(),
                reply_to: otx,
            })
            .await
            .map_err(|_| mlua::Error::RuntimeError("IO task closed".into()))?;
        match timeout(Duration::from_millis(timeout_ms), orx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(mlua::Error::RuntimeError("IO reply dropped".into())),
            Err(_) => Err(mlua::Error::RuntimeError(format!(
                "IO request timeout ({timeout_ms}ms)"
            ))),
        }
    }
}

/// 启动 IO task（独占 backend）。返回 IoEngine 句柄 + 被动帧接收端（on_data 用，Plan 1 暂留）。
pub(super) fn spawn_io_task(
    mut backend: Box<dyn IoBackend>,
) -> (IoEngine, mpsc::Receiver<Vec<u8>>) {
    let (tx, mut rx) = mpsc::channel::<IoRequest>(64);
    let (data_tx, data_rx) = mpsc::channel::<Vec<u8>>(64);
    tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            let r = backend.request(&req.frame).await;
            // 回执失败说明 Lua 侧已超时放弃 → 忽略。
            let _ = req.reply_to.send(r);
            // 注：被动帧（on_data）需 backend 主动推 data_tx；Plan 1 的 Mock/Tcp 暂不推。
        }
        let _ = data_tx; // 保留发送端，Plan 1 不用
    });
    (IoEngine { tx }, data_rx)
}

// ===== MockIoTask（测试用，可编程响应）=====

/// mock IO backend：按闭包响应。用于无真实设备的单测/集成测。
pub(super) struct MockIoTask {
    pub responder: Box<dyn Fn(&[u8]) -> mlua::Result<Vec<u8>> + Send + Sync>,
}

#[async_trait::async_trait]
impl IoBackend for MockIoTask {
    async fn request(&mut self, frame: &[u8]) -> mlua::Result<Vec<u8>> {
        (self.responder)(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_roundtrip() {
        let mock = MockIoTask {
            responder: Box::new(|f: &[u8]| {
                // echo：响应 = 请求 + 一个尾字节
                let mut r = f.to_vec();
                r.push(0xAA);
                Ok(r)
            }),
        };
        let (engine, _data_rx) = spawn_io_task(Box::new(mock));
        let resp = engine.request(b"\x01\x02", 200).await.unwrap();
        assert_eq!(resp, vec![0x01, 0x02, 0xAA]);
    }

    #[tokio::test]
    async fn timeout_returns_err() {
        let mock = MockIoTask {
            responder: Box::new(|_| {
                // 模拟慢响应：这里同步返回不模拟慢；用 channel 容量满模拟关闭。
                Ok(vec![])
            }),
        };
        let (engine, _data_rx) = spawn_io_task(Box::new(mock));
        // drop tx 模拟 IO task 关闭：直接构造一个连不上 backend 的 engine。
        let (tx, _rx) = mpsc::channel::<IoRequest>(1);
        let dead = IoEngine { tx };
        let _ = engine; // 保留以避免编译警告
        let r = dead.request(b"x", 100).await;
        // _rx 还活着，send 可能成功但无 backend 处理 → 超时
        assert!(r.is_err(), "无 backend 处理应超时或报错");
    }
}
```

- [ ] **Step 2：在 `mod.rs` 加 `mod io_engine;`**

```rust
mod io_engine;
use io_engine::{spawn_io_task, IoEngine, IoBackend, MockIoTask};
```

- [ ] **Step 3：跑测试验证通过**

```bash
cargo test --features lua io_engine::tests
```
Expected：2 个测试 PASS（mock roundtrip、超时报错）。

- [ ] **Step 4：fmt + clippy + commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
git add src/plugin/builtin/lua_client/io_engine.rs src/plugin/builtin/lua_client/mod.rs
git commit -m "feat(lua): IO 引擎（channel+独占 task+oneshot timeout+MockIoTask）"
```

---

## Task 6：LuaClient 主循环（`mod.rs`）— 接通 hub + collect

**Files:**
- Modify: `src/plugin/builtin/lua_client/mod.rs`（填 `run`）

把 Task 1 的 stub `run` 换成真实主循环（照抄 `data_sim/mod.rs:56-93` 的 `select!{token/cmd/tick}` 骨架，把 ticker 内的「simulate」换成「调 Lua collect」）。Plan 1 先用 `MockIoTask`（Task 7 换 tcp），先验证 collect 返回值落 rtdata。

- [ ] **Step 1：写失败测试（collect 返回 table → Sample 推 tx）**

在 `src/plugin/builtin/lua_client/mod.rs` 的 `#[cfg(test)] mod tests` 内加：

```rust
    use crate::host::{BridgeRegistry, CtlRegistry, HostContext};
    use crate::types::{AppType, CtlMessage, DeviceSpec, HostKind, Hwid, InstConfig, PluginName};
    use serde_json::json;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn collect_pushes_sample_to_hub() {
        let tmp = std::env::temp_dir().join(format!("lua-collect-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("devA")).unwrap();
        std::fs::write(
            tmp.join("devA/main.lua"),
            "function collect(ctx) return { temp = 42.5 } end",
        )
        .unwrap();

        let (tx, mut rx) = mpsc::channel::<crate::types::Sample>(8);
        let ctx = HostContext {
            lux: mock_lux().await,
            hwid: Hwid::new("it"),
            shutdown: CancellationToken::new(),
            ctl_registry: CtlRegistry::new(),
            bridge_registry: BridgeRegistry::new(),
        };
        let inst = InstConfig {
            inst_id: "lua-it".into(),
            inst_name: "it".into(),
            app_code: PluginName::new("luaClient"),
            app_type: AppType::South,
            auto_start: false,
            app_config: json!({
                "script":"devA","endpoint":"127.0.0.1:1","period":1,
                "scripts_root": tmp.to_str().unwrap()
            }),
            devices: vec![DeviceSpec { label:"d".into(), value:"devA".into(), children: vec![] }],
            host_kind: HostKind::Builtin,
        };
        let (cmd_tx, cmd_rx) = mpsc::channel::<CtlMessage>(1);
        let token = CancellationToken::new();
        let run_token = token.child_token();
        tokio::spawn(async move {
            LuaClient.run(ctx, inst, tx, cmd_rx, run_token).await;
        });
        let _ = cmd_tx; // 保留 sender，防 cmd_rx.recv() 立即 None 致忙循环

        let s = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
            .await
            .expect("应收到 Sample")
            .expect("Sample 非 None");
        assert_eq!(s.devid, "devA");
        assert!(format!("{:?}", s.values).contains("42.5"));
        token.cancel();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    async fn mock_lux() -> lux::EmbeddedClient {
        let cfg = rs_iot::config::AppConfig::default();
        let lux_cfg = rs_iot::db::build_lux_config(&cfg);
        let handle = lux::run_with_config(lux_cfg).await.expect("lux");
        handle.client()
    }
```

> 注：`run` 签名第 4 参数是 `cmd_rx`，测试里给一个孤立 channel（`.1` = 空接收端）。`scripts_root` 通过 `app_config` 或运行时工作目录解析——Plan 1 约定 `run` 从 `app_config.scripts_root`（缺省 `"scripts"`）解析，测试用 `std::env::set_current_dir(&tmp)` 或显式传。**实现里采用：`run` 接受 `app_config.scripts_root`（缺省 "scripts"），测试里 set_current_dir 到 tmp 的父目录。** 调整测试：在 spawn 前加 `std::env::set_current_dir(&tmp).unwrap();` 并把脚本路径改为相对（`scripts_root="."`）。见 Step 2 实现的解析。

- [ ] **Step 2：实现 `run` 主循环（替换 Task 1 的 stub）**

替换 `src/plugin/builtin/lua_client/mod.rs` 中 `impl Source for LuaClient` 的 `run` 方法体（替换 Step 4 的 stub），并调整文件顶部 import：

```rust
use crate::host::HostContext;
use crate::plugin::traits::Source;
use crate::types::{CtlMessage, InstConfig, Sample, SampleValue};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
```

`run` 方法体：

```rust
    async fn run(
        &self,
        ctx: HostContext,
        inst: InstConfig,
        tx: mpsc::Sender<Sample>,
        mut cmd_rx: mpsc::Receiver<CtlMessage>,
        token: CancellationToken,
    ) {
        // 1. 解析配置。
        let cfg = match parse_cfg(&inst.app_config) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("[luaClient] inst {} 配置错误：{e}", inst.inst_id);
                return;
            }
        };
        let scripts_root = inst
            .app_config
            .get("scripts_root")
            .and_then(|v| v.as_str())
            .unwrap_or(loader::SCRIPTS_ROOT);
        let scripts_root = std::path::Path::new(scripts_root).to_path_buf();

        // 2. 建 VM + 加载脚本。
        let lua = match engine::new_vm(&cfg) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("[luaClient] inst {} VM 创建失败：{e}", inst.inst_id);
                return;
            }
        };
        let lctx = LoadCtx {
            scripts_root: Arc::from(scripts_root.as_path()),
            script_name: Arc::<str>::from(cfg.script.as_str()),
        };
        if let Err(e) = load_main(&lua, &lctx) {
            tracing::warn!("[luaClient] inst {} 脚本加载失败：{e}", inst.inst_id);
            return;
        }

        // 3. IO 引擎（Plan 1 用 MockIoTask 占位；Task 7 换 TcpIo）。
        let backend: Box<dyn IoBackend> = Box::new(MockIoTask {
            responder: Box::new(|_| Ok(vec![])), // 占位：Task 7 替换
        });
        let (io_engine, mut _data_rx) = spawn_io_task(backend);

        // 4. 注册 ctx API（Task 8 扩充；Plan 1 先注册 request + log）。
        api::register(&lua, ctx.clone(), &inst, io_engine).expect("register ctx api");

        // 5. on_init（若脚本定义）。
        if lua.globals().get::<_, mlua::Function>("on_init").is_ok() {
            let _: mlua::Result<()> = lua
                .load("on_init(ctx)")
                .eval_async()
                .await
                .map_err(|e| tracing::warn!("[luaClient] on_init 失败：{e}"));
        }

        // 6. 主循环（照抄 data_sim/mod.rs:56-93 的 select! 骨架）。
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(cfg.period.max(1)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tracing::info!(
            "[luaClient] inst {} started (script={}, period={}s)",
            inst.inst_id, cfg.script, cfg.period
        );

        loop {
            tokio::select! {
                _ = token.cancelled() => {
                    tracing::info!("[luaClient] inst {} stopping", inst.inst_id);
                    break;
                }
                msg = cmd_rx.recv() => {
                    if let Some(m) = msg {
                        if m.cmd == "write" {
                            if let Err(e) = call_on_write(&lua, &m.data).await {
                                tracing::warn!("[luaClient] on_write 失败：{e}");
                            }
                        }
                    }
                }
                _ = ticker.tick() => {
                    if cfg.period == 0 { continue; } // 纯被动
                    let now = chrono::Local::now().timestamp();
                    for dev in &inst.devices {
                        match call_collect(&lua).await {
                            Ok(Some(values)) if !values.is_empty() => {
                                let sample = Sample {
                                    hwid: ctx.hwid.clone(),
                                    devid: dev.value.clone(),
                                    values,
                                    timestamp: now,
                                };
                                if tx.send(sample).await.is_err() { break; }
                            }
                            Ok(_) => {} // 脚本返回 nil 或空 table
                            Err(e) => tracing::warn!("[luaClient] collect 失败：{e}"),
                        }
                    }
                }
            }
        }
    }
```

并加两个辅助 async fn（文件底部、`inventory::submit!` 前）：

```rust
/// 调脚本 collect(ctx)，返回 values map（key=tag）。
async fn call_collect(lua: &mlua::Lua) -> mlua::Result<Option<BTreeMap<String, SampleValue>>> {
    let f: mlua::Function = match lua.globals().get("collect") {
        Ok(f) => f,
        Err(_) => return Ok(None), // 脚本未定义 collect
    };
    let result: mlua::Value = f.call_async(mlua::Value::Nil).await?;
    match result {
        mlua::Value::Nil => Ok(None),
        mlua::Value::Table(t) => {
            let mut values = BTreeMap::new();
            for pair in t.pairs::<String, mlua::Value>() {
                let (k, v) = pair?;
                if let Some(sv) = lua_value_to_sample(&v) {
                    values.insert(k, sv);
                }
            }
            Ok(Some(values))
        }
        _ => Ok(None),
    }
}

/// 调脚本 on_write(ctx, cmd)。cmd_data 是 JSON 文本，转 table 传给脚本。
async fn call_on_write(lua: &mlua::Lua, cmd_data: &str) -> mlua::Result<()> {
    let f: mlua::Function = match lua.globals().get("on_write") {
        Ok(f) => f,
        Err(_) => return Ok(()),
    };
    let cmd: mlua::Value = if cmd_data.is_empty() {
        mlua::Value::Nil
    } else {
        let v: serde_json::Value = serde_json::from_str(cmd_data)
            .map_err(|e| mlua::Error::RuntimeError(format!("cmd JSON 解析失败：{e}")))?;
        lua.to_value(&v)?
    };
    let _: mlua::Value = f.call_async(cmd).await?;
    Ok(())
}

/// mlua::Value → SampleValue（对标 transform/rhai.rs 的类型映射）。
fn lua_value_to_sample(v: &mlua::Value) -> Option<SampleValue> {
    match v {
        mlua::Value::Integer(i) => Some(SampleValue::Int(*i as i64)),
        mlua::Value::Number(n) => Some(SampleValue::Float(*n)),
        mlua::Value::Boolean(b) => Some(SampleValue::Bool(*b)),
        mlua::Value::String(s) => Some(SampleValue::Str(s.to_str().ok()?.to_string())),
        _ => None,
    }
}
```

- [ ] **Step 3：确认脚本路径解析**

`run` 从 `app_config.scripts_root`（缺省 `"scripts"`）解析根目录；Step 1 测试已把 `scripts_root` 指向临时目录 `tmp`，故 `scripts/<name>/main.lua` = `tmp/devA/main.lua`。无需 `set_current_dir`。

- [ ] **Step 4：跑测试验证通过**

```bash
cargo test --features lua collect_pushes_sample_to_hub
```
Expected：PASS（收到 Sample，devid=devA，values 含 42.5）。

> 若失败常见原因：`api::register` 尚未定义 → Task 8 才完整；此处 Plan 1 约定 Task 6 先加一个**最小 `api.rs`**（见 Task 8 Step 1 的骨架），即 Task 6 依赖 Task 8 的 api.rs 骨架先行。**执行顺序调整：先做 Task 8 的 api.rs 骨架（register 空 fn），再回 Task 6。** 见 Task 8。

- [ ] **Step 5：fmt + clippy + commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
git add src/plugin/builtin/lua_client/mod.rs
git commit -m "feat(lua): LuaClient 主循环（select! + collect→Sample→hub + on_write）"
```

---

## Task 7：TCP 传输接入 IO 引擎（`io_engine.rs` 扩 `TcpIo`）

**Files:**
- Modify: `src/plugin/builtin/lua_client/io_engine.rs`（加 `TcpIo`）
- Modify: `src/plugin/builtin/lua_client/mod.rs`（`run` 按 `cfg.transport` 选 backend）

Plan 1 仅 TCP（serial/udp 框架已在 `LuaCfg`/`Transport` 预留，后续 plan 接）。复用 `tokio::net::TcpStream`；超时连接 + 断链重连（对标 modbus `reconnect_secs`）。

- [ ] **Step 1：写 `TcpIo` + 失败测试**

在 `io_engine.rs` 加（`MockIoTask` 后）：

```rust
/// TCP IO backend：独占一条 TcpStream，request = write_all(frame) + 读响应（Timeout 模式：
/// 读到静默 recv_idle_ms 返回累积字节）。
pub(super) struct TcpIo {
    stream: tokio::net::TcpStream,
    idle: Duration,
}

impl TcpIo {
    /// 连接 endpoint（`ip:port`），返回 backend。失败由调用方按 reconnect_secs 重试。
    pub(super) async fn connect(endpoint: &str, recv_idle_ms: u64) -> mlua::Result<Self> {
        let stream = tokio::net::TcpStream::connect(endpoint)
            .await
            .map_err(|e| mlua::Error::RuntimeError(format!("tcp connect {endpoint}: {e}")))?;
        Ok(Self {
            stream,
            idle: Duration::from_millis(recv_idle_ms),
        })
    }
}

#[async_trait::async_trait]
impl IoBackend for TcpIo {
    async fn request(&mut self, frame: &[u8]) -> mlua::Result<Vec<u8>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        self.stream
            .write_all(frame)
            .await
            .map_err(|e| mlua::Error::RuntimeError(format!("tcp write: {e}")))?;
        // Timeout 成帧：循环读，每次 idle 内无新数据即返回。
        let mut buf = Vec::new();
        let mut tmp = [0u8; 256];
        loop {
            match timeout(self.idle, self.stream.read(&mut tmp)).await {
                Ok(Ok(0)) => break, // 对端关闭
                Ok(Ok(n)) => buf.extend_from_slice(&tmp[..n]),
                Ok(Err(e)) => return Err(mlua::Error::RuntimeError(format!("tcp read: {e}"))),
                Err(_) => break, // idle 静默 → 帧完成
            }
        }
        if buf.is_empty() {
            return Err(mlua::Error::RuntimeError("tcp: 无响应".into()));
        }
        Ok(buf)
    }
}
```

测试（`#[cfg(test)] mod tests` 内加）：

```rust
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn tcp_roundtrip() {
        // mock server：收什么回什么 + 尾巴。
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 64];
            loop {
                match s.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut r = buf[..n].to_vec();
                        r.push(0xAA);
                        let _ = s.write_all(&r).await;
                    }
                }
            }
        });
        // 给 server 起一点时间
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut io = TcpIo::connect(&format!("127.0.0.1:{port}"), 50).await.unwrap();
        let resp = io.request(b"\x01\x02").await.unwrap();
        assert_eq!(resp, vec![0x01, 0x02, 0xAA]);
    }
```

- [ ] **Step 2：`mod.rs` 的 `run` 按 transport 选 backend**

把 Task 6 Step 2 里 `run` 的第 3 步「MockIoTask 占位」替换为按 `cfg.transport` 选：

```rust
        // 3. IO 引擎：按 transport 选 backend。Plan 1 实现 tcp；serial/udp 后续 plan。
        let backend: Box<dyn IoBackend> = match cfg.transport {
            config::Transport::Tcp => match TcpIo::connect(&cfg.endpoint, cfg.recv_idle_ms).await {
                Ok(io) => Box::new(io),
                Err(e) => {
                    tracing::warn!("[luaClient] inst {} tcp 连接失败：{e}（{}s 后重试由重启实例兜底）", inst.inst_id, cfg.reconnect_secs);
                    Box::new(MockIoTask { responder: Box::new(|_| Ok(vec![])) })
                }
            },
            _ => {
                tracing::warn!("[luaClient] inst {} transport {:?} 暂未实现（Plan 1 仅 tcp），用 mock 占位", inst.inst_id, cfg.transport);
                Box::new(MockIoTask { responder: Box::new(|_| Ok(vec![])) })
            }
        };
        let (io_engine, _data_rx) = spawn_io_task(backend);
```

并加 import：`use io_engine::TcpIo;`

- [ ] **Step 3：跑测试验证通过**

```bash
cargo test --features lua io_engine::tests::tcp_roundtrip
```
Expected：PASS。

- [ ] **Step 4：fmt + clippy + commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
git add src/plugin/builtin/lua_client/io_engine.rs src/plugin/builtin/lua_client/mod.rs
git commit -m "feat(lua): TCP 传输接入 IO 引擎（Timeout 成帧）"
```

---

## Task 8：ctx API 注册（`api.rs`）— L1 request + L6 log + ctx 占位

**Files:**
- Create: `src/plugin/builtin/lua_client/api.rs`
- Modify: `src/plugin/builtin/lua_client/mod.rs`

**注意执行顺序**：Task 8 的 `api.rs` 骨架需在 Task 6 之前完成（Task 6 的 `run` 调 `api::register`）。所以实际执行：Task 1→2→3→4→5→**8 骨架**→6→7→8 扩充。Plan 1 此 task 给出最终 `api.rs`（含 L1 request + L6 log/now/time + ctx 占位；L2 get_value/set_value 等后续 plan 扩）。

- [ ] **Step 1：写 `api.rs`**

```rust
//! ctx 对象 + host API 注册（spec §7）。Plan 1 实现 L1 request + L6 log/now/time；
//! L2 get_value/set_value/get_device/list_devices/config/dump_comm/crc 后续 plan 扩。

use crate::host::HostContext;
use crate::plugin::builtin::lua_client::io_engine::IoEngine;
use crate::types::InstConfig;
use mlua::{Lua, Value};

/// 在 `lua` 上注册 ctx 全局对象 + host 函数。
pub(super) fn register(
    lua: &Lua,
    ctx_host: HostContext,
    inst: &InstConfig,
    io: IoEngine,
) -> mlua::Result<()> {
    // ctx 表：方法挂在其上（ctx:request / ctx:log_info / ...）。
    let ctx = lua.create_table()?;

    // --- L1 ctx:request(frame:str, timeout_ms:num) -> resp:str | nil,err ---
    // 必须 create_async_function：ctx:request 内部 coroutine.yield 桥接 IO task（spec §8）。
    // async function 挂 table 字段，脚本 ctx:request(...) 调用时 mlua 自动按 async 驱动协程。
    let io_req = io.clone();
    let request_fn = lua.create_async_function(
        move |lua, (frame, timeout_ms): (mlua::String, u64)| {
            let io = io_req.clone();
            async move {
                let bytes = frame.as_bytes().to_vec();
                match io.request(&bytes, timeout_ms).await {
                    Ok(resp) => Ok::<_, mlua::Error>((
                        Value::String(lua.create_string(&resp)?),
                        Value::Nil,
                    )),
                    Err(e) => Ok((
                        Value::Nil,
                        Value::String(lua.create_string(&e.to_string()))?,
                    )),
                }
            }
        },
    )?;
    ctx.set("request", request_fn)?;

    // --- L6 log_info / log_warn / log_err / log_debug ---
    let name = inst.inst_id.as_str().to_string();
    register_log(lua, &ctx, &name)?;

    // --- L6 now() / time() ---
    ctx.set(
        "now",
        lua.create_function(|_lua, ()| Ok(chrono::Local::now().timestamp_millis() as f64))?,
    )?;
    ctx.set(
        "time",
        lua.create_function(|_lua, ()| Ok(chrono::Local::now().timestamp() as f64))?,
    )?;

    lua.globals().set("ctx", ctx)?;
    let _ = ctx_host; // HostContext 在后续 plan（get_value 等）才用，Plan 1 保留
    Ok(())
}

fn register_log(lua: &Lua, ctx: &mlua::Table, name: &str) -> mlua::Result<()> {
    for (level, log_fn) in [
        ("log_info", tracing::Level::INFO as u8),
        ("log_warn", tracing::Level::WARN as u8),
        ("log_err", tracing::Level::ERROR as u8),
        ("log_debug", tracing::Level::DEBUG as u8),
    ] {
        let n = name.to_string();
        let f = lua.create_function(move |_lua, msg: mlua::String| {
            let msg = msg.to_string_lossy().to_string();
            match log_fn {
                x if x == tracing::Level::INFO as u8 => tracing::info!("[lua:{n}] {msg}"),
                x if x == tracing::Level::WARN as u8 => tracing::warn!("[lua:{n}] {msg}"),
                x if x == tracing::Level::ERROR as u8 => tracing::error!("[lua:{n}] {msg}"),
                _ => tracing::debug!("[lua:{n}] {msg}"),
            }
            Ok(())
        })?;
        ctx.set(level, f)?;
    }
    Ok(())
}
```

（`request` 已用 `create_async_function` 注册为 async 并挂 `ctx.request`；mlua 在脚本调用时自动驱动协程，经 `coroutine.yield` 桥接 IO task。）

- [ ] **Step 2：在 `mod.rs` 加 `mod api;`**

```rust
mod api;
```

- [ ] **Step 3：跑全部单测验证通过**

```bash
cargo test --features lua
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
```
Expected：Task 6 的 collect 测试 + 此前所有测试 PASS（collect 脚本里可不调 ctx:request，仅返回固定 table）。

- [ ] **Step 4：commit**

```bash
git add src/plugin/builtin/lua_client/api.rs src/plugin/builtin/lua_client/mod.rs
git commit -m "feat(lua): ctx API（L1 request async + L6 log/now/time）"
```

---

## Task 9：端到端集成测 A（标准 Modbus TCP）

**Files:**
- Create: `scripts/it_modbus/main.lua`
- Create: `tests/lua_modbus_e2e.rs`

复用 `tests/s7_e2e.rs` 的完整范式：`common::start_lux` + `TcpListener` mock server + `ServerState{regs,writes}` + `setup/teardown` + `hget_cell` + `send_ctl`。

- [ ] **Step 1：创建测试脚本 `scripts/it_modbus/main.lua`**

```lua
-- 标准 Modbus TCP 客户端（Lua 重写，验证 luaClient 表达力上限）。
-- FC03 读保持寄存器（addr=0, qty=2）；FC06 写单寄存器。

function collect(ctx)
  -- MBAP(tid,pid,len=6,uid) + FC03(fc=3,addr=0,qty=2)，全大端
  local req = string.pack(">I2I2I2BBI2I2", 1, 0, 6, 1, 3, 0, 2)
  local resp = ctx:request(req, 200)
  if not resp or #resp < 11 then return nil end
  local r0, r1 = string.unpack(">I2I2", resp:sub(10))  -- 跳 MBAP(7)+fc(1)+bc(1)
  return { temp = r0 / 10, speed = r1 }
end

function on_write(ctx, c)
  local req = string.pack(">I2I2I2BBI2I2", 2, 0, 6, 1, 6, c.addr, c.value)
  return ctx:request(req, 200) ~= nil
end
```

- [ ] **Step 2：创建 `tests/lua_modbus_e2e.rs`**

```rust
//! luaClient 端到端：lux + mock Modbus TCP server + luaClient 实例（scripts/it_modbus）
//! + Lua 脚本采集 → rtdata + 写命令 round-trip。复用 s7_e2e 范式。

mod common;

use common::test_data_dir;
use rs_iot::host::{BridgeRegistry, CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::plugin::traits::Processor;
use rs_iot::transform::passthrough::Passthrough;
use rs_iot::types::{
    AppType, CtlMessage, DeviceSpec, HostKind, Hwid, InstConfig, InstId, PluginName, Sample,
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

struct ServerState {
    regs: Mutex<HashMap<u16, u16>>,
    writes: Mutex<Vec<(u16, u16)>>,
}

#[tokio::test]
async fn lua_modbus_collect_and_write_roundtrip() {
    let data_dir = test_data_dir("lua-modbus");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client).await.expect("bootstrap");

    // mock Modbus TCP server：预播 regs，解析 FC03/FC06。
    let state = Arc::new(ServerState {
        regs: Mutex::new(HashMap::from([(0u16, 125u16), (1u16, 32000u16)])),
        writes: Mutex::new(vec![]),
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let sv_state = state.clone();
    let sv_token = CancellationToken::new();
    let t2 = sv_token.clone();
    tokio::spawn(async move { serve_modbus(listener, sv_state, t2).await });

    // HostContext + hub + pipeline（落 rtdata）。
    let shutdown = CancellationToken::new();
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown: shutdown.clone(),
        ctl_registry: CtlRegistry::new(),
        bridge_registry: BridgeRegistry::new(),
    };
    let (hub_tx, hub_rx) = tokio::sync::mpsc::channel::<Sample>(64);
    let mgr = Arc::new(InstanceManager::new(ctx.clone(), hub_tx));
    let proc: Arc<dyn Processor> = Arc::new(Passthrough);
    let pt = shutdown.child_token();
    let _pipeline = tokio::spawn(rs_iot::pipeline::run(ctx, hub_rx, proc, pt));

    // luaClient 实例：脚本 it_modbus，指向 mock server。period=1s。
    // scripts_root 指向仓库 scripts/ 目录（CARGO_MANIFEST_DIR）。
    let scripts_root = env!("CARGO_MANIFEST_DIR").to_string() + "/scripts";
    let inst = InstConfig {
        inst_id: "lua-it-modbus".into(),
        inst_name: "it modbus".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: true,
        app_config: json!({
            "script":"it_modbus",
            "kind":"tcp",
            "endpoint": format!("127.0.0.1:{port}"),
            "period": 1,
            "recv_idle_ms": 80,
            "scripts_root": scripts_root,
        }),
        devices: vec![DeviceSpec { label:"m".into(), value:"devM".into(), children: vec![] }],
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start lua");

    // 等至少一轮采集 + 落库。
    tokio::time::sleep(Duration::from_millis(2500)).await;

    let temp = hget_cell(&client, "rtdata:devM", "devM.temp").await;
    assert!(temp.contains("12.5"), "reg0=125 → temp=12.5，得到 '{temp}'");
    let speed = hget_cell(&client, "rtdata:devM", "devM.speed").await;
    assert!(speed.contains("32000"), "reg1=32000，得到 '{speed}'");

    // 写命令（FC06 addr=0 value=1）→ mock writes 收到。
    let msg = CtlMessage {
        instance: InstId::new("lua-it-modbus"),
        devid: "devM".into(),
        cmd: "write".into(),
        data: json!({"addr":0,"value":1}).to_string(),
    };
    assert!(mgr.send_ctl("lua-it-modbus", msg), "send_ctl 应命中实例");
    tokio::time::sleep(Duration::from_millis(800)).await;
    let writes = state.writes.lock().unwrap().clone();
    assert_eq!(writes.len(), 1, "mock 应收到 1 条写，{writes:?}");
    assert_eq!(writes[0], (0, 1), "addr=0 value=1");

    shutdown.cancel();
    sv_token.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
}

/// mock modbus server：裸字节解析 MBAP+FC03/FC06。
async fn serve_modbus(listener: TcpListener, state: Arc<ServerState>, token: CancellationToken) {
    loop {
        tokio::select! {
            biased;
            _ = token.cancelled() => break,
            res = listener.accept() => {
                if let Ok((stream, _)) = res {
                    let st = state.clone();
                    let t = token.child_token();
                    tokio::spawn(async move { serve_one(stream, st, t).await });
                }
            }
        }
    }
}

async fn serve_one(mut stream: TcpStream, state: Arc<ServerState>, token: CancellationToken) {
    let mut buf = [0u8; 256];
    loop {
        tokio::select! {
            biased;
            _ = token.cancelled() => break,
            res = stream.read(&mut buf) => {
                let n = match res { Ok(0) | Err(_) => break, Ok(n) => n };
                if n < 12 { continue; } // MBAP(7)+fc(1)+addr(2)+qty(2) = 12
                let fc = buf[7];
                let resp = match fc {
                    3 => { // FC03：回 MBAP + fc=3 + bc + 2寄存器值
                        let addr = u16::from_be_bytes([buf[8], buf[9]]) as usize;
                        let qty = u16::from_be_bytes([buf[10], buf[11]]) as usize;
                        let regs = state.regs.lock().unwrap();
                        let mut data = Vec::new();
                        for i in 0..qty {
                            let v = regs.get(&(addr as u16 + i as u16)).copied().unwrap_or(0);
                            data.extend_from_slice(&v.to_be_bytes());
                        }
                        drop(regs);
                        let mut r = Vec::new();
                        r.extend_from_slice(&buf[0..4]); // tid+pid
                        let len: u16 = 3 + (qty * 2) as u16;
                        r.extend_from_slice(&len.to_be_bytes()); // len
                        r.push(buf[6]); // uid
                        r.push(3); // fc
                        r.push((qty * 2) as u8); // bc
                        r.extend_from_slice(&data);
                        r
                    }
                    6 => { // FC06：echo + 记录写
                        let addr = u16::from_be_bytes([buf[8], buf[9]]);
                        let value = u16::from_be_bytes([buf[10], buf[11]]);
                        state.writes.lock().unwrap().push((addr, value));
                        buf[..12].to_vec() // echo（含 tid/pid/len/uid/fc/addr/value）
                    }
                    _ => continue,
                };
                use tokio::io::AsyncWriteExt;
                let _ = stream.write_all(&resp).await;
            }
        }
    }
}

async fn hget_cell(client: &lux::EmbeddedClient, key: &str, field: &str) -> String {
    use lux::EmbeddedValue;
    match client.execute_value("HGET", &[key, field]).await {
        Ok(EmbeddedValue::Bulk(b)) => String::from_utf8_lossy(&b).to_string(),
        Ok(EmbeddedValue::Simple(s)) => s,
        _ => String::new(),
    }
}
```

- [ ] **Step 3：跑端到端测验证通过**

```bash
cargo test --features lua --test lua_modbus_e2e
```
Expected：PASS（temp=12.5、speed=32000 落 rtdata；写命令 mock 收到 (0,1)）。

> 若失败排查：① mock server 响应字节不匹配 `string.unpack` 偏移 → 用 `tracing::debug` 打印 resp 字节；② `recv_idle_ms` 太短读到不完整 → 调大到 80-150ms；③ 脚本路径 → 确认 `scripts/it_modbus/main.lua` 存在且 `scripts_root` 正确。

- [ ] **Step 4：fmt + clippy + 全量测试 + commit**

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets --all-features
git add scripts/it_modbus/main.lua tests/lua_modbus_e2e.rs
git commit -m "test(lua): 端到端闭环（标准 Modbus TCP，mock server + rtdata + 写命令）"
```

---

## Plan 1 完成标准（Definition of Done）

- [ ] `cargo build --features lua`（Windows + Linux）零错误零警告（mlua vendored 编入）
- [ ] `cargo test --all-targets --all-features` 全绿（含 Task 1-8 单测 + Task 9 端到端）
- [ ] `cargo fmt --all --check` + `cargo clippy --all-targets -- -D warnings` 零警告
- [ ] 手动验证：`./target/debug/rs-iot --config config.toml`（建一个 `app_code="luaClient"` + `scripts/devA/main.lua` 实例）→ `redis-cli -p 6390 HGETALL rtdata:devA` 看到采集值；`POST /api/v1/command/send` 写命令经 `on_write` 下发
- [ ] CLAUDE.md 更新「模块职责」表（加 luaClient）+ 「端到端验证」小节

## 后续 Plan（本 plan 不实现，完成 Plan 1 后基于 mlua 实战经验详写）

- **Plan 2（M4）**：L3 `send_command` 联动 + L4 `kv`（`luakv.rs` + identity 清单 + 实例删除清理）
- **Plan 3（M5）**：L5 `http_get/post`（`lua-http` feature + reqwest + 审计）
- **Plan 4（M6）**：热加载（notify + debouncer + 重建 VM + dry-run + 失败回退）+ on_data 被动帧推送
- **Plan 5（M7）**：状态监测（`stat:<inst>` 指标）+ REST（`routes_lua.rs`）
- **Plan 6（M8）**：serial/udp 传输 + 文档（`lua-script-guide.md`）+ examples
