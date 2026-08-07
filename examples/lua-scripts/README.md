# Lua 脚本示例

luaClient 脚本示例（配合 [`docs/lua-script-guide.md`](../../docs/lua-script-guide.md)）。

## 目录

- **`modbus-like/`**：Modbus TCP 协议（MBAP + FC03 读 / FC06 写），一问一答（`collect` + `on_write`）。
- **`custom-on-data/`**：自定义协议 + `on_data` 被动广播上报（`set_value` + `kv_set` 持久化）。

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

## API 参考

见 [`docs/lua-script-guide.md`](../../docs/lua-script-guide.md)（ctx API L1-L6 + 回调 + 配置 + 传输 + 沙箱）。
