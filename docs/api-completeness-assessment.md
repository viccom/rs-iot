# REST API 完备性评估（前端系统支撑）

> 评估当前 REST API（[`rest-api.md`](rest-api.md)）能否为**管理员 + 普通用户**使用的**前端系统**提供完善接口。
> **本文为评估报告 + 建议，不含实现**。事实依据来自代码核实（`src/api/`）。

## 1. 定位

当前 API 的设计定位是**单管理员调试 / 运维**（对标 Lite_IOT）：一个管理员带 JWT 或全局 AccessKey 操作全部资源。端点覆盖系统信息、实例/设备/用户/分组 CRUD、实例启停、Lua 运维、写命令、实时数据——**作为"网关运维控制台"已基本够用**。

要演进为"**管理员 + 多用户前端系统**"（多角色、多租户、用户自服务、大数据量分页、写命令反馈、历史查询），存在系统性缺口。

## 2. 结论：能否支撑前端系统？

| 前端系统能力 | 当前是否具备 | 说明 |
|---|---|---|
| 管理员单用户运维控制台 | ✅ 基本具备 | CRUD + 启停 + 写命令 + 实时数据齐全 |
| 多用户**授权**（角色/ACL） | ❌ **不具备** | 只有认证，无授权层；任何用户全权限 |
| 用户**自服务**（注册/改密/找回） | ❌ **不具备** | 无 register；user/set 不写密码；新用户无法经 API 建可用账号 |
| 大数据量**分页/过滤** | ❌ **不具备** | 所有 list 端点全量返回；实例/设备上百后会爆 |
| 写命令**执行反馈** | ❌ **不具备** | command/send 只确认投递，无回执/结果/超时 |
| 实时数据**历史/趋势** | ❌ **不具备** | rtdatas 只读最新快照，无时间范围/聚合 |
| 健康探针 / 指标 | ❌ **不具备** | 无 /healthz；无 /metrics（见 [场景缺口评估](scenario-gap-assessment.md)） |

**总结论**：当前 API **不足以直接支撑"管理员 + 多用户前端系统"**。它是一个合格的"网关运维 API"，但缺少从"工具"到"产品"的几项关键能力——授权层、用户自服务、分页、命令回执是前置硬缺口。

## 3. 缺口清单（按前端系统关切分组）

### A. 认证与授权（最关键）
| # | 缺口 | 现状证据 | 影响 |
|---|---|---|---|
| A1 | **无授权层（仅认证）** | 中间件解析出 userId 但丢弃（`auth.rs`），未注入上下文；`groups`/`usergroups` 数据可读写但**无端点消费**做权限判定 | 任何持 token/accesskey 者可改任何人密码、删实例、设 accesskey。多用户前端下普通用户=管理员 |
| A2 | 无 token 吊销/黑名单 | logout 仅清 cookie；JWT 有效到 24h 过期；user/delete、accesskey/set 不让已签发 token 夽效 | 用户离职/泄露无法即时止损 |
| A3 | AccessKey 单全局值 | 仅一个全局 accesskey，无分用户/作用域/实例 | 无法给不同集成方发不同凭据；轮换全局即时生效，无灰度 |

### B. 用户管理（自服务闭环）
| # | 缺口 | 现状证据 | 影响 |
|---|---|---|---|
| B1 | **无 register 端点** | `PUBLIC_PATHS` 含 `/api/v1/user/register` 但 `build_router` 未注册、无 handler（`mod.rs:37,108`） | 无法自助注册/初始化账号（白名单悬空） |
| B2 | **user/set 不写密码** | `routes_user.rs:35-39` 只 HSET `users`，不写 `passwords` | 新建用户无密码无法 login |
| B3 | changepass 要旧密码 | `routes_user.rs:85-92` | 新用户（无旧密码）无法设密码 |
| → | **bootstrap 死锁** | B1+B2+B3 组合 | 首个账号靠种子（admin/admin）；后续账号无干净路径创建可用账号 |

