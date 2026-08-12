# rs-iot 前端设计：工业监控界面 + AI 浮动面板

> 日期：2026-08-12
> 分支：`feature/ai-agent-integration`
> 前置：v3 三件套（rs-iot + reasonix serve + rsiot-gateway）已联调跑通（commit `d702ae0`），gateway 反代真实 serve 的 SSE/cookie/JWT 全链路验证通过。

## 一、目标与范围

### 核心诉求

用户经 AI 面板发布指令后，必须在**同一界面**直观看到执行结果（采集数据/日志/报文），用于审核验证。否则 AI 面板与直接用 reasonix CLI 无区别。

**两个核心子系统**（相互依存）：
1. **工业监控界面**：替代现有 scada.html 的**展示**功能（采集器状态、实时数据、日志流、报文 trace、系统状态）。
2. **AI 浮动面板**：全局浮层（agent 对话流、工具卡片、权限审批）。

### 明确不做（后置）

- 插件实例启停/设备配置 CRUD（管理功能后置，写操作只走 AI 面板调 MCP 工具）
- 路由/多页面（暂时单页 `/scada` 主页）
- E2E 自动化测试、组件快照测

### 成功标准

- M2 后：监控界面实时显示 rs-iot 数据，信息量对标 scada.html
- M3 后：AI 闭环——发指令→流式推理→工具调用→权限审批→结果展示，全部前端可见
- M4 后：错误边界全处理，长时间运行稳定

## 二、技术栈与架构

### 技术选型（brainstorming 确认）

| 维度 | 选择 | 理由 |
|---|---|---|
| 框架 | Vue3 + Vite + TypeScript | 抽取 zacp Vue3 组件零改造；调研阶段结论；AI 重构快路径 |
| 状态 | Pinia | Vue3 官方推荐，store 分离 monitor/agent |
| 路由 | vue-router | 单页为主，预留扩展 |
| 视觉 | zacp 浅色风（sky-blue + slate） | zacp 组件零改造；放弃 scada 琥珀工业风（两者冲突，二选一） |
| 数据 | SSE 推送为主 + 轮询 | 复用 rs-iot 已验证 SSE；与 agent SSE 统一双 EventSource 架构 |

### 整体拓扑

```
浏览器 :8080(gateway)
├─ 静态资源(index.html + JS/CSS) ◄── gateway rust-embed 嵌入 frontend/dist
├─ /api/rsiot/* ──反代──► rs-iot :8790
│   ├─ GET /api/rsiot/events (SSE: log/packet/state)  ◄── 监控界面主数据源
│   ├─ GET /api/rsiot/rtdatas (实时值轮询 2s)
│   └─ GET /api/rsiot/inststatus + /sysstatus(轮询 5s) + /logs/recent + /inst/trace
├─ /api/agent/* ──反代──► reasonix serve :8787
│   ├─ GET /api/agent/events (SSE: agent 流,无 event: 头)  ◄── AI 面板主数据源
│   └─ POST /api/agent/submit + /approve + /cancel
└─ /api/gw/* (gateway 自有:login + health)
```

### 开发/部署模式

- **dev**：vite dev server :5173（HMR），proxy `/api` → gateway :8080
- **prod**：`npm run build` → `dist/` → gateway rust-embed 嵌入（`#[folder] = "../../frontend/dist"`）

### 目录结构

