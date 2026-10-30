# 画面/主题版本管理实施计划（P0 第 1 轮 · TDD 任务表）

> **状态**：implemented（A1~A9 全部完成）
> **日期**：2026-10-30
> **实施**：2026-10-30 `166fe25` 版本层+三端点+SSE+指标（→ `40698d4` 文档收口）；11 组测试全绿 + just check 全过 + release 重编（--features history）warden 重启后 §6 六条实测全符
> **代码**：计划扩展 `src/api/routes_meta.rs`（版本层同文件）、`src/event_bus.rs`（Screen 变体）、`src/config.rs`（[meta] 段）、`src/process_metrics.rs` + `src/api/routes_metrics/render.rs`（四指标）、`src/api/mod.rs`（路由注册）；不新增模块
> **测试**：计划新增 `tests/meta_versioning.rs`（设计 §6 十一组）
> **遗留**：无（dev 构建须带 `--features history` 的教训已登记最终报告；紧缩读改写窗口等设计级已知限制见设计文档 §7）

权威设计：`docs/designs/2026-10-30-screen-versioning-design.md`（单一权威源，本文件
只做任务切分与状态跟踪）。任务书：hmi-studio
`docs/plans/2026-10-30-p0-p4-kickoff-brief.md`。

## 0. 开工基线（2026-10-30 实测，与任务书 §4 对比）——**存在漂移，已按 §7 停止**

| 门禁 | 任务书 §4 基线 | 本轮开工实测 | 判定 |
|---|---|---|---|
| `just check` | ✅ 全过（`✅ rs-iot 本地门禁全过`） | ❌ **两次瞬态红**：①`log_e2e::logs_recent_empty_returns_array` ②重跑 `events_e2e::events_with_cookie_auth_ok`，均为 lux data_dir flock `EAGAIN`（`tests/common/mod.rs:42`）；单跑复验 `--test log_e2e` ×3、`--test events_e2e` ×2 **全绿** | **漂移**（events_e2e 可勉强归入 §4 flaky 第 2 条；log_e2e 不在清单） |
| `cargo test --lib` | ✅ 670 passed, 0 failed, 9 ignored | 随 `just check` test 档首段通过（两次运行 lib 段均绿） | 一致 |
| `python3 scripts/check_docs.py` | ✅ 71 md 全过 | 随两次 `just check` docs 档通过 | 一致 |
| 工作区状态 | §4 晚间变更仅记「README/AGENTS/.gitignore 环境整理，不涉 Rust/前端代码」 | ❌ 实测另有**功能性未提交改动**（18 文件 +367 行）：`src/api/mod.rs`、`src/config.rs`、`src/api/routes_metrics/render.rs`、`src/api/routes_history.rs`、`src/history/{engine,mod}.rs`、`src/lib.rs`、`src/plugin/builtin/lua_client/mod.rs`、`tests/bind_fail_exit_e2e.rs`、`tests/history_bi_e2e.rs`、`docs/reference/rest-api.md` 等——内容为 BUG#11 深度审查修复（bind 级联/stats truncated/flush_gate 观测），已完成已验证未提交 | **漂移且命中 §7 停止条件**：「已有未提交改动涉及你要改的文件」——`src/api/mod.rs`、`src/config.rs`、`render.rs`、`rest-api.md` 均为本计划 A 系任务目标文件 |

停止理由：任务书 §7「基线漂移 → 先报告差异再等指示，不要自行判断后硬推」。分笔提交
纪律（§8：每笔可独立构建通过）在上述混流状态下无法干净执行。lux 瞬态红的机理排查
（时钟粒度 20 万次零重复证伪、/tmp 为 ext2/3 flock 正常、单跑绿）已完成到「非本轮
引入」判定，根因未定（多 binary `--test-threads=4` 并行档下偶发，建议后续单独立项）。

## 1. 设计偏差（开工前实读发现，以代码为准）

1. 设计 §3.6 说「Claims.sub 未注入请求扩展，需补提取」——实读 `src/api/auth.rs:96-100`
   **已存在 `AuthedUser` Extension 注入**（复审 M2 已做，via="jwt"；accesskey 直通不注入）。
   实现 A6 直接消费 `Extension<Option<AuthedUser>>`，auth.rs 零改动。

## 2. 任务表（状态真相源：☐ 未开始 / ⏳ 进行中 / ✅ 完成且验证过）

### A. P0 后端（screen-versioning-design.md 全文）

