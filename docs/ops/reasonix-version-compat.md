# Reasonix 版本兼容性（rs-iot AI 集成）

> **状态**：活文档（随 reasonix 版本演进与实测结果持续维护）
> 本文档是「rs-iot AI Agent 集成 × reasonix 版本」兼容性的**唯一权威源**：版本对照、兼容面、断裂面与各版本部署差异。
> 部署操作见 [deploy-gateway-and-mcp.md](deploy-gateway-and-mcp.md)，现场测试场见 [field-e2e-windows.md](field-e2e-windows.md)。
>
> **证据等级**：本文区分「实测」（三件套端到端跑通）与「代码核对」（读当前 HEAD 源码/路由/commit 确认，未起服务）。
> v2.35.0 的结论是实测（§五）；v2.58.1 的结论是代码核对（§四），**未在 v2.58.1 下复跑端到端**。

## 一、版本对照

| 版本 | 地位 | 依据 |
|---|---|---|
| **v1.25.3** | rs-iot 文档记载的测试通过版本；field-kit 交付基线（官方发布单二进制） | `field-e2e-windows.md`（"reasonix v1.25.3 写纪律"）；deploy 文档与 field-kit 模板均按此版本行为编写 |
| **v2.35.0**（+14 commits，d08661885） | 2026-10-11 本机 git 源码构建**实测**版本 | `git describe: studio-v2.35.0-14-gd08661885`；端到端实测记录见 §五 |
| **v2.58.1**（+1 commit，9ee11c6b1） | 2026-11-09 本机仓库当前版本；**代码核对，未端到端实测** | `git describe: studio-v2.58.1-1-g9ee11c6b1`；核对记录见 §四 |

## 二、兼容面（v2.35.0 实测；v2.58.1 代码核对仍成立）

以下核心集成契约在 v2.35.0 下实测成立（2026-10-11 三件套端到端）；v2.58.1 下按当前 HEAD 源码逐条核对，除 §四列出的变化外未见移除：

- **层3 鉴权**：serve 只认 `Cookie: reasonix_token=<token>`（v2.58.1 `internal/frontend/serve/auth.go:43` `cookieToken = "reasonix_token"` 未变）；gateway 注入 cookie 的方式继续有效
- **SSE**：`GET /events` 仍为裸 NDJSON（无 `event:` 头，kind 在 JSON body 内，`: ping` 心跳）——gateway 裸字节流透传（不用 axum Sse）的前提不变。v2.58.1 仅核对了 `/events` 路由仍注册，**未逐帧复验帧格式**
- **端点面**：`/submit` `/approve` `/cancel` `/answer` `/new` `/resume` `/delete-session` `/plan` `/goal` `/tool-approval-mode` `/status` `/sessions` `/history` `/models` `/events` 共 15 个，v2.58.1 `internal/frontend/serve/*.go` 路由注册逐条核对 **15/15 仍存在**
- **MCP stdio spawn**：`[[plugins]]` 声明 + spawn stdio MCP 子进程（rsiot-mcp 25 工具）工作正常；agent 经 `use_capability` 发现并调用 `mcp-tool:rsiot/<tool>` 闭环实测通过。v2.58.1 `mcp-tool:<name>/<tool>` capability ID 前缀在测试代码里仍在用（v2.35.0 与 HEAD 均有），**MCP 工具 ID 前缀未变**
- **gateway 注入**：`forward_agent_submit` 的工具提示注入用户消息头部，serve 正常接收（v2.58.1 未复跑）

## 三、断裂面（v1.25.3 → v2.35.0 行为变更，部署会踩）