```
frontend/
├─ package.json          # vue3 + vite + typescript + pinia + vue-router
├─ vite.config.ts        # proxy /api → :8080, build outDir=dist
├─ src/
│   ├─ main.ts           # 应用入口(pinia + router + 全局样式)
│   ├─ App.vue           # 根布局(监控界面 + AI 抽屉)
│   ├─ router.ts         # 路由(单页,/scada 主页 + /login)
│   ├─ stores/
│   │   ├─ auth.ts       # JWT + 登录态(localStorage 持久)
│   │   ├─ monitor.ts    # 监控数据(rs-iot SSE + rtdata/logs/状态轮询)
│   │   └─ agent.ts      # agent 对话流(serve SSE + submit/approve)
│   ├─ views/
│   │   ├─ Login.vue     # 登录页
│   │   └─ Monitor.vue   # 监控主界面
│   ├─ components/
│   │   ├─ monitor/      # 监控组件(从 scada.html 逻辑移植)
│   │   │   ├─ CollectorList.vue   # 采集器(实例)列表+状态
│   │   │   ├─ RtdataPanel.vue     # 实时数据面板
│   │   │   ├─ LogStream.vue       # 日志流(SSE log 事件)
│   │   │   ├─ PacketTrace.vue     # 报文 trace(SSE packet 事件)
│   │   │   └─ SystemStatus.vue    # 系统状态
│   │   └─ chat/         # AI 面板组件(抽自 zacp,浅色风零改造)
│   │       ├─ AgentDrawer.vue     # 右侧抽屉容器(新写)
│   │       ├─ MessageItem.vue     # 抽自 zacp(405行)
│   │       ├─ ToolCallCard.vue    # 抽自 zacp(150行)
│   │       ├─ PlanCard.vue        # 抽自 zacp(69行)
│   │       ├─ PermissionModal.vue # 抽自 zacp(61行)
│   │       └─ composables/
│   │           ├─ useChatScroll.ts    # 抽自 zacp(63行)
│   │           └─ useMessageBlocks.ts # 抽自 zacp(88行)
│   ├─ api/
│   │   └─ client.ts     # fetch 封装(自动带 JWT + 401 重定向登录)
│   └─ styles/
│       └─ main.css      # zacp 主题 token 浅色风
└─ dist/                 # build 产物(gateway rust-embed #[folder] 指此)
```

## 三、组件设计与数据流

### 监控界面（Monitor.vue）

grid 布局：左 240px 采集器栏 + 右侧 tab 区：

```
Monitor.vue
├─ CollectorList.vue        # 左栏:实例列表(mb-tcp-2002/mb-rtu-com3),状态色(running/failed)
│   └─ 点击选中 → 切换右侧 tab 聚焦该实例
├─ tab 切换栏:[数据] [日志] [报文] [系统]
├─ RtdataPanel.vue          # tab1: 实时数据表格(devId.tag = value [ts])
├─ LogStream.vue            # tab2: 日志流(SSE log,level/target 过滤,自动滚动)
├─ PacketTrace.vue          # tab3: 报文 trace(SSE packet,hex 展示 TX/RX)
└─ SystemStatus.vue         # tab4: 系统状态(sysstatus: dbsize/cpu/mem/pipeline 计数)
```

### AI 面板（AgentDrawer.vue）

App.vue 根布局挂右侧抽屉按钮（380px 宽，可收起）：

```
AgentDrawer.vue
├─ 头部:agent 状态(在线/思考中) + 收起按钮 + 权限模式切换(Ask/Auto)
├─ 消息流(useChatScroll 自动滚底)
│   └─ MessageItem.vue × N    # 抽自 zacp:user/assistant/tool 消息块
│       ├─ 文本(流式 token 追加)
│       ├─ ReasoningBlock     # 思考流折叠面板
│       ├─ ToolCallCard.vue   # 工具调用卡片(工具名+参数+结果)
│       └─ PlanCard.vue       # 计划卡片
├─ PermissionModal.vue        # 权限审批弹窗(写操作触发,批准/拒绝)
└─ 输入栏:文本框 + 发送(POST /api/agent/submit)
```

### 数据流

```
── 监控数据流(monitor store)─────────────────────
gateway /api/rsiot/events (SSE,有 event: 头) ──► monitor store
  ├─ event: log    → LogStream 追加(按 level/target 过滤)
  ├─ event: packet → PacketTrace 追加(按选中实例过滤)
  └─ event: state  → CollectorList 更新实例状态
gateway /api/rsiot/rtdatas (轮询 2s) ──► RtdataPanel
gateway /api/rsiot/sysstatus (轮询 5s) ──► SystemStatus

── agent 对话流(agent store)─────────────────────
gateway /api/agent/events (SSE,无 event: 头裸 NDJSON) ──► agent store
  └─ onmessage: JSON.parse → 按 kind 分发
      ├─ turn_started/turn_phase → 状态指示器
      ├─ reasoning/text          → MessageItem 流式追加
      ├─ tool_dispatch/tool_result → ToolCallCard
      ├─ approval_request        → PermissionModal 弹出
      └─ turn_done               → 状态归位
gateway /api/agent/submit (POST) ──► 触发 agent 一轮
gateway /api/agent/approve (POST) ──► 权限审批回传
```

