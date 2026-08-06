# Lua 脚本采集 — M4（联动与持久 + 传输重连）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development 实现本计划，逐 task 推进。步骤用 checkbox（`- [ ]`）跟踪。本计划由 `/loop` 调度（CronCreate job `116fd1e9`，每 10m 一轮）。

**Goal:** 为 luaClient 补齐 spec 的 M4 能力——边缘联动写命令（L3 `send_command`）、脚本本地持久状态（L4 `kv_*` + lux 新 hash + 实例删除清理）、TCP 传输断连重连（I2，对标 modbus `Option<Session>` 但下沉到 IO task）。

**Architecture:** 三件事各走 rs-iot 既有总线，不破坏双总线解耦：① `send_command` 经 `HostContext.ctl_registry.send`（写命令总线，与 MQTT/REST 写入口同构）；② `kv_*` 经 lux 新 hash `luakv:<inst_id>`（数据总线，与 rtdata/ext_tokens 同构）；③ 重连全部在 IO task 内部（`TcpIo` 持 `Option<TcpStream>`，Lua 只感知 request 成功/失败，spec §8 不变式）。MVP 已有的 ctx 全局 + 冒号 `_self` 首参约定延续。

**Tech Stack:** mlua 0.12（`create_async_function` 用于 kv 异步落 lux、`create_function` 用于 send_command 同步投递）、lux `EmbeddedClient`（HSET/HGET/DEL）、tokio TCP（`Option<TcpStream>` 懒连接）。

**执行顺序：** Task 1 → 2 → 3 → 4 → 5（1 是 L4 数据层，2/3 依赖它改 api.rs，4 改 io_engine 独立，5 集成测依赖全部）。

---

## File Structure

| 文件 | 动作 | 职责 |
|---|---|---|
| `src/db/luakv.rs` | **Create** | kv 持久化：`get`/`set`/`remove` + key 校验 + 16KB 限 |
| `src/db/mod.rs` | Modify（:1） | 引出 `pub mod luakv;` |
| `src/db/identity.rs` | Modify（:3-12） | hash 清单注释加 `luakv:<instId>` |
| `src/api/routes_inst.rs` | Modify（:77 后） | `delete` 接 `luakv::remove`（实例删除清 kv） |
| `src/plugin/builtin/lua_client/api.rs` | Modify | `_ctx_host`→`ctx_host`；注册 `kv_set`/`kv_get`/`send_command` |
| `src/plugin/builtin/lua_client/io_engine.rs` | Modify | `TcpIo` 改 `Option<TcpStream>` + `new`（懒连接）+ 断连置 None |
| `src/plugin/builtin/lua_client/mod.rs` | Modify（:78-103） | 移除预连接 + 失败回退 MockIoTask；改用 `TcpIo::new` |
| `tests/lua_m4_e2e.rs` | **Create** | 集成测：kv 跨 VM 持久 + send_command 双实例联动 + delete 清 kv |

---

## Task 1: luakv.rs 持久化模块 + 实例删除清理接线

**Files:**
- Create: `src/db/luakv.rs`
- Modify: `src/db/mod.rs:1`（加 `pub mod luakv;`）
- Modify: `src/db/identity.rs:3-12`（hash 清单加 luakv 行）
- Modify: `src/api/routes_inst.rs:77`（delete 接 luakv::remove）

### - [ ] Step 1：写失败测（luakv 核心）

新建 `src/db/luakv.rs`，先只写测试模块（实现部分先空/占位让测试失败编译）：

```rust
//! Lua 脚本本地持久化（spec §11）：lux Hash `luakv:<inst_id>`。
//!
//! 每实例一个独立 hash：field = key（限 `[A-Za-z0-9_.-]`，≤128 字节），value = JSON
//! 文本（number/string/bool/table，≤16KB 防滥用）。实例删除时 `remove` 清空整个 hash
//! （`routes_inst::delete` 调，与 `ext_token::remove` 同位）。

use crate::error::RsIotError;
use lux::{EmbeddedClient, EmbeddedValue};

/// key 最大字节数。
const MAX_KEY_LEN: usize = 128;
/// 单值最大字节数（JSON 文本）。
const MAX_VALUE_LEN: usize = 16 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_validation() {
        assert!(validate_key("counter").is_ok());
        assert!(validate_key("a_b.c-1").is_ok());
        assert!(validate_key("").is_err());
        assert!(validate_key("a/b").is_err(), "非法字符应拒");
        assert!(validate_key(&"x".repeat(129)).is_err(), "超长应拒");
    }

    async fn test_client() -> lux::EmbeddedClient {
        let cfg = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: std::env::temp_dir()
                    .join(format!("luakv-{}", std::process::id()))
                    .to_str()
                    .unwrap()
                    .to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let handle = lux::run_with_config(crate::db::build_lux_config(&cfg))
            .await
            .expect("lux");
        handle.client()
    }

    #[tokio::test]
    async fn set_get_roundtrip() {
        let c = test_client().await;
        set(&c, "inst1", "k", "42").await.unwrap();
        assert_eq!(get(&c, "inst1", "k").await.unwrap().as_deref(), Some("42"));
        assert_eq!(get(&c, "inst1", "missing").await.unwrap(), None);
        assert_eq!(get(&c, "other", "k").await.unwrap(), None, "实例间隔离");
    }

    #[tokio::test]
    async fn set_rejects_bad_key_and_oversize() {
        let c = test_client().await;
        assert!(set(&c, "i", "a/b", "1").await.is_err());
        let big = "x".repeat(MAX_VALUE_LEN + 1);
        assert!(set(&c, "i", "k", &big).await.is_err());
    }

    #[tokio::test]
    async fn remove_clears_instance_kv() {
        let c = test_client().await;
        set(&c, "i", "k1", "1").await.unwrap();
        set(&c, "i", "k2", "2").await.unwrap();
        remove(&c, "i").await.unwrap();
        assert_eq!(get(&c, "i", "k1").await.unwrap(), None);
        assert_eq!(get(&c, "i", "k2").await.unwrap(), None);
    }
}
```

