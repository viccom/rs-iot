# Lua 脚本采集 — M6 热加载实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development 实现本计划。步骤用 checkbox（`- [ ]`）。由 `/loop` 调度（CronCreate `116fd1e9`，每 10m 一轮）。

**Goal:** 补齐 luaClient 的脚本热加载——`notify` 监听 `scripts/<name>/` 改动 → 去抖 200ms → Source task 内编译新 VM（沙箱+hook）→ dry-run（on_init 验证不死循环）→ 成功切换 `lua = new_lua`（旧 VM drop）/ 失败 warn 回退（旧 VM 继续跑，永不半生效）。spec §5.3。

**Architecture:** 主循环改 **Evt 枚举模式**（spec §5.4）—— `select!` 产 `Evt`（Cmd/Data/Tick/Reload），`match evt` 单分支处理，解耦 `&lua` 借用，支持 `Evt::Reload` 分支 `lua = new_lua`（&mut）与其他分支 `call_*(&lua)`（&）共存。notify watcher 独立 task（监听 + debouncer 去抖）→ `reload_tx` → 主循环 `Evt::Reload` → `reload_vm`。IO task / ctx_host / io_engine 不随 VM 替换（连接持续，新 VM 复用）。

**Tech Stack:** notify 8 + notify-debouncer-mini 0.7（去抖）、mlua 0.12（VM 重建）、tokio mpsc（reload 信号）。

**执行顺序：** Task 1（Cargo deps）→ Task 2（Evt 重构 + reload_vm + Evt::Reload，手动 reload_tx 测）→ Task 3（notify watcher 生产触发）→ Task 4（集成测）。

**已知简化 / 风险（注明）：**
- **dry-run = 编译 + load + on_init**（hook 验证不死循环）。spec §5.3 说「on_init + 一次 collect」，但 collect dry-run 会真发 request（副作用），v1 简化为 on_init only（hook 兜底死循环；collect 在下 tick 真跑，失败 warn 跳过）。
- **notify Windows 事件丢失**（spec §18，ReadDirectoryChangesW 已知）→ 去抖 + M7 REST 手动 reload 兜底。
- **VM 重建不重启 IO task / IO 连接**（连接持续；新 VM 复用 io_engine）。若脚本依赖「重载重连」，需手动重启实例（InstanceManager stop/start）。
- notify-debouncer-mini 0.7 API（`Debouncer` 构造 + 事件）实现时按实际 API 调整（plan 给基于常见用法的设计）。

---

## File Structure

| 文件 | 动作 | 职责 |
|---|---|---|
| `Cargo.toml` | Modify | lua feature 加 `dep:notify,dep:notify-debouncer-mini` |
| `src/plugin/builtin/lua_client/mod.rs` | Modify | Evt 枚举 + 主循环 Evt 重构 + reload channel + `reload_vm` + Evt::Reload 替换 + spawn notify watcher |
| `src/plugin/builtin/lua_client/hotload.rs` | **Create** | `spawn_watcher`（notify-debouncer 监听 + 去抖 → reload_tx） |

---

## Task 1: Cargo.toml lua feature 加 notify + notify-debouncer-mini

**Files:** Modify `Cargo.toml`

### - [ ] Step 1：加依赖 + feature

dependencies 加（mlua 行后）：
```toml
# Lua 脚本热加载（lua feature）：notify 监听 scripts/ 改动 + notify-debouncer-mini 去抖。
# notify-debouncer-mini 0.7 依赖 notify ^8.2（已核实 crates.io，0.5 要 notify 7 会冲突）。
notify = { version = "8", optional = true }
notify-debouncer-mini = { version = "0.7", optional = true }
```

features `lua` 加 notify + debouncer：
```toml
lua = ["dep:mlua", "dep:notify", "dep:notify-debouncer-mini"]
```

### - [ ] Step 2：验证编译 + fmt + clippy + 提交

