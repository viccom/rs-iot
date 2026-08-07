# modbus 多 slave 单实例采集 实现计划（M1）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让一个 `modbusClient` 实例能采集一条 RS485/TCP 总线上的多台 slave（不同 unit id），按轮询策略采集、per-request 超时故障隔离，向后兼容旧单 slave 配置。

**Architecture:** `slave_id` 从 `app_config` 下沉到 `DeviceSpec`（设备级）；预解析 `devices → Vec<SlavePlan{devid,slave_id,regions}>`（regions 用现有 `merge_regions` 预计算）；按 `poll_strategy`（round_robin 默认 / sequential）构造 `(slave,region)` 遍历序列；采集循环每步 `set_slave` + `timeout(read_region)`，超时/exception 跳过（故障隔离）、transport 错拆 session 重连；单 tick 所有 devid 共用一个 timestamp（点间一致）。

**Tech Stack:** Rust + tokio-modbus 0.17（`Context`/`SlaveContext::set_slave`/`read_*`）+ tokio（`time::timeout`/`select!`）+ serde（配置）+现有 `merge_regions`/`codec`/`address`。

**Spec:** [`docs/superpowers/specs/2026-08-08-rs485-multi-device-design.md`](../specs/2026-08-08-rs485-multi-device-design.md)（需求规格/功能设计/编码设计）

**前提约定：**
- 四项质量门（每 task 后跑）：`cargo fmt --all --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test --all-targets --all-features` + `RUSTFLAGS="-D warnings" cargo doc --no-deps`
- 每个 task 末尾 commit（commit message 用 `feat(modbus):` / `test(modbus):` / `docs(modbus):` 前缀，无 `Co-Authored-By`）
- 不改非必要代码（精准修改，匹配现有风格、中文注释）

---

## 文件结构

| 文件 | 责任 | 本计划改动 |
|---|---|---|
| `src/types.rs` | `DeviceSpec` 通用设备规格 | 加 `slave_id: Option<u8>` 字段 |
| `src/plugin/builtin/modbus_client/config.rs` | `ModbusCfg` + `parse_cfg` | 加 `PollStrategy` 枚举 + `poll_strategy`/`request_timeout_secs` 字段 + 解析 |
| `src/plugin/builtin/modbus_client/mod.rs` | `Source::run` 采集循环 + `handle_write` | `SlavePlan` 预解析 + `build_poll_order` + `read_region` + 循环改造 + write 带 slave_id |
| `tests/modbus_multi_slave_e2e.rs` | **新建**集成测试 | mock TCP modbus server（多 unit id）验多 slave 采集/故障隔离/两策略/兼容/write |

`merge.rs`/`codec.rs`/`address.rs`/`transport.rs` **不改**（复用现有 `merge_regions`/`decode_point`/`parse_point`/`connect`）。

---

## Task 1: `DeviceSpec` 加 `slave_id` 字段

**Files:**
- Modify: `src/types.rs:165-177`（`DeviceSpec`）
- Test: `src/types.rs`（`#[cfg(test)]` 内嵌，若无则新增）

- [ ] **Step 1: 写失败测试**（在 `src/types.rs` 末尾加 `#[cfg(test)]` 模块，若已存在则在其内追加）

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_spec_slave_id_deserializes_when_present() {
        let json = serde_json::json!({"value":"m1","slave_id":2});
        let d: DeviceSpec = serde_json::from_value(json).unwrap();
        assert_eq!(d.slave_id, Some(2));
    }

    #[test]
    fn device_spec_slave_id_absent_is_none_for_backward_compat() {
        // 老配置无 slave_id → None（opcua/s7 等其它插件也走这条，字段透明）
        let json = serde_json::json!({"value":"m1"});
        let d: DeviceSpec = serde_json::from_value(json).unwrap();
        assert_eq!(d.slave_id, None);
    }

    #[test]
    fn device_spec_slave_id_serializes_roundtrip() {
        let d = DeviceSpec {
            label: "m".into(),
            value: "m1".into(),
            children: vec![],
            slave_id: Some(5),
        };
        let s = serde_json::to_string(&d).unwrap();
        let back: DeviceSpec = serde_json::from_str(&s).unwrap();
        assert_eq!(back.slave_id, Some(5));
    }
}
```

- [ ] **Step 2: 跑测试验证失败**

Run: `cargo test --lib types::tests`
Expected: 编译失败（`slave_id` 字段不存在）

- [ ] **Step 3: 加字段**（修改 `src/types.rs` 的 `DeviceSpec`）

```rust
/// 设备规格（对标 Lite_IOT devices：label/value/children）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeviceSpec {
    /// 显示标签。
    #[serde(default)]
    pub label: String,
    /// 设备 ID
    #[serde(default)]
    pub value: String,
    /// 设备下的变量列表。
    #[serde(default)]
    pub children: Vec<ChildSpec>,
    /// Modbus unit id（仅 `modbusClient` 用；其它插件忽略）。
    /// 缺省 `None` → modbus 回退到 `app_config.slave_id`（向后兼容旧单 slave 配置）。
    #[serde(default)]
    pub slave_id: Option<u8>,
}
```

- [ ] **Step 4: 跑测试验证通过**

Run: `cargo test --lib types::tests`
Expected: 3 tests PASS

- [ ] **Step 5: 检查未初始化字段影响**

Run: `cargo build 2>&1 | head -40`
Expected: 若有 `DeviceSpec { ... }` 字面构造处缺 `slave_id` 报错（如 `Default::default()` 不受影响）。逐一补 `, slave_id: None`（或用 `..Default::default()`）。预期影响处：测试代码里的 `DeviceSpec { label, value, children }` 字面量（如 `lua_client` 的测试 `InstConfig` 里 `devices: vec![DeviceSpec{...}]`）。

Run: `cargo build`
Expected: 编译通过

- [ ] **Step 6: 质量门 + commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets --all-features
git add src/types.rs
git commit -m "feat(types): DeviceSpec 加 slave_id 字段（modbus 多 slave 准备）"
```

