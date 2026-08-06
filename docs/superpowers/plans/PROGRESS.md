# Lua 脚本采集 spec 实现进度（/loop 驱动）

> **运行时不变量**：CronCreate job `116fd1e9`（每 10m，session-only，7 天自动过期）。**轮内禁止 ScheduleWakeup**；完成/阻塞 → `CronDelete 116fd1e9`。每轮 cron 触发时读本文件 + plan checkbox + `git log --oneline -15` 重建上下文。

## 当前状态

- ✅ Plan 1（M1-M3 MVP）+ Plan 2（M4 联动与持久 + I2 重连）— 合并 master @ `136e9a9`（**107 测试全绿**）
- 🔵 下一步：写 Plan 3（M5 HTTP）plan

## Plan 路线图（spec §附 milestone）

| Plan | Milestone | 状态 | plan 文件 |
|---|---|---|---|
| 1 | M1-M3 MVP | ✅ done @ master | `2026-08-06-lua-client-mvp.md` |
| 2 | M4（L3 send_command + L4 kv + I2 TCP 重连） | ✅ done @ master（`136e9a9`，2026-08-07 合并） | `2026-08-06-lua-client-m4.md` |
| 3 | M5 HTTP（L5 http_get/post + 全审计，feature `lua-http`） | 🔵 待写 plan | — |
| 4 | M6 热加载 + `on_data` 被动帧 | ⏭ 待写 plan | — |
| 5 | M7 状态监测（`stat:<inst>`）+ REST（`routes_lua`） | ⏭ 待写 plan | — |
| 6 | M8 serial/udp 传输 + 文档 + 沙箱测补全 | ⏭ 待写 plan | — |

## Plan 2（M4）完成总结 ✅（合并 master @ `136e9a9`）

- [x] Task 1：`luakv.rs` + identity 清单 + 实例删除清理（`0ca0255`）
- [x] Task 2：`ctx:kv_set/kv_get` L4 API（`a042781`）
- [x] Task 3：`ctx:send_command` L3 联动（`9b33e34`）
- [x] Task 4：`TcpIo` 断连重连 I2（`a1bd236`；修 plan 两缺陷：tcp_roundtrip 改 `new`、request buf 非空+peer_closed 帧完成逻辑）
- [x] Task 5：M4 集成测（`136e9a9`；务实偏离 plan 3 轻量测 → 1 真实端到端测）

107 测试全绿（lib 90 + 集成测 17：lua_modbus 1 / lua_m4 1 / s7 3 / sidecar 4 / dataflow+api_flow+config_load 8）。

## 下一步（每轮 cron 触发执行）

**写 Plan 3（M5 HTTP）plan**：superpowers:writing-plans，spec §12（HTTP 出站与审计）+ §7 L5。`ctx:http_get/http_post`（reqwest async + create_async_function + 全审计日志）+ feature `lua-http` 门控（Cargo.toml，默认随 `lua` 开）+ 默认超时 5s + 响应体上限 1MB。文件名 `2026-08-07-lua-client-m5.md`。写完建 `feature/lua-m5` 分支执行。

## 接手模式说明（API 503 期间）

API 503 间歇故障（combo models / zai-b cooldown）致 subagent dispatch 不稳。Plan 2 Task 2 起控制器接手机械 task（plan 含完整代码 + cargo 不依赖 LLM API）：实现→自跑 CI 三项→自审→提交。独立 reviewer 因 API 不稳暂跳过。Task 4/5 还发现并修复 plan 实现缺陷。API 恢复后恢复 subagent-driven 双 review。

## 后续清理项（跨模块，非阻断）

- `luakv::get` 空 JSON 值误判 missing（与 `ext_token::get` 同根）— 区分 `EmbeddedValue::Nil` vs 空 Bulk/Simple
- `RsIotError::Config` 用于运行时校验语义勉强 — 待新增 `Validation` 变体后与 ext_token 一并迁移
- in-crate 单测 data_dir 按 pid 共享 — lux Tiered 锁目录致并发失败时跨模块统一改

## CI 命令（Windows，规避 link.exe 并行 OOM）

```bash
CARGO_BUILD_JOBS=1 cargo test --all-targets --all-features
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
```

## 阻塞点

- ⚠️ API 503 间歇故障 → subagent dispatch 不稳，控制器接手中。无逻辑阻塞。
