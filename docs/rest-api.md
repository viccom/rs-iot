# rs-iot REST API 文档

> 管理 API（axum）。默认端口 **8780**（`config.rs` 默认值；仓库 `config.toml` 改为 **8790** 避让本机占用，下文示例统一用 8790）。
> 鉴权：JWT（24h）+ `X-Access-Key` + 白名单。共 34 条路由（32 业务 + `/apitest` + `/`）。
> 所有端点前缀 `/api/v1`；请求/响应体均为 JSON（`Content-Type: application/json`）。

## 1. 总览

| 项 | 值 |
|---|---|
| 框架 | axum 0.8（`src/api/mod.rs::build_router`） |
| 监听 | `0.0.0.0:{api_port}` |
| 业务前缀 | `/api/v1/...` |
| 持久层 | lux 嵌入式 KV（Hash: `users`/`passwords`/`instances`/`devices`/`groups`；string: `accesskey`；Hash: `rtdata:<devId>`；string: `stat:<inst>`；Hash: `ext_tokens`） |
| 鉴权中间件 | 全局 `auth::auth_middleware`（`from_fn_with_state`）+ `tower_http::TraceLayer` |

**错误响应格式（当前不统一，调用方需兼容两种）**：
- 多数错误：HTTP 状态码 + 纯文本 body，如 `(404, "instance not running")`
- 实例配置错误：JSON `{"status":"error","msg":"invalid instance config: ..."}`
- 成功：JSON `{"status":"ok"}`（少数 `{"status":"deleted"}`）

## 2. 鉴权

中间件按顺序判定，**任一通过即放行**：

1. **白名单**：`/api/v1/user/login`、`/api/v1/user/register`（注：`register` 路由**当前未注册**，白名单项悬空）、`/apitest*`
2. **`X-Access-Key` 直通**：请求头 `x-access-key` 与 lux `GET accesskey` 的值非空且相等 → 放行（机器对机器，对标 Lite_IOT `globalAccessKey`）
3. **JWT**：`Authorization: <裸 token>`（**不剥离 `Bearer ` 前缀**，客户端直接传 token 字符串）或 Cookie `token=<jwt>`，HS256 校验

**JWT 要点**：
- 签发：`POST /api/v1/user/login` 校验 bcrypt 密码 → 返回 `token`（body）+ `Set-Cookie: token=...; HttpOnly; Max-Age=86400`（24h）
- Claims：`{ sub: userId, exp }`
- 失效：自然过期（24h）；`logout` 仅清 cookie，**服务端无吊销/黑名单**
- **当前无授权层（仅认证）**：中间件解析出 `userId` 但未注入上下文，任何持 token/accesskey 者可访问全部资源，`groups` 数据不参与权限判定。详见 [API 完备性评估](api-completeness-assessment.md)

## 3. 快速上手

```bash
# 1) 登录拿 JWT（admin/admin 由 identity::bootstrap 幂等建）
TOKEN=$(curl -s -XPOST http://localhost:8790/api/v1/user/login \
  -H 'Content-Type: application/json' \
  -d '{"user_id":"admin","pass":"admin"}' | jq -r .token)

# 2) 带 token 调 API（注意：Authorization 传裸 token，不加 Bearer）
curl -s -H "Authorization: $TOKEN" http://localhost:8790/api/v1/sysinfo
curl -s -H "Authorization: $TOKEN" http://localhost:8790/api/v1/rtdatas           # 全量实时数据
curl -s -H "Authorization: $TOKEN" "http://localhost:8790/api/v1/rtdatas?dev_id=devA"  # 单设备

# 3) 建实例 → 启动 → 下写命令
curl -s -XPOST -H "Authorization: $TOKEN" -H 'Content-Type: application/json' \
  http://localhost:8790/api/v1/inst/set -d '{...见 4.2...}'
curl -s -XPOST -H "Authorization: $TOKEN" -H 'Content-Type: application/json' \
  http://localhost:8790/api/v1/startinst -d '{"inst_id":"mb1"}'
curl -s -XPOST -H "Authorization: $TOKEN" -H 'Content-Type: application/json' \
  http://localhost:8790/api/v1/command/send \
  -d '{"instance":"mb1","devid":"devA","cmd":"write","data":"{\"address\":\"holding:0:f32:ABCD\",\"value\":12.5}"}'
```

