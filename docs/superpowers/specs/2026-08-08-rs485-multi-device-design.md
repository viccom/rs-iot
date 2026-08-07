# RS485 总线多设备/多从站采集 设计（spec）

> **状态**：设计完成，待 review → 转 plan/实现
> **日期**：2026-08-08
> **来源**：现场场景评估（[`docs/scenario-gap-assessment.md`](../../scenario-gap-assessment.md) §1.2）+ 设计澄清
> **关联代码**：`src/types.rs`、`src/plugin/builtin/modbus_client/`、`src/plugin/builtin/lua_client/`
> **优先级**：P0（RS485 现场"一条总线多从站"能否落地的关键）

## 0. 概述

### 0.1 背景

现场典型：100 台仪表，RS485 为主，一条 RS485 总线上挂多台**同协议、同串口参数、不同从站地址（unit id）**的设备。rs-iot 的实例模型是"**一条通讯链路 = 一个实例**"（链路宿主封装、脚本/插件管报文），这是正确的设计——但当前：

- `luaClient` 单实例是**单设备输出语义**（`set_value`/`collect` 硬绑 `devices[0]`，`api/mod.rs:164-168`）
- `modbusClient` 单实例是**单 `slave_id`**（实例级，CLAUDE.md"每 slave 建一实例"）

而一条 RS485 总线由一个实例独占（串口 OS 独占，拆多实例必冲突），所以"一条总线多从站"当前**无法采集**——这是 RS485 现场的硬阻塞。

### 0.2 目标

让一个实例能采**一条链路上的多台从站**，对齐 `opcuaClient`/`s7Client` 已有的"单实例多设备多点位"能力：
- `modbusClient`：单实例多 slave 轮询
- `luaClient`：单实例多设备批量输出

### 0.3 非目标（明确不做）

- ❌ 一个实例同时打开**多个物理接口**（多串口）
- ❌ 一个实例同时建立**多个 TCP/UDP 连接**
- ❌ "串口共享层"（多实例复用一个物理串口）——设计是一条链路一实例，不需要

不同协议 / 不同串口参数 / 不同物理接口 → 仍走**不同实例**（不同脚本或同脚本多实例）。

---

## 1. 需求规格（Requirements）

### 1.1 需求清单

| ID | 需求 | 说明 |
|---|---|---|
| **R1** | modbus 单实例采一条 RS485 总线多 slave | 同串口参数、不同 unit id、点表同异随意 |
| **R2** | lua 单实例采一条链路多设备 | 输出各自 devid（RS485 多从站） |
| **R3** | 批量采集 | 一设备多点**一次上报**，保持 `Sample.values` 多变量语义 + 点间时间一致（同一采集时刻 timestamp） |
| **R4** | 两种采集策略 | 轮转 round-robin（默认）/ 逐设备 sequential，可配 |
| **R5** | 故障隔离 | 单 slave 无响应/超时**不拖垮整轮**，其余 slave 正常采集 |
| **R6** | 向后兼容 | 旧 modbus 单 slave 配置、老 lua 脚本（collect 平铺/set_value 旧形态）仍工作 |

### 1.2 约束

- **串口 OS 独占**：一条物理串口一次只能被一个实例 open
- **RS485 半双工**：物理上"一次一对请求-响应"，多 slave 是**时分串行轮询**，不能并发
- **同总线参数一致**：一条 RS485 总线上所有 slave 的波特率/数据位/校验/停止位必须相同（故串口参数实例级、slave_id 设备级）
- **`Sample` 语义不变**：`{hwid, devid, values:BTreeMap<tag,value>, timestamp}`——一设备一时刻多变量

### 1.3 验收标准

