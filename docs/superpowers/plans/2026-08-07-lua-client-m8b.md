# Lua 脚本采集 — M8b L2 数据 API + 文档实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development 实现本计划。步骤用 checkbox（`- [ ]`）。由 `/loop` 调度（CronCreate `39632fab`，每 10m 一轮）。

**Goal:** 补齐 luaClient 的 L2 数据 ctx API（`set_value`/`get_value`/`get_device`/`list_devices`，spec §7）+ 脚本开发指南（`docs/lua-script-guide.md`）+ 示例脚本（`examples/lua-scripts/`）。前置：api.rs 616 行超 <500 → 拆分 `api/` 目录。**spec 最后 milestone**——完成后 CronDelete 收尾。

**Architecture:** api.rs 拆 `api/{mod,comm,data,http,util}.rs` + tests 留各模块（按 spec §7 L 层 + 重量分：comm=L1/L3 通讯控制，data=L2/L4 数据读写 lux，http=L5 出站，util=L6 工具）。L2 实现：`set_value(tag,v)` 推 Sample 到 hub（register 加 `tx: mpsc::Sender<Sample>` 参数，devid=inst.devices[0]，v1 单 dev 简化）；`get_value(devid,tag)`/`get_device(devid)` 读 lux `rtdata:<devid>`；`list_devices()` 返 inst.devices。

**Tech Stack:** mlua（create_async_function for set_value/get_value/get_device lux 操作；create_function for list_devices 同步）、lux EmbeddedClient（HGET/HGETALL rtdata）、mpsc::Sender<Sample>（set_value 推 hub）。

**执行顺序：** Task 1（api.rs 拆分，守 <500）→ Task 2（L2 API + register 加 tx + mod.rs/reload_vm 接）→ Task 3（lua-script-guide.md）→ Task 4（examples + L2 集成测）。

**已知简化 / 风险（注明）：**
- **set_value devid = inst.devices[0].value**（v1 单 dev 假设）：多 dev 实例 set_value 都写到第一个 device（语义模糊，标简化；多 dev 用 collect 返回 table 按 dev 分发或后续 set_value(devid,...) 扩展）。
- **get_value 返字符串**（rtdata value_str）：rtdata 存 `[ts,localTime,value]`，value 是 to_string。get_value 返原始字符串（脚本 `tonumber` 转换）。类型推断（number/bool/string）后置。
- **list_devices 返 inst.devices**（当前实例设备树，非本机全部）：跨实例设备查询用 get_value(devid)。spec「本机设备树」v1 简化为实例自身。
- **api.rs 拆分是机械重构**（移动代码到子模块，行为不变）：Task 1 纯 move + mod 编排，125 测（M7 后）不变。
- **register 加 tx 影响 mod.rs/reload_vm**：两处调 register 需传 tx.clone()（mod.rs run() 有 tx；reload_vm 需接 tx）。

---

## File Structure

| 文件 | 动作 | 职责 |
|---|---|---|
| `src/plugin/builtin/lua_client/api.rs` | **删除→拆** | 拆为 `api/` 目录（mod.rs 编排 + comm/data/http/util 子模块） |
| `src/plugin/builtin/lua_client/api/mod.rs` | **Create** | `register` 编排（ctx 表 + 调各子模块 register_xxx + globals.set）+ re-export |
| `src/plugin/builtin/lua_client/api/comm.rs` | **Create** | L1 request + L3 send_command（create_function/async） |
| `src/plugin/builtin/lua_client/api/data.rs` | **Create** | L2 set_value/get_value/get_device/list_devices（新）+ L4 kv_set/kv_get（既有，移动） |
| `src/plugin/builtin/lua_client/api/http.rs` | **Create** | L5 http_get/http_post + headers_to_vec + http_request（feature lua-http，既有移动） |
| `src/plugin/builtin/lua_client/api/util.rs` | **Create** | L6 register_log + now/time（既有移动） |
| `src/plugin/builtin/lua_client/mod.rs` | Modify | run() register 调用加 tx.clone()；reload_vm 同 |
| `docs/lua-script-guide.md` | **Create** | 脚本开发指南（API 参考 + 示例） |
| `examples/lua-scripts/` | **Create** | 示例脚本（modbus-like / custom / on_data / kv / http） |
| `tests/lua_l2_e2e.rs` | **Create** | L2 端到端（set_value → hub → rtdata；get_value 读回） |

---

## Task 1: api.rs 拆分 api/ 目录（守 <500，重构）✅

> **实现调整（2026-08-07）**：用 **3 文件简化**（`api/mod.rs` 184 行 = register L1/L3/L4/L6 + register_log；`api/http.rs` 142 行 = L5 + headers_to_vec + http_request；`api/tests.rs` 322 行 = 4 测原样）替代 plan 的 5 文件（comm/data/http/util）——行为等价、守 <500、减少散布（tests 集中不拆）。CI 三项全绿（fmt + clippy -D warnings + test 131 绿），行为不变。register 签名 Task 1 不变（4 参数），Task 2 加 tx。