## 4. 端点详解

> 鉴权列：`公开` / `JWT`（JWT 与 AccessKey 均可通过，表中标主路径）；除注明公开外全部需 JWT 或 AccessKey。

### 4.1 系统 / 实例启停（`routes_sys.rs`）

| Method | Path | Body/Query | 成功响应 |
|---|---|---|---|
| GET | `/api/v1/sysinfo` | — | `{"hwid":"...","version":"..."}` |
| GET | `/api/v1/sysstatus` | — | `{"status":"ok","dbsize":<int>,"instances":{<inst>:<state>}}` |
| GET | `/api/v1/plugins` | — | `{"sources":[...],"sinks":[...]}` |
| GET | `/api/v1/inststatus` | — | `{<inst>:<state>}` |
| POST | `/api/v1/startinst` | `{"inst_id":"..."}` | `{"status":"ok"}`；不存在 404；配置非法 400；非南向 400 |
| POST | `/api/v1/stopinst` | `{"inst_id":"..."}` | `{"status":"ok"}` |

- `<state>` ∈ `starting`/`running`/`stopping`/`stopped`/`failed`（注：`failed` 当前代码未写入）
- `startinst` 仅支持**南向 Source**（拒绝北向 `app_type=north`）

### 4.2 实例 CRUD（`routes_inst.rs`，lux Hash `instances`）

| Method | Path | Body/Query | 成功响应 |
|---|---|---|---|
| GET | `/api/v1/insts` | — | `{<inst_id>:<InstConfig>}` |
| GET | `/api/v1/inst/get` | `?inst_id=` | `{<inst_id>:<InstConfig>}`，无则 `{}` |
| POST | `/api/v1/inst/set` | `{"inst_id":"...","object":<InstConfig>}` | `{"status":"ok"}`；外部插件额外 `{"status":"ok","plugin_token":"<hex>"}`；非法配置 `{"status":"error","msg":...}` |
| POST | `/api/v1/inst/delete` | `{"inst_id":"..."}` | `{"status":"deleted"}` |

- `inst/set` 会**强校验** `object` 反序列化为 `InstConfig`（避免存入非法配置延迟到 startinst 才报错）
- **外部插件**（`host_kind:"external"`）：每次 `set` 调 `ext_token::issue` **覆盖旧 token**（旧 token 即失效），明文 `plugin_token` 仅此次返回（管理员交付外部进程）。重复 set 同实例 = 轮换 token
- `inst/delete` 连带清除外插件 token + 该实例全部 Lua kv（`luakv`）

**建普通采集实例示例**（modbus）：
```bash
curl -s -XPOST -H "Authorization: $TOKEN" -H 'Content-Type: application/json' \
  http://localhost:8790/api/v1/inst/set -d '{
    "inst_id":"mb1",
    "object":{
      "inst_name":"modbus 电表","app_code":"modbusClient","app_type":"south",
      "app_config":{"kind":"tcp","endpoint":"192.168.1.10:502","slave_id":1,"period":2},
      "devices":[{"label":"电表","value":"devA","children":[
        {"label":"电压","value":"holding:0:f32:ABCD","type":"float"}
      ]}]
    }
  }'
```

**建外部插件实例示例**（sidecarClient，返回 plugin_token）：
```bash
curl -s -XPOST -H "Authorization: $TOKEN" -H 'Content-Type: application/json' \
  http://localhost:8790/api/v1/inst/set -d '{
    "inst_id":"ext1",
    "object":{
      "inst_name":"python demo","app_code":"sidecarClient","app_type":"south",
      "host_kind":"external","app_config":{"heartbeat_timeout_secs":30},
      "devices":[{"label":"demo","value":"devP","children":[
        {"label":"温度","value":"temp","type":"float"}
      ]}]
    }
  }'   # → {"status":"ok","plugin_token":"<hex>"}
```