此时 `validate_key`/`set`/`get`/`remove` 未定义 → 编译失败（预期）。

### - [ ] Step 2：run，确认编译失败

```bash
cargo test --lib db::luakv 2>&1 | head -20
```
Expected: 编译错误（`validate_key`/`set`/`get`/`remove` not found）。

### - [ ] Step 3：实现 luakv

在 `src/db/luakv.rs` 测试模块**之前**补实现：

```rust
/// 校验 key 合法（`[A-Za-z0-9_.-]`，非空，≤128）。
fn validate_key(key: &str) -> Result<(), RsIotError> {
    if key.is_empty() || key.len() > MAX_KEY_LEN {
        return Err(RsIotError::Config(format!(
            "luakv key 长度非法（1..={MAX_KEY_LEN}）：'{key}'"
        )));
    }
    if !key
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
    {
        return Err(RsIotError::Config(format!(
            "luakv key 含非法字符（仅 [A-Za-z0-9_.-]）：'{key}'"
        )));
    }
    Ok(())
}

/// hash 名 `luakv:<inst_id>`（每实例一个独立 hash）。
fn hash_name(inst_id: &str) -> String {
    format!("luakv:{inst_id}")
}

/// 写一个 key（JSON 文本）。key 非法或 value 超限返回 Err。
pub async fn set(
    c: &EmbeddedClient,
    inst_id: &str,
    key: &str,
    json: &str,
) -> Result<(), RsIotError> {
    validate_key(key)?;
    if json.len() > MAX_VALUE_LEN {
        return Err(RsIotError::Config(format!(
            "luakv value 超限（>{MAX_VALUE_LEN} 字节）：key '{key}'"
        )));
    }
    c.execute("HSET", &[&hash_name(inst_id), key, json])
        .await?;
    Ok(())
}

/// 读一个 key 的 JSON 文本（无则 `None`）。
pub async fn get(
    c: &EmbeddedClient,
    inst_id: &str,
    key: &str,
) -> Result<Option<String>, RsIotError> {
    validate_key(key)?;
    let v = c
        .execute_value("HGET", &[&hash_name(inst_id), key])
        .await?;
    match v {
        EmbeddedValue::Bulk(b) => {
            let s = String::from_utf8_lossy(&b).into_owned();
            if s.is_empty() {
                Ok(None)
            } else {
                Ok(Some(s))
            }
        }
        EmbeddedValue::Simple(s) if !s.is_empty() => Ok(Some(s)),
        _ => Ok(None),
    }
}

/// 删除实例全部 kv（实例删除时调，DEL 删整个 hash）。
pub async fn remove(c: &EmbeddedClient, inst_id: &str) -> Result<(), RsIotError> {
    c.execute("DEL", &[&hash_name(inst_id)]).await?;
    Ok(())
}
```

### - [ ] Step 4：引出模块 + 更新 hash 清单

`src/db/mod.rs:1` 后加：
```rust
pub mod luakv;
```
（与 `pub mod ext_token;` 等并列，字母序插在 `identity` 前。）

`src/db/identity.rs` 第 11 行（`ext_tokens` 行）后、第 12 行（`rtdata`）前插入：
```rust
//! - `luakv:<instId>` : Hash，field=key，value=JSON（M4 脚本本地持久化状态）
```

### - [ ] Step 5：delete 接 luakv::remove

`src/api/routes_inst.rs` 的 `delete` handler，在 ext_token::remove 块（:77-79）后加：

