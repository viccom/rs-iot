# iot-demo 限值报警（limit_alarm transform）运维说明

> **状态**：活文档（随脚本阈值与报警规则演进；2026-11-24 落地）
>
> 2026-11-24 落地。脚本：`scripts/limit-alarm/main.lua`；实例：`limit_alarm`
> （`app_code=luaTransform`，`auto_start=true`）；HMI 侧消费：`hmi-studio/src/lib/synthetic.ts`。
> 定位：把"点位越限"变成 rs-iot 活动告警，喂给 HMI 报警徽标/区域计数/报警抽屉。

## 1. 数据流

```
simulator 采样 → rs-iot pipeline → EventBus Rtdata → limit_alarm(transform on_rtdata)
   → ctx:fire_event/clear_event → lux alarms 哈希 → SSE alarm 帧
   → hmi-studio AlarmStore → 合成点位（__prio/__alarms/__area/__sys）→ 画面
```

- 判定在**服务端**（脚本内阈值/回差），HMI 不做阈值计算，只显示结果。
- 报警级别映射（HMI）：`error→P1`（红）、`warn→P3`（黄）、`info→P4`（蓝）；P2 预留联锁。
- key 格式：`<devid>.<tag>.<hh|hi|lo|ll|alarm>`。合成点位按第一个 `.` 取 devid、
  前两段取点字段（`dev_wm1.flow_rate.__prio`）。

## 2. 阈值（与 `hmi-studio/src/screen-assets/iot-catalog.ts` 的 LIMITS **必须一致**）

| 设备 | 点位 | 规则 | 级别 |
|---|---|---|---|
| dev_wm1/2/3 | flow_rate | H 31.5/36.5/41.5，HH 31.9/37/42 | warn / error |
| dev_wm1/2/3 | pressure | H 0.428/0.468/0.508 MPa | warn |
| dev_wm* | leak_alarm | true | error |
| dev_wm* | reverse_flow | true | info |
| dev_gm1/2 | flow_rate | H 21.7/26.5，HH 22/27 | warn / error |
| dev_gm1/2 | pressure | L 1.75/1.85，H 3.05/3.55 kPa | warn |
| dev_gm* | leak_alarm / overpressure | true | error |
| dev_em1/2/3 | active_power | H 51.8/57.8/63.5，HH 52/58/64 kW | warn / error |
| dev_em* | thd_voltage | H 2.95/3.45/3.95 % | warn |
| dev_em* | overload_alarm | true | error |
| dev_env1 | co2 | H 810，HH 830 ppm | warn / error |
| dev_env1 | temperature | H 28.8 °C | warn |
| dev_env1 | alarm | true | error |
| dev_pt1 | pressure_1 | H 1.69，HH 1.695 MPa | warn / error |
| dev_pt1 | vibration | H 3.8 mm/s | warn |
| dev_pt1 | fault | true | error |

回差 = 阈值 2%（`HYS_REL`）；布尔故障用 `alarm` 规则，true→fire、false→clear。
阈值取值贴近模拟器正弦峰值：不加延时时，峰值附近每周期都短暂越限（1–12 秒），见 §2.1。
仿真里三个电表同相位（`sin(t/30)`，周期约 3 分钟），峰值时刻会同时越限。

### 2.1 模拟量延时（2026-11-25）

模拟量规则（h/hh/l/ll）**连续越限满 `ANALOG_DELAY_S`（默认 15 秒）才 fire**（ISA-18.2 的
alarm delay）。要点：

- 首次越限开始计时，到点时仍在越限才报。计时期间回落到阈值下方的回差带内（2% 以内）
  **不打断计时**，跌破回差带才取消——与报出后的清除线一致，避免临界抖动反复重计时。
- 报出后的清除不受延时影响（跌破回差带即清）。fire 被频控拒收（`ctx:fire_event` 返回
  false）时，下一个越限采样立即重试，不重新计时。
- 布尔故障点（`alarm` 规则）**不走延时**，立即报。
- 重启/热加载后计时从零开始：重启时正在持续的模拟量越限先被启动对账清掉（§3），再等满
  一个延时才重新报出，即有约 15 秒的空窗。

为什么需要：模拟器的模拟量是正弦波，不加延时时峰顶每周期都短促越限，演示墙的全厂 P1
徽标每分钟闪几次。13.6 分钟实测（1 Hz 实时值，按同一套阈值与回差回放）：