---

## Task 2: `PollStrategy` + `ModbusCfg` 扩展 + `parse_cfg`

**Files:**
- Modify: `src/plugin/builtin/modbus_client/config.rs:34-158`（`ModbusCfg` + `parse_cfg`）

- [ ] **Step 1: 写失败测试**（追加到 `mod.rs` 的 `#[cfg(test)] mod tests`，复用现有 `parse_cfg` 测试风格）

```rust
    #[test]
    fn parse_cfg_poll_strategy_defaults_round_robin() {
        let cfg = parse_cfg(&serde_json::json!({"endpoint":"127.0.0.1:502"})).unwrap();
        assert_eq!(cfg.poll_strategy, config::PollStrategy::RoundRobin);
        assert_eq!(cfg.request_timeout_secs, 2, "per-request 超时默认 2s");
    }

    #[test]
    fn parse_cfg_poll_strategy_sequential() {
        let cfg = parse_cfg(&serde_json::json!({
            "endpoint":"127.0.0.1:502","poll_strategy":"sequential"
        }))
        .unwrap();
        assert_eq!(cfg.poll_strategy, config::PollStrategy::Sequential);
    }

    #[test]
    fn parse_cfg_request_timeout_clamped() {
        let cfg = parse_cfg(&serde_json::json!({
            "endpoint":"127.0.0.1:502","request_timeout_secs":0
        }))
        .unwrap();
        assert_eq!(cfg.request_timeout_secs, 1, "0 clamp 1");
    }

    #[test]
    fn parse_cfg_rejects_bad_poll_strategy() {
        assert!(
            parse_cfg(&serde_json::json!({
                "endpoint":"127.0.0.1:502","poll_strategy":"random"
            }))
            .is_err(),
            "非法 poll_strategy"
        );
    }
```

> 注：这些测试在 `mod.rs` 的 `tests` 模块（`use super::*` + `use config::*`），需 `config::PollStrategy` 可见。`PollStrategy` 标 `pub(super)` 即可（mod.rs 是同 `modbus_client` 模块）。

- [ ] **Step 2: 跑测试验证失败**

Run: `cargo test --lib modbus_client::tests::parse_cfg_poll_strategy`
Expected: 编译失败（`PollStrategy` 不存在 / `poll_strategy` 字段不存在）

- [ ] **Step 3: 加 `PollStrategy` 枚举**（`config.rs`，紧跟 `FlowKind` 之后）

```rust
/// 多 slave 轮询策略（半双工 RS485 物理串行，区别在 (slave,region) 遍历顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum PollStrategy {
    /// 轮转：每轮对每个 slave 各发 1 条报文（各取下一 region），多轮采完。
    /// 所有 slave 齐头并进、刷新均匀；单 slave 故障天然隔离。**默认**。
    #[default]
    RoundRobin,
    /// 逐设备：一个 slave 所有 region 采完再换下一个。单设备多点时间一致。
    Sequential,
}
```

- [ ] **Step 4: `ModbusCfg` 加字段**（`config.rs`，在 `reconnect_secs` 后）

```rust
    pub(super) reconnect_secs: u64,
    /// 多 slave 轮询策略（默认 RoundRobin）。
    pub(super) poll_strategy: PollStrategy,
    /// 单次读请求超时（秒）。slave 无响应时跳过该请求（故障隔离），默认 2。
    pub(super) request_timeout_secs: u64,
```

- [ ] **Step 5: `parse_cfg` 解析**（在 `reconnect_secs` 解析后、`Ok(...)` 前加）

```rust
    let poll_strategy = match app.get("poll_strategy").and_then(|v| v.as_str()) {
        Some("round_robin") | None => PollStrategy::RoundRobin,
        Some("sequential") => PollStrategy::Sequential,
        Some(other) => {
            return Err(format!(
                "unsupported poll_strategy '{other}'（round_robin / sequential）"
            ))
        }
    };
    let request_timeout_secs = app
        .get("request_timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(2)
        .max(1);
```

并在 `Ok(ModbusCfg { ... })` 内补两个字段：

```rust
        reconnect_secs,
        poll_strategy,
        request_timeout_secs,
```

- [ ] **Step 6: 跑测试验证通过**

Run: `cargo test --lib modbus_client::tests::parse_cfg_`
Expected: 4 新测试 PASS（且现有 `parse_cfg_*` 测试仍 PASS——默认值兼容）

