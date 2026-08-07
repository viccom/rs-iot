# Lua 脚本采集 spec 实现进度（/loop 驱动）

> **运行时不变量**：CronCreate job `39632fab`（每 10m，session-only，7 天自动过期）。**轮内禁止 ScheduleWakeup**；完成/阻塞 → `CronDelete 39632fab`。每轮 cron 触发时读本文件 + plan checkbox + `git log --oneline -15` 重建上下文。

## 当前状态

- ✅ Plan 1-5（M1-M6：MVP / M4 联动持久 / M5 HTTP / on_data / M6 热加载）— 合并 master @ `cf92eee`（**111 测试全绿**）
- ✅ Plan 6（M7 状态监测 + REST）plan 就绪 @ `939fe25`（2026-08-07）
- 🔵 Plan 6 执行中：分支 `feature/lua-m7`，Task 1 ✅ @ `f5d25bc`（db/luastat 3 单测绿），下一步 Task 2（config fail_threshold）

## Plan 路线图（spec milestone + 欠账补全）

| Plan | 内容 | 状态 | plan 文件 |
|---|---|---|---|
| 1 | M1-M3 MVP | ✅ done @ master | `2026-08-06-lua-client-mvp.md` |
| 2 | M4（L3 send_command + L4 kv + I2 重连） | ✅ done @ master（`136e9a9`） | `2026-08-06-lua-client-m4.md` |
| 3 | M5 HTTP（L5 http_get/post + 审计） | ✅ done @ master（`6ff4ff0`） | `2026-08-07-lua-client-m5.md` |
| 4 | on_data 被动帧（M2 欠账） | ✅ done @ master（`797545d`） | `2026-08-07-lua-client-on-data.md` |
| 5 | M6 热加载（notify + VM 重建 + dry-run + 失败回退） | ✅ done @ master（`cf92eee`，2026-08-07 合并） | `2026-08-07-lua-client-m6.md` |
| 6 | M7 状态监测（`stat:<inst>`）+ REST（`routes_lua` stats/reload） | 🔵 plan 就绪 @ `939fe25`，待执行 | `2026-08-07-lua-client-m7.md` |
| 7 | M8 serial/udp 传输 + L2 数据 API（set_value/get_value/...）+ 文档 + 沙箱测补全 | ⏭ 待写 plan | — |

## Plan 5（M6 热加载）完成总结 ✅（合并 master @ `cf92eee`）

- [x] Task 1：Cargo notify 8 + notify-debouncer-mini 0.7（`c079aca`）
- [x] Task 2：Evt 枚举 + 主循环 Evt 重构 + `reload_vm`（new_vm+load+register+dry-run on_init）+ Evt::Reload 替换 lua（`25efc3b`）
- [x] Task 3：`hotload.rs` notify watcher（去抖 200ms → reload_tx）（`4d2fb62`；**修复** std::thread 替 spawn_blocking 避免 runtime shutdown 卡）
- [x] Task 4：热加载集成测（改脚本 v1→v2 → reload → 新 collect 值）（`cf92eee`）

111 测试全绿（lib 92 + 集成测 19）。spec §5.3 热加载覆盖（notify + VM 重建 + dry-run + 失败回退「永不半生效」）。

## 下一步（每轮 cron 触发执行）

**执行 Plan 6（M7 状态监测 + REST）**：建 `feature/lua-m7` 分支（从 master `939fe25`），subagent-driven 逐 task（API 503 时控制器接手）。Plan `2026-08-07-lua-client-m7.md` @ `939fe25`，5 task：
- ✅ Task 1：`db/luastat.rs`（LuaStat + write/read，lux `stat:<inst>` SET/GET JSON）+ `db/mod.rs` 引出 + `identity.rs` 清单注释。— @ `f5d25bc`（3 单测绿）
- 🔵 Task 2：`lua_client/config.rs` 加 `fail_threshold: u32`（连续失败 N 次标 degraded，默认 10）。（下一步）
- ⏳ Task 3：`lua_client/mod.rs` run() stat 采点（启动 running / Tick collect 计数+耗时 / Reload vm_restart+1 / 退出 stopped）+ `script_version`（main.lua sha256 前 8 hex）。
- ⏳ Task 4：`api/routes_lua.rs`（stats/reload/scripts，query 参数 `?inst_id=`，JWT 默认保护）+ `api/mod.rs` 注册三路由。
- ⏳ Task 5：集成测（`lua_stat_e2e` 采集+stopped / `lua_api_e2e` REST stats/reload/scripts/JWT）。

**关键设计（plan 已定）**：REST reload = `mgr.stop + start_source(config_from_lux)`（复用 InstanceManager，spec §5.3「VM 替换 ≈ 轻量重启 instance」；IO 重连，结果与 notify 自动热加载一致）；stat 用 lux **string key**（SET/GET，非 Hash）；REST **query 参数**（贴 `routes_inst::IdQuery` 约定，避 axum 0.8 路径参数 `{param}`/`:param` 语法歧义）。
**v1 简化（plan 注明）**：`io_connected`/`last_io_err` 后置（io_engine 未暴露连接状态，需 io_engine 改造）；`script_version` = main.lua 单文件 sha256 前 8 hex（不含 lib/）；stat 按 collect 周期计（多 dev 任一失败记 fail 周期）。

## 已知简化 / 风险（累计，注明）

- Plan 5 dry-run = on_init only；notify Windows 事件丢失（去抖 + M7 REST reload 兜底）；VM 重建不重启 IO；watcher std::thread
- Plan 4 on_data 混合协议竞争丢帧（纯被动不受影响）
- Plan 3 http：reqwest Client per-call / bytes() 后 check 上限 / http_timeout_secs config 未接
- mod.rs reload channel 注释 _reload_tx 过时待清（Plan 5 Task 3 遗留）

## 接手模式说明（API 503 期间）

API 503 间歇故障致 subagent dispatch 不稳。Plan 2 起控制器接手机械 task（plan 含完整代码 + cargo 不依赖 LLM API）。独立 reviewer 暂跳过。API 恢复后恢复 subagent-driven 双 review。

## 后续清理项（跨模块，非阻断）

- `luakv::get` 空 JSON 值误判 missing（与 `ext_token::get` 同根）
- `RsIotError::Config` 用于运行时校验语义勉强（待 `Validation` 变体）
- in-crate 单测 data_dir 按 pid 共享
- M5 http 简化（per-call Client / 流式 limit / config 字段）
- on_data 混合协议竞争丢帧
- mod.rs reload channel 注释 _reload_tx 过时待清

## CI 命令（Windows，规避 link.exe 并行 OOM）

```bash
CARGO_BUILD_JOBS=1 cargo test --all-targets --all-features
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
```

## 阻塞点

- ⚠️ API 503 间歇故障 → subagent dispatch 不稳，控制器接手中。无逻辑阻塞。