| 延时 | 仍报出的模拟量告警 | 事件/分钟 | 其中 error 级 |
|---|---|---|---|
| 0（旧行为） | 362 | 53.2 | 36 |
| 10 秒 | 18 | 2.6 | 0 |
| **15 秒（现行）** | **10** | **1.5** | **0** |
| 20 秒 | 6 | 0.9 | 0 |
| 30 秒 | 4 | 0.6 | 0 |

现场实测（15 秒，10.4 分钟）：模拟量 fire 9 次、**error 级 0 次**（改前约 3 分钟内 78 次、
error 级 10 次）。留下来的都是严格越限满 15 秒的：3 号电表 `active_power` H（报出后可见
约 14 秒，每周期一次）、电表 `thd_voltage`、环境 `temperature` / `co2`（可见 24–91 秒）。
调到 20 秒只能再去掉 3 号电表那一个，收益递减。

调参：改脚本顶部 `ANALOG_DELAY_S`（`0` = 关闭延时），保存后热加载生效。判定逻辑的单测在
`src/plugin/builtin/lua_transform/tests.rs`（假 `ctx` + 可控时钟驱动本脚本）。

## 3. 启动对账（重要）

transform 重启会丢失脚本内的 `fired` 表，上一轮已 fire 的告警会变成孤儿（值回落也不 clear）。
脚本 `on_timer` 以 15 条/秒分批对全部规则 key 调 `clear_event`（避开 20/s 频控），
**跳过本轮已 fire 的 key**，对账完成后停止。实例 `period=1`（tick 用于对账，不做别的事）。
布尔故障在重启后的下一个 `true` 采样（≤1 秒）即重新 fire：若早于对账扫到该 key 则被跳过，
若晚于对账则先清后报（闪断不到 1 秒）。模拟量因有延时（§2.1）来不及在对账前重新 fire，
会先被清掉、满一个延时后再报。

## 4. 实例配置（REST 复建）

```bash
B=http://127.0.0.1:20000
TOKEN=$(curl -s -X POST $B/api/v1/user/login -H 'Content-Type: application/json' \
  -d '{"user_id":"admin","pass":"admin"}' | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
curl -s -X POST $B/api/v1/inst/set -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' -d '{
  "inst_id":"limit_alarm","object":{
    "inst_name":"限值报警（iot-demo）","app_code":"luaTransform","app_type":"south","auto_start":true,
    "app_config":{"script":"limit-alarm","period":1,"subscribe":["*"],"callback_timeout_ms":10000,"mem_limit_mb":32},
    "devices":[],"host_kind":"builtin"}}'
curl -s -X POST $B/api/v1/startinst -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' -d '{"inst_id":"limit_alarm"}'
```

- 脚本热加载：改 `scripts/limit-alarm/main.lua` 后 watcher 会 reload（新 VM 重跑 on_init/对账）。
- 停用：`stopinst`（只停报警判定，不影响采集与画面）。
- 查看：`GET /api/v1/alarms`；日志关键字 `[alarm] fire|clear`（source=`limit_alarm`）。

## 5. 边界

- 只有 clear（人工"确认"= DELETE `/alarms?key=`），没有 ack 语义——HMI 不伪装确认态。
- 不做报警历史时序/通知路由（Q12 最小闭环的既定边界）。
- 阈值双处维护（本文件 + Lua 表 / 画面目录 TS）：改一处必须同步另一处。

## 6. 历史读阻塞的现场处置（2026-11-25）

若 BI 面板全空但 `GET /api/v1/sysstatus` 的 history 块仍报 `engine:"ok"`：
那是**读路径被存储引擎的可见性栅栏写锁挡住**（不是数据丢失）。处置顺序：

1. 确认：`curl -m 5 "…/api/v1/history?dev_id=dev_em1&tag=active_power&start=…&end=…" -H "Authorization: Bearer …"`
   超时不返回 = 命中该故障。
2. 恢复：重启 rs-iot（数据在 tsink 落盘，重启不丢）：
   `curl -X POST -H "Authorization: Bearer $WARDEN_TOKEN" http://127.0.0.1:8791/api/v1/services/rs-iot/restart`
   （优雅停机可能超时被 warden 强杀，属该故障的伴随现象。）
3. 读超时护栏（已上线）：读超过 20s 返回 503 `{error:"history query timeout"}`，
   预算可用环境变量 `RSIOT_HISTORY_READ_TIMEOUT_SECS` 调整。
4. 根因与修复方向见 `tsink_fork/docs/plans/2026-11-25-visibility-fence-read-deadlock.md`。