```bash
CARGO_BUILD_JOBS=1 cargo build
cargo fmt --all --check && CARGO_BUILD_JOBS=1 cargo clippy --all-targets -- -D warnings
git add Cargo.toml Cargo.lock
git commit -m "feat(lua): lua feature 加 notify + notify-debouncer-mini（M6 热加载依赖）"
```

---

## Task 2: mod.rs Evt 重构 + reload_vm + Evt::Reload

**Files:** Modify `src/plugin/builtin/lua_client/mod.rs`

### - [ ] Step 1：Evt 枚举 + 主循环 Evt 重构

mod.rs 顶部（`use` 后）加 Evt 枚举：
```rust
/// 主循环事件（spec §5.4）：解耦 select! 与 `&lua` / `&mut lua` 借用，
/// 使 `Evt::Reload` 的 `lua = new_lua`（&mut）与其他分支 `call_*(&lua)`（&）共存。
enum Evt {
    /// 写命令（cmd_rx）。
    Cmd(Option<CtlMessage>),
    /// on_data 被动帧（data_rx）。
    Data(Vec<u8>),
    /// 采集周期（ticker）。
    Tick,
    /// 热加载信号（reload_rx，notify watcher 触发）。
    Reload,
}
```

主循环（`loop { tokio::select! { ... } }`，约 line 120-159）整体替换为 Evt 模式：
```rust
        // reload channel：notify watcher → 主循环 Evt::Reload（Task 3 接 watcher）。
        let (reload_tx, mut reload_rx) = mpsc::channel::<()>(8);

        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(cfg.period.max(1)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tracing::info!(
            "[luaClient] inst {} started (script={}, period={}s)",
            inst.inst_id,
            cfg.script,
            cfg.period
        );

        let mut lua = lua; // 可变（Evt::Reload 替换）
        loop {
            let evt = tokio::select! {
                _ = token.cancelled() => break,
                msg = cmd_rx.recv() => Evt::Cmd(msg),
                Some(frame) = data_rx.recv() => Evt::Data(frame),
                _ = reload_rx.recv() => Evt::Reload,
                _ = ticker.tick() => Evt::Tick,
            };
            match evt {
                Evt::Cmd(Some(m)) => {
                    if m.cmd == "write" {
                        if let Err(e) = call_on_write(&lua, &m.data).await {
                            tracing::warn!("[luaClient] on_write 失败：{e}");
                        }
                    }
                }
                Evt::Cmd(None) => break,
                Evt::Data(frame) => {
                    if let Err(e) = call_on_data(&lua, &frame).await {
                        tracing::warn!("[luaClient] on_data 失败：{e}");
                    }
                }
                Evt::Tick => {
                    if cfg.period == 0 {
                        continue;
                    }
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
                                if tx.send(sample).await.is_err() {
                                    break;
                                }
                            }
                            Ok(_) => {}
                            Err(e) => tracing::warn!("[luaClient] collect 失败：{e}"),
                        }
                    }
                }
                Evt::Reload => {
                    match reload_vm(&cfg, &lctx, ctx.clone(), &inst, io_engine.clone()).await {
                        Ok(new_lua) => {
                            lua = new_lua;
                            tracing::info!("[luaClient] inst {} 热加载成功", inst.inst_id);
                        }
                        Err(e) => tracing::warn!(
                            "[luaClient] inst {} 热加载失败，旧 VM 继续：{e}",
                            inst.inst_id
                        ),
                    }
                }
            }
        }
```

> **注意：** 原 `let lua = ...`（line 61）改为后续 `let mut lua = lua;`（主循环前）。reload_tx 暂不用（Task 3 spawn watcher；本 task 手动测可发 reload_tx）。reload_rx 在 select!（无 watcher 时恒 pending，不影响）。

### - [ ] Step 2：reload_vm helper