### 关键设计点

- **SSE 事件格式差异已处理**：rs-iot SSE 有 `event:` 头（`addEventListener(kind)`），serve SSE 无（`onmessage` + JSON.parse 看 kind）。两个 store 各自适配，gateway 透传不破坏（B1-1 已验）。
- **流式渲染**：MessageItem 的 token 流式追加用 zacp 的 `useMessageBlocks` composable（已处理负 id 占位→转正、流式抖动）。
- **页面上下文感知**：AI 面板发 prompt 时附带当前选中实例/设备 ID（monitor store 提供）。
- **轮询节流**：rtdata 2s + sysstatus 5s。tab 不可见时暂停轮询（`visibilitychange`），SSE 保持。

## 四、API 契约与鉴权

### 端点清单（经 gateway :8080）

**鉴权类**（gateway 自有）：
| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/api/gw/login` | body `{user_id, pass}`,转发 rs-iot `/api/rsiot/user/login`,透传返 JWT |
| GET | `/api/gw/health` | 健康聚合(免 JWT),返 `{rsiot, serve, mcp}` |

**监控类**（反代 rs-iot，JWT 保护）：
| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api/rsiot/events?types=log,packet,state&token=<JWT>` | SSE,监控主数据源 |
| GET | `/api/rsiot/rtdatas[?dev_id=]` | 实时数据(轮询) |
| GET | `/api/rsiot/inststatus` | 实例状态列表 |
| GET | `/api/rsiot/sysstatus` | 系统状态 |
| GET | `/api/rsiot/logs/recent?level=&target=&limit=` | 日志快照(兜底丢帧) |
| GET | `/api/rsiot/inst/trace` | 报文 trace 激活状态查询 |

**agent 类**（反代 serve，JWT 保护 + gateway 注入 serve cookie）：
| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api/agent/events?token=<JWT>` | SSE,agent 流(无 event: 头) |
| POST | `/api/agent/submit` | body `{input, context?}`,触发 agent 一轮 |
| POST | `/api/agent/approve` | body `{approval_id, decision}`,权限审批 |
| POST | `/api/agent/cancel` | 取消当前轮 |
| GET | `/api/agent/status` | agent 状态 |

### JWT 鉴权流

```
登录: POST /api/gw/login {user_id, pass}
      └─ gateway 转发 rs-iot /api/rsiot/user/login(验 bcrypt + 返 JWT)
      └─ gateway 透传 JWT 给前端(jwt_secret 同值,gateway 也认)
