# Lua 脚本系统演进 · 阶段 A（机制地基）实施计划

> 日期：2026-08-19
> **状态**：implemented（2026-08-19~20 于 worktree `feat/lua-stage-a` 执行完毕，13 task 全落地）
> **实施**：T1-T13 逐 task commit（`c31e4a6` spike-1 → `496549a` T13 公钥回填）；2026-08-20 四轮独立审查后追加修复 commit（安全/行为/接口共 16 项，见设计文档 v0.5.1 版本记录）
> **代码**：`src/plugin/lua_core/` + `src/plugin/builtin/lua_client/` + `src/api/routes_lua.rs`
> **测试**：`cargo test -p rs-iot --features lua,lua-http`（注：主 crate 包名为 `rs-iot`，下文 `rsiot` 为计划期笔误统称）全绿；`cargo test -p rsiot-mcp` 全绿
> **遗留**：审查登记项见设计文档头部遗留清单（模板安装器/sync 角色权限/min_host_ver 等，排期 B）

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 落地 `2026-08-18-lua-script-system-evolution-design.md` v0.5 阶段 A——lua_core 重构、能力卡、五项护栏、require 命名空间路由、stdlib 种子/json/index/sync、`_lib/` 共享库、manifest dry-run、官方源仓建制。

**Architecture:** `lua_client/`（15 文件）重组为 `src/plugin/lua_core/` 公共层 + `builtin/lua_client/` 薄壳（Q19，纯搬移先行）；require 按「首段路由」分发 builtin/stdlib/共享/私有四层（§3.1，加载器强制拒绝违规命名）；护栏五项收敛在引擎与 API 注册层；stdlib 以 rust-embed 种子兜底 + `_stdlib/` 同步目录 + ed25519 验签 index（§3.5-3.6）。

**Tech Stack:** mlua 0.12（lua54/async/send/serde）、rust-embed 8（已有依赖）、ed25519-dalek 2（新增，lua feature 门控）、serde_json/sha2（已有）、axum REST、notify 热加载。

**执行前置：** 本计划跨 ≥5 文件 ≥10 commit，**必须在独立 worktree 执行**（`git worktree add ../rs-iot-lua-a <branch>`）。设计依据：`docs/designs/2026-08-18-lua-script-system-evolution-design.md` v0.5（Q 编号/§ 引用均指向它）。

**约定：** 所有 `cargo test` 用 `cargo test -p rsiot --features lua,lua-http`（主 crate 名以 `Cargo.toml [package].name` 为准，下文统称 `rsiot`）；每 Task 末 commit。测试目录无独立 tests/，单测在各文件 `#[cfg(test)]` 与 `lua_client/tests.rs`（重构后随迁）。

---

## 组 0：spike（阻塞 T5 实现分支）

### Task 1: spike-1——mlua 回调超时 drop 后 VM 状态语义

**Files:**
- Create: `src/plugin/builtin/lua_client/tests.rs` 追加（重构前就地做，结论通用）
- 产出：结论写入设计文档 §4.5（一行）+ 本计划 T5 分支选择

**Step 1: 写实验测试**（追加到 `src/plugin/builtin/lua_client/tests.rs`）

```rust
/// spike-1（设计 §9 R1）：call_async future 被 timeout drop 后，协程停在 await 点，
/// 验证 VM 是否可继续用、脚本全局态是否"半途"。结论决定 T5 方案 a（接受半途态）/b（超时重建 VM）。
#[tokio::test]
async fn spike_timeout_drop_leaves_vm_usable() {
    use crate::plugin::builtin::lua_client::engine;
    let mut c = crate::plugin::builtin::lua_client::config::parse_cfg(
        &serde_json::json!({"script":"t","endpoint":"x"})).unwrap();
    c.hook_instr_limit = 1_000_000;
    let lua = engine::new_vm(&c).unwrap();
    // 永不完成的宿主函数（模拟最慢的 await）
    lua.globals().set("hang", lua.create_async_function(|_, _: mlua::Value| {
        async move { std::future::pending::<()>().await }
    })?).unwrap();
    lua.load("t = {}; function work() t.a = 1; hang(); t.b = 2 end").exec().unwrap();
    let f: mlua::Function = lua.globals().get("work").unwrap();
    // 100ms 超时包裹 call_async → drop future
    let r: Result<mlua::Value, _> = tokio::time::timeout(
        std::time::Duration::from_millis(100), f.call_async(mlua::Value::Nil)).await
        .map_err(|_| mlua::Error::RuntimeError("timeout".into())).and_then(|x| x);
    assert!(r.is_err(), "应超时");
    // 验证点 1：VM 仍可执行新代码
    let v: i64 = lua.load("return 1 + 1").eval().unwrap();
    assert_eq!(v, 2);
    // 验证点 2：半途态可见（t.a 已写、t.b 未写）——记录实际值
    let a: i64 = lua.load("return t.a").eval().unwrap();
    let has_b: bool = lua.load("return t.b ~= nil").eval().unwrap();
    // 验证点 3：work 可再次调用（协程未锁死 VM）——用短路径版本
    lua.load("function work2() return 7 end").exec().unwrap();
    let f2: mlua::Function = lua.globals().get("work2").unwrap();
    let v2: i64 = f2.call_async(mlua::Value::Nil).await.unwrap();
    assert_eq!(v2, 7);
    println!("SPIKE1: a={a} has_b={has_b}"); // 预期 a=1, has_b=false → 半途态证实
}
```