**Files:** 拆 `api.rs` → `api/{mod,comm,data,http,util}.rs`；`mod.rs`（lua_client）`mod api;` 不变（找 api/mod.rs）

### - [ ] Step 1：创建 api/ 目录子模块（移动 api.rs 各段）

api.rs 当前结构（616 行）：use + register（编排 L1/L3/L4/L5/L6）+ register_log + headers_to_vec + http_request + tests。

拆分映射（机械移动，不改逻辑）：

**api/mod.rs**（~90 行）：
```rust
//! ctx 对象 + host API 注册（spec §7）。编排各 L 层子模块注册到 ctx 全局。
//!
//! 拆分自原 api.rs（守 <500 行）：comm=L1/L3，data=L2/L4，http=L5，util=L6。

mod comm;
mod data;
mod http;
mod util;

use crate::host::HostContext;
use crate::plugin::builtin::lua_client::io_engine::IoEngine;
use crate::types::InstConfig;
use mlua::Lua;
use tokio::sync::mpsc;

/// 在 `lua` 上注册 ctx 全局对象 + 全部 host API（L1-L6）。
///
/// `tx`：set_value（L2）推 Sample 到 hub 的发送端（spec §7 set_value 写自己 devid）。
pub(super) fn register(
    lua: &Lua,
    ctx_host: HostContext,
    inst: &InstConfig,
    io: IoEngine,
    tx: mpsc::Sender<crate::types::Sample>,
) -> mlua::Result<()> {
    let ctx = lua.create_table()?;
    comm::register(lua, &ctx, &io, &ctx_host)?;
    data::register(lua, &ctx, &ctx_host, inst, tx)?;
    util::register(lua, &ctx, inst)?;
    #[cfg(feature = "lua-http")]
    http::register(lua, &ctx, inst)?;
    lua.globals().set("ctx", ctx)?;
    Ok(())
}
```

> **注：** register 签名加 `tx`（Task 2 set_value 用）。Task 1 拆分时 tx 参数已加（但 data.rs Task 1 仅移动 L4 kv，set_value Task 2 加）。Task 1 commit 时 tx 未被 data.rs set_value 用 → `tx` unused（Task 2 用）。Task 1 在 data::register 签名加 `tx` 但不用 → `#[allow(unused)]` 临时（Task 2 移除）。**或 Task 1 拆分时 register 不加 tx，Task 2 加**（更干净，但 Task 2 改 register 签名 + 所有子模块）。**选 Task 2 加 tx**（Task 1 拆分纯重构，register 不加 tx；Task 2 加 tx + set_value）。下面 Task 1 按"register 不加 tx"。

**修正 Task 1 api/mod.rs**（register 不加 tx，Task 2 加）：
```rust
pub(super) fn register(
    lua: &Lua,
    ctx_host: HostContext,
    inst: &InstConfig,
    io: IoEngine,
) -> mlua::Result<()> {
    let ctx = lua.create_table()?;
    comm::register(lua, &ctx, &io, &ctx_host)?;
    data::register(lua, &ctx, &ctx_host, inst)?;
    util::register(lua, &ctx, inst)?;
    #[cfg(feature = "lua-http")]
    http::register(lua, &ctx, inst)?;
    lua.globals().set("ctx", ctx)?;
    Ok(())
}
```

**api/comm.rs**（~80 行）：移动 api.rs 的 L1 request + L3 send_command 注册段，包为 `pub(super) fn register(lua, ctx, io, ctx_host)`：
```rust
//! L1 通讯（ctx:request）+ L3 控制下行（ctx:send_command）。spec §7。

use crate::host::HostContext;
use crate::plugin::builtin::lua_client::io_engine::IoEngine;
use crate::types::{CtlMessage, InstId};
use mlua::{Lua, LuaString, Value};

/// 注册 L1 request + L3 send_command 到 ctx 表。
pub(super) fn register(
    lua: &Lua,
    ctx: &mlua::Table,
    io: &IoEngine,
    ctx_host: &HostContext,
) -> mlua::Result<()> {
    // --- L1 ctx:request(frame:str, timeout_ms:num) -> resp:str | nil,err ---
    // （从原 api.rs 移动 request_fn 创建段，闭包捕获 io.clone()）
    let io_req = io.clone();
    let request_fn = lua.create_async_function(
        move |lua, (_self, frame, timeout_ms): (Value, LuaString, u64)| {
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
                        Value::String(lua.create_string(e.to_string())?),
                    )),
                }
            }
        },
    )?;
    ctx.set("request", request_fn)?;

    // --- L3 ctx:send_command(inst_id, devid, cmd, data) -> bool ---
    // （从原 api.rs 移动 send_fn 创建段，闭包捕获 ctx_host.ctl_registry.clone()）
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
    Ok(())
}
```

