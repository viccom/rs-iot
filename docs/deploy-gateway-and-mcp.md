# 部署指南：rsiot-gateway + rsiot-mcp + reasonix

> v3 AI Agent 集成的现场部署。三件套：rsiot-mcp（MCP server 子进程）+ rsiot-gateway（API Gateway）+ reasonix serve（AI agent）。
> 前置：rs-iot 主程序已部署运行（:8790）。设计见 `plan-ai-agent-integration-v3.md` + `plan-gateway-tech-selection.md`。

## 一、二进制构建

```bash
# 在 rs-iot 仓库根（workspace 根）
cargo build -j 2 --release -p rsiot-mcp -p rsiot-gateway
# 产物：target/release/rsiot-mcp + target/release/rsiot-gateway
```

拷贝到现场运行目录（与 rs-iot 二进制同目录）：
- `rsiot-mcp`（reasonix 经 `[[plugins]]` spawn）
- `rsiot-gateway`（OS 服务常驻）
- `rs-iot`（主程序）

## 二、rsiot-gateway 配置 + OS 服务化

### gateway.toml（放运行目录，与 rs-iot config.toml 同级）

```toml
listen      = "0.0.0.0:8080"
rsiot_base  = "http://127.0.0.1:8790"   # rs-iot 主程序
serve_base  = "http://127.0.0.1:8787"   # reasonix serve（下一节起）
jwt_secret  = "<与 rs-iot config.toml [api] jwt_secret 同值>"
serve_token = "<reasonix serve --auth token，见下节>"
```

> **jwt_secret 必须与 rs-iot 同值**（层1 复用 JWT，gateway 颁的 token 直通 rs-iot，见选型报告 §四层1）。

### Windows Service

```powershell
sc create rsiot-gateway binPath= "C:\path\to\rsiot-gateway.exe C:\path\to\gateway.toml" start= auto
sc description rsiot-gateway "rs-iot AI Agent API Gateway"
sc start rsiot-gateway
```

### Linux systemd（/etc/systemd/system/rsiot-gateway.service）

```ini
[Unit]
Description=rs-iot AI Agent API Gateway
After=network.target rs-iot.service

[Service]
ExecStart=/opt/rs-iot/rsiot-gateway /opt/rs-iot/gateway.toml
Restart=on-failure
User=rsiot

[Install]
WantedBy=multi-user.target
```

```bash
systemctl daemon-reload && systemctl enable --now rsiot-gateway
```

## 三、reasonix serve 配置 + OS 服务化

### reasonix.toml（放运行目录）

```toml
default_model = "deepseek"          # 或本地 Ollama provider（数据不出厂）

[serve]
addr       = "127.0.0.1:8787"       # gateway 反代目标（同机 loopback）
auth_mode  = "token"
token      = "<生成的高熵 token，与 gateway.toml serve_token 同值>"

[[providers]]
name     = "deepseek"
kind     = "openai"
base_url = "https://api.deepseek.com"
models   = ["deepseek-v4-flash"]
api_key_env = "DEEPSEEK_API_KEY"

# rsiot-mcp：让 agent 调网关工具（read_tags/send_command/save_script...）
[[plugins]]
name    = "rsiot"
command = "<运行目录>/rsiot-mcp"
args    = []
env     = { RSIOT_BASE = "http://127.0.0.1:8790", RSIOT_JWT = "<预签发 JWT>" }

[sandbox]
workspace_root = "<运行目录>"
allow_write    = ["<运行目录>/scripts"]   # 仅脚本可写
forbid_read    = ["<运行目录>/data"]      # 隔离 lux 数据
bash           = "off"                    # Windows 无 OS 级 bash 沙箱
network        = true                     # 允许连模型 API
```

> **RSIOT_JWT**：rs-iot 主进程签发一次（登录后取 token），写入此处。rsiot-mcp 用它调 rs-iot REST（层2 Authorization）。

### reasonix serve OS 服务化

Windows Service（同 gateway 模式，`sc create reasonix ...`）或 systemd unit（`ExecStart=/opt/rs-iot/reasonix serve --config /opt/rs-iot/reasonix.toml`）。

## 四、验证（部署后）

```bash
# 1. gateway health（免 JWT，PUBLIC_PATHS）
curl http://localhost:8080/api/gw/health
# {"rsiot":"online","serve":"online","mcp":"unknown"}

# 2. 前端经 gateway 访 rs-iot（带 JWT）
curl -H "Authorization: Bearer <jwt>" http://localhost:8080/api/rsiot/api/v1/sysstatus

# 3. agent 经 MCP 调网关（在 reasonix 对话里）
#    "列出所有设备" → reasonix 调 rsiot_list_devices → rsiot-mcp → rs-iot /devices
```

## 五、安全边界（对标 plan-ai-agent-integration-v3.md §六）

| 风险 | 对策 |
|---|---|
| gateway 是新单点 | OS 服务化 + `Restart=on-failure`；逻辑 < 500 行易可靠 |
| serve token 泄露 | serve bind loopback（同机）+ `--auth token`；gateway 反代注入，前端不感知 |
| jwt_secret 不同步 | gateway.toml 与 rs-iot config.toml 共享同值（部署时 cp/符号链接）|
| AI 写脚本越权 | rsiot-mcp `save_lua_script` → rs-iot `/lua/script/save` 路径白名单（`[A-Za-z0-9_-]`）|
| AI 读凭据 | `get_config` 返脱敏配置（jwt_secret/mqtt 凭据遮蔽）；reasonix `[sandbox] forbid_read` 隔离 data |