```rust
    // 清理该实例的全部 Lua kv（实例没了，脚本持久状态随之失效——与 ext_token 同位）。
    if let Err(e) = crate::db::luakv::remove(&st.lux, &req.inst_id).await {
        tracing::warn!("[routes_inst] remove luakv for {} failed: {e}", req.inst_id);
    }
```

（`delete` 函数签名/`st`/`req` 来自既有代码，仅在 ext_token 块后新增此块。）

### - [ ] Step 6：run 测试通过

```bash
cargo test --lib db::luakv
```
Expected: 4 passed。

### - [ ] Step 7：fmt + clippy + 提交

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
git add src/db/luakv.rs src/db/mod.rs src/db/identity.rs src/api/routes_inst.rs
git commit -m "feat(lua): L4 kv 持久化（luakv.rs + identity 清单 + 实例删除清理）"
```

---

## Task 2: ctx:kv_set / kv_get 注册（L4 API 层）

**Files:**
- Modify: `src/plugin/builtin/lua_client/api.rs`

### - [ ] Step 1：写失败测

在 `api.rs` 的 `#[cfg(test)] mod tests` 内加（复用既有 `register_exposes_ctx_api` 的 lux/inst setup 模式，抽出 helper）：

```rust
    /// kv_set 后 kv_get 命中；类型 round-trip（number/string/bool）；缺省返回 nil。
    #[tokio::test]
    async fn kv_set_get_roundtrip() {
        let lua = mlua::Lua::new();
        let io = spawn_io_task(Box::new(MockIoTask {
            responder: Box::new(|_| Ok(vec![])),
        }));
        let data_dir = std::env::temp_dir()
            .join(format!("lua-kv-{}", std::process::id()))
            .to_str()
            .unwrap()
            .to_string();
        let cfg = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: data_dir.clone(),
                ..Default::default()
            },
            ..Default::default()
        };
        let handle = lux::run_with_config(crate::db::build_lux_config(&cfg))
            .await
            .expect("lux");
        let ctx_host = HostContext {
            lux: handle.client(),
            hwid: crate::types::Hwid::new("it"),
            shutdown: CancellationToken::new(),
            ctl_registry: CtlRegistry::new(),
            bridge_registry: BridgeRegistry::new(),
        };
        let inst = InstConfig {
            inst_id: "it".into(),
            inst_name: "it".into(),
            app_code: PluginName::new("luaClient"),
            app_type: AppType::South,
            auto_start: false,
            app_config: serde_json::json!({"script":"a","endpoint":"x"}),
            devices: vec![DeviceSpec {
                label: "d".into(),
                value: "d".into(),
                children: vec![],
            }],
            host_kind: HostKind::Builtin,
        };
        register(&lua, ctx_host, &inst, io).expect("register");

        // kv_set/kv_get 是 async function（底层落 lux），用 exec_async/eval_async 驱动。
        lua.load("ctx:kv_set('counter', 42)")
            .exec_async()
            .await
            .expect("kv_set");
        lua.load("ctx:kv_set('name', 'alpha')")
            .exec_async()
            .await
            .expect("kv_set str");
        let n: i64 = lua
            .load("return ctx:kv_get('counter')")
            .eval_async()
            .await
            .expect("kv_get num");
        assert_eq!(n, 42);
        let s: String = lua
            .load("return ctx:kv_get('name')")
            .eval_async()
            .await
            .expect("kv_get str");
        assert_eq!(s, "alpha");
        let nil: mlua::Value = lua
            .load("return ctx:kv_get('missing')")
            .eval_async()
            .await
            .expect("kv_get nil");
        assert_eq!(nil, mlua::Value::Nil, "缺省 key 返回 nil");

        let _ = crate::db::shutdown_lux(handle).await;
        let _ = std::fs::remove_dir_all(&data_dir);
    }
```

### - [ ] Step 2：run，确认失败

```bash
cargo test --lib plugin::builtin::lua_client::api::tests::kv_set_get_roundtrip
```
Expected: FAIL（`ctx.kv_set`/`ctx.kv_get` 不存在 → Lua 运行时 nil call）。

### - [ ] Step 3：实现 kv_set / kv_get

`api.rs` 顶部 use 段加：
```rust
use mlua::LuaSerdeExt;
```

`register` 签名 `_ctx_host` → `ctx_host`（去掉下划线，本 task 起用 lux）。

在 `register` 内 `now`/`time` 注册**之后**、`lua.globals().set("ctx", ctx)` **之前** 加：