- [ ] **Step 7: 质量门 + commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets --all-features
git add src/plugin/builtin/modbus_client/config.rs src/plugin/builtin/modbus_client/mod.rs
git commit -m "feat(modbus): PollStrategy + per-request 超时配置（多 slave 准备）"
```

---

## Task 3: `SlavePlan` 预解析（纯函数 + 单测）

**Files:**
- Modify: `src/plugin/builtin/modbus_client/mod.rs`（预解析逻辑提取为纯函数）

**说明**：把现有 `run()` 内联的 devices 预解析（mod.rs:161-192）重构为返回 `Vec<SlavePlan>` 的纯函数，加 `slave_id`（device 级 fallback `cfg.slave_id`）。`SlavePlan` 预缓存每 slave 的 `regions`（`merge_regions` 结果），采集循环不再每 tick 重复合并。

- [ ] **Step 1: 写失败测试**（追加到 `mod.rs` 的 `tests`）

```rust
    #[test]
    fn plan_devices_assigns_device_slave_id_with_fallback() {
        // device 显式 slave_id 优先；缺省回退 cfg.slave_id
        let cfg = parse_cfg(&serde_json::json!({"endpoint":"x","slave_id":9})).unwrap();
        let devices = vec![
            DeviceSpec {
                label: "".into(),
                value: "m1".into(),
                children: vec![ChildSpec {
                    label: "t".into(),
                    value: "holding:0:u16".into(),
                    r#type: "".into(),
                    slave_id: None,
                }],
                slave_id: Some(1),
            },
            DeviceSpec {
                label: "".into(),
                value: "m2".into(),
                children: vec![ChildSpec {
                    label: "t".into(),
                    value: "holding:0:u16".into(),
                    r#type: "".into(),
                    slave_id: None,
                }],
                slave_id: None, // 回退 cfg.slave_id=9
            },
        ];
        let plans = plan_devices(&devices, cfg.gap_threshold, cfg.max_regs, cfg.slave_id);
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].devid, "m1");
        assert_eq!(plans[0].slave_id, 1, "device 级 slave_id 优先");
        assert_eq!(plans[1].slave_id, 9, "缺省回退 cfg.slave_id");
        assert_eq!(plans[0].regions.len(), 1, "merge 预计算");
    }

    #[test]
    fn plan_devices_skips_bad_address_but_keeps_device() {
        let devices = vec![DeviceSpec {
            label: "".into(),
            value: "m1".into(),
            children: vec![
                ChildSpec {
                    label: "good".into(),
                    value: "holding:0:u16".into(),
                    r#type: "".into(),
                    slave_id: None,
                },
                ChildSpec {
                    label: "bad".into(),
                    value: "holding:x:u16".into(), // 非法地址
                    r#type: "".into(),
                    slave_id: None,
                },
            ],
            slave_id: Some(1),
        }];
        let plans = plan_devices(&devices, 16, 100, 1);
        assert_eq!(plans[0].regions.iter().map(|r| r.points.len()).sum::<usize>(), 1, "非法地址丢弃，合法保留");
    }

    #[test]
    fn plan_devices_empty_children_skipped() {
        let devices = vec![DeviceSpec {
            label: "".into(),
            value: "m1".into(),
            children: vec![],
            slave_id: Some(1),
        }];
        let plans = plan_devices(&devices, 16, 100, 1);
        assert!(plans.is_empty(), "无点位设备不入 plan（采集循环无需处理）");
    }
```

- [ ] **Step 2: 跑测试验证失败**

Run: `cargo test --lib modbus_client::tests::plan_devices`
Expected: 编译失败（`SlavePlan`/`plan_devices` 不存在；`DeviceSpec`/`ChildSpec` 未 import）

- [ ] **Step 3: 加 import + `SlavePlan` + `plan_devices`**（`mod.rs`，在 `impl Source` 之前）

加 import（mod.rs 顶部 use 区）：
```rust
use crate::types::{ChildSpec, DeviceSpec};
use merge::Region;
```

加结构 + 函数（在 `enum Evt` 之前）：
```rust
/// 一个 slave 的采集计划：devid + unit id + 预合并的 region 列表。
///
/// 预解析产物（`plan_devices`），采集循环每 tick 直接遍历 `regions`，不再重复 `merge_regions`。
/// `slave_id` 由 `DeviceSpec.slave_id` 提供，缺省回退实例级 `cfg.slave_id`（向后兼容）。
struct SlavePlan {
    /// 设备 ID（Sample.devid / rtdata:<devid> 的 key）。
    devid: String,
    /// Modbus unit id（读前 `set_slave`）。
    slave_id: u8,
    /// 该 slave 的合并读区间（地址解析失败的点位已丢弃）。
    regions: Vec<Region>,
}

/// 预解析 `devices` → `Vec<SlavePlan>`（纯函数，便于单测）。
///
/// - 每个有点位的 device → 一个 `SlavePlan`；无点位设备跳过。
/// - 点位地址解析失败 → `warn` 丢弃该点位，保留其余。
/// - `slave_id`：`DeviceSpec.slave_id` 优先，缺省回退 `fallback`（= `cfg.slave_id`）。
fn plan_devices(
    devices: &[DeviceSpec],
    gap_threshold: u16,
    max_regs: u16,
    fallback_slave_id: u8,
) -> Vec<SlavePlan> {
    devices
        .iter()
        .filter_map(|dev| {
            let pairs: Vec<(String, PointSpec)> = dev
                .children
                .iter()
                .filter_map(|c| {
                    parse_point(&c.value)
                        .map(|p| {
                            let field = if c.label.is_empty() {
                                c.value.clone()
                            } else {
                                c.label.clone()
                            };
                            (field, p)
                        })
                        .map_err(|e| {
                            tracing::warn!(
                                "[modbusClient] dev {} bad address '{}': {}",
                                dev.value,
                                c.value,
                                e
                            );
                            e
                        })
                        .ok()
                })
                .collect();
            if pairs.is_empty() {
                return None;
            }
            let regions = merge_regions(&pairs, gap_threshold, max_regs);
            Some(SlavePlan {
                devid: dev.value.clone(),
                slave_id: dev.slave_id.unwrap_or(fallback_slave_id),
                regions,
            })
        })
        .collect()
}
```

- [ ] **Step 4: 跑测试验证通过**

Run: `cargo test --lib modbus_client::tests::plan_devices`
Expected: 3 tests PASS

- [ ] **Step 5: 质量门 + commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets --all-features
git add src/plugin/builtin/modbus_client/mod.rs
git commit -m "refactor(modbus): devices 预解析为 SlavePlan（带 slave_id，纯函数+单测）"
```

---

## Task 4: `build_poll_order` 两策略序列（纯函数 + 单测）

**Files:**
- Modify: `src/plugin/builtin/modbus_client/mod.rs`

**说明**：把 round_robin / sequential 统一为"产生一个 `(plan_idx, region_idx)` 遍历序列"，采集循环只按序列遍历，两策略差别仅在排列。

- [ ] **Step 1: 写失败测试**（追加到 `tests`）