前端存 localStorage('rsiot_token'),所有 fetch + EventSource 自动带
```

### EventSource 鉴权（query token）

EventSource 标准 API 不能设 header。采用 query 参数：`new EventSource('/api/rsiot/events?types=...&token=<JWT>')`。

**gateway 改动**：`auth.rs` 的 `auth_middleware` 从 `?token=` query 也取 token（当前只取 Authorization header + cookie）。

## 五、错误处理与边界

### 错误分层

- **401**：清 localStorage token + 跳 `/login` + 关闭所有 EventSource（防 zombie）
- **5xx / 网络错**：监控组件显示"数据获取失败，重试"；AI 面板显示"提交失败"
- **SSE 断线**：浏览器自动重连（不手动）；重连后 monitor store 补拉 `/api/rsiot/logs/recent` 兜底丢帧；agent store 不补拉（serve 会话瞬时）
- **agent 繁忙（submit 409）**：serve 自动入 inbox，前端提示"已排队"
- **权限审批超时（serve 5min）**：PermissionModal 显示"审批超时"

### 边界情况

1. **上下文同步**：monitor store 暴露选中实例/tab，agent store submit 时读 monitor store 拼 context。若 serve submit 不支持额外 `context` 字段，拼进 input 文本兜底（`"[当前查看 mb-tcp-2002 报文] 分析这帧"`）。**待开发时验证 serve 契约**。
2. **SSE 生命周期**：登录后开两条 EventSource，登出全关。tab 后台时轮询暂停、SSE 保持。
3. **dev 跨端口**：vite proxy `/api` 到 :8080（同源，无 CORS）。SSE 经 vite proxy 若缓冲，加 `proxy.configure` 禁用（开发时验证）。
4. **大量日志/报文性能**：LogStream 保留最近 500 条，PacketTrace 保留最近 200 帧（DOM 节点上限，环形缓冲丢最旧）。

## 六、测试策略

保持轻量——核心验证人工 + 联调。

- **store 单测**（vitest）：monitor store SSE 事件分发、agent store kind 分发。纯逻辑无 DOM。
- **组件交互**（人工，走真实三件套）：登录→监控数据→AI 抽屉→发任务→流式输出→审批。
- **gateway 改动回归**：login + query token + 静态资源服务的 axum 单测。
- **不做**：组件快照测、E2E 自动化。

## 七、实施分期

| 里程碑 | 内容 | 成功标准 |
|---|---|---|
| **M1 骨架** | frontend/ Vue3+Vite 工程 + 最简页面；gateway rust-embed 静态服务 + fallback 路由；gateway `/api/gw/login`（转发 rs-iot）；gateway auth query token 支持 | `npm run build` 后 gateway :8080 访问到页面 + 登录拿 JWT |
| **M2 监控** | Monitor.vue + 采集器列表 + 4 tab；RtdataPanel/SystemStatus（轮询）；LogStream/PacketTrace（SSE）；monitor store | 监控界面实时显示 mb-tcp-2002/mb-rtu-com3 状态 + devA 数据 + 日志 + 报文 |
| **M3 AI 面板** | AgentDrawer；抽 zacp 6 组件；agent store（serve SSE + submit/approve）；上下文同步 | 发"读 devA 数据"→流式推理 + 工具卡 + 结果；写操作触发权限卡 |
| **M4 打磨** | 错误处理（401/502/SSE 重连）；日志/报文环形缓冲；dev vite SSE 验证；视觉打磨 | 错误边界全处理，长时间稳定 |

## 八、关键决策回顾

1. **范围**：2 核心（监控展示 + AI 面板），管理功能后置
2. **栈**：Vue3 + Vite + TS + Pinia + vue-router
3. **位置**：`frontend/`，构建产物嵌入 gateway（`#[folder] = "../../frontend/dist"`）
4. **布局**：监控全屏 + AI 右侧抽屉（380px，可收起）
5. **视觉**：zacp 浅色风（sky-blue + slate），组件零改造
6. **数据**：方案 A（SSE 推送为主 + rtdata/sysstatus 轮询）
7. **鉴权**：gateway 加 login（转发 rs-iot）+ query token（EventSource）
8. **分期**：M1 骨架 → M2 监控 → M3 AI → M4 打磨

## 九、gateway 改动清单（本设计需要的）

本设计需要 gateway 侧三处改动（rs-iot 主 crate + rsiot-mcp 零改动）：

1. **静态资源服务**：gateway 加 rust-embed（`#[folder] = "../../frontend/dist"`）+ 静态文件路由 + SPA fallback（非 `/api/*` 路径回退 index.html）
2. **`POST /api/gw/login`**：新增 handler，转发 rs-iot `/api/rsiot/user/login`，透传 JWT
3. **auth query token**：`auth.rs::auth_middleware` 增加 `?token=` query 参数取 token（EventSource 用）

## 十、未覆盖/待验证点

| 点 | 风险 | 验证时机 |
|---|---|---|
| serve submit body 是否支持 `context` 字段 | 中 | M3 开发时 curl serve /submit 验证 |
| vite proxy SSE 是否缓冲 | 低 | M1/M4 dev 时观察 |
| zacp 组件 props 契约与 serve 实际事件字段 | 中 | M3 抽取时核对 |
| gateway 静态资源 fallback 与反代路由优先级 | 低 | M1 实现时（`/api/*` 优先于 SPA fallback）|
