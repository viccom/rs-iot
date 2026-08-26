# 非破坏性质量修复包（审计后续）

> **状态**：implemented（2026-08-26 收口；B1-B7 全修，门禁三档全绿）

## 验收记录（commit 对照）

| 项 | commit | 锚点 |
|---|---|---|
| B1 谎报 ok | `8b7f486` | 6 处写点 warn+500（set/del/changepass/group×2/accesskey）+inst HDEL 对齐邻居；失败注入不可达豁免（EmbeddedClient 进程内直连 store），正向守门 set 回读一致 api_flow 4/4 |
| B2 毒化恢复 | `cc32af7` | discovery 生产 6 处+lua_client+lua_transform(.ok() 链适配)；红实证（临时回退单点→毒化 panic）→绿；测试体 4 处 unwrap 连带统一 |
| B3 弱密钥+bind_host | `17f8e6c` | DEFAULT_JWT_SECRET 常量+启动 error 提示（不拒启）+api_bind_host 增量键默认 0.0.0.0 零变化；config 56/56 |
| B4 消环 | `41f5769`+`6f53b02` | 三 helper 下沉 db，api 经 pub(crate) re-export；rg 实证 model/plugin 逆向引用清零（2 环消解）；3 个 e2e import 随迁；items-after-test-module lint 收尾 |
| B5 前端收敛 | `d7cd46c` | api/fetch.ts 拆档（plain object+大小写规范化出口，可观察形态逐字节一致）；18 处 Bearer 复制收敛；197 测断言零改动+build |
| B6 常量化 | `6c1e4e7` | 四常量+心跳单源（config 导出 sidecar 引用）+前端轮询四节拍+gateway 注释；纯重构零值变化 |
| B7 EngineParts | `8d73b45` | lib.rs 60 行引擎知识收敛 of()/disabled()；引擎所有权留调用方（D10 &mut close）；dataflow 2/2+init_failed 1/1 |

**门禁**（2026-08-26 实测）：fmt --check 过；clippy 双档零警告；默认档全绿；history 档全绿；minimal 358/0；前端 197+build。
> **日期**：2026-08-26
> **来源**：全仓三路审计（硬编码/空实现/耦合）+ 用户裁决「修复对当前代码不带来破坏的问题」
> **执行方式**：主会话直做（额度不稳先例）；TDD 能红则红，纯重构以编译+全量门禁为验证

## 〇、裁决记录（何为"非破坏"边界）

| 审计发现 | 本批处置 | 理由 |
|---|---|---|
| 端口 8780/8790 三处双源 | **已裁决收敛（2026-08-26）** | 用户裁定统一 8790（测试验证阶段无兼容负担）——config.rs 默认值/两 toml 注释/4 处 doc 注释/scada 页脚随迁，mcp/gateway 本就 8790；红→绿 config_load 2 断言 |
| secrets 未 init fallback | **撤销** | 复核发现已有 warn + 注释论证（"往返不坏/强度同默认 jwt_secret"），系知情设计非静默 |
| jwt 弱密钥 | **warn 版护栏**（不拒启）+ `api_bind_host` 增量配置（默认 0.0.0.0 不变） | 拒启属破坏；日志提示 + 可配置纯增量 |
| 谎报 ok | **修**（失败路径返 500 + warn） | 失败显示成功是缺陷；正常路径零变化；前端 500 透传已就绪 |
| 毒化级联 | **修**（into_inner 范式） | 仅毒化后行为不同（panic→恢复旧值），循 cache L-4 已裁决先例 |
| lux helper 下沉 / 常量化 / parts() 封装 / client.ts 拆分 | **修**（纯重构） | 行为等价，测试护航 |

## 一、任务清单

### B1：HTTP 写路径谎报 ok → 失败 warn + 500

