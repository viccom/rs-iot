# 多系统 API Gateway 分层设计（gateway-core 泛化）

> **状态**：accepted（定稿待实施）
> **日期**：2026-08-18（定稿）
> **实施**：未开始
> **代码**：目标 `crates/gateway-core/`（新建通用引擎）+ `crates/rsiot-gateway/`（改薄组装）
> **测试**：随实施建立（现有 gateway 单测迁移守护）
> **遗留**：全部（workspace 拆 crate / 配置 v2 上游表 / ext 扩展边界）
>
> 2026-08-18 brainstorming 定稿。前置评审：当前 `crates/rsiot-gateway` 传输核心（SSE 裸透传/凭证注入/JWT 复用）质量良好，
> 问题是角色漂移——"纯无状态反代"长成了 reasonix 配置面/文件面宿主，业务专属逻辑与通用引擎混在同一 crate。
> 本文档是本次改造的唯一权威设计；与代码冲突时以代码为准并回改本文。
> 选型依据：`docs/decisions/2026-08-12-gateway-tech-selection.md`（v3 网关 axum 0.8 + reqwest 手写反代，结论不变，本次只动分层不动传输设计）。

## 一、定位与边界

**目标**：把 rs-iot 专用网关改造为支持多个业务系统的通用网关——分层、模块化、低耦合。

**范围决策**（brainstorming 确认）：**同域多服务**——后续接入的系统与 rs-iot 共享同一 JWT 用户体系
（同一 `jwt_secret`、同一登录端点）。**不做**身份联邦、不做每后端独立用户库、不做运行时动态插件加载。

新系统接入分两档：

| 系统形态 | 接入成本 |
|---|---|
| 纯反代（REST/SSE 透传） | 只加 `[[upstreams]]` 配置段，零代码 |
| 有专属逻辑（配置面/文件面/请求改写） | 在 rsiot-gateway `src/ext/` 加模块（系统多了再拆独立薄 crate，本次不做） |

**非目标**：路由前缀变更（对外行为零变化，前端零改动）、二进制改名（现场服务名稳定优先）、
providers/attachments 迁回 reasonix（边界先隔离，归属另议）。

## 二、架构与依赖方向

```
crates/
├─ gateway-core/          ← 通用引擎，零业务知识（不知道 rs-iot/reasonix 存在）
│  └─ src/
│     ├─ config.rs        # [auth] + [[upstreams]] 解析与校验
│     ├─ auth.rs          # JWT 中间件（Bearer + ?token= + public paths）
│     ├─ proxy.rs         # forward 通用透传 + 凭证机制分流
│     ├─ health.rs        # 自动探全部 upstreams
│     ├─ login.rs         # POST /api/gw/login → auth.login_url 字节透传
│     └─ assets.rs        # 泛型 serve<E: RustEmbed>（SPA 静态托管）
│
└─ rsiot-gateway/          ← 薄组装层（业务专属全在这）
   └─ src/
      ├─ main.rs          # 读 TOML → core 配置 + [reasonix] 扩展配置双解析 → 组装
      ├─ app.rs           # core 路由构建器 + ext 子路由 + 静态资源挂载
      └─ ext/
         ├─ agent.rs      # /api/agent：HINT 注入 + 斜杠命令守卫 + fallback 透传
         ├─ providers.rs  # 原样迁移（reasonix 配置面）
         └─ attachments.rs# 原样迁移（reasonix 文件面）
```

依赖单向：`rsiot-gateway → gateway-core`，编译器强制 core 不 import 任何业务概念。

core 认识的只有通用网关概念：路由表、JWT 校验、凭证机制（`none` / `jwt-passthrough` / `static-cookie`）、
SSE 字节透传、健康聚合、登录转发、嵌入式静态资源。

**备选与否决**：单 crate 目录分层（`src/core/` + `src/ext/`）改动更小但边界靠约定，编译器不拦
core 混入业务代码——否决；运行时插件（WASM/动态库/dlopen）对同域多服务是投机机制——否决。

## 三、配置模型（gateway.toml v2，破坏性切换）

