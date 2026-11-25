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
阈值取值贴近模拟器正弦峰值——正常态大部分时间**无报警**，峰值附近短暂越限，符合 HP-HMI
"正常灰阶、异常才出饱和色"的纪律。仿真里三个电表同相位（`sin(t/30)`），峰值时刻会同时越限。

## 3. 启动对账（重要）

transform 重启会丢失脚本内的 `fired` 表，上一轮已 fire 的告警会变成孤儿（值回落也不 clear）。
脚本 `on_timer` 以 15 条/秒分批对全部规则 key 调 `clear_event`（避开 20/s 频控），
**跳过本轮已 fire 的 key**，对账完成后停止。实例 `period=1`（tick 用于对账，不做别的事）。

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
