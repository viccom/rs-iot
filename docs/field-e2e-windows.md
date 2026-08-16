# Windows 端到端测试环境搭建与测试手册（E:\rsiot-field 测试场）

> 本文是 rs-iot 在 Windows 下搭建"真实工厂级"端到端测试场的完整手册：
> 设备模拟器（Modbus TCP / RTU 串口 / OPC UA）+ warden 进程监护 + rs-iot 三件套
> + 北向 MQTT 公共 broker + Reasonix AI 助手全链路。
> 配套资产在仓库 `field-kit/` 目录（启动脚本 + 全部配置模板 + 打包脚本）。

## 一、拓扑与端口总览

```
                          ┌─ warden 控制台 :8789（监护全部服务，Web UI）─┐
                          │                                            │
 浏览器 :8080 ──► rsiot-gateway ─┬─ /api/rsiot/* ► rs-iot REST :8790 ◄─┤ modbus-tcp 实例 ─► 模拟器 :502
 (嵌入前端 dist)                 └─ /api/agent/* ► reasonix serve :8787 ◄─┤ modbus-rtu 实例 ─► COM2 ⇄ COM3(模拟器 RTU 从站)
                                     └─ stdio spawn rsiot-mcp.exe        └─ opcua 实例 ────► 模拟器 opc.tcp://127.0.0.1:4840
 rs-iot 内嵌 lux RESP :6390（redis-cli 直查）        模拟器控制台 :8090
 北向 MQTT：broker.emqx.io:1883（上行 <hwid>/<devid>/datas，下行 <hwid>/command）
```

| 组件 | 端口/路径 | 说明 |
|---|---|---|
| 设备模拟器 simulator | Modbus TCP :502 / RTU COM3 / OPC UA :4840 / 控制台 :8090 | 100 点表（四种 Modbus 区、全部数据类型、多种源），源码 `G:\github\simulator` |
| rs-iot 主程序 | REST :8790 / lux :6390 | 工业采集核心 |
| reasonix serve | :8787 | AI agent 宿主（token 鉴权），spawn rsiot-mcp |
| rsiot-gateway | :8080 | 前端入口 + 反代 rs-iot/reasonix |
| warden | :8789 | 进程监护（启停/健康检查/日志/崩溃自愈/Job Object 清进程树） |

## 二、资产来源与目录结构

### 2.1 二进制来源

| 文件 | 来源 |
|---|---|
| `bin/rs-iot.exe`、`bin/rsiot-gateway.exe`、`bin/rsiot-mcp.exe` | rs-iot 仓库 `cargo build -j 2 --release -p rs-iot -p rsiot-gateway -p rsiot-mcp`（前端改动需先 `cd frontend && npm run build` 再重编 gateway 嵌入） |
| `warden/warden.exe` | warden 仓库（`E:\github.com\warden`）`cargo build --release` |
| `bin/reasonix.exe` | Reasonix 官方发布单二进制（Go） |
| `simulator/dist/simulator.exe` | 模拟器仓库（`G:\github\simulator`）发行包 **simulator-dist.zip 解压即用，必备** |
| `simulator/com0com/` | com0com 虚拟串口驱动包（见下节） |

### 2.2 测试场目录结构（`E:\rsiot-field`，任意盘符路径均可——脚本用 `%~dp0` 自定位）

```
E:\rsiot-field\
├── start-rsiot.bat                 # 一键冷启动（6 步编排，幂等，见 field-kit/）
├── stop-rsiot.bat                  # 一键停止（stop-all + 杀 warden，Job Object 清树）
├── config.toml                     # rs-iot 配置（api_port 8790 / lux 6390 / [mqtt] 公共 broker）
├── gateway.toml                    # 网关配置（jwt_secret 与 config.toml 同值；[reasonix] workspace_dir 指 work/）
├── bin\                            # rs-iot.exe / rsiot-gateway.exe / rsiot-mcp.exe / reasonix.exe
├── warden\
│   ├── warden.exe
│   ├── config\services.toml.template   # __ROOT__ 占位模板（启动时生成 services.toml）
│   └── data\                          # 服务日志按日落盘 + 期望状态
├── work\                           # reasonix 工作目录
│   └── reasonix.toml.template      # __ROOT__/__JWT__ 占位模板（启动时生成）
├── home\                           # REASONIX_HOME（config.toml providers + .env 密钥，绝不入 git）
├── simulator\
│   ├── dist\                       # simulator.exe + config.yaml + points-100.yaml
│   └── com0com\                    # 虚拟串口驱动包
├── scripts\                        # AI 写命令等脚本产物（可写沙箱目录）
├── data\  logs\                    # rs-iot 运行时数据（全新初始化 = 删 data/）
```