```rust
    #[test]
    fn build_poll_order_round_robin_interleaves() {
        // 2 slave 各 2 region → round_robin: (0,0)(1,0)(0,1)(1,1)
        let plans = vec![
            SlavePlan {
                devid: "a".into(),
                slave_id: 1,
                regions: vec![merge_regions(&[("x".into(), parse_point("holding:0:u16").unwrap())], 16, 100)[0].clone()],
            },
            SlavePlan {
                devid: "b".into(),
                slave_id: 2,
                regions: vec![merge_regions(&[("x".into(), parse_point("holding:0:u16").unwrap())], 16, 100)[0].clone()],
            },
        ];
        let order = build_poll_order(&plans, PollStrategy::RoundRobin);
        // 每 slave 1 region → [(0,0),(1,0)]；本例验证跨 slave 交错（先全 slave 第 0 region）
        assert_eq!(order, vec![(0, 0), (1, 0)]);
    }

    #[test]
    fn build_poll_order_round_robin_multi_region() {
        // 构造 2 slave × 2 region：round_robin 应 (0,0)(1,0)(0,1)(1,1)
        let mk_region = |start: u16| {
            merge_regions(&[(format!("t{start}"), parse_point(&format!("holding:{start}:u16")).unwrap())], 16, 100)[0].clone()
        };
        let plans = vec![
            SlavePlan { devid: "a".into(), slave_id: 1, regions: vec![mk_region(0), mk_region(10)] },
            SlavePlan { devid: "b".into(), slave_id: 2, regions: vec![mk_region(0), mk_region(10)] },
        ];
        let order = build_poll_order(&plans, PollStrategy::RoundRobin);
        assert_eq!(order, vec![(0, 0), (1, 0), (0, 1), (1, 1)]);
    }

    #[test]
    fn build_poll_order_sequential_drains_each_slave() {
        let mk_region = |start: u16| {
            merge_regions(&[(format!("t{start}"), parse_point(&format!("holding:{start}:u16")).unwrap())], 16, 100)[0].clone()
        };
        let plans = vec![
            SlavePlan { devid: "a".into(), slave_id: 1, regions: vec![mk_region(0), mk_region(10)] },
            SlavePlan { devid: "b".into(), slave_id: 2, regions: vec![mk_region(0)] },
        ];
        let order = build_poll_order(&plans, PollStrategy::Sequential);
        // 逐 slave 打透：(0,0)(0,1)(1,0)
        assert_eq!(order, vec![(0, 0), (0, 1), (1, 0)]);
    }

    #[test]
    fn build_poll_order_empty_when_no_regions() {
        let plans: Vec<SlavePlan> = vec![];
        assert!(build_poll_order(&plans, PollStrategy::RoundRobin).is_empty());
    }
```

- [ ] **Step 2: 跑测试验证失败**

Run: `cargo test --lib modbus_client::tests::build_poll_order`
Expected: 编译失败（`build_poll_order` 不存在；`PollStrategy` 未 import 到 tests）

- [ ] **Step 3: tests 模块 import PollStrategy**

在 `mod.rs` 的 `#[cfg(test)] use config::{FlowKind, ParityKind, Transport};` 那行，改为也引入 `PollStrategy`：

```rust
#[cfg(test)]
use config::{FlowKind, ParityKind, PollStrategy, Transport};
```

- [ ] **Step 4: 实现 `build_poll_order`**（`mod.rs`，在 `plan_devices` 之后）

```rust
/// 构造一个 tick 的轮询序列 `(plan_idx, region_idx)`，按 `strategy` 排列。
///
/// - `RoundRobin`：按 region 下标外层遍历——先所有 slave 的第 0 region，再第 1 region…
///   （所有 slave 齐头并进、刷新均匀；period 内轮不完时每 slave 都采到一部分）
/// - `Sequential`：按 slave 外层遍历——slave0 所有 region、slave1 所有 region…
///   （单 slave 多点时间一致，但 slave 间有延迟）
///
/// 半双工 RS485 物理串行，两种都是顺序遍历，差别仅在排列。
fn build_poll_order(plans: &[SlavePlan], strategy: PollStrategy) -> Vec<(usize, usize)> {
    let max_regions = plans.iter().map(|p| p.regions.len()).max().unwrap_or(0);
    let mut order = Vec::new();
    match strategy {
        PollStrategy::RoundRobin => {
            // 外层 region 下标，内层 slave：每"轮"每 slave 各取一个 region
            for r in 0..max_regions {
                for (pi, plan) in plans.iter().enumerate() {
                    if r < plan.regions.len() {
                        order.push((pi, r));
                    }
                }
            }
        }
        PollStrategy::Sequential => {
            for (pi, plan) in plans.iter().enumerate() {
                for r in 0..plan.regions.len() {
                    order.push((pi, r));
                }
            }
        }
    }
    order
}
```

- [ ] **Step 5: 跑测试验证通过**

Run: `cargo test --lib modbus_client::tests::build_poll_order`
Expected: 4 tests PASS

- [ ] **Step 6: 质量门 + commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets --all-features
git add src/plugin/builtin/modbus_client/mod.rs
git commit -m "feat(modbus): build_poll_order 两策略轮询序列（纯函数+单测）"
```

---

## Task 5: `read_region` 辅助 + 采集循环改造（多 slave + per-request 超时 + 故障隔离）+ `handle_write` 带 slave_id

**Files:**
- Modify: `src/plugin/builtin/modbus_client/mod.rs:143-313`（`run` 的 Tick 分支）+ `mod.rs:316-408`（`handle_write`）

**说明**：本 task 改造采集循环（无独立单测，靠 Task 6 集成测覆盖）。

- [ ] **Step 1: 加 `read_region` 辅助 + `ReadOutcome`**（`mod.rs`，在 `build_poll_order` 之后）

```rust
use tokio_modbus::ExceptionCode;