**Step 2: 跑测试** `cargo test -p rsiot --features lua spike_timeout -- --nocapture`
预期：PASS 且打印 `SPIKE1: a=1 has_b=false`。若 VM 不可用（验证点 1 失败）→ 结论 b 强制。

**Step 3: 记录结论**到设计文档 §4.5 第 1 条末尾（"spike-1 实测：半途态 a 已写 b 未写，VM 可复用，取方案 a"或 b），commit `docs(spec): spike-1 结论落档`。

---

## 组 1：重构（纯搬移，零行为变化）

### Task 2: lua_client → lua_core 目录重组

**Files:**
- Move: `src/plugin/builtin/lua_client/{engine,script,loader,config,hotload}.rs`、`api/`、`io_engine/` → `src/plugin/lua_core/`
- Keep: `src/plugin/builtin/lua_client/mod.rs`（Source 薄壳）+ `tests.rs`
- Modify: `src/plugin/mod.rs`（加 `pub mod lua_core;`）、`src/plugin/builtin/mod.rs`

**Step 1: git mv 搬移**

```bash
mkdir -p src/plugin/lua_core
git mv src/plugin/builtin/lua_client/engine.rs src/plugin/lua_core/engine.rs
git mv src/plugin/builtin/lua_client/script.rs src/plugin/lua_core/script.rs
git mv src/plugin/builtin/lua_client/loader.rs src/plugin/lua_core/loader.rs
git mv src/plugin/builtin/lua_client/config.rs src/plugin/lua_core/config.rs
git mv src/plugin/builtin/lua_client/hotload.rs src/plugin/lua_core/hotload.rs
git mv src/plugin/builtin/lua_client/api src/plugin/lua_core/api
git mv src/plugin/builtin/lua_client/io_engine src/plugin/lua_core/io_engine
```

`lua_core/mod.rs` 新建（聚合子模块，可见性 `pub(crate)`）：

```rust
//! Lua 脚本系统公共层（设计 §5.0）：沙箱引擎 / 受控加载 / 命名空间路由 / ctx 能力注册 / IO 引擎。
//! 薄壳插件（builtin/lua_client 等）按角色组装本层。
pub(crate) mod api;
pub(crate) mod config;
pub(crate) mod engine;
pub(crate) mod hotload;
pub(crate) mod io_engine;
pub(crate) mod loader;
pub(crate) mod script;
```

**Step 2: 修可见性与路径**——搬移文件内 `crate::plugin::builtin::lua_client::X` 全部改 `crate::plugin::lua_core::X`（rg 逐处）；`pub(super)` 改 `pub(crate)`（跨目录引用）；`lua_client/mod.rs` 头部加 `use crate::plugin::lua_core as core_;` 并把对 engine/loader/api 等的引用改为 `core_::`（mod.rs 保留 Source impl、run 主循环、call_collect 等——A 阶段主循环仍留薄壳，角色化拆分在 C 阶段）。`src/plugin/mod.rs` 加 `#[cfg(feature = "lua")] pub mod lua_core;`。

**Step 3: 全量验证** `cargo test -p rsiot --features lua,lua-http` 预期：现有测试全绿（零行为变化准入/准出条件）。

**Step 4: Commit** `refactor(lua): lua_client 拆出 lua_core 公共层（纯搬移，设计 §5.0/Q19）`

---

## 组 2：能力卡与护栏

### Task 3: CapabilityCard 常量表 + 一致性测试（Q2）

**Files:**
- Create: `src/plugin/lua_core/capability.rs`
- Modify: `src/plugin/lua_core/mod.rs`（挂模块）

**Step 1: 写失败测试**（capability.rs 底部）

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::lua_core::engine;
    use crate::plugin::lua_core::config::parse_cfg;

    /// Q2 一致性锁：每张能力卡 name 必须真注册在 ctx 表上（mock VM）。
    #[test]
    fn every_card_is_registered_on_ctx() {
        let cfg = parse_cfg(&serde_json::json!({"script":"t","endpoint":"x"})).unwrap();
        let lua = engine::new_vm(&cfg).unwrap();
        // register 需要 HostContext——此处仅校验"卡名格式 + ctx: 前缀 + 无重复"，
        // 真注册断言放 api/tests.rs（已有 register 测试基建，追加循环）。
        let mut seen = std::collections::HashSet::new();
        for c in CAPABILITIES {
            assert!(c.name.starts_with("ctx:") || c.layer == Layer::C0 && BUILTIN_MODULES.contains(&c.name.as_str()),
                "能力卡 {c:?} name 非法");
            assert!(seen.insert(&c.name), "能力卡重复：{}", c.name);
        }
        assert!(CAPABILITIES.len() >= 17, "现有 17 个 ctx API 应全部登记");
    }
}
```

**Step 2: 跑** `cargo test -p rsiot --features lua capability` 预期 FAIL（模块不存在）。

**Step 3: 实现**（capability.rs 核心）

```rust
//! 能力卡（设计 §4.2/Q2）：宿主赋予脚本能力的元数据登记——单一事实源，
//! 驱动一致性测试、护栏参数与文档生成。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Layer { C0, C1, C2, C3, C4, C5, C6, C7 }
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Risk { R0, R1, R2, R3 }

