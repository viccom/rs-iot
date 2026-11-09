# Reasonix 版本兼容性（rs-iot AI 集成）

> **状态**：活文档（随 reasonix 版本演进与实测结果持续维护）
> 本文档是「rs-iot AI Agent 集成 × reasonix 版本」兼容性的**唯一权威源**：版本对照、兼容面、断裂面与各版本部署差异。
> 部署操作见 [deploy-gateway-and-mcp.md](deploy-gateway-and-mcp.md)，现场测试场见 [field-e2e-windows.md](field-e2e-windows.md)。

## 一、版本对照

| 版本 | 地位 | 依据 |
|---|---|---|
| **v1.25.3** | rs-iot 文档记载的测试通过版本；field-kit 交付基线（官方发布单二进制） | `field-e2e-windows.md`（"reasonix v1.25.3 写纪律"）；deploy 文档与 field-kit 模板均按此版本行为编写 |
| **v2.35.0**（+14 commits，d08661885） | 2026-10-11 本机 git 源码构建实测版本 | `git describe: studio-v2.35.0-14-gd08661885`；端到端实测记录见本文 §四 |

## 二、兼容面（v2.35.0 实测通过，协议契约未变）

以下核心集成契约在 v2.35.0 下实测成立（2026-10-11 三件套端到端）：

- **层3 鉴权**：serve 只认 `Cookie: reasonix_token=<token>`（`auth.go` cookieToken 常量未变）；gateway 注入 cookie 的方式继续有效
- **SSE**：`GET /events` 仍为裸 NDJSON（无 `event:` 头，kind 在 JSON body 内，`: ping` 心跳）——gateway 裸字节流透传（不用 axum Sse）的前提不变
- **端点面**：前端/网关调用的 `/submit` `/approve` `/cancel` `/answer` `/new` `/resume` `/delete-session` `/plan` `/goal` `/tool-approval-mode` `/status` `/sessions` `/history` `/models` `/events` 在 `routes.go` 全部存在且语义一致
- **MCP stdio spawn**：`[[plugins]]` 声明 + spawn stdio MCP 子进程（rsiot-mcp 25 工具）工作正常；agent 经 `use_capability` 发现并调用 `mcp-tool:rsiot/<tool>` 闭环实测通过
- **gateway 注入**：`forward_agent_submit` 的工具提示注入用户消息头部，serve 正常接收

## 三、断裂面（v1.25.3 → v2.35.0 行为变更，部署会踩）

| # | 变更 | v1.25.3 行为（文档/field-kit 契约） | v2.35.0 实测行为 | 对策 |
|---|---|---|---|---|
| 1 | **项目级 `[serve]` token 不生效** | reasonix.toml（项目级）写 `token = "..."`，与 gateway.toml `serve_token` 同值即通（start-rsiot.bat 健康检查 `?token=` 即按此契约） | 项目级 `[serve]` token **被忽略**，serve 生成随机 launch token（`~/.reasonix/remote/launch-<pid>.token`），配置 token 401 | serve 启动显式传 `-token <值>` 或 `-token-file <路径>`；gateway `serve_token` 必须与实际生效 token 同值。**改用 -token 传参后契约恢复**（token 回到部署者掌控） |
| 2 | **MCP 服务器审批门** | 项目 `[[plugins]]` 声明即可用 | 项目声明的 MCP server 处于 `project_mcp_awaiting_approval`（SSE notice 可见），工具 **off**，须先执行 `reasonix mcp enable <name>`；`tool-approval-mode=yolo` 只覆盖运行期工具审批，**不覆盖此门** | 部署/启动流程加一步 `reasonix mcp enable rsiot`（幂等；批准状态持久于 `~/.reasonix/projects/<slug>/`，之后重启免重复） |
| 3 | **`serve --config <file>` 参数移除** | deploy 文档 OS 服务化写 `reasonix serve --config /opt/rs-iot/reasonix.toml` | v2.35.0 serve 无 `--config` flag（参数：`-addr -auth -token -token-file -model -preset -pid-file -port-file ...`），配置靠 REASONIX_HOME / 项目目录发现 | OS 服务 unit 改 `WorkingDirectory=<运行目录>`（项目 reasonix.toml 所在）+ 去掉 `--config`；用户级配置放 `~/.reasonix/config.toml` |
| 4 | **`[serve].addr` 不是配置键**（v1 时代文档即与模板坑位注记矛盾） | deploy 文档 §三写 `addr = "127.0.0.1:8787"`；field-kit 模板坑位注记已纠正（"use -addr flag"） | 监听地址只认 `-addr` flag（缺省 `127.0.0.1:8787`） | 统一用 `-addr` flag；deploy 文档示例已同步修正 |

> **凭据来源变更**（环境侧，非集成契约）：v2.35.0 provider `api_key_env` 从 `~/.reasonix/.env` 解析（进程环境变量同名值不覆盖文件？——实测以 `.env` 文件为准）。现场升级时检查该文件中各 `*_API_KEY` 的新鲜度（2026-10-11 实测：过期 `LOCAL_API_KEY` 致 LLM 401"认证失败"，更新后恢复）。

## 四、v2.35.0 实测记录（2026-10-11，本机 Linux）

三件套 `rs-iot(:8790) + rsiot-gateway(:18080) + reasonix serve(:8787)` 端到端：

- `/api/gw/health` 聚合 `rsiot:online / serve:online`；gateway 登录、rust-embed 静态、`/api/rsiot/*` 透传、`/api/agent/status`（cookie 注入）、`/api/agent/events` SSE 全通
- 真实提交任务：`turn_started`（含 gateway 工具提示注入）→ `reasoning` → `use_capability` 发现 `mcp-tool:rsiot/rsiot_list_plugins` → serve spawn rsiot-mcp（进程实测可见）→ 工具返回真实插件清单（7 source + 1 sink）→ 中文回答 → `turn_done` 无错误
- 断裂面 #1/#2 均为实测踩中后按对策修复；测试基线：gateway+mcp 96 单测、前端 203 单测全绿

## 五、版本选择建议

- **存量现场（v1.25.3 交付）**：field-kit 现有脚本/模板原样可用，不必升级；升级前按本文 §三逐项核对
- **新部署（v2.35.0）**：按 [deploy-gateway-and-mcp.md](deploy-gateway-and-mcp.md) §3.2 的差异清单走；field-kit 模板用 `reasonix-v2.35.toml.template`
- 混合环境严禁假设 `[serve]` token 双版本同语义——gateway `serve_token` 永远以"serve 实际接受的 token"为准（部署后用 §四 health 验证）
