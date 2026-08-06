# Lua 脚本采集 spec 实现进度（/loop 驱动）

> **运行时不变量**：CronCreate job `116fd1e9`（每 10m，session-only，7 天自动过期）。**轮内禁止 ScheduleWakeup**（cadence 全交 cron）；spec 全完成或阻塞 → `CronDelete 116fd1e9` 收尾。每轮 cron 触发时读本文件 + plan checkbox + `git log --oneline -15` 重建上下文。

## 当前状态

- ✅ Plan 1 / M1-M3 MVP — 合并 master @ `4423da6`（99 测试绿）
- 🔵 Plan 2 / M4 — 分支 `feature/lua-m4`，**Task 1+2+3 ✅**（L4 kv + L3 send_command 完成），执行中 Task 4-5

## Plan 路线图（spec §附 milestone）

| Plan | Milestone | 状态 | plan 文件 |
|---|---|---|---|
| 1 | M1-M3 MVP | ✅ done @ master | `2026-08-06-lua-client-mvp.md` |
| 2 | M4（L3 send_command + L4 kv + I2 TCP 重连） | 🔵 `feature/lua-m4`，Task 1+2+3 ✅ | `2026-08-06-lua-client-m4.md` |
| 3 | M5 HTTP（L5 http_get/post + 审计） | ⏭ 待写 plan | — |
| 4 | M6 热加载 + on_data 被动帧 | ⏭ 待写 plan | — |
| 5 | M7 状态监测 + REST | ⏭ 待写 plan | — |
| 6 | M8 serial/udp + 文档 + 沙箱测补全 | ⏭ 待写 | — |

## Plan 2（M4）执行进度（分支 `feature/lua-m4`）

- [x] Task 1：`luakv.rs` + identity 清单 + 实例删除清理 ✅（`0ca0255`；spec+代码双 review 过）
- [x] Task 2：`ctx:kv_set/kv_get`（L4 API）✅（`a042781`；implementer agent API 503 收尾失败但已完整提交，控制器自验 CI 全绿 + 自审）
- [x] Task 3：`ctx:send_command`（L3 联动）✅（`9b33e34`；控制器接手 API 不稳，CI 三项全绿 + 自审）
- [ ] Task 4：`TcpIo` 断连重连（I2，`Option<TcpStream>` 懒连接）← 🔵 下一个
- [ ] Task 5：M4 集成测（kv 跨 VM 持久 + send_command 双实例 + delete 清 kv）

## 下一步（每轮 cron 触发执行）

执行 **Plan 2 Task 4**（`TcpIo` 断连重连，改 `io_engine.rs` + `mod.rs`）：spec §5.4 line 157 + §8 line 282/288，对标 modbus `Option<Session>` 但下沉 IO task。控制器接手（API 不稳；cargo 路径已证稳）或 subagent（若 API 恢复）。Task 4+5 完成 → `finishing-a-development-branch` 合并 `feature/lua-m4` → master → 写 Plan 3（M5 HTTP）plan。

## 接手模式说明（API 503 期间）

API 503 间歇故障（combo models / zai-b cooldown）致 subagent dispatch 不稳。Task 2 起控制器接手机械 task（plan 含完整代码 + 控制器已读相关文件 + cargo 不依赖 LLM API）：实现→自跑 CI 三项→自审（spec 合规 + 代码质量）→提交。独立 reviewer 因 API 不稳暂跳过，自审结论标注于各 task 行。API 恢复后恢复 subagent-driven 双 review。

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
