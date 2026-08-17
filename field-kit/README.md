# field-kit — Windows 端到端测试场套件

> rs-iot "真实工厂级"端到端测试环境的**配置与脚本资产包**（不含二进制，见下方"二进制来源"）。
> 完整搭建手册：[docs/field-e2e-windows.md](../docs/field-e2e-windows.md)（com0com 安装、
> warden 配置管理、全流程测试命令、坑位记录）。

## 内容

| 文件 | 用途 |
|---|---|
| `start-rsiot.bat` | 一键冷启动（ASCII+CRLF）。幂等编排：生成 services.toml → warden → 模拟器 → rs-iot → 登录取 JWT → 生成 reasonix.toml → reasonix → 设 yolo → gateway。`%~dp0` 自定位，任意根目录可用 |
| `stop-rsiot.bat` | 一键全停（stop-all 优雅 + 杀 warden，Job Object 连带清进程树） |
| `config/services.toml.template` | warden 服务定义模板（`__ROOT__` 占位，启动时生成） |
| `config/reasonix.toml.template` | reasonix 项目配置模板（`__ROOT__`/`__JWT__` 占位，启动时生成） |
| `config/rsiot-config.toml` | rs-iot 主程序配置（REST 8790 / lux 6390 / MQTT 公共 broker） |
| `config/gateway.toml.example` | rsiot-gateway 配置模板（jwt_secret 与 rsiot-config.toml 同值；含可选 `[reasonix]` 供应商管理段。部署时复制为 `gateway.toml` 并替换 `__ROOT__`） |
| `config/simulator-config.yaml` | 设备模拟器配置（REST 8090 避让 gateway / RTU 从站 COM3） |
| `pack-field-kit.ps1` | 从本机标准位置组装**完整**测试场发行包（含全部二进制与 com0com 驱动包） |

## 部署新测试场（三步）

```bat
:: 1. 建目录骨架并放入本套件（任意根，以 E:\rsiot-field 为例）
mkdir E:\rsiot-field && cd /d E:\rsiot-field
xcopy <仓库>\field-kit . /E

:: 2. 摆二进制（见手册 §2.1 来源表）：
::    bin\rs-iot.exe / rsiot-gateway.exe / rsiot-mcp.exe / reasonix.exe
::    warden\warden.exe
::    simulator\dist\（simulator-dist.zip 解压 + 覆盖 config\simulator-config.yaml→config.yaml）
::    simulator\com0com\（虚拟串口驱动包）
::    home\（REASONIX_HOME：providers config.toml + .env 密钥——绝不入 git）
::    或直接运行 pack-field-kit.ps1 生成完整包解压到此

:: 3. com0com 建串口对（管理员，手册 §3）后一键启动
start-rsiot.bat
```

## 二进制来源（不随仓库入库）

| 二进制 | 来源 |
|---|---|
| rs-iot 三件套 | 本仓库 `cargo build -j 2 --release -p rs-iot -p rsiot-gateway -p rsiot-mcp` |
| warden.exe | warden 仓库 `cargo build --release` |
| reasonix.exe | Reasonix 官方发布 |
| simulator-dist.zip | simulator 仓库（github.com/viccom/simulator）发行包 |
| com0com 驱动包 | SourceForge com0com（或既有驱动目录拷贝） |

## 验收（启动后）

```
warden 控制台   http://127.0.0.1:8789     四服务 running
前端           http://127.0.0.1:8080     登录 admin/admin
gateway 健康   /api/gw/health → {"rsiot":"online","serve":"online",...}
```