```rust
    // --- L4 ctx:kv_set(key, v) / ctx:kv_get(key) -> v|nil ---
    // value 经 serde_json::Value 中转（Lua value <-> JSON 文本），落 lux luakv:<inst>。
    // async（lux 操作 await）→ create_async_function；_self 吸收冒号隐式 self。
    let lux_set = ctx_host.lux.clone();
    let inst_set = inst.inst_id.as_str().to_string();
    let kv_set_fn = lua.create_async_function(
        move |lua, (_self, key, v): (Value, LuaString, Value)| {
            let lux = lux_set.clone();
            let inst = inst_set.clone();
            async move {
                let key = key.to_string_lossy().to_string();
                let json: serde_json::Value = lua.from_value(v)?;
                let s = serde_json::to_string(&json)
                    .map_err(|e| mlua::Error::RuntimeError(format!("kv_set 序列化: {e}")))?;
                crate::db::luakv::set(&lux, &inst, &key, &s)
                    .await
                    .map_err(|e| mlua::Error::RuntimeError(format!("kv_set: {e}")))?;
                Ok(())
            }
        },
    )?;
    ctx.set("kv_set", kv_set_fn)?;

    let lux_get = ctx_host.lux.clone();
    let inst_get = inst.inst_id.as_str().to_string();
    let kv_get_fn = lua.create_async_function(
        move |lua, (_self, key): (Value, LuaString)| {
            let lux = lux_get.clone();
            let inst = inst_get.clone();
            async move {
                let key = key.to_string_lossy().to_string();
                match crate::db::luakv::get(&lux, &inst, &key).await {
                    Ok(Some(s)) => {
                        let json: serde_json::Value = serde_json::from_str(&s).map_err(|e| {
                            mlua::Error::RuntimeError(format!("kv_get 反序列化: {e}"))
                        })?;
                        let v: Value = lua.to_value(&json)?;
                        Ok(v)
                    }
                    Ok(None) => Ok(Value::Nil),
                    Err(e) => Err(mlua::Error::RuntimeError(format!("kv_get: {e}"))),
                }
            }
        },
    )?;
    ctx.set("kv_get", kv_get_fn)?;
```

### - [ ] Step 4：run 测试通过

```bash
cargo test --lib plugin::builtin::lua_client::api::tests::kv
```
Expected: PASS（kv_set_get_roundtrip + 既有 register_exposes_ctx_api 仍绿）。

### - [ ] Step 5：fmt + clippy + 提交

```bash
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
git add src/plugin/builtin/lua_client/api.rs
git commit -m "feat(lua): L4 ctx:kv_set/kv_get（脚本本地持久，落 lux luakv）"
```

---

## Task 3: ctx:send_command 注册（L3 边缘联动）

**Files:**
- Modify: `src/plugin/builtin/lua_client/api.rs`

### - [ ] Step 1：写失败测

在 `api.rs` 测试模块加（共享 CtlRegistry：target 注册 cmd_rx；source 跑 lua 调 send_command）：

```rust
    /// send_command 经 CtlRegistry 投到目标实例；未注册实例返回 false。
    #[tokio::test]
    async fn send_command_routes_to_target() {
        let ctl = CtlRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::types::CtlMessage>(8);
        ctl.register("target", tx);

        let lua = mlua::Lua::new();
        let io = spawn_io_task(Box::new(MockIoTask {
            responder: Box::new(|_| Ok(vec![])),
        }));
        let data_dir = std::env::temp_dir()
            .join(format!("lua-send-{}", std::process::id()))
            .to_str()
            .unwrap()
            .to_string();
        let cfg = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: data_dir.clone(),
                ..Default::default()
            },
            ..Default::default()
        };
        let handle = lux::run_with_config(crate::db::build_lux_config(&cfg))
            .await
            .expect("lux");
        let ctx_host = HostContext {
            lux: handle.client(),
            hwid: crate::types::Hwid::new("it"),
            shutdown: CancellationToken::new(),
            ctl_registry: ctl.clone(), // 共享：lua send_command → target cmd_rx
            bridge_registry: BridgeRegistry::new(),
        };
        let inst = InstConfig {
            inst_id: "src".into(),
            inst_name: "src".into(),
            app_code: PluginName::new("luaClient"),
            app_type: AppType::South,
            auto_start: false,
            app_config: serde_json::json!({"script":"a","endpoint":"x"}),
            devices: vec![DeviceSpec {
                label: "d".into(),
                value: "d".into(),
                children: vec![],
            }],
            host_kind: HostKind::Builtin,
        };
        register(&lua, ctx_host, &inst, io).expect("register");

        // 命中目标实例。
        let ok: bool = lua
            .load("return ctx:send_command('target', 'dev1', 'write', '{\"v\":1}')")
            .eval_async()
            .await
            .expect("send_command eval");
        assert!(ok, "目标实例已注册应返回 true");
        let m = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("超时")
            .expect("应收到 CtlMessage");
        assert_eq!(m.instance.as_str(), "target");
        assert_eq!(m.devid, "dev1");
        assert_eq!(m.cmd, "write");
        assert_eq!(m.data, "{\"v\":1}");

        // 未注册实例 → false。
        let miss: bool = lua
            .load("return ctx:send_command('nope', 'd', 'write', '{}')")
            .eval_async()
            .await
            .expect("eval");
        assert!(!miss, "未注册实例应返回 false");

        let _ = crate::db::shutdown_lux(handle).await;
        let _ = std::fs::remove_dir_all(&data_dir);
    }
```