## 三、com0com 虚拟串口对（Modbus RTU 测试必备）

com0com 是 Windows 内核态空猫（null-modem）串口对模拟器：一对端口互为收发
（CNCA0 的输出即 CNCB0 的输入）。本测试场用 **COM2⇄COM3**：rs-iot 作 RTU 主站开
COM2，模拟器作 RTU 从站开 COM3。

### 3.1 驱动安装（simulator\com0com\ 为 2017 版 3.0 驱动裸包）

标准方式是官方 `setup.exe` 安装器（SourceForge com0com 项目页下载）；本测试场的
裸包（含 `com0com.inf/.sys/.cat` + `setupc.exe` 命令行 + `setupg.exe` 图形界面）按以下步骤：

1. **管理员**命令行进入驱动目录，安装总线驱动与端口驱动：
   ```bat
   pnputil /add-install com0com.inf
   pnputil /add-install cncport.inf
   ```
2. x64 系统：该驱动为测试签名。若驱动加载失败（设备管理器黄色感叹号/代码 39），
   需开启测试签名并重启（**会降低系统安全性，仅测试机使用**）：
   ```bat
   bcdedit.exe -set TESTSIGNING ON
   ```
3. 已签名正规渠道：从 SourceForge 下载带 setup.exe 的完整安装包，安装向导会自动
   处理驱动并预建一对端口。

### 3.2 创建/查看串口对（管理员）

```bat
cd /d E:\rsiot-field\simulator\com0com
setupc.exe install PortName=COM2 PortName=COM3   &REM 创建一对（本场默认）
setupc.exe install PortName=COM4 PortName=COM5   &REM 第二对（备用）
setupc.exe list                                   &REM 查看现有对，实机输出：
       CNCA0 PortName=COM2
       CNCB0 PortName=COM3
       CNCA1 PortName=COM4
       CNCB1 PortName=COM5
       CNCA2 PortName=COM6
       CNCB2 PortName=COM7
```

图形界面等价操作：`setupg.exe` → Add pair。删除对：`setupc.exe remove 0`（对编号）。
验收：两端各开一个串口终端互发字符即可对通；或直接跑本手册第五节 RTU 实例。

## 四、warden 进程监护配置管理

warden 是 supervisord 风格的监护 daemon：统一拉起/监护/观测一组本地进程。
**服务定义唯一权威源是 `warden\config\services.toml`**（由 `services.toml.template`
在每次启动时幂等生成，`__ROOT__` 替换为部署根正斜杠路径）。

### 4.1 services.toml 语义（每个 [[service]]）

| 字段 | 说明 |
|---|---|
| `command` / `args` / `working_dir` | 进程与工作目录（**路径一律正斜杠**，TOML 反斜杠是转义符） |
| `priority` | 小者先启动、后停止（本场：模拟器 0 → rs-iot 1 → reasonix 5 → gateway 10） |
| `auto_start` | warden daemon 启动时是否自动拉起。**本场全 false**——reasonix 需先注入 JWT，顺序编排由 start-rsiot.bat 走 API 完成 |
| `auto_restart` | 崩溃自愈（指数退避，超限熔断进 Failed 等手动重启）。核心服务 true，agent false |
| `graceful_timeout_secs` | 优雅停止等待，超时强杀**整棵进程树**（Windows Job Object，杀 reasonix 连带 rsiot-mcp 孙进程无孤儿） |
| `[service.health]` | TCP 端口探测，状态机 unknown→healthy/failed |
| `environment` | 注入环境变量（reasonix 的 REASONIX_HOME 等） |

