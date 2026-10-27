# rs-iot 外部采集插件：OPC DA（Go）

sidecarClient bridge 协议 **v2** 的 Go 参考实现（模型点表形态），连 OPC DA Server（如 `Matrikon.OPC.Simulation.1`），用 [huskar-t/opcda](https://github.com/huskar-t/opcda) 库（Windows COM）。证明 rs-iot 外部插件体系支持**编译型语言 + 真实工业协议**——OPC DA 是典型"Windows COM、rs-iot 跨平台 Rust"场景，外部 Go 进程在 Windows 跑连 DA Server，经 bridge 把数据交给任意平台的 rs-iot。

## 能力

- ✅ TCP 连 bridge + Hello 握手 + token 鉴权
- ✅ 收 ConfigPush（OPC itemIDs）→ opcda Connect + AddItems
- ✅ 周期同步读（`SyncRead`）→ DataUp
- ✅ 写命令 Command → 按 item 原生类型（`GetCanonicalDataType`）转换 → opcda Write → CommandAck
- ✅ 心跳 + 断连指数退避重连（bridge + DA Server 每 session 重建）

**仅 Windows**（opcda 依赖 COM）。需 Go 1.20+。

## 前置

### 1. rs-iot 开 bridge

```toml
# config.toml
[bridge]
enable = true
bind_host = "127.0.0.1"
port = 8781
```

### 2. 建设备模型 + 设备 + External 实例（OPC itemID 作 tag 的 `addr`）

```bash
TOKEN=$(curl -s -XPOST http://localhost:8780/api/v1/user/login \
  -H 'Content-Type: application/json' \
  -d '{"user_id":"admin","pass":"admin"}' | jq -r .token)

# 2a. 建模型（tag.addr = OPC itemID；纯软模型 addr 也可写成 tag 名）
curl -s -XPOST http://localhost:8780/api/v1/devmodel/set \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{
    "model_id": "matrikon_sim",
    "vendor": "Matrikon",
    "model_no": "OPC.Simulation.1",
    "tags": [
      {"name":"bool_val","data_type":"bool","addr":"Random.Boolean"},
      {"name":"int1","data_type":"i32","addr":"Random.Int1"},
      {"name":"int2","data_type":"i32","addr":"Random.Int2"},
      {"name":"int4","data_type":"i32","addr":"Random.Int4"},
      {"name":"real4","data_type":"f32","addr":"Random.Real4"},
      {"name":"real8","data_type":"f64","addr":"Random.Real8"},
      {"name":"str_val","data_type":"string","addr":"Random.String"},
      {"name":"time_val","data_type":"string","addr":"Random.Time"},
      {"name":"uint1","data_type":"u8","addr":"Random.UInt1"},
      {"name":"uint2","data_type":"u16","addr":"Random.UInt2"},
      {"name":"uint4","data_type":"u32","addr":"Random.UInt4"},
      {"name":"bb_int1","data_type":"i32","addr":"Bucket Brigade.Int1","rw":"rw"},
      {"name":"bb_int2","data_type":"i32","addr":"Bucket Brigade.Int2","rw":"rw"},
      {"name":"bb_int4","data_type":"i32","addr":"Bucket Brigade.Int4","rw":"rw"},
      {"name":"bb_real4","data_type":"f32","addr":"Bucket Brigade.Real4","rw":"rw"},
      {"name":"bb_real8","data_type":"f64","addr":"Bucket Brigade.Real8","rw":"rw"},
      {"name":"bb_str","data_type":"string","addr":"Bucket Brigade.String","rw":"rw"},
      {"name":"bb_time","data_type":"string","addr":"Bucket Brigade.Time","rw":"rw"}
    ]
  }'

# 2b. 建设备（params 按模型 params 齐套；本模型无 params）
curl -s -XPOST http://localhost:8780/api/v1/devreg/set \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"devid":"devOpc","model":"matrikon_sim","desc":"Matrikon 仿真"}'

# 2c. 建 External 实例（devices 只写绑定；连接参数在 app_config）
curl -s -XPOST http://localhost:8780/api/v1/inst/set \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{
    "inst_id": "opc1",
    "object": {
      "inst_name": "opc da demo",
      "app_code": "sidecarClient",
      "app_type": "south",
      "host_kind": "external",
      "app_config": {
        "heartbeat_timeout_secs": 30,
        "prog_id": "Matrikon.OPC.Simulation.1",
        "opc_host": "localhost",
        "period": 2
      },
      "devices": [{"devid": "devOpc", "bind": {"group": "matrikon"}}]
    }
  }' | jq .   # → {"status":"ok","plugin_token":"<hex>"}

curl -s -XPOST http://localhost:8780/api/v1/startinst \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"inst_id":"opc1"}'
```

> **`tags[].addr` = OPC itemID**（如 `Random.Boolean`）——bridge 在 ConfigPush 里下发已解析的 `addr`，外部插件直接拿它做 `AddItems`。
> **上报用 `tags[].name`**（如 `bool_val`）——DataUp 的 `tag` 必须是模型点表名，用 itemID 上报会被校验闸拒收。
> **Money 类型跳过**：`Random.Money` / `Bucket Brigade.Money` 是 `VT_CY`（currency），opcda 不支持，AddItem 会失败并被插件跳过（warn），不影响其余 item。
> **Qualities 跳过**：`Random.Qualities` 类型异常，同样容错跳过。

### 3. opcda 依赖

`go.mod` 已 `replace github.com/huskar-t/opcda => ../../../opcda`（opcda 源码在 rs-iot 同级 `E:\github.com\opcda`）。首次 `go build` 会下载 `golang.org/x/sys`（opcda 的间接依赖）。

## 编译（Windows）

```bash
cd examples/ext-plugin-opcda-go
go build -o ext-opcda.exe .
```

## 运行

```bash
./ext-opcda.exe --inst opc1 --token <plugin_token> \
  --bridge-host 127.0.0.1 --bridge-port 8781
```

## 验证

```bash
# 采集入库（经 bridge → pipeline → lux rtdata）
redis-cli -p 6390 HGETALL rtdata:devOpc

# 写命令（Bucket Brigade.* 可写；Random.* 只读，写会返回 ok:false err）
curl -s -XPOST http://localhost:8780/api/v1/command/send \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"instance":"opc1","devid":"devOpc","cmd":"write",
       "data":"{\"item\":\"Bucket Brigade.Real4\",\"value\":12.34}"}'
# → Go 进程打印：写 Bucket Brigade.Real4 = 12.34（VT_R4）成功
```

## 写类型自动转换

OPC DA 严格类型（类型不匹配 → `BadTypeMismatch`）。Go 插件按 `item.GetCanonicalDataType()` 自动转换写值：

| OPC VT | Go 类型 | JSON value |
|---|---|---|
| VT_BOOL | bool | true/false |
| VT_I1/UI1/I2/UI2/I4/UI4/I8/UI8 | int8..uint64 | 数字 |
| VT_R4 | float32 | 数字 |
| VT_R8 | float64 | 数字 |
| VT_BSTR | string | 字符串 |
| VT_DATE | time.Time | RFC3339 字符串 / Unix 秒 |

写命令 `data` JSON：`{"item":"<itemID>","value":<值>}`，value 按上表传，插件自动转目标类型。

## 配置传递说明

本示例走 **协议 v2**：只消费 ConfigPush 的 `devices[].tags[]`（`name` 上报 / `addr` 寻址）。

`--prog-id` / `--opc-host` / `--period` 仍是**命令行参数**——v2 不下发 `app_config`。
若要用**协议 v3** 让连接参数也集中在 rs-iot（`app_config.prog_id` / `opc_host` / `period`，
密码可写 `secret:<ref>` 由宿主解析），改三处即可：

1. `const protoVer = "3"`
2. `configPush` struct 加 `AppConfig map[string]any `json:"app_config"``
3. 从 `cfg.AppConfig` 取 `prog_id` / `opc_host` / `period`，删对应 flag

v3 载荷字段见 `docs/reference/external-plugin-protocol.md` §9.2。