| # | 变更 | v1.25.3 行为（文档/field-kit 契约） | v2.35.0 实测行为 | 对策 |
|---|---|---|---|---|
| 1 | **项目级 `[serve]` token 不生效** | reasonix.toml（项目级）写 `token = "..."`，与 gateway.toml `serve_token` 同值即通（start-rsiot.bat 健康检查 `?token=` 即按此契约） | 项目级 `[serve]` token **被忽略**，serve 生成随机 launch token（`~/.reasonix/remote/launch-<pid>.token`），配置 token 401 | serve 启动显式传 `-token <值>` 或 `-token-file <路径>`；gateway `serve_token` 必须与实际生效 token 同值。**改用 -token 传参后契约恢复**（token 回到部署者掌控） |
| 2 | **MCP 服务器审批门** | 项目 `[[plugins]]` 声明即可用 | 项目声明的 MCP server 处于 `project_mcp_awaiting_approval`（SSE notice 可见），工具 **off**，须先执行 `reasonix mcp enable <name>`；`tool-approval-mode=yolo` 只覆盖运行期工具审批，**不覆盖此门** | 部署/启动流程加一步 `reasonix mcp enable rsiot`（幂等；批准状态持久于 `~/.reasonix/projects/<slug>/`，之后重启免重复） |
| 3 | **`serve --config <file>` 参数移除** | deploy 文档 OS 服务化写 `reasonix serve --config /opt/rs-iot/reasonix.toml` | v2.35.0 serve 无 `--config` flag（参数：`-addr -auth -token -token-file -model -preset -pid-file -port-file ...`），配置靠 REASONIX_HOME / 项目目录发现 | OS 服务 unit 改 `WorkingDirectory=<运行目录>`（项目 reasonix.toml 所在）+ 去掉 `--config`；用户级配置放 `~/.reasonix/config.toml` |
| 4 | **`[serve].addr` 不是配置键**（v1 时代文档即与模板坑位注记矛盾） | deploy 文档 §三写 `addr = "127.0.0.1:8787"`；field-kit 模板坑位注记已纠正（"use -addr flag"） | 监听地址只认 `-addr` flag（缺省 `127.0.0.1:8787`） | 统一用 `-addr` flag；deploy 文档示例已同步修正 |

> **凭据来源变更**（环境侧，非集成契约）：v2.35.0 provider `api_key_env` 从 `~/.reasonix/.env` 解析（进程环境变量同名值不覆盖文件？——实测以 `.env` 文件为准）。现场升级时检查该文件中各 `*_API_KEY` 的新鲜度（2026-10-11 实测：过期 `LOCAL_API_KEY` 致 LLM 401"认证失败"，更新后恢复）。v2.58.1 下此条未复验。

## 四、v2.58.1 变更核对（2026-11-09，代码级，未端到端实测）

对 `v2.35.0..HEAD`（4350 提交）中与 rs-iot 集成相关的部分，按当前 HEAD 源码、路由注册与 `release-notes/studio/2.58.{0,1}.md` 核对：

### 4.1 仍成立（代码证据）

- 鉴权 cookie、`-token` / `-token-file` / `-addr` flag（无 `--config`）、MCP 审批门（`project_mcp_awaiting_approval`、`mcp enable` 提示）、15 个端点、`mcp-tool:` capability ID 前缀，均在 HEAD 源码中存在（见 §二）
- §三 四条断裂面的对策在 v2.58.1 下**仍是正确做法**，无需回滚

### 4.2 变化（相对 v2.35.0 基线，影响部署/配置，rs-iot 需知）

> 下表是 `v2.35.0..v2.58.1` 跨度内与集成相关的变化，**不等于都是 v2.58.1 才引入**；每项标了实际引入版本。

