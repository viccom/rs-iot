# Lua 脚本开发指南

> rs-iot luaClient 动态脚本采集——覆盖长尾工业协议（自定义报文、串口/以太网、一问一答/广播推送）。
> 随改随用、无需编译；沙箱护栏（库裁剪 + 指令 hook + 内存限额）挡脚本错误。

## 概述

luaClient 是 rs-iot 的南向 `Source`（与 modbus/opcua/s7 同构，`inventory` 静态注册）。
脚本（Lua 5.4，经 mlua 嵌入）聚焦**协议报文解析 + 数据计算**，宿主管通讯链路
（tcp/serial/udp）+ 双总线：

- **数据上行**：`collect` 返 table / `on_data` 调 `ctx:set_value` → hub → pipeline → lux `rtdata:<devid>`。
- **控制下行**：写命令经 `CtlRegistry` → 实例 `cmd_rx` → `on_write(cmd)`。

详见 `docs/superpowers/specs/2026-08-06-lua-script-collector-design.md`。

## 快速开始

1. 建脚本目录 `scripts/<name>/main.lua`（`<name>` 仅 `[A-Za-z0-9_-]`，防路径穿越）：

   ```lua
   function collect()
     local resp = ctx:request("\x01\x03\x00\x00\x00\x02\xC4\x0B", 200)
     if not resp or #resp < 7 then return nil end
     local temp = string.unpack(">i2", resp:sub(4, 5)) / 10
     return { temp = temp }
   end
   ```

2. REST 建实例（`app_code="luaClient"`）：

   ```json
   {
     "inst_id": "dev1",
     "app_code": "luaClient",
     "app_config": { "script": "mydev", "kind": "tcp", "endpoint": "192.168.1.10:502", "period": 2 },
     "devices": [{ "label": "传感器", "value": "dev1", "children": [] }]
   }
   ```

3. `POST /api/v1/startinst`（body `{"inst_id":"dev1"}`）→ `redis-cli -p 6390 HGETALL rtdata:dev1` 验采集。REST 端点全表见 [`rest-api.md`](rest-api.md)。

## 回调函数

`ctx` 是全局对象（回调不取 ctx 参数）。冒号语法 `ctx:request(...)`（隐式 self 由宿主吸收）。

| 函数 | 调用时机 | 返回 | 用途 |
|---|---|---|---|
| `on_init()` | 启动时一次（dry-run 验证） | 忽略 | 初始化（建表/读 kv/日志） |
| `collect()` | 每个 `period` 周期（`period=0` 不调） | `table\|nil` | 主动一问一答采集 |
| `on_data(frame)` | IO task 收被动帧（`recv_mode`） | 忽略 | 广播/推送；`ctx:set_value` 上报 |
| `on_write(cmd)` | 收写命令 | `bool\|nil` | 处理写命令；`cmd` 为 JSON 解析后的 table |

`collect` 返 table：
- **平铺**（单设备，老脚本兼容）：`{ tag = value, ... }` → 宿主按 `inst.devices[0]` 组 `Sample` 推 hub。
- **嵌套**（多设备，M2）：`{ [devid] = { tag = value, ... }, ... }` → 宿主为每 `devid` 各组一个 `Sample`（同一次 collect 共用同一 timestamp，点间同时刻）。适合一条 RS485 总线多从站。

自动判定：外层 table 的 value 是 table → 嵌套；是标量 → 平铺（零歧义）。

## ctx API 参考

### L1 通讯

- `ctx:request(frame, timeout_ms) -> resp|nil, err?`：主动一问一答。`frame` 字符串（含二进制，
  `"\x01\x03..."`）；`timeout_ms` 毫秒；返回响应字节串或 `nil, err`。tcp/serial/udp 统一入口。

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

### L3 控制（联动）

- `ctx:send_command(inst_id, devid, cmd, data) -> bool`：经 `CtlRegistry` 投递写命令到目标实例
  （与 MQTT/REST 写入口同总线，`try_send` 非阻塞，未注册返 `false`）。

### L4 持久（脚本本地状态）

- `ctx:kv_set(key, v)`：async，落 lux `luakv:<inst>`（JSON 文本）。跨重启保留、实例间隔离。
- `ctx:kv_get(key) -> v|nil`：async，读 kv（value 经 JSON round-trip，支持 number/string/bool/table）。

### L5 HTTP（feature `lua-http`，全审计）

- `ctx:http_get(url, headers?, timeout?) -> {status, body}`：GET。`headers` 为 `{name=value}` table。
- `ctx:http_post(url, body, headers?, timeout?) -> {status, body}`：POST。
- 响应体上限 1MB（防 OOM）；超时默认 5s；每次请求 INFO 审计（成功）/ warn（失败或超限）。