**点位**（rg 实测）：routes_user.rs:36-37（set：`to_string().unwrap_or_default()` + HSET 吞错）、:50-51（del：双 HDEL 吞错）、routes_group.rs:29（set HSET）、:68（accesskey SET）、routes_inst.rs:208（del HDEL）。实现时再以宽模式复查 routes_user.rs:95（改密）与 routes_device/routes_inst 相邻写点，同类形态一并修。
**修法**：每处 `let _ =` 改为 match——Err(e) → `tracing::warn!` + 返回 500 JSON `{"error":"..."}`；序列化失败单独 500（不再写空串）。响应形态对齐仓库 500 惯例（routes_history.rs:220 的 `{"error": ...}`）。
**红测**：构造 lux 不可写调 set 端点断言 500；正常路径 e2e 零回归。若注入困难：现有 e2e 零回归 + 豁免登记。

### B2：毒化恢复（L-4 范式推广）

**点位**：model/discovery.rs 生产路径 6 处 `engine().lock().unwrap()`（110,223,309,320,338,356；497/609/671 判读是否测试模块内）；lua_client/mod.rs:72；lua_transform/mod.rs:338 `.lock().ok()`（毒化使 self-loop guard 静默失效）。
**修法**：统一 `unwrap_or_else(|p| p.into_inner())` + 一行注释（引 cache.rs L-4 先例）。
**红测**：discovery 毒化测试（持锁 panic 毒化 ENGINE → 调 load/note_once 不 panic）红→绿；lua 侧豁免（注入成本高），登记。

### B3：jwt 弱密钥启动护栏（warn 版）+ api_bind_host 增量配置

- config.rs：`AppConfig` 增 `api_bind_host: String`（默认 `"0.0.0.0"` 保持现状）+ `pub const DEFAULT_JWT_SECRET`。
- lib.rs:448 绑定改用 `cfg.api_bind_host`。
- 启动早期：`jwt_secret == DEFAULT_JWT_SECRET` → `tracing::error!`（不拒启）。
**测试**：config 默认值断言扩展；护栏行为豁免（起进程成本），dataflow e2e 零回归。rest-api.md config 键清单补 `api_bind_host`。

### B4：lux helper 下沉 db 层（消解 2 个循环依赖环）

`str_from_value`/`hgetall`/`hget` 自 api/mod.rs:113-154 移至 db/mod.rs（pub）；消费点全改 `crate::db::`：model/migrate.rs:187-188、lua_north/compat.rs:191,683,783 + api 内部。纯重构。**验证消环**：rg 确认 src/model/ 与 src/plugin/ 无 `crate::api::` 代码引用（doc 链接除外）。

### B5：前端 client.ts 拆两档 + 收敛鉴权拷贝

- 拆 `apiFetch(path, init?)`（Bearer 注入 + 401 clear，**无 router**）与薄 `api()`（401 导航副作用）。
- 收敛：HistoryQuery.vue、devreg.ts、secrets.ts、monitor.ts 裸 fetch → apiFetch（401 语义逐处核对对齐）；monitor.ts 绕开注释删除。
**验证**：vitest 全绿 + build。

### B6：常量化批量 + 心跳双源收敛（纯重构）

cookie Max-Age 86400、SECS_PER_DAY、lua http 1MiB、IO_CHAN_CAP 64、config.rs 导出 `DEFAULT_HEARTBEAT_TIMEOUT_SECS`（sidecar 引用）、monitor.ts 轮询四间隔常量化（双份收敛）、gateway providers 两处 timeout 注释。

### B7：lib.rs history init 封装

lib.rs:314-373 引擎三句柄/三态推导/双 cfg 块封装为 engine.rs `init_parts()`，lib.rs 收敛 ~5 行。纯重构：双档编译 + e2e 零回归。

## 二、顺序与门禁

B1 → B2 → B3 → B4 → B7（Rust 串行）→ B5 → B6 → 收口。
门禁：fmt、双档 clippy、三档 test、前端 vitest+build。验收线：全部零回归。

## 三、风险与回退

| 项 | 风险 | 回退 |
|---|---|---|
| B1 失败返 500 | 依赖"失败也 ok"的客户端（即缺陷本身）；e2e 旧断言若有依赖需同步 | revert |
| B3 新键 | 纯增量默认不变 | revert |
| B4 搬迁 | 编译期验证充分 | revert |
| B5 收敛 | vitest 护航；401 语义逐处核对 | revert |
| 端口统一 | **已裁决收敛 8790**（2026-08-26 用户裁定，收口 commit 见 git log；config_load 红→绿锚定） | 单 commit revert |
