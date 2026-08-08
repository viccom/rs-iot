# Lua 单实例多设备 实现 Plan（M2）

> **For agentic workers:** REQUIRED SUB-SKILL: 用 `superpowers:subagent-driven-development` 逐 task 实现。步骤用 `- [ ]` 勾选跟踪。

**Goal:** 让 `luaClient` 单实例支持**一条链路多设备**采集——`collect` 嵌套返回 `{[devid]={tag=v,...}}` 产出多 Sample、新增 `ctx:set_values(devid,{tag=v})` 批量、`set_value` 改 3 参（`devid` 参数化），对齐 `opcuaClient`/`s7Client` 已有的单实例多设备能力。

**Architecture:** 实例模型不变（一条链路 = 一个实例，`io_engine` 仍单 backend 独占）。仅扩展两处：① ctx 数据 API（`set_value` 3 参 / 新增 `set_values` 批量，`devid` 由脚本显式传入）② `collect` 返回值解析（外层 table 的 value 是 table → 多设备嵌套；是标量 → 单设备平铺，归 `devices[0]`，老脚本兼容；零歧义，因 `SampleValue` 只含标量）。`Sample` 语义不变（一设备一时刻多变量）。一次 `collect` 取一个 `now`，该次所有 Sample 共用（点间同时刻）。

**Tech Stack:** Rust + mlua（Lua 5.4 嵌入）+ tokio + `BTreeMap`。测试：lib 内 `#[cfg(test)]` 单测 + `tests/` 集成测（mock lux + 闲置 tcp endpoint，collect 不发真实 request）。

**Spec 来源:** `docs/superpowers/specs/2026-08-08-rs485-multi-device-design.md` §2.3 / §3.3 / §3.4 / §3.5（M2 章节）。

---

## File Structure

| 文件 | 职责 | M2 改动 |
|---|---|---|
| `src/plugin/builtin/lua_client/api/mod.rs` | ctx API 注册（L1-L6） | `set_value` 2 参→3 参、新增 `set_values`、抽 `push_sample` helper（DRY） |
| `src/plugin/builtin/lua_client/api/tests.rs` | api 单测 | 改 `set_value` 测为 3 参、新增 `set_values` 多 tag + `set_value` 按 devid 路由测 |
| `src/plugin/builtin/lua_client/mod.rs` | `Source::run` 主循环 + `call_collect` + `lua_value_to_sample` | `call_collect` 改返 `Vec<(devid,BTreeMap)>` 嵌套/平铺解析、Tick 遍历推多 Sample（共享 now）、移除"多设备仅采首个"warn、新增 `call_collect` 单测 |
| `tests/lua_l2_e2e.rs` | L2 端到端 | 脚本 `set_value` 2 参→3 参；`multi_device_collect_pushes_only_first_device` 更新为"平铺兼容"语义 + 改名 |
| `tests/lua_multi_device_e2e.rs`（新） | 多设备端到端 | `collect` 嵌套多 devid 各自入库 + `set_values` 批量多 tag |
| `examples/lua-scripts/custom-on-data/main.lua` | 被动上报示例 | `set_value` 2 参→3 参（`list_devices` 取 devid） |
| `examples/lua-scripts/multi-device/main.lua`（新） | 多设备示例 | `collect` 嵌套返回多从站（演示模式） |
| `examples/lua-scripts/README.md` | 示例索引 | 加 `multi-device` 条目 |
| `docs/lua-script-guide.md` | lua 开发指南 | L2 `set_value`/`set_values` 更新 + `collect` 嵌套/平铺说明 + 多设备示例 |
| `docs/superpowers/specs/2026-08-08-rs485-multi-device-design.md` | M2 spec | 文末 M2 标记 ✅ + 偏差记录 |

**破坏性变更（spec §3.4 明确许可——rs-iot 未正式发布，无线上脚本包袱）：** `ctx:set_value(tag, v)` → `ctx:set_value(devid, tag, v)`。示例脚本同步更新。

---

## Task 1: `set_value` 改 3 参 + 新增 `set_values` 批量

**Files:**
- Modify: `src/plugin/builtin/lua_client/api/mod.rs`（`set_value` 注册块 + 新增 `set_values` + 新增 `push_sample` helper）
- Modify: `src/plugin/builtin/lua_client/api/tests.rs`（改 `set_value_pushes_sample_to_hub` + 新增 2 测）
- Modify: `tests/lua_l2_e2e.rs`（脚本 `set_value` 调用 2 参→3 参）

- [ ] **Step 1: 改 `api/tests.rs` 现有 set_value 测为 3 参（先红）**

把 `set_value_pushes_sample_to_hub` 里的调用从 2 参改 3 参。定位该测试（搜 `async fn set_value_pushes_sample_to_hub`），将其中的：

```rust
    lua.load("ctx:set_value('temp', 42.5)")
        .exec_async()
        .await
        .expect("set_value");
```

改为：

```rust
    lua.load("ctx:set_value('devX', 'temp', 42.5)")
        .exec_async()
        .await
        .expect("set_value");
```

（`devX` 是该测试 `inst.devices[0].value`，devid 现显式传入。）

- [ ] **Step 2: 跑该测，确认红（签名未改，3 参调用报错）**

Run: `cargo test --lib set_value_pushes_sample_to_hub`
Expected: FAIL（`set_value` 仍 2 参，Lua 调用多传参数报错 / mlua arg mismatch）。