**api/util.rs**（~70 行）：移动 register_log + L6 now/time：
```rust
//! L6 工具：log_info/warn/err/debug + now/time。spec §7。

use crate::types::InstConfig;
use mlua::{Lua, Value};

/// 注册 L6 log/now/time 到 ctx 表。
pub(super) fn register(lua: &Lua, ctx: &mlua::Table, inst: &InstConfig) -> mlua::Result<()> {
    let name = inst.inst_id.as_str().to_string();
    register_log(lua, ctx, &name)?;
    ctx.set(
        "now",
        lua.create_function(|_lua, _self: Value| {
            Ok(chrono::Local::now().timestamp_millis() as f64)
        })?,
    )?;
    ctx.set(
        "time",
        lua.create_function(|_lua, _self: Value| {
            Ok(chrono::Local::now().timestamp() as f64)
        })?,
    )?;
    Ok(())
}

/// 注册 log_info/warn/err/debug 到 ctx 表（tracing，前缀 [lua:<name>]）。
/// （从原 api.rs 移动 register_log 全文，不改）
fn register_log(lua: &Lua, ctx: &mlua::Table, name: &str) -> mlua::Result<()> {
    // ... 从原 api.rs register_log 移动（for key/level 循环 + 闭包）...
    #![allow(unused)] // 占位：实现时从 api.rs 移动 register_log 全文
    let _ = (lua, ctx, name);
    Ok(())
}
```

> **实现指引：** register_log 全文从原 api.rs（line ~196-225 的 `for (key, level) in [...]` 循环 + 闭包）原样移动。util.rs 不重写逻辑，只 move。

**api/data.rs**（~100 行，Task 1 仅 L4 kv；Task 2 加 L2）：
```rust
//! L2 数据读写（set_value/get_value/get_device/list_devices，Task 2）+ L4 kv 持久。
//! Task 1 仅移动 L4 kv_set/kv_get；Task 2 加 L2。

use crate::host::HostContext;
use crate::types::InstConfig;
use mlua::{Lua, LuaString, Value};

/// 注册 L4 kv + L2 数据 API（Task 2）到 ctx 表。
pub(super) fn register(
    lua: &Lua,
    ctx: &mlua::Table,
    ctx_host: &HostContext,
    inst: &InstConfig,
) -> mlua::Result<()> {
    // --- L4 ctx:kv_set(key, v) / ctx:kv_get(key) ---
    // （从原 api.rs 移动 kv_set_fn / kv_get_fn 创建段，闭包捕获 ctx_host.lux.clone() + inst.inst_id）
    let lux_set = ctx_host.lux.clone();
    let inst_set = inst.inst_id.as_str().to_string();
    let kv_set_fn =
        lua.create_async_function(move |lua, (_self, key, v): (Value, LuaString, Value)| {
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
        })?;
    ctx.set("kv_set", kv_set_fn)?;

    let lux_get = ctx_host.lux.clone();
    let inst_get = inst.inst_id.as_str().to_string();
    let kv_get_fn = lua.create_async_function(move |lua, (_self, key): (Value, LuaString)| {
        let lux = lux_get.clone();
        let inst = inst_get.clone();
        async move {
            let key = key.to_string_lossy().to_string();
            match crate::db::luakv::get(&lux, &inst, &key).await {
                Ok(Some(s)) => {
                    let json: serde_json::Value = serde_json::from_str(&s)
                        .map_err(|e| mlua::Error::RuntimeError(format!("kv_get 反序列化: {e}")))?;
                    let v: Value = lua.to_value(&json)?;
                    Ok(v)
                }
                Ok(None) => Ok(Value::Nil),
                Err(e) => Err(mlua::Error::RuntimeError(format!("kv_get: {e}"))),
            }
        }
    })?;
    ctx.set("kv_get", kv_get_fn)?;

    // Task 2 在此加 L2 set_value/get_value/get_device/list_devices
    Ok(())
}
```

**api/http.rs**（~110 行，feature lua-http）：移动 L5 http_get/http_post + headers_to_vec + http_request：
```rust
//! L5 HTTP 出站（ctx:http_get/http_post，feature lua-http，全审计）。spec §7/§12。

use crate::types::InstConfig;
use mlua::{Lua, LuaString, Value};

/// 注册 L5 http_get/http_post 到 ctx 表。
pub(super) fn register(lua: &Lua, ctx: &mlua::Table, inst: &InstConfig) -> mlua::Result<()> {
    // （从原 api.rs 移动 http_get_fn / http_post_fn 创建段 + headers_to_vec + http_request）
    #![allow(unused)] // 占位：实现时从 api.rs 移动 http_get/post + helpers 全文
    let _ = (lua, ctx, inst);
    Ok(())
}

// headers_to_vec + http_request 从原 api.rs 移动（不改）
```

> **实现指引：** http.rs 从原 api.rs 移动 http_get_fn/http_post_fn 注册段（~line 137-190）+ headers_to_vec（~230-240）+ http_request（~244-295）全文。不改逻辑。