mod.rs（`call_on_data` 后）加：
```rust
/// 重建 VM：new_vm + load_main + register ctx + dry-run on_init。
///
/// IO engine / ctx_host 复用（不随 VM 替换）。dry-run 跑 on_init（hook 验证不死循环）；
/// collect dry-run 跳过（真 collect 在下 tick，失败 warn）。失败返回 Err（主循环回退旧 VM）。
async fn reload_vm(
    cfg: &config::LuaCfg,
    lctx: &script::LoadCtx,
    ctx_host: crate::host::HostContext,
    inst: &InstConfig,
    io_engine: io_engine::IoEngine,
) -> mlua::Result<mlua::Lua> {
    let lua = engine::new_vm(cfg)?;
    script::load_main(&lua, lctx)?;
    api::register(&lua, ctx_host, inst, io_engine)?;
    // dry-run on_init（hook 兜底死循环；失败 → Err → 回退）。
    if lua.globals().get::<mlua::Function>("on_init").is_ok() {
        let r: mlua::Result<()> = lua.load("on_init()").eval_async().await;
        r?;
    }
    Ok(lua)
}
```

> **注意：** `engine` / `script` / `api` / `io_engine` 模块在 mod.rs 已 `mod`/`use`。`config::LuaCfg` / `script::LoadCtx` / `io_engine::IoEngine` 可见性确认（均 `pub(super)`，mod.rs 同模块可见）。`HostContext` clone（register 取所有权）。

### - [ ] Step 3：单测（手动 reload_tx 触发 Evt::Reload）

mod.rs tests 加（手动发 reload 信号，验 VM 替换）。但 run() 是长驻 task，单测困难——**改为集成测（Task 4）验证**。本 task 单测跳过（reload_vm 逻辑由集成测覆盖）。

### - [ ] Step 4：run 全量 lua_client + lua_modbus_e2e + fmt + clippy + 提交

```bash
CARGO_BUILD_JOBS=1 cargo test --lib plugin::builtin::lua_client
CARGO_BUILD_JOBS=1 cargo test --test lua_modbus_e2e
cargo fmt --all && cargo fmt --all --check && CARGO_BUILD_JOBS=1 cargo clippy --all-targets -- -D warnings
git add src/plugin/builtin/lua_client/mod.rs
git commit -m "feat(lua): M6 Evt 主循环重构 + reload_vm（VM 重建 dry-run，热加载核心）"
```
Expected: lua_client 24 测试绿（Evt 重构不破坏 collect/on_write/on_data）+ lua_modbus_e2e 1 passed。

---

## Task 3: notify watcher（生产热加载触发）

**Files:** Create `src/plugin/builtin/lua_client/hotload.rs`; Modify `mod.rs`

### - [ ] Step 1：hotload.rs spawn_watcher

新建 `src/plugin/builtin/lua_client/hotload.rs`：
```rust
//! 脚本热加载监听（spec §5.3）：notify-debouncer-mini 监听 scripts/<name>/，
//! 去抖 200ms → 改动经 reload_tx 通知主循环（Evt::Reload → reload_vm）。
//!
//! Windows notify 偶有事件丢失（spec §18 ReadDirectoryChangesW），去抖 + M7 REST reload 兜底。

use notify_debouncer_mini::{new_debouncer, DebouncedEvent};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// 启动热加载监听 task：监听 `scripts/<name>/` 改动，去抖 200ms → `reload_tx.send(())`。
///
/// `script_dir` = `scripts/<name>` 全路径。watcher 在独立 task（notify 回调 → reload_tx）。
/// 失败仅 warn（监听不到不致命，靠 M7 REST 手动 reload 兜底）。
pub(super) fn spawn_watcher(script_dir: PathBuf, reload_tx: mpsc::Sender<()>) {
    tokio::task::spawn_blocking(move || {
        let tx_for_notify = reload_tx.clone();
        let mut debouncer = match new_debouncer(
            Duration::from_millis(200),
            move |_evs: Result<Vec<DebouncedEvent>, _>| {
                // 任何改动事件 → 通知主循环（去抖后）。
                let _ = tx_for_notify.blocking_send(());
            },
        ) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("[luaClient] 热加载 watcher 启动失败：{e}");
                return;
            }
        };
        if let Err(e) = debouncer.watcher().watch(
            &script_dir,
            notify::RecursiveMode::NonRecursive,
        ) {
            tracing::warn!("[luaClient] 热加载 watch {script_dir:?} 失败：{e}");
            return;
        }
        // 阻塞保 watcher 活；task 在主进程退出时随进程结束。
        // 注：reload_tx drop（主循环退出）时 blocking_send 失败 → watcher 回调无副作用。
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    });
}
```