- [ ] **Step 3: 改 `api/mod.rs`——抽 `push_sample` helper**

在 `register` 函数**之前**（文件内任意 fn 顺序无关，建议放 `register` 上方）新增模块级 helper：

```rust
/// 组 `Sample` 推 hub（`set_value`/`set_values` 共用，DRY）。
///
/// `values` 空 → 直接返回（无点不上报）。`tx.send` 失败（hub 关）→ warn（对标
/// `run()` collect 推 hub 容错语义）。timestamp 取当前本地秒（单次 set 同时刻）。
async fn push_sample(
    tx: &mpsc::Sender<Sample>,
    hwid: &crate::types::Hwid,
    devid: String,
    values: std::collections::BTreeMap<String, SampleValue>,
) {
    if values.is_empty() {
        return;
    }
    let sample = Sample {
        hwid: hwid.clone(),
        devid,
        values,
        timestamp: chrono::Local::now().timestamp(),
    };
    if tx.send(sample).await.is_err() {
        tracing::warn!("[luaClient] set_value(s): hub 关闭，值未上报");
    }
}
```

（`SampleValue` 已在 `use crate::types::{...}` 之外——检查文件头 `use`，若 `SampleValue` 未引入则补。实际 `api/mod.rs` 头部 `use crate::types::{CtlMessage, InstConfig, InstId, Sample};`，需加 `SampleValue`。）

- [ ] **Step 4: 改 `api/mod.rs`——`set_value` 注册块改 3 参**

定位现有 `// --- L2 ctx:set_value(tag, v) ...` 注释块（搜 `let set_value_fn`）。整段替换（删 `devid_set` 预捕获，devid 改从参数取）：

```rust
    // --- L2 ctx:set_value(devid, tag, v) → 推 Sample 到 hub（数据上行总线）---
    // devid 由脚本显式传入（多设备支持：脚本可对 ctx:list_devices() 的任一 devid 上报）。
    // value 经 lua_value_to_sample 转 SampleValue；单点便捷，等价 set_values(devid,{[tag]=v})。
    let tx_set = tx.clone();
    let hwid_set = ctx_host.hwid.clone();
    let set_value_fn = lua.create_async_function(
        move |_lua, (_self, devid, tag, v): (Value, LuaString, LuaString, Value)| {
            let tx = tx_set.clone();
            let hwid = hwid_set.clone();
            async move {
                let devid = devid.to_string_lossy().to_string();
                let tag = tag.to_string_lossy().to_string();
                let sv = super::lua_value_to_sample(&v)
                    .ok_or_else(|| mlua::Error::RuntimeError("set_value: 不支持的值类型".into()))?;
                let mut values = std::collections::BTreeMap::new();
                values.insert(tag, sv);
                push_sample(&tx, &hwid, devid, values).await;
                Ok(())
            }
        },
    )?;
    ctx.set("set_value", set_value_fn)?;

    // --- L2 ctx:set_values(devid, {tag=v,...}) → 批量推一个设备多 tag（一个 Sample）---
    // 贴合 Sample 多变量语义：on_data 解析被动帧多点一次上报（点间同时刻 timestamp）。
    // table 经 pairs 遍历 → BTreeMap；空 table 不上报。
    let tx_sv = tx.clone();
    let hwid_sv = ctx_host.hwid.clone();
    let set_values_fn =
        lua.create_async_function(move |_lua, (_self, devid, table): (Value, LuaString, mlua::Table)| {
            let tx = tx_sv.clone();
            let hwid = hwid_sv.clone();
            async move {
                let devid = devid.to_string_lossy().to_string();
                let mut values = std::collections::BTreeMap::new();
                for pair in table.pairs::<String, Value>() {
                    let (k, v) = pair?;
                    if let Some(sv) = super::lua_value_to_sample(&v) {
                        values.insert(k, sv);
                    }
                }
                push_sample(&tx, &hwid, devid, values).await;
                Ok(())
            }
        })?;
    ctx.set("set_values", set_values_fn)?;
```

- [ ] **Step 5: 跑改后的 set_value 测，确认绿**

Run: `cargo test --lib set_value_pushes_sample_to_hub`
Expected: PASS。

- [ ] **Step 6: 新增 `api/tests.rs`——set_values 多 tag 测（先红）**

在 `set_value_pushes_sample_to_hub` 测**之后**新增（复制其编排，改调用 + 多 tag 断言）：

```rust
/// L2 set_values(devid, {tag=v,...}) 批量推一个 Sample（多 tag 同时刻）。
#[tokio::test]
async fn set_values_pushes_multi_tag_sample() {
    let lua = Lua::new();
    let io = spawn_mock_io();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::types::Sample>(8);
    let (ctx_host, handle, data_dir) = setup_ctx("l2-setvalues").await;
    let inst = InstConfig {
        inst_id: "it".into(),
        inst_name: "it".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: false,
        app_config: serde_json::json!({"script":"a","endpoint":"x"}),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "devX".into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    };
    register(&lua, ctx_host, &inst, io, tx).expect("register");

    lua.load("ctx:set_values('devX', { temp = 42.5, hum = 60 })")
        .exec_async()
        .await
        .expect("set_values");

    let s = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("超时")
        .expect("应收到一个 Sample");
    assert_eq!(s.devid, "devX", "一个 Sample 一个 devid");
    assert_eq!(s.values.len(), 2, "应含 2 个 tag（批量）");
    match s.values.get("temp") {
        Some(crate::types::SampleValue::Float(f)) => {
            assert!((f - 42.5).abs() < 1e-9, "temp 应为 42.5，实得 {f}")
        }
        other => panic!("temp 应是 Float(42.5)，实得 {other:?}"),
    }
    match s.values.get("hum") {
        Some(crate::types::SampleValue::Int(i)) => assert_eq!(*i, 60, "hum 应为 60"),
        other => panic!("hum 应是 Int(60)，实得 {other:?}"),
    }

    let _ = crate::db::shutdown_lux(handle).await;
    let _ = std::fs::remove_dir_all(&data_dir);
}
```

