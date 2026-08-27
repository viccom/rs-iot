# rs-iot REST API 文档

> **状态**：活文档（API 变更同步）
> 管理 API（axum）。默认端口 **8790**（2026-08-26 用户裁决统一——`config.rs` 默认值、rsiot-mcp 缺省 `RSIOT_BASE`、rsiot-gateway 默认基址三处一致；下文示例同）。
> 鉴权：JWT（24h）+ `X-Access-Key` + 白名单。业务前缀 `/api/v1`；`/metrics` 为顶层路径（免 JWT，Prometheus 抓取）。
> 请求/响应体均为 JSON（`Content-Type: application/json`），唯 `/metrics` 返回 Prometheus 文本。

## 1. 总览

| 项 | 值 |
|---|---|
| 框架 | axum 0.8（`src/api/mod.rs::build_router`） |
| 监听 | `{api_bind_host}:{api_port}`（`api_bind_host` 缺省 `0.0.0.0` 保持既有行为，2026-08-26 审计 B3 增量键——与 lux/bridge bind_host 姿态对齐） |
| 业务前缀 | `/api/v1/...` |
| 持久层 | lux 嵌入式 KV（Hash: `users`/`passwords`/`instances`/`devices`(遗留)/`groups`/`secrets`/`alarms`/`devmodels`/`devreg`/`discoveries`；string: `accesskey`；Hash: `rtdata:<devId>`；string: `stat:<inst>`；Hash: `ext_tokens`。旧 `tags` 表已退役 M-4——被 devreg.overrides 吸收，遗留数据经 `/api/v1/tags/legacy-export` 导出） |
| 鉴权中间件 | 全局 `auth::auth_middleware`（`from_fn_with_state`）+ `tower_http::TraceLayer` |

**错误响应格式（当前不统一，调用方需兼容两种）**：
- 多数错误：HTTP 状态码 + 纯文本 body，如 `(404, "instance not running")`
- 实例配置错误：JSON `{"status":"error","msg":"invalid instance config: ..."}`
- 成功：JSON `{"status":"ok"}`（少数 `{"status":"deleted"}`）

## 2. 鉴权

中间件按顺序判定，**任一通过即放行**：

1. **白名单**：`/api/v1/user/login`、`/api/v1/user/register`（注：`register` 路由**当前未注册**，白名单项悬空）、`/metrics`（Prometheus 抓取，机器出口）、`/apitest*`
2. **`X-Access-Key` 直通**：请求头 `x-access-key` 与 lux `GET accesskey` 的值非空且相等 → 放行（机器对机器，对标 Lite_IOT `globalAccessKey`）
3. **JWT**：`Authorization: Bearer <jwt>` 或裸 `<jwt>`（中间件自动剥离可选的 `Bearer ` 前缀，`auth.rs` auth_middleware）或 Cookie `token=<jwt>`，HS256 校验

**JWT 要点**：
- 签发：`POST /api/v1/user/login` 校验 bcrypt 密码 → 返回 `token`（body）+ `Set-Cookie: token=...; HttpOnly; Max-Age=86400`（24h）
- Claims：`{ sub: userId, exp }`
- 失效：自然过期（24h）；`logout` 仅清 cookie，**服务端无吊销/黑名单**
- **当前无授权层（仅认证）**：中间件解析出 `userId` 但未注入上下文，任何持 token/accesskey 者可访问全部资源，`groups` 数据不参与权限判定。详见 API 完备性评估（`api-completeness-assessment.md`，已归档，git 历史 66945dd 可溯）

## 3. 快速上手

```bash
# 1) 登录拿 JWT（admin/admin 由 identity::bootstrap 幂等建）
TOKEN=$(curl -s -XPOST http://localhost:8790/api/v1/user/login \
  -H 'Content-Type: application/json' \
  -d '{"user_id":"admin","pass":"admin"}' | jq -r .token)

# 2) 带 token 调 API（Authorization 裸 token 与 "Bearer <token>" 两种写法均可，§2）
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
| GET | `/api/v1/sysstatus` | — | `{"status":"ok","dbsize":<int>,"instances":{<inst>:<state>},"history":{...}}`（history 块见下方 bullet） |
| GET | `/api/v1/plugins` | — | `{"sources":[...],"sinks":[...]}` |
| GET | `/api/v1/inststatus` | — | `{<inst>:<state>}` |
| POST | `/api/v1/startinst` | `{"inst_id":"..."}` | `{"status":"ok"}`；不存在 404；配置非法 400；非南向 400 |
| POST | `/api/v1/stopinst` | `{"inst_id":"..."}` | `{"status":"ok"}` |
| GET | `/api/v1/config` | — | 脱敏后的运行配置 JSON（`AppConfig` 全量结构） |

- `<state>` ∈ `starting`/`running`/`stopping`/`stopped`/`failed`。**`failed` 由 M3 supervisor 真实写入**：实例 task 异常退出（panic/配置错误早退/非正常 return）且未被主动 stop → 标 `failed` + `error!` 落盘；运维 `startinst` 可重拉（`failed → starting → running`）。主动 `stopinst` 走 `stopping → stopped`（token cancel）不误判
- `sysstatus` 的 `history` 块：`{"feature":<bool>,"enabled":<bool>,"engine":"ok"|"disabled"|"init_failed","stats":{...}|null}`——L1 feature 是否编入 / L2 `[history].enabled` 是否启用 / 引擎状态；语义见 §4.18。**engine=`ok` 时**（Phase 3 治理扩展）：顶层增 `memory_used_bytes`/`memory_budget_bytes`（tsink 引擎内存口径，预算=D6 `memory_limit_mb` 换算；`series` 为 `list_metrics().len()` 官方口径、**5s TTL 缓存**——高频轮询下数值至多滞后 5s）/`archived_tags`（effective 快照 history=true 计数）/`deadbands":{"evaluated":<int>,"ratio":<float>}`（ratio=deadbanded/evaluated，分母 0 时 0）；`stats` 扩为十四计数器 `{sent,written,dropped,deadbanded,deadband_evaluated,skipped,lane_mismatch,nonfinite_skipped,dup_ts_skipped,stale_ts_dropped,ts_rejected,len_rejected,errors,errors_by_kind}`——`deadband_evaluated`=死区评估行数（削减率分母），四拒收计数 `nonfinite_skipped`（NaN/Inf）/`ts_rejected`（时界窗 `[now−7d,+1h]` 外或溢出）/`len_rejected`（tag>64KiB/label>16KiB）/`dup_ts_skipped`（跨批同 ts）/`stale_ts_dropped`（跨批回卷更旧丢弃，语义见 §4.18 写路径要点）为 2026-08-26 审查修复新增，`errors_by_kind` 五键 `{memory_budget,cardinality,wal_disk,closed,other}`（按行分桶，Σkind == errors）。**engine 非 `ok` 时**这些新字段整体**省略**（非 null，与 `stats:null` 语义区分——省略=引擎不可用无数据，null=字段存在但无值），`stats` 保持 null
- `startinst` 仅支持**南向 Source**（拒绝北向 `app_type=north`）
- `config` 返回**脱敏回显**：顶层 `jwt_secret`、`mqtt.username`/`mqtt.password` 与 `lux.password` 遮蔽为 `"***"`（保留键、替换值），其余字段原样——诊断需看 lux 端口/`data_dir`、bridge bind、log 级别等（供运维与 MCP `get_config` 工具读配置形态）
- `[history]` 节（L2 部署期开关，缺省整节走默认 `enabled=false`）键清单：`enabled`/`data_dir`/`memory_limit_mb`/`cardinality_limit`/`retention_days`/`wal_sync_ms`/`wal_size_limit_mb`/`channel_capacity`/`batch_max_rows`/`close_timeout_secs`（全键定义 `src/config.rs::HistoryConfig`）；其中 `data_dir` **有意可见**（纯路径无密钥，现场诊断需要）——三层开关语义见 §4.18。**边界校验**（2026-08-26 审查修复）：`memory_limit_mb`/`wal_size_limit_mb ∈ [1, 2^20]`、`retention_days ∈ [1, 36500]`、`channel_capacity ∈ [1, 2^24]`、`batch_max_rows ∈ [1, 2^20]`、`cardinality_limit ∈ [0, 2^24]`、`data_dir` 非空——非法值**启动时按 init_failed fail-soft 处理**（error 日志 + 引擎停用 + `/api/v1/history` 503，主程序继续），此前超大值乘法换算可致 panic/静默 wrap；`wal_sync_ms`/`close_timeout_secs` 不设限（0 有合法语义：PerAppend 严格档/立即放弃）
- `[lux] password`（D-LUX-1）：`bind_host` 非 loopback（非 `127.0.0.1`/`::1`/`localhost`）时**必须**设置，否则启动即拒（错误信息指路：设密码或改回 loopback 绑定）；设置后 RESP 端口强制 AUTH。loopback 部署缺省空串，维持无密码单机信任模型
- 响应含 `hwid` 覆盖键：空串 = 启动时按硬件自动生成；非空 = 部署期指定的网关 ID（多网关接同一平台需唯一）