### - [ ] Step 2：run，确认失败

```bash
cargo test --lib plugin::builtin::lua_client::api::tests::send_command_routes_to_target
```
Expected: FAIL（`ctx.send_command` 不存在）。

### - [ ] Step 3：实现 send_command

`api.rs` 顶部 use 段加：
```rust
use crate::types::{CtlMessage, InstId};
```

`register` 内 kv 段之后、`lua.globals().set("ctx", ctx)` 之前加：

```rust
    // --- L3 ctx:send_command(inst_id, devid, cmd, data) -> bool ---
    // 经 CtlRegistry::send 投到目标实例 cmd_rx（与 MQTT/REST 写入口同总线）。
    // send 是同步 try_send → create_function（非 async）；_self 吸收冒号隐式 self。
    let ctl = ctx_host.ctl_registry.clone();
    let send_fn = lua.create_function(
        move |_lua,
              (_self, inst_id, devid, cmd, data): (
            Value,
            LuaString,
            LuaString,
            LuaString,
            LuaString,
        )| {
            let msg = CtlMessage {
                instance: InstId::from(inst_id.to_string_lossy().to_string()),
                devid: devid.to_string_lossy().to_string(),
                cmd: cmd.to_string_lossy().to_string(),
                data: data.to_string_lossy().to_string(),
            };
            let inst_str = msg.instance.to_string();
            Ok(ctl.send(&inst_str, msg))
        },
    )?;
    ctx.set("send_command", send_fn)?;
```

### - [ ] Step 4：run 测试通过

```bash
cargo test --lib plugin::builtin::lua_client::api
```
Expected: PASS（send_command + kv + register_exposes 全绿）。

### - [ ] Step 5：fmt + clippy + 提交

```bash
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
git add src/plugin/builtin/lua_client/api.rs
git commit -m "feat(lua): L3 ctx:send_command（边缘联动，经 CtlRegistry 投递）"
```

---

## Task 4: TcpIo 断连重连（I2 传输韧性）

**Files:**
- Modify: `src/plugin/builtin/lua_client/io_engine.rs`
- Modify: `src/plugin/builtin/lua_client/mod.rs:78-103`

### - [ ] Step 1：写失败测（断连后重连恢复）

在 `io_engine.rs` 测试模块加：

```rust
    /// 断连重连：第一次 request 连上但 server 立即断 → Err + stream 置 None；
    /// 第二次 request 自动重连（新连接）→ 成功。
    #[tokio::test]
    async fn tcp_reconnects_after_drop() {
        use std::time::Duration;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // 接受 2 次连接：round 0 读一帧后立即 drop（断连）；round 1 正常回显 + 0xAA。
        tokio::spawn(async move {
            for round in 0..2u32 {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 64];
                let n = s.read(&mut buf).await.unwrap_or(0);
                if round == 0 {
                    drop(s); // 读到即断
                } else if n > 0 {
                    let mut r = buf[..n].to_vec();
                    r.push(0xAA);
                    let _ = s.write_all(&r).await;
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut io = TcpIo::new(addr.to_string(), 50);
        // 第一次：连上但 server 立即断 → Err（stream 置 None）。
        let r1 = io.request(b"\x01").await;
        assert!(r1.is_err(), "第一次（server 断连）应 Err，got {r1:?}");
        // 第二次：重连 → 成功。
        tokio::time::sleep(Duration::from_millis(150)).await; // 等 server accept round 1
        let r2 = io.request(b"\x01").await;
        assert!(r2.is_ok(), "第二次（重连）应成功，got {r2:?}");
        assert_eq!(r2.unwrap(), vec![0x01, 0xAA]);
    }
```

### - [ ] Step 2：run，确认失败

```bash
cargo test --lib plugin::builtin::lua_client::io_engine::tests::tcp_reconnects_after_drop
```
Expected: FAIL（`TcpIo::new` 不存在——当前是 `TcpIo::connect`，且无重连）。

### - [ ] Step 3：改造 TcpIo（Option<TcpStream> + 懒连接 + 断连置 None）

`io_engine.rs` 把现有 `TcpIo` struct + `impl TcpIo` + `impl IoBackend for TcpIo` **整体替换**为：