#[derive(Debug, Clone)]
pub struct CapabilityCard {
    pub name: &'static str,          // "ctx:http_get" 或 builtin 模块名 "json"
    pub layer: Layer,
    pub risk: Risk,
    pub feature: Option<&'static str>, // 门控 feature 名
    pub timeout_cap_ms: u64,         // 脚本可传超时的宿主上限（0=无超时参数）
    pub rate_per_sec: u32,           // 频控（0=不限）
    pub api_ver: u32,
    pub desc: &'static str,
}

pub const BUILTIN_MODULES: &[&str] = &["json"];

pub static CAPABILITIES: &[CapabilityCard] = &[
    // L1-L6 现有 17 项逐一登记（request/log*4/now/time/kv*2/send_command/
    // set_value/set_values/get_value/get_device/list_devices/transport/http*2）
    CapabilityCard { name: "ctx:request", layer: Layer::C1, risk: Risk::R1,
        feature: None, timeout_cap_ms: 30_000, rate_per_sec: 0, api_ver: 1, desc: "链路一问一答" },
    CapabilityCard { name: "ctx:http_get", layer: Layer::C5, risk: Risk::R2,
        feature: Some("lua-http"), timeout_cap_ms: 30_000, rate_per_sec: 20, api_ver: 1, desc: "HTTP GET 出站" },
    // ... 其余 15 项同构补全（见 api/mod.rs 注册清单，一一对应）
    CapabilityCard { name: "json", layer: Layer::C0, risk: Risk::R0,
        feature: None, timeout_cap_ms: 0, rate_per_sec: 0, api_ver: 1, desc: "JSON 编解码（T8）" },
];
```

并在 `api/tests.rs` 追加真注册断言（复用已有 register 测试的 HostContext mock）：

```rust
#[test]
fn capability_cards_match_registered_ctx() {
    // （复用本文件既有 register 场景）注册后遍历 CAPABILITIES：
    // name 以 "ctx:" 开头的 → ctx 表上该方法必须存在
    for c in crate::plugin::lua_core::capability::CAPABILITIES {
        if let Some(method) = c.name.strip_prefix("ctx:") {
            assert!(ctx.get::<_, mlua::Function>(method).is_ok(), "{} 未注册", c.name);
        }
    }
}
```

**Step 4-5:** 跑测试全绿 → commit `feat(lua): 能力卡常量表 + 一致性测试锁（Q2/§4.2）`

### Task 4: 护栏 P0/P1——超时 clamp + hub send 防反压（Q3/附录 B 冻结面①②）

**Files:**
- Modify: `src/plugin/lua_core/io_engine/mod.rs:53-69`（request clamp）
- Modify: `src/plugin/lua_core/api/http.rs:32,59`（timeout clamp）
- Modify: `src/plugin/lua_core/api/mod.rs` push_sample + `builtin/lua_client/mod.rs` collect 分支（send 超时）

**Step 1: 失败测试**（io_engine/mod.rs tests 追加）

```rust
#[tokio::test]
async fn request_timeout_is_clamped_to_30s() {
    let (tx, rx) = mpsc::channel::<IoRequest>(1);
    std::mem::forget(rx); // 保接收端活着 → 挂起等超时
    let engine = IoEngine { tx };
    let started = std::time::Instant::now();
    let _ = engine.request(b"x", 3_600_000).await; // 脚本传 1 小时
    assert!(started.elapsed() < std::time::Duration::from_secs(35),
        "clamp 后不应等满脚本超时（附录 B 冻结面②）");
}
```

（http.rs 同理：`timeout_secs` 传 3600 断言 30s 封顶——复用未启动本地 server 让 reqwest 走完超时。）

**Step 2: 跑** 确认 FAIL（当前无 clamp 会挂 1 小时——测试加 `#[ignore]` 保护？不：先断言失败于 35s 内不成立会真的等 1 小时。**改用逻辑单测**：把 clamp 提为纯函数 `pub fn clamp_timeout_ms(v: u64) -> u64 { v.min(30_000).max(1) }`，测试直接断言 `clamp_timeout_ms(3_600_000) == 30_000`——秒级完成。io request/http 两个调用点改用该函数。）

**Step 3: 实现**——`lua_core/capability.rs` 加 `pub fn clamp_timeout_ms`；`IoEngine::request` 首行 `let timeout_ms = crate::plugin::lua_core::capability::clamp_timeout_ms(timeout_ms);`；http.rs 两处 `let timeout_secs = timeout.unwrap_or(5).max(1);` 改为 clamp 上限 30（秒）。

**Step 4: hub send 防反压**——`api/mod.rs` `push_sample` 尾部与 `builtin/lua_client/mod.rs` collect 分支的 `tx.send(sample).await` 统一改为：

```rust
match tokio::time::timeout(std::time::Duration::from_secs(2), tx.send(sample)).await {
    Ok(Ok(())) => {}
    Ok(Err(_)) => tracing::warn!("[luaClient] hub 关闭，值未上报"),
    Err(_) => { dropped.fetch_add(1, Ordering::Relaxed);
        tracing::warn!("[luaClient] hub 背压超 2s，丢样（累计 {dropped}）"); }
}
```