### 4.2 实例 CRUD（`routes_inst.rs`，lux Hash `instances`）

| Method | Path | Body/Query | 成功响应 |
|---|---|---|---|
| GET | `/api/v1/insts` | — | `{<inst_id>:<InstConfig>}` |
| GET | `/api/v1/inst/roster` | — | `[{<InstConfig>, "state":"<running\|stopped\|...>"}, ...]`（配置全集 ∪ 运行态合并） |
| GET | `/api/v1/inst/get` | `?inst_id=` | `{<inst_id>:<InstConfig>}`，无则 `{}` |
| POST | `/api/v1/inst/set` | `{"inst_id":"...","object":<InstConfig>}` | `{"status":"ok"}`；外部插件额外 `{"status":"ok","plugin_token":"<hex>"}`；非法配置 `{"status":"error","msg":...}` |
| POST | `/api/v1/inst/delete` | `{"inst_id":"..."}` | `{"status":"deleted"}` |
| POST | `/api/v1/purge` | `{"scope":"all"\|"rtdata"}` | all：`{"status":"purged","instances":<N>,"rtdata_keys":<M>}`；rtdata：`{"status":"purged","rtdata_keys":<N>}` |

- `inst/set` 会**强校验** `object` 反序列化为 `InstConfig`（避免存入非法配置延迟到 startinst 才报错）
- **外部插件**（`host_kind:"external"`）：每次 `set` 调 `ext_token::issue` **覆盖旧 token**（旧 token 即失效），明文 `plugin_token` 仅此次返回（管理员交付外部进程）。重复 set 同实例 = 轮换 token
- `inst/delete` 连带清除外插件 token + 该实例全部 Lua kv（`luakv`）
- **`inst/roster` 是实例名册的单一权威数据源**：`/inststatus`（`mgr.status()`）只含已 start 过的实例，`auto_start=false` 从未启动的配置实例会缺席。roster 合并 lux `instances` 配置全集与运行态——未在运行态的实例标 `state:"stopped"`；运行态中配置没有的实例（config.toml/代码启动）追加仅含 `inst_id`+`state` 的条目。回显经 M3 脱敏（`app_config.password` 字面值遮 `"***"`，`secret:` 引用原样），MCP `rsiot_list_instances` 同源
- **`purge` 是危险操作**（前端已接二次确认）：`scope=all` 遍历全部实例逐个级联删除（停 task + 清 rtdata/stat/trace/配置/token/luakv），再兜底 `KEYS` 清 `rtdata:*`/`stat:*`/`luakv:*` 孤儿残留；**绝不碰** `users`/`passwords`/`groups`/`accesskey`/`ext_tokens` 系统数据。`scope=rtdata` 只清实时数据缓存（保留实例配置）；未知 scope 返回 `{"status":"error","msg":"unknown scope: '...', expected 'all' or 'rtdata'"}`

**建普通采集实例示例**（modbus，RS485 多 slave 单实例）：
```bash
curl -s -XPOST -H "Authorization: $TOKEN" -H 'Content-Type: application/json' \
  http://localhost:8790/api/v1/inst/set -d '{
    "inst_id":"mb1",
    "object":{
      "inst_name":"modbus 电表","app_code":"modbusClient","app_type":"south",
      "app_config":{"kind":"tcp","endpoint":"192.168.1.10:502","period":2,
        "poll_strategy":"round_robin","request_timeout_secs":2},
      "devices":[
        {"label":"电表A","value":"devA","slave_id":1,"children":[
          {"label":"电压","value":"holding:0:f32:ABCD","type":"float"}]},
        {"label":"电表B","value":"devB","slave_id":2,"children":[
          {"label":"电压","value":"holding:0:f32:ABCD","type":"float"}]}
      ]
    }
  }'
```
- **M1 RS485 多 slave**：`slave_id` 下沉到设备级（`DeviceSpec.slave_id`），一条总线（同 `app_config` 串口参数）多从站由单实例轮询；`poll_strategy` = `round_robin`（默认，齐头并进）/ `sequential`（单设备多点时间一致）；`request_timeout_secs` per-request 超时跳过（单 slave 无响应故障隔离，不拖垮整轮）。单 slave 旧配置（`app_config.slave_id`）向后兼容回退。详见 CLAUDE.md「modbus_client 端到端」

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

### 4.3 设备 CRUD（`routes_device.rs`，实例配置聚合 ∪ lux Hash `dev2inst`）

| Method | Path | Body/Query | 成功响应 |
|---|---|---|---|
| GET | `/api/v1/devices` | — | `{<dev_id>:<DeviceSpec>}` |
| GET | `/api/v1/device/get` | `?dev_id=` | `{<dev_id>:<DeviceSpec>}`，无则 `{}` |
| POST | `/api/v1/device/set` | `{"dev_id":"...","object":<DeviceSpec>}` | `{"status":"ok"}` |
| POST | `/api/v1/device/delete` | `{"dev_id":"..."}` | `{"status":"deleted"}` |