```toml
listen = "0.0.0.0:8080"

[auth]
jwt_secret = "<与 rs-iot 同值>"
login_url  = "http://127.0.0.1:8790/api/v1/user/login"   # POST /api/gw/login 转发目标
# public_paths 缺省 ["/api/gw/health", "/api/gw/login"]

[[upstreams]]
id         = "rsiot"
prefix     = "/api/rsiot"
base_url   = "http://127.0.0.1:8790"
credential = "jwt-passthrough"          # C1：?token= → Authorization 注入

[[upstreams]]
id         = "agent"
prefix     = "/api/agent"
base_url   = "http://127.0.0.1:8787"
credential = "static-cookie"            # 固定 cookie 注入
cookie_name  = "reasonix_token"
cookie_value = "<serve --auth token>"

[reasonix]     # 扩展段：core 不解析；rsiot-gateway 对同一 TOML 文档二次解析
home_dir      = "<REASONIX_HOME>"
warden_url    = "http://127.0.0.1:9100"
service_name  = "reasonix"              # 缺省
workspace_dir = "<reasonix 工作区根>"
```

- **不保留旧扁平格式（`rsiot_base`/`serve_base`/`serve_token`）兼容垫层**——项目"显式配置、缺失即失败"
  文化；`field-kit/config/gateway.toml.example`、`field-kit/README.md`、`docs/ops/deploy-gateway-and-mcp.md`、
  CLAUDE.md 同步更新。
- 启动校验：prefix 必须以 `/` 开头、互不冲突（一个是另一个前缀即冲突）、credential 字段完备
  （`static-cookie` 必带 name/value），失败即拒启。
- 凭证机制语义（core 内 `Credential` 枚举）：

| credential | 行为 |
|---|---|
| `none` | 纯透传，出站仅剥 hop-by-hop 头 |
| `jwt-passthrough` | 透传 `Authorization`；入站 `?token=` 转为 `Authorization: Bearer`（C1，EventSource 场景） |
| `static-cookie` | 注入固定 `Cookie: <name>=<value>`（替换入站 cookie，防浏览器 cookie 漏给上游） |

- `token=` query 参数对**所有上游**统一剥离（网关自身鉴权参数，不属于任何业务，防泄漏到上游日志）。

## 四、gateway-core 模块设计

- **`config.rs`**：`[auth]` + `[[upstreams]]` 解析；未知段（`[reasonix]` 等）留给 app 层二次解析。
  启动时校验（见上）。
- **`auth.rs`**：现 `crates/rsiot-gateway/src/auth.rs` 原样迁移——中间件（Bearer 优先、`?token=` 兜底、
  非 `/api/` 放行静态资源）、`Claims`、`verify_token`。顺带收敛上轮评审的"Claims 与主 crate 复制"问题：
  今后网关侧只维护 core 这一处。
- **`proxy.rs`**：`forward(state, req, upstream: &UpstreamCfg)`——现逻辑泛化，`Upstream` 枚举换成配置表条目，
  credential 按种类分流。SSE 裸字节透传（`Body::from_stream(bytes_stream())`）、hop-by-hop 剥离、
  `x-accel-buffering: no`、无全局 timeout——全部不动（选型文档结论仍权威）。
- **`health.rs`**：自动探全部 upstreams（probe 语义不变：2s 超时、任何 HTTP 响应含 4xx 算在线）。
  响应泛化为 `{"upstreams": {"rsiot": "online", "agent": "online"}}`。
  消费者兼容已核实：`field-kit/start-rsiot.bat:135` 仅 `findstr "online"` 字符串匹配；前端不调此端点；
  README 文档同步更新。原 `mcp: "unknown"` 占位字段删除（无消费者）。
- **`login.rs`**：`auth.login_url` 已配置时挂载 `POST /api/gw/login`，请求/响应**字节透传**——
  不再 typed `LoginReq`，任何身份提供方的登录表单结构都能过（现 typed 版对 rs-iot `{user_id, pass}`
  的耦合解除）。
- **`assets.rs`**：泛型 `serve<E: RustEmbed>(uri) -> Response`（SPA fallback 逻辑原样泛化），
  业务 crate 定义自己的 embed 结构体（rsiot-gateway 的 `FrontendAsset` 指 `../../frontend/dist`）调它。

## 五、rsiot-gateway 组装层