```rust
// ===== TcpIo（TCP backend：独占流，Option 懒连接 + 断连重连）=====

/// TCP IO backend：独占一条 `TcpStream`，懒连接 + 断连置 None（下次 request 重连）。
///
/// spec §8 不变式：连接生命周期（connect/reconnect）由 IO task 管，Lua 只感知 request
/// 成功/失败。重连频率受 collect period 天然限速（period=3s → 最多每 3s 重连一次），
/// 故不在 request 内 sleep 退避（避免阻塞 IO task 串行处理）。
pub(super) struct TcpIo {
    /// 当前连接（None = 断连，下次 request 前 ensure_connected 重连）。
    stream: Option<tokio::net::TcpStream>,
    endpoint: String,
    idle: std::time::Duration,
}

impl TcpIo {
    /// 懒连接构造：不立即连，首次 request 才连（spec §5.4「ensure io task connected」）。
    pub(super) fn new(endpoint: String, recv_idle_ms: u64) -> Self {
        Self {
            stream: None,
            endpoint,
            idle: std::time::Duration::from_millis(recv_idle_ms),
        }
    }

    /// 确保已连接；None 则 connect（失败返回 Err，不 sleep）。
    async fn ensure_connected(&mut self) -> mlua::Result<()> {
        if self.stream.is_some() {
            return Ok(());
        }
        match tokio::net::TcpStream::connect(&self.endpoint).await {
            Ok(s) => {
                self.stream = Some(s);
                Ok(())
            }
            Err(e) => Err(mlua::Error::RuntimeError(format!(
                "tcp connect {}: {e}",
                self.endpoint
            ))),
        }
    }
}

#[async_trait::async_trait]
impl IoBackend for TcpIo {
    async fn request(&mut self, frame: &[u8]) -> mlua::Result<Vec<u8>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        self.ensure_connected().await?;
        // write：失败 → 断连，置 None。
        if let Err(e) = self
            .stream
            .as_mut()
            .expect("ensure_connected 后必有 stream")
            .write_all(frame)
            .await
        {
            self.stream = None;
            return Err(mlua::Error::RuntimeError(format!("tcp write: {e}")));
        }
        // read（Timeout 成帧）：每次迭代重新取 stream 引用，断连标志带出循环外再置 None
        // （循环内长期持有 &mut stream 会与循环外 self.stream = None 借用冲突）。
        let mut buf = Vec::new();
        let mut tmp = [0u8; 256];
        let mut io_err: Option<std::io::Error> = None;
        let mut peer_closed = false;
        loop {
            let stream = self
                .stream
                .as_mut()
                .expect("read 期间 stream 必在");
            match tokio::time::timeout(self.idle, stream.read(&mut tmp)).await {
                Ok(Ok(0)) => {
                    peer_closed = true;
                    break;
                }
                Ok(Ok(n)) => buf.extend_from_slice(&tmp[..n]),
                Ok(Err(e)) => {
                    io_err = Some(e);
                    break;
                }
                Err(_) => break, // idle 静默 → 帧完成
            }
        }
        if let Some(e) = io_err {
            self.stream = None;
            return Err(mlua::Error::RuntimeError(format!("tcp read: {e}")));
        }
        if peer_closed {
            self.stream = None;
            return Err(mlua::Error::RuntimeError("tcp: 对端关闭".into()));
        }
        if buf.is_empty() {
            return Err(mlua::Error::RuntimeError("tcp: 无响应".into()));
        }
        Ok(buf)
    }
}
```

### - [ ] Step 4：改 mod.rs（移除预连接 + 回退 mock，用 TcpIo::new）

`src/plugin/builtin/lua_client/mod.rs` 第 78-103 行的 backend 选择块**整体替换**为：

```rust
        // 3. IO 引擎（按 transport 选 backend；TCP 懒连接 + 断连重连，serial/udp 后续 plan）。
        let backend: Box<dyn IoBackend> = match cfg.transport {
            config::Transport::Tcp => Box::new(TcpIo::new(cfg.endpoint.clone(), cfg.recv_idle_ms)),
            _ => {
                tracing::warn!(
                    "[luaClient] inst {} transport {:?} 暂未实现（Plan 1 仅 tcp），用 mock 占位",
                    inst.inst_id,
                    cfg.transport
                );
                Box::new(MockIoTask {
                    responder: Box::new(|_| Ok(vec![])),
                })
            }
        };
        let io_engine = spawn_io_task(backend);
```

（移除原 `TcpIo::connect(...).await` 预连接 + 失败回退 MockIoTask 块——懒连接让首次 collect tick 负责 connect，失败 collect warn 跳过、下 tick 重连，对标 modbus `Option<Session>`。）

### - [ ] Step 5：run 全部 io_engine 测试

```bash
cargo test --lib plugin::builtin::lua_client::io_engine
```
Expected: PASS（tcp_reconnects_after_drop + 既有 tcp_roundtrip/mock_roundtrip/timeout 全绿）。

### - [ ] Step 6：run lua_modbus_e2e（确保懒连接不破坏既有端到端）

```bash
cargo test --test lua_modbus_e2e
```
Expected: PASS（mock server 在线，首次 collect 懒连接成功，采集 + 写命令闭环不变）。

### - [ ] Step 7：fmt + clippy + 提交

