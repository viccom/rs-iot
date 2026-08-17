# rs-iot 外部采集插件：OPC DA（Go）

sidecarClient bridge 协议 v1 的 Go 参考实现，连 OPC DA Server（如 `Matrikon.OPC.Simulation.1`），用 [huskar-t/opcda](https://github.com/huskar-t/opcda) 库（Windows COM）。证明 rs-iot 外部插件体系支持**编译型语言 + 真实工业协议**——OPC DA 是典型"Windows COM、rs-iot 跨平台 Rust"场景，外部 Go 进程在 Windows 跑连 DA Server，经 bridge 把数据交给任意平台的 rs-iot。

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

### 2. 建 External 实例（OPC itemIDs 作 `children.value`）

```bash
TOKEN=$(curl -s -XPOST http://localhost:8780/api/v1/user/login \
  -H 'Content-Type: application/json' \
  -d '{"user_id":"admin","pass":"admin"}' | jq -r .token)

curl -s -XPOST http://localhost:8780/api/v1/inst/set \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{
    "inst_id": "opc1",
    "object": {
      "inst_name": "opc da demo",
      "app_code": "sidecarClient",
      "app_type": "south",
      "host_kind": "external",
      "app_config": {"heartbeat_timeout_secs": 30},
      "devices": [{"label":"matrikon","value":"devOpc","children":[
        {"label":"Random.Boolean","value":"Random.Boolean","type":"bool"},
        {"label":"Random.Int1","value":"Random.Int1","type":"int"},
        {"label":"Random.Int2","value":"Random.Int2","type":"int"},
        {"label":"Random.Int4","value":"Random.Int4","type":"int"},
        {"label":"Random.Real4","value":"Random.Real4","type":"float"},
        {"label":"Random.Real8","value":"Random.Real8","type":"float"},
        {"label":"Random.String","value":"Random.String","type":"string"},
        {"label":"Random.Time","value":"Random.Time","type":"time"},
        {"label":"Random.UInt1","value":"Random.UInt1","type":"int"},
        {"label":"Random.UInt2","value":"Random.UInt2","type":"int"},
        {"label":"Random.UInt4","value":"Random.UInt4","type":"int"},
        {"label":"Bucket Brigade.Int1","value":"Bucket Brigade.Int1","type":"int"},
        {"label":"Bucket Brigade.Int2","value":"Bucket Brigade.Int2","type":"int"},
        {"label":"Bucket Brigade.Int4","value":"Bucket Brigade.Int4","type":"int"},
        {"label":"Bucket Brigade.Real4","value":"Bucket Brigade.Real4","type":"float"},
        {"label":"Bucket Brigade.Real8","value":"Bucket Brigade.Real8","type":"float"},
        {"label":"Bucket Brigade.String","value":"Bucket Brigade.String","type":"string"},
        {"label":"Bucket Brigade.Time","value":"Bucket Brigade.Time","type":"time"}
      ]}]
    }
  }' | jq .   # → {"status":"ok","plugin_token":"<hex>"}

curl -s -XPOST http://localhost:8780/api/v1/startinst \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"inst_id":"opc1"}'
```

> **`children.value` = OPC itemID**（如 `Random.Boolean`）。
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
  --bridge-host 127.0.0.1 --bridge-port 8781 \
  --prog-id Matrikon.OPC.Simulation.1 --opc-host localhost --period 2
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

- `--prog-id` / `--opc-host` / `--period` 经**命令行参数**传（PoC）。
- OPC items 来自 rs-iot **ConfigPush**（`children.value`）。
- 当前 ConfigPush 只下发 devices（不含 app_config），故 DA Server 连接信息走命令行。后续可扩展 ConfigPush 含 app_config，让 prog-id/opc-host 也集中在 rs-iot（需改 `sidecar_client.rs` ConfigPush 序列化，见 `docs/external-plugin-protocol.md`）。