> **API 注意（实现时按 notify-debouncer-mini 0.7 实际调整）：** `new_debouncer(timeout, callback)` 返回 `Debouncer`；回调收 `Result<Vec<DebouncedEvent>, notify::Error>`。`debouncer.watcher().watch(path, RecursiveMode)`。若 0.7 API 签名不同（如 `DebouncedEventKind`），调整回调签名。`blocking_send`（spawn_blocking 内同步回调用 blocking_send，非 await）。

### - [ ] Step 2：mod.rs 接 spawn_watcher

mod.rs 顶部 `mod hotload;`（与 `mod api;` 等并列）。主循环前（reload_tx 建后）加：
```rust
        // notify watcher 监听 scripts/<name>/ 改动 → reload_tx（热加载）。
        let script_dir = scripts_root.join(cfg.script.as_str());
        hotload::spawn_watcher(script_dir, reload_tx.clone());
```

> 注意：`scripts_root`（PathBuf，run() 内 line 58）+ `cfg.script` 拼 script_dir。reload_tx clone（主循环持 reload_rx，watcher 持 reload_tx）。

### - [ ] Step 3：lua_client mod.rs 注册 hotload 模块 + fmt + clippy + 提交

mod.rs 顶部 `mod` 段加 `mod hotload;`。验证：
```bash
CARGO_BUILD_JOBS=1 cargo build --lib
cargo fmt --all && cargo fmt --all --check && CARGO_BUILD_JOBS=1 cargo clippy --all-targets -- -D warnings
git add src/plugin/builtin/lua_client/hotload.rs src/plugin/builtin/lua_client/mod.rs
git commit -m "feat(lua): M6 notify watcher（scripts/ 改动去抖 → 热加载触发）"
```

---

## Task 4: 热加载集成测（改脚本 → reload → 新 collect 值）

**Files:** Create `tests/lua_hotload_e2e.rs`

### - [ ] Step 1：写集成测

