# rs-iot 外部插件开发指南

> 面向**插件开发者**的"从零开发一个外部南向插件"总纲。协议二进制规范见
> [`external-plugin-protocol.md`](external-plugin-protocol.md)；可运行样例见
> [`examples/ext-plugin-python/`](../examples/ext-plugin-python/)（Python）与
> [`examples/ext-plugin-opcda-go/`](../examples/ext-plugin-opcda-go/)（Go + OPC DA）。
> REST 端点全表见 [`rest-api.md`](rest-api.md)。

## 1. 何时用外部插件（选型）

rs-iot 有三条南向扩展路径，按协议特征选：

| 场景 | 推荐路径 | 理由 |
|---|---|---|
| 主流协议（OPC UA / Modbus / S7） | **内置插件** | 已实现，零开销，团队维护 |
| 协议简单、点数少、自定义报文（串口为主） | **luaClient 脚本** | 随改随用、无需编译、沙箱护栏（见 [`lua-script-guide.md`](lua-script-guide.md)） |
| rs-iot 暂不支持的协议 **且** 有现成采集库（C/Go/Python/Java） | **外部插件（本文）** | 复用成熟库、跨语言、进程隔离 |
| 协议有 Rust 生态且希望进程内 | 内置插件（贡献上游） | 走 [`plugin-development-guide.md`](plugin-development-guide.md) |

**外部插件的核心价值**：把"rs-iot 跨平台 Rust 主进程"与"协议采集库的平台/语言约束"解耦。典型如 **OPC DA**（Windows COM，Rust 难直接调）：外部 Go 进程在 Windows 跑连 DA Server，经 bridge 把数据交给任意平台的 rs-iot。再如某 SCADA **只提供 C# API**：外部 C# 进程封装 API，经 bridge 接入，rs-iot 侧零改动。

## 2. 架构与边界（方案 C）

```
外部进程(任意语言) ──TCP──► bridge acceptor(:8781, token 鉴权)
                                │ 按 inst_id 分发到实例 task
                                ▼
                          sidecarClient task（每实例一个）
                                │ DataUp → Sample → hub → pipeline → lux rtdata
                                │ 写命令 ← CtlRegistry ← MQTT / REST → Command 帧 → 外部
                                ▼
                          lux（loopback，外部永不触碰）
```

**外部 = 纯运行时**：
- 配置/数据/状态**全在 rs-iot**（lux `instances`/`rtdata`/`ext_tokens`）
- 外部本地**仅缓存断连期间数据**待补传
- **lux 永不对暴露**（安全边界）：外部只连 bridge，认证/授权集中在 rs-iot

## 3. 接入流程（rs-iot 侧，无需写代码）

### 3.1 启用 bridge

`config.toml`：
```toml
[bridge]
enable = true               # 默认 false；现有部署零影响
bind_host = "127.0.0.1"     # 同机；跨机改 "0.0.0.0"（token 强制校验）
port = 8781
heartbeat_timeout_secs = 15
accept_hello_timeout_secs = 10
```

### 3.2 建外部插件实例（拿一次性 token）

```bash
# 登录拿 JWT（注意 body 字段：user_id / pass）
TOKEN=$(curl -s -XPOST http://localhost:8790/api/v1/user/login \
  -H 'Content-Type: application/json' \
  -d '{"user_id":"admin","pass":"admin"}' | jq -r .token)

# 建实例（app_code=sidecarClient, host_kind=external）→ 响应含 plugin_token（仅此次）
curl -s -XPOST http://localhost:8790/api/v1/inst/set \
  -H "Authorization: $TOKEN" -H 'Content-Type: application/json' \
  -d '{
    "inst_id":"ext1",
    "object":{
      "inst_name":"my collector","app_code":"sidecarClient","app_type":"south",
      "host_kind":"external","app_config":{"heartbeat_timeout_secs":30},
      "devices":[{"label":"设备","value":"devP","children":[
        {"label":"温度","value":"temp","type":"float"},
        {"label":"湿度","value":"humidity","type":"float"}
      ]}]
    }
  }' | jq .    # → {"status":"ok","plugin_token":"<hex>"}

# 启动实例（注册 slot，等外部接入）
curl -s -XPOST http://localhost:8790/api/v1/startinst \
  -H "Authorization: $TOKEN" -H 'Content-Type: application/json' \
  -d '{"inst_id":"ext1"}'
```

