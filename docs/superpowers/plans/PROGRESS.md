# Lua 脚本采集 spec 实现进度（/loop 驱动）

> **运行时不变量**：CronCreate job `116fd1e9`（每 10m，session-only，7 天自动过期）。**轮内禁止 ScheduleWakeup**；完成/阻塞 → `CronDelete 116fd1e9`。每轮 cron 触发时读本文件 + plan checkbox + `git log --oneline -15` 重建上下文。

## 当前状态

- ✅ Plan 1（M1-M3 MVP）+ Plan 2（M4 联动与持久 + I2 重连）— 合并 master @ `136e9a9`（107 测试全绿）
- 🔵 Plan 3（M5 HTTP）— **plan 就绪**（`2026-08-07-lua-client-m5.md`，2 task），待建分支执行

## Plan 路线图（spec §附 milestone）

| Plan | Milestone | 状态 | plan 文件 |
|---|---|---|---|
| 1 | M1-M3 MVP | ✅ done @ master | `2026-08-06-lua-client-mvp.md` |
| 2 | M4（L3 send_command + L4 kv + I2 TCP 重连） | ✅ done @ master（`136e9a9`） | `2026-08-06-lua-client-m4.md` |
| 3 | M5 HTTP（L5 http_get/post + 全审计，feature `lua-http`） | 🔵 plan 就绪，待执行 | `2026-08-07-lua-client-m5.md` |
| 4 | M6 热加载 + `on_data` 被动帧 | ⏭ 待写 plan | — |
| 5 | M7 状态监测（`stat:<inst>`）+ REST（`routes_lua`） | ⏭ 待写 plan | — |
| 6 | M8 serial/udp 传输 + 文档 + 沙箱测补全 | ⏭ 待写 plan | — |

## Plan 3（M5 HTTP）执行进度（待建分支 `feature/lua-m5`）

- [ ] Task 1：Cargo.toml `lua-http` feature + reqwest 依赖（默认开，独立可关）← 🔵 下一个
- [ ] Task 2：`ctx:http_get/http_post` + 全审计 + 5s 默认超时 + 1MB 上限（api.rs，`#[cfg(feature="lua-http")]`）

## 下一步（每轮 cron 触发执行）

1. 建 `feature/lua-m5` 分支（从 master `136e9a9`）
2. 执行 **Task 1**（Cargo.toml）：subagent-driven（若 API 恢复）或控制器接手。**注意**：Task 1 引入 reqwest + rustls，首次编译 ~3-5min（预期）。验证 `cargo build`（默认）+ `cargo build --no-default-features --features lua`（关 lua-http 不编 http）。
3. Task 1+2 完成 → `finishing-a-development-branch` 合并 → 写 Plan 4（M6 热加载 + on_data）plan。

## Plan 2 完成总结 ✅（合并 master @ `136e9a9`，2026-08-07）

5 task 全完成（luakv 数据层 / ctx:kv_set/kv_get L4 / ctx:send_command L3 / TcpIo 断连重连 I2 / M4 集成测），107 测试全绿。接手模式（API 503）+ 修 plan 两缺陷（tcp_roundtrip、request 帧完成逻辑）。

## 接手模式说明（API 503 期间）

API 503 间歇故障（combo models / zai-b cooldown）致 subagent dispatch 不稳。Plan 2 Task 2 起控制器接手机械 task（plan 含完整代码 + cargo 不依赖 LLM API）：实现→自跑 CI 三项→自审→提交。独立 reviewer 暂跳过。API 恢复后恢复 subagent-driven 双 review。

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