- `device/set` **无 schema 校验**（裸 JSON 写入），建议仍按 `DeviceSpec` 结构写
- `GET /devices` / `device/get` 响应可能含**运行时设备**条目（lua `set_value` 等运行时产生、无配置点表，经 `dev2inst` 归属表并入）：额外带 `"runtime": true` 与 `"owner_inst": "<宿主实例>"`（多宿主逗号串）字段；同 devid 存在配置时以配置为准（不带这两字段）

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
- 无 `register` 端点（白名单悬空）；`changepass` 要求旧密码（新用户不可用）→ 首个账号只能靠 `identity::bootstrap` 种子（admin/admin），后续账号需带 token 调 `user/set` + 直写密码或先 login 再 changepass。详见 API 完备性评估（`api-completeness-assessment.md`，已归档，git 历史 66945dd 可溯）

### 4.5 用户组 / AccessKey（`routes_group.rs`）

| Method | Path | Body/Query | 成功响应 |
|---|---|---|---|
| GET | `/api/v1/usergroups` | — | `{<group_id>:[<user_id>,...]}` |
| POST | `/api/v1/usergroup/add` | `{"user_id":"...","group_id":"..."}` | `{"status":"ok"}`（组不存在隐式创建） |
| POST | `/api/v1/usergroup/remove` | `{"user_id":"...","group_id":"..."}` | `{"status":"ok"}` |
| POST | `/api/v1/accesskey/set` | `{"key":"..."}` | `{"status":"ok"}` |

- 用户组数据**当前不参与任何权限判定**（无授权层）；AccessKey 全局单值，轮换即时生效

### 4.6 Lua 脚本与 stdlib 运维（`routes_lua.rs`）

脚本/共享库资产端点（`kind` 参数区分资产类型：`script` 缺省 / `lib` 站点共享库，阶段 A）：

| Method | Path | Body/Query | 响应 |
|---|---|---|---|
| POST | `/api/v1/lua/reload` | `?inst_id=` | `{"status":"ok"}`；实例不存在 404；非 luaClient 400 |
| GET | `/api/v1/lua/scripts` | `?kind=script\|lib`（缺省 script） | script：`{"scripts":["<目录名>",...]}`；lib：`{"libs":["<org>/<路径>.lua",...]}` |
| GET | `/api/v1/lua/script` | `?name=`、`?kind=` | `{"name","kind","content","size","mtime?"}`；不存在 404 |
| POST | `/api/v1/lua/script/save` | `{"name","content","kind?"}` | script 写 `scripts/<名>/main.lua`；lib 写 `scripts/_lib/<org>/...`；覆盖前自动存版本快照 |
| GET | `/api/v1/lua/script/versions` | `?name=`、`?kind=` | `{"versions":[{ts,size},...]}`（ts 降序） |
| POST | `/api/v1/lua/script/rollback` | `{"name","ts","kind?"}` | 回滚到 `versions/<ts>.lua`（回滚点另存快照） |
| GET | `/api/v1/lua/stdlib` | — | `{"version","files":[{name,path,ver,source}]}`（source：`synced`/`seed`；`scripts/` 模板条目不列入） |
| POST | `/api/v1/lua/stdlib/sync` | `{"upstream?","pin?"}` | `{"status","upstream","pin","report":{added,updated,removed,version},"reloaded":[{inst_id,result}]}`；失败 502 |

- `reload` 仅对 `app_code=="luaClient"` 生效（stop+start，IO 重连）；与文件 notify 自动热加载等价
- `kind=lib` 路径校验：**≥2 段**（`<org>/<库名>.lua`）、首段非官方保留段（`algo`/`codec`/`proto`/`cloud`/`util`/`oo`）、防穿越；脚本名白名单 `[A-Za-z0-9_-]` 且拒绝保留目录 `_lib`/`_stdlib`
- `sync`：ed25519 验签 + 逐文件 sha256 + `_stdlib/` 原子替换（失败本地不动）；**pin 默认锁定**（缺省 = 锁定本地已装版本，`"pin":"auto"` 显式跟随 latest，配置键 `lua_stdlib_pin` 持久化锁定）；成功/失败落审计日志；成功后批量 **VM 级 reload**——运行中实例逐一热加载（失败自动回退旧 VM），非运行实例记 `skipped` 不复活；需 `lua-http` feature（缺省开启）
- **实例运行指标（含 luaClient）已统一到 `GET /api/v1/inst/stats`**（M4，见 §4.10）；旧 `GET /api/v1/lua/stats` 已移除

### 4.7 写命令下发（`routes_command.rs`）

| Method | Path | Body | 响应 |
|---|---|---|---|
| POST | `/api/v1/command/send` | `{"instance":"<InstId>","devid":"...","cmd":"write","data":"<JSON 文本>"}` | `{"status":"ok"}`；实例未运行 404 |

- `data` 是**字符串化的 JSON**（注意转义），由具体插件 `handle_write` 解析：
  - opcua：`{"node_id":"ns=2;s=v1","value":123,"opc_type":"Int32"}`
  - modbus：`{"address":"holding:0:f32:ABCD","value":12.5}`
  - s7：`{"address":"DB1.DBD0","value":42.5,"kind":"f32"}`
  - **selfMonitor（rs-iot 自监控设备，instance 恒 `"self"`）**：`{"watchdog":7}`（任意整数，下周期回读——写通路自检）/ `{"note":"机房A-1号网关"}`（现场备注，≤256 字符）/ `{"report_period":2}`（自身上报周期秒，钳 [1,3600]，下周期生效）；多键可一命令同写；写后随下一上报周期出现在 `rtdata:rsiot`（标签全表见 `docs/designs/2026-08-21-self-monitor-device-plan.md`）
- 与 MQTT `<hwid>/command` 共用同一条 `CtlRegistry` 写总线；**仅确认投递到 `cmd_rx`，无执行回执**（见完备性评估）

### 4.8 实时数据（`routes_rtdata.rs`，lux Hash `rtdata:<devId>`）

| Method | Path | Query | 响应 |
|---|---|---|---|
| GET | `/api/v1/rtdatas` | `?dev_id=`（可选） | 单设备 `{<field>:[ts,localTime,value]}`；不传 `{<devId>:{<field>:...}}` 全量 |
| GET | `/api/v1/rtdatas/subscribe` | `?dev_id=`（逗号分隔，可选）`&tag=`（可选）`&interval=`（ms，可选，默认 1000，clamp [100,60000]） | SSE：`snapshot`（连接即推，逐设备全量）+ `datachange`（周期，仅值变化/删除）；cell 与 `/rtdatas` 同构（§4.14） |

- 每个 field 值尝试 JSON 解析（失败保留原始字符串）
- **只读最新快照**，无时间范围/分页/聚合——历史查询走 §4.18 `GET /api/v1/history`

### 4.9 静态前端（`frontend.rs`，rust-embed）

| Method | Path | 鉴权 | 响应 |
|---|---|---|---|
| GET | `/apitest` | 公开 | 内嵌 apiTest 页面（`index.html`） |
| GET | `/` | JWT | 302 → `/apitest`（未带凭据访问根路径会得 401） |

### 4.10 可观测性（`routes_metrics.rs` / `routes_stat.rs` / `routes_trace.rs` / `routes_logs.rs`）

> M4 统一埋点 + P1 三子系统（/metrics + 报文 trace + 日志 ring buffer）。