新建 `tests/lua_hotload_e2e.rs`（仿 lua_modbus_e2e InstanceManager 模式）：
```rust
//! 热加载端到端：起实例（脚本返回 v=1）→ 改 main.lua（v=2）→ notify 去抖 → reload_vm
//! → 新 collect 值 2。spec §5.3 / 测试 F（热加载）。

mod common;

use common::test_data_dir;
use rs_iot::host::{BridgeRegistry, CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::plugin::traits::Processor;
use rs_iot::transform::passthrough::Passthrough;
use rs_iot::types::{AppType, DeviceSpec, HostKind, Hwid, InstConfig, PluginName, Sample};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn hotload_picks_up_script_change() {
    let data_dir = test_data_dir("lua-hotload");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client).await.expect("bootstrap");

    // 临时脚本目录：v1 返回 v=1。
    let scripts = std::env::temp_dir().join(format!("lua-hl-{}", std::process::id()));
    let script_dir = scripts.join("hl");
    std::fs::create_dir_all(&script_dir).expect("mkdir");
    std::fs::write(
        script_dir.join("main.lua"),
        "function collect() return { v = 1 } end\n",
    )
    .expect("write v1");

    // mock tcp server（luaClient endpoint；collect 不 request 本测，endpoint 闲置即可，
    // 但 TcpIo 懒连接 + collect 不 request → 不连）。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);

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

    let inst = InstConfig {
        inst_id: "lua-hl".into(),
        inst_name: "hl".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: true,
        app_config: json!({
            "script": "hl",
            "kind": "tcp",
            "endpoint": format!("127.0.0.1:{port}"),
            "period": 1,
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "devHL".into(),
            children: vec![],
        }],
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start lua");

    // 等 v1 采集落库。
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let v1 = hget_cell(&client, "rtdata:devHL", "devHL.v").await;
    assert!(v1.contains("1"), "v1 应采集 v=1，得到 '{v1}'");

    // 改脚本：v=2 → notify 去抖 → reload。
    std::fs::write(
        script_dir.join("main.lua"),
        "function collect() return { v = 2 } end\n",
    )
    .expect("write v2");
    // 等去抖 200ms + reload_vm + 新 collect（period 1s）→ 落库 v=2。
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let v2 = hget_cell(&client, "rtdata:devHL", "devHL.v").await;
    assert!(v2.contains("2"), "热加载后应采集 v=2，得到 '{v2}'");

    shutdown.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
    let _ = std::fs::remove_dir_all(&scripts);
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

> **Windows notify 风险：** 若本测在 Windows 偶发失败（notify 事件丢失，spec §18），标注 `#[ignore]` 或加 retry/延长等待。CI Linux 应稳定。实现时若 Windows flaky，加 `tokio::time::sleep` 延长或 retry 改脚本。

### - [ ] Step 2：run + fmt + clippy + 提交

```bash
CARGO_BUILD_JOBS=1 cargo test --test lua_hotload_e2e
cargo fmt --all --check && CARGO_BUILD_JOBS=1 cargo clippy --all-targets -- -D warnings
git add tests/lua_hotload_e2e.rs
git commit -m "test(lua): M6 热加载集成测（改脚本 → reload → 新 collect 值）"
```

---

## Self-Review

**1. Spec coverage（M6 热加载）：**
- ✅ notify + notify-debouncer-mini 去抖 200ms（spec §5.3 line 126）→ Task 1/3
- ✅ 编译新 VM（沙箱+hook）+ dry-run + 切换/回退（spec §5.3 line 127）→ Task 2 reload_vm
- ✅ 失败旧 VM 继续（永不半生效）（spec §5.3 line 127）→ Task 2 Evt::Reload Err 分支 warn
- ✅ 与 InstanceManager stop/start 同构（spec §5.3 line 128）—— VM 替换在 Source task 内，不重启实例

**2. Placeholder scan：** 无 TBD；reload_vm/hotload 代码完整；notify-debouncer-mini 0.7 API 标注「实现时调整」（非占位，是已知对齐点）；Windows notify flaky 标注 retry。

**3. Type consistency：** `Evt` 枚举（Cmd/Data/Tick/Reload）；`reload_vm(cfg, lctx, ctx_host, inst, io_engine) -> Result<Lua>`；`spawn_watcher(PathBuf, Sender<()>)`；`reload_tx: mpsc::Sender<()>`；io_engine.clone()（IoEngine Clone，Task 2/3 复用）。

**已知简化（非占位）：** dry-run on_init only（collect dry-run 跳过）；notify Windows 事件丢失（去抖 + M7 REST 兜底）；VM 重建不重启 IO 连接。

---

## Execution Handoff

本计划由 `/loop`（CronCreate `116fd1e9`）驱动，每轮 subagent-driven-development 执行下一个未勾选 task。CI 门槛（fmt + clippy -D warnings + test --all-targets --all-features，Windows CARGO_BUILD_JOBS=1）。API 503 间歇不稳时控制器接手。Task 1-4 完成 → `finishing-a-development-branch` 合并 → 写 Plan 6（M7 状态 REST）plan。

建分支：`feature/lua-m6`（从 master `7416127`）。
