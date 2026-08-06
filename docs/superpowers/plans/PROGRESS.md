# Lua 脚本采集 spec 实现进度（/loop 驱动）

> **运行时不变量**：CronCreate job `116fd1e9`（每 10m，session-only，7 天自动过期）。**轮内禁止 ScheduleWakeup**；完成/阻塞 → `CronDelete 116fd1e9`。每轮 cron 触发时读本文件 + plan checkbox + `git log --oneline -15` 重建上下文。

## 当前状态

- ✅ Plan 1（M1-M3）+ Plan 2（M4）+ Plan 3（M5 HTTP）+ Plan 4（on_data M2 欠账）— 合并 master @ `797545d`（110 测试绿）
- 🔵 Plan 5（M6 热加载）— **plan 就绪**（`2026-08-07-lua-client-m6.md`，4 task），待建分支执行

## Plan 路线图（spec milestone + 欠账补全）

| Plan | 内容 | 状态 | plan 文件 |
|---|---|---|---|
| 1 | M1-M3 MVP | ✅ done @ master | `2026-08-06-lua-client-mvp.md` |
| 2 | M4（L3 send_command + L4 kv + I2 重连） | ✅ done @ master（`136e9a9`） | `2026-08-06-lua-client-m4.md` |
| 3 | M5 HTTP（L5 http_get/post + 审计） | ✅ done @ master（`6ff4ff0`） | `2026-08-07-lua-client-m5.md` |
| 4 | on_data 被动帧（M2 欠账） | ✅ done @ master（`797545d`） | `2026-08-07-lua-client-on-data.md` |
| 5 | M6 热加载（notify + VM 重建 + dry-run + 失败回退） | 🔵 plan 就绪，待执行 | `2026-08-07-lua-client-m6.md` |
| 6 | M7 状态监测（`stat:<inst>`）+ REST（`routes_lua`） | ⏭ 待写 plan | — |
| 7 | M8 serial/udp + L2 数据 API + 文档 + 沙箱测补全 | ⏭ 待写 plan | — |

## Plan 5（M6 热加载）执行进度（待建分支 `feature/lua-m6`）

- [ ] Task 1：Cargo.toml lua feature 加 notify + notify-debouncer-mini ← 🔵 下一个
- [ ] Task 2：mod.rs Evt 枚举 + 主循环 Evt 重构 + `reload_vm`（new_vm+load+register+dry-run on_init）+ Evt::Reload 替换 lua（核心 VM 重建）
- [ ] Task 3：`hotload.rs` notify watcher（监听 scripts/<name>/ 去抖 200ms → reload_tx）
- [ ] Task 4：热加载集成测（改脚本 → reload → 新 collect 值）

## 下一步（每轮 cron 触发执行）

1. 建 `feature/lua-m6` 分支（从 master `7416127`）
2. 执行 **Task 1**（Cargo notify + notify-debouncer-mini）：控制器接手（plan 含完整代码）或 subagent（API 恢复）。
3. Task 1-4 完成 → 合并 → 写 Plan 6（M7 状态 REST）plan。

## 已知简化 / 风险（Plan 5，注明）

- **dry-run = on_init only**（spec 说 on_init + collect；collect dry-run 真发 request 有副作用，v1 跳过，hook 兜底死循环）
- **notify Windows 事件丢失**（spec §18）→ 去抖 + M7 REST 手动 reload 兜底
- **VM 重建不重启 IO 连接**（新 VM 复用 io_engine；需重连则手动 stop/start 实例）
- notify-debouncer-mini 0.7 API 实现时按实际调整（plan 给常见用法设计）

## 接手模式说明（API 503 期间）

API 503 间歇故障致 subagent dispatch 不稳。Plan 2 起控制器接手机械 task（plan 含完整代码 + cargo 不依赖 LLM API）。独立 reviewer 暂跳过。API 恢复后恢复 subagent-driven 双 review。

## 后续清理项（跨模块，非阻断）

- `luakv::get` 空 JSON 值误判 missing（与 `ext_token::get` 同根）
- `RsIotError::Config` 用于运行时校验语义勉强（待 `Validation` 变体）
- in-crate 单测 data_dir 按 pid 共享
- M5 http 简化：reqwest Client per-call / bytes() 后 check 上限 / `http_timeout_secs` config 未接
- on_data 混合协议竞争丢帧（纯被动不受影响）

## CI 命令（Windows，规避 link.exe 并行 OOM）

```bash
CARGO_BUILD_JOBS=1 cargo test --all-targets --all-features
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
```

## 阻塞点

- ⚠️ API 503 间歇故障 → subagent dispatch 不稳，控制器接手中。无逻辑阻塞。
