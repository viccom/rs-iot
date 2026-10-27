# Release Note：Lua 脚本系统 · 阶段 C（数据加工）

> 日期：2026-08-21
> **状态**：已发布（worktree `feat/lua-stage-c`，`13875a6..` 9 task，已合并 master）
> 范围：演进设计 v0.6 阶段 C 全量——luaTransform（transform 角色，Source 订阅型，Q19 三薄插件之三）/ EventBus Rtdata 载荷带值（Q6）/ subscribe 宿主过滤 + 自环防护（Q6 硬性验收）/ on_timer 周期回调 / rhai 冻结标注（Q7）
> 读者：网关运维 + 数据加工脚本作者。升级动作集中在「升级必读」，其余为了解性内容。

## 升级必读（替换二进制即生效）

1. **新配置键 `event_bus_capacity`**（顶层，默认 256 不变）：EventBus broadcast 容量
   原为硬编码 256，现可配——SSE 消费者多/transform 订阅实例多且出现 lagged 告警时调大
   （`/metrics` 的 `rs_iot_sse_lagged_total` 持续增长是信号）。不配置零行为变化。
2. **rtdata SSE 事件载荷新增 `samples` 字段**：`{dev_ids, count, samples, ts}`——
   `samples: [{devid, values: {tag: 值}, ts}]`（按 dev 聚合 + 截断 50 dev）。前端
   monitor 收到带 `samples` 的事件**直接合并进实时表即时刷新**，不再等 2s 轮询补拉；
   `dev_ids`/`count` 旧字段原样保留（旧消费方零破坏）。前端随本版已同步，自研消费方
   可选直用 `samples`（不识别也无影响）。
3. **`app_type:"South"` + `app_code:"luaTransform"` 实例可用**：transform 角色订阅型
   Source——`@role transform` 脚本（on_timer/on_rtdata 至少其一），配置见下。前端
   新建实例下拉与表单已支持（订阅模式逗号分隔输入）。
4. **rhai 已冻结（deprecated）**：`transform` feature（rhai 脚本数据转换）标冻结——
   已实现未接线（恒 Passthrough）、不再投入，数据加工统一走 luaTransform。**不删代码
   不摘 feature**（裁剪构建零变化）；Lua transform 实战半年后复审移除或接线（Q7/D4）。
5. **无破坏性变更**：collector（luaClient）/ north（luaNorth）行为零变化；pipeline
   仅 `publish_rtdata_notify` 聚合带值（dispatcher/worker/背压链零触碰，验收 diff 留证）。

## 新能力

- **luaTransform（transform 角色）**：Lua 脚本化数据加工——订阅 EventBus 实时数据
  （`subscribe: ["dev1", "dev2.temp*"]` 宿主侧模式过滤，空 = 纯定时）→
  `on_rtdata(devid, tag, value)` 逐点回调（每次独立 `callback_timeout_ms` 预算，单点
  超限跳过不弃批）；`on_timer()` 周期回调（`period=0` 纯订阅）；输出经 `ctx:set_value`
  写派生点（与 collector 同出口：hub → pipeline → lux，前端/北向零差别消费）。
  能力包 C0/C2/C4/C6（json/数据/kv+secret/工具）——C1 通讯/C3 控制/C5 HTTP 不注册。
  走 InstanceManager 全套（生命周期/stat/热加载/VM 级 reload 失败回退）。
- **自环防护（Q6 硬性验收）**：实例自身 `set_value` 的产出回流 EventBus 不触发自身
  `on_rtdata`（api 层 per-instance self-write 记录 + 10s 窗口过滤）——脚本订阅自身
  输出 tag 不会回调放大。跨实例单跳传播不受影响（e2e 实测）。
- **EventBus Rtdata 载荷带值（Q6）**：pipeline 攒批发布时按 dev 聚合最新值
  （同批同 dev 后写覆盖，截断 50 dev）——SSE/前端即时刷新不再依赖轮询兜底拉取
  （轮询保留作 lagged 丢帧兜底）。
- **event_bus 容量可配 + SSE lag 可观测（R6）**：`event_bus_capacity` 配置键 +
  `/metrics` 新增 `rs_iot_sse_lagged_total`（SSE 消费者 lagged 累计，此前静默丢弃）。
- **清洗/聚合示例上线**：`examples/lua-scripts/agg-filter/`——订阅原始点 → 野点
  剔除（限幅）→ 滑动窗口均值 → 派生点；窗口状态 kv 持久、窗口大小 kv 可配
  （`cfg_window`）。示例文件本体经真实链路 e2e 锁定（防示例与实现漂移）。

## luaTransform 配置示例

```json
{
  "inst_id": "aggfilter1",
  "app_code": "luaTransform",
  "app_type": "South",
  "app_config": {
    "script": "agg-filter",
    "period": 0,
    "subscribe": ["raw1.temp*"]
  },
  "devices": [{ "label": "派生设备", "value": "agg1", "children": [] }]
}
```

- `subscribe`：数据面入口（`"dev1"` 全 tag / `"dev2.temp*"` tag 前缀 / `"*"` 全设备；
  非法 pattern 启动拒载；**devid 不含 `.`**——模式以 `.` 分段）；空 = 纯定时。
- `period`：on_timer 周期秒（默认 3；0 = 纯订阅）。
- `callback_timeout_ms` 默认 10000、`mem_limit_mb` 32、`hook_instr_limit` 1000000
  （与 collector 对齐）；`scripts_root` 缺省 `scripts`。
- 自环取舍与跨实例环配置责任见指南「transform 角色」章（跨实例环靠配置保证 DAG，
  异常频率告警依赖 D 阶段 Q12 告警模型）。

## 已知限制（上线前关注）

- **自环防护窗口的跨源静默取舍**：10s 窗口内**其他源**对同 `(devid,tag)` 的并发写入
  一并静默——per-instance 表只保证「自身输出不触发自身」，高频同 tag 多源写场景需知晓。
- **reload 不重评估订阅模式**：`subscribe` 是实例配置级，热加载只换 VM——改订阅
  须 stop/start 实例。
- **EventBus fan-out 成本随订阅实例数线性增长**：每个订阅实例一个 broadcast 接收端；
  实例数大时关注 `event_bus_capacity` 与 lagged 计数。
- rhai 冻结为文档标注（`#[deprecated]` 会响编译警告，不取）——代码保留，feature 裁剪
  组合零变化。

## 配置新增汇总

| 键 | 层 | 默认 | 说明 |
|---|---|---|---|
| `event_bus_capacity` | 顶层（TOML） | 256 | EventBus broadcast 容量（原硬编码，现可配） |
| `script` / `period` / `subscribe` | transform app_config | 必填 / 3 / [] | luaTransform 主键集（见上） |
| `callback_timeout_ms` / `scripts_root` / `mem_limit_mb` / `hook_instr_limit` | transform app_config | 10000 / scripts / 32 / 1000000 | 预算与沙箱参数 |
| 指标 | — | — | `/metrics` 新增 `rs_iot_sse_lagged_total` |

> 工程侧：阶段 C 全程 worktree `feat/lua-stage-c` 隔离执行 + task 间审查，验收清单
> 逐项实测记录于
> [阶段 C 计划](../../designs/2026-08-20-lua-script-system-stage-c-plan.md)文末
> （含 pipeline 零改动 diff 留证）。