### 4.2 运维 API 与控制台

```bash
curl http://127.0.0.1:8789/api/v1/health                          # daemon 健康
curl http://127.0.0.1:8789/api/v1/services                        # 服务清单+状态
curl -X POST http://127.0.0.1:8789/api/v1/services/rs-iot/start   # 启动/停止/重启
curl -X POST http://127.0.0.1:8789/api/v1/services/stop-all       # 全停（优雅+超时强杀）
curl "http://127.0.0.1:8789/api/v1/services/rs-iot/logs?tail=50"  # 服务日志（stdout/stderr 统一采集）
```

浏览器打开 `http://127.0.0.1:8789` 即控制台：服务启停/状态/实时日志(SSE)/资源指标。
**替换二进制前先 stop 该服务**（Windows 锁运行中的 exe）：`stop → cp → start`。

## 五、端到端测试全流程

### 5.1 冷启动 / 全新初始化

```bat
E:\rsiot-field\start-rsiot.bat
```
脚本编排（幂等，可重复执行）：生成 services.toml → 起 warden → 起模拟器 → 起
rs-iot → 轮询登录(admin/admin)取 JWT → 生成 work\reasonix.toml（注入
RSIOT_JWT/RSIOT_BASE）→ 起 reasonix → **设 yolo 模式**（测试场全流程免审批）→
起 gateway。bat 退出服务不受影响（warden 常驻）。

**全新系统初始化**（清空 lux 全部历史数据/用户/会话）：
```bat
stop-rsiot.bat
rmdir /s /q data logs
start-rsiot.bat
```
全新启动自动完成：admin/admin 用户种子；lux 无 autoStart 实例时启动 demo 实例
sim1（建真实实例后应删除：`POST /api/v1/inst/delete {"inst_id":"sim1"}`，级联清 rtdata）。

### 5.2 建采集实例（真实工厂流程：REST 建实例+点表→启动）

```bash
TOK=$(curl -s -X POST localhost:8790/api/v1/user/login -H "content-type: application/json" \
  -d '{"user_id":"admin","pass":"admin"}' | jq -r .token)
H="Authorization: Bearer $TOK"; CT="content-type: application/json"

# Modbus TCP（注意请求体是 {inst_id, object} 包裹结构）
curl -X POST localhost:8790/api/v1/inst/set -H "$H" -H "$CT" -d '{"inst_id":"modbus-tcp","object":{
  "inst_id":"modbus-tcp","app_code":"modbusClient","app_type":"south","auto_start":true,"host_kind":"builtin",
  "app_config":{"kind":"tcp","endpoint":"127.0.0.1:502","slave_id":1,"period":2},
  "devices":[{"label":"devMbTcp","value":"devMbTcp","children":[
    {"label":"const616","value":"holding:21:i16","type":"i16"},
    {"label":"sine","value":"holding:32:f32","type":"f32"},
    {"label":"input432","value":"input:21:u16","type":"u16"}]}]}}'

# Modbus RTU 串口（com0com COM2⇄COM3）
curl -X POST localhost:8790/api/v1/inst/set -H "$H" -H "$CT" -d '{"inst_id":"modbus-rtu","object":{
  "inst_id":"modbus-rtu","app_code":"modbusClient","app_type":"south","auto_start":true,"host_kind":"builtin",
  "app_config":{"kind":"rtu_serial","serial_port":"COM2","baud_rate":9600,"slave_id":1,"period":3},
  "devices":[{"label":"devMbRtu","value":"devMbRtu","children":[
    {"label":"const616","value":"holding:21:i16","type":"i16"}]}]}}'

# OPC UA（模拟器免安认证匿名端点；节点 ns=2;s=Points/<点名>）
curl -X POST localhost:8790/api/v1/inst/set -H "$H" -H "$CT" -d '{"inst_id":"opcua","object":{
  "inst_id":"opcua","app_code":"opcuaClient","app_type":"south","auto_start":true,"host_kind":"builtin",
  "app_config":{"endpoint":"opc.tcp://127.0.0.1:4840","security_policy":"none","period":3},
  "devices":[{"label":"devUa","value":"devUa","children":[
    {"label":"p008","value":"ns=2;s=Points/p008_i16","type":"i16"}]}]}}'

# 启动（路由是 /startinst /stopinst，不是 /inst/start）+ 清 demo
for i in modbus-tcp modbus-rtu opcua; do curl -X POST localhost:8790/api/v1/startinst -H "$H" -H "$CT" -d "{\"inst_id\":\"$i\"}"; done
curl -X POST localhost:8790/api/v1/inst/delete -H "$H" -H "$CT" -d '{"inst_id":"sim1"}'
```