1. modbus：一个实例配 `devices:[{slave_id:1,...},{slave_id:2,...}]`，按 `poll_strategy` 轮询，每 slave 数据入 `rtdata:<devid>`
2. lua：`collect` 嵌套返回 `{[devid]={tag=v}}` / `ctx:set_values(devid,{tag=v})` 批量上报，多 devid 各自 Sample 入库
3. 故障隔离：mock 一个 slave 持续超时，其余 slave 采集周期与成功率不受影响
4. 兼容：旧 `app_config.slave_id`（单值）配置仍工作；老 lua collect 平铺返回仍工作
5. 四项质量门全绿（fmt/clippy `-D warnings`/test/doc）

---

## 2. 功能设计（Functional Design）

### 2.1 实例模型（不变，仅确认）

| 项 | 规则 |
|---|---|
| 一条通讯链路 | = 一个实例（一个串口 或 一个 TCP/UDP 连接） |
| 一个 lua 实例 | 只持一个 `io_engine` backend，**绝不开多接口/多连接** |
| "多设备" | 仅指**同一条链路上的多台从站**（RS485 总线：同协议、同参数、不同地址） |
| 不同协议/参数/接口 | → 不同实例 |

### 2.2 modbus 单实例多 slave

**配置模型**（`slave_id` 下沉到设备级，串口参数留实例级）：

```jsonc
{
  "app_code": "modbusClient",
  "app_config": {
    "kind": "rtu_serial",
    "serial_port": "COM3", "baud_rate": 9600, "data_bits": 8, "parity": "none", "stop_bits": 1,
    "poll_strategy": "round_robin",      // 默认；可 "sequential"
    "gap_threshold": 10, "max_regs": 100, "period": 2, "reconnect_secs": 2
  },
  "devices": [
    { "value": "meter1", "slave_id": 1, "children": [{"value":"holding:0:f32:ABCD","type":"float"}] },
    { "value": "meter2", "slave_id": 2, "children": [{"value":"holding:0:f32:ABCD","type":"float"}] }
  ]
}
```

**两种轮询策略**（半双工串行，区别在顺序）：

| 策略 | 顺序 | 强项 |
|---|---|---|
| `round_robin`（默认） | 每轮对每个 slave 各发 1 条报文（各取点表下一合并区间），多轮采完 | 全局刷新均匀；单 slave 故障天然隔离 |
| `sequential` | 一个 slave 所有报文采完再换下一个 | 单设备多点时间一致（V/A/P 同时刻） |

**故障隔离**：每次请求独立超时；超时/exception → `warn` 跳过该请求，继续下一个（不重连整总线，因总线本身没断，只是某 slave 没响应）。

**写命令**：`handle_write` 的 `data` JSON 加 `slave_id`（或按 `devid` 反查 device 的 slave_id），`set_slave` 后写。

### 2.3 lua 单实例多设备 API

**核心：批量导向**（贴合 `Sample` 多变量语义，一设备一 Sample 含多 tag、同时刻 timestamp）。

| API | 形态 | 场景 |
|---|---|---|
| `collect` 嵌套返回（主力） | `return {[devid]={tag1=v1, tag2=v2, ...}, ...}` | 周期一问一答（声明式批量） |
| `ctx:set_values(devid, {tag=v,...})` | 批量写一个设备 | `on_data` 被动帧解析多点 |
| `ctx:set_value(devid, tag, v)` | 单点（便捷） | `on_data` 偶发单点 |

**collect 嵌套 vs 平铺自动判定**：`SampleValue` 只含标量（Float/Int/Bool/Str），故返回 table 的"第二层 value 是 table → 多设备嵌套；是标量 → 单设备平铺（归 `devices[0]`，老脚本兼容）"，零歧义。

**链路不变**：`io_engine` 仍单 backend 独占链路；脚本在 `collect` 内循环 `ctx:request`（组不同 uid 的 RTU 帧）轮询多从站，宿主收发。

**示例**：
```lua
function collect()
  local out = {}
  for _, dev in ipairs(ctx:list_devices()) do
    local resp = ctx:request(build_req(dev.slave_id, ...), 200)
    out[dev.value] = parse(resp)   -- dev.value = devid
  end
  return out   -- {[meter1]={100点}, [meter2]={80点}} → 2 个 Sample
end
```

