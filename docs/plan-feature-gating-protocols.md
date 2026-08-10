# 方案 B：内置协议 Cargo feature 裁剪 —— 评估与风险控制

> **状态**：✅ 已实施（README 里程碑 M11，`--no-default-features --features "lua,modbus"` 等组合可编译 + 优雅降级，CI minimal job 守护）
> **日期**：2026-08-07（评估）/ 2026-08-08（实施落地）
> **关联**：`docs/plugin-extension-strategy.md`（上层策略，本方案是其第 6/7 节"现场裁剪"的具体落地）
> **目标**：把 `opcua_client` / `modbus_client` / `s7_client` 三个南向协议插件改为可选 Cargo feature，实现"现场按需编译 + 系统依赖可选 + 二进制体积裁剪"，**零运行时风险、零行为破坏**。

## 1. 现状分析

### 1.1 引用面：极干净（feature 化的低风险根基）

三个协议 crate **只在各自 `src/plugin/builtin/{opcua_client,modbus_client,s7_client}/` 子目录内部使用**。主程序核心代码（`lib.rs` / `config.rs` / `api/` / `host.rs` / `instance.rs` / `registry.rs`）**零直接引用**。

证据（`rg` 全仓扫描）：
- `async_opcua` / `opcua_client` → 仅 `builtin/opcua_client/mod.rs`
- `tokio_modbus` → 仅 `builtin/modbus_client/{transport,mod}.rs`
- `tokio_s7` → 仅 `builtin/s7_client/mod.rs`

这印证了 rs-iot 分层架构的收益：南向协议完全封装在 builtin，主程序只通过 `Source` trait + `app_code` 字符串交互。**feature 化只需在边界处（模块声明 + Cargo.toml）动刀**，不碰任何核心逻辑。

注册查找是运行时按字符串（`registry.rs:29` `find_source`），找不到返回 `Config` 错误（`instance.rs:78-80`）—— **关掉某协议后，用户配置该 `app_code` 会得到清晰的 "source plugin not found" 错误，是优雅降级，不是崩溃**。

### 1.2 关键耦合点：`tokio-serial` 被 modbus 与 lua 共用

`Cargo.toml:68` 的 `tokio-serial = "5.5"` 是**无条件依赖**，被两处使用：
- `builtin/modbus_client/transport.rs:13`（modbus RTU serial）
- `builtin/lua_client/io_engine/serial.rs:5`（lua 串口 IO，**lua 默认开启**）

**含义**：`tokio-serial` 不能简单跟随 `modbus` feature gate——因为 lua 默认开启也依赖它。`tokio-serial` Linux 链接 `libudev`，CI 已在 clippy/test/doc 三处安装（`ci.yml:38-41,57-59,71-73`）。

### 1.3 测试依赖面

| 测试文件 | 是否受 feature 化影响 | 说明 |
|---|---|---|
| `tests/s7_e2e.rs` | **是（必须 gate）** | 直接 `use tokio_s7::...`（`:30-37`）+ `app_code: "s7Client"`（`:495`）。关 `s7` feature 时编译失败。 |
| `tests/lua_modbus_e2e.rs` | **否** | `app_code: "luaClient"` + 自带 mock modbus server（`serve_modbus`），不依赖 `tokio-modbus` crate 也不依赖 `modbus_client` 插件。 |
| `tests/dataflow.rs` 等 | 否 | 用 `dataSim` / `sidecarClient` / `luaClient`，不碰三协议。 |

**唯一必须处理的是 `tests/s7_e2e.rs`**。

### 1.4 CI 现状与验证缺口

`ci.yml`：
- `clippy` job（`:42`）：`cargo clippy --all-targets` —— **用 default features**
- `test` job（`:60`）：`cargo test --all-targets --all-features` —— 用全 features
- **缺口**：没有任何 job 验证"关闭某 feature 也能编译/测试"。feature 化后，若不补这个验证，裁剪组合可能在某次改动后悄悄编译失败却无人发现。

## 2. 方案设计

### 2.1 默认值策略：默认开启（向后兼容）

| 选项 | 行为 | 评价 |
|---|---|---|
| **默认开启**（推荐） | `default = [..., "opcua", "modbus", "s7"]`，现有用户升级零感知 | ✅ 向后兼容；CI clippy（default）仍覆盖三协议代码，无质量缺口 |
| 默认关闭 | 现有用户升级后协议消失 | ❌ 破坏性；clippy 不再检查三协议代码 → 质量回归 |

**决策：默认开启**。现场裁剪通过 `--no-default-features --features "..."` 显式选择。这样既保持现状行为，又给现场裁剪开了口子。

### 2.2 `tokio-serial` 处理：B1 保守起步

| 子方案 | 做法 | 评价 |
|---|---|---|
| **B1 保守（推荐）** | `tokio-serial` 保持**无条件依赖**；只 gate `tokio-modbus` / `async-opcua` / `tokio-s7` | ✅ 改动最小，不动 `lua` feature；因 lua 默认开，tokio-serial 反正要编译。省它的边际收益小。 |
| B2 彻底 | `tokio-serial` 改 `optional`，由 `lua` 与 `modbus` 共享引入 | 改动 `lua` feature 定义；边际收益（关 lua+modbus 时省 libudev）非起步必要。 |

**决策：B1 起步**。未来若 `lua` 也想可关，再升级到 B2。

### 2.3 feature 命名

沿用现有风格（`transform` / `lua` / `lua-http` 均为协议/能力名）：
- `opcua` → `dep:async-opcua`
- `modbus` → `dep:tokio-modbus`（注：`tokio-serial` 保持无条件，不进 feature）
- `s7` → `dep:tokio-s7`