| Method | Path | 鉴权 | Body/Query | 响应 |
|---|---|---|---|---|
| GET | `/metrics` | **公开** | — | Prometheus exposition 文本（`Content-Type: text/plain; version=0.0.4`） |
| GET | `/api/v1/inst/stats` | JWT | `?inst_id=`（可选） | `{inst_id, app_code, host_kind, state, health}`（health = lux `stat:<inst>` InstanceStat） |
| POST | `/api/v1/inst/trace` | JWT | `{"inst_id":"...","duration_secs":60}` | `{"status":"ok","deadline_ms":...}`；实例未运行 404 |
| GET | `/api/v1/inst/trace` | JWT | — | `{"active":["<inst_id>",...]}`（已惰性过滤超时项） |
| DELETE | `/api/v1/inst/trace` | JWT | `?inst_id=` | `{"status":"ok"}` |
| GET | `/api/v1/logs/recent` | JWT | `?level=&target=&limit=`（均可选） | `[LogEntry]`（最近 N 条结构化日志） |

**`/metrics`**（P1 #1，机器出口）：进程级 14 metric family（`rs_iot_info`/`uptime_seconds`/`process_cpu_pct`/`process_mem_bytes`/`ctl_*_total`/`pipeline_*_total`/`hub_depth[_max]`/`mqtt_*`）——注：mqtt_* 阶段 B 起恒 0（MqttPub 迁移 luaNorth，实时计数在 /api/v1/inst/stats extra：publish_ok/publish_fail/reconnect_count；字段保留为端点形状兼容，外部面板请迁移）。+ per-instance 5（`rs_iot_instance_info{inst_id,app_code,host_kind,state,health}` + `instance_tick_total`/`fail_total`/`last_activity_duration_ms`/`last_activity_ts`）。+ history 19（Phase 3 治理全量 + 2026-08-26 审查修复五拒收/跳过计数，**仅引擎 ok 时输出**——disabled/init_failed 省略防误导零值；源 `HistoryStats` atomic + `HistoryTelemetry` 引擎口径；运行态快照另见 `GET /api/v1/sysstatus` 的 `history` 块，语义见 §4.18）。十二计数器 counter：`rs_iot_history_{sent,written,dropped,deadbanded,skipped,lane_mismatch,nonfinite_skipped,dup_ts_skipped,stale_ts_dropped,ts_rejected,len_rejected,errors}_total`；治理七 family：gauge `rs_iot_history_archived_tags`（effective 快照 history=true 计数）/ gauge `rs_iot_history_series`（`list_metrics().len()` 官方口径，5s TTL 缓存）/ gauge `rs_iot_history_memory_used_bytes` / gauge `rs_iot_history_memory_budget_bytes`（tsink 引擎内存口径，预算=D6 `memory_limit_mb`）/ counter `rs_iot_history_deadband_evaluated_total`（死区评估行数，削减率分母——deadband=Some 标签的数值点）/ gauge `rs_iot_history_deadband_reduction_ratio`（=deadbanded/evaluated，分母 0 时输出 0）/ counter `rs_iot_history_write_errors_total{kind="memory_budget|cardinality|wal_disk|closed|other"}`（写错误按行五桶分型，Σkind == errors_total，值 0 桶也输出）。命名遵循 Prometheus 规范（counter `_total`、gauge 无后缀、耗时 `_ms`/字节 `_bytes`）。对接 Prometheus/Grafana：`job: rs-iot, /metrics`，免认证（内网信任，跨网加 IP 白名单/mTLS）。

**`/api/v1/inst/stats`**（M4）：替代废弃的 `/api/v1/lua/stats`，**全部内置插件**（dataSim/opcua/modbus/s7/lua/sidecar）+ 北向 luaNorth 统一经 `StatTracker` 写 lux `stat:<inst>` InstanceStat。核心 6 字段：`status`（running/degraded/stopped）/`tick_count`/`fail_count`/`last_activity_ts`/`last_activity_duration_ms`/`last_err`；插件扩展字段经 `extra` 透传（如 opcua `session_reconnect_count`、modbus `exception_count`、lua `vm_restart_count`/`script_version`）。

**`/api/v1/inst/trace`**（P1 #2，报文 trace 开关）：per-instance 运行时激活收发字节 hex dump（裸报文调试）。激活后该实例收发字节进 tracing target `io_trace`（抓包风格 `[inst_id] TX/RX <hex>`，惰性超时关闭）。**覆盖 lua（io_engine direct dump：request TX/RX + recv_idle 被动 RX 三点）+ modbus 4 传输（`TraceIo<T>` 包底层 IO——TCP/RTU serial/RTU tcp/RTU udp）**；opcua/s7（非裸报文协议）/sidecar（bridge IPC 帧）后置。

**`/api/v1/logs/recent`**（P1 #3，日志 ring buffer 快照）：内存 ring buffer 截获最近 N 条日志（`[log].ring_buffer_capacity` 默认 2000），结构化 `LogEntry{ts, level, target, message, fields}`。`level=`（>= 给定，如 `warn` → error+warn）/`target=`（精确，如 `io_trace`）/`limit=`（默认 200）。**报文 trace 衔接**：`?target=io_trace` 拉报文 hex（ring buffer 用独立 filter `info,io_trace=trace` 抓 trace level）。内存态（重启清空，"最近"快照），常规运行日志落盘靠 daily 文件（M3）。

> **报文 trace 按设计不落盘**：trace 报文（trace level）只进内存 ring buffer（2000 条/实例，按需激活、用完关闭），不写入日志文件——避免 hex 数据撑爆磁盘；常规运行日志（info/warn/error）才 daily 落盘。

> 实时性补充：活跃告警清单（§4.12 `GET /api/v1/alarms`）与 SSE 事件流（§4.13 `GET /api/v1/events`）同属可观测面——前者 REST 拉取当前活跃告警，后者增量推送日志/报文/状态/实时数据/告警五类事件。

### 4.11 平台凭证 secret（`routes_secret.rs`，阶段 B T4）

平台级凭证（MQTT broker 密码、OneNET access_key 等）的 lux 全局 `secrets` 表管理面。
ref 规则：`secret:` 前缀 + `[a-z0-9_-]`（小写；非法字符 400，消息含合法字符集）。
消费侧见 Lua 脚本指南（脚本 `ctx:secret_get` 走实例 `app_config.secrets` 授权清单；
mqtt-north password 的 `secret:` 引用走宿主连接序列）。

| Method | Path | 鉴权 | Body/Query | 响应 |
|---|---|---|---|---|
| POST | `/api/v1/secret/set` | JWT | `{"ref":"secret:onenet_key","value":"<明文>"}` | `{"status":"ok","ref":...}`（覆盖旧值；不回显 value） |
| GET | `/api/v1/secret/list` | JWT | — | `[{"ref":"secret:mqtt_pwd","masked":"****"}, ...]`（ref 字典序） |
| DELETE | `/api/v1/secret` | JWT | `?ref=secret:onenet_key` | `{"status":"ok","ref":...}`（幂等：不存在仍 200） |

