# Lua 脚本示例

luaClient（collector）/ luaTransform（transform）脚本示例（配合
[`docs/guides/lua-script-guide.md`](../../docs/guides/lua-script-guide.md)）。

## 目录

- **`modbus-like/`**：Modbus TCP 协议（MBAP + FC03 读 / FC06 写），一问一答（`collect` + `on_write`）。
  头部带 manifest（`@role collector` / `@api_ver 1` / `@stdlib >=1.0`）——启动 dry-run 据此校验契约；
  CRC16 用官方 stdlib：`local crc16 = require("algo.crc16")`（`scripts/_stdlib/` 同步文件优先，
  离线回落宿主内嵌种子；依赖 stdlib ≥1.0，阶段 A 种子即满足）。
- **`custom-on-data/`**：自定义协议 + `on_data` 被动广播上报（`set_value` + `kv_set` 持久化）。
- **`multi-device/`**：单实例多设备（一条 RS485 总线多从站），`collect` 嵌套返回 `{[devid]={tag=v}}`（M2）。
  手写 CRC16 仅为演示组帧，可直接换官方库 `require("algo.crc16")`（见脚本内注释）。
- **`agg-filter/`**：transform 角色完整示例（`@role transform`）——订阅原始点 →
  限幅剔除野点 → 滑动窗口均值 → `set_value` 写派生点；窗口状态存 `ctx:kv_get/kv_set`
  （跨重启/热加载不丢），窗口大小经 kv `cfg_window` 可配（reload 生效）。
  实例配置 `app_code="luaTransform"` + `subscribe`（如 `["raw1.temp*"]`，devid
  不含 "."——订阅模式以 "." 分段）。

## 运行

1. 拷贝脚本到 rs-iot `scripts/` 目录（或用 `app_config.scripts_root` 指定自定义根）：

   ```bash
   cp -r examples/lua-scripts/modbus-like scripts/
   ```

2. REST 建实例（`app_code="luaClient"`）：

   ```json
   {
     "inst_id": "modbus1",
     "app_code": "luaClient",
     "app_config": {
       "script": "modbus-like",
       "kind": "tcp",
       "endpoint": "192.168.1.10:502",
       "period": 2
     },
     "devices": [{ "label": "设备", "value": "dev1", "children": [] }]
   }
   ```

3. `POST /api/v1/startinst`（body `{"inst_id":"modbus1"}`）→ `redis-cli -p 6390 HGETALL rtdata:dev1` 验采集。

## 写命令

经 REST `POST /api/v1/command/send` 或 MQTT `<hwid>/command`，`data` JSON 由脚本 `on_write`
解析（modbus-like 用 `{"addr":0,"value":100}` → FC06）。

## 多设备

M2 起 `luaClient` 单实例支持一条链路多设备（如一条 RS485 总线多台从站）：

- `collect` **嵌套返回** `{[devid]={tag=v, ...}, ...}` → 宿主为每 `devid` 各产一个 `Sample`。
- `ctx:set_values(devid, {tag=v,...})` 批量上报一个设备多点（一个 `Sample`，同时刻 timestamp）。
- `ctx:set_value(devid, tag, v)` 单点（3 参，`devid` 显式传入）。

平铺返回 `{tag=v}`（老脚本形态）仍归 `devices[0]`，向后兼容。详见 `docs/guides/lua-script-guide.md`。

## 数据加工（luaTransform）

阶段 C 起 `luaTransform`（`@role transform`）订阅 EventBus 实时数据做清洗/聚合，输出
派生点与 collector 同出口（前端/北向零差别消费）。agg-filter 示例：

1. 拷贝脚本到 rs-iot `scripts/` 目录，REST 建实例（`app_code="luaTransform"`）：

   ```json
   {
     "inst_id": "aggfilter1",
     "app_code": "luaTransform",
     "app_config": {
       "script": "agg-filter",
       "period": 0,
       "subscribe": ["raw1.temp*"]
     },
     "devices": [{ "label": "派生设备", "value": "agg1", "children": [] }]
   }
   ```

   `subscribe` 是数据面入口（`"raw1.temp*"` = raw1 的 temp 开头 tag；空 = 纯定时，
   devid 不含 "."——订阅模式以 "." 分段），`period=0` = 纯订阅不调 `on_timer`。
   窗口大小默认 5，写 kv `cfg_window` 后 reload 可改。

2. `POST /api/v1/startinst` → raw1.temp 有新值即触发 `on_rtdata` →
   `redis-cli -p 6390 HGET rtdata:agg1 agg1.temp_avg` 看窗口均值派生点。

订阅语法、自环防护（输出不触发自身）、跨实例环配置责任见指南
「transform 角色（luaTransform）」章。

## API 参考

见 [`docs/guides/lua-script-guide.md`](../../docs/guides/lua-script-guide.md)（ctx API L1-L6 + 回调 + 配置 + 传输 + 沙箱）。