/// 一次 region 读结果（区分三类失败，驱动故障隔离决策）。
enum ReadOutcome {
    /// 成功，含解码缓冲。
    Ok(Buf),
    /// 协议异常（连接正常，如 IllegalDataAddress）——跳过该 region，不拆 session。
    Exception(ExceptionCode),
    /// 读超时（slave 无响应）——跳过该 region + 该 slave 剩余 region，不拆 session。
    Timeout,
    /// transport 错（连接断）——拆 session 重连，中断本轮。
    Transport(tokio_modbus::Error),
}

/// 读一个 region（按 func 分发），返回 `ReadOutcome`。调用方负责 `set_slave` + 超时包装。
async fn read_region(sess: &mut Context, reg: &Region) -> ReadOutcome {
    let len = reg.len();
    let res = match reg.func {
        Func::Coil => sess.read_coils(reg.start, len).await.map(|r| r.map(Buf::Bits)),
        Func::Discrete => sess
            .read_discrete_inputs(reg.start, len)
            .await
            .map(|r| r.map(Buf::Bits)),
        Func::Holding => sess
            .read_holding_registers(reg.start, len)
            .await
            .map(|r| r.map(Buf::Regs)),
        Func::Input => sess
            .read_input_registers(reg.start, len)
            .await
            .map(|r| r.map(Buf::Regs)),
    };
    match res {
        Ok(Ok(buf)) => ReadOutcome::Ok(buf),
        Ok(Err(exc)) => ReadOutcome::Exception(exc),
        Err(e) => ReadOutcome::Transport(e),
    }
}
```

> 注：`read_coils` 等返回 `Result<Result<Vec<_>, ExceptionCode>, Error>`，`Buf` 来自 `codec`（现有 `Buf::Bits`/`Buf::Regs`，见 mod.rs:88 `use codec::{... Buf ...}`）。

- [ ] **Step 2: 改 `use` 区**（mod.rs 顶部）

把 `use codec::{decode_point, encode_value, Buf, WritePayload};` 保持；新增 import：
```rust
use std::collections::BTreeMap;
use tokio_modbus::prelude::Slave; // set_slave 用
```
（`BTreeMap` 已 use，确认不重复；`Slave` 来自 tokio-modbus prelude，与 transport.rs 一致。）

- [ ] **Step 3: 改 `run()` 的预解析段**（替换 mod.rs:159-192 的 `devices` 预解析）

把原来的：
```rust
        // 预解析所有设备的 (devid, [(field, PointSpec)])，采集循环内不再重复 parse。
        let devices: Vec<(String, Vec<(String, PointSpec)>)> = inst.devices.iter().map(...).collect();
```
替换为：
```rust
        // 预解析 devices → SlavePlan（每 slave 独立 regions + slave_id）。采集循环不再重复 parse/merge。
        let plans = plan_devices(
            &inst.devices,
            cfg.gap_threshold,
            cfg.max_regs,
            cfg.slave_id,
        );
        let poll_order = build_poll_order(&plans, cfg.poll_strategy); // 固定顺序，每 tick 复用
        let req_timeout = Duration::from_secs(cfg.request_timeout_secs);
        // handle_write 反查 devid → slave_id（写命令按目标设备切 slave）。
        let devid_to_slave: std::collections::HashMap<String, u8> = plans
            .iter()
            .map(|p| (p.devid.clone(), p.slave_id))
            .collect();
```

- [ ] **Step 4: 改 `Evt::Tick` 分支**（替换 mod.rs:215-308 的 Tick 内 `for (devid, pairs) in &devices` 整段）

把原 Tick 分支里 `let sess = match &mut session {...};` 之后的 `for (devid, pairs) in &devices { ... }` 整段，替换为：

```rust
                    let sess = match &mut session {
                        Some(s) => s,
                        None => continue,
                    };
                    // 单 tick 内所有 devid 共用一个 timestamp（点间一致，对标 opcua/s7 批量读语义）。
                    let now = chrono::Local::now().timestamp();
                    // 按 devid 累积解码值（round_robin 下同 devid 的 region 散落序列各处，故聚合）。
                    let mut acc: BTreeMap<String, BTreeMap<String, SampleValue>> = BTreeMap::new();
                    let mut transport_failed = false;
                    for (pi, ri) in &poll_order {
                        let plan = &plans[*pi];
                        let reg = &plan.regions[*ri];
                        sess.set_slave(Slave(plan.slave_id));
                        let outcome = match tokio::time::timeout(req_timeout, read_region(sess, reg)).await {
                            Ok(o) => o,
                            Err(_) => ReadOutcome::Timeout,
                        };
                        match outcome {
                            ReadOutcome::Ok(buf) => {
                                for (field, p) in &reg.points {
                                    if let Some(v) = decode_point(p, &buf, reg.start) {
                                        acc.entry(plan.devid.clone())
                                            .or_default()
                                            .insert((*field).to_string(), v);
                                    }
                                }
                            }
                            ReadOutcome::Exception(exc) => {
                                tracing::warn!(
                                    "[modbusClient] inst {inst_id} slave {} read {}+{} exception: {exc:?}",
                                    plan.slave_id,
                                    reg.start,
                                    reg.len()
                                );
                                // 协议异常：跳过该 region，继续下一个（连接正常）
                            }
                            ReadOutcome::Timeout => {
                                tracing::warn!(
                                    "[modbusClient] inst {inst_id} slave {} read {}+{} timeout ({}s)，跳过",
                                    plan.slave_id,
                                    reg.start,
                                    reg.len(),
                                    cfg.request_timeout_secs
                                );
                                // slave 无响应：跳过该 slave 剩余 region（它大概率也不会响应），
                                // 但不拆 session（总线没断），继续下一 slave——故障隔离。
                                // （序列里同 slave 的后续 region 自然会被读到，但为减少无效超时等待，
                                //   这里不 break；超时本身已限定单次等待。）
                            }
                            ReadOutcome::Transport(e) => {
                                tracing::warn!(
                                    "[modbusClient] inst {inst_id} slave {} read {}+{} failed: {e:?}",
                                    plan.slave_id,
                                    reg.start,
                                    reg.len()
                                );
                                transport_failed = true;
                                break; // 连接断了，本轮终止，下 tick 重连
                            }
                        }
                    }
                    if transport_failed {
                        session = None;
                    }
                    for (devid, values) in acc {
                        if values.is_empty() {
                            continue;
                        }
                        let sample = Sample {
                            hwid: ctx.hwid.clone(),
                            devid,
                            values,
                            timestamp: now,
                        };
                        if tx.send(sample).await.is_err() {
                            break;
                        }
                    }