**tests**：各子模块的 tests（register_exposes_ctx_api / kv_set_get / send_command / http_get）移到对应子模块 `#[cfg(test)] mod tests`，或集中 api/mod.rs tests。**选集中 api/mod.rs tests**（减少散布）。从原 api.rs tests（~line 297-616）移动到 api/mod.rs `#[cfg(test)] mod tests`（tests 用 `super::register`，签名 Task 1 不变，Task 2 加 tx 后改）。

### - [ ] Step 2：删 api.rs，建 api/mod.rs

`git mv src/plugin/builtin/lua_client/api.rs src/plugin/builtin/lua_client/api/mod.rs` 后按 Step 1 编辑各子模块。或先建 api/ 子文件再 rm api.rs。

### - [ ] Step 3：编译 + fmt + clippy + test（行为不变）

```bash
CARGO_BUILD_JOBS=1 cargo build --lib
cargo fmt --all && cargo fmt --all --check && CARGO_BUILD_JOBS=1 cargo clippy --all-targets -- -D warnings
CARGO_BUILD_JOBS=1 cargo test --lib plugin::builtin::lua_client
```
Expected: 编译过（拆分纯重构）；lua_client 全测绿（拆分不改行为）；各 api/ 文件 <500。

### - [ ] Step 4：提交

```bash
git add src/plugin/builtin/lua_client/api
git rm src/plugin/builtin/lua_client/api.rs  # 若未 git mv
git commit -m "refactor(lua): M8b api.rs 拆分 api/ 目录（mod/comm/data/http/util，守 <500）"
```

---

## Task 2: L2 数据 API + register 加 tx

**Files:** Modify `api/data.rs`（加 L2）+ `api/mod.rs`（register 加 tx）+ `mod.rs`（lua_client run()/reload_vm 接 tx）

### - [ ] Step 1：先加 L2 失败单测（TDD，api/mod.rs tests）

api/mod.rs tests 加：
```rust
    /// L2 set_value 推 hub → get_value 读回（经 lux rtdata）。
    #[tokio::test]
    async fn l2_set_value_then_get_value() {
        // setup（同 register_exposes_ctx_api 的 lux + ctx_host + inst + io）
        // ... 起 lux + register ...
        // set_value("temp", 42.5) → 推 hub（但单测无 pipeline，tx 直接收 Sample）
        // 验：tx 收 Sample（devid=inst.devices[0].value, values={"temp":42.5}）
        // get_value 读 lux rtdata（需 pipeline 写入，或单测直接 lux HSET 模拟 rtdata）
    }
```

> **单测策略：** set_value 推 tx → 单测 tx 收 Sample 验值。get_value 单测：先 lux HSET rtdata:<devid> 模拟，再 ctx:get_value 读回。**实现时按此写完整测**（参考 register_exposes_ctx_api 的 lux setup）。

### - [ ] Step 2：register 加 tx（api/mod.rs）

api/mod.rs register 签名加 `tx`：
```rust
pub(super) fn register(
    lua: &Lua,
    ctx_host: HostContext,
    inst: &InstConfig,
    io: IoEngine,
    tx: mpsc::Sender<crate::types::Sample>,
) -> mlua::Result<()> {
    let ctx = lua.create_table()?;
    comm::register(lua, &ctx, &io, &ctx_host)?;
    data::register(lua, &ctx, &ctx_host, inst, tx)?;  // tx 给 data（set_value）
    util::register(lua, &ctx, inst)?;
    #[cfg(feature = "lua-http")]
    http::register(lua, &ctx, inst)?;
    lua.globals().set("ctx", ctx)?;
    Ok(())
}
```

