# Lua 脚本采集 spec 实现进度（/loop 驱动）

> **运行时不变量**：CronCreate job `116fd1e9`（每 10m，session-only，7 天自动过期）。**轮内禁止 ScheduleWakeup**（cadence 全交 cron）；spec 全完成或阻塞 → `CronDelete 116fd1e9` 收尾。每轮 cron 触发时读本文件 + plan checkbox + `git log --oneline -15` 重建上下文。

## 当前状态

- ✅ Plan 1 / M1-M3 MVP（最小采集闭环）— 合并 master @ `4423da6`（99 测试绿）
- 🔵 Plan 2 / M4（联动与持久 + I2 重连）— 分支 `feature/lua-m4`，**Task 1+2 ✅**，执行中 Task 3-5

## Plan 路线图（spec §附 milestone）

| Plan | Milestone | 状态 | plan 文件 |
|---|---|---|---|
| 1 | M1-M3 MVP（VM 沙箱 / IO 引擎 / ctx L1+L6 / TCP / 端到端 Modbus） | ✅ done @ master | `2026-08-06-lua-client-mvp.md` |
| 2 | M4 联动与持久（L3 `send_command` + L4 `kv` + I2 TCP 重连） | 🔵 `feature/lua-m4`，Task 1+2 ✅ 执行中 | `2026-08-06-lua-client-m4.md` |
| 3 | M5 HTTP（L5 `http_get`/`http_post` + 全审计，feature `lua-http`） | ⏭ 待写 plan | — |
| 4 | M6 热加载（notify + debouncer + 重建 VM + 失败回退）+ `on_data` 被动帧 | ⏭ 待写 plan | — |
| 5 | M7 状态监测（`stat:<inst>`）+ REST（`routes_lua`） | ⏭ 待写 plan | — |
| 6 | M8 serial/udp 传输 + 文档 + 示例 + 沙箱测补全 | ⏭ 待写 | — |

## Plan 2（M4）执行进度（分支 `feature/lua-m4`）

- [x] Task 1：`luakv.rs` 持久化模块 + identity 清单 + 实例删除清理 ✅（`0ca0255`；spec+代码双 review 过）
- [x] Task 2：`ctx:kv_set` / `ctx:kv_get`（L4 API）✅（`a042781`；implementer agent 因 API 503 在收尾确认步失败，但实现+测试+提交均先完成；控制器自验 CI 三项全绿 + 自审 spec/代码——独立 reviewer 因 API 不稳未 dispatch）
- [ ] Task 3：`ctx:send_command`（L3 边缘联动）← 🔵 下一个
- [ ] Task 4：`TcpIo` 断连重连（I2，`Option<TcpStream>` 懒连接）
- [ ] Task 5：M4 集成测（kv 跨 VM 持久 + send_command 双实例 + delete 清 kv）

## 下一步（每轮 cron 触发执行）

执行 **Plan 2 Task 3**（`ctx:send_command`，改 `api.rs`，经 `CtlRegistry::send`）：subagent-driven（若 API 恢复）或控制器接手（若 API 仍不稳，Task 3 机械且 plan 含完整代码）。Plan 2 全 5 task 完成 → `finishing-a-development-branch` 合并 `feature/lua-m4` → master → 写 Plan 3（M5 HTTP）plan。

## 后续清理项（跨模块，非阻断，代码 review 标记）

- `luakv::get` 空 JSON 值（如 `""`）误判为 missing（与 `ext_token::get` 同根）— 区分 `EmbeddedValue::Nil` vs 空 Bulk/Simple，日后跨模块统一修
- `RsIotError::Config` 用于 key/value 运行时校验语义勉强 — 待项目新增 `Validation` 变体后与 ext_token 一并迁移
- in-crate 单测 data_dir 按 pid 共享（非按 test name 隔离）— 与 `tests/common` 约定偏离但与 `api.rs` 既有单测一致；lux Tiered 锁目录致并发失败时跨模块统一改

## CI 命令（Windows，规避 link.exe 并行 OOM）

```bash
CARGO_BUILD_JOBS=1 cargo test --all-targets --all-features
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
```

## 阻塞点

- ⚠️ **API 503 间歇故障**（combo models / zai-b cooldown）：致 Task 2 implementer 收尾步失败。dispatch subagent 可能再失败；机械 task 控制器可接手（cargo 不依赖 LLM API）。监控：若连续 3 次 dispatch 失败 → 暂停 loop（CronDelete）上报用户。