**关键约定**：
- `device.value`（`devP`）= 设备 ID = `rtdata:devP` 的 key = `DataUp.devid`
- `children[].value`（`temp`）= 数据点位 tag = `DataUp.samples[].tag`
- `children[].type`（`float`）= 值类型提示，外部按实际采集类型用
- **每次 `inst/set` 都重新签发并覆盖旧 token**（旧 token 即失效）——重复 set 同实例等于轮换 token，外部需用新 token 重连

### 3.3 外部进程连 bridge 报到

```
外部 TCP connect → 发 Hello{inst_id, token, ver:"1"} → 等 HelloAck{ok:true} + ConfigPush{devices}
```

## 4. 二进制协议要点

帧 = `[4B 大端 length][1B type][JSON body]`，`length = 1 + len(body)`，最大 4 MiB。7 种帧（Hello/HelloAck/ConfigPush/DataUp/Command/CommandAck/Heartbeat）。**完整字段与方向见 [`external-plugin-protocol.md`](external-plugin-protocol.md)**，本文不重复。

## 5. 外部进程实现要点

### 5.1 握手
- **必须先发 Hello**（带 `inst_id`/`token`/`ver:"1"`），等 `HelloAck{ok:true}` + `ConfigPush`
- token 错 / 版本不符 / 实例未启动 / slot 满 → `HelloAck{ok:false, msg}` + 连接关闭

### 5.2 数据上行（DataUp）
- `DataUp{devid, samples:[{tag, value, ts}, ...]}`，一帧可批量（不同 ts 也行）
- `value`：JSON 数字/布尔/字符串（bridge 按 `SampleValue` 解析：整数→Int、浮点→Float、布尔→Bool、字符串→Str）
- **`ts` 用采集时刻的 Unix 秒**（非发送时刻）——补传时保留原 ts

### 5.3 写命令（Command）
- rs-iot 收写命令（MQTT 或 REST）→ `Command{cmd_id, devid, cmd, data}` → 外部执行 → 回 `CommandAck{cmd_id, ok, err?}`
- `cmd_id`：bridge 递增 u64，用于关联 Ack
- `data`：透传 `CtlMessage.data` JSON（`cmd="write"`，`data` 由协议自定，如 `{"address","value"}` / `{"item","value"}`）
- **失败/超时 rs-iot 不重试**（工业写命令"最新值生效"语义）；**失联期间 rs-iot 丢弃写命令**，不排队旧命令

### 5.4 心跳（外部 → bridge 单向）
- 每 `heartbeat_secs / 2` 发 `Heartbeat{ts}`；bridge `heartbeat_secs` 未收任何帧 → 判失联断连
- **DataUp 也刷新活跃**（数据上行视为活跃）；bridge 侧不发心跳，仅检测超时

### 5.5 重连与缓存补传（外部自行实现，rs-iot 不规定）
- **断连检测**：心跳超时 / TCP 断 / HelloAck 未到 / 读错
- **重连**：指数退避，重 connect + Hello → 等 HelloAck + ConfigPush
- **缓存补传**：断连期间外部把采集数据存本地缓存（ring buffer / SQLite），重连后按 ts 顺序补传（DataUp 带原始 ts）

## 6. 最简骨架（语言无关伪代码）