api/data.rs register 签名加 `tx` + 实现 L2：
```rust
pub(super) fn register(
    lua: &Lua,
    ctx: &mlua::Table,
    ctx_host: &HostContext,
    inst: &InstConfig,
    tx: mpsc::Sender<crate::types::Sample>,
) -> mlua::Result<()> {
    // ... 既有 L4 kv_set/kv_get（不变）...

    // --- L2 ctx:set_value(tag, v) → 推 Sample 到 hub（devid=inst.devices[0]）---
    let tx_set = tx.clone();
    let hwid = ctx_host.hwid.clone();
    let devid = inst
        .devices
        .first()
        .map(|d| d.value.clone())
        .unwrap_or_default();
    let set_value_fn =
        lua.create_async_function(move |_lua, (_self, tag, v): (Value, LuaString, Value)| {
            let tx = tx_set.clone();
            let hwid = hwid.clone();
            let devid = devid.clone();
            async move {
                let tag = tag.to_string_lossy().to_string();
                // v: mlua::Value → SampleValue（number/int/bool/string）
                let sv = lua_value_to_sample(&v)
                    .ok_or_else(|| mlua::Error::RuntimeError("set_value: 不支持的值类型".into()))?;
                let mut values = std::collections::BTreeMap::new();
                values.insert(tag, sv);
                let sample = crate::types::Sample {
                    hwid,
                    devid,
                    values,
                    timestamp: chrono::Local::now().timestamp(),
                };
                // tx.send 失败（hub 关）→ 不致命，warn（对标 on_data 推 hub）
                if tx.send(sample).await.is_err() {
                    tracing::warn!("[luaClient] set_value: hub 关闭，值未上报");
                }
                Ok(())
            }
        })?;
    ctx.set("set_value", set_value_fn)?;

    // --- L2 ctx:get_value(devid, tag) → str|nil ---
    // 读 lux HGET rtdata:<devid> <devid>.<tag> → [ts,localTime,value] → value_str
    let lux_gv = ctx_host.lux.clone();
    let get_value_fn =
        lua.create_async_function(move |_lua, (_self, devid, tag): (Value, LuaString, LuaString)| {
            let lux = lux_gv.clone();
            async move {
                let devid = devid.to_string_lossy().to_string();
                let tag = tag.to_string_lossy().to_string();
                let field = if tag.contains(&devid) {
                    tag
                } else {
                    format!("{devid}.{tag}")
                };
                match lux.execute_value("HGET", &[&format!("rtdata:{devid}"), &field]).await {
                    Ok(lux::EmbeddedValue::Bulk(b)) => {
                        let s = String::from_utf8_lossy(&b).to_string();
                        parse_rtdata_value(&s).map(|v| Value::String(_lua.create_string(&v)?))
                            .unwrap_or(Value::Nil)
                    }
                    _ => Value::Nil,
                }
            }
        })?;
    ctx.set("get_value", get_value_fn)?;

    // --- L2 ctx:get_device(devid) → table|nil ---
    // 读 lux HGETALL rtdata:<devid> → {tag: value_str}
    let lux_gd = ctx_host.lux.clone();
    let get_device_fn =
        lua.create_async_function(move |lua, (_self, devid): (Value, LuaString)| {
            let lux = lux_gd.clone();
            async move {
                let devid = devid.to_string_lossy().to_string();
                match lux.execute_value("HGETALL", &[&format!("rtdata:{devid}")]).await {
                    Ok(lux::EmbeddedValue::Array(arr)) => {
                        let t = lua.create_table()?;
                        let mut i = 0;
                        while i + 1 < arr.len() {
                            let field = match &arr[i] {
                                lux::EmbeddedValue::Bulk(b) => String::from_utf8_lossy(b).into_owned(),
                                lux::EmbeddedValue::Simple(s) => s.clone(),
                                _ => { i += 2; continue; }
                            };
                            let raw = match &arr[i + 1] {
                                lux::EmbeddedValue::Bulk(b) => String::from_utf8_lossy(b).into_owned(),
                                lux::EmbeddedValue::Simple(s) => s.clone(),
                                _ => { i += 2; continue; }
                            };
                            // field = "<devid>.<tag>" → tag；value = [ts,localTime,v] → v
                            let tag = field.strip_prefix(&format!("{devid}.")).unwrap_or(&field);
                            if let Some(v) = parse_rtdata_value(&raw) {
                                t.set(tag, v)?;
                            }
                            i += 2;
                        }
                        Ok(Value::Table(t))
                    }
                    _ => Ok(Value::Nil),
                }
            }
        })?;
    ctx.set("get_device", get_device_fn)?;

    // --- L2 ctx:list_devices() → table ---
    // 返 inst.devices（当前实例设备树）。同步（create_function，inst.devices 已知）。
    let devices: Vec<(String, String)> = inst
        .devices
        .iter()
        .map(|d| (d.label.clone(), d.value.clone()))
        .collect();
    let list_devices_fn = lua.create_function(move |lua, _self: Value| {
        let t = lua.create_table()?;
        for (i, (label, value)) in devices.iter().enumerate() {
            let dev = lua.create_table()?;
            dev.set("label", label.clone())?;
            dev.set("value", value.clone())?;
            t.set(i + 1, dev)?;
        }
        Ok(t)
    })?;
    ctx.set("list_devices", list_devices_fn)?;

    Ok(())
}

/// mlua::Value → SampleValue（对标 mod.rs lua_value_to_sample）。
fn lua_value_to_sample(v: &Value) -> Option<crate::types::SampleValue> {
    use crate::types::SampleValue;
    match v {
        Value::Integer(i) => Some(SampleValue::Int(*i)),
        Value::Number(n) => Some(SampleValue::Float(*n)),
        Value::Boolean(b) => Some(SampleValue::Bool(*b)),
        Value::String(s) => Some(SampleValue::Str(s.to_str().ok()?.to_string())),
        _ => None,
    }
}

/// 解析 rtdata cell `[ts, localTime, value]` JSON → value_str（index 2）。
fn parse_rtdata_value(cell: &str) -> Option<String> {
    let arr: serde_json::Value = serde_json::from_str(cell).ok()?;
    arr.get(2).and_then(|v| v.as_str()).map(String::from)
}
```

### - [ ] Step 3：mod.rs run() + reload_vm 接 tx

`mod.rs`（lua_client）run() 的 register 调用（line ~111）加 `tx.clone()`：
```rust
        if let Err(e) = api::register(&lua, ctx.clone(), &inst, io_engine.clone(), tx.clone()) {
```

