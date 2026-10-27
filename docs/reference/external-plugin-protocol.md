# rs-iot 外部插件 Bridge 协议规约（v3）

> **状态**：v3 已实施（v2 + ConfigPush 追加 `app_config`/`devices[].bind` + 版本集合协商）；v2 客户端继续可用，v1 及未知版本拒连

rs-iot 通过 **sidecarClient bridge** 接入任意语言（C/C++/Go/Rust/Java/Python/Node…）的外部南向采集插件。外部插件是**纯运行时**：配置/数据/状态全在 rs-iot，本地仅缓存数据待补传。经 TCP 连 bridge，token 鉴权，lux 不对暴露（方案 C）。

## 1. 总览

```
外部进程(任意语言) ──TCP──► rs-iot bridge(默认 :8781)
                              │ acceptor: 读 Hello → 校验 token → 分发到实例 task
                              ▼
                    sidecarClient task（每外部实例一个）
                              │ DataUp → Sample → pipeline → lux rtdata
                              │ 写命令 ← CtlRegistry ← MQTT / REST
                              ▼
                       lux（loopback，外部永不触碰）
```

| 项 | 值 |
|---|---|
| 端口 | `config.bridge.port`（默认 8781） |
| bind | `config.bridge.bind_host`（默认 `127.0.0.1`；跨机改 `0.0.0.0`） |
| 启用 | `config.bridge.enable = true`（默认 `false`，现有部署零影响） |
| 协议版本 | `2`（`Hello.ver`；v1 拒连带升级提示） |

## 2. 帧格式

每帧 = `[4 字节大端 length][1 字节 type][JSON body (utf-8)]`，其中 `length = 1 + body.len()`。

- **length**：4 字节大端 u32，= 后续字节数（type + body）。最大 4 MiB。
- **type**：1 字节帧类型（见下表）。
- **body**：JSON（Heartbeat 可为空）。

> 等价于 tokio `LengthDelimitedCodec` 默认（4B BE u32 长度前缀）。

## 3. 帧类型

| type | 值 | 名称 | 方向 | body 字段 |
|---|---|---|---|---|
| Hello | 0x01 | 握手 | ext→bridge | `{inst_id, token, ver}` |
| HelloAck | 0x02 | 握手应答 | bridge→ext | `{ok, msg, heartbeat_secs}` |
| ConfigPush | 0x03 | 配置下发 | bridge→ext | `{inst_id, v, devices:[...]}` + v3 `app_config` |
| DataUp | 0x04 | 数据上行 | ext→bridge | `{devid, samples:[{tag,value,ts},...]}` |
| Command | 0x05 | 写命令 | bridge→ext | `{cmd_id, devid, cmd, data}` |
| CommandAck | 0x06 | 写命令应答 | ext→bridge | `{cmd_id, ok, err?}` |
| Heartbeat | 0x07 | 心跳 | ext→bridge | `{ts}` 或空（bridge 仅检测超时，不发） |

## 4. 连接与握手

1. 外部进程 TCP connect bridge 端口
2. 外部发 `Hello{inst_id, token, ver}`（**版本协商**：宿主支持 `"2"` / `"3"`，集合匹配；`ver` 缺失按 `"2"` 处理；不在支持集 → `HelloAck{ok:false, msg}` 点名版本并列出支持集）
3. bridge 校验 token（`hmac_sha256(jwt_secret, token)` 与 lux `ext_tokens` 存的 hash 比对）：
   - **通过** → 回 `HelloAck{ok:true, heartbeat_secs}`，紧接发 `ConfigPush`（载荷形态按 `ver` 决定）
   - **失败**（token 错 / 版本不符 / 实例未启动 / slot 满）→ `HelloAck{ok:false, msg}`，连接关闭
4. 外部进入数据/命令/心跳循环

## 5. 数据上行 DataUp

外部采集后发 `DataUp{devid, samples:[{tag, value, ts}, ...]}`。

- **批量**：一帧可带多 sample（不同 ts 也行），bridge 拆成多个 Sample 逐个入库
- **value**：JSON 数字 / 布尔 / 字符串（bridge 按 `SampleValue` 解析：整数→Int、浮点→Float、布尔→Bool、字符串→Str）
- **ts**：Unix 秒，用**采集时刻**（非发送时刻）——补传时保留原 ts
- **校验闸（M-3）**：devid 必须 ∈ 本实例绑定设备 ∧ tag 必须 ∈ 该设备模型点表（ConfigPush v2 的 `tags[].name`）——未建模 sample 被 bridge 拒收（warn + `gate_rejected` 计数，被拒 tag 进 `discoveries` 发现队列供 rs-iot 侧人工 adopt 建模——数据本身不入库），合法 sample 正常入库

## 6. 写命令 Command

rs-iot 收到写命令（MQTT `<hwid>/command` 或 REST `POST /api/v1/command/send`）→ `Command{cmd_id, devid, cmd, data}` → 外部执行 → 回 `CommandAck{cmd_id, ok, err?}`。

- `cmd_id`：bridge 递增 u64，用于关联 Ack
- `cmd` / `data`：透传 `CtlMessage`（`cmd="write"`，`data` 由具体协议自定，如 modbus `{"address","value"}`）
- 失败/超时：bridge 仅 warn，**不重试**（工业写命令"最新值生效"语义）
- **失联期间**：bridge 丢弃写命令 + warn，不排队旧命令

## 7. 心跳（外部 → bridge 单向）