`dropped: Arc<AtomicU64>` 新增经 `api::register` 传入（签名 +1 参数），collect 分支共享同一实例；flush 时 `tracker.extra_insert("dropped_samples", …)`。失败测试先行：构造满 channel（容量 1 塞 1 条不消费）断言 send 2s+ε 返回而非永挂（用 `tokio::time::pause` 加速）。

**Step 5-6:** 全测试 → commit `feat(lua): 护栏 P0/P1——超时 clamp 30s + hub send 2s 防反压（Q3/附录 B）`

### Task 5: 护栏——callback_timeout_ms + R2 频控（依 spike-1 结论）

**Files:**
- Modify: `src/plugin/lua_core/config.rs`（`callback_timeout_ms: u64` 默认 10_000，parse_cfg 读 `callback_timeout_ms` 键）
- Modify: `src/plugin/lua_core/api/mod.rs`（http 入口频控）+ `builtin/lua_client/mod.rs`（回调包 timeout）
- Create: `src/plugin/lua_core/ratelimit.rs`（滑动窗口计数器）

**Step 1: 失败测试**——ratelimit.rs：

```rust
/// 每秒滑动窗口计数（Q3：R2 频控，默认 20/s；超限返回 false 由调用方转错误）。
pub(crate) struct RateLimiter { limit: u32, window: std::time::Duration,
    cur: std::sync::atomic::AtomicU32, window_start: std::sync::Mutex<std::time::Instant> }
impl RateLimiter {
    pub(crate) fn new(limit: u32) -> Self { /* limit, window=1s, cur=0, start=now */ }
    pub(crate) fn check(&self) -> bool {
        // 窗口过期则重置；cur < limit → fetch_add 放行 true，否则 false
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn allows_up_to_limit_then_blocks() {
        let rl = RateLimiter::new(3);
        assert!(rl.check() && rl.check() && rl.check());
        assert!(!rl.check(), "第 4 次应被限流");
    }
}
```

**Step 2-3:** 跑 FAIL → 补完实现 → PASS。

**Step 4: 接线**——① `http.rs` 闭包捕获 `Arc<RateLimiter>`（RateLimiter::new(20)），`http_request` 前 `if !rl.check() { return Err(RuntimeError("rate limited（20/s）".into())) }`；② `builtin/lua_client/mod.rs`：`call_collect/call_on_write/call_on_data` 三处调用点包 `tokio::time::timeout(Duration::from_millis(cfg.callback_timeout_ms), …)`，`Err(Elapsed)` 分支：`tracker.tick_fail("callback timeout")` + 按 **spike-1 结论**：方案 a 仅告警继续（协程弃留）；方案 b 调 `reload_vm` 重建。测试：脚本 collect 内 `while true do end`（hook 熔断已有测试）+ collect 内调 pending 宿主函数（T1 场景）断言 10s（测试用 200ms 配置）超时后实例仍响应下一 tick。

**Step 5-6:** 全测试 → commit `feat(lua): 回调时长预算 + R2 频控护栏（Q3）`

---

## 组 3：require 路由与 stdlib

### Task 6: loader 命名空间路由（§3.1/Q19 D11）

**Files:**
- Modify: `src/plugin/lua_core/loader.rs`、`script.rs`

**Step 1: 失败测试**（loader.rs tests 追加）

```rust
use super::{route, ModuleSource};
#[test]
fn route_by_first_segment() {
    assert_eq!(route("json").unwrap(), ModuleSource::Builtin);
    assert_eq!(route("algo.crc16").unwrap(), ModuleSource::Stdlib);
    assert_eq!(route("cloud.onenet.token").unwrap(), ModuleSource::Stdlib);
    assert_eq!(route("viccom.calcex").unwrap(), ModuleSource::Shared);
    assert_eq!(route("myutil").unwrap(), ModuleSource::Private);
    assert_eq!(route("lib.util").unwrap(), ModuleSource::Private); // 兼容旧 lib. 前缀
}
#[test]
fn route_rejects_violations() {
    assert!(route("").is_err());
    assert!(route("viccom").is_err(), "共享层单段被拒（D11：第三方须 ≥2 段）");
    assert!(route("../evil").is_err());           // 防穿越（沿用现有规则）
    // 首段判别：viccom.algo 合法（algo 是 viccom 的库名）；拒的是首段 ∈ 保留段但 _stdlib/ 无文件
}
```

**Step 2: 实现**（loader.rs）