- [ ] **Step 7: 新增 `api/tests.rs`——set_value 按 devid 路由测（多设备实例，先红）**

紧接上一测新增（inst 配 2 设备，分别 set_value 不同 devid，验各自 Sample）：

```rust
/// L2 set_value(devid, tag, v) 按 devid 路由：多设备实例对不同 devid 各推一个 Sample。
#[tokio::test]
async fn set_value_routes_by_devid() {
    let lua = Lua::new();
    let io = spawn_mock_io();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::types::Sample>(8);
    let (ctx_host, handle, data_dir) = setup_ctx("l2-route").await;
    let inst = InstConfig {
        inst_id: "it".into(),
        inst_name: "it".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: false,
        app_config: serde_json::json!({"script":"a","endpoint":"x"}),
        devices: vec![
            DeviceSpec {
                label: "A".into(),
                value: "devA".into(),
                children: vec![],
                slave_id: None,
            },
            DeviceSpec {
                label: "B".into(),
                value: "devB".into(),
                children: vec![],
                slave_id: None,
            },
        ],
        host_kind: HostKind::Builtin,
    };
    register(&lua, ctx_host, &inst, io, tx).expect("register");

    lua.load("ctx:set_value('devB', 'v', 2)")
        .exec_async()
        .await
        .expect("set_value devB");

    let s = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("超时")
        .expect("应收到 Sample");
    assert_eq!(s.devid, "devB", "devid 应路由到 devB（非 devices[0]）");

    let _ = crate::db::shutdown_lux(handle).await;
    let _ = std::fs::remove_dir_all(&data_dir);
}
```

- [ ] **Step 8: 跑新测，确认绿（实现已在 Step 4 完成）**

Run: `cargo test --lib set_values_pushes_multi_tag_sample set_value_routes_by_devid`
Expected: 2 PASS。

- [ ] **Step 9: 改 `tests/lua_l2_e2e.rs`——脚本 set_value 2 参→3 参**

定位 `l2_set_value_writes_rtdata` 测里的脚本字符串（搜 `ctx:set_value(\"temp\", 42.5)`），把：

```rust
        "function collect()\n    ctx:set_value(\"temp\", 42.5)\n    return nil\nend\n",
```

改为（devid=`devL2` 显式传入，与该测 `devices[0].value="devL2"` 一致）：

```rust
        "function collect()\n    ctx:set_value(\"devL2\", \"temp\", 42.5)\n    return nil\nend\n",
```

- [ ] **Step 10: 跑 l2 端到端测，确认绿**

Run: `cargo test --test lua_l2_e2e l2_set_value_writes_rtdata`
Expected: PASS。

- [ ] **Step 11: fmt + clippy + 提交**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
git add -A
git commit -m "feat(lua): set_value 改 3 参(devid 参数化) + 新增 set_values 批量"
```

（clippy 应零警告；若 `push_sample` 触发 unused（仅 set_value 用）检查——set_values 也用，不会 unused。）

---

## Task 2: `call_collect` 嵌套/平铺解析 + Tick 推多 Sample + 移除多设备 warn

**Files:**
- Modify: `src/plugin/builtin/lua_client/mod.rs`（`call_collect` 函数体 + `Source::run` 的 Tick 分支 + 删除多设备 warn 块 + 新增 2 单测）

- [ ] **Step 1: 新增 `call_collect` 单测——嵌套返回多设备（先红）**

在 `mod.rs` 文件底部 `#[cfg(test)] mod tests { ... }` 内（`collect_pushes_sample_to_hub` 测附近）新增。`call_collect` 改后签名 `async fn call_collect(lua: &mlua::Lua, fallback_devid: &str) -> mlua::Result<Vec<(String, BTreeMap<String, SampleValue>)>>`，单测直接构造 lua + load 脚本 + 调 `super::call_collect`：