- **掩码规则**（list 绝不回显明文，D6）：值 >4 字符 → `****` + 后 4；≤4 → 全 `****`。
- **审计**（Q4 ref 级、不落值）：set/delete 各落一条 `[secret][audit] set/delete ref=...`
  日志——日志与响应里**绝不出现明文**（who 由 JWT 访问日志侧承载）；脚本侧读取不落日志。
- **加密存储**（D-SEC-1）：值经 chacha20poly1305 AEAD 加密落库（密钥由 `jwt_secret` 经
  HKDF-SHA256 派生）——RESP 直读 `HGETALL secrets` 只见 `enc:v1:` 密文；旧明文值读兼容
  （懒迁移：读到不回写，下次 set 覆盖时才加密）。
- 删除后引用该 ref 的实例下一轮 auth/重连按"认证失败"退避重试（补写即救活，无需重启）。

```bash
# 写入（管理机）
curl -X POST -H "Authorization: <token>" -H 'Content-Type: application/json' \
  -d '{"ref":"secret:onenet_key","value":"<平台分配 access_key>"}' \
  localhost:8790/api/v1/secret/set
# 列表（仅掩码）
curl -H "Authorization: <token>" localhost:8790/api/v1/secret/list
# 删除
curl -X DELETE -H "Authorization: <token>" "localhost:8790/api/v1/secret?ref=secret:onenet_key"
```

### 4.12 活跃告警（`routes_alarm.rs`，Q12 告警最小闭环）

脚本判定的活跃告警清单（lux Hash `alarms`：field = key，value = `{level,source,message,ts,count}`）的查询/手动清除面。只存**当前活跃**（历史时序表明确不做）；lux 持久——重启后活跃清单与 count 保留。

| Method | Path | 鉴权 | Body/Query | 响应 |
|---|---|---|---|---|
| GET | `/api/v1/alarms` | JWT | — | `[{key,level,source,message,ts,count}, ...]`（按 ts 倒序，空表 `[]`）；lux 故障 500 |
| DELETE | `/api/v1/alarms` | JWT | `?key=<k>` | `{"status":"ok","key":...}`（幂等：不存在仍 200）；lux 故障 500 |

- **触发（fire）无 REST 端点**——判定归脚本 `ctx:fire_event(key, level, message)`（阈值/迟滞逻辑在 Lua 内）；`source` 由宿主注入实例 ID（API 无 source 参数，脚本不可伪造）
- `level` ∈ `info`/`warn`/`error`；`count` 为同 key 累计触发次数（重复报警风暴可见性）；同 key **1s 抖动窗口**内重复 fire 不重发 SSE 帧（前端不被同键闪屏）但 count 仍累计；fire 频控 20/s/实例、key 非空且 ≤128 字符、全局活跃键上限 1000（新键拒存，运维清除后恢复容量）
- DELETE 手动清除（运维兜底）：条目存在才发 Clear SSE 事件（alarm 帧 `action:"clear"`，见 §4.13——前端面板视图收敛）；HDEL→publish 统一序，清除失败整轮报错不发帧
- **审计 key 级不落 message 全文**（防日志洪水；message 看 REST/SSE）

```bash
# 活跃清单（ts 倒序）
curl -H "Authorization: <token>" localhost:8790/api/v1/alarms
# 手动清除（幂等，前端已接二次确认）
curl -X DELETE -H "Authorization: <token>" "localhost:8790/api/v1/alarms?key=dev1.temp_high"
```

### 4.13 SSE 实时事件流（`routes_events.rs`）

| Method | Path | 鉴权 | Body/Query | 响应 |
|---|---|---|---|---|
| GET | `/api/v1/events` | JWT | `?types=<逗号分隔 kind>`（缺省/空 = 全推） | `text/event-stream`：每事件一帧 `event: <kind>\ndata: <json>\n\n` |

- **kind 五种**：`rtdata`（实时数据批量帧，Q6 带值：`{dev_ids,count,samples,ts}`——50ms 攒批窗口**一批一事件**而非每点一次，`samples` 按 dev 聚合携带最新值；lagged 丢帧/dev 超限截断时数据不全，前端靠既有 2s 轮询 `GET /rtdatas` 兜底）/ `alarm`（告警 fire/clear）/ `log`（与 `/api/v1/logs/recent` 同构的 LogEntry）/ `packet`（报文 trace hex dump：`{inst_id,dir:"tx"\|"rx",hex,ts}`）/ `state`（实例状态变更：`{inst_id,state,app_code,host_kind,ts}`）
- **alarm 帧** data 形状：`{"kind":"alarm","action":"fire"|"clear","key","level","source","message","ts"}`（`source` 为宿主注入的实例 ID，不可伪造；一次 `addEventListener("alarm")` 覆盖双向）
- **鉴权走 cookie**：EventSource 无法带自定义 header，`/login` 已 `Set-Cookie token=...; HttpOnly` 同源自动带（Authorization 头通道同全局中间件，§2，curl 调试可用）
- broadcast 语义：每连接独立 Receiver；慢客户端缓冲满收 `Lagged` → 丢帧继续不终结流，丢失条数累入 Prometheus `rs_iot_sse_lagged_total`（持续增长提示调大 `event_bus_capacity`，默认 256）
- 无事件时 KeepAlive 周期发注释帧防代理超时；无生产者时连接静默等待

### 4.14 实时数据订阅（OPC UA 式，`routes_rtdata_subscribe.rs`）

外部客户端订阅实时数据（对标 OPC UA CreateSubscription 语义：初次全量 + 周期只推变化；SSE，鉴权同全局中间件——Authorization / cookie / X-Access-Key）。

| Method | Path | 鉴权 | Body/Query | 响应 |
|---|---|---|---|---|
| GET | `/api/v1/rtdatas/subscribe` | JWT | `?dev_id=<逗号分隔设备ID>`（缺省=全部设备）`&tag=<contains>`（可选）`&interval=<ms>`（默认 1000，clamp [100,60000]） | `text/event-stream`：`snapshot` + `datachange` 帧 |

```bash
curl -N -H "Authorization: Bearer $JWT" \
  "http://localhost:8790/api/v1/rtdatas/subscribe?dev_id=devA,devB&tag=temp&interval=1000"
```

帧类型（`event:` 头，EventSource `addEventListener` 分发）：

- `event: snapshot`（连接建立即推，一设备一帧）：`{"dev":"devA","points":{"devA.temp":[ts,"localTime","value"],...}}`
- `event: datachange`（每 interval 一查，仅当有变化/删除才推一帧）：`{"changes":{<field>:[ts,"localTime","value"],...},"removed":[<field>,...],"ts":"RFC3339"}`
- 注释帧（`: `，15s KeepAlive）——代理防超时

语义保证：