```rust
pub(crate) const RESERVED_TOP: &[&str] = &["algo", "codec", "proto", "cloud", "util", "oo"];
use crate::plugin::lua_core::capability::BUILTIN_MODULES;

pub(crate) enum ModuleSource { Builtin(String), Stdlib(PathBuf), Shared(PathBuf), Private(PathBuf) }

/// 首段路由（设计 §3.1）。返回 Err=违规名（含穿越）。
pub(crate) fn route(scripts_root: &Path, script_name: &str, req: &str) -> Result<ModuleSource, String> {
    if req.contains("..") || req.starts_with('/') || req.contains('\\') { return Err("路径违规".into()); }
    let parts: Vec<&str> = req.split('.').collect();
    if parts.iter().any(|p| p.is_empty()) { return Err("空段".into()); }
    let first = parts[0];
    // builtin 名单优先（json 等是单段名，须在单段=私有的判断之前）
    if BUILTIN_MODULES.contains(&first) { return Ok(ModuleSource::Builtin(first.to_string())); }
    // 兼容旧 "lib." 前缀 → 私有
    let priv_path = |parts: &[&str]| { let r = if parts[0]=="lib" {&parts[1..]} else {parts};
        scripts_root.join(script_name).join("lib").join(format!("{}.lua", r.join("/"))) };
    if parts.len() == 1 || first == "lib" {
        return Ok(ModuleSource::Private(priv_path(&parts)));
    }
    if RESERVED_TOP.contains(&first) {
        // 官方层：_stdlib/<路径>.lua（永不回落共享/私有——防投毒核心规则）
        return Ok(ModuleSource::Stdlib(scripts_root.join("_stdlib").join(format!("{}.lua", parts.join("/")))));
    }
    // 第三方共享：_lib/<作者>/<库路径>.lua（强制 ≥2 段已由上面 len==1 分支保证）
    Ok(ModuleSource::Shared(scripts_root.join("_lib").join(first)
        .join(format!("{}.lua", parts[1..].join("/")))))
}
```

`script.rs` 受控 require 改为按 `ModuleSource` 分发：Private 读文件（现状逻辑）；Builtin → 调 `lua_core::api::builtin_module(name, lua)`（T8 实现，本 task 先返回 Err("json 未实现")）；Stdlib/Shared 读对应路径（不存在 → 明确报错"官方库未同步，请 sync 或检查版本"）。**Stdlib 路径不存在时先查 embed 种子（T7）**——T6 时种子未就绪，留 `todo!()` 于 T7 接线。

**Step 3-5:** 测试全绿 → commit `feat(lua): require 首段命名空间路由 + 违规强制拒绝（§3.1/D11）`

### Task 7: stdlib 种子 embed（FreeIOE 4 库搬运 + selftest 约定）

**Files:**
- Create: `src/plugin/lua_core/stdlib_seed/algo/crc16.lua`、`codec/basexx.lua`、`util/retry.lua`、`util/tablex.lua`
- Create: `src/plugin/lua_core/stdlib.rs`（rust-embed + 加载 + selftest 冒烟）
- Modify: `script.rs`（Stdlib 分支接种子兜底）、`Cargo.toml`（lua feature 加 `dep:rust-embed`——rust-embed 已是无条件依赖，仅挂 feature 即可）

**Step 1: 搬运**——源 `I:\code\freeioe\lualib\`：
- `hwtest/crc16.lua`（38 行）→ `algo/crc16.lua`：头部加 `-- vendored from freeioe@1398d47 2026-08-19 (MIT)`，改为模块化（`local M = {} … return M`），追加：

```lua
function M.selftest()
    -- Modbus CRC16 已知向量：01 03 00 00 00 01 → 0x840A（低字节在前传输）
    local crc = M.crc16("\x01\x03\x00\x00\x00\x01")
    return crc == 0x840A or crc == 0x0A84  -- 按实现字节序，二者其一
end
```
- `basexx.lua` → `codec/basexx.lua`：原样 + vendored 头 + 底部 `M.selftest = function() return M.from_base64(M.to_base64("rsiot")) == "rsiot" end`（挂进其返回表 M）。
- `utils/retry.lua` → `util/retry.lua`、`utils/table/equals.lua` → `util/tablex.lua`（表名 equals）：各加 vendored 头 + 简单 selftest。

**Step 2: stdlib.rs**

```rust
//! stdlib 种子（rust-embed，设计 §3.6 D5：内网冷启动兜底；权威源在官方仓 rsiot-lualib）。
#[derive(rust_embed::RustEmbed)]
#[folder = "src/plugin/lua_core/stdlib_seed/"]
struct Seed;

pub(crate) fn seed_get(path: &str) -> Option<Vec<u8>> { // "algo/crc16" → 文件内容
    Seed::get(&format!("{path}.lua")).map(|f| f.data().to_vec())
}