- **路由组装**：core 提供路由构建器 `RouterBuilder::new(cfg).skip_prefix("/api/agent").build(state)`——
  被 skip 的前缀由 ext 接管，core 不挂裸透传（防路由冲突）；未被 skip 的 upstream 由 core 自动
  `nest_service(prefix, forward)`。
- **ext/agent.rs**：`/api/agent` 子路由——`/submit` HINT 注入（含斜杠命令守卫，对齐 serve.go
  `HasPrefix` 拦截的业务语义，留在 ext 层）、`/providers*`、`/attachments`、`/workspace`、
  fallback 裸透传（内部调 `core::forward` + agent upstream 条目）。
- **ext/providers.rs、ext/attachments.rs**：原样迁移，仅状态引用从 `GwState` 换成
  `CoreState + ReasonixCfg`（handler 签名改为 axum `State<(Arc<CoreState>, ReasonixCfg)>` 或双 State 组合，
  实现时按 axum 0.8 最顺的形式定）。
- **静态资源**：`FrontendAsset` 定义与 fallback 挂载留 app 层，调 `core::assets::serve`。

## 六、对外行为兼容性承诺

| 项 | 变化 |
|---|---|
| `/api/rsiot/*`、`/api/agent/*`、`/api/gw/login` | 零变化（前缀、鉴权流、SSE、上传限额、HINT 行为全不变） |
| `/api/gw/health` | 响应结构泛化（见上），唯一脚本消费者已核实兼容 |
| gateway.toml | v2 破坏性切换，模板/文档同步 |
| 前端 | 零改动 |

## 七、测试与迁移步骤

单测随模块走：auth/proxy/config/health 的 ~15 个单测迁入 core；providers/attachments 的留 app 层；
7 个 e2e 留 `rsiot-gateway/tests`（`build_router` 在 app 层，组装行为不变）。

迁移四步，每步 `cargo test -p gateway-core -p rsiot-gateway` 全绿再进下一步：

1. **建 gateway-core crate**：搬 auth/proxy（泛化 UpstreamCfg）+ health/login 泛化 + assets 泛型化 + 单测迁入；
   rsiot-gateway 依赖 core 删除本地副本 → 全绿。
2. **配置 v2**：`[[upstreams]]` + `[auth]` 生效；field-kit 模板、README、deploy 文档同步 → 全绿 + e2e 过。
3. **ext 目录化**：providers/attachments/agent 聚到 `src/ext/`，路由组装收敛 → 全绿。
4. **文档收尾**：本设计文档状态更新 + CLAUDE.md gateway 章节改写。

## 八、决策记录

| 决策 | 选择 | 否决项与理由 |
|---|---|---|
| 分层形式 | workspace 拆 crate | 单 crate 目录分层（边界靠约定）；运行时插件（投机机制） |
| 用户体系 | 同域共享 JWT，core 只做校验 | 身份联邦/多用户库（范围决策为同域多服务） |
| 配置兼容 | v2 直接切换 | 兼容垫层（违背显式配置文化，现场模板同步更新即迁移成本） |
| health 结构 | 泛化为 upstreams 表 | 保留 rsiot/serve/mcp 专属形状（消费者已核实兼容，无保留必要） |
| login 实现 | 字节透传 | typed LoginReq（对 rs-iot 表单结构耦合） |
| HINT/斜杠守卫归属 | ext/agent.rs | core（业务语义不进通用层） |
| 二进制名 | 不改 | 改名（纯化妆，现场服务名稳定优先） |

## 九、遗留与后续（不在本次范围）

- providers/attachments 的镜像耦合（对齐 reasonix internal 常量/清单）仍在——边界已隔离到 ext，
  归属（迁回 reasonix admin API vs 常驻 gateway）另议，建议加契约测试锁定对齐。
- `fetch_models` 存量密钥可被携带到任意 URL 的安全问题（上轮评审 P1）——独立修复，不与本分层改造捆绑。
- 未来接异域系统时：`[auth]` 段扩展为多身份源 + core 凭证机制扩 per-upstream secret 引用，本设计的
  `Credential` 枚举与配置表结构已留缝（加枚举值 + 加配置字段，不动组装层）。