- **值变化只比 cell[2] 值串**——采集插件周期写回同值不产生通知（对标 OPC UA DataChangeFilter 值触发）
- **断线重连即重新 snapshot 全量重同步**（EventSource 自动重连即可恢复，无需 resync 协议）
- 设备/点位删除 → `datachange.removed` 列出
- 与 `GET /api/v1/rtdatas` 的 cell 输出同构（`[ts, localTime, value]` 值串），客户端复用同一解析
- **tag 命名约束：tag 名不含点（`.`）**——field 形如 `<devid>.<tag>`，ksub 档增量路径按**最后一个**点切分归属设备段（`dev_segment`），tag 含点会误归设备域（表现为多余的删→重推抖动，不丢数据；tag 过滤本身为末段 contains 不受影响）。**碰撞 fail-closed（2026-08-26 审计 C3）**：devid/tag 含点使不同 (devid,tag) 对可映射同一 tag_key（如 (a,b.c) 与 (a.b,c) 同键）——effective 构建时碰撞 key 整体剔除并 warn 列出冲突源（不授权任何一方，防未授权方蹭白名单归档）；现场按日志改不含点命名即恢复
- 机制为**每连接读侧 poll-diff**（非 EventBus 转发，不受其 dev 截断/丢帧特性影响；设计依据：[2026-08-23-rtdata-subscribe-plan.md](../designs/2026-08-23-rtdata-subscribe-plan.md)）
- **读源双 impl**（`[rtdata].sub_impl`，Phase 2 订阅升级——SSE 帧/参数/diff 语义**两档完全一致**，行为等价由双档参数化 e2e 守门）：

| 配置键 | 值 | 机制 |
|---|---|---|
| `[rtdata].sub_impl` | `ksub`（**缺省**） | 进程内常驻 `RtdataWatchService`（`src/api/rtdata_watch.rs`）`ksubscribe("rtdata:*")` 消费 key 事件维护 per-devid 单调版本表（**共享 dirty-set**）——连接每 tick 只对版本落后的设备精准 `HGETALL`（替代每连接每 tick 全量轮询；无变更 tick 经 marks 计数器 O(1) 早退，免全表扫描）；订阅溢出（Lagged）→ epoch+1 → 连接全量重读后恢复增量，不丢帧；**watcher 订阅异常 → 自动退订重建**（epoch+1 + 重铺 KEYS + 退避 1s），不静默冻结 |
| | `poll`（回退开关） | 每连接每 tick 全量重读订阅范围（原 Phase 1 实现路径原样保留）；watcher 不启动，lux 无订阅者零开销承诺保持 |

  回退 = 改一个配置键（`sub_impl = "poll"`）重启；`[rtdata]` 节缺省整节走默认 `ksub`（键定义 `src/config.rs::RtdataConfig`）

### 4.15 标签元数据 effective 视图（`routes_tag.rs`，M-4 T3 改造）

标签级属性读写全部走**设备模型注册表**（旧 lux `tags` 表已退役——被 `devreg.overrides` 吸收，决策 D-M3）：读侧 = 模板默认 ∪ 覆盖三态合并（effective）；写侧 = 设备覆盖（`TagOverride` 三态）。设计：[2026-08-25-device-model-registry-plan.md](../designs/2026-08-25-device-model-registry-plan.md)。

| Method | Path | 鉴权 | Body/Query | 响应 |
|---|---|---|---|---|
| GET | `/api/v1/tags` | JWT | `?dev_id=<设备ID>` `&history=true`（按 effective 值过滤） | `{"<dev.tag>": {…TagProps, _source: "model" 或 "override"}}` |
| GET | `/api/v1/tags/legacy-export` | JWT | —（旧 `tags` 表只读快照，升级过渡一个版本） | `{"<dev.tag>": TagProps}` |
| POST | `/api/v1/tag/set` | JWT | `{"dev_id","tag", history?, deadband?, desc?, unit?}`（三态：缺省=不动，`null`=恢复继承模板，值=覆盖） | `{"status":"ok","key","props"}`（合并后 effective） |
| POST | `/api/v1/tag/batch` | JWT | `{"pattern":"dev*.temp*","history":true,...}`（glob 批量写覆盖） | `{"status":"ok","matched":n,"updated":n}` |
| POST | `/api/v1/tag/delete` | JWT | `{"dev_id","tag"}`（清该 tag 全部覆盖回模板默认；模型 tag 不删） | `{"status":"ok","cleared":bool}` |

```bash
# 批量开归档 + 死区（历史存储 feature 消费这两个属性；写的是设备覆盖）
curl -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json"   -d '{"pattern":"furnace*.temp*","history":true,"deadband":0.5}'   http://localhost:8790/api/v1/tag/batch
```

- 三态语义：字段**缺省**=不动现有覆盖，显式 `null`=恢复继承模板，值=覆盖；`deadband` 显式关死区传 `0.0`（下游过滤语义与无死区等价——API 不暴露双层 Option）
- `_source` 标注条目来源（"model"=纯模板派生 / "override"=含设备覆盖）；模板顶层 `unit`/`desc` 是 props 同名字段的垫底默认
- 设备不在 devreg → 404；tag 不在设备模型 → 400
- glob `*` 通配匹配 `<devid>.<tag>` 全键（匹配域 = 模型 tag 全集）

> **deadband 三态口径警示（两端口相反，批量消费方必读）**：`/tag/set`、`/tag/batch` 的 `deadband` 三态是 **`null` = 恢复继承模板、`0.0` = 显式关死区**（本端口不暴露双层 Option，`0.0` 与无死区过滤语义等价）；而 `POST /api/v1/devreg/set` 写 `DeviceEntry.overrides` 的 `TagOverride.deadband` 是**双层 Option：字段缺省 = 继承模板、显式 `null` = 显式关死区**——两端口径**相反**，批量 UI/脚本在两端口间搬运 deadband 必须做 `0.0 ↔ null` 语义转换（口径依据：`src/api/routes_tag.rs` 头注、`src/model/types.rs` `TagOverride` serde 注释）。

### 4.16 设备模型注册表（`routes_model.rs`/`routes_devreg.rs`，M-1 起）

物模型（devmodels：参数化地址模板 + 型号级默认属性）+ 全局设备注册表（devreg：模型引用 + 实例化参数 + per-tag 三态覆盖）。**元数据唯一出生地**（决策 [2026-08-25-device-model-registry.md](../decisions/2026-08-25-device-model-registry.md)）。

| Method | Path | 鉴权 | Body/Query | 响应 |
|---|---|---|---|---|
| GET | `/api/v1/devmodels` | JWT | `?model_id=`（单查） | `{model_id: DeviceModel}` |
| POST | `/api/v1/devmodel/set` | JWT | DeviceModel JSON（**create-only**：重名 409） | `{"status":"ok","model_id"}` |
| POST | `/api/v1/devmodel/update` | JWT | DeviceModel JSON（整包替换；引用设备预检 409 带明细） | `{"status":"ok"}` |
| POST | `/api/v1/devmodel/delete` | JWT | `{"model_id"}`（被 devreg 引用 → 409 带引用列表） | `{"status":"ok"}` |
| GET | `/api/v1/devreg` | JWT | `?devid=` `?model=` | `{devid: {…DeviceEntry, effective}}` |
| POST | `/api/v1/devreg/set` | JWT | DeviceEntry JSON（upsert；写前三级校验：模型存在/params 齐/覆盖 tag ∈ 模型） | `{"status":"ok","devid"}` |
| POST | `/api/v1/devreg/delete` | JWT | `{"devid"}`（级联：**解绑实例引用** + dev2inst + rtdata；历史不动） | `{"status":"ok","unbound_instances":n}` |
| POST | `/api/v1/migrate/inline-devices` | JWT | —（幂等；M-2 起**升级必经**——旧内联形态无法反序列化） | `{converted_models, converted_devices, converted_instances, failures}` |