```rust
    /// call_collect 嵌套返回 {[devid]={tag=v}} → 多个 (devid, map)。
    #[tokio::test]
    async fn call_collect_nested_returns_multi_devices() {
        let lua = mlua::Lua::new();
        lua.load("function collect() return { dev1 = { a = 1, b = 2 }, dev2 = { c = 3.5 } } end")
            .exec()
            .unwrap();
        let out = super::call_collect(&lua, "fallback").await.expect("collect");
        assert_eq!(out.len(), 2, "嵌套应产出 2 个 (devid,map)");
        let by_devid: std::collections::HashMap<&str, &std::collections::BTreeMap<String, crate::types::SampleValue>> =
            out.iter().map(|(d, m)| (d.as_str(), m)).collect();
        let m1 = by_devid.get("dev1").expect("dev1 在");
        assert_eq!(m1.len(), 2, "dev1 应含 a,b");
        let m2 = by_devid.get("dev2").expect("dev2 在");
        assert_eq!(m2.len(), 1, "dev2 应含 c");
    }

    /// call_collect 平铺返回 {tag=v} → 单个 (fallback_devid, map)（老脚本兼容）。
    #[tokio::test]
    async fn call_collect_flat_uses_fallback_devid() {
        let lua = mlua::Lua::new();
        lua.load("function collect() return { temp = 42.5 } end")
            .exec()
            .unwrap();
        let out = super::call_collect(&lua, "devFirst").await.expect("collect");
        assert_eq!(out.len(), 1, "平铺应产出 1 个");
        assert_eq!(out[0].0, "devFirst", "平铺归 fallback_devid");
        assert_eq!(out[0].1.len(), 1, "含 1 tag");
    }

    /// call_collect 返 nil / 无 collect → 空 Vec（不推 Sample）。
    #[tokio::test]
    async fn call_collect_nil_or_absent_returns_empty() {
        let lua = mlua::Lua::new();
        lua.load("function collect() return nil end").exec().unwrap();
        let out = super::call_collect(&lua, "d").await.expect("collect nil");
        assert!(out.is_empty(), "nil 返回应空");
        // 无 collect 函数。
        let lua2 = mlua::Lua::new();
        let out2 = super::call_collect(&lua2, "d").await.expect("no collect");
        assert!(out2.is_empty(), "无 collect 应空");
    }
```

- [ ] **Step 2: 跑新测，确认红（call_collect 仍是旧签名/旧返回）**

Run: `cargo test --lib call_collect_nested_returns_multi_devices call_collect_flat_uses_fallback_devid call_collect_nil_or_absent_returns_empty`
Expected: FAIL（编译错——`call_collect` 签名/返回类型未改，新测调用不匹配）。

- [ ] **Step 3: 重写 `call_collect`（嵌套/平铺解析）**

定位 `mod.rs` 中 `async fn call_collect(lua: &mlua::Lua) -> mlua::Result<Option<BTreeMap<String, SampleValue>>>`（搜 `async fn call_collect`），整函数体替换为：

```rust
/// 调脚本 `collect()`，返回 `(devid, values)` 列表（多设备支持）。
///
/// 返回值 table 自动判定（spec §3.3，零歧义——`SampleValue` 只含标量）：
/// - 外层 table 的 value 是 **table** → 嵌套多设备：`{[devid]={tag=v,...}}`
///   → 每个内层 table 解析成一个 `(devid, map)`。
/// - 外层 table 的 value 是 **标量** → 平铺单设备（老脚本兼容）：
///   → 整个 table 解析成 `(fallback_devid, map)`。
/// - 嵌套中混入标量（脚本 bug）→ warn 跳过该项。
///
/// `fallback_devid` 为平铺模式的归口 devid（调用方传 `devices[0].value`）。
/// 脚本未定义 collect / 返 nil / 非 table → 空 Vec（不推 Sample）。
async fn call_collect(
    lua: &mlua::Lua,
    fallback_devid: &str,
) -> mlua::Result<Vec<(String, BTreeMap<String, SampleValue>)>> {
    let f: mlua::Function = match lua.globals().get("collect") {
        Ok(f) => f,
        Err(_) => return Ok(vec![]),
    };
    let result: mlua::Value = f.call_async(mlua::Value::Nil).await?;
    let outer = match result {
        mlua::Value::Nil => return Ok(vec![]),
        mlua::Value::Table(t) => t,
        _ => return Ok(vec![]),
    };
    // 一遍检测：外层任一 value 是 table → 嵌套模式（table.clone() 廉价，RefCounted）。
    let mut nested = false;
    for pair in outer.clone().pairs::<String, mlua::Value>() {
        if let Ok((_, v)) = pair {
            if matches!(v, mlua::Value::Table(_)) {
                nested = true;
                break;
            }
        }
    }
    let mut out: Vec<(String, BTreeMap<String, SampleValue>)> = Vec::new();
    if nested {
        for pair in outer.pairs::<String, mlua::Value>() {
            let (devid, v) = pair?;
            match v {
                mlua::Value::Table(inner) => {
                    let map = parse_table_to_sample_map(&inner)?;
                    if !map.is_empty() {
                        out.push((devid, map));
                    }
                }
                _ => tracing::warn!(
                    "[luaClient] collect 嵌套返回含非 table 值（key={devid}），已跳过"
                ),
            }
        }
    } else {
        let map = parse_table_to_sample_map(&outer)?;
        if !map.is_empty() {
            out.push((fallback_devid.to_string(), map));
        }
    }
    Ok(out)
}

/// mlua table `{tag = value}` → `BTreeMap<String, SampleValue>`（标量 value；非标量跳过）。
fn parse_table_to_sample_map(t: &mlua::Table) -> mlua::Result<BTreeMap<String, SampleValue>> {
    let mut values = BTreeMap::new();
    for pair in t.pairs::<String, mlua::Value>() {
        let (k, v) = pair?;
        if let Some(sv) = lua_value_to_sample(&v) {
            values.insert(k, sv);
        }
    }
    Ok(values)
}
```

（`lua_value_to_sample` 已存在于同文件，直接调。）

- [ ] **Step 4: 改 `Source::run` 的 Tick 分支——遍历推多 Sample**

定位 Tick 分支里 `match call_collect(&lua).await {` 块（搜 `call_collect(&lua).await`），整个 `match` 表达式替换。原逻辑（单 Sample）：