| ID | 内容 | 依赖 | 验证命令 | 状态 |
|---|---|---|---|---|
| A1 | 计划新增 `tests/meta_versioning.rs` 骨架 + 11 组用例先红（判重/捕获/乐观锁/回滚/TTL/紧缩/级联删/SSE/themes 同构/列表隔离/失败矩阵） | — | `cargo test --test meta_versioning 2>&1 \| tail -5`（红 = 编译失败或断言失败，逐组绿随 A2~A7 转绿） | ☐✅ |
| A2 | 存储层 `sv:` 键族 helper（seq INCR / meta hash / body SETEX + TTL）+ 元数据 JSON（seq/ts/author/size/sha）+ 删除级联（DEL×2 + `KEYS sv:{kind}:{id}:v:*` 逐删） | A1 | `cargo test --test meta_versioning cascade_and_theme`（第 7/9 组）+ `cargo test --lib` | ☐✅ |
| A3 | put 捕获链：HGET 旧 doc + seq → sha256 判重（同内容不建版直接返回当前 version）→ 旧内容 SETEX+HSET 快照 → INCR → HSET head（最后一步不变量）→ 失败矩阵语义（捕获失败 500 不写 head） | A2 | `cargo test --test meta_versioning dedup_and_capture`（第 1/2/11 组） | ☐✅ |
| A4 | `if_version` 乐观锁（query 参数；缺省无条件写；不匹配 409+current_version 不留痕；seq 缺失视为 0）+ `GET` 响应加 `version` 字段 | A3 | `cargo test --test meta_versioning optimistic_lock`（第 3 组） | ☐✅ |
| A5 | 三端点：`GET /{kind}/{id}/versions`（seq 降序 cap 条 + MGET 批量 expired 标记）、`GET .../versions/{seq}`（410/404 矩阵）、`POST .../rollback`（快照当前 head → 写目标正文 → INCR；可选 if_version） | A4 | `cargo test --test meta_versioning versions_list_fetch_rollback`（第 4/5 组） | ☐✅ |
| A6 | author 提取（`Extension<Option<AuthedUser>>`，缺失记 "unknown"）+ `EventPayload::Screen` 变体（enum/kind/publish 三处）+ delete 级联 + publish（判重命中不发帧） | A2 | `cargo test --test meta_versioning sse_and_author`（第 2/8 组）+ `cargo test --lib`（event_bus 穷尽性） | ☐✅ |
| A7 | 紧缩（HLEN > 2×cap 触发：HGETALL → 按 seq 降序保留 cap 条且正文存活 → DEL 整键 → 重写；每 PUT 至多一次）+ `[meta]` 配置段（`versions_ttl_secs`=2592000 / `versions_cap`=50，钳制 1..=500）+ 四指标（created/conflicts/rollbacks/compactions ×{kind}，挂 ProcessMetrics + render） | A5 | `cargo test --test meta_versioning ttl_compact_metrics`（第 6 组）+ `curl 127.0.0.1:8790/metrics \| grep rs_iot_meta_`（重启后实测） | ☐✅ |
| A8 | `docs/reference/rest-api.md` §4.19 增补（三端点契约 + if_version + version 字段 + 409/410 形态）+ §4.10 事件表加 `screen` 行 + `[meta]` 键入 §1 配置清单 | A2~A7 | `python3 scripts/check_docs.py` | ☐✅ |
| A9 | 全门禁 + 分笔提交（存储层/契约端点/SSE+指标/文档 四笔，每笔独立构建通过）+ `cargo build --release -p rs-iot`（单独构建）+ warden `dev-rs-iot` 重启 + 任务书 §6 六条 curl 实测 | A1~A8 | `just check`（两次瞬态红豁免口径见 §0）+ §6 六条逐条记录输出 | ☐⏳✅ |

## 3. 完成判据（任务书 §9 摘要）

- 11 组测试全绿；`just check` 除 §0 登记的瞬态项外全绿
- 服务重启后 §6 六条实测全部预期（409+current_version / 判重 version 不变 / versions 元数据 / rollback 双验 / 列表无 `sv:` 污染 / 四指标存在）
- e2e 预期副作用（w8 等播种产生版本条目由判重+TTL 消化）不修测试不加清理钩子

## 4. 实施后复审修复（2026-10-30）

对 P0 实现做深度复审后的修复（均为错误路径/边界语义，不改主流程）：

| 发现 | 修复 | 验证 |
|---|---|---|
| M1 紧缩的 MGET 失败被吞成"全过期"，随后仍 `DEL meta` + 重写空集 → 一次瞬态故障静默清空整屏版本历史 | MGET 失败/非数组/长度不符一律 `Err`（调用方本就 warn+跳过）；HGETALL 读到空表直接跳过，**存活判定不可信时绝不 DEL** | 代码路径 + `compaction_keeps_latest_cap` 零回归 |
| L4 判重先于 if_version：过期版本号 + 同内容可绕过乐观锁 | `if_version` 校验前移（提供且不等即 409），再判重；新增 `stale_if_version_conflicts_even_when_content_identical` | `cargo test --test meta_versioning` 14/14 |
| L5 GET/DELETE 把 seq 读失败吞成 0（与模块"所有 lux 调用判 Err"矛盾） | GET 改直接 HGET（Nil→404 / Err→500 / seq Err→500）；DELETE 记 warn 后继续（幂等优先） | 同上 |
| L6 回滚到与当前 head 同内容的版本仍建版 + INCR（重复历史条目） | `RollbackOutcome::Unchanged` no-op（不建版/不 INCR/不发帧/不计数）；新增 `rollback_to_same_content_is_noop` | 同上 |
| L13 迁移路径（遗留画面 seq 缺失→版本 0）与 delete/rollback SSE 帧无覆盖 | 新增 `legacy_screen_first_change_captures_version_zero`；SSE 用例扩为 put×2+rollback+delete 四帧 + 判重静默 | 同上 |
| L13 `[meta]` 钳制无单测 | 新增 `meta_config_sanitize_clamps_zero_and_overflow`（cap=0 会让每次写清空历史，必须锁死） | `cargo test --lib` 673/673 |
| L10 设计文档未回写 / metric family 计数错 | 设计头回写 implemented；§3.2 补"校验先于判重"、§3.4 补 rollback no-op、§4.4 补"存活判定不可信→放弃紧缩"；rest-api §4.10 18→22、§4.19 同步三处语义 | `python3 scripts/check_docs.py` |

验证口径：`cargo test --test meta_versioning` 14/14、`cargo test --lib` 673/673、`cargo fmt --check`、`cargo clippy -p rs-iot --all-targets -- -D warnings`。
**未重编 release、未重启 dev-rs-iot**——线上仍跑修复前二进制；重编（须 `--features history`）与 warden 重启留待用户裁决。