**开放扩展槽**（设计 [2026-08-27-model-extension-slot-design.md](../designs/2026-08-27-model-extension-slot-design.md)）：DeviceModel 与每个 tag 均可携带 `x_` 前缀扩展键（wire 上顶层平铺，如 `"x_asset_category":"动力配电"`；tag 级如 `"x_scale":{"k":0.1}`）。

- 键契约：**MUST `^x_[A-Za-z0-9_]+$`**（模型级与 tag 级 extra 顶层键均查）；违规 → **400**（`ModelError` 既有映射）
- 语义：宿主**不解释**扩展键（采集/归档/生效链路零消费）——供外置应用（报警/资产/协议元数据等）经 GET 读取；读写原样保留（跨版本读-改-写不丢）
- GET `/api/v1/devmodels` 全量回显扩展键；前端 ModelForm 只读展示；MCP 工具 `rsiot_list_devmodels` 可读

```bash
# 带扩展键建模型（合法 x_ 键随 GET 原样回显；"vendo" 这类非 x_ 键会被 400 拒绝）
curl -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" -d '{
  "model_id":"vx_dtz341","params":["base"],
  "x_asset_category":"动力配电",
  "tags":[{"name":"ua","data_type":"f32","addr":"holding:{base}:f32","x_scale":{"k":0.1}}]
}' http://localhost:8790/api/v1/devmodel/set
```

### 4.17 发现队列（`routes_discovery.rs`，M-4）

未建模标签进 `discoveries` 队列（不自动入正式册）——**发现是建模工作流的入口**（人工确认后 adopt 一键建模），D-M3。馈入两路：① pipeline flush 出口（luaTransform 等 hub 旁路数据）；② **lua/sidecar 校验闸拒收点**（D-M4：闸拒的 (devid,tag,样本值) 进队——数据本身不放行，元数据可见供 adopt）。

| Method | Path | 鉴权 | Body/Query | 响应 |
|---|---|---|---|---|
| GET | `/api/v1/discoveries` | JWT | `?dev_id=`（按设备过滤） | `[Discovery]`（count 降序） |
| POST | `/api/v1/discovery/clear` | JWT | `{"dev_id"?}`（缺省全清） | `{"status":"ok","cleared":n}` |
| POST | `/api/v1/discovery/adopt` | JWT | `{"model_id", "tags":[{name,data_type,addr?,unit?,desc?}]}`（同名跳过；引用预检 409/结构 400/模型缺 404） | `{"status":"ok","merged":n,"swept":n}`（swept=对账清除的已建模队列条目） |

```bash
# 一键建模：发现的 ghost tag 并入模型 m1，队列自动对账清除
curl -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -d '{"model_id":"m1","tags":[{"name":"ghost","data_type":"f32","unit":"V"}]}' \
  http://localhost:8790/api/v1/discovery/adopt
```

- 引擎行为（`src/model/discovery.rs`）：effective 缓存 O(1) 比对（已建模 once-set 纯内存）；未建模 per-entry 60s 节流聚合 count/样本；上限 5000 LRU 批量逐出；重启 count 接续

### 4.18 历史时序存储行为（`src/history/`，Phase 1b 写 + Phase 2 读）

历史归档是 pipeline 出口的**旁路**（fail-soft，不阻塞主链路），受三层开关节流——现场裁剪的机制骨架（总方案 [2026-08-24-history-storage-master-plan.md](../designs/2026-08-24-history-storage-master-plan.md) §一）：

| 层 | 粒度 | 开关 | 默认 |
|---|---|---|---|
| L1 | 编译期 | Cargo feature `history` | **off**（裁剪构建不编入 tsink 依赖，需同级 `../tsink_fork` checkout `rs-iot-1b-baseline`） |
| L2 | 部署期 | `config.toml [history] enabled` | `false`（不初始化引擎、不获取 `.tsink.lock`） |
| L3 | 标签期 | `TagProps.history`（effective 合并视图，过滤只认 effective） | `false`（逐标签白名单，默认零归档；批量操作面 = §4.15 `POST /api/v1/tag/batch`） |

- **当前状态**：历史**写路径**（Phase 1b）与**读路径**（Phase 2）均已实施，**治理运维收尾**（Phase 3 余项，2026-08-26）已收口——pipeline 出口旁路 fan_out + `GET /api/v1/history` 类型化读端点（全契约见下方）+ 前端历史查询面板（Telemetry 遥测页"历史趋势"面板，实时点位列表之后：raw/六聚合+手写 SVG 曲线；2026-08-26 自模型页迁址，关注点分离）；运行态可观测：`GET /api/v1/sysstatus` 的 `history` 块（§4.1，Phase 3 起含内存水位/series/archived_tags/死区削减率/错误分型）+ `/metrics` 的 `rs_iot_history_*` 18 family（§4.10）。L2 off / L1 裁剪时读端点返 **503**（总方案 §一承诺——显式拒绝而非静默空数据，矩阵见下）
- **写路径语义要点**：**满即丢**——旁路有界 channel `try_send` 满 → 丢样本 + `dropped` 计数（不阻塞主链路，A-B 门禁主链路劣化 <5%）；**deadband 绝对死区**（PI exception 风格：`|new − last_archived| ≥ deadband` 才归档，`last_archived` 仅在归档时更新；仅数值类型生效，Bool/Str 恒归档）；**类型保真**——样本按原类型入行，同 series 类型漂变剔除并计 `lane_mismatch`（防混型污染）；**三类写侧拒收**（审查修复 2026-08-26，坏行单行剔除不再以 tsink 拒整批方式连坐同批正常点）：Float 非有限值（NaN/Inf）剔行计 `nonfinite_skipped`（坏质量位非有效测量值，且 NaN 会毒化死区基准使死区永久失效）；时间戳 `ts×1000` 溢出或超出 `[now−7d, now+1h]` 窗剔行计 `ts_rejected`（工业采集正常抖动远小于该窗；历史回补是显式运维动作，可经 tsink 工具离线导入）；tag > 64KiB 或 devid/hwid > 16KiB 剔行计 `len_rejected`（tsink metric/label 长度上限）；同 series 跨批同 ts 跳过计 `dup_ts_skipped`（P5 防双点，可对账）；**严格更旧 ts 丢弃计 `stale_ts_dropped`**（D-T3，2026-08-26 P1 修复：跨批回卷的迟到样本不落盘——闭合"回卷后再卷"同 ts 双点对 Count/Avg 的双计；>7d 更旧已在 fan_out 计 `ts_rejected`，此处为窗内乱序）

#### 4.18.1 历史读端点 `GET /api/v1/history`（`routes_history.rs`，Phase 2）

单 series 类型化读（raw 或降采样聚合；select 经 `spawn_blocking` 包裹，长窗扫描不占 runtime worker）：

| Method | Path | 鉴权 | Query | 成功响应 |
|---|---|---|---|---|
| GET | `/api/v1/history` | JWT | 见下 | `{dev_id, tag, points, count}`（200） |