```rust
                    match call_collect(&lua).await {
                        Ok(Some(values)) if !values.is_empty() => { ... 单 Sample 推 hub ... }
                        Ok(_) => {}
                        Err(e) => { ... }
                    }
```

替换为（多 Sample，共享 `now`）：

```rust
                    match call_collect(&lua, inst.devices.first().map(|d| d.value.as_str()).unwrap_or("")).await {
                        Ok(samples) if !samples.is_empty() => {
                            for (devid, values) in samples {
                                let sample = Sample {
                                    hwid: ctx.hwid.clone(),
                                    devid,
                                    values,
                                    timestamp: now,
                                };
                                if tx.send(sample).await.is_err() {
                                    tracing::warn!("[luaClient] hub 关闭，collect 值未上报");
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!("[luaClient] collect 失败：{e}");
                            tick_err = Some(e.to_string());
                        }
                    }
```

（`now` 已在 Tick 分支顶部 `let now = chrono::Local::now().timestamp();` 取好，所有 Sample 共用→点间同时刻。`devices` 空时 fallback 为 `""`，平铺 map 归 `""` devid——等价旧"devices 空丢弃"语义的弱化版，可接受；空 devices 实例本就异常配置。）

- [ ] **Step 5: 删除"多设备仅采首个"warn 块**

定位 `mod.rs` 中（搜 `仅采集 devices\[0\]`）这段 warn：

```rust
        // luaClient 实例语义为单设备（collect 无参、set_value 限 devices[0]）；多设备仅采首个。
        if inst.devices.len() > 1 {
            tracing::warn!(
                "[luaClient] inst {} 配置 {} 个设备，仅采集 devices[0]（多设备应拆为多实例）",
                inst.inst_id,
                inst.devices.len()
            );
        }
```

整段**删除**（M2 后多设备是正常用法，collect 嵌套返回即多设备支持）。

- [ ] **Step 6: 跑 call_collect 单测 + 现有 collect 测，确认绿**

Run: `cargo test --lib call_collect collect_pushes_sample_to_hub`
Expected: 全 PASS（嵌套/平铺/nil 单测绿；`collect_pushes_sample_to_hub` 平铺仍工作——其脚本 `return { temp = 42.5 }` 平铺归 `devA`）。

- [ ] **Step 7: 跑 lua_l2_e2e 的多设备回归测（平铺兼容仍绿）**

Run: `cargo test --test lua_l2_e2e multi_device_collect_pushes_only_first_device`
Expected: PASS（平铺 `{v=1}` 仍只归 devFirst，devSecond 无——Task 3 再改其注释/改名）。