`reload_vm`（mod.rs line ~256）签名加 `tx` + 调 register 传 tx：
```rust
async fn reload_vm(
    cfg: &config::LuaCfg,
    lctx: &LoadCtx,
    ctx_host: HostContext,
    inst: &InstConfig,
    io: io_engine::IoEngine,
    tx: mpsc::Sender<Sample>,
) -> mlua::Result<mlua::Lua> {
    let lua = engine::new_vm(cfg)?;
    load_main(&lua, lctx)?;
    api::register(&lua, ctx_host, inst, io, tx)?;  // 加 tx
    // ... on_init dry-run ...
}
```

mod.rs Evt::Reload 调 reload_vm 加 `tx.clone()`：
```rust
match reload_vm(&cfg, &lctx, ctx.clone(), &inst, io_engine.clone(), tx.clone()).await {
```

### - [ ] Step 4：run lua_client 全测 + fmt + clippy + 提交

```bash
CARGO_BUILD_JOBS=1 cargo test --lib plugin::builtin::lua_client
cargo fmt --all && cargo fmt --all --check && CARGO_BUILD_JOBS=1 cargo clippy --all-targets -- -D warnings
git add src/plugin/builtin/lua_client/api/data.rs src/plugin/builtin/lua_client/api/mod.rs src/plugin/builtin/lua_client/mod.rs
git commit -m "feat(lua): M8b L2 数据 API（set_value/get_value/get_device/list_devices）+ register 加 tx"
```

---

## Task 3: docs/lua-script-guide.md 脚本开发指南

**Files:** Create `docs/lua-script-guide.md`

### - [ ] Step 1：写指南

`docs/lua-script-guide.md` 大纲：
- # Lua 脚本开发指南
- ## 概述（luaClient 架构 + 双总线 + 脚本回调模型）
- ## 快速开始（目录约定 scripts/<name>/main.lua + InstConfig 示例）
- ## 回调函数（on_init/collect/on_data/on_write 表 + 调用时机）
- ## ctx API 参考
  - L1 通讯：request / sleep（sleep 后置）/ on_data
  - L2 数据：set_value / get_value / get_device / list_devices
  - L3 控制：send_command（联动）
  - L4 持久：kv_set / kv_get
  - L5 HTTP：http_get / http_post（feature lua-http，审计）
  - L6 工具：log_info/warn/err/debug / now / time / config / crc16（crc 后置）
- ## 配置（app_config 字段：script/kind/endpoint/serial_port/baud_rate/.../period/timeout_ms/recv_idle_ms/fail_threshold/mem_limit_mb/...）
- ## 传输（tcp/serial/udp 三传输 + recv_mode）
- ## 示例（引用 examples/lua-scripts/）
- ## 沙箱限制（禁 io/os/debug/package + 指令 hook + 内存限额 + 不暴露 lux/文件/socket）
- ## 热加载（notify 自动 + REST reload 兜底）
- ## 状态监测（stat:<inst> + REST stats）

完整内容（按 spec §6/§7/§9/§13/§14 编写，~400 行）：
```markdown
# Lua 脚本开发指南

> rs-iot luaClient 动态脚本采集——覆盖长尾工业协议（简单自定义、串口/以太网、一问一答/广播）。
> 随改随用、无需编译；沙箱护栏（库裁剪+指令hook+内存限额）挡实施工程师的脚本错误。

## 概述

luaClient 是 rs-iot 的南向 Source（与 modbus/opcua/s7 同构）。脚本（Lua 5.4）聚焦
**协议报文解析 + 数据计算**，宿主管通讯链路（tcp/serial/udp）+ 双总线（数据→lux rtdata、
控制→CtlRegistry）。详见 `docs/superpowers/specs/2026-08-06-lua-script-collector-design.md`。

## 快速开始

1. 建脚本目录 `scripts/<name>/main.lua`（`<name>` 仅 [A-Za-z0-9_-]）：
   ```lua
   function collect()
     local resp = ctx:request("\x01\x03\x00\x00\x00\x02\xC4\x0B", 200)
     if not resp or #resp < 7 then return nil end
       local temp = string.unpack(">i2", resp:sub(4,5)) / 10
       return { temp = temp }
   end
   ```
2. REST 建实例 `app_code="luaClient"` + `app_config{script="<name>", kind="tcp",
   endpoint="ip:port", period=2}` + `startinst`。
3. `redis-cli -p 6390 HGETALL rtdata:<devid>` 验采集。

## 回调函数

| 函数 | 调用时机 | 返回 | 用途 |
|---|---|---|---|
| `on_init()` | 启动时一次 | 忽略 | 初始化（建表/读 kv/日志）|
| `collect()` | 每个 period 周期（period=0 不调）| table\|nil | 主动一问一答采集 |
| `on_data(frame)` | IO task 收被动帧 | 忽略 | 广播/推送；ctx:set_value 上报 |
| `on_write(cmd)` | 收写命令 | bool\|nil | 处理写命令；cmd=JSON 解析 table |

ctx 是全局（回调不取 ctx 参数）。冒号语法 `ctx:request(...)`（隐式 self 由宿主吸收）。

## ctx API 参考

### L1 通讯
- `ctx:request(frame, timeout_ms) -> resp|nil, err?`：主动一问一答（tcp/serial/udp 统一）。
- `on_data(frame)`（脚本定义）：被动帧。

### L2 数据
- `ctx:set_value(tag, v)`：写自己当前 devid 的值（→ hub → lux rtdata）。on_data 单点上报。
- `get_value`/`get_device`/`list_devices`（见 spec §7）。

### L3 控制 / L4 持久 / L5 HTTP / L6 工具
（按 spec §7 表，完整 API 参考）

## 配置 / 传输 / 沙箱 / 热加载 / 状态监测

（按 spec §14/§8/§9/§5.3/§13 编写）
```