### L6 工具

- `ctx:log_info(msg)` / `ctx:log_warn(msg)` / `ctx:log_err(msg)` / `ctx:log_debug(msg)`：tracing 日志，
  前缀 `[lua:<inst>]`。
- `ctx:now() -> num`：当前本地毫秒时间戳（f64）。
- `ctx:time() -> num`：当前本地秒时间戳（f64）。

## 配置（`app_config`）

| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `script` | str | 必填 | 脚本目录名（→ `scripts/<script>/main.lua`，仅 `[A-Za-z0-9_-]`） |
| `kind` | str | `tcp` | 传输：`tcp` / `serial` / `udp` |
| `endpoint` | str | tcp/udp 必填 | `ip:port`（serial 时忽略） |
| `serial_port` | str | serial 必填 | 串口名（如 `COM3` / `/dev/ttyUSB0`） |
| `baud_rate` | num | `9600` | serial 波特率 |
| `data_bits` | num | `8` | serial 数据位（5/6/7/8） |
| `parity` | str | `none` | serial 校验：`none` / `even` / `odd` |
| `stop_bits` | num | `1` | serial 停止位（1/2） |
| `flow_control` | str | `none` | serial 流控：`none` / `software` / `hardware` |
| `period` | num | 必填 | collect 周期（秒）；`0` = 纯被动（仅 `on_data`） |
| `timeout_ms` | num | — | `ctx:request` 默认超时（毫秒） |
| `recv_mode` | str | — | 被动帧成帧模式（`timeout` / 后置 length/delimiter/fixed） |
| `recv_idle_ms` | num | — | 被动帧 idle 静默成帧（毫秒） |
| `reconnect_secs` | num | — | 断连重连间隔（秒） |
| `mem_limit_mb` | num | — | VM 内存限额（MB，防脚本泄漏） |
| `hook_instr_limit` | num | — | 指令 hook 阈值（防死循环，spec §9.2） |
| `fail_threshold` | num | `10` | 连续 collect 失败 N 次标记 degraded |

## 传输

- **tcp**（`kind="tcp"`，`endpoint="ip:port"`）：独占 `TcpStream`，懒连接 + 断连重连。
- **serial**（`kind="serial"`，`serial_port=...`）：tokio-serial，RTU 帧走串口（Linux 需 libudev-dev）。
- **udp**（`kind="udp"`，`endpoint="ip:port"`）：connected `UdpSocket`，datagram 边界天然保帧。

三种传输统一 `ctx:request`（IO task 独占处理，spec §8 模式 Z）。`recv_mode` 控被动帧成帧
（`on_data`）：当前 `timeout`（idle 静默）；length/delimiter/fixed 后置。

## 示例

`examples/lua-scripts/`：

- `modbus-like/`：Modbus TCP 协议（MBAP + FC03 读 / FC06 写），一问一答（`collect` + `on_write`）。
- `custom-on-data/`：自定义协议 + `on_data` 被动广播上报（`set_value` + `kv_set` 持久化）。
- `multi-device/`：单实例多设备（一条 RS485 总线多从站），`collect` 嵌套返回（M2）。

## 沙箱限制

脚本在受限 mlua VM 运行（spec §9）：

- **禁库**：`io` / `os` / `debug` / `package`（不可文件 IO / 执行命令 / require 任意模块）。
- **指令 hook**：超阈值（`hook_instr_limit`）中断死循环。
- **内存限额**：`mem_limit_mb` 上限，超则 OOM 中断。
- **不暴露**：lux client / 文件系统 / 原始 socket（通讯仅经 `ctx:request`）。
- **本地模块**：`require "foo"` 解析 `scripts/<script>/?foo?.lua`（防 `..` 穿越仓库）。

## 热加载

改 `scripts/<name>/main.lua` → notify watcher（去抖 200ms）→ VM 重建（新 load + register +
dry-run `on_init` 验证）。失败回退旧 VM（永不半生效）。

兜底：`POST /api/v1/lua/reload?inst_id=<id>`（stop + start，IO 重连）。

## 状态监测

lux `stat:<inst>` JSON（每 collect 周期采点）：

- `running` / `stopped` / `degraded`（连续失败 ≥ `fail_threshold`）。
- `collect_count` / `collect_errors` / `last_collect_ms` / `reload_count` / `script_version`。

REST：`GET /api/v1/lua/stats?inst_id=<id>`（读 stat）/ `GET /api/v1/lua/scripts`（列脚本）。