## 3. 风险识别与控制

| # | 风险 | 等级 | 控制措施 | 验证 |
|---|---|---|---|---|
| R1 | 关闭协议后，用户配置该 `app_code` 报错 | 低（设计如此） | `find_source` 返回 None → `Config` 错误（`instance.rs:78-80`），REST 返回 400/404 | 手动：关 modbus，建 `app_code="modbusClient"` 实例，确认错误信息清晰 |
| R2 | `tests/s7_e2e.rs` 关 s7 时编译失败 | 中 | 文件级 `#![cfg(feature = "s7")]` | `cargo build --no-default-features --features lua`（不含 s7）须编译通过 |
| R3 | clippy（default features）覆盖不到被默认关的协议 | —— | 已由"默认开启"策略规避 | default features 下 `cargo clippy` 仍检查三协议 |
| R4 | 裁剪组合编译失败无人发现（CI 缺口） | 中 | CI 新增 `minimal` job：`cargo build --no-default-features --features lua` | CI 矩阵见 §5 |
| R5 | API 层硬编码协议名白名单导致 gate 后异常 | 低（架构未用白名单） | 实施时 `rg "opcuaClient\|modbusClient\|s7Client"` 在 `src/api/` 确认仅文档/示例 | grep 验证 |
| R6 | `tokio-serial` 误 gate 导致 lua serial 编译失败 | 中 | B1 不动 `tokio-serial`，保持无条件 | lua 的 5 个 e2e 测试全绿 |
| R7 | 文档/config 示例未说明 feature 依赖 | 低 | README + `docs/lua-script-guide.md` 补 feature 说明（可选） | 人工 review |
| R8 | `--all-features` 语义变化 | 无 | `test` job 仍 `--all-features`，三协议全开，行为不变 | CI test 全绿 |

**整体风险评级：低**。核心得益于引用面干净 + 默认开启策略 + find_source 优雅降级。

## 4. 实现清单

### 4.1 `Cargo.toml`（约 +10 行）

```toml
# 三协议改 optional
async-opcua = { version = "0.19", default-features = false, features = ["client"], optional = true }
tokio-modbus = { version = "0.17", default-features = false, features = ["tcp", "rtu"], optional = true }
# tokio-serial 保持无条件（B1，lua 默认依赖）
tokio-s7 = { version = "0.1", optional = true }

[features]
default = ["transform", "lua", "lua-http", "opcua", "modbus", "s7"]  # 新增三项
# ...
opcua = ["dep:async-opcua"]
modbus = ["dep:tokio-modbus"]
s7 = ["dep:tokio-s7"]
```

### 4.2 `src/plugin/builtin/mod.rs`（+3 行，照搬 lua 范本）

```rust
pub mod data_sim;
#[cfg(feature = "lua")]
pub mod lua_client;
#[cfg(feature = "modbus")]
pub mod modbus_client;
pub mod mqtt_pub;
#[cfg(feature = "opcua")]
pub mod opcua_client;
#[cfg(feature = "s7")]
pub mod s7_client;
pub mod sidecar_client;
```

### 4.3 `tests/s7_e2e.rs`（+1 行，文件头）

```rust
#![cfg(feature = "s7")]
```

### 4.4 `.github/workflows/ci.yml`（新增 minimal 编译检查 job，约 +20 行）

```yaml
  minimal:
    # 验证裁剪组合可编译：关掉三协议，只留 lua（覆盖最小现场部署）
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/checkout@v4
        with:
          repository: viccom/lux
          path: ../lux
      - uses: dtolnay/rust-toolchain@stable
      - name: Install libudev-dev (Linux)
        if: runner.os == 'Linux'
        run: sudo apt-get update && sudo apt-get install -y libudev-dev
      - run: cargo build --no-default-features --features lua
      - run: cargo build --no-default-features --features "lua,modbus,opcua"
```

## 5. 验证策略

实施后须全绿（与项目"验证规则"对齐）：

| 验证项 | 命令 | 预期 |
|---|---|---|
| 格式 | `cargo fmt --all --check` | 通过 |
| default clippy | `cargo clippy --all-targets -- -D warnings` | 零警告（三协议仍被检查） |
| default 测试 | `cargo test --all-targets` | 全绿 |
| 全 feature 测试 | `cargo test --all-targets --all-features` | 全绿（含 s7_e2e） |
| **裁剪编译（最小现场）** | `cargo build --no-default-features --features lua` | 编译通过（s7_e2e 被 cfg 跳过） |
| **裁剪编译（modbus+opcua）** | `cargo build --no-default-features --features "lua,modbus,opcua"` | 编译通过 |
| 优雅降级 | 关 modbus，建 `app_code="modbusClient"` 实例 | 返回清晰 "source plugin not found" |

## 6. 工作量与交付

- **代码改动**：约 15 行（Cargo.toml + builtin/mod.rs + tests/s7_e2e.rs）
- **CI 改动**：约 20 行（新增 minimal job）
- **预估工时**：0.5–1 天（含验证）
- **风险**：低（引用面干净 + 默认开启 + 优雅降级三重保障）

## 7. 与上层策略的关系

本方案是 `docs/plugin-extension-strategy.md` 第 6/7 节"现场裁剪"的具体落地，**不依赖** SDK crate 抽取或 sidecar 演进，可独立先行。其收益（系统依赖可选、二进制裁剪、编译加速）是动态库方案收益子集的**零风险版本**。
