# Lua 脚本开发指南

> **状态**：活文档（随 luaClient/luaTransform 演进同步）
> rs-iot luaClient 动态脚本采集——覆盖长尾工业协议（自定义报文、串口/以太网、一问一答/广播推送）。
> 随改随用、无需编译；沙箱护栏（库裁剪 + 指令 hook + 内存限额）挡脚本错误。

## 概述

luaClient 是 rs-iot 的南向 `Source`（与 modbus/opcua/s7 同构，`inventory` 静态注册）。
脚本（Lua 5.4，经 mlua 嵌入）聚焦**协议报文解析 + 数据计算**，宿主管通讯链路
（tcp/serial/udp）+ 双总线：

- **数据上行**：`collect` 返 table / `on_data` 调 `ctx:set_value` → hub → pipeline → lux `rtdata:<devid>`。
- **控制下行**：写命令经 `CtlRegistry` → 实例 `cmd_rx` → `on_write(cmd)`。

另有两类角色（同一套沙箱与 ctx API）：

- **mqtt-north 角色**（`luaNorth` 实例，`@role mqtt-north`）：北向把收拢的批量数据经
  MQTT 发布上云（EMQX/OneNET 等平台配方见「mqtt-north 角色」章节）。
- **transform 角色**（`luaTransform` 实例，`@role transform`，阶段 C）：订阅 EventBus
  实时数据做清洗/聚合，输出派生点与 collector 同出口（见「transform 角色」章节）。

设计详见 `docs/designs/2026-08-06-lua-script-collector-design.md`、
`docs/designs/2026-08-18-lua-script-system-evolution-design.md` 与
`docs/designs/2026-08-20-lua-script-system-stage-b-plan.md` /
`docs/designs/2026-08-20-lua-script-system-stage-c-plan.md`。

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

3. `POST /api/v1/startinst`（body `{"inst_id":"dev1"}`）→ `redis-cli -p 6390 HGETALL rtdata:dev1` 验采集。REST 端点全表见 [`rest-api.md`](../reference/rest-api.md)。

脚本头部可选择性声明 manifest（`@role`/`@api_ver`/`@stdlib` 契约，启动 dry-run 校验，
见「manifest 头部声明」）；需要复用库时经 `require` 四层路由取 json / 官方 stdlib / 共享库
（见「require 与模块库」）。

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
- `ctx:transport() -> str`：返回底层链路类型 `"tcp"`/`"serial"`/`"udp"`/`"mqtt"`（与 `app_config.kind` 一致，
  同步）。脚本据此**选帧格式**——同一份脚本自适应不同链路：

  ```lua
  local crc16 = require("algo.crc16")   -- 官方 stdlib（见「require 与模块库」）

  function collect()
      local pdu = string.pack(">BBHH", unit, 3, addr, qty)
      -- serial/udp 走 RTU 帧（PDU + CRC16 小端附尾），tcp 走 MBAP 头（无 CRC）
      local frame
      if ctx:transport() == "serial" or ctx:transport() == "udp" then
          frame = pdu .. crc16.crc16(pdu)   -- 返回小端 2 字节串，直接拼帧尾
      else
          frame = string.pack(">I2I2I2BB", 1, 0, 6, unit, 3) .. string.pack(">I2I2", addr, qty)
      end
      return decode(ctx:request(frame, 200))
  end
  ```

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

## require 与模块库

沙箱内 `require` 按模块名**首段**路由四层（设计 §3.1）。判别顺序：违规拒绝 → builtin 名单 →
单段/旧 `lib.` 前缀（私有）→ 官方保留段（stdlib）→ 其余 ≥2 段（站点共享）。

### 命名空间路由表

| require 名 | 层 | 解析位置 | 说明 |
|---|---|---|---|
| `json` | builtin | Rust 注册（不读盘） | builtin 名单优先于"单段=私有"判断；阶段 A 仅 `json` |
| `algo.crc16`、`codec.basexx`、`cloud.onenet` | 官方 stdlib | `scripts/_stdlib/<路径>.lua` | 首段 ∈ 保留段（`algo`/`codec`/`proto`/`cloud`/`util`/`oo`）。`_stdlib/` 同步文件优先，miss 回落宿主内嵌种子，仍 miss 报"未同步"可行动错误——**永不回落共享/私有层**（防投毒核心规则） |
| `viccom.calcex`、`viccom.calcex.poller` | 站点共享 | `scripts/_lib/<首段>/<余段>.lua` | 首段 = 作者/组织命名空间，**须 ≥2 段**——单段名结构上进不了共享层（见下违规示例） |
| `myutil`、`lib.util`（旧前缀兼容） | 脚本私有 | `scripts/<script>/lib/<段>.lua` | 单段名或旧 `lib.` 前缀，现状语义不变 |

违规与边界示例：

```lua
require("../evil")   -- 拒：穿越 / 绝对路径 / 反斜杠 / 内嵌斜杠 / 空段，一律报错
require("C:foo")     -- 拒：段含盘符冒号（Windows 上 join 会以盘符前缀整体替换基路径逃逸）
require("lib")       -- 拒：裸 lib 前缀无模块名
require("json.util") -- 拒：builtin 名（json）是封闭单段名，不可多段引用
require("viccom")    -- 不拒但**进不了共享层**：单段名路由到私有 scripts/<script>/lib/viccom.lua；
                    --   要进 _lib/ 须经 REST 保存（保存侧拒单段与保留段首段，见下）
require("algo.evil") -- 保留段只查 _stdlib/ 与种子：不存在即报"未同步"，不会回落到任何第三方文件
```