/// 启动/CI 冒烟：全部 selftest（设计 §3.3 质量闸门）。
pub(crate) fn seed_selftest_all(lua: &mlua::Lua) -> mlua::Result<usize> {
    let mut ok = 0;
    for path in ["algo/crc16", "codec/basexx", "util/retry", "util/tablex"] {
        let src = seed_get(path).expect("种子内嵌");
        lua.load(&src).exec()?;                       // 语法闸门（FreeIOE 有语法错误先例）
        let m: mlua::Table = lua.globals().get(path.replace('/', "_"))?; // 见下：模块表注册策略
        let f: mlua::Function = m.get("selftest")?;
        if f.call::<bool>(())? { ok += 1 } else { return Err(RuntimeError(format!("{path} selftest=false"))); }
    }
    Ok(ok)
}
```

（模块加载策略：`script.rs` Stdlib 分支 = `seed_get(path)` → `lua.load(src).eval()`，模块文件自身 `return M`——require 返回值即 M，**不入 globals**，与 Lua require 语义一致；selftest_all 单独 load 后从 eval 返回值取表，不走 globals——修正上面示例为 `let m: mlua::Table = lua.load(&src).eval()?;`）

**Step 3: 失败测试**（stdlib.rs tests）——`seed_selftest_all` 在新 VM 上返回 4；`require("algo.crc16")` 经 script.rs 分发能取回模块且 `selftest()==true`（走 script.rs 的 require 注入路径集成测试，追加到 api/tests.rs）。

**Step 4-5:** 全绿 → commit `feat(lua): stdlib 种子 embed——FreeIOE 4 库 + selftest 闸门（§3.6 D5）`

### Task 8: builtin json 模块（C0，输入上限）

**Files:**
- Create: `src/plugin/lua_core/api/json.rs`
- Modify: `script.rs` Builtin 分支、`capability.rs`（json 卡已有占位）

**Step 1: 失败测试**（api/tests.rs）

```rust
#[tokio::test]
async fn json_roundtrip_and_limit() {
    // 经完整 VM：load_main 一个 require("json") 的脚本，调 json.encode/decode 往返
    // 1) encode({a=1,b="x"}) → '{"a":1,"b":"x"}'（key 序不做断言，parse 回来比对值）
    // 2) decode('{"k":[1,2,3]}').k[2] == 2
    // 3) decode 超 1MB 字符串 → RuntimeError "json: 输入超限"
    // 4) decode 非法 JSON → err 含 "解析失败"
}
```

**Step 2: 实现**（api/json.rs）

```rust
pub(crate) fn make_module(lua: &mlua::Lua) -> mlua::Result<mlua::Table> {
    const MAX_BYTES: usize = 1024 * 1024;
    let t = lua.create_table()?;
    let encode = lua.create_function(|lua, v: mlua::Value| {
        let j: serde_json::Value = lua.from_value(v)
            .map_err(|e| mlua::Error::RuntimeError(format!("json encode: {e}")))?;
        serde_json::to_string(&j).map_err(|e| mlua::Error::RuntimeError(format!("json encode: {e}")).into())
    })?;
    let decode = lua.create_function(|lua, s: mlua::LuaString| {
        let b = s.as_bytes();
        if b.len() > MAX_BYTES { return Err(mlua::Error::RuntimeError("json: 输入超 1MB".into())); }
        let j: serde_json::Value = serde_json::from_slice(b)
            .map_err(|e| mlua::Error::RuntimeError(format!("json 解析失败: {e}")))?;
        lua.to_value(&j)
    })?;
    t.set("encode", encode)?; t.set("decode", decode)?;
    Ok(t)
}
```

`script.rs` Builtin 分支：`"json" => api::json::make_module(lua)?.into()`。

**Step 3-5:** 全绿 → commit `feat(lua): builtin json 模块（C0/上限 1MB，§4.1/D5）`

### Task 9: 本地 index + stdlib.list() + sync 客户端（D1/D2 核心）

**Files:**
- Create: `src/plugin/lua_core/stdlib_sync.rs`（index 读写/验签/下载/原子替换/pin）
- Modify: `script.rs`（Stdlib 分支：`_stdlib/` 文件优先，miss 查种子）、`Cargo.toml`（`lua` feature + `dep:ed25519-dalek`）、REST `src/api/routes_lua.rs`（两端点）

**Step 1: 失败测试**（stdlib_sync.rs tests，全用本地文件+自签 key，不发网络）

```rust
fn dev_keypair() -> (ed25519_dalek::SigningKey, ed25519_dalek::VerifyingKey) {
    use ed25519_dalek::SigningKey;
    let sk = SigningKey::from_bytes(&[7u8; 32]); // 确定性测试钥
    (sk, sk.verifying_key())
}
#[test]
fn index_verifies_and_rejects_tamper() {
    // 造 index.json {"version":"1.0.0","files":{"algo/crc16.lua":{"sha256":"..","ver":"1.0.0"}}}
    // 签名 → verify_index(&bytes, &sig, &vk) == Ok(idx)
    // 篡改 1 字节再验 → Err（镜像被劫持场景，§3.7 供应链对策）
}
#[test]
#[should_panic(expected = "sha256")]
fn sync_rejects_bad_file_hash() {
    // 本地 file:// upstream 目录（index 声明的 sha 与文件实际不符）→ sync 返回 Err
}
```

**Step 2: 实现**（stdlib_sync.rs 核心，约 200 行）

```rust
pub(crate) const VERIFY_KEYS: &[[u8; 32]] = &[/* TODO(T13): 官方公钥，v1 先放 dev 钥 */];
pub(crate) struct LocalIndex { pub version: String, pub files: std::collections::BTreeMap<String, FileEntry> }
// read_index(scripts_root) -> Option<LocalIndex>   // scripts/_stdlib/.index.json
// verify_index(bytes, sig, keys) -> Result<LocalIndex>  // 逐 key 试验签
// sync(upstream: &str, scripts_root, pin: Option<&str>) -> Result<SyncReport>
//   1. GET {upstream}/index.json + index.json.sig（reqwest，10s 超时）
//   2. verify_index → 校验 min_api_ver（对比 CUR_API_VER 常量=1）
//   3. 逐文件比对本地 sha256（sha2 已依赖）→ 下载缺失/变更 → 校验 sha → 写 _stdlib.tmp/
//   4. 原子 rename _stdlib.tmp → _stdlib + 写 .index.json
//   5. 返回 SyncReport{added, updated, removed, version}
pub(crate) fn stdlib_table(lua) -> mlua::Table  // stdlib.list()/info(mod)：index+种子合并视图
```

**Step 3: REST**（routes_lua.rs）——`GET /api/v1/lua/stdlib`（返回 list + version + 来源徽标 seed/synced）、`POST /api/v1/lua/stdlib/sync`（body `{upstream?, pin?}`；默认读 AppConfig 新键 `lua_stdlib_upstream`；成功后**全实例 reload**：遍历 lux instances 中 app_code ∈ {luaClient} 的实例逐一发 reload（复用 routes_lua 既有单实例 reload 逻辑提为 helper `reload_all_lua_instances()`，Q11））。

**Step 4-5:** 测试（index 验签/坏 hash/原子替换后 require 命中 `_stdlib/` 文件优先于种子）全绿 → commit `feat(lua): stdlib 本地 index + sync 客户端 + stdlib.list + REST（§3.5/3.6 D1）`

### Task 10: `_lib/` 站点共享库（Q13/Q15）

**Files:**
- Modify: `src/api/routes_lua.rs`（save/list/versions 扩 kind 参数）、`lua_core/hotload.rs`（全局 `_lib` watcher）

**Step 1: 失败测试**——routes_lua 现有 save 测试旁追加：`kind=lib` + path `viccom/hex.lua` 保存成功且落 `scripts/_lib/viccom/hex.lua`；`kind=lib` + path `algo/evil.lua` **被拒**（保留段，D11）；`kind=lib` 单段 `foo.lua` 被拒；版本快照落 `scripts/_lib/viccom/versions/`。

**Step 2: 实现**——save 端点路径校验函数抽出 `validate_script_path` → 加 `validate_lib_path`（≥2 段、首段非保留段、防穿越同规则）；`lua_core/hotload.rs` 新增 `spawn_global_lib_watcher(scripts_root, token)`：`Recursive` 监听 `scripts/_lib/`，去抖 200ms → `reload_all_lua_instances()`（T9 helper，Q15 全实例策略）；`run()` 启动处挂一个（per-instance watcher 不动）。

**Step 3-5:** 全绿 → commit `feat(lua): _lib 站点共享库——REST kind=lib + 保留段拒绝 + 全局 watcher 全实例 reload（Q13/Q15）`

---

## 组 4：manifest 与 dry-run

### Task 11: manifest 头部解析 + dry-run 校验（Q1/API_VER）

**Files:**
- Create: `src/plugin/lua_core/manifest.rs`
- Modify: `builtin/lua_client/mod.rs`（启动与 reload_vm 接 dry-run）

**Step 1: 失败测试**（manifest.rs tests）

```rust
use super::parse;
#[test]
fn parses_header_block() {
    let src = "-- @role collector\n-- @api_ver 1\n-- @stdlib >=1.0\n\
               -- @param period number 可选 上报周期秒 默认5 范围:1-3600\n\
               -- @source local\nlocal x = 1";
    let m = parse(src).unwrap();
    assert_eq!(m.role.as_deref(), Some("collector"));
    assert_eq!(m.api_ver, 1);
    assert_eq!(m.stdlib.as_deref(), Some(">=1.0"));
    assert_eq!(m.source.as_deref(), Some("local"));
    assert_eq!(m.params.len(), 1);
    assert_eq!(m.params[0].name, "period");
}
#[test]
fn defaults_when_absent() {
    let m = parse("-- hi").unwrap();
    assert_eq!(m.api_ver, 1);              // 缺省 1
    assert_eq!(m.role.as_deref(), Some("collector")); // 缺省 collector
    assert!(m.source.is_none());
}
```

**Step 2: 实现**——`Manifest{role,api_ver,stdlib,source,params:Vec<ParamDecl>}`；`parse` 行扫描 `^--\s@(\w+)\s+(.*)$`；`ParamDecl{name,ty,required,default,constraints}`（`默认X`/`枚举:a|b`/`范围:1-3600` 后缀解析）；**dry_run 校验函数**：

```rust
pub(crate) const CUR_API_VER: u32 = 1;
pub(crate) const MIN_API_VER: u32 = 1;
pub(crate) fn dry_run(m: &Manifest, lua: &Lua, stdlib_ver: Option<&str>) -> Result<(), String> {
    if m.api_ver > CUR_API_VER { return Err(format!("脚本 api_ver {} > 宿主 {CUR_API_VER}，请升级宿主", m.api_ver)); }
    if m.api_ver < MIN_API_VER { return Err(format!("api_ver {} < {MIN_API_VER}，请升级脚本", m.api_ver)); }
    // role=collector：collect 必须存在（v1 唯一角色）
    if m.role.as_deref().unwrap_or("collector") == "collector"
        && lua.globals().get::<mlua::Function>("collect").is_err() {
        return Err("collector 角色缺少 collect()".into());
    }
    // @stdlib 范围 vs 本地 index version（简单前缀比较 >=X.Y / <X），不满足给可行动错误
    Ok(())
}
```

**Step 3: 接线**——`run()` 与 `reload_vm()` 在 load_main 后：读 main.lua 源（load_main 返回源或重读文件）→ parse → dry_run → Err 则启动 warn+return / reload 回退旧 VM（Q1/§2.2）。

**Step 4-5:** 全绿 → commit `feat(lua): manifest 头部解析 + dry-run（role/api_ver/stdlib 校验，§2.1-2.2/§4.4）`

### Task 12: 示例迁移 + 文档同步（阶段 A 收口）

**Files:**
- Modify: `examples/lua-scripts/modbus-like/main.lua`（删手写 crc16 → `local crc16 = require("algo.crc16")`；`examples/lua-scripts/README.md`）、`docs/guides/lua-script-guide.md`

**Step 1:** modbus-like 改造：删除 5-15 行手写 crc16，头部加 `local crc16 = require("algo.crc16")`（v0.5 契约：`crc16.crc16(buf)` 或直接函数——与 T7 模块面一致：`local M={}; M.crc16=…; return M`，脚本用 `crc16.crc16(req)`）；README 注明依赖 stdlib ≥1.0。

**Step 2:** lua-script-guide.md 增补：§命名空间路由表（§3.1 四层+违规示例）、§stdlib 种子清单与 selftest、§json 模块、§护栏参数表（callback_timeout_ms/rate/clamp/丢样）、§manifest 语法、§sync/`_lib` 使用、兼容矩阵表（宿主 × API_VER=1 × STDLIB 1.0）。

**Step 3: 验证**——`cargo test -p rsiot --features lua,lua-http` 全绿；手动冒烟：临时 scripts 目录放 modbus-like 实例跑一个 tick（mock 或 it_modbus 集成路径），确认 `require("algo.crc16")` 生效（route A 验收标准之一）。

**Step 4: Commit** `docs(lua)+examples: 阶段 A 收口——示例迁移 stdlib + 指南同步`

### Task 13: 官方源仓 rsiot-lualib 建制（跨仓，D1 上游）

**Files:**（新 GitHub 仓 `rsiot-lualib`，非本仓改动；产物回填 T9 公钥）

**Step 1: 仓结构**

```
rsiot-lualib/
├── stdlib/algo/crc16.lua          ← 从主仓 stdlib_seed/ 拷入（初始反向：主仓为源）
├── stdlib/codec/basexx.lua
├── stdlib/util/{retry,tablex}.lua
├── scripts/                        ← 官方脚本模板（首发 it_modbus 改造版）
├── gen_index.sh                    ← 收集文件→sha256→index.json
├── sign_index.sh                   ← openssl pkeyutl -sign -rawin -inkey $KEY
├── .github/workflows/release.yml
└── README.md                       ← 命名规范/版本策略/镜像说明（§3.1/3.3 摘要）
```

**Step 2: 脚本**——`gen_index.sh`：`find stdlib scripts -name '*.lua' | while read f; do sha=$(sha256sum "$f" | cut -d' ' -f1); … done > dist/index.json`（jq 组装 `{version: $TAG, files: {path: {sha256, ver}}, min_api_ver: 1}`）；`sign_index.sh`：`openssl pkeyutl -sign -rawin -inkey ed25519.key -in dist/index.json | base64 > dist/index.json.sig`。

**Step 3: CI**（release.yml 核心）：tag push → 跑两脚本 → `actions/upload-pages-artifact` 或 attach Release（Q16 静态托管定案）；`ED25519_KEY` 为 encrypted secret（Q17：CI secret 保管制）。

**Step 4: 密钥与回填**——`openssl genpkey -algorithm ed25519 -out ed25519.key`；公钥 `openssl pkey -in ed25519.key -pubout -outform DER | tail -c 32` → 32 字节填入主仓 `stdlib_sync.rs::VERIFY_KEYS`（替换 dev 钥），私钥入 CI secret（**绝不入仓**，硬性规则）。

**Step 5: 端到端验收**——本地起 `python -m http.server` 于 dist/ 作 upstream → 主仓 `POST /api/v1/lua/stdlib/sync {"upstream":"http://127.0.0.1:8000"}` → 断言 `_stdlib/` 落文件、`stdlib.list()` 徽标 synced、篡改任一文件后 sync 拒绝。两仓分别 commit（源仓 `chore: initial stdlib 1.0.0`）。

---

## 验收清单（对照路线图阶段 A）

- [x] 现有 lua 测试全绿（重构零行为变化）+ 新增护栏/路由/stdlib/manifest 测试全绿
- [x] `require("algo.crc16")` 生效（种子与 `_stdlib/` 双路径）；modbus-like 示例迁移后行为不变
- [x] 命名违规被拒：共享层单段、`kind=lib` 撞保留段、路径穿越
- [x] 离线冷启动：空 `_stdlib/` 时种子兜底可用
- [x] 镜像/upstream 切换 + 篡改拒绝（验签）
- [x] spike-1 结论落档；护栏五项参数可见于能力卡与指南
- [x] 兼容矩阵成文（lua-script-guide.md）

> B/C/D 阶段计划待 A 落地后按同法拆分（B：luaNorth/Sink 接线/mqtt-north/secret/EMQX→OneNET；C：luaTransform/EventBus 带值/自环防护；D：Sparkplug/alarm/透传/脚本包）。