### 2.4 数据流

```
collect {嵌套} / set_values ──► hub（每 devid 一个 Sample，多 tag 同时刻）──► pipeline ──► rtdata:<devid>
modbus 多 slave 轮询 ──► 每 slave 一个 Sample（多 tag）──► hub ──► pipeline ──► rtdata:<devid>
```

---

## 3. 编码设计（Implementation Design）

### 3.1 数据结构变更

**`src/types.rs`**：
- `DeviceSpec` 加 `#[serde(default)] pub slave_id: Option<u8>`（modbus 用；opcua/s7 等忽略，serde 缺省 `None` 对它们透明）

**`src/plugin/builtin/modbus_client/config.rs`**：
- `ModbusCfg` 加 `pub poll_strategy: PollStrategy`（默认 `RoundRobin`）
- 新增 `enum PollStrategy { RoundRobin, Sequential }`（serde rename lowercase）
- `slave_id` 字段保留（实例级，向后兼容——见 3.4）

**`src/plugin/builtin/lua_client/config.rs`**：
- 无结构变更（`devices` 已是多设备列表，只是当前只用 `[0]`）

### 3.2 modbus 多 slave 轮询（`modbus_client/mod.rs`）

**预解析**（run 入口）：`devices` → `Vec<SlavePlan { devid, slave_id, regions: Vec<Region> }>`（每个 device 的 children 经 `merge_regions` 合并）。

**采集循环**（每 tick，按 `poll_strategy`）：

```text
RoundRobin（默认）:
  维护每 slave 的"下一区间"游标
  loop until 一个 period 的预算用完或所有区间轮完一遍:
    for slave in slaves:
      取 slave 当前游标的下一个 region
      ctx.set_slave(slave_id); 读该 region; 解码; 组 Sample(devid, 多 tag) 推 hub
      游标前进；region 超时/exception → warn 跳过
    （一轮 = 每 slave 各读一个 region；多轮采完所有 region）

Sequential:
  for slave in slaves:
    for region in slave.regions:
      ctx.set_slave(slave_id); 读; 解码; 组 Sample 推 hub
      超时/exception → warn 跳过，break 到下一 slave（该 slave 剩余 region 下 tick 再试）
```

**关键点**：
- `ctx.set_slave(Slave(slave_id))` 每次请求前切（RTU `attach_slave` 后仍可 `set_slave` 切换）
- **per-request 超时跳过**：单 region 失败不重连整 Context（总线没断），只跳过该 region/slave
- **period 预算**：一个 period 轮不完所有 region → 跳剩余（`MissedTickBehavior::Skip`），下个 period 继续（可能数据刷新变慢，但不堆积）
- 写命令 `handle_write`：`data` JSON 可带 `slave_id`，或按 `devid` 反查 `SlavePlan` 得 slave_id

### 3.3 lua 多设备（`lua_client/api/mod.rs` + `mod.rs`）

**`api/mod.rs`**：
- `set_value` 改 3 参：`(_self, devid, tag, v)` —— devid 参数化（不再固定 `devices[0]`）
- 新增 `set_values`：`(_self, devid, table)` —— 一次写一个设备多点 → 组一个 `Sample{devid, values:BTreeMap, timestamp}` 推 hub
- 单点 `set_value(devid, tag, v)` 保留（便捷；等价 `set_values(devid, {[tag]=v})`）

**`mod.rs::call_collect`**（返回值解析扩展）：
```text
collect 返回 table T:
  若 T 的 value 是 table（嵌套）→ 多设备：
    for devid, points in T: 组 Sample{devid, values=points, 同一 timestamp} 推 hub
  若 T 的 value 是标量（平铺）→ 单设备兼容：
    组 Sample{devices[0].value, values=T, timestamp} 推 hub（老脚本行为不变）
```

**timestamp 一致性**：一次 `collect` 调用取一个 `now`，该次所有 Sample 共用（保证点间同时刻）。

