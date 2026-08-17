# rs-iot 外部插件 Python 样例

sidecarClient bridge 协议 v1 的 Python 参考实现。供 Go/C/Java/Node 等其他语言移植参照。

## 能力演示

- ✅ TCP 连接 + Hello 握手 + token 鉴权
- ✅ 接收 HelloAck + ConfigPush（devices/children）
- ✅ 周期模拟采集 → DataUp 上报
- ✅ 接收写命令 Command → 执行（打印）→ 回 CommandAck
- ✅ 定时心跳 Heartbeat
- ✅ 断连缓存（内存队列）+ 重连后按 ts 顺序补传
- ✅ 指数退避重连

无第三方依赖，仅标准库（`asyncio` + `struct` + `json`）。要求 Python 3.10+。

## 前置：rs-iot 侧准备

1. `config.toml` 启用 bridge：

   ```toml
   [bridge]
   enable = true
   bind_host = "127.0.0.1"   # 跨机改 "0.0.0.0"
   port = 8781
   heartbeat_timeout_secs = 15
   accept_hello_timeout_secs = 10
   ```

2. 启动 rs-iot：`./target/debug/rs-iot --config config.toml`

3. REST 建外部插件实例（`host_kind=external`），拿一次性返回的 `plugin_token`：

   ```bash
   # 登录拿 JWT
   TOKEN=$(curl -s -XPOST http://localhost:8780/api/v1/user/login \
     -H 'Content-Type: application/json' \
     -d '{"user_id":"admin","pass":"admin"}' | jq -r .token)

   # 建实例（注意 host_kind:"external"）→ 响应含 plugin_token（仅此次）
   curl -s -XPOST http://localhost:8780/api/v1/inst/set \
     -H "Authorization: Bearer $TOKEN" \
     -H 'Content-Type: application/json' \
     -d '{
       "inst_id": "ext1",
       "object": {
         "inst_name": "python demo",
         "app_code": "sidecarClient",
         "app_type": "south",
         "host_kind": "external",
         "app_config": {"heartbeat_timeout_secs": 30},
         "devices": [{"label":"demo","value":"devP","children":[
           {"label":"温度","value":"temp","type":"float"},
           {"label":"湿度","value":"humidity","type":"float"}
         ]}]
       }
     }' | jq .   # → {"status":"ok","plugin_token":"<hex>"}

   # 启动实例（注册 slot，等外部接入）
   curl -s -XPOST http://localhost:8780/api/v1/startinst \
     -H "Authorization: Bearer $TOKEN" \
     -H 'Content-Type: application/json' \
     -d '{"inst_id":"ext1"}'
   ```

## 运行

```bash
python main.py --inst ext1 --token <上一步的 plugin_token> --host 127.0.0.1 --port 8781
```

## 验证

```bash
# 看 rtdata 入库（DataUp 经 bridge → pipeline → lux）
redis-cli -p 6390 HGETALL rtdata:devP

# 发写命令（经 CtlRegistry → bridge → 外部）
curl -s -XPOST http://localhost:8780/api/v1/command/send \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"instance":"ext1","devid":"devP","cmd":"write","data":"{\"address\":\"temp\",\"value\":50}"}'
# → Python 进程打印 CMD #1 write: ...
```

## 断连补传验证

运行中 `Ctrl+C` 停 Python（缓存积压），等几秒重启 `python main.py ...`，观察：
- 重连后先补传断连期间的 DataUp（带原始 ts）
- rs-iot 日志 `[sidecarClient] inst ext1 external connected, config pushed`

## 移植到其他语言

协议要点（详见 [`../../../docs/external-plugin-protocol.md`](../../../docs/external-plugin-protocol.md)）：
- 帧 = `[4B 大端 length][1B type][JSON body]`，`length = 1 + len(body)`
- 握手必须先发 Hello（带 inst_id/token/ver），等 HelloAck{ok:true} + ConfigPush
- 心跳/采集/命令接收并发处理
- 断连重连 + 缓存补传由外部自行实现（rs-iot 不规定）