### 4.3 设备 CRUD（`routes_device.rs`，lux Hash `devices`）

| Method | Path | Body/Query | 成功响应 |
|---|---|---|---|
| GET | `/api/v1/devices` | — | `{<dev_id>:<DeviceSpec>}` |
| GET | `/api/v1/device/get` | `?dev_id=` | `{<dev_id>:<DeviceSpec>}`，无则 `{}` |
| POST | `/api/v1/device/set` | `{"dev_id":"...","object":<DeviceSpec>}` | `{"status":"ok"}` |
| POST | `/api/v1/device/delete` | `{"dev_id":"..."}` | `{"status":"deleted"}` |

- `device/set` **无 schema 校验**（裸 JSON 写入），建议仍按 `DeviceSpec` 结构写

### 4.4 用户 / 登录（`routes_user.rs`，lux Hash `users`/`passwords`）

| Method | Path | 鉴权 | Body/Query | 响应 |
|---|---|---|---|---|
| GET | `/api/v1/users` | JWT | — | `{<user_id>:<object>}` |
| GET | `/api/v1/user/get` | JWT | `?user_id=` | `{<user_id>:<object>}`，无则 `{}` |
| POST | `/api/v1/user/set` | JWT | `{"user_id":"...","object":<任意 JSON>}` | `{"status":"ok"}` |
| POST | `/api/v1/user/delete` | JWT | `{"user_id":"..."}` | `{"status":"deleted"}`（连清密码） |
| POST | `/api/v1/user/verify` | JWT | `{"user_id":"...","pass":"..."}` | `{"userId":"...","status":<bool>}` |
| POST | `/api/v1/user/changepass` | JWT | `{"user_id":"...","pass":"<旧>","new_pass":"<新>"}` | `{"status":"ok"}`；旧密码错 401 |
| GET | `/api/v1/user/checktoken` | JWT | 头 `Authorization:<token>` | `{"status":"ok","userId":"..."}`；失效 401 |
| POST | `/api/v1/user/login` | **公开** | `{"user_id":"...","pass":"..."}` | `{"status":"ok","token":"...","user":<obj>}` + Set-Cookie；账密错 401 |
| POST | `/api/v1/user/logout` | JWT | — | `{"status":"ok"}` + 清 cookie |

**重要限制**：
- `user/set` **不写密码**（密码独立存 `passwords` Hash）——新建用户无密码无法 login
- 无 `register` 端点（白名单悬空）；`changepass` 要求旧密码（新用户不可用）→ 首个账号只能靠 `identity::bootstrap` 种子（admin/admin），后续账号需带 token 调 `user/set` + 直写密码或先 login 再 changepass。详见 [API 完备性评估](api-completeness-assessment.md)

### 4.5 用户组 / AccessKey（`routes_group.rs`）

| Method | Path | Body/Query | 成功响应 |
|---|---|---|---|
| GET | `/api/v1/usergroups` | — | `{<group_id>:[<user_id>,...]}` |
| POST | `/api/v1/usergroup/add` | `{"user_id":"...","group_id":"..."}` | `{"status":"ok"}`（组不存在隐式创建） |
| POST | `/api/v1/usergroup/remove` | `{"user_id":"...","group_id":"..."}` | `{"status":"ok"}` |
| POST | `/api/v1/accesskey/set` | `{"key":"..."}` | `{"status":"ok"}` |

- 用户组数据**当前不参与任何权限判定**（无授权层）；AccessKey 全局单值，轮换即时生效

### 4.6 Lua 运维（`routes_lua.rs`）

