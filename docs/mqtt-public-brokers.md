# 公共 MQTT Broker 清单（端到端测试备用）

> 来源：用户提供（2026-08-05）。rs-iot 北向发布验证用。
> **当前 `config.toml` 配置 `host = "broker.emqx.io"`** —— 该地址在下方 EMQX 清单中合法可用，无需修改。
> 若连不上，可替换为 `broker-cn.emqx.io`（EMQX 中国节点，同端口 1883）。
>
> ⚠️ 全部仅限测试/学习/原型开发，无隐私与可靠性保障，**勿用于生产**。

## 一、EMQX 公共 Broker（中国节点）

| 协议 | 服务器 | 端口 | 备注 |
|---|---|---|---|
| TCP | broker-cn.emqx.io | 1883 | 可订阅 testtopic/# |
| TCP/TLS | broker-cn.emqx.io | 8883 | 可订阅 testtopic/# |
| Websocket | broker-cn.emqx.io | 8083 | 可订阅 testtopic/# |
| Websocket/TLS | broker-cn.emqx.io | 8084 | 可订阅 testtopic/# |

## 二、EMQX 公共 Broker（国际节点）

- **地址**: broker.emqx.io
- **端口**: TCP 1883 / WebSocket 8083 / TCP/TLS 8883 / WebSocket/TLS 8084
- **特点**: MQTT 3.1 / 3.1.1 / 5.0，免费免注册，100 万免费会话分钟/月
- **注意**: 数据无隐私保障，仅测试

## 三、HiveMQ Public Broker

- **地址**: broker.hivemq.com
- **端口**: TCP 1883 / WebSocket 8000
- **特点**: MQTT 3.1.1 + WebSocket，免注册，提供浏览器客户端调试
- **注意**: 不适合生产，可能有法律和隐私限制

## 四、Mosquitto Test Server

| 协议 | 端口 | 备注 |
|---|---|---|
| TCP | 1883 | 无加密，无认证 |
| TCP | 1884 | 无加密，有认证 |
| TCP/TLS | 8883 | 有加密，无认证 |
| TCP/TLS | 8884 | 有加密，客户端证书 |
| TCP/TLS | 8885 | 有加密，有认证 |
| TCP/TLS | 8886 | 有加密，无认证 |
| TCP/TLS | 8887 | 有加密，服务器证书故意过期 |
| Websocket | 8080 | 无加密，无认证 |
| Websocket/TLS | 8081 | 有加密，无认证 |
| Websocket | 8090 | 无加密，有认证 |
| Websocket/TLS | 8091 | 有加密，有认证 |

- **地址**: test.mosquitto.org，支持 MQTT 3.1 / 3.1.1 / 5.0
- **TLS 证书**: mosquitto.org.crt 或 mosquitto.org.der
- **注意**: 运行实验性代码，可能不稳定

## 五、Flespi MQTT Broker

- **地址**: mqtt.flespi.io
- **端口**: TCP 1883 / WebSocket 80 / TLS 8883 / WebSocket/TLS 443
- **特点**: MQTT 5.0，私有命名空间 + REST API + ACL，免注册
- **注意**: 数据公开，建议用唯一主题前缀避免冲突，仅测试

## 六、MQTTServer.eu

- **地址**: public.mqttserver.eu
- **端口**: TCP 1883 / WebSocket 8080 / TLS 8883 / WebSocket/TLS 8081
- **特点**: 免费公共 + 免费私有（需注册），MQTT 3.1 / 5.0
- **注意**: 保留消息可能定期删除，生产用付费计划

## 七、MQTTHQ

- **地址**: public.mqtthq.com
- **端口**: TCP 1883 / WebSocket 80
- **特点**: 高可用多节点集群，专为 IoT 开发设计
- **注意**: 数据无隐私保护，仅开发/测试

## 八、Bevywise Public Broker

- **地址**: public-mqtt-broker.bevywise.com 或 broker.bevywise.com
- **端口**: TCP 1883 / WebSocket 10443
- **特点**: MQTT 3.1 / 3.1.1，无认证无 TLS
- **注意**: 不支持长期数据存储，仅开发；提供受限免费私有实例

## 九、Eclipse Public Broker

- **地址**: mqtt.eclipseprojects.io
- **端口**: TCP 1883 / TLS 8883
- **特点**: Eclipse 开源项目维护，免费公共

## 十、其他（需注册/特殊获取）

- **IoT with Us**: 地址需通过其 Telegram 频道获取，免用户名/密码，建议用唯一主题（unique-id/*），仅测试
- **MyQttHub**: myqtthub.com（需注册免费计划），MQTT + HTTPS API，社区论坛支持，免费计划功能有限

---

## rs-iot 端到端验证命令参考

```bash
# 订阅北向发布（rs-iot 以 <hwid>/<devid>/datas 主题发布）
mosquitto_sub -h broker.emqx.io -t '+/devA/datas' -v        # 国际节点（当前 config 同款）
mosquitto_sub -h broker-cn.emqx.io -t '+/devA/datas' -v     # 中国节点
mosquitto_sub -h test.mosquitto.org -t '+/devA/datas' -v    # Mosquitto
```