**Query 参数**（时间一律**秒**，REST 出入秒/内部 ms）：

| 参数 | 必填 | 语义 |
|---|---|---|
| `dev_id` / `tag` | 是 | 单 series 定位（series = metric(tag) + labels{devid,hwid}） |
| `start` / `end` | 是 | 窗口秒，**半开 [start, end)**——含 start 不含 end |
| `agg` | 否 | `min`/`max`/`avg`/`sum`/`count`/`last` 六值；与 `interval` **成对出现**（单出其一 → 400） |
| `interval` | 否 | 降采样桶宽（秒，>0）；仅与 `agg` 同现；**桶数 `(end-start)/interval` 上限 100000**（超限 400——downsample 全窗聚合的读放大护栏，raw 路径有 limit 分页早停不受限；`end-start` 差值本身溢出 i64（如两端取 i64 极值）同样 400"窗口跨度过大"——审查修复 2026-08-26，此前 debug 档可被单请求 panic） |
| `limit` | 否 | 缺省 10000、上限 100000——**取时间升序最旧 n 个**（非最新；raw 路径分页下推免全窗物化，聚合路径取前 n 桶） |

**响应**（200）——`value` **类型保真**（写路径四型直映：Float/Int → JSON number、Bool → boolean、Str → string）；空窗/不存在 series → 200 `points:[]`（tsink 语义，非错误）：

```json
{
  "dev_id": "devA",
  "tag": "temp",
  "points": [
    {"ts": 1756080000, "value": 23.5},
    {"ts": 1756080001, "value": true}
  ],
  "count": 2
}
```

**503 矩阵**（reader 未就绪；body JSON `{"error":"history unavailable","reason":…}`）：

| 场景 | `reason` |
|---|---|
| L1 feature 未编译进二进制 | `feature_not_compiled` |
| L2 `[history].enabled=false` | `history_disabled` |
| 引擎初始化失败 | `init_failed` |

**400 矩阵**：① 解析层（纯文本 body）——缺 `dev_id`/`tag`/`start`/`end` 任一；`start >= end`；`agg` 无 `interval` 或 `interval` 无 `agg`；`agg` 不在六值集合；`interval=0`；`interval` 域外/秒→ms 换算溢出；降采样桶数 `(end-start)/interval > 100000`；`limit > 100000`。② 引擎参数类错误映射（JSON body `{"error":…}`；tsink `InvalidTimeRange`/`UnsupportedAggregation` 等）——**Bool/String series 上数值聚合（`avg`/`sum`）→ 400**。其余引擎错误 → 500（JSON `{"error":…}`）。**注意两种 400 的 body 形态不同**（①纯文本 / ②JSON），客户端按 Content-Type 区分。

**downsample 桶语义三要点**（tsink 基线冻结）：

1. **桶 origin = 查询 `start`**（非 epoch 对齐）——首桶从 start 起算
2. **空桶省略**（无 gap filling）——interval 内无点的桶不出现在 points
3. **桶点 `ts` = 桶起点**（秒）

**聚合类型退化矩阵**：

| agg | 输出类型 |
|---|---|
| `avg` | float64（JSON number） |
| `count` | uint64（**JSON 整数**） |
| `sum` | 保数值域（F64/I64 原域，混型 → F64） |
| `min` / `max` / `last` | **保原类型**（含 bool/string） |

```bash
# raw：最近 1 小时，最旧 10000 点升序
curl -s -H "Authorization: $TOKEN" \
  "http://localhost:8790/api/v1/history?dev_id=devA&tag=temp&start=$(( $(date +%s) - 3600 ))&end=$(date +%s)"
# downsample：5 分钟桶平均
curl -s -H "Authorization: $TOKEN" \
  "http://localhost:8790/api/v1/history?dev_id=devA&tag=temp&start=1756080000&end=1756166400&agg=avg&interval=300"
# → {"dev_id":"devA","tag":"temp","points":[{"ts":1756080000,"value":23.4},{"ts":1756080300,"value":23.6}...],"count":288}
```

## 5. 数据模型（`src/types.rs`）

```
InstConfig {
  inst_id: String,          // 实例 ID（rtdata key 组成部分）
  inst_name: String,        // 显示名
  app_code: String,         // 插件注册名：dataSim/opcuaClient/modbusClient/s7Client/eipClient/luaClient/luaNorth/sidecarClient
  app_type: "south"|"north",
  auto_start: bool,         // 启动时自动拉起
  app_config: <任意 JSON>,   // 插件自解析（period/endpoint/...）
  devices: [DeviceBinding], // M-2：绑定全局设备（点表来自设备模型，不在实例配置）
  host_kind: "builtin"|"external"  // 缺省 builtin
}
DeviceBinding { devid, bind:{slave_id?…} }          // 引用 devreg（M-2，旧 DeviceSpec/ChildSpec 已退役）
DeviceModel  { model_id, vendor?, model_no?, category?, desc?, params:[..], tags:[TagTemplate] }  // lux `devmodels`
TagTemplate  { name, data_type, desc?, unit?, addr?(参数化), rw:"r"|"rw", props:TagProps }         // addr 语法 {name} 占位 + +N 偏移
DeviceEntry  { devid, model, params:{..}, overrides:{tag→TagOverride}, desc? }                      // lux `devreg`
TagOverride  { history?, deadband?(双层), desc?, unit? }  // 三态：缺省=继承模板（deadband null=显式关）
TagProps     { history:bool=false, deadband?:f64, desc?, unit?, owner_inst:Sys }  // 模板默认（effective = 模板 ∪ 覆盖）
Discovery    { devid, tag, first_ts, last_ts, count, sample_value }               // lux `discoveries`（M-4 发现队列）
CtlMessage   { instance:InstId, devid, cmd, data:<JSON 文本> }
```

- `app_code` 一经发布**锁定不改**（持久化在 lux `instances`，改名破坏已部署实例）
- 各插件 `app_config` 与模型 `addr` 的协议语义见各插件文档（CLAUDE.md「验证规则」段）
- **元数据唯一来源**：设备/标签元数据只能出自 devmodels+devreg（决策 2026-08-25）；实例只绑定设备，采集实现不得自行创建真实设备与标签

## 6. 已知限制与完备性

当前 API 定位为**单管理员调试/运维**（对标 Lite_IOT）。要支撑「管理员 + 多用户前端系统」尚有缺口（无授权层、无用户自服务、无批量/分页、命令无回执、无 `/healthz`、错误格式不统一等；历史查询已由 §4.18 提供 raw+downsample 单 series 面，多 series 批量/分页仍缺），详见 API 完备性评估（`api-completeness-assessment.md`，已归档，git 历史 66945dd 可溯）。**可观测性已具备**：`/metrics`（Prometheus）+ `/api/v1/inst/stats`（per-instance JSON）+ `/api/v1/inst/trace`（报文 trace）+ `/api/v1/logs/recent`（日志快照）+ `/api/v1/alarms`（告警清单，§4.12）+ `/api/v1/events`（SSE 实时事件流，§4.13）六端点覆盖指标/诊断/日志/告警/实时推送。