| # | 变化 | 代码/文档证据 | 对 rs-iot 的影响 |
|---|---|---|---|
| 1 | **serve 新增 `-dir` flag**（相对 v2.35.0 是新增；**并非 v2.58.1 才加**，v2.43.0 起已有） | `internal/frontend/cli/serve_frontend.go:277` `dir := fs.String("dir", "", "change to this directory first (project root)...")`；引入 commit `063ad8131`（2026-10-16），`git tag --contains` 首个为 `studio-v2.43.0`；v2.35.0 同文件无此 flag | §三 #3 的 `WorkingDirectory=` 方案仍有效，但**多了一个显式替代**：`reasonix serve -dir <项目根> ...` 可直接指定项目根，无需依赖进程 cwd。OS 服务 unit 可改用 `-dir`，语义更明确 |
| 2 | **v2.58.0 移除一批能力/配置键** | `release-notes/studio/2.58.0.md`「移除」段：`task`、`fleet`、`parallel_tasks`、后台子任务、转向、`continue_from`、隔离工作树、多候选择优（`ad2c96347`）；`--fail-on-unverified`、`agent.host_verification`（`8c785fa4b`）；`task_cost_budget` / `task_time_budget_minutes` / `task_token_budget`（`a341d5615`）。**旧配置带这些键仍可加载，启动时自动清理并提示一次** | rs-iot 的 field-kit 模板与 gateway 配置**未使用**这些键，无直接改动需求；但存量现场若手工加过，升级后会被自动清理，属正常行为 |
| 3 | **`use_capability` 描述不再点名 `task:fleet`** | `release-notes/studio/2.58.0.md:41`「工具清单前缀因此变化一次」（`ad2c96347`） | 这是 `use_capability` 的**工具清单描述/前缀**变化，**不是** `mcp-tool:<name>/<tool>` capability ID 前缀变化（两版代码均用后者）。rs-iot 不硬编码该描述，无直接影响；但 §五 记录的 `turn_started → use_capability` 流程在 v2.58.1 下未复跑 |
| 4 | **`reasonix mcp` 子命令 `--help` 不再误执行** | `release-notes/studio/2.58.1.md:10`（#15022）：此前 `mcp import --help` 会把 `--help` 当参数真的执行并改写 MCP 配置；现在只打印用法、不做改动 | 部署脚本里若有 `reasonix mcp ... --help` 探测逻辑，v2.58.1 起行为变安全；rs-iot 部署流程用 `mcp enable rsiot`（无 `--help`），不受影响 |

### 4.3 本轮**未**覆盖（明确声明，勿当作已验证）

- 未在 v2.58.1 下起三件套跑端到端；§五 的 `turn_started → use_capability → spawn rsiot-mcp → turn_done` 闭环**未在 v2.58.1 复验**
- `/events` SSE 帧格式（裸 NDJSON / `: ping` 心跳）未逐帧复验，仅确认路由注册
- 项目 `reasonix.toml` 靠 cwd / `-dir` 发现的细节、`[sandbox] network` 项目级被忽略的规则、`api_key_env` 的 `.env` 解析，v2.58.1 下均未复验
- v2.36.0 ~ v2.57.1 的逐版本 release notes 未逐条审，只扫了含 breaking/移除 关键词的条目（主要断裂集中在 2.58.0）

## 五、v2.35.0 实测记录（2026-10-11，本机 Linux，历史）

三件套 `rs-iot(:8790) + rsiot-gateway(:18080) + reasonix serve(:8787)` 端到端：

- `/api/gw/health` 聚合 `rsiot:online / serve:online`；gateway 登录、rust-embed 静态、`/api/rsiot/*` 透传、`/api/agent/status`（cookie 注入）、`/api/agent/events` SSE 全通
- 真实提交任务：`turn_started`（含 gateway 工具提示注入）→ `reasoning` → `use_capability` 发现 `mcp-tool:rsiot/rsiot_list_plugins` → serve spawn rsiot-mcp（进程实测可见）→ 工具返回真实插件清单（7 source + 1 sink）→ 中文回答 → `turn_done` 无错误
- 断裂面 #1/#2 均为实测踩中后按对策修复；测试基线：gateway+mcp 96 单测、前端 203 单测全绿

## 六、版本选择建议

- **存量现场（v1.25.3 交付）**：field-kit 现有脚本/模板原样可用，不必升级；升级前按 §三逐项核对
- **新部署 / 升级到 v2.58.1**：按 [deploy-gateway-and-mcp.md](deploy-gateway-and-mcp.md) §3.2 的差异清单走；field-kit 模板继续用 `reasonix-v2.35.toml.template`（其记录的三条对策在 v2.58.1 仍成立，模板文件名是历史命名、未按版本改名）；OS 服务化可选 `WorkingDirectory=` 或新增的 `-dir` flag
- 升级到 v2.58.x 后**建议在现场复跑一次 §五 的端到端闭环**，把 §4.3 的未覆盖项补成实测记录
- 混合环境严禁假设 `[serve]` token 双版本同语义——gateway `serve_token` 永远以"serve 实际接受的 token"为准（部署后用 health 验证）