| Method | Path | Body/Query | 响应 |
|---|---|---|---|
| GET | `/api/v1/lua/stats` | `?inst_id=` | lux `stat:<inst>` JSON（采集计数/耗时/degraded 等）；无则 `{}` |
| POST | `/api/v1/lua/reload` | `?inst_id=` | `{"status":"ok"}`；实例不存在 404；非 luaClient 400 |
| GET | `/api/v1/lua/scripts` | — | `{"scripts":["<目录名>",...]}` |

- `reload` 仅对 `app_code=="luaClient"` 生效（stop+start，IO 重连）；与文件 notify 自动热加载等价
- 仅 luaClient 有结构化运行指标；opcua/modbus/s7 暂无（见 [场景缺口评估](scenario-gap-assessment.md)）

### 4.7 写命令下发（`routes_command.rs`）

| Method | Path | Body | 响应 |
|---|---|---|---|
| POST | `/api/v1/command/send` | `{"instance":"<InstId>","devid":"...","cmd":"write","data":"<JSON 文本>"}` | `{"status":"ok"}`；实例未运行 404 |

- `data` 是**字符串化的 JSON**（注意转义），由具体插件 `handle_write` 解析：
  - opcua：`{"node_id":"ns=2;s=v1","value":123,"opc_type":"Int32"}`
  - modbus：`{"address":"holding:0:f32:ABCD","value":12.5}`
  - s7：`{"address":"DB1.DBD0","value":42.5,"kind":"f32"}`
- 与 MQTT `<hwid>/command` 共用同一条 `CtlRegistry` 写总线；**仅确认投递到 `cmd_rx`，无执行回执**（见完备性评估）

### 4.8 实时数据（`routes_rtdata.rs`，lux Hash `rtdata:<devId>`）

| Method | Path | Query | 响应 |
|---|---|---|---|
| GET | `/api/v1/rtdatas` | `?dev_id=`（可选） | 单设备 `{<field>:[ts,localTime,value]}`；不传 `{<devId>:{<field>:...}}` 全量 |

- 每个 field 值尝试 JSON 解析（失败保留原始字符串）
- **只读最新快照**，无历史/时间范围/分页/聚合（见完备性评估）

### 4.9 静态前端（`frontend.rs`，rust-embed）

| Method | Path | 鉴权 | 响应 |
|---|---|---|---|
| GET | `/apitest` | 公开 | 内嵌 apiTest 页面（`index.html`） |
| GET | `/` | JWT | 302 → `/apitest`（未带凭据访问根路径会得 401） |

## 5. 数据模型（`src/types.rs`）

```
InstConfig {
  inst_id: String,          // 实例 ID（rtdata key 组成部分）
  inst_name: String,        // 显示名
  app_code: String,         // 插件注册名：dataSim/opcuaClient/modbusClient/s7Client/luaClient/sidecarClient/mqttPub
  app_type: "south"|"north",
  auto_start: bool,         // 启动时自动拉起
  app_config: <任意 JSON>,   // 插件自解析（period/endpoint/...）
  devices: [DeviceSpec],
  host_kind: "builtin"|"external"  // 缺省 builtin
}
DeviceSpec { label, value:<设备ID>, children:[ChildSpec] }
ChildSpec  { label:<数据点名>, value:<协议地址>, type:<值类型> }
CtlMessage { instance:InstId, devid, cmd, data:<JSON 文本> }
```

- `app_code` 一经发布**锁定不改**（持久化在 lux `instances`，改名破坏已部署实例）
- 各插件 `app_config`/`ChildSpec.value`/`ChildSpec.type` 的语义见各插件文档（CLAUDE.md「验证规则」段）

## 6. 已知限制与完备性

当前 API 定位为**单管理员调试/运维**（对标 Lite_IOT）。要支撑「管理员 + 多用户前端系统」尚有缺口（无授权层、无用户自服务、无批量/分页/历史、命令无回执、无 healthz/metrics、错误格式不统一等），详见 [`docs/api-completeness-assessment.md`](api-completeness-assessment.md)。