```
function main():
    config = parse_args(inst_id, token, host, port)
    while true:                                  # 外层重连
        try:
            conn = tcp_connect(host, port)
            send_frame(conn, Hello, {inst_id, token, ver:"1"})
            ack = recv_frame(conn)               # HelloAck
            if not ack.ok: log(ack.msg); sleep(backoff); continue
            cfg = recv_frame(conn)               # ConfigPush（devices）
            devices = cfg.devices
            last_hb = now()
            spawn(collector(conn, devices))       # 周期采集 → DataUp
            while connected:
                frame = recv_frame(conn)          # 并发：Command / （DataUp 不回包）
                if frame.type == Command:
                    ok, err = handle_write(frame) # 用 children.value 定位点位
                    send_frame(conn, CommandAck, {cmd_id: frame.cmd_id, ok, err})
                if now() - last_hb > hb_secs/2:
                    send_frame(conn, Heartbeat, {ts: now()}); last_hb = now()
        catch Disconnected:
            cache_flush_pending()                 # 断连期间数据入缓存
            sleep(backoff_next())                 # 指数退避
```

## 7. 配置传递约定（ConfigPush 现状）

- 当前 `ConfigPush` **只下发 `devices`**（含 children 的 label/value/type），**不含 `app_config`**
- 因此协议连接参数（如 OPC DA 的 `prog-id`/`opc-host`、串口波特率等）当前经**外部进程自己的命令行参数 / 配置文件**传入
- 后续可扩展 `ConfigPush` 含 `app_config`，让连接参数也集中在 rs-iot（需改 `sidecar_client.rs` 序列化，见 [`external-plugin-protocol.md`](external-plugin-protocol.md)）

## 8. 调试技巧

- **验入库**：`redis-cli -p 6390 HGETALL rtdata:<devid>`（DataUp → bridge → pipeline → lux）
- **rs-iot 日志**：grep `[sidecarClient]`（连接/配置推送/断连/写命令丢弃均有日志）
- **查 sidecar 实例 health**（M4 统一埋点）：`GET /api/v1/inst/stats?inst_id=<ext>` → `{state, health}`（health 含 `connected`/`heartbeat` 扩展字段，经 `extra` 透传；每收 DataUp → `tick_ok`）
- **token 错**：外部会收到 `HelloAck{ok:false, msg}`，连接被关
- **配置变更生效**：`stopinst` + `startinst`（外部重连拿新 ConfigPush；PoC 不做自动 notify）
- **发写命令验证回路**：`POST /api/v1/command/send` → 外部打印/执行 → CommandAck

## 9. 安全

- **token**：创建外部实例时 rs-iot 生成**明文一次**返回，存 `hmac_sha256` hash 于 lux `ext_tokens`（校验用常量时间比对）。删实例自动清除
- **跨机部署**：token 明文 over TCP；`bind_host=0.0.0.0` 时 token **强制**校验（不可关）。敏感场景加 TLS（后续）
- **lux 不暴露**：外部永远拿不到 lux 句柄，所有数据经 bridge 透传
- **勿**把真实 token 写进 config.toml / 日志 / 版本库

## 10. 样例指引

| 样例 | 语言 | 协议 | 平台 | 看点 |
|---|---|---|---|---|
| [`ext-plugin-python/`](../examples/ext-plugin-python/) | Python 3.10+（仅标准库） | 模拟采集 | 跨平台 | 最小闭环：Hello/ConfigPush/DataUp/Command/Ack/Heartbeat + 断连缓存补传 + 指数退避。**移植到其他语言的首选参照** |
| [`ext-plugin-opcda-go/`](../examples/ext-plugin-opcda-go/) | Go（huskar-t/opcda） | OPC DA | Windows（COM） | 编译型语言 + 真实工业协议：SyncRead→DataUp、Command→item.Write（按 `GetCanonicalDataType` 自动转类型） |

> 协议要点（帧格式、握手顺序、并发处理、重连补传）见 [`external-plugin-protocol.md`](external-plugin-protocol.md) 第 11 节"参考实现"。