> **实现指引：** 完整 ~400 行，按 spec §6/§7/§9/§13/§14 + examples 展开。所有 ctx API（L1-L6）列表 + 参数 + 返回 + 示例。沙箱限制（禁 io/os/debug/package，hook 阈值，内存 32MB）。

### - [ ] Step 2：提交

```bash
git add docs/lua-script-guide.md
git commit -m "docs(lua): M8b lua-script-guide 脚本开发指南（API 参考 + 示例）"
```

---

## Task 4: examples/lua-scripts/ + L2 集成测

**Files:** Create `examples/lua-scripts/<protocol>/main.lua` + `tests/lua_l2_e2e.rs`

### - [ ] Step 1：示例脚本

`examples/lua-scripts/modbus-like/main.lua`（Modbus TCP 重写，对标 tests/lua_modbus_e2e 脚本）：
```lua
-- Modbus TCP 协议（MBAP + FC03/FC06）重写示例。
local function crc16(buf)  -- 实际 Modbus TCP 无 CRC，RTU 才用；此处示范 helper
    local s = 0
    for i = 1, #buf do
        s = (s ~ buf:byte(i)) & 0xFFFF
        for _ = 1, 8 do
            s = (s & 1 ~= 0) and ((s >> 1) ~ 0xA001) or (s >> 1)
        end
    end
    return s
end

function on_init()
    ctx:log_info("modbus-like started")
end

function collect()
    -- MBAP(tid,pid,len=6,uid) + FC03(fc=3,addr=0,qty=2)
    local req = string.pack(">I2I2I2BBI2I2", 1, 0, 6, 1, 3, 0, 2)
    local resp = ctx:request(req, 200)
    if not resp or #resp < 11 then return nil end
    local r0, r1 = string.unpack(">I2I2", resp:sub(10))  -- 跳 MBAP(7)+fc(1)+bc(1)
    return { temp = r0/10, speed = r1 }
end

function on_write(c)
    local req = string.pack(">I2I2I2BBI2I2", 2, 0, 6, 1, 6, c.addr, c.value)
    return ctx:request(req, 200) ~= nil
end
```

`examples/lua-scripts/custom-on-data/main.lua`（自定义协议 + on_data 被动上报）：
```lua
-- 完全自定义协议（STX/cmd/len/payload/ETX）+ on_data 心跳上报示例。
function collect()
    local r = ctx:request("\x02\x01\x00\x03", 200)  -- 主动查询
    if r and #r >= 6 then return { level = r:byte(4) } end
end

function on_data(frame)
    -- 被动广播帧：STX=0x02 cmd=0x10 → 心跳，set_value 上报
    if frame:byte(1) == 0x02 and frame:byte(2) == 0x10 then
        ctx:set_value("heartbeat", ctx:time())
        ctx:kv_set("last_heartbeat", ctx:time())  -- 持久化
    end
end
```

`examples/lua-scripts/README.md`：示例目录索引 + 运行说明（配 InstConfig + startinst）。

### - [ ] Step 2：L2 集成测

