# Lua 脚本采集 spec 实现进度（/loop 驱动）

> **运行时不变量**：CronCreate job `116fd1e9`（每 10m，session-only，7 天自动过期）。**轮内禁止 ScheduleWakeup**；完成/阻塞 → `CronDelete 116fd1e9`。每轮 cron 触发时读本文件 + plan checkbox + `git log --oneline -15` 重建上下文。

## 当前状态

- ✅ Plan 1（M1-M3）+ Plan 2（M4）+ Plan 3（M5 HTTP）— 合并 master @ `6ff4ff0`（**108 测试全绿**）
- 🔵 下一步：写 Plan 4（M6 热加载 + on_data 被动帧）plan

## Plan 路线图（spec §附 milestone）

| Plan | Milestone | 状态 | plan 文件 |
|---|---|---|---|
| 1 | M1-M3 MVP | ✅ done @ master | `2026-08-06-lua-client-mvp.md` |
| 2 | M4（L3 send_command + L4 kv + I2 TCP 重连） | ✅ done @ master（`136e9a9`） | `2026-08-06-lua-client-m4.md` |
| 3 | M5 HTTP（L5 http_get/post + 全审计，feature `lua-http`） | ✅ done @ master（`6ff4ff0`，2026-08-07 合并） | `2026-08-07-lua-client-m5.md` |
| 4 | M6 热加载（notify + debouncer + 重建 VM + 失败回退）+ `on_data` 被动帧 | 🔵 待写 plan | — |
| 5 | M7 状态监测（`stat:<inst>`）+ REST（`routes_lua`） | ⏭ 待写 plan | — |
| 6 | M8 serial/udp 传输 + 文档 + 沙箱测补全 | ⏭ 待写 plan | — |

## Plan 3（M5 HTTP）完成总结 ✅（合并 master @ `6ff4ff0`）

- [x] Task 1：Cargo.toml `lua-http` feature + reqwest 依赖（`79ad01b`；默认开 / 关 lua-http 双向编译验证）
- [x] Task 2：`ctx:http_get/http_post` + 全审计 + 5s 默认超时 + 1MB 上限 + mock server 测试（`6ff4ff0`）

108 测试全绿（lib 91 + 集成测 17）。spec §12/§7 L5/§16 全覆盖（per-call Client / bytes() 后 check 上限 / config 字段三项简化注明后置）。

## 下一步（每轮 cron 触发执行）

**写 Plan 4（M6 热加载 + on_data）plan**：superpowers:writing-plans。
- **热加载**（spec §5.3）：`notify::RecommendedWatcher` + `notify-debouncer-mini` 去抖 200ms → 收 `<name>/` 改动 → 编译新 VM（沙箱+hook）→ dry-run（on_init + 一次 collect 严格 hook）→ 成功切换（旧 VM CancellationToken 退出）/ 失败 warn 回退（旧 VM 继续跑，永不半生效）。Cargo.toml lua feature 加 `dep:notify,dep:notify-debouncer-mini`。
- **on_data 被动帧**（spec §6/§8）：IO task 在「无在飞 request」时收到的帧 → 经独立 channel → 主循环 select! `Evt::Data(frame)` → `call_on_data`。data_rx 已在主循环 select! 待接（spec §5.4 line 136）。

文件名 `2026-08-07-lua-client-m6.md`。写完建 `feature/lua-m6` 分支执行。

## 接手模式说明（API 503 期间）

API 503 间歇故障（combo models / zai-b cooldown）致 subagent dispatch 不稳。Plan 2 起控制器接手机械 task（plan 含完整代码 + cargo 不依赖 LLM API）：实现→自跑 CI 三项→自审→提交。独立 reviewer 暂跳过。API 恢复后恢复 subagent-driven 双 review。

## 后续清理项（跨模块，非阻断）

- `luakv::get` 空 JSON 值误判 missing（与 `ext_token::get` 同根）— 区分 `EmbeddedValue::Nil` vs 空 Bulk/Simple
- `RsIotError::Config` 用于运行时校验语义勉强 — 待新增 `Validation` 变体后与 ext_token 一并迁移
- in-crate 单测 data_dir 按 pid 共享 — lux Tiered 锁目录致并发失败时跨模块统一改
- M5 http 简化：reqwest Client per-call（共享连池后置）/ bytes() 后 check 上限（流式 limit 后置）/ `app_config.http_timeout_secs` 未接（Lua 传 timeout 覆盖）

## CI 命令（Windows，规避 link.exe 并行 OOM）

```bash
CARGO_BUILD_JOBS=1 cargo test --all-targets --all-features
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
```

## 阻塞点

- ⚠️ API 503 间歇故障 → subagent dispatch 不稳，控制器接手中。无逻辑阻塞。