> **⚠️ 旧脚本迁移告示（多段私有 require 行为变更）**：旧版加载器把任意点分名（如
> `require("vendor.helper")`）都映射到私有 `scripts/<name>/lib/vendor/helper.lua`；
> 首段路由上线后，多段名改判共享层（`scripts/_lib/vendor/helper.lua`）——旧用法会得到
> "共享库不存在"错误。**修复**：私有库改单段名（`require("helper")` →
> `scripts/<name>/lib/helper.lua`）或旧 `lib.` 前缀（`require("lib.helper")`，语义不变）。
> 单段与 `lib.` 前缀的私有语义**逐字节不变**，未用过多段私有 require 的脚本零影响。

### json 模块（builtin，C0）

```lua
local json = require("json")
local s = json.encode({ a = 1, b = "x" })   -- table → JSON 字符串
local t = json.decode('{"k":[1,2,3]}')      -- JSON 字符串 → table；t.k[2] == 2
```

- 输入上限 **1MB**（防 OOM）：`decode` 超限报 `json: 输入超 1MB`；非法 JSON 报解析失败。
- encode/decode 与宿主 serde_json 同源，number/string/bool/table（数组/对象）往返无损。

### 官方 stdlib（保留段命名空间）

阶段 A 种子 4 库（vendored from freeioe@1398d47，MIT；随宿主内嵌，离线冷启动兜底）：

| 模块 | 函数面 | 说明 |
|---|---|---|
| `algo.crc16` | `crc16.crc16(adu) -> str` | Modbus CRC16（多项式 0xA001 反射、初值 0xFFFF）；返回 `string.pack('I2', crc)` **小端 2 字节串**，可直接拼帧尾：`req .. crc16.crc16(req)` |
| `codec.basexx` | `basexx.to_base64 / from_base64 / to_hex / from_hex / to_url64 / to_z85 / to_base32 / ...` | base2 / base16 / base32（RFC3548 + Crockford）/ base64 / url64 / z85 编解码 |
| `util.retry` | `retry.retry(count, func, ...)` | 可重试调用：`func` 首返回值 falsy 时重试至多 `count` 次（透传全部返回值） |
| `util.tablex` | `tablex.equals(a, b, ignore_mt?)` | 深度表相等比较（`__eq` 元方法短路 + 键集双向比对） |

- 每库带 `selftest()`（已知向量/往返校验），宿主启动与 CI 冒烟全量跑（语法 + 语义闸门）。
- **优先级**：`scripts/_stdlib/<路径>.lua` 存在 → 同步文件优先；否则回落 embed 种子。
- 种子无独立版本轴（`stdlib.list()` 中 `ver=nil`）；sync 后以本地 index 为准。

`stdlib` 全局表（本地清单只读视图，index 与种子合并、index 为准消歧）：

```lua
stdlib.list()   -- -> { {name="algo.crc16", path="algo/crc16.lua", ver=nil, source="seed"}, ... }
                --    source: "seed"（内嵌兜底）| "synced"（index 安装，含 ver）
stdlib.info("algo.crc16")   -- 点分模块名或文件路径（"algo/crc16.lua"）→ 单条 table | nil
```

**同步官方源/内网镜像**（ed25519 验签 + sha256 逐文件校验 + `_stdlib.tmp/` 原子替换 + 全实例 reload）：

```bash
# 清单（含版本与来源徽标）
curl -H "Authorization: <token>" localhost:8790/api/v1/lua/stdlib
# 同步：upstream/pin 均可省——upstream 缺省读 AppConfig.lua_stdlib_upstream（内网镜像改配置切换）
curl -X POST -H "Authorization: <token>" -H 'Content-Type: application/json' \
  -d '{"upstream":"https://rsiot.github.io/lualib/","pin":"1.0.0"}' \
  localhost:8790/api/v1/lua/stdlib/sync
```

**版本策略（pin，设计 §3.3 默认锁定）**：`pin` 缺省 = **锁定当前已装版本**（上游发新版会
因"pin 版本不匹配"拒装，须显式升级）；`"pin":"auto"` = 显式跟随 latest；请求体显式版本 >
配置键 `lua_stdlib_pin`（持久化锁定）> 本地已装版本；首次安装（本地无 index）自动取 latest。
每次 sync 成功/失败均落审计日志（`[lua][audit] stdlib sync ...`，§3.7）。

sync 后的批量 reload 为 **VM 级热加载**（运行中实例逐一经控制通道触发 `reload_vm`，失败自动
回退旧 VM）；**非运行态实例不复活**（响应中记 `skipped`，下次启动自然取新库）。

官方脚本模板（index 中 `scripts/` 前缀条目，如 `scripts/modbus-like/main.lua`）随 sync 分发到
`_stdlib/scripts/` 供查阅，但**尚不可直接配置运行**——安装到 `scripts/<name>/` 的安装器与
`@source` 徽标（设计 Q18）为 B 阶段项；模板条目也不列入 `stdlib.list()`（非 require 可达模块）。

### 在线分发库与 cloud/ 云平台配方（官方源 ≥1.1.0）

官方源 1.1.0 起在线分发（**不进宿主种子**，sync 后 require 可达）：

| 版本 | 模块 | 函数面 | 说明 |
|---|---|---|---|
| 1.1.0 | `algo.hashings.{uintn,md5,sha1,sha256,hmac}` | 流式哈希 + HMAC（RFC 2104） | vendored from freeioe@1398d47（MIT）；selftest 带 NIST/RFC 已知向量 |
| 1.1.0 | `cloud.emqx` | `auth(cfg) -> nil`；`topic_data(hw, devid) -> "{hw}/{devid}/datas"` | EMQX 裸 broker 薄配方：无平台级认证（凭证/主题 ACL 全走 broker 侧），脚本直连 |
| 1.2.0 | `cloud.onenet` | `token(ak, pid, dev, et)`；`auth_builder(cfg)`；`topic_property_post(pid, dev)`；`parse_property_set(topic, payload)` | OneNET 安全鉴权 token 配方（见下） |

**OneNET token 约定假设**（`cloud.onenet`，务必现场核对）：

