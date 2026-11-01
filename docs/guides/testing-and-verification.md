# 测试与验证手册（分域验证规则 / 端到端清单）

> **状态**：活文档（随 feature 面与测试结构演进；改动后同步 `just check` 与 `.github/workflows/ci.yml`）
>
> 本文是 `CLAUDE.md`「验证规则」节的完整版。**日常只需 `just check`**；本文用于按改动面选择精准验证集，
> 以及需要真机/外网依赖时的端到端步骤。

## 〇、一键门禁（首选）

```bash
just check          # fmt + docs + layers + clippy 三档 + 单测/e2e（本机实测基线：全绿）
just check-s7-eip   # s7/eip 裁剪档 e2e（出默认 feature，需显式档）
```

分步：

```bash
just fmt                     # cargo fmt --all --check
just docs                    # python3 scripts/check_docs.py（文档宪法）
just layers                  # python3 scripts/check_layers.py（模块图无环 + 禁止越层引用）
just lint                    # clippy 默认档 + lua-only 档（均 -- -D warnings）
just test                    # cargo test --lib + cargo test --tests --no-default-features --features lua
```

> **不要用 `--all-features` 作为日常门禁**：`history` feature 依赖 `../tsink_fork` 真实仓库，
> 本机若为占位 stub 则不可编译（`just lint` 会自动跳过并打印原因）。
> 需要全开时显式确认 tsink 可用：`cargo test --all-targets --all-features`。

## 一、按改动面选验证集

### 通用

改 `src/` 任意代码：`just check`（= fmt + clippy 三档 + 单测 + 裁剪档 e2e）。

### 插件 / 管道 / 类型

- 对应单测：`cargo test <module>`（如 `cargo test hwid`、`cargo test pipeline::route`）
- **改 pipeline/dispatcher/worker/write_batch**：额外跑 `cargo test --test throughput_e2e`
  + `cargo run --release --example bench_pipeline_e2e`（吞吐基线对照）
- **改 rtdata 写路径**：`cargo test --lib db::rtdata`（含 per-devid 容错与旁路过滤语义）

### 历史存储（feature `history`，需真实 tsink）

| 改动面 | 验证命令 |
|---|---|
| 写路径 / pipeline 出口（Phase 1b） | `cargo test -j 2 --features history --test history_dataflow_e2e --test history_write_e2e --test history_kill_recovery_e2e --test history_visibility_e2e` |
| 读路径 / 订阅读源（Phase 2） | `cargo test -j 2 --features history --test history_read_e2e` + `--test rtdata_subscribe_e2e --test rtdata_watch_e2e`（双 impl 等价守门） |
| 治理 / init 路径（Phase 3） | `cargo test -j 2 --features history --test history_visibility_e2e --test history_init_failed_e2e --test history_init_stack_e2e` + 前端 `npm run test -- --run historyQuery` |

bench 守门（主链路劣化 <5%）：
```bash
RSIOT_BENCH_HISTORY=off|on cargo run --release --features history --example bench_pipeline_e2e
RSIOT_BENCH_HISTORY=on RSIOT_BENCH_SUB=poll|ksub cargo run --release --features history --example bench_pipeline_e2e
```
门禁数据见 `designs/2026-08-25-history-{write-path,read-subscribe}-plan.md §三`。

### 协议客户端端到端（真机/外网依赖，CI 不跑）

| 协议 | 前置 | 步骤 |
|---|---|---|
| `opcuaClient` | 本机 UA Server `opc.tcp://127.0.0.1:49320` | REST 建实例 → `/rtdatas` 见点 → 写命令闭环 |
| `modbusClient` | TCP / RTU serial / RTU over TCP-UDP 设备 | 同上；serial 需 `libudev-dev`（Linux） |
| `s7Client` | 西门子 S7（CI 内 `tests/s7_e2e.rs` 有 mock server 覆盖协议链路） | REST 建实例 → 采点 |
| `eipClient` | 罗克韦尔 AB PLC（**需 `--features eip`**，已出默认） | REST 建实例 → 采点；位寻址写 tag 路径 `.N` |
| `sidecarClient` | 无（CI 内 `tests/sidecar_e2e.rs` 进程内模拟外部客户端） | bridge 握手/配置推送/数据上行/写命令闭环；改 bridge 协议跑 `cargo test --test sidecar_e2e` |

### REST 端点

| 端点 | 验证要点 |
|---|---|
| `POST /api/v1/command/send` | body `{instance,devid,cmd:'write',data:'{"tag":..,"value":..}'}`；实例未跑 → 404 |
| `GET /metrics`（顶层，免 JWT） | Prometheus 文本；进程级 + per-instance + history family |
| `POST/GET/DELETE /api/v1/inst/trace` | per-instance 报文 hex dump 按需激活（实例须 Running） |
| `GET /api/v1/logs/recent` | ring buffer 快照（内存 2000 条/实例；另可走 SSE `log` 事件） |
| `GET /api/v1/history` | raw + 六聚合；503/400 矩阵 |

### 数据流端到端（改数据流/API 后必做）

```bash
./target/debug/rs-iot --config config.toml &
redis-cli -p 6390 PING                       # → PONG
redis-cli -p 6390 HGETALL rtdata:devA        # 采集→管道→入库
mosquitto_sub -h broker.emqx.io -t '+/devA/datas' -v   # 北向发布
# REST：login 拿 token → 带 Authorization 调 /sysinfo、/rtdatas 等
```

### 前端（改 `frontend/` 后）

```bash
cd frontend && npm ci && npm run test -- --run && npm run build
```
**rust-embed 重建陷阱**：`crates/rsiot-gateway/src/static_assets.rs` 编译期嵌入 `frontend/dist/`，
仅 `npm run build` 不会更新二进制——必须重编 gateway；且 rust-embed 不感知 dist 内容变化，
必要时 `touch crates/rsiot-gateway/src/static_assets.rs` 再编译。

## 二、集成测试索引（`tests/`，45 个，2026-10-31 时点）

命名 `*_e2e.rs` 为端到端，无后缀三篇为早期 M0 遗留（`dataflow` / `api_flow` / `config_load`）。
`tests/common/mod.rs` 提供 `start_lux`（端口 0 + 独立临时 data_dir）/ `test_data_dir` /
`register_devices_as_models`，避免测试间端口与数据冲突。

## 三、已知失败与豁免

- Windows Service `sc stop` 退出码 1066（ERROR_EXCEPTION_IN_SERVICE）是已知平台限制，日志可见完整关闭序列即视为通过。
- `lua_north_e2e::north_lifecycle_running_stats_stop` 需本地 MQTT broker（`127.0.0.1:1883`）；
  无 broker 时该用例显式 SKIP 并打印原因（`degraded` 是正确行为，不是缺陷）。
- 本机真机 e2e 环境说明见仓库根 `FIELD_E2E_LOCAL.md`（已 gitignore，不入库）。