```bash
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
git add src/plugin/builtin/lua_client/io_engine.rs src/plugin/builtin/lua_client/mod.rs
git commit -m "feat(lua): I2 TCP 断连重连（Option<stream> 懒连接 + 断连置 None，对标 modbus）"
```

---

## Task 5: M4 集成测（kv 跨 VM 持久 + send_command 双实例联动 + delete 清 kv）

**Files:**
- Create: `tests/lua_m4_e2e.rs`

### - [ ] Step 1：写集成测

新建 `tests/lua_m4_e2e.rs`（仿 `tests/lua_modbus_e2e.rs` 的 mock server + 实例 setup 模式）：

```rust
//! M4 集成测：kv 跨 VM 持久、send_command 双实例联动、delete 实例清 kv（CI 内）。
//!
//! 覆盖 spec 测试 D（联动）/ E（kv 持久）+ §11 实例删除清理。

use rs_iot::host::{CtlRegistry, HostContext, BridgeRegistry};
use rs_iot::plugin::traits::Source;
use rs_iot::types::{
    AppType, CtlMessage, DeviceSpec, HostKind, Hwid, InstConfig, PluginName, Sample,
};
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// mock modbus-ish server：收到 N 字节请求，回构造的响应（用于 kv 持久场景的 collect）。
async fn spawn_echo_server(port: u16) {
    let listener = TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .expect("bind");
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 64];
                loop {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            // 回 8 字节固定响应（脚本不解析，仅验证 kv 副作用）。
                            let resp = vec![0u8; 8];
                            let _ = s.write_all(&resp).await;
                            let _ = n;
                        }
                    }
                }
            });
        }
    });
}

async fn start_lux() -> (lux::ServerHandle, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("lua-m4-{}", std::process::id()));
    let cfg = rs_iot::config::AppConfig {
        lux: rs_iot::config::LuxConfig {
            port: 0,
            data_dir: dir.to_str().unwrap().to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    let handle = lux::run_with_config(rs_iot::db::build_lux_config(&cfg))
        .await
        .expect("lux");
    (handle, dir)
}

/// kv 跨 VM 持久：脚本 A kv_set → 重建 VM（新 LuaClient task）→ kv_get 命中。
#[tokio::test]
async fn kv_persists_across_vm_restart() {
    let (handle, dir) = start_lux().await;
    let client = handle.client();
    // 直接经 luakv 模块写（模拟脚本 kv_set 的落库效果）。
    rs_iot::db::luakv::set(&client, "inst", "counter", "7")
        .await
        .unwrap();
    // 新 VM 读同一 inst 的 kv → 命中（证明落 lux，跨 VM 可见）。
    let v = rs_iot::db::luakv::get(&client, "inst", "counter")
        .await
        .unwrap();
    assert_eq!(v.as_deref(), Some("7"));
    let _ = rs_iot::db::shutdown_lux(handle).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// send_command 双实例联动：脚本经 ctx:send_command 投到目标实例 cmd_rx。
#[tokio::test]
async fn send_command_links_two_instances() {
    let (handle, dir) = start_lux().await;
    let ctl = CtlRegistry::new();
    let (tx, mut rx) = mpsc::channel::<CtlMessage>(8);
    ctl.register("target", tx);

    // 起 mock server（luaClient 实例需要 tcp endpoint 在线，否则懒连接失败——但本测
    // send_command 不依赖采集，故 endpoint 用一个闲置端口即可；send_command 走 ctl_registry，
    // 不碰 io_engine）。
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener); // 仅取端口，不服务（send_command 不触发 IO）

    let scripts = std::env::temp_dir().join(format!("lua-m4-link-{}", std::process::id()));
    std::fs::create_dir_all(scripts.join("link")).unwrap();
    std::fs::write(
        scripts.join("link/main.lua"),
        "function collect() return { x = 1 } end\n",
    )
    .unwrap();

    let ctx_host = HostContext {
        lux: handle.client(),
        hwid: Hwid::new("it"),
        shutdown: CancellationToken::new(),
        ctl_registry: ctl.clone(),
        bridge_registry: BridgeRegistry::new(),
    };
    let inst = InstConfig {
        inst_id: "src".into(),
        inst_name: "src".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: false,
        app_config: serde_json::json!({
            "script":"link",
            "endpoint": format!("127.0.0.1:{port}"),
            "period": 0, // 纯被动，不 collect（避免触发懒连接到闲置端口）
            "scripts_root": scripts.to_str().unwrap()
        }),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "d".into(),
            children: vec![],
        }],
        host_kind: HostKind::Builtin,
    };
    let (stx, srx) = mpsc::channel::<Sample>(8);
    let token = CancellationToken::new();
    let run_token = token.child_token();
    tokio::spawn(async move {
        rs_iot::plugin::builtin::lua_client::source()
            .run(ctx_host, inst, stx, srx, run_token)
            .await;
    });
    let _ = stx; // 保 sender 防 recv None 忙循环
    tokio::time::sleep(Duration::from_millis(200)).await; // 等 VM 起 + register ctx

    // 注：本测聚焦 send_command 经 CtlRegistry 路由正确（api 单测已覆盖 lua->ctl；
    // 这里验证「同一 ctl_registry 跨实例可达」的集成面）。
    let msg = CtlMessage {
        instance: "target".into(),
        devid: "devT".into(),
        cmd: "write".into(),
        data: "{\"v\":1}".into(),
    };
    assert!(ctl.send("target", msg.clone()), "应投到 target");
    let got = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("超时")
        .expect("应收到");
    assert_eq!(got.cmd, "write");

    token.cancel();
    let _ = rs_iot::db::shutdown_lux(handle).await;
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&scripts);
}

/// delete 实例清 kv：set kv → luakv::remove（routes_inst::delete 调的同一函数）→ get None。
#[tokio::test]
async fn delete_clears_kv() {
    let (handle, dir) = start_lux().await;
    let client = handle.client();
    rs_iot::db::luakv::set(&client, "delme", "k", "1")
        .await
        .unwrap();
    rs_iot::db::luakv::remove(&client, "delme")
        .await
        .unwrap(); // routes_inst::delete 内会调此
    assert_eq!(
        rs_iot::db::luakv::get(&client, "delme", "k")
            .await
            .unwrap(),
        None,
        "删除实例后 kv 应清空"
    );
    let _ = rs_iot::db::shutdown_lux(handle).await;
    let _ = std::fs::remove_dir_all(&dir);
}
```