`tests/lua_l2_e2e.rs`（L2 端到端：set_value → hub → rtdata；get_value 读回）：
```rust
//! M8b L2 数据 API 端到端：on_data 内 set_value → hub → pipeline → rtdata；
//! get_value 读 lux rtdata 回。spec §7 L2。

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
async fn l2_set_value_writes_rtdata() {
    let data_dir = test_data_dir("lua-l2");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client).await.expect("bootstrap");

    // 脚本：collect 调 set_value("temp", 42.5)（v1 set_value devid=devices[0]）。
    let scripts = std::env::temp_dir().join(format!("lua-l2-{}", std::process::id()));
    std::fs::create_dir_all(scripts.join("l2")).expect("mkdir");
    std::fs::write(
        scripts.join("l2/main.lua"),
        "function collect()\n\
         ctx:set_value(\"temp\", 42.5)\n\
         return nil\n\  -- set_value 已推 hub，collect 返回 nil（不走 table 路径）
         end\n",
    ).expect("write");

    let shutdown = CancellationToken::new();
    let ctx = HostContext {
        lux: client.clone(), hwid: Hwid::new("it-hwid"), shutdown: shutdown.clone(),
        ctl_registry: CtlRegistry::new(), bridge_registry: BridgeRegistry::new(),
    };
    let (hub_tx, hub_rx) = tokio::sync::mpsc::channel::<Sample>(64);
    let mgr = Arc::new(InstanceManager::new(ctx.clone(), hub_tx));
    let proc: Arc<dyn Processor> = Arc::new(Passthrough);
    let pt = shutdown.child_token();
    let _pipeline = tokio::spawn(rs_iot::pipeline::run(ctx, hub_rx, proc, pt));

    let inst = InstConfig {
        inst_id: "lua-l2".into(), inst_name: "l2".into(),
        app_code: PluginName::new("luaClient"), app_type: AppType::South, auto_start: true,
        app_config: json!({
            "script": "l2", "kind": "tcp", "endpoint": "127.0.0.1:1",
            "period": 1, "scripts_root": scripts.to_str().expect("utf8"),
        }),
        devices: vec![DeviceSpec { label: "d".into(), value: "devL2".into(), children: vec![] }],
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start");

    // 等 collect → set_value → hub → pipeline → rtdata。
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let v = hget_cell(&client, "rtdata:devL2", "devL2.temp").await;
    assert!(v.contains("42.5"), "set_value 应落 rtdata temp=42.5，得到 '{v}'");

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

### - [ ] Step 3：run + fmt + clippy + 全量 test + 提交

```bash
CARGO_BUILD_JOBS=1 cargo test --test lua_l2_e2e
cargo fmt --all --check && CARGO_BUILD_JOBS=1 cargo clippy --all-targets -- -D warnings
CARGO_BUILD_JOBS=1 cargo test --all-targets --all-features
git add examples/lua-scripts tests/lua_l2_e2e.rs
git commit -m "feat(lua): M8b examples 脚本 + L2 集成测（set_value → rtdata）"
```

---

## Self-Review

**1. Spec coverage（M8b L2 + 文档）：**
- ✅ L2 set_value（推 hub，spec §7）→ Task 2
- ✅ L2 get_value/get_device（读 lux rtdata，spec §7）→ Task 2
- ✅ L2 list_devices（设备树，spec §7）→ Task 2
- ✅ docs/lua-script-guide.md（spec §4 模块划分）→ Task 3
- ✅ examples/lua-scripts/（spec §15.3/§4）→ Task 4
- ✅ api.rs 拆分守 <500（CLAUDE.md）→ Task 1
- ⚠️ L6 其他（config/dump_comm/crc/sleep）spec §17 v1 含，但 Plan 8 不做——已实现的 log/now/time + 既有 L1-L5 覆盖 v1 主体，config/crc/dump_comm/sleep 标 v2 后置（spec §17 列「脚本间消息总线」等 v2，config/crc 同类低频后置）。

**2. Placeholder scan：** Task 1 拆分用"从 api.rs 移动 X 段"指引（明确移动源，非占位——实现时机械 move）；Task 2 L2 完整代码；Task 3 文档大纲 + 关键节（完整展开实现时按 spec）；Task 4 示例脚本 + 集成测完整。

**3. Type consistency：**
- `register(lua, ctx_host, inst, io, tx)`（Task 2 mod.rs register 签名）→ data::register(lua, ctx, ctx_host, inst, tx) + mod.rs run()/reload_vm 调用传 tx.clone() 一致
- `lua_value_to_sample`（data.rs）对标 mod.rs 同名 fn（类型映射一致）
- `parse_rtdata_value(cell) -> Option<String>`（data.rs）→ get_value/get_device 用，一致
- `Sample { hwid, devid, values, timestamp }`（set_value 构造）→ 对标 mod.rs Evt::Tick Sample 一致

**已知简化（非占位）：** set_value devid=devices[0]（单 dev）；get_value 返字符串（rtdata value_str）；list_devices 返 inst.devices（非本机全部）；config/crc/dump_comm/sleep 后置 v2。

---

## Execution Handoff

本计划由 `/loop`（CronCreate `39632fab`）驱动，每轮 subagent-driven-development 执行下一个未勾选 task。CI 门槛：
```bash
cargo fmt --all --check && CARGO_BUILD_JOBS=1 cargo clippy --all-targets -- -D warnings
CARGO_BUILD_JOBS=1 cargo test --all-targets --all-features
```
（Windows CARGO_BUILD_JOBS=1）。API 503 时控制器接手。

Task 1-4 完成 → `finishing-a-development-branch` 合并 master + 删分支 → **spec 全 milestone 实现**（M1-M8）→ `CronDelete 39632fab` 收尾。

建分支：`feature/lua-m8b`（从 master `1c13396`）。

> **spec 最后 milestone**：Plan 8 完成后，luaClient 全功能（L1-L6 ctx API + tcp/serial/udp 三传输 + 沙箱 + 热加载 + kv + 联动 + 状态 REST + 文档 + 示例）落地。CronDelete 收尾（session-only job 随会话结束，但显式删确认终止）。