- [ ] **Step 8: fmt + clippy + 提交**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
git add -A
git commit -m "feat(lua): call_collect 嵌套/平铺自动解析多设备 + Tick 推多 Sample"
```

---

## Task 3: 重写"多设备仅采首个"回归测为"平铺兼容"语义

**Files:**
- Modify: `tests/lua_l2_e2e.rs`（`multi_device_collect_pushes_only_first_device` 测改名 + 注释更新）

- [ ] **Step 1: 改测名 + doc 注释（语义从"bug 回归"→"平铺兼容回归"）**

定位 `multi_device_collect_pushes_only_first_device` 测（搜该名）。改函数名与 doc 注释。原：

```rust
/// 多设备实例 collect 只推 devices[0]（🔴 2 回归）。
///
/// `collect` 无参不感知设备，不应遍历 `inst.devices` 重复调用（否则副作用翻倍 +
/// 推所有设备）。配 2 设备，断言 devices[0] 有采集值、devices[1] 无——修复下 collect
/// 单次化只推首个；若退回 `for dev` 循环，devSecond 也会被写入 → 断言失败。
#[tokio::test]
async fn multi_device_collect_pushes_only_first_device() {
```

改为（改名 + 新语义注释；断言体不变——平铺 `{v=1}` 仍只归 devFirst）：

```rust
/// 平铺 collect 返回 `{tag=v}` 归 devices[0]（M2 向后兼容回归）。
///
/// M2 起 collect 支持嵌套返回 `{[devid]={...}}` 多设备；但**平铺返回**（老脚本形态）
/// 仍归 `devices[0]`（fallback_devid），devSecond 不被采集。配 2 设备 + 平铺 `{v=1}`，
/// 断言 devFirst 有值、devSecond 无——若平铺误入嵌套分支（把 `v=1` 当 devid），devFirst
/// 会落空 → 断言失败。
#[tokio::test]
async fn flat_collect_attributed_to_first_device() {
```

（函数体内断言一字不改：`devFirst.v` 含 '1'、`devSecond.v` 为空——平铺兼容下仍成立。）

- [ ] **Step 2: 跑该测，确认绿**

Run: `cargo test --test lua_l2_e2e flat_collect_attributed_to_first_device`
Expected: PASS。

- [ ] **Step 3: fmt + 提交**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
git add -A
git commit -m "test(lua): 多设备回归测改名为平铺兼容语义（M2）"
```

---

## Task 4: 集成测试 `tests/lua_multi_device_e2e.rs`（嵌套 collect + set_values）

**Files:**
- Create: `tests/lua_multi_device_e2e.rs`

- [ ] **Step 1: 写集成测试文件（两个测：嵌套 collect 多 devid + set_values 批量）**

新建 `tests/lua_multi_device_e2e.rs`，编排照搬 `tests/lua_l2_e2e.rs`（start_lux + bootstrap + InstanceManager + pipeline + 闲置 tcp endpoint；collect 不发真实 request）：

```rust
//! luaClient 单实例多设备 端到端（CI 内，mock lux + 闲置 tcp endpoint）。
//!
//! M2：collect 嵌套返回 {[devid]={tag=v}} → 多 devid 各自入 rtdata；
//!     ctx:set_values(devid,{tag=v}) 批量 → 一个 Sample 多 tag 入 rtdata。
//! 编排照搬 tests/lua_l2_e2e.rs。

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

/// collect 嵌套返回 {[dev1]={a,b}, [dev2]={c}} → rtdata:dev1/dev2 各自入库（多 tag）。
#[tokio::test]
async fn nested_collect_writes_multi_devid_rtdata() {
    let data_dir = test_data_dir("lua-md-nested");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client).await.expect("bootstrap");

    let scripts = std::env::temp_dir().join(format!("lua-mdn-{}", std::process::id()));
    let script_dir = scripts.join("mdn");
    std::fs::create_dir_all(&script_dir).expect("mkdir");
    std::fs::write(
        script_dir.join("main.lua"),
        "function collect()\n  return { dev1 = { a = 1, b = 2 }, dev2 = { c = 3 } }\nend\n",
    )
    .expect("write");

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
        inst_id: "lua-mdn".into(),
        inst_name: "mdn".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: true,
        app_config: json!({
            "script": "mdn",
            "kind": "tcp",
            "endpoint": format!("127.0.0.1:{port}"),
            "period": 1,
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
        devices: vec![
            DeviceSpec { label: "d1".into(), value: "dev1".into(), children: vec![], slave_id: None },
            DeviceSpec { label: "d2".into(), value: "dev2".into(), children: vec![], slave_id: None },
        ],
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start");

    tokio::time::sleep(Duration::from_millis(2000)).await;
    let dev1_a = hget_cell(&client, "rtdata:dev1", "dev1.a").await;
    let dev1_b = hget_cell(&client, "rtdata:dev1", "dev1.b").await;
    let dev2_c = hget_cell(&client, "rtdata:dev2", "dev2.c").await;
    assert!(dev1_a.contains('1'), "dev1.a 应入库，得到 '{dev1_a}'");
    assert!(dev1_b.contains('2'), "dev1.b 应入库（同 Sample 多 tag），得到 '{dev1_b}'");
    assert!(dev2_c.contains('3'), "dev2.c 应入库（嵌套多设备），得到 '{dev2_c}'");

    shutdown.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
    let _ = std::fs::remove_dir_all(&scripts);
}

/// ctx:set_values(devid, {tag=v,...}) 批量 → 一个 Sample 多 tag 同时刻入 rtdata。
#[tokio::test]
async fn set_values_writes_multi_tag_rtdata() {
    let data_dir = test_data_dir("lua-md-setvalues");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client).await.expect("bootstrap");

    let scripts = std::env::temp_dir().join(format!("lua-mdsv-{}", std::process::id()));
    let script_dir = scripts.join("mdsv");
    std::fs::create_dir_all(&script_dir).expect("mkdir");
    std::fs::write(
        script_dir.join("main.lua"),
        "function collect()\n  ctx:set_values(\"devS\", { temp = 42.5, hum = 60 })\n  return nil\nend\n",
    )
    .expect("write");

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
        inst_id: "lua-mdsv".into(),
        inst_name: "mdsv".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: true,
        app_config: json!({
            "script": "mdsv",
            "kind": "tcp",
            "endpoint": format!("127.0.0.1:{port}"),
            "period": 1,
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
        devices: vec![DeviceSpec { label: "d".into(), value: "devS".into(), children: vec![], slave_id: None }],
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start");

    tokio::time::sleep(Duration::from_millis(2000)).await;
    let temp = hget_cell(&client, "rtdata:devS", "devS.temp").await;
    let hum = hget_cell(&client, "rtdata:devS", "devS.hum").await;
    assert!(temp.contains("42.5"), "set_values temp 应入库，得到 '{temp}'");
    assert!(hum.contains("60"), "set_values hum 应入库（批量多 tag），得到 '{hum}'");

    shutdown.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
    let _ = std::fs::remove_dir_all(&scripts);
}

/// HGET rtdata cell（`[ts, localTime, value]` JSON 文本）原样返回，断言用 contains。
async fn hget_cell(client: &lux::EmbeddedClient, key: &str, field: &str) -> String {
    use lux::EmbeddedValue;
    match client.execute_value("HGET", &[key, field]).await {
        Ok(EmbeddedValue::Bulk(b)) => String::from_utf8_lossy(&b).to_string(),
        Ok(EmbeddedValue::Simple(s)) => s,
        _ => String::new(),
    }
}
```

- [ ] **Step 2: 跑新集成测，确认绿**

Run: `cargo test --test lua_multi_device_e2e`
Expected: 2 PASS。

- [ ] **Step 3: fmt + clippy + 提交**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
git add -A
git commit -m "test(lua): 多设备端到端（嵌套 collect 多 devid + set_values 批量）"
```

---

## Task 5: 示例脚本 + 文档更新

**Files:**
- Modify: `examples/lua-scripts/custom-on-data/main.lua`
- Create: `examples/lua-scripts/multi-device/main.lua`
- Modify: `examples/lua-scripts/README.md`
- Modify: `docs/lua-script-guide.md`
- Modify: `docs/superpowers/specs/2026-08-08-rs485-multi-device-design.md`

- [ ] **Step 1: 改 custom-on-data 脚本 set_value 2 参→3 参**

`examples/lua-scripts/custom-on-data/main.lua` 的 `on_data` 里 `ctx:set_value("heartbeat", ctx:time())` 改为（用 `list_devices` 取首设备 devid）：

```lua
function on_data(frame)
    -- 被动广播：STX=0x02 cmd=0x10 → 心跳，set_value 上报 + kv 持久化（跨重启）。
    if frame:byte(1) == 0x02 and frame:byte(2) == 0x10 then
        local dev = ctx:list_devices()[1].value   -- 本实例首设备 devid（M2：set_value 需显式 devid）
        ctx:set_value(dev, "heartbeat", ctx:time())
        ctx:kv_set("last_heartbeat", ctx:time())
    end
end
```

- [ ] **Step 2: 新增 multi-device 示例脚本**

新建 `examples/lua-scripts/multi-device/main.lua`（演示 collect 嵌套返回多从站——一条 RS485 总线多 unit id 轮询；endpoint 需真实设备或 mock，此处演示模式）：

```lua
-- 单实例多设备示例：一条 RS485 总线挂多台从站（同协议、同参数、不同 unit id）。
-- collect 嵌套返回 {[devid]={tag=v,...}} → 宿主为每 devid 各产一个 Sample（M2）。
-- 配合 docs/lua-script-guide.md「多设备」节。

-- 构造 Modbus RTU 读保持寄存器请求（FC03）：unit + addr + qty + CRC16。
local function read_req(unit, addr, qty)
    local pdu = string.pack(">BBHH", unit, 3, addr, qty)
    -- CRC16（Modbus）附尾——实际 RTU 传输需；本示例演示组帧。
    local s = 0xFFFF
    for i = 1, #pdu do
        s = (s ~ pdu:byte(i)) & 0xFFFF
        for _ = 1, 8 do
            s = (s & 1 ~= 0) and ((s >> 1) ~ 0xA001) or (s >> 1)
        end
    end
    return pdu .. string.pack("<H", s)
end

function collect()
    local out = {}
    -- 遍历本实例设备树（devices[].value = devid），逐从站轮询。
    for _, dev in ipairs(ctx:list_devices()) do
        local unit = tonumber(ctx:kv_get("unit_" .. dev.value)) or 1  -- devid→unit 映射存 kv
        local resp = ctx:request(read_req(unit, 0, 2), 200)
        if resp and #resp >= 5 then
            -- 跳 unit(1)+fc(1)+bc(1)=3，解两寄存器（big-endian int16）。
            local r0 = string.unpack(">i2", resp:sub(4, 5))
            out[dev.value] = { voltage = r0 / 10 }   -- 嵌套：out[devid] = { tag = v }
        end
    end
    return out   -- {[meter1]={voltage=..}, [meter2]={voltage=..}} → 2 个 Sample
end
```

- [ ] **Step 3: 更新 examples/lua-scripts/README.md**

目录列表加 `multi-device` 条目。定位 README「## 目录」段，在 `custom-on-data` 项后加：

```markdown
- **`multi-device/`**：单实例多设备（一条 RS485 总线多从站），`collect` 嵌套返回 `{[devid]={tag=v}}`（M2）。
```

并在「## 写命令」段后新增「## 多设备」段：

```markdown
## 多设备

M2 起 `luaClient` 单实例支持一条链路多设备（如一条 RS485 总线多台从站）：

- `collect` **嵌套返回** `{[devid]={tag=v, ...}, ...}` → 宿主为每 `devid` 各产一个 `Sample`。
- `ctx:set_values(devid, {tag=v,...})` 批量上报一个设备多点（一个 `Sample`，同时刻 timestamp）。
- `ctx:set_value(devid, tag, v)` 单点（3 参，`devid` 显式传入）。

平铺返回 `{tag=v}`（老脚本形态）仍归 `devices[0]`，向后兼容。详见 `docs/lua-script-guide.md`。
```

- [ ] **Step 4: 更新 docs/lua-script-guide.md**

① 「回调函数」表后 `collect 返 table` 段（约 54-55 行）改为：

```markdown
`collect` 返 table：
- **平铺**（单设备，老脚本兼容）：`{ tag = value, ... }` → 宿主按 `inst.devices[0]` 组 `Sample` 推 hub。
- **嵌套**（多设备，M2）：`{ [devid] = { tag = value, ... }, ... }` → 宿主为每 `devid` 各组一个 `Sample`（同一次 collect 共用同一 timestamp，点间同时刻）。适合一条 RS485 总线多从站。

自动判定：外层 table 的 value 是 table → 嵌套；是标量 → 平铺（零歧义）。
```

② 「### L2 数据」段，把现有 `ctx:set_value(tag, v)` 行改为 + 新增 `set_values`：

```markdown
### L2 数据（自身实例）

- `ctx:set_value(devid, tag, v)`：async，写指定 `devid` 的 `tag` 值（3 参，`devid` 显式传入——
  多设备实例可对 `ctx:list_devices()` 任一 devid 上报）。推 `Sample` 到 hub（与 `collect` 同路径，
  pipeline 入库）。`on_data` 单点上报用。
- `ctx:set_values(devid, {tag=v, ...})`：async，批量写一个设备多点 → 一个 `Sample`（多 tag 同时刻
  timestamp）。`on_data` 解析被动帧多点上报用（贴合 `Sample` 多变量语义）。
- `ctx:get_value(devid, tag) -> str|nil`：async，读 lux `rtdata:<devid>` 的 `tag` 最新值字符串。
- `ctx:get_device(devid) -> table|nil`：async，读设备全部数据点 `{tag = value_str, ...}`；
  缺省设备（无数据）返 `nil`。
- `ctx:list_devices() -> table`：返当前实例设备树 `[{label=.., value=..}, ...]`（同步）。
```

③ 「## 示例」段，加 multi-device 指向：

```markdown
- `multi-device/`：单实例多设备（一条 RS485 总线多从站），`collect` 嵌套返回（M2）。
```

- [ ] **Step 5: 更新 spec 文末（M2 标记 ✅）**

`docs/superpowers/specs/2026-08-08-rs485-multi-device-design.md` 文末 `*本 spec 随实现推进更新；M1/M2 完成后补实测数据与偏差记录。*` 行之前，加一段：

```markdown
## 6. 实现状态

- **M1 modbus 多 slave**：✅ 已实现并合并 master（2026-08-08，9 commit）。两策略（round_robin/sequential）+ per-request 超时故障隔离 + device 级 slave_id + 旧配置兼容。集成测 `tests/modbus_multi_slave_e2e.rs`（5 测，mock TCP modbus server）。
- **M2 lua 多设备**：✅ 已实现（本里程碑）。`call_collect` 嵌套/平铺自动解析 + `set_values` 批量 + `set_value` 3 参（devid 参数化）。集成测 `tests/lua_multi_device_e2e.rs`。破坏性变更：`set_value` 2 参→3 参（spec §3.4 许可，无线上脚本）。
- **M3 真机验证**：⏳ 待办（RS485 多从站 modbus + lua 各一场景）。
```

- [ ] **Step 6: fmt + clippy（保零警告）+ 提交**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
git add -A
git commit -m "docs(lua): M2 多设备示例 + 指南 + spec 状态更新（set_value 3 参/set_values/嵌套 collect）"
```

（lua 脚本与 md 不参与 Rust 编译；clippy 仅守 Rust 代码零警告。）

---

## Task 6: 全量质量门收尾

**Files:** 无（仅验证）

- [ ] **Step 1: fmt 检查**

Run: `cargo fmt --all --check`
Expected: 无 diff 输出（零警告门槛）。

- [ ] **Step 2: clippy 全目标零警告**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: 无警告通过。

- [ ] **Step 3: 全量测试（含 all-features）**

Run: `cargo test --all-targets --all-features`
Expected: 全绿（lib 单测 + 全 `tests/*` 集成测，含新 `lua_multi_device_e2e`）。

- [ ] **Step 4: 文档构建零警告**

Run: `cargo doc --no-deps`
Expected: 无 warning（`missing_docs` 等）。

- [ ] **Step 5: 收尾提交（若有 fmt/小修）**

```bash
git status   # 确认工作区干净（Task 1-5 已各自提交）
```

若 Step 1-4 全绿且工作区干净，无需额外提交——M2 实现完成。

---

## 自检（writing-plans self-review）

**1. Spec coverage（spec §2.3/§3.3/§3.4/§3.5 逐条）：**
- §3.3 `set_value` 改 3 参 → Task 1 ✓
- §3.3 新增 `set_values` → Task 1 ✓
- §3.3 `call_collect` 嵌套/平铺解析 → Task 2 ✓
- §3.3 timestamp 一致性（一次 collect 一个 now）→ Task 2 Step 4（共享 `now`）✓
- §3.4 向后兼容（collect 平铺归 devices[0]）→ Task 2（平铺分支）+ Task 3（回归测）✓
- §3.4 `set_value` 旧 2 参不兼容（改 3 参 + 示例更新）→ Task 1 + Task 5 Step 1 ✓
- §3.5 单测（call_collect 嵌套/平铺、set_values、set_value 3 参）→ Task 1（api 测）+ Task 2（call_collect 测）✓
- §3.5 集成测（lua_multi_device_e2e）→ Task 4 ✓
- 无遗漏。

**2. Placeholder scan：** 无 TBD/TODO/「类似 Task N」/「适当错误处理」。每步含完整代码或确切命令。✓

**3. Type consistency：**
- `call_collect` 签名 `(lua, fallback_devid: &str) -> Result<Vec<(String, BTreeMap<String, SampleValue>)>>` —— Task 2 定义、Task 2 单测调用一致 ✓
- `push_sample(tx: &Sender, hwid: &Hwid, devid: String, values: BTreeMap)` —— Task 1 定义、set_value/set_values 调用一致 ✓
- `set_value` Lua 签名 `(devid, tag, v)`、`set_values` `(devid, table)` —— api 注册、tests、集成测脚本调用一致 ✓
- `SampleValue` import：Task 1 Step 3 提醒补 `api/mod.rs` 头部 use ✓

**4. 破坏性变更处理：** `set_value` 2→3 参，Task 1 同步改 `api/tests.rs` + `tests/lua_l2_e2e.rs`，Task 5 同步改 `custom-on-data` 示例——所有调用点覆盖 ✓

---

*Plan 完成。执行：`superpowers:subagent-driven-development`，逐 task 派发实现子代理 + 两阶段 review。*