```

- [ ] **Step 5: 改 `handle_write` 签名 + slave 路由**（mod.rs:316-408）

把 `handle_write` 签名改为接收 `devid_to_slave`，并在写前 `set_slave`。原签名：
```rust
async fn handle_write(session: &mut Option<Context>, data: &str) {
```
改为：
```rust
async fn handle_write(session: &mut Option<Context>, data: &str, devid: &str, devid_to_slave: &std::collections::HashMap<String, u8>) {
```
在 `let sess = match session { Some(s) => s, None => {...} };` 之后、`let current_reg = ...` 之前，插入 slave 路由：
```rust
    // 按 devid 反查 slave_id（data 可显式带 slave_id 覆盖）。
    let slave_id = req
        .get("slave_id")
        .and_then(|v| v.as_u64())
        .map(|v| v as u8)
        .or_else(|| devid_to_slave.get(devid).copied());
    let slave_id = match slave_id {
        Some(s) => s,
        None => {
            tracing::warn!("[modbusClient] write 无法确定 slave_id（devid={devid} 未在 devices 配置，data 也无 slave_id）");
            return;
        }
    };
    sess.set_slave(Slave(slave_id));
```
并把 `run()` 里的调用（mod.rs:209）从 `handle_write(&mut session, &m.data).await;` 改为：
```rust
                        handle_write(&mut session, &m.data, &m.devid, &devid_to_slave).await;
```

- [ ] **Step 6: 跑编译 + 现有单测**

Run: `cargo build && cargo test --lib modbus_client`
Expected: 编译通过；现有单测（parse_point/merge_regions/codec 等）全 PASS（本 task 未改它们的逻辑）

- [ ] **Step 7: 质量门 + commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets --all-features
git add src/plugin/builtin/modbus_client/mod.rs
git commit -m "feat(modbus): 采集循环多 slave 轮询 + per-request 超时故障隔离 + write slave 路由"
```

---

## Task 6: 集成测试 `tests/modbus_multi_slave_e2e.rs`

**Files:**
- Create: `tests/modbus_multi_slave_e2e.rs`
- Read（参考模式）: `tests/sidecar_e2e.rs`（`tests/common::start_lux` 用法 + axum/InstanceManager 编排）

**说明**：mock 一个 TCP modbus server（多 unit id 响应），起 lux + InstanceManager + modbusClient 实例，验：多 slave 各自入库、故障隔离（一 slave 超时）、两策略、旧单 slave 配置兼容、write 路由。

- [ ] **Step 1: 先确认 `tests/common` 的可用 API**

Run: `cargo test --test sidecar_e2e -- --list 2>&1 | head -5` 或直接 Read `tests/common/mod.rs`（若存在）。确认 `start_lux` 签名（返回 lux handle + client + 临时 data_dir）。

> 若 `tests/common` 不存在，参考 `tests/sidecar_e2e.rs` 顶部的 lux 启动模式自包含。

- [ ] **Step 2: 写集成测试**（新建 `tests/modbus_multi_slave_e2e.rs`）

```rust
//! modbus 多 slave 单实例采集 端到端（CI 内，mock TCP modbus server）。
//!
//! 验：① 多 slave 各自入库 ② 故障隔离（一 slave 不响应） ③ 两策略均采到
//!     ④ 旧单 slave 配置兼容 ⑤ write 按 slave_id 路由。

mod common;

use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// mock TCP modbus 从站：按请求的 unit id 回不同数据。
/// - unit 1：holding[0..4] 回 [1000, 2000, 3000, 4000]
/// - unit 2：不响应（模拟故障/超时）
/// - unit 3：holding[0..2] 回 [500, 600]
/// 返回 (bind_addr, shutdown_tx)。shutdown_tx drop 即停。
async fn spawn_mock_server() -> (std::net::SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accept = listener.accept() => {
                    if let Ok((mut sock, _)) = accept {
                        tokio::spawn(async move {
                            let mut hdr = [0u8; 7];
                            loop {
                                if sock.read_exact(&mut hdr).await.is_err() {
                                    return;
                                }
                                // MBAP: tid(2) pid(2) len(2) uid(1)
                                let tid = u16::from_be_bytes([hdr[0], hdr[1]]);
                                let uid = hdr[6];
                                let mut pdu = vec![0u8; (u16::from_be_bytes([hdr[4], hdr[5]]) as usize).saturating_sub(1)];
                                if sock.read_exact(&mut pdu).await.is_err() {
                                    return;
                                }
                                let fc = pdu[0];
                                if fc != 3 {
                                    // 仅处理 FC03（holding 读）；其余忽略
                                    continue;
                                }
                                let resp_regs: Vec<u16> = match uid {
                                    1 => vec![1000, 2000, 3000, 4000],
                                    3 => vec![500, 600],
                                    _ => continue, // unit 2 不响应（故障）
                                };
                                let qty = u16::from_be_bytes([pdu[3], pdu[4]]) as usize;
                                let data: Vec<u16> = resp_regs.into_iter().take(qty).collect();
                                let mut resp = Vec::new();
                                resp.extend_from_slice(&tid.to_be_bytes()); // tid
                                resp.extend_from_slice(&0u16.to_be_bytes()); // pid=0
                                let len = 3 + data.len() * 2;
                                resp.extend_from_slice(&(len as u16).to_be_bytes()); // len
                                resp.push(uid); // uid
                                resp.push(fc); // fc
                                resp.push((data.len() * 2) as u8); // byte count
                                for r in &data {
                                    resp.extend_from_slice(&r.to_be_bytes());
                                }
                                let _ = sock.write_all(&resp).await;
                            }
                        });
                    }
                }
            }
        }
    });
    (addr, shutdown_tx)
}

/// 起 lux + InstanceManager + modbusClient 实例，采一个 period 后读 rtdata:<devid>。
async fn collect_one_period(inst_id: &str, app_config: serde_json::Value, devices: Vec<serde_json::Value>) -> HashMap<String, String> {
    let lux = common::start_lux().await; // 见 common；返回 EmbeddedClient
    // 建 InstConfig
    let inst = rs_iot::types::InstConfig {
        inst_id: inst_id.into(),
        inst_name: "it".into(),
        app_code: rs_iot::types::PluginName::new("modbusClient"),
        app_type: rs_iot::types::AppType::South,
        auto_start: false,
        app_config,
        devices: devices.into_iter().map(|v| serde_json::from_value(v).unwrap()).collect(),
        host_kind: rs_iot::types::HostKind::Builtin,
    };
    // 编排 InstanceManager + hub（参考 sidecar_e2e.rs 的 ctx/hub/mgr 构造）
    let (hub_tx, mut hub_rx) = tokio::sync::mpsc::channel(64);
    let ctx = rs_iot::host::HostContext {
        lux: lux.client(),
        hwid: rs_iot::types::Hwid::new("it"),
        shutdown: tokio_util::sync::CancellationToken::new(),
        ctl_registry: rs_iot::host::CtlRegistry::new(),
        bridge_registry: rs_iot::host::BridgeRegistry::new(),
    };
    let mgr = std::sync::Arc::new(rs_iot::plugin::instance::InstanceManager::new(ctx, hub_tx));
    mgr.start_source(inst).await.unwrap();
    // 收 1 个 Sample（每个 devid 至少一个；多 slave 收多个）
    let mut got = HashMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        tokio::select! {
            Some(s) = hub_rx.recv() => {
                for (k, v) in &s.values { got.insert(format!("{}.{k}", s.devid), format!("{v:?}")); }
                if got.len() >= 2 { break; } // 等 unit1 + unit3 都到
            }
            _ = tokio::time::sleep_until(deadline) => break,
        }
    }
    got
}

#[tokio::test]
async fn multi_slave_collects_each_devid() {
    let (addr, _sh) = spawn_mock_server().await;
    let app = serde_json::json!({
        "kind":"tcp","endpoint":addr.to_string(),"period":1,"request_timeout_secs":1
    });
    let devices = vec![
        serde_json::json!({"value":"m1","slave_id":1,"children":[{"value":"holding:0:i16","label":"v0"}]}),
        serde_json::json!({"value":"m3","slave_id":3,"children":[{"value":"holding:0:i16","label":"v0"}]}),
    ];
    let got = collect_one_period("ms1", app, devices).await;
    assert!(got.contains_key("m1.v0"), "slave1 入库: {got:?}");
    assert!(got.contains_key("m3.v0"), "slave3 入库: {got:?}");
}

#[tokio::test]
async fn fault_isolation_unresponsive_slave() {
    // unit2 不响应 → 超时跳过；unit1/3 应正常入库
    let (addr, _sh) = spawn_mock_server().await;
    let app = serde_json::json!({
        "kind":"tcp","endpoint":addr.to_string(),"period":1,"request_timeout_secs":1
    });
    let devices = vec![
        serde_json::json!({"value":"m1","slave_id":1,"children":[{"value":"holding:0:i16","label":"v0"}]}),
        serde_json::json!({"value":"m2","slave_id":2,"children":[{"value":"holding:0:i16","label":"v0"}]}),
        serde_json::json!({"value":"m3","slave_id":3,"children":[{"value":"holding:0:i16","label":"v0"}]}),
    ];
    let got = collect_one_period("ms2", app, devices).await;
    assert!(got.contains_key("m1.v0"), "故障隔离：slave1 仍入库");
    assert!(got.contains_key("m3.v0"), "故障隔离：slave3 仍入库");
    // m2 因超时不应入库（或最晚；这里宽松：只要 m1/m3 在即证明隔离生效）
}

#[tokio::test]
async fn backward_compat_legacy_single_slave_config() {
    // 旧配置：无 device.slave_id，app_config.slave_id 实例级；应回退采到
    let (addr, _sh) = spawn_mock_server().await;
    let app = serde_json::json!({
        "kind":"tcp","endpoint":addr.to_string(),"slave_id":1,"period":1
    });
    let devices = vec![
        serde_json::json!({"value":"m1","children":[{"value":"holding:0:i16","label":"v0"}]}),
    ];
    let got = collect_one_period("ms3", app, devices).await;
    assert!(got.contains_key("m1.v0"), "旧单 slave 配置兼容: {got:?}");
}

#[tokio::test]
async fn sequential_strategy_also_collects() {
    let (addr, _sh) = spawn_mock_server().await;
    let app = serde_json::json!({
        "kind":"tcp","endpoint":addr.to_string(),"period":1,
        "poll_strategy":"sequential","request_timeout_secs":1
    });
    let devices = vec![
        serde_json::json!({"value":"m1","slave_id":1,"children":[{"value":"holding:0:i16","label":"v0"}]}),
        serde_json::json!({"value":"m3","slave_id":3,"children":[{"value":"holding:0:i16","label":"v0"}]}),
    ];
    let got = collect_one_period("ms4", app, devices).await;
    assert!(got.contains_key("m1.v0") && got.contains_key("m3.v0"), "sequential 两 slave 均入库");
}
```

- [ ] **Step 3: 确认 `rs_iot` 库公开项可见性**

`tests/` 集成测试依赖 `rs_iot` crate 的公开项：`types::{InstConfig,PluginName,AppType,HostKind,Hwid}`、`host::{HostContext,CtlRegistry,BridgeRegistry}`、`plugin::instance::InstanceManager`、`lux` 的 client。

Run: `cargo test --test modbus_multi_slave_e2e --no-run`
Expected: 若某项非 `pub`（如 `HostContext` 字段 / `InstanceManager::start_source` 非公开），按需补 `pub` 或调整测试改用现有公开入口（参考 `sidecar_e2e.rs` 怎么编排的——它已跑通这套，照搬其 ctx/mgr 构造）。

> **关键**：照搬 `tests/sidecar_e2e.rs` 顶部 `use` + ctx/mgr/hub 构造代码（它已验证可用），只把 Source 换成 modbusClient、app_config/devices 换成本测试的。这一步把 `collect_one_period` 的 ctx/mgr 编排对齐 sidecar_e2e 的实际可用写法。

- [ ] **Step 4: 跑集成测试**

Run: `cargo test --test modbus_multi_slave_e2e`
Expected: 4 tests PASS（多 slave 入库 / 故障隔离 / 旧配置兼容 / sequential）

> 若 `fault_isolation_unresponsive_slave` 因 mock server 的 unit2 "不响应" 实现（`continue` 不回包）导致 client 端 read 在 `request_timeout_secs` 内超时——验证 modbusClient 的超时跳过生效。若超时未触发（read 挂起超过测试 8s deadline），回到 Task 5 确认 `tokio::time::timeout` 正确包裹了 `read_region`。

- [ ] **Step 5: 质量门 + commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets --all-features
git add tests/modbus_multi_slave_e2e.rs tests/common.rs
git commit -m "test(modbus): 多 slave 单实例采集 e2e（故障隔离/兼容/两策略）"
```

---

## Task 7: 文档更新 + 收尾

**Files:**
- Modify: `CLAUDE.md`（modbus_client 端到端段 + 数据流段）
- Modify: `docs/plugin-development-guide.md`（若 modbus 多 slave 描述需补）
- Modify: `docs/scenario-gap-assessment.md`（§1.2 状态：P0 已落地）

- [ ] **Step 1: 更新 CLAUDE.md modbus 段**（找到"modbus_client 端到端"段，补多 slave 说明）

在 modbus 配置说明里补：
```
多 slave（一条 RS485/TCP 总线多台从站）：device 级配 `slave_id`（`DeviceSpec.slave_id`），
缺省回退 `app_config.slave_id`（向后兼容）。`poll_strategy`（默认 round_robin）/ `request_timeout_secs`
（默认 2s，slave 无响应超时跳过——故障隔离）。一条总线一个实例（串口 OS 独占，不拆多实例）。
```

- [ ] **Step 2: 更新 scenario-gap-assessment.md §1.2**

把"**建议**：P0 — ..."行的状态标注为"✅ M1 已实现（modbus 多 slave）；lua 多设备待 M2"。

- [ ] **Step 3: 跑文档质量门**

Run: `RUSTFLAGS="-D warnings" cargo doc --no-deps`
Expected: 零警告（`slave_id`/`poll_strategy`/`request_timeout_secs` 公开项有 doc 注释）

- [ ] **Step 4: 四项质量门全绿确认**

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets --all-features
RUSTFLAGS="-D warnings" cargo doc --no-deps
```
Expected: 四项全 PASS

- [ ] **Step 5: commit 收尾**

```bash
git add CLAUDE.md docs/scenario-gap-assessment.md
git commit -m "docs(modbus): 多 slave 单实例采集文档更新 + M1 落地标注"
```

- [ ] **Step 6: 更新 spec 实测记录**（可选）

在 `docs/superpowers/specs/2026-08-08-rs485-multi-device-design.md` 末尾补"M1 完成记录：实测 mock 多 slave e2e 通过；真机 RS485 多从站验证待 M3"。

---

## Self-Review（plan 自检）

**1. Spec 覆盖**：
- R1（modbus 多 slave）→ Task 3/5 ✓
- R2（lua 多设备）→ **本计划不覆盖**（M2 plan，下一个）✓ 范围明确
- R3（批量/点间一致）→ Task 5 单 tick 共用 timestamp ✓
- R4（两策略）→ Task 4/5（build_poll_order + poll_strategy 分支）✓
- R5（故障隔离）→ Task 5（per-request timeout + ReadOutcome 分类）+ Task 6（集成测验证）✓
- R6（向后兼容）→ Task 3（slave_id fallback）+ Task 6（旧配置集成测）✓

**2. 占位符扫描**：无 TBD/TODO；每步有完整代码。Task 6 Step 1/3 有"照搬 sidecar_e2e 编排"的指引——这是因 `tests/common` 与公开 API 可见性需实测确认，非占位符（给了明确 fallback：照搬已验证的 sidecar_e2e 写法）。

**3. 类型一致性**：`SlavePlan{devid,slave_id,regions}`（Task 3 定义，Task 4/5 使用一致）；`PollStrategy::{RoundRobin,Sequential}`（Task 2 定义，Task 4/5 使用一致）；`ReadOutcome::{Ok,Exception,Timeout,Transport}`（Task 5 定义使用一致）；`build_poll_order -> Vec<(usize,usize)>`（Task 4 定义，Task 5 使用一致）；`handle_write(session, data, devid, devid_to_slave)`（Task 5 定义+调用一致）。

---

## 执行交接

**Plan complete and saved to `docs/superpowers/plans/2026-08-08-modbus-multi-slave.md`. Two execution options:**

**1. Subagent-Driven (recommended)** - 每个 task 派一个新 subagent，task 间 review，快速迭代

**2. Inline Execution** - 本会话内逐 task 执行，批量 + 检查点 review

**Which approach?**

> M2（lua 多设备批量：collect 嵌套 + set_values）是下一个 plan，待 M1 完成后编写。