### C. CRUD 与运维完备性
| # | 缺口 | 说明 |
|---|---|---|
| C1 | **无分页/过滤** | `/insts`/`/devices`/`/users`/`/usergroups`/`/rtdatas` 全量返回；上百实例后响应体膨胀、前端卡顿 |
| C2 | 无批量操作 | 单条 set/delete/启停/命令；几百实例运维需调几百次 |
| C3 | 分组能力弱 | 无"整组删除"、无"单组查询"、无"列用户所属组" |
| C4 | AccessKey 不可查/删 | 只有 set；无法经 API 确认当前值或显式清空 |
| C5 | startinst 仅南向 | 北向 Sink 无 REST 启动入口（只能 auto_start 配置） |
| C6 | reload 仅 luaClient | 其它插件实例无"重启"端点（需 stop+start，且 start 仅南向） |

### D. 数据查询与命令反馈
| # | 缺口 | 说明 |
|---|---|---|
| D1 | **写命令无回执** | command/send 只确认投递到 cmd_rx（`routes_command.rs:38`），无 commandId、无结果查询、无超时——前端无法告知用户"写入成功/失败" |
| D2 | **实时数据无历史/聚合** | rtdatas 只读最新快照；无时间范围、无分页、无按 field 过滤、无聚合（趋势曲线做不了） |
| D3 | 实时数据无推送 | 仅轮询；无 WebSocket/SSE 实时推送 |

### E. 运维与契约
| # | 缺口 | 说明 |
|---|---|---|
| E1 | 无 `/healthz`/`/readyz` | sysstatus 带鉴权+业务数据，不适合 k8s/LB 探活 |
| E2 | 无 `/metrics`/审计日志查询 | 见 [场景缺口评估](scenario-gap-assessment.md) §4 |
| E3 | DTO 校验不一致 | inst/set 强校验（好）；device/user/accesskey set 裸 JSON 直写无校验，易入脏数据 |
| E4 | 错误响应格式不统一 | 有的 `(status, "字符串")`，有的 `{"status":"error","msg"}`，有的 `{"status":"ok"}`；无统一 envelope |
| E5 | Authorization 不剥离 Bearer | 客户端须传裸 token，与通用 `Bearer xxx` 约定相悖 |
| E6 | `GET /` 在鉴权后 | 未带凭据访问根路径得 401 而非重定向，前端首屏体验差 |

## 4. 建议的演进路径

### Phase 1 —— 可用的多用户前端（P0，前置硬缺口）
- **A1 授权层**：中间件注入 userId → 实现角色（admin/operator/viewer）+ 基于 usergroups 的资源可见性。这是多用户系统的基石
- **B1+B2 用户自服务**：实现 register（或 user/set 接受可选 pass），打通账号创建闭环
- **C1 分页**：list 端点加 `?page=&size=` + 总数；响应 envelope 化
- **D1 命令回执**：command/send 返回 commandId + 新增 `GET /api/v1/command/result?id=`（或同步等待插件 handle_write 结果，带超时）
- **E4 统一错误 envelope**：`{code, msg, data}` 全端点统一

### Phase 2 —— 好用的前端（P1）
- **D2 历史查询**：lux 时序存储（Stream/列表）+ `GET /api/v1/rtdatas/history?dev_id=&from=&to=&interval=`（对接场景评估的时序 downsample）
- **C2 批量操作**：批量 set/delete/启停/命令
- **E1 healthz** + **E2 /metrics**（见场景评估）
- **A2 token 吊销**：登出/删用户/改 accesskey 即时失效（ lux 黑名单或短 TTL + refresh）
- **D3 实时推送**：WebSocket/SSE 订阅设备值变化（替代前端轮询）

### Phase 3 —— 企业级（P2）
- **A3 AccessKey 分作用域**：按用户/实例/读写分发，支持灰度轮换
- **E3 DTO 校验统一**：所有 set 端点 schema 校验
- 审计日志（谁在何时改了什么）、多租户、E5/E6 契约修正

## 5. 与场景评估的关系

本文件聚焦"前端系统能力"（认证授权 / CRUD / 分页 / 命令回执 / 历史查询）。运维可观测性（埋点 /metrics、报文 trace、日志 ring buffer、自愈）见 [`scenario-gap-assessment.md`](scenario-gap-assessment.md)。两者交集（/metrics、/healthz、inst/stats）在各自文中互为引用，不重复展开。

> 以上为评估建议，是否实现、实现哪些，由项目决策。