### 5.3 采集验证（交叉核对）

```bash
redis-cli -p 6390 HGETALL rtdata:devMbTcp     # 字段名带前缀：<devid>.<tag>
redis-cli -p 6390 HGET rtdata:devMbTcp devMbTcp.const616_i16   # 恒定点应精确等于模拟器点表常量
curl localhost:8090/api/points | jq           # 模拟器控制台取"真值"对照
curl -H "$H" localhost:8790/api/v1/inst/stats # 三实例 running + tick 递增 + fail=0
```
核对要点：常量点精确相等（点表 616/432/203/65）；同秒采样 UA 与 Modbus 同点差值
≈ f32 精度；RTU 与 TCP 同刻读值完全一致。lux 是分层存储，`KEYS *` 枚举不全属正常，
用 HGETALL/HGET 查询。

### 5.4 写命令（注意：地址必须带全类型）

```bash
w() { curl -s -X POST localhost:8790/api/v1/command/send -H "$H" -H "$CT" \
  -d "{\"instance\":\"$1\",\"devid\":\"$2\",\"cmd\":\"write\",\"data\":$3}"; }
w modbus-tcp devMbTcp '"{\"address\":\"holding:10:u16\",\"value\":1234}"'   # FC06：holding:10 ❌ 会静默丢
w modbus-tcp devMbTcp '"{\"address\":\"holding:262:u32\",\"value\":305419896}"'  # FC16 多寄存器
w modbus-tcp devMbTcp '"{\"address\":\"holding:228:bit:5\",\"value\":1}"'  # bit RMW（目标寄存器须无源）
w opcua devUa '"{\"node_id\":\"ns=2;s=Points/p092_u32\",\"value\":987654,\"opc_type\":\"u32\"}"'
```
模拟器点表中 **source 为空的点是"客户端驱动"**，写入持久；带源（sine/step/constant）
的寄存器每 200ms 被源覆盖——bit RMW 目标必须选无源寄存器。

### 5.5 观测面

```bash
curl -X POST localhost:8790/api/v1/inst/trace -H "$H" -H "$CT" -d '{"inst_id":"modbus-tcp","duration_secs":20}'
curl -H "$H" "localhost:8790/api/v1/logs/recent?target=io_trace&limit=6"   # MBAP 帧级 TX/RX hex
curl localhost:8790/metrics | findstr instance                             # Prometheus 指标
```

### 5.6 北向 MQTT（公共 broker）

rs-iot 按 config.toml `[mqtt]` 段自动起发布器（无需建实例）：每 5s 上行
`<hwid>/<devid>/datas`，并订阅 `<hwid>/command` 接收下行命令。hwid 见启动日志
（`rs-iot starting hwid=...`）。验证（mosquitto 工具 `scoop install mosquitto`）：

```bash
HWID=eiotccbebfa2
mosquitto_sub -h broker.emqx.io -t "$HWID/+/datas" -C 2 -W 20 -v          # 上行实测
mosquitto_pub -h broker.emqx.io -t "$HWID/command" -m '{"instance":"modbus-tcp","devid":"devMbTcp","cmd":"write","data":"{\"address\":\"holding:10:u16\",\"value\":6666}"}'
redis-cli -p 6390 HGET rtdata:devMbTcp devMbTcp.holding10_u16             # 应回读 6666
```