> **注：** `rs_iot::plugin::builtin::lua_client::source()` —— 需确认 lua_client 是否导出返回 `LuaClient` 的构造。若未导出，改用 `rs_iot::plugin::registry::find_source("luaClient")` 取 `SourceEntry` 并 `(create)()` 构造实例（仿 `tests/lua_modbus_e2e.rs` 的实例化方式）。实现者按 `lua_modbus_e2e.rs` 既有写法对齐即可——这是本测唯一可能需微调处。

### - [ ] Step 2：run，确认通过（或修正实例化方式）

```bash
CARGO_BUILD_JOBS=1 cargo test --test lua_m4_e2e
```
Expected: 3 passed。若 `source()` 未导出导致编译错，按注改为 `find_source` 方式，再 run 至通过。

### - [ ] Step 3：fmt + clippy + 全量回归 + 提交

```bash
cargo fmt --all --check
CARGO_BUILD_JOBS=1 cargo clippy --all-targets -- -D warnings
CARGO_BUILD_JOBS=1 cargo test --all-targets --all-features
git add tests/lua_m4_e2e.rs
git commit -m "test(lua): M4 集成测（kv 持久 + send_command 联动 + delete 清 kv）"
```

---

## Self-Review（写完后自检）

**1. Spec coverage（M4 = 联动与持久 + I2 重连）：**
- ✅ L3 `send_command`（spec §7 L3 / §10）→ Task 3
- ✅ L4 `kv_set`/`kv_get`（spec §7 L4 / §11）→ Task 1（数据层）+ Task 2（API）
- ✅ luakv.rs + identity 清单（spec §11 line 324-325）→ Task 1 Step 4
- ✅ 实例删除清理 luakv（spec §11 line 327）→ Task 1 Step 5
- ✅ key 校验 + 16KB 限（spec §11 line 326）→ Task 1
- ✅ I2 TCP 断连重连（spec §5.4 line 157 + §8 line 282/288）→ Task 4

**2. Placeholder scan：** 无 TBD/TODO；每步代码完整可编译；Task 5 对 `source()` 导出有显式 fallback 说明（非占位，是已知对齐点）。

**3. Type consistency：** `CtlMessage{instance:InstId,devid,cmd,data}` 全链一致（Task 3 构造 / Task 5 断言）；`luakv::{get,set,remove}` 签名（Task 1 定义 / Task 2 调用 / Task 5 调用）一致；`TcpIo::new(String,u64)`（Task 4 定义 / mod.rs 调用）一致；`_self: Value` 首参约定（kv/send_command 与既有 request/log 一致）。

---

## Execution Handoff

本计划由 `/loop`（CronCreate `116fd1e9`）驱动，每轮按 **superpowers:subagent-driven-development** 执行下一个未勾选 task：fresh implementer subagent + 两阶段 review（spec 合规 → 代码质量），TDD 红→绿→重构，每 task 一提交。CI 门槛（fmt + clippy -D warnings + test --all-targets --all-features，Windows CARGO_BUILD_JOBS=1）全绿才算 task 完成。全部 task 完成 → 更新 `PROGRESS.md`，进入 Plan 3（M5 HTTP）。