- 签名算法按官方「安全鉴权token」规范 **version 2018-10-31**：签名原文
  `et + "\n" + method + "\n" + res + "\n" + version`（参数名排序，仅 value 参与）；
  `access_key` **先 base64 解码**再作 HMAC-SHA1 key；`sig` = 标准 base64 后做 URL
  百分号编码（`+`→`%2B`、`/`→`%2F`、`=`→`%3D`，官方 Python 示例 `quote(safe='')` 语义，
  **非** RFC 4648 urlsafe 字母表）；`token = version&res&et&method&sig`；设备接入
  `res = products/{pid}/devices/{device_name}`。
- MQTT 凭证采用 **2024+ 新版物模型接入**形态：`client_id = device_name`、
  `username = product_id&device_name`、`password = token`；**旧版 MQTTS 多协议**接入
  是 `username = device_name`——现场是旧版时脚本自行组包（fork 配方改 `auth_builder`）。
- 属性通道 topic：上报 `$sys/{pid}/{dev}/thing/property/post`；设置下行
  `$sys/{pid}/{dev}/thing/property/set`（`parse_property_set` 校验 topic 形状并解
  OneJSON，返回 `{pid, dev, id, params}` 或 `nil, 原因`）。
- selftest 锁 Python 参考向量（算法一致性）；**性能**：token 含一次纯 Lua hmac_sha1
  （~70 万 VM 指令），mqtt-north 实例须调高 `hook_instr_limit`（见 mqtt-north 章节）。

### 站点共享库（`scripts/_lib/`）

第三方/站点级复用库经 REST 保存（`kind=lib`），require 按 `<org>.<库路径>` 引用：

```bash
# 保存（≥2 段、首段非保留段、防穿越；版本快照落 scripts/_lib/viccom/versions/）
curl -X POST -H "Authorization: <token>" -H 'Content-Type: application/json' \
  -d '{"kind":"lib","name":"viccom/hex.lua","content":"local M={} ... return M"}' \
  localhost:8790/api/v1/lua/script/save
# 列表 / 版本 / 回滚同族端点均支持 ?kind=lib（GET /api/v1/lua/scripts?kind=lib 等）
```

```lua
local hex = require("viccom.hex")   -- → scripts/_lib/viccom/hex.lua
```

`_lib/` 由**全局 watcher** 监听（去抖 200ms）：任一共享库变更 → **全实例** reload（区别于
main.lua 的 per-instance watcher）。保存侧拒绝单段（`foo.lua`）与撞保留段（`algo/evil.lua`）。

## manifest 头部声明

main.lua 顶部注释块可声明脚本契约（`-- @key value`，逐行扫至首个代码行；未知 `@key` 忽略——
向前兼容）：

```lua
-- @role collector
-- @api_ver 1
-- @stdlib >=1.0
-- @param period number 可选 上报周期秒 默认5 范围:1-3600
-- @source local
```

| 键 | 含义 | 缺省 |
|---|---|---|
| `@role` | 脚本角色：`collector`（南向采集，必需 `collect`）/ `mqtt-north`（北向 MQTT，必需 `auth`+`up`，见「mqtt-north 角色」）/ `transform`（数据加工，必需 `on_timer`/`on_rtdata` **至少其一**，见「transform 角色」） | 不声明（缺省语义 collector，但不强制契约，见下） |
| `@api_ver` | 要求的宿主 API 版本（整数） | `1`（非法值报错，不静默放行） |
| `@stdlib` | stdlib 版本范围：`>=1.0`、`>=1.0 <2`（空格组合，全部满足） | 不限 |
| `@param` | 参数声明，可多行（B 阶段参数面板消费） | — |
| `@source` | 来源徽标（`local` / `market` 等） | — |

`@param` 后缀语法：`<name> <type> [可选] [描述...] [默认X] [枚举:a|b] [范围:a-b]`——
`type` 为 `number`/`string`/`boolean`；出现「可选」token → 非必填；`默认X` 存默认值；
首个 `枚举:`/`范围:` token 整体存为约束。

**dry-run 校验**（启动与热加载 reload 均执行；失败启动放弃、reload 回退旧 VM）：

- `api_ver` > 宿主当前（见兼容矩阵）→ 拒载，提示"请升级宿主"；< 宿主最低 → 拒载，提示"请升级脚本"。
- **仅当显式声明 `@role`** 时强制角色契约（显式声明 = 作者签署契约 → 严格校验）：
  `collector` → 全局 `collect` 必需；`mqtt-north` → `auth`+`up` 必需
  （down/will/on_connack/on_disconnect 可选）；`transform` → `on_timer`/`on_rtdata`
  **any-of**（至少声明其一，on_init 恒可选）。无 manifest 头的存量脚本豁免——
  可能只定义 `on_data`，向后兼容放行。未知角色名拒载。
- `@stdlib` 范围 vs 本地 index 版本，不满足提示"请 sync"；未 sync（种子兜底态）跳过比对——
  内网冷启动不拒载。


## 编写约束（现场实测提示）

- **collector 脚本顶层不可用 `ctx`**：ctx 在 register 前于 load_main 之后才可用（luaNorth 相反——register 先于 load_main，顶层可用 ctx/cfg）。collector 需要初始化状态时在 `on_init` 里存全局变量（现场实测：顶层 `ctx:now()` 报 index nil）。
- **serial 对端须真实可达**：串口对端接错/无人监听时，单次 request 会按脚本超时干净返回 nil（warn 可感知）；但底层串口句柄在驱动层异常场景可能 wedged（stop 后端口被占需重启进程——已登记专项改造）。

## 配置（`app_config`）

| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `script` | str | 必填 | 脚本目录名（→ `scripts/<script>/main.lua`，仅 `[A-Za-z0-9_-]`） |
| `kind` | str | `tcp` | 传输：`tcp` / `serial` / `udp` / `mqtt`（透传） |
| `endpoint` | str | tcp/udp/mqtt 必填 | `ip:port`（serial 时忽略；mqtt = broker 地址） |
| `req_topic` | str | mqtt 必填 | mqtt 透传：`ctx:request` 发布请求帧的 topic |
| `resp_topic` | str | mqtt 必填 | mqtt 透传：在飞 request 期间收到的首帧即响应 |
| `passive_topics` | [str] | mqtt 可选 | mqtt 透传被动帧 topics（无在飞 request 时收到 → `on_data`） |
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
| `callback_timeout_ms` | num | `10000` | 单次回调（collect/on_write/on_data/**on_init**）时长预算（毫秒，见护栏） |
| `fail_threshold` | num | `10` | 连续 collect 失败 N 次标记 degraded |

## mqtt-north 角色（北向 `luaNorth` 实例）

阶段 B 起脚本系统支持双角色：`luaClient`（南向 collector，上文）与 `luaNorth`
（北向 **mqtt-north Sink**——收拢批量数据发布到 MQTT 平台/裸 broker）。同一套沙箱、
ctx API（L2 数据/L4 kv+secret/L5 HTTP/L6 工具全量；角色裁剪掉 C1 `ctx:request` 与
`ctx:transport()`——MQTT 连接层在宿主 rumqttc，脚本拿不到连接对象）、manifest/热
加载/stdlib require 机制。设计见
`docs/designs/2026-08-18-lua-script-system-evolution-design.md` §5。

### 回调契约（六回调）

main.lua 头部声明 `-- @role mqtt-north`（dry-run 校验 `auth`+`up` 必需）：

| 函数 | 必需 | 调用时机 | 返回 |
|---|---|---|---|
| `auth(cfg)` | ✅ | **每次**连接/重连前（Q5：重连必重调——token 型认证刷新的生效点） | `{client_id?, username?, password?, keepalive?}`（覆盖 NorthCfg 对应项）或 nil（cfg 缺省生效）；password 支持 `secret:` 引用 |
| `up(batch)` | ✅ | 批量周期（满 `batch_max_count` 条或 `batch_max_secs` 秒先到先触发；断连期跳过、数据留缓冲重连后补发） | `{ {topic=str, payload=str, qos=0\|1\|2?}, ... }` 发布数组（或 nil） |
| `down(topic, payload)` | 可选 | `subscribe` 订阅 topic 收到下行 | 忽略（命令处理/落 kv 由脚本自定） |
| `will()` | 可选 | 每次连接前组 LWT（遗嘱） | `{topic, payload, qos?, retain?}` 或 nil（无遗嘱） |
| `on_connack()` | 可选 | 连接成功后（宿主随后自动订阅 `cfg.subscribe`） | 忽略 |
| `on_disconnect()` | 可选 | 断连后（退避 `period` 秒重连，重连重调 auth） | 忽略 |

- `batch` 是数组：`{{devid="...", ts=<来源时间戳>, values={tag=v, ...}}, ...}`
  （lux `rtdata:*` 快照 + 实时样本合并，一个设备一条）；`up` 返回的坏条目
  （topic/payload 非 string）计 `publish_fail` 跳过不中断。
- 全局 `cfg` 表（`auth(cfg)` 同参）：NorthCfg 有效子集
  （`hwid/script/host/port/username/password/client_id/batch_max_count/batch_max_secs/
  period/subscribe`；password 的 `secret:` 引用保持原样，连接前一刻才解析）。

```lua
-- @role mqtt-north
-- @stdlib >=1.2
local onenet = require("cloud.onenet")
local json = require("json")

function auth(cfg)
  return onenet.auth_builder({
    product_id = "123456",
    device_name = "dev01",
    access_key = ctx:secret_get("secret:onenet_key"),  -- 授权清单内引用
    et = 2000000000,                                     -- 过期 unix 秒
  })
end

function will() return { topic = "offline/dev01", payload = "gone" } end

function up(batch)
  local posts, pid, dev = {}, "123456", "dev01"
  for _, e in ipairs(batch) do
    -- OneJSON 属性上报：params 每点嵌套 {value=...}
    local params = {}
    for tag, v in pairs(e.values) do params[tag] = { value = v } end
    posts[#posts+1] = {
      topic = onenet.topic_property_post(pid, dev),
      payload = json.encode({ id = tostring(e.ts), version = "1.0", params = params }),
    }
  end
  return posts
end

function down(topic, payload)
  local m = onenet.parse_property_set(topic, payload)
  if m then ctx:kv_set("last_set", json.encode(m.params)) end
end
```

### 配置（NorthCfg，`app_config`）

`app_code="luaNorth"`（`app_type` North）。**无传输项**（kind/endpoint/serial_*——
MQTT 连接在宿主）：

| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `script` | str | 必填 | 脚本目录名（校验同 collector） |
| `host` / `port` | str / num | 必填 / `1883` | broker 地址 |
| `username` / `password` | str | — | 凭证缺省项（auth 返回优先）；password 支持 `secret:<ref>` |
| `client_id` | str | `hwid-<inst_id>` | auth 返回优先 |
| `period` | num | `5` | 断连重连退避秒数 |
| `batch_max_count` / `batch_max_secs` | num | `50` / 取 `period` | 混合批量触发（先到先发） |
| `subscribe` | str[] | `[]` | ConnAck 后订阅的 topic 列表（`down` 数据源） |
| `secrets` | str[] | `[]` | `ctx:secret_get` 授权清单（同 collector） |
| `callback_timeout_ms` | num | `10000` | 单回调预算（auth/up/down/will/on_*） |
| `mem_limit_mb` / `hook_instr_limit` | num | `32` / `1000000` | 沙箱参数（见下提示） |

### secret 引用（平台凭证不落脚本/配置）

```bash
# 管理面写入 lux secrets 表（REST，见 rest-api.md §4.11；写入/删除有审计日志）
curl -X POST -H "Authorization: <token>" -H 'Content-Type: application/json' \
  -d '{"ref":"secret:onenet_key","value":"<平台分配 access_key>"}' \
  localhost:8790/api/v1/secret/set
```

两条独立通路，规则不同：

- **脚本侧** `ctx:secret_get(ref)`：受实例 `app_config.secrets` 授权清单约束——
  清单外引用**报错**（可感知）；清单内但未写入返回 `nil`（补写后下一轮 auth 即救活）。
- **宿主侧** password `secret:<ref>`（NorthCfg.password 或 auth 返回的 password）：
  连接配置的一部分，**不受授权清单约束**（清单管脚本侧读取）；连接序列解析时未写
  按"认证失败"处理退避重试，不静默改无密码连接。

### hook_instr_limit 提示（OneNET 接入必读）

OneNET token 签名含一次纯 Lua hmac_sha1（实测 ~70 万条 VM 指令，叠加脚本自身
逻辑会逼近默认 1M 指令护栏；护栏在同一 VM 内跨回调**累计**）。超限 auth 回调被
切断 → 实例反复重连失败。**OneNET 接入实例配置 `hook_instr_limit ≥ 2000000`**
（CI 以 4M 验证全量 selftest）。

### 兼容默认实例（T8 迁移）

存量 `[mqtt]`（mqttPub）配置在启动时**无损迁移**为默认 luaNorth 实例
`mqtt-pub-default`（内置 mqttPub 兼容脚本：每设备一条
`<hwid>/<devid>/datas` topic、payload `{hwid, devid, values, timestamp}`、qos 0）。
R5 单通道锁：`[mqtt]` 与 luaNorth 不双通道并存（默认实例已在运行则跳过二次启动）；
`cfg.mqtt.host` 留空 = 显式关闭不生成。迁移细节见 stage-b 设计 §7。

## transform 角色（南向 `luaTransform` 实例）

阶段 C 起脚本系统支持第三角色：`luaTransform`（**Source 订阅型**——与 collector
同为 South 实例，但输入不是通讯链路而是 **EventBus 实时数据**）。同一套沙箱、
manifest/热加载/stdlib require 机制；聚焦**数据加工**：清洗（野点剔除/限幅）、聚合
（滑动窗口均值）、派生点产出。输出经 `ctx:set_value` → hub → pipeline → lux
`rtdata:<devid>`——与 collector 同出口，前端/北向零差别消费派生点。

设计约束（ADR D1）：transform 不进 pipeline Processor（`Sample` 结构不动是硬约束），
做成订阅型实例——EventBus 的 Rtdata 事件（带值，pipeline 攒批后按 dev 聚合发布）
经宿主 `subscribe` 模式过滤后逐点投给脚本。

### 回调契约（三回调，on_timer/on_rtdata any-of）

main.lua 头部声明 `-- @role transform`（dry-run 校验 `on_timer`/`on_rtdata` 至少其一，
两者皆无拒载——纯定时与纯订阅都合法，啥都不做不合法）：

| 函数 | 必需 | 调用时机 | 返回 |
|---|---|---|---|
| `on_init()` | 可选 | 启动时一次（reload 后新 VM 同样调） | 忽略 |
| `on_timer()` | any-of | 每 `period` 秒（`period=0` 不调——纯订阅模式） | 忽略 |
| `on_rtdata(devid, tag, value)` | any-of | 订阅命中一个数据点一次（批量事件由宿主拆点逐调；`value` 为 Lua 标量 int/float/bool/string） | 忽略 |

- 每次回调独立包 `callback_timeout_ms` 预算（默认 10s）：`on_rtdata` 单点超预算/出错
  → warn + `rtdata_skipped` 计数 + **跳过该点继续**（一点坏不弃整批）；`on_timer`
  超限走 tick_fail → 连续失败 `degraded`（与 collector 同语义）。
- **顶层可用 `ctx`**（register 先于 load_main，同 luaNorth；区别于 collector——collector
  顶层不可用 ctx，初始化须在 `on_init`）。
- 非 Rtdata 事件（State/Log 等同总线流经）忽略；EventBus lagged（慢消费覆写旧值）
  → debug 日志继续收（broadcast 尽量处理语义）。

### 能力包（§5.2 矩阵 transform 列）

| 能力 | 注入 | 说明 |
|---|---|---|
| C0 json | ✓ | `require("json")` |
| C2 数据 | ✓ | `set_value`/`set_values`/`get_value`/`get_device`/`list_devices` |
| C4 持久 | ✓ | `kv_set`/`kv_get`/`secret_get`（推荐滑窗/计数器状态的载体） |
| C6 工具 | ✓ | `log_*`/`now`/`time` |
| C1 通讯 / C3 控制 / C5 HTTP | **✗** | `request`/`transport`/`send_command`/`http_get` 不注册——transform 无链路无出站，数据进出全走宿主 |

### 配置（TransformCfg，`app_config`）

`app_code="luaTransform"`（`app_type` South）。**无传输项**（kind/endpoint/serial_*）：

| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `script` | str | 必填 | 脚本目录名（校验同 collector，仅 `[A-Za-z0-9_-]`） |
| `period` | num | `3` | on_timer 周期秒；**0 = 纯订阅**（不调 on_timer、不占节拍） |
| `subscribe` | str[] | `[]` | 订阅模式清单（见下）；**空 = 纯定时**（不订 EventBus） |
| `callback_timeout_ms` | num | `10000` | 单回调预算（on_init/on_timer/on_rtdata） |
| `scripts_root` | str | `scripts` | 脚本根目录（per-instance 覆盖，多用于测试隔离） |
| `mem_limit_mb` / `hook_instr_limit` | num | `32` / `1000000` | 沙箱参数（同 collector） |

### subscribe 订阅语法（Q6 宿主过滤）

`subscribe` 是 transform 的**数据面入口**——模式在宿主侧匹配，不命中的点根本不进
脚本（脚本不用再判来源）。每条模式两段，以 `.` 分隔：

| 模式 | 语义 |
|---|---|
| `"dev1"` | dev1 的**全部 tag** |
| `"dev2.temp*"` | dev2 的 temp 开头 tag（tag 段仅 `*` 通配，无 `?`；中缀 `t*p` 亦可） |
| `"*"` | 全部设备（dev 段整段通配） |
| `"dev.*"` | 等价 `"dev"`（tag 段 `*` 全匹配） |

多模式 any-of（清单内任一命中即投递）。**非法 pattern 启动拒载**（坏声明可见——
订阅写错 = 数据面静默丢失，必须点名）：空串、`.`、`..`、超 2 段（`a.b.c`）、空段
（`.tag` / `dev.`）、dev 段部分通配（`dev*`——dev 段只支持精确名或整段 `*`）。

> **devid 命名注意**：模式以 `.` 分段，**devid 本身不要含 `.`**（如 `dev.raw` 无法用
> 模式表达——`dev.raw.temp` 会被拆三段拒载）。

### 自环防护与跨实例环（Q6）

- **自环（宿主硬保证）**：实例 `ctx:set_value` 写出的点，经 hub → pipeline →
  publish Rtdata 回流 EventBus 后，**不触发自身的 `on_rtdata`**——api 层 per-instance
  self-write 记录 + 10s 窗口过滤（`SELF_LOOP_GUARD_WINDOW`）。脚本订阅自身输出 tag
  是安全的（不产生回调放大）。
- **已知取舍**：窗口内**其他源**对同 `(devid,tag)` 的并发写入会被一并静默——防护
  只保证「自身输出不触发自身」，跨源对同 tag 的可见性不受保证（高频同 tag 双写场景
  需知晓；正常场景 collector 写原始点、transform 写派生点不同 tag，无影响）。
- **跨实例环（配置责任）**：A 订阅 B 的输出、B 订阅 A 的输出会形成互触环——宿主
  **不做静态消除**（Q6 定案），由配置者保证派生关系是 DAG（派生点用独立 devid/tag
  命名是最简单防法）。环异常表现为回调频率异常升高，可经 `/inst/stats` 的
  `rtdata_points` 增速与 EventBus lagged 观测；异常频率告警依赖 Q12 告警模型（D 阶段）。
- reload（文件 watcher 或控制命令）只换 VM：**订阅模式不重评估**（模式是实例配置级，
  改 `subscribe` 须 stop/start 实例）。

### 示例：agg-filter（滑动窗口均值 + 野点剔除）

`examples/lua-scripts/agg-filter/main.lua`（完整可跑，e2e 实测见
`tests/lua_transform_e2e.rs::agg_filter_example_end_to_end`）：

```lua
-- @role transform
-- @api_ver 1
-- @param window number 可选 滑动窗口点数 默认5 范围:1-100（kv 键 cfg_window 优先，reload 生效）
function on_rtdata(devid, tag, value)
  if value < CLAMP_LO or value > CLAMP_HI then  -- 限幅：野点剔除（不进窗不产出）
    ctx:kv_set("rejected", (ctx:kv_get("rejected") or 0) + 1)
    return
  end
  local win = ctx:kv_get("win") or {}           -- 滑动窗口状态存 kv（C4：跨重启/热加载不丢）
  win[#win + 1] = value
  while #win > WINDOW do table.remove(win, 1) end
  ctx:kv_set("win", win)
  local sum = 0
  for _, v in ipairs(win) do sum = sum + v end
  ctx:set_value(OUT_DEV, OUT_TAG, sum / #win)   -- 派生点 → hub → pipeline 全链路
end
```

实例配置：`{"script": "agg-filter", "period": 0, "subscribe": ["raw1.temp*"]}`。
窗口大小默认 5，写 kv `cfg_window` 后 reload 生效（on_init 读取——窗口可配演示）。

### 状态监测扩展

stat 走 InstanceManager 全套（核心 6 字段 + `script_version`/`vm_restart_count`），
transform 专属 extra：`rtdata_points`（on_rtdata 成功回调累计）、`rtdata_skipped`
（超预算/出错/自环过滤跳过累计）、`runtime_devices`（派生点上报集）、`dropped_samples`。

## 运行时护栏

脚本错误不应拖垮实例或宿主——五项运行时护栏（参数与能力卡单一事实源一致）：

| 护栏 | 参数 / 默认 | 超限行为 |
|---|---|---|
| 回调时长预算 | `callback_timeout_ms`（默认 10s，可配；覆盖 collect/on_write/on_data/on_init 四类回调） | 本次回调被放弃（协程弃留，VM 不重建）、tick 记失败——连续超时照常走 `fail_threshold` → `degraded`，下一 tick 继续；reload 路径的 on_init 超限 → 回退旧 VM |
| HTTP 频控 | 20 次/s（`http_get`/`http_post` 共享一个出站窗口；**固定 1s 窗口计数**语义，窗口边界理论可突发 2×——护栏目的等效，token bucket 列为后续优化） | 向脚本返回错误 `rate limited（20/s）`（可感知退避） |
| `ctx:request` 超时上限 | 脚本传值 clamp 至 30s（`0` → 1ms；上限读能力卡 `timeout_cap_ms`） | 防超大超时挂死等待 |
| HTTP 超时上限 | `timeout` 参数 clamp 至 1-30s（缺省 5s；上限读能力卡） | 同上 |
| hub 背压防反压 | 样本上送 `send` 超 2s；同一 collect 周期首批丢弃后，剩余样本改即时丢弃（不再逐条各等 2s） | 丢弃该样本并累计计数——stat 扩展字段 `dropped_samples`（见状态监测）；hub 关闭时 warn |
| on_data 帧通道防反压 | 被动帧通道（容量 64）满时即时丢帧（不等待） | 丢弃并累计计数——stat 扩展字段 `dropped_frames`；主循环恢复消费后自愈 |

死循环由指令 hook 熔断（`hook_instr_limit`，见沙箱限制）、内存由 `mem_limit_mb` 封顶，与上表
互补构成完整护栏面。

## 通讯链路抽象（必读）

脚本作者须先理解 lua 采集的链路分工模型：

- **链路由 rs-iot 管，脚本不碰**：open/connect/收发/重连/报文 trace 全在宿主 IO task（`io_engine`）。
  脚本只经 `ctx:request(frame, timeout)` 组帧/解帧——**拿不到也不持有任何连接对象**。
- **一个实例一条链路**：`io_engine` 单 backend（tcp/serial/udp/mqtt 四选一）。不同厂商协议 / 不同串口参数 /
  不同物理接口 → 必须不同实例（不同脚本，或同脚本多实例配不同 `serial_port`/`endpoint`）。
- **同一条链路多从站**（RS485/RS485-like 总线，同协议同参数不同 unit id）：脚本在 `collect()` 里
  循环组不同 uid 帧轮询，经同一个 `ctx:request` 发收；M2 多设备输出（`collect` 嵌套返回 `{[devid]={tag=v}}`）。
- **重连自愈内置**：连接失败/读写 err/对端关闭 → 宿主置 `None`，下次 `ctx:request` 自动重连/重 open。
  重连频率受 collect period 天然限速，**脚本无需写任何重连逻辑**。
- **感知链路类型**：脚本调 `ctx:transport()`（见 L1 通讯）得知当前链路，据此选帧格式（RTU+CRC / MBAP）。

## 传输

- **tcp**（`kind="tcp"`，`endpoint="ip:port"`）：独占 `TcpStream`，懒连接 + 断连重连。**TCP client** 角色。
- **serial**（`kind="serial"`，`serial_port=...`）：tokio-serial，RTU 帧走串口（Linux 需 libudev-dev）。
  串口直连（RS485 主站 / RS232 点对点）。
- **udp**（`kind="udp"`，`endpoint="ip:port"`）：connected `UdpSocket`，datagram 边界天然保帧。**UDP client** 角色。
- **mqtt 透传**（`kind="mqtt"`，`endpoint="broker的ip:port"` + `req_topic`/`resp_topic` 必填 +
  `passive_topics` 可选，阶段 B T11）：设备侧是 MQTT 透传 DTU/模组的场景——rs-iot 作 MQTT client
  连 broker，`ctx:request` 的请求帧发布到 `req_topic`，**等待期间收到的第一帧 Publish（`resp_topic`
  订阅）即响应**；无在飞 request 时收到的 Publish（`passive_topics` 订阅）即被动帧 → `on_data`。
  连接/重连/订阅由宿主 EventLoop pump 管（每次连上自动重订阅全集合，clean session 不依赖 broker
  会话恢复），断连期间 request 立即报错。当前 topic 为固定字符串（`{devid}` 等占位符模板后置）。

四种传输统一 `ctx:request`（IO task 独占处理，spec §8 模式 Z）。`recv_mode` 控被动帧成帧
（`on_data`）：当前 `timeout`（idle 静默）；length/delimiter/fixed 后置。

**mqtt 透传注意事项**：

- 请求/响应分路按「当前是否有在飞 request」判定——resp 首帧即响应意味着**设备须在请求期间才推
  `resp_topic`**；周期性状态推送应走 `passive_topics`（on_data），不要混入 resp 通道。
- QoS1 断连补发可能迟到：`ctx:request` 超时放弃后，设备的响应帧仍可能在下一个 request 在飞期间
  补达——分路会把它当作当前请求的响应投递。**设备/脚本侧须幂等或带序号校验**（帧内嵌流水号比对）。

### 链路角色：当前支持 / 后置

当前四种传输都是 **client / 主站**侧（脚本主动发请求、等响应；mqtt 为透传 client）。

**TCP Server / UDP Server 本阶段不支持**——rs-iot 做监听服务器、等网络客户端连入的场景。原因：
`ctx:request` 的"一问一答"模型与 server 的"被动收多客户端"根本冲突。现实里此类场景的约束：

- **Modbus over net 类**（如 rs-iot 作 Modbus TCP slave）：Modbus 协议要求**只允许单连接**，仍可套用
  请求-响应模型，但需 accept 单连接 + 客户端断开后重新 listen。
- **特殊推送类协议**：网络中**多客户端同时推送实时数据集**（如多台仪表主动上报），需多客户端管理
  + `on_data` 被动帧按客户端路由——超出当前单连接 `IoBackend` trait 能力。

后期按需评估实现：需新 IO 模型（accept 循环 + 客户端表 + 被动帧路由），属架构级扩展，非加一个
backend 可解决。触发条件：现场出现明确的 server 侧协议需求。

## 示例

`examples/lua-scripts/`：

- `modbus-like/`：Modbus TCP 协议（MBAP + FC03 读 / FC06 写），一问一答（`collect` + `on_write`）。
  头部带 manifest 声明，CRC16 用 `require("algo.crc16")`（官方 stdlib，见上）。
- `custom-on-data/`：自定义协议 + `on_data` 被动广播上报（`set_value` + `kv_set` 持久化）。
- `multi-device/`：单实例多设备（一条 RS485 总线多从站），`collect` 嵌套返回（M2）。
- `agg-filter/`：transform 角色完整示例——订阅原始点 → 野点剔除 + 滑动窗口均值 →
  `set_value` 写派生点；窗口状态 kv 持久、窗口大小 kv 可配（见「transform 角色」章）。

## 沙箱限制

脚本在受限 mlua VM 运行（spec §9）：

- **禁库**：`io` / `os` / `debug` / `package`（不可文件 IO / 执行命令 / require 任意模块）。
- **指令 hook**：超阈值（`hook_instr_limit`）中断死循环。
- **内存限额**：`mem_limit_mb` 上限，超则 OOM 中断。
- **不暴露**：lux client / 文件系统 / 原始 socket（通讯仅经 `ctx:request`）。
- **受控 require**：`require` 按首段命名空间路由四层（见「require 与模块库」），只读约定目录；
  单段私有模块解析 `scripts/<script>/lib/<名>.lua`（防 `..` 穿越仓库）。

## 热加载

改 `scripts/<name>/main.lua` → notify watcher（去抖 200ms）→ VM 重建（新 load + register +
dry-run：manifest 契约校验 + `on_init` 验证）。失败回退旧 VM（永不半生效）。

共享库另有**全局 watcher**：`scripts/_lib/` 变更（去抖 200ms）→ 全实例 **VM 级 reload**
（stdlib sync 后的批量 reload 同路径：运行中实例逐一经控制通道触发 `reload_vm`，失败回退旧 VM，
非运行态实例跳过不复活）。

兜底：`POST /api/v1/lua/reload?inst_id=<id>`（单实例显式操作，stop + start，IO 重连）。

**远程 reload**：MQTT `<hwid>/command` 主题发布 `{"instance":"<lua实例>","cmd":"reload"}` 同样
触发该实例的 **VM 级热加载**（经控制通道走 `Evt::Reload`，失败自动回退旧 VM）——与 JWT 保护的
REST reload 等价能力，授权面 = broker 命令 topic 的发布权（运维按需收敛 topic ACL）。

> **scripts 根说明**：REST 保存/读取、`_lib` watcher 与 stdlib sync 固定使用进程工作目录下的
> `scripts/`；实例可用 `app_config.scripts_root` 覆盖自己的脚本根（多见于测试）——覆盖根的实例
> 不在 REST save 的热加载/`_lib` watcher 覆盖范围内（沿袭既有约定）。

## 状态监测（M4 统一埋点）

lux `stat:<inst>` 存 `InstanceStat` JSON（`StatTracker` 周期 flush，每 collect 周期更新）。**核心 6 字段所有插件一致**：

- `status`：`running` / `degraded`（连续失败 ≥ 阈值）/ `stopped`。
- `tick_count` / `fail_count`：活动成功/失败累计计数。
- `last_activity_ts` / `last_activity_duration_ms`：最近活动时间戳与耗时。
- `last_err`：最近失败原因（成功时清空）。

lua 专有扩展字段经 `extra` 透传：`vm_restart_count`（VM 重建次数）、`script_version`（main.lua sha256 前 8 hex）、`consecutive_fail`、`dropped_samples`（hub 背压累计丢样数，见护栏）、`dropped_frames`（on_data 通道满累计丢帧数，见护栏）。

REST：`GET /api/v1/inst/stats?inst_id=<id>`（读 stat，M4 统一端点，**旧 `GET /api/v1/lua/stats` 已废弃移除**）/ `GET /api/v1/lua/scripts`（列脚本）。埋点约定详见 [`instrumentation-conventions.md`](../conventions/instrumentation-conventions.md)。

## 报文 trace（P1 #2）

lua 实例收发报文可 per-instance 按需激活 hex dump（裸报文调试）：

```bash
# 激活 60s
curl -X POST -H "Authorization: <token>" -H 'Content-Type: application/json' \
  -d '{"inst_id":"<id>","duration_secs":60}' localhost:8790/api/v1/inst/trace
# 拉报文 hex（经日志 ring buffer，target=io_trace）
curl -H "Authorization: <token>" "localhost:8790/api/v1/logs/recent?target=io_trace&limit=10"
```

激活后该实例 `ctx:request` 的 request TX/RX + `recv_idle` 被动 RX **三点**字节进 tracing target `io_trace`（抓包风格 hex）。**按设计只进内存 ring buffer（2000 条/实例，不落盘）**，惰性超时关闭。lua 走 io_engine direct dump（frame 已在手）；modbus 走 `TraceIo<T>` 拦截层。

## 兼容矩阵

脚本契约版本协商面（manifest `@api_ver`/`@stdlib` 与之对应；能力卡是单一事实源）：

| 宿主版本 | API_VER（MIN / CUR） | STDLIB | builtin 模块 | 实例类型（app_code） |
|---|---|---|---|---|
| 阶段 A | 1 / 1 | 种子 4 库（freeioe@1398d47 vendored，随宿主内嵌、无独立版本轴）；官方源 index 首发版本 **1.0.0**（sync 后本地 index 为准） | `json` | luaClient |
| 阶段 B | 1 / 1 | 种子不变；官方源在线分发：**1.1.0**（`algo.hashings` 子集 uintn/md5/sha1/sha256/hmac + `cloud.emqx`）→ **1.2.0**（+`cloud.onenet` token 配方）。sync 后本地 index 为准 | `json` | luaClient、luaNorth |
| 阶段 C（本版） | 1 / 1 | 不变 | `json` | luaClient、luaNorth、**luaTransform**（`@role transform`，on_timer/on_rtdata any-of） |

- 脚本 `@api_ver 1`（或缺省）在本版通过；宿主升级 CUR 后旧脚本不受影响（MIN 锁下界）。
- `@stdlib >=1.0`：种子兜底态跳过比对（冷启动不拒载）；sync 后 `>=1.0 <2` 区间均满足
  （1.1.0/1.2.0 亦然）——用 `cloud.onenet`/hashings 的脚本声明 `@stdlib >=1.1`/`>=1.2`
  可获得"未 sync 即拒载"的显式闸门。
- hashings/cloud 均不进种子：新装宿主未 sync 前 `require` 报"未同步"（可行动错误，
  按上文 sync 流程安装）。
