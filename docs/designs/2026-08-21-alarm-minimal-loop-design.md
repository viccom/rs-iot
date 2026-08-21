# 告警最小闭环设计（Q12 独立 spec · D 阶段前置）

> 日期：2026-08-21
> **状态**：decided（Q12 于演进设计 v0.5 定案，本文为实施级 spec；2026-08-21 D 阶段评估采纳简化：**不建独立 alarm 角色薄壳**，fire_event/clear_event 作为跨角色 C6 能力注入——判定逻辑用既有 collector/transform/north 实例承载，专职告警实例需求出现再独立角色）
> **代码**：`src/event_bus.rs`（Alarm 变体）+ `src/db/alarms.rs`（lux Hash）+ `src/api/routes_alarm.rs` + `src/plugin/lua_core/api/mod.rs`（fire_event/clear_event）
> **前置**：`2026-08-18-lua-script-system-evolution-design.md` Q12/§5.1（alarm 行注记简化决策）

## 0. 定位与边界

**最小闭环** = 脚本可报、宿主可存、人可看：`ctx:fire_event/clear_event`（脚本判定）→ lux `alarms` Hash（活跃清单持久）→ EventBus `Alarm` 变体（SSE 实时推）→ REST 查询/手动清除 + 前端展示。

**明确不做**（Q12「最小」语义 + 防过度设计）：
- 告警规则引擎/模板/分级路由（判定逻辑归脚本——阈值/持续时间/迟滞全在 Lua 里写，宿主不猜）；
- 告警历史时序表（活跃清单 only；历史走日志/外部系统，需求出现再议）；
- 通知渠道（邮件/短信/webhook——出站通知可用现有 C5 http 能力脚本自组，宿主通知管道列按需）；
- 独立 alarm 角色薄壳（D 评估定案：transform 已覆盖「订阅判定」场景，第四角色 YAGNI 挂按需）。

## 1. 数据模型

### 1.1 EventBus 变体（SSE `event: alarm`）

```rust
Alarm {
    action: AlarmAction,   // Fire | Clear
    key: String,           // 告警键（脚本自定义，如 "dev1.temp_high"）
    level: String,         // "info" | "warn" | "error"（fire 时；clear 沿用清除时条目的 level）
    source: String,        // 实例 ID（宿主填，脚本不可伪造）
    message: String,       // 人类可读描述（fire 时）
    ts: String,            // ISO8601（宿主填）
}
```

- `action` 区分 fire/clear：前端一次订阅覆盖两向事件；clear 事件让多客户端视图收敛一致。
- `source` 由宿主从 ApiCtx 的 inst_id 注入——脚本无法冒名其他实例。
- `level` 字符串三档（非数字）：SSE JSON 直读、前端无需映射表；非法值在 ctx API 入口拒绝。

### 1.2 lux `alarms` Hash（活跃清单）

- field = `key`；value = JSON：`{level, source, message, ts, count}`。
- **fire 同 key**：覆盖写 + `count += 1`（重复报警次数——风暴可见性；read-modify-write 由宿主单点执行，无并发竞争）。
- **clear**：`HDEL alarms <key>`（不存在也返回成功，幂等）。
- 重启后活跃清单天然保留（lux 持久）——告警不因网关重启而消失，运维语义正确。

## 2. ctx API（C6 能力，全角色注入）

```lua
ctx:fire_event(key, level, message) -> bool   -- true=已上报；false=被限流（见 §3）
ctx:clear_event(key) -> bool                  -- true=清除成功（幂等）
```

- `key` 非空、≤128 字符；`level ∈ {info, warn, error}`；`message` ≤512 字符——超限返回 `nil, err`（可感知，与 R2 频控同错误形态）。
- 注入矩阵（§5.2 C6 行）：collector/transform/north **全部注入**（判定场景可能在任一角色：采集侧判原始点、transform 侧判派生点、north 侧判平台回执）。
- 审计：fire/clear 各落一条 `tracing::info!("[alarm] fire key={} level={} by={}")`（key 级不落 message 全文——防日志洪水，message 看 REST/SSE）。

## 3. 护栏（防告警风暴）

- **每实例频控**：token bucket 20 次/s（照 C5 http 的 R2 频控模式）；超限 `fire_event` 返回 `false` + stat `alarm_rate_limited` 计数——风暴时清单不被刷爆，SSE 不被挤爆。
- **同 key 抖动**：同 key 1s 窗口内重复 fire 仍计数（count++）但**不重复发 SSE**——前端不被同键闪屏；1s 后再 fire 正常发。
- EventBus 容量共享既有 256 ring（Alarm 事件小，挤占可忽略；lagged 有既有计数）。

## 4. REST（JWT 保护，照 routes_secret 模式）

| 端点 | 语义 |
|---|---|
| `GET /api/v1/alarms` | 活跃清单（HGETALL → `[{key, level, source, message, ts, count}]`，按 ts 倒序） |
| `DELETE /api/v1/alarms?key=<key>` | 手动清除（运维兜底；发 Clear 事件让前端收敛） |

## 5. 前端（最小面）

- `monitor.ts`：SSE `alarm` 监听 + `alarms` ref（fire upsert/clear remove）；`loadAlarms()` 拉取兜底（登录即拉 + 手动刷新）。
- 展示位：监控页新增「告警」面板（活跃清单列表：level 徽标/key/来源/次数/时间 + 手动清除按钮）——复用现有面板样式，不建独立路由页。
- types.ts：`AlarmEvent`/`AlarmItem` 类型。

## 6. 测试面（验收）

- ctx API：三角色注入矩阵锁 / key-level-message 校验 / 频控 false / 同 key 抖动 count++ 不重发 SSE；
- lux：fire 覆盖 count 累计 / clear 幂等 / 重启保留（lux 重启语义）；
- REST：JWT 401 / 清单形状 / DELETE 后 SSE Clear 事件到；
- SSE e2e：真实 EventBus publish → 前端 store 形状（vitest）；
- 指南：lua-script-guide.md 告警节（fire/clear 用法 + 判定脚本示例片段 + 频控说明）。
