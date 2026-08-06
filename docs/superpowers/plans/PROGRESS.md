# Lua 脚本采集 spec 实现进度（/loop 驱动）

> **运行时不变量**：CronCreate job `116fd1e9`（每 10m，session-only，7 天自动过期）。**轮内禁止 ScheduleWakeup**；完成/阻塞 → `CronDelete 116fd1e9`。每轮 cron 触发时读本文件 + plan checkbox + `git log --oneline -15` 重建上下文。

## 当前状态

- ✅ Plan 1（M1-M3）+ Plan 2（M4）+ Plan 3（M5 HTTP）— 合并 master @ `6ff4ff0`（108 测试绿）
- 🔵 Plan 4（on_data 被动帧，M2 欠账补全）— 分支 `feature/lua-on-data`，**Task 1 ✅**（`c021271`），执行中 Task 2-3

## Plan 路线图（spec milestone + 欠账补全）

| Plan | 内容 | 状态 | plan 文件 |
|---|---|---|---|
| 1 | M1-M3 MVP | ✅ done @ master | `2026-08-06-lua-client-mvp.md` |
| 2 | M4（L3 send_command + L4 kv + I2 重连） | ✅ done @ master（`136e9a9`） | `2026-08-06-lua-client-m4.md` |
| 3 | M5 HTTP（L5 http_get/post + 审计） | ✅ done @ master（`6ff4ff0`） | `2026-08-07-lua-client-m5.md` |
| 4 | on_data 被动帧（M2 欠账） | 🔵 `feature/lua-on-data`，Task 1 ✅ 执行中 | `2026-08-07-lua-client-on-data.md` |
| 5 | M6 热加载（notify + VM 重建 + dry-run + 失败回退） | ⏭ 待写 plan | — |
| 6 | M7 状态监测（`stat:<inst>`）+ REST（`routes_lua`） | ⏭ 待写 plan | — |
| 7 | M8 serial/udp + L2 数据 API + 文档 + 沙箱测补全 | ⏭ 待写 plan | — |

## Plan 4（on_data）执行进度（分支 `feature/lua-on-data`）

- [x] Task 1：io_engine on_data 改造 ✅（`c021271`；IoBackend `recv_idle`（默认 sleep 50ms 避忙循环，TcpIo 覆盖主动 read）+ spawn_io_task 加 on_data_tx + IO task select!{ biased; req / recv_idle } + on_data 单测；mod.rs 建 data channel；api.rs 4 处测试改 spawn_mock_io helper。**修复设计缺陷**：默认 recv_idle 立即 None 致 select! 忙循环 CPU 100% → sleep 50ms。lua_client 24 测试绿）
- [ ] Task 2：mod.rs 主循环 data_rx 分支 + `call_on_data`（`_data_rx` → `mut data_rx`）← 🔵 下一个
- [ ] Task 3：on_data 集成测（mock server 推帧 → on_data kv_set 端到端）

## 下一步（每轮 cron 触发执行）

执行 **Plan 4 Task 2**（mod.rs 主循环）：3 Edit（`_data_rx`→`mut data_rx` / 主循环 select! 加 `Some(frame)=data_rx.recv()` 分支 / 加 `call_on_data` helper）。控制器接手（plan 含完整代码）。Task 2 绿 → Task 3 集成测 → 合并 → 写 Plan 5（M6 热加载）plan。

## 已知简化（Plan 4，注明）

- 混合协议（collect + on_data）下 request cancel recv_idle 可能丢部分广播片段 —— 纯被动 period=0 不受影响；v1 接受。
- `set_value`（L2）未实现，Task 3 用 `kv_set` 绕过测 on_data 链路。

## 接手模式说明（API 503 期间）

API 503 间歇故障致 subagent dispatch 不稳。Plan 2 起控制器接手机械 task（plan 含完整代码 + cargo 不依赖 LLM API）。独立 reviewer 暂跳过。API 恢复后恢复 subagent-driven 双 review。

## 后续清理项（跨模块，非阻断）

- `luakv::get` 空 JSON 值误判 missing（与 `ext_token::get` 同根）
- `RsIotError::Config` 用于运行时校验语义勉强（待 `Validation` 变体）
- in-crate 单测 data_dir 按 pid 共享
- M5 http 简化：reqwest Client per-call / bytes() 后 check 上限 / `http_timeout_secs` config 未接

## CI 命令（Windows，规避 link.exe 并行 OOM）

```bash
CARGO_BUILD_JOBS=1 cargo test --all-targets --all-features
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
```

## 阻塞点

- ⚠️ API 503 间歇故障 → subagent dispatch 不稳，控制器接手中。无逻辑阻塞。