### 3.4 向后兼容

- **modbus 旧 `app_config.slave_id`（单值）**：若 `devices[].slave_id` 全缺省，则所有 device 用 `app_config.slave_id`（= 旧单 slave 行为，多 device 共享一个 slave——通常旧配置就一个 device）。若 device 显式带 `slave_id`，用 device 级。两者可混用。
- **lua collect 平铺返回**：归 `devices[0]`（3.3 已设计）
- **lua `set_value` 旧 2 参签名**：**不兼容**（rs-iot 未正式发布，无线上脚本包袱）。改 3 参，示例脚本（`modbus-like`/`custom-on-data`）一并更新。

### 3.5 测试计划

**单元测试**（`#[cfg(test)]` 内）：
- modbus: `parse_cfg` 多 slave 配置（device.slave_id + 旧 app_config.slave_id 混用）、`PollStrategy` 解析、`merge_regions` 每 slave 独立合并
- lua: `call_collect` 嵌套 vs 平铺判定、`set_values` 组 Sample、`set_value` 3 参

**集成测试**（`tests/`）：
- `modbus_multi_slave_e2e`：mock modbus server（多 unit id），建多 slave 实例 → 验 `rtdata:<devid>` 各自入库；mock 一 slave 超时 → 验其余 slave 不受影响（故障隔离）
- `lua_multi_device_e2e`：mock RS485 多从站（或 mock IO），lua collect 嵌套返回 → 验多 devid Sample 入库；`set_values` 批量 → 验一个 Sample 多 tag

**真机验证**（CI 不跑，手动）：
- RS485 总线挂 2+ 台 modbus 从站（不同 unit id）→ 两种策略各跑 → `redis-cli HGETALL rtdata:<devid>` 验
- lua 脚本一条串口轮询多从站

### 3.6 风险与对策

| 风险 | 对策 |
|---|---|
| `DeviceSpec.slave_id` 污染通用类型 | `Option<u8>` + `#[serde(default)]`，opcua/s7 忽略；文档注明"modbus 专用" |
| 多 slave 多点一个 period 轮不完 | `MissedTickBehavior::Skip` + 日志告警周期内未轮完；`stat` 暴露轮询完成率（对接埋点 P0） |
| 半双工串行性能上限 | 总线物理限制，无法并发；文档说明"采集周期需 ≥ 总线轮询时间" |
| lua collect 嵌套判定歧义 | `SampleValue` 不含 table（已论证），无歧义；单测覆盖 |
| `set_slave` 切换在 RTU over TCP/UDP 的行为 | tokio-modbus `SlaveContext::set_slave` 对三种 RTU 传输均生效（已用）；单测/真机验证 |

---

## 4. 实施顺序（建议里程碑）

- **M1 modbus 多 slave**：config（slave_id 下沉 + poll_strategy）→ 轮询（两策略 + per-request 超时跳过）→ 兼容 → 单测 + 集成（mock 多 slave + 故障隔离）
- **M2 lua 多设备 API**：`set_values` + `set_value` 3 参 → `call_collect` 嵌套解析 → 示例脚本更新 → 单测 + 集成（mock 多从站）
- **M3 真机验证**：RS485 多从站（modbus + lua 各一场景）

每里程碑走 TDD（测试先行）+ 四项质量门。

---

## 5. 参考

- 场景缺口评估：[`docs/scenario-gap-assessment.md`](../../scenario-gap-assessment.md) §1.2
- 内置插件指南：[`docs/plugin-development-guide.md`](../../plugin-development-guide.md)
- lua 脚本指南：[`docs/lua-script-guide.md`](../../lua-script-guide.md)
- modbus 端到端：`CLAUDE.md`「验证规则 - modbus_client 端到端」
- 类型定义：`src/types.rs`（`DeviceSpec`/`ChildSpec`/`Sample`/`SampleValue`）

---

*本 spec 随实现推进更新；M1/M2 完成后补实测数据与偏差记录。*