外部每 `heartbeat_secs / 2` 发 `Heartbeat{ts}`；bridge `heartbeat_secs` 未收任何帧（Heartbeat 或 DataUp）→ 判失联断连。DataUp 也刷新活跃（数据上行视为活跃）。**bridge 侧不发心跳**，仅检测超时——外部单向发即可。

## 8. 重连与缓存补传（外部实现，rs-iot 不规定）

- **断连检测**：心跳超时 / TCP 断 / HelloAck 未到 / 连接读错
- **重连**：指数退避，重 connect + Hello → 等 HelloAck + ConfigPush
- **缓存补传**：断连期间外部把采集数据存本地缓存（ring buffer / SQLite 随意），重连后按 ts 顺序补传（DataUp 带原始 ts）
- **bridge 侧**：slot 支持外部多次重连，新连接经 acceptor 重新校验 + 分发到同一实例 task

## 9. 配置 ConfigPush

握手后 + 每次重连后下发。载荷形态由 Hello 的 `ver` 决定：**v2 只有 `devices`；v3 追加顶层 `app_config` 与 `devices[].bind`**（v2 客户端零影响——宿主按版本分支组装）。

### 9.1 v2 载荷（devices 点表）

```json
{
  "inst_id": "ext1",
  "v": 2,
  "devices": [
    {
      "devid": "devT",
      "model": "vx_dtz341",
      "tags": [
        {"name": "温度", "type": "f32", "addr": "holding:100:f32", "unit": "V", "rw": "r"},
        {"name": "湿度", "type": "f32", "addr": "holding:124:f32", "unit": null, "rw": "r"}
      ]
    }
  ]
}
```

- `devid`：设备 ID（`rtdata:<devid>` 的 key，DataUp 的 `devid`）
- `model`：设备模型 id（元数据追溯）
- `tags[]`：设备模型点表——`name`（DataUp 的 `tag`）、`type`（值类型）、`addr`（已解析绝对地址或纯软 tag 名）、`unit`（单位，可 null）、`rw`（"r"/"rw"）
- **外部按 `tags[].name` 采集/上报**（`addr` 供协议寻址参考；外部自有协议解释）

### 9.2 v3 追加字段（连接参数与设备级绑定）

v3 = v2 全部字段 **+ 两处新增**（`devices[]` 其余字段与 v2 逐字节一致）：

```json
{
  "inst_id": "opcda-1",
  "v": 3,
  "app_config": {
    "prog_id": "Matrikon.OPC.Simulation.1",
    "host": "192.168.1.20",
    "username": "opcuser",
    "password": "解析后的明文"
  },
  "devices": [
    {
      "devid": "devOpc",
      "model": "vx_dtz341",
      "bind": {"group": "g1", "update_rate": "1000"},
      "tags": [
        {"name": "temp", "type": "f32", "addr": "Random.Real4", "unit": "C", "rw": "r"}
      ]
    }
  ]
}
```

- **`app_config`**：实例的 `app_config` 原样（自由 JSON，各协议自行解释）——连接级参数（服务器地址、账号、协议私有键）**集中在 rs-iot 管理**，外部进程无需本地配置文件。**宿主内部键不下发**：`secrets`（脚本侧授权清单）、`heartbeat_timeout_secs`（已由 `HelloAck.heartbeat_secs` 下发）。非对象 `app_config` 统一为 `{}`。
- **`devices[].bind`**：设备级协议参数（`DeviceBinding.bind` 原样，`BTreeMap<String,String>`）——OPC DA 的 group/更新速率、Modbus 的 unit id 等。空 bind 下发 `{}`（形态稳定，外部按存在性判断能力）。
- **凭证**：`app_config.password` 若为 `secret:<ref>` 形式，宿主在组装 ConfigPush 前解析成**明文**下发（复用 north 的同一解析函数）。引用未写入或解析失败 → 下发 `null` + 宿主 warn，**不拒连**（外部按"无凭证"自行处理）。字面值原样下发。

**配置变更生效**：重启实例（REST `stopinst` + `startinst`），外部重连拿新 ConfigPush。不做自动 notify（增量推送需 diff 协议 + 版本序号 + 回执三件套；重启是秒级操作，且外部本就要处理重连）。

## 10. 安全

- **token**：创建外部插件实例时（REST `set`，`host_kind=external`）rs-iot 生成并返回**明文一次**，存 `hmac_sha256` hash 于 lux `ext_tokens`（校验用常量时间 `mac.verify_slice`，避免时序泄漏）。删实例时清除。**注意**：每次 `set` 都重新签发并覆盖旧 token（旧 token 即失效）——重复 set 同实例等于轮换 token，外部需用新 token 重连。
- **lux 不暴露**（方案 C）：外部只连 bridge，认证/授权集中在 rs-iot
- **v3 明文凭证过 bridge**：`app_config.password`（含 `secret:` 解析后的明文）随 ConfigPush 下发——**token 保护的是"谁能连"，不是"通道是否加密"**。默认 `bind_host=127.0.0.1`（同机）风险可控；跨机部署必须 TLS 或隔离网段。
- **跨机部署**：token 与 v3 凭证均明文 over TCP，敏感场景加 TLS（后续）

## 11. 参考实现

- **Python 样例**：`examples/ext-plugin-python/`（可运行，含缓存补传）
- **bridge 侧（Rust）**：`src/plugin/builtin/sidecar_client/mod.rs`、`src/db/ext_token.rs`、`src/host.rs`（`BridgeRegistry`）
- **端到端测试**：`tests/sidecar_e2e.rs`
