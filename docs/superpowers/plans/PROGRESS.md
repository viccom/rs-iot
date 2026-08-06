# Lua 脚本采集 spec 实现进度（/loop 驱动）

> **运行时不变量**：CronCreate job `116fd1e9`（每 10m，session-only，7 天自动过期）。**轮内禁止 ScheduleWakeup**；完成/阻塞 → `CronDelete 116fd1e9`。每轮 cron 触发时读本文件 + plan checkbox + `git log --oneline -15` 重建上下文。

## 当前状态

- ✅ Plan 1（M1-M3）+ Plan 2（M4）+ Plan 3（M5 HTTP）+ Plan 4（on_data M2 欠账）— 合并 master @ `797545d`（**110 测试全绿**）
- 🔵 下一步：写 Plan 5（M6 热加载）plan

## Plan 路线图（spec milestone + 欠账补全）

| Plan | 内容 | 状态 | plan 文件 |
|---|---|---|---|
| 1 | M1-M3 MVP | ✅ done @ master | `2026-08-06-lua-client-mvp.md` |
| 2 | M4（L3 send_command + L4 kv + I2 重连） | ✅ done @ master（`136e9a9`） | `2026-08-06-lua-client-m4.md` |
| 3 | M5 HTTP（L5 http_get/post + 审计） | ✅ done @ master（`6ff4ff0`） | `2026-08-07-lua-client-m5.md` |
| 4 | on_data 被动帧（M2 欠账） | ✅ done @ master（`797545d`，2026-08-07 合并） | `2026-08-07-lua-client-on-data.md` |
| 5 | M6 热加载（notify + VM 重建 + dry-run + 失败回退） | 🔵 待写 plan | — |
| 6 | M7 状态监测（`stat:<inst>`）+ REST（`routes_lua`） | ⏭ 待写 plan | — |
| 7 | M8 serial/udp + L2 数据 API（set_value/get_value/...）+ 文档 + 沙箱测补全 | ⏭ 待写 plan | — |

## Plan 4（on_data）完成总结 ✅（合并 master @ `797545d`）

- [x] Task 1：io_engine on_data 改造（`c021271`；IoBackend `recv_idle` + spawn_io_task select! + on_data_tx；**修复忙循环**：默认 recv_idle 立即 None 致 CPU 100%，改 sleep 50ms）
- [x] Task 2：mod.rs 主循环 data_rx 分支 + call_on_data（`876de55`）
- [x] Task 3：on_data 集成测（`797545d`；mock server 推帧 → on_data kv_set 端到端，绕过未实现的 set_value）

110 测试全绿（lib 92 + 集成测 18）。spec §8/§6 on_data 覆盖（IO recv_idle + 主循环 Evt::Data + 脚本回调）。

## 下一步（每轮 cron 触发执行）

**写 Plan 5（M6 热加载）plan**：superpowers:writing-plans，spec §5.3。
- `notify::RecommendedWatcher` + `notify-debouncer-mini` 去抖 200ms → 收 `<name>/` 改动 → Source task 内编译新 VM（沙箱+hook）→ dry-run（on_init + 一次 collect 严格 hook）→ 成功切换（旧 VM CancellationToken 退出）/ 失败 warn 回退（旧 VM 继续跑，永不半生效）。
- Cargo.toml lua feature 加 `dep:notify,dep:notify-debouncer-mini`（spec §16）。
- spec §18 风险：notify Windows 偶有事件丢失 → 去抖 + REST 手动 reload（M7）兜底。

文件名 `2026-08-07-lua-client-m6.md`。

## 已知简化（Plan 4，注明，后续）

- 混合协议（collect + on_data）下 request cancel recv_idle 可能丢部分广播片段 —— 纯被动 period=0 不受影响。
- `set_value`（L2）未实现，Plan 4 集成测用 `kv_set` 绕过（L2 整组 set_value/get_value/get_device/list_devices 留 Plan 7）。

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
