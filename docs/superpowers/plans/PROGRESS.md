# Lua 脚本采集 spec 实现进度（/loop 驱动）

> **运行时不变量**：CronCreate job `116fd1e9`（每 10m，session-only，7 天自动过期）。**轮内禁止 ScheduleWakeup**（cadence 全交 cron）；spec 全完成或阻塞 → `CronDelete 116fd1e9` 收尾。每轮 cron 触发时读本文件 + plan checkbox + `git log --oneline -15` 重建上下文。

## 当前状态

- ✅ Plan 1 / M1-M3 MVP（最小采集闭环）— 合并 master @ `4423da6`（99 测试绿）
- 🔵 Plan 2 / M4（联动与持久 + I2 重连）— **plan 已写**（`2026-08-06-lua-client-m4.md`），待逐 task 执行

## Plan 路线图（spec §附 milestone）

| Plan | Milestone | 状态 | plan 文件 |
|---|---|---|---|
| 1 | M1-M3 MVP（VM 沙箱 / IO 引擎 / ctx L1+L6 / TCP / 端到端 Modbus） | ✅ done @ master | `2026-08-06-lua-client-mvp.md` |
| 2 | M4 联动与持久（L3 `send_command` + L4 `kv` + I2 TCP 重连） | 🔵 plan 就绪，执行中 | `2026-08-06-lua-client-m4.md` |
| 3 | M5 HTTP（L5 `http_get`/`http_post` + 全审计，feature `lua-http`） | ⏭ 待写 plan | — |
| 4 | M6 热加载（notify + debouncer + 重建 VM + 失败回退）+ `on_data` 被动帧推送 | ⏭ 待写 plan | — |
| 5 | M7 状态监测（`stat:<inst>` 指标）+ REST（`routes_lua` stats/reload） | ⏭ 待写 plan | — |
| 6 | M8 serial/udp 传输 + lua-script-guide.md + examples + 沙箱测补全（debug/package） | ⏭ 待写 plan | — |

## Plan 2（M4）执行进度

- [ ] Task 1：`luakv.rs` 持久化模块 + identity 清单 + 实例删除清理接线
- [ ] Task 2：`ctx:kv_set` / `ctx:kv_get`（L4 API）
- [ ] Task 3：`ctx:send_command`（L3 边缘联动）
- [ ] Task 4：`TcpIo` 断连重连（I2，`Option<TcpStream>` 懒连接）
- [ ] Task 5：M4 集成测（kv 跨 VM 持久 + send_command 双实例 + delete 清 kv）

## 下一步（每轮 cron 触发执行）

执行 **Plan 2 Task 1**（`luakv.rs`）：superpowers:subagent-driven-development，fresh implementer + 两阶段 review，TDD 红→绿→重构，每 task 一提交。然后 Task 2→3→4→5。Plan 2 全绿 → 写 Plan 3（M5 HTTP）plan。

## CI 命令（Windows，规避 link.exe 并行 OOM）

```bash
CARGO_BUILD_JOBS=1 cargo test --all-targets --all-features
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
```

## 阻塞点

（无）