### 5.7 AI 助手链路（yolo 模式）

启动脚本已自动设 yolo。读写全流程（warmup 预热 → 提问 → 观测工具调用）：
```bash
python scripts/test_serve_e2e.py "列出系统里所有的设备和实例，以及每台设备的点位数量。不要用 bash。"
python scripts/test_serve_e2e.py "读取 devMbTcp 设备的所有点位当前值，用表格列出。不要用 bash。"
```
**写操作提示词需含"先建立任务清单"**（Reasonix v1.25.3 写纪律：写类工具前置
todo+验收义务，与 yolo/审批模式无关——runtimepolicy/guards.go 的
ContractPreconditionGuard）：
```bash
python scripts/test_serve_e2e.py "按顺序执行：1) 先调用 todo_write 建立任务清单（写 holding:10:u16=4321、回读验证两步）；2) 调用 rsiot_send_device_command（instance modbus-tcp，dev_id devMbTcp，data 传 JSON 字符串 {\"address\":\"holding:10:u16\",\"value\":4321}）；3) rsiot_read_tags 回读验证；4) 标记完成。不要用 bash。"
```
注意 `rsiot_send_device_command` 的 `data` 参数必须传 **JSON 字符串**（不能传对象）。

## 六、脚本清单（端到端测试涉及）

| 脚本 | 位置 | 用途 |
|---|---|---|
| `start-rsiot.bat` / `stop-rsiot.bat` | 测试场根（仓库 `field-kit/`） | 冷启动编排 / 全停；ASCII+CRLF，勿转码 |
| `services.toml.template` / `reasonix.toml.template` | `field-kit/config/` | warden 服务定义与 reasonix 项目配置（占位符运行时替换） |
| `scripts/test_serve_e2e.py` | rs-iot 仓库 | serve 全流程 e2e（鉴权→yolo→MCP 预热→提交→SSE 观测→工具汇总） |
| `field-kit/pack-field-kit.ps1` | rs-iot 仓库 | 从本机标准位置组装完整测试场发行包（含二进制） |
| 本文命令块 | 手册第五节 | 实例/采集/写/trace/MQTT/AI 全流程可直接复制执行 |

## 七、坑位记录（排障必读）

1. **bat 编码**：cmd 对 UTF-8/LF 批处理会错位执行。本测试场 bat 一律 **ASCII+CRLF**
   + goto 直线流（无括号块）。
2. **TOML 路径**：基本字符串里 `\` 是转义符，`E:\x` 变 `E:x`——一律正斜杠。
3. **写地址全类型**：`holding:10` 缺 `:u16` 仅 WARN 且命令静默不执行（status 仍 ok）。
4. **v1.25.3 serve 提交准入**：会话 rotating 窗口 `/submit` 返 409 "not admitted"
   （真任务短重试可穿过；管理动词如 `/mcp connect` 执行成功也恒返 409——成败看
   SSE Notice）；SSE 无 replay，**先挂 /events 流再提交**才能捕到 Notice。
5. **写纪律门**：yolo 只解除审批门；写类工具另有 todo+验收义务硬门（源码
   runtimepolicy/guards.go），提示词见 5.7。
6. **模拟器控制台端口**：默认 8080 与 gateway 冲突，测试场配置已改 8090。
7. **换 exe 前先 stop 服务**：Windows 锁运行中的可执行文件，cp 报 "Device or resource busy"。
8. **Git Bash 控制台是 GBK——curl -d 直传中文必污染会话**：控制台把中文按 GBK 编码
   发出，serve 的 JSON 解码将非法 UTF-8 替换为 U+FFFD（混合 GBK 字节）落库，前端
   忠实渲染成乱码（实测现场事故）。中文 payload 一律用 **`curl --data-binary @文件`**
   （UTF-8 文件）或经 `scripts/test_serve_e2e.py`（python 自动 `\uXXXX` 转义）提交。
   污染会话处置：`POST /new` 后逐个 `POST /delete-session {"name":...}` 删除。
