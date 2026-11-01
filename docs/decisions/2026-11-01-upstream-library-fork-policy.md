# 上游库缺陷处置策略（插件规避 / 最小 fork / 上游 PR）

> **状态**：accepted（策略已决；tokio-s7 fork 补丁已备好并本地验证，远程仓与 `[patch]` 接入待执行，见 §9）
> **日期**：2026-11-01
> **来源**：协议插件协议语义审查（见 [台账](../designs/2026-11-01-protocol-semantics-audit.md)）中 21 条上游库条目的处置裁决
> **关联**：`Cargo.toml` 依赖段（`lux`/`tsink_fork` path 依赖先例、`tokio-s7`/`rust-ethernet-ip` 版本约束）、`.github/workflows/ci.yml` 的私有 path 依赖 checkout 先例

## 1. 背景与约束

- 四家协议库共 21 条按"上游库"定性的问题（Modbus 1、S7 8、EtherNet/IP 12；OPC UA 无），其中真正**必须改库内部**的只有少数（见 §3）；其余可在插件侧规避开。
- rs-iot 是多 crate workspace（`[workspace] members = [".", "crates/rsiot-mcp", "crates/rsiot-gateway"]`），CI 有 `--all-features` 构建档（clippy/test 均含 s7/eip），任何依赖形态变化必须同时适配本地与 CI。
- 工作区近期先例是"同级独立仓 + path 依赖"（`lux = { path = "../lux" }`、`tsink = { path = "../tsink_fork" }`，CI 用 `actions/checkout` 把私有仓拉到同级目录）。该先例成立的前提是这些仓**本来就由本团队维护**；对第三方 crate 的临时补丁，直接照搬会把"上游 fork"与"长期维护的分支"混为一谈。
- 四家库许可：tokio-modbus `MIT OR Apache-2.0`、async-opcua `MPL-2.0`、tokio-s7 `Apache-2.0`、rust-ethernet-ip `MIT`。MPL-2.0 是文件级 copyleft，fork 修改会被要求以 MPL 发布修改文件——这是"不 fork async-opcua"的额外理由。
- 上游活跃度（2026-11-01 核实）：tokio-s7 仓库最近提交 2026-07-23、crates.io 停在 0.1.7；rust-ethernet-ip 最近提交 2026-09-09、已发 1.2.1（但其 `rust-ethernet-ip-protocol` 1.2.1 的 `values.rs` 相对 1.2.0 仅文档注释变化，解码层未修）；tokio-modbus 最近提交 2026-07-03；async-opcua 0.19.0 为当前版本。

## 2. 决策：三层策略，禁止 vendoring

| 层 | 做法 | 判据 |
|---|---|---|
| 1 | **插件侧规避**（代码留在 rs-iot） | 库通过公开 API/字段暴露了足够信息（如 `tokio-s7` 的 `pdu_length` 是 `pub` 字段），或缺陷在插件用法层面（如未判 `BatchError::NetworkError`） |
| 2 | **最小 fork**（独立仓 + 3~4 处补丁） | 缺陷在库内部解码/应答处理，插件无法经公开 API 获得正确信息，且后果为 P0/P1 |
| 3 | **上游 PR**（与 2 并行，不是替代） | 所有 fork 补丁同时提上游；合入后删本地补丁、切回 crates.io |

**处置代码**（台账"处置"列使用）：`插件` = 插件侧修；`P` = 插件侧规避库缺陷；`F` = fork 上游打补丁；`U` = 提上游 PR；`N` = 记录不修；`P+U` = 先规避、同时推动上游；`框架` = 框架侧修。

## 3. 逐库裁决

| 库 | 版本 | 必须改库内部的条目 | 裁决 | 载体 |
|---|---|---|---|---|
| tokio-s7 | 0.1.7 | S-1（写应答 `return_items` 被丢弃）、S-4（头级错误解析错位）、S-5（STRING UTF-8 lossy）、S-9（`connection_type` 死配置） | **fork**（4 补丁，预计 30~50 行 diff） | `viccom/tokio-s7`（git.metme.top）分支 `rs-iot/fixes`；`Cargo.toml` 加 `[patch.crates-io]` |
| rust-ethernet-ip | 1.2.0（1.2.1 已发，解码层未变） | E-2（0xD3 兼作 DWORD 与 BOOL 数组标记）、E-3（WORD/LWORD/BYTE 类型码无解码分支）；E-16（含 `:` 的 I/O tag 编码） | **不 fork**：先插件规避（配置期拒绝 + 大值单点读）+ 提上游 PR；仅当现场确需 DWORD 数组/WORD 类型点位时才条件 fork | 上游 issue/PR；插件侧 `eip_client` 配置校验 |
| async-opcua | 0.19.0 | 无（OPC-2/10/13/1/4/8 均可在插件侧解决：分批、`request_timeout`、`disconnect()`、质量计数、`Display`、`nsu=` 预解析） | **不 fork** | — |
| tokio-modbus | 0.17.0 | 无（MB-8 是主站帧间静默，主站侧影响低） | **不 fork**：插件加可选请求间隔 | 插件配置项 |

**fork 最小补丁集（tokio-s7）**：
1. `handle_write` 逐项校验 `datum.return_items` 的 `S7ReturnCode`（S-1）；
2. `header.rs` 按两字节（class, code）解析错误区（S-4）；
3. `read_string` 改 Latin-1 逐字节解码（S-5）；
4. `connection_type` 编入 local TSAP 首字节（S-9）。

每个补丁必须附一条库内单测（S-1 用 mock PLC 回 `0x05` 断言 Err；S-4 用 `0x85/0x04` 帧断言 class/code；S-5 用 `0xB0 0x43` 断言 `"°C"`；S-9 断言 TSAP 字节）。

## 4. fork 的工程形态

**引用方式（推荐 `[patch.crates-io]` + git）**：

```toml
[patch.crates-io]
tokio-s7 = { git = "https://git.metme.top/viccom/tokio-s7.git", branch = "rs-iot/fixes" }
```

- 不改 `Cargo.toml` 的依赖声明（`tokio-s7 = { version = "0.1", optional = true }` 保持），`[patch]` 只覆盖来源；
- fork 的 `Cargo.toml` 版本号保持 `0.1.7`（或 `0.1.7-rsiot.1`），否则 patch 匹配不上 `version = "0.1"`；
- CI 不需要像 lux/tsink 那样加 `actions/checkout` 步骤（Cargo 自行拉 git）；
- 退出 = 删 `[patch]` 段，一行回退。

**备选（Plan B）**：若 CI runner 访问不了 git.metme.top，改用 path 依赖 + CI checkout（照抄 `.github/workflows/ci.yml` 的 lux/tsink 步骤，把 fork 镜像到 GitHub 私有仓或加代理）。**Plan C（不推荐）**：把 fork 源码拷入本仓。

**不选 path 作首选的理由**：path 依赖会让 CI 多一步 checkout、把"第三方 crate 的临时补丁"伪装成"自有仓"；而 `[patch]` 是 Cargo 为此场景设计的机制，且切回 crates.io 的成本最低。若团队更看重"与 lux/tsink 先例一致"，Plan B 也可接受，但必须在 CI 每个 Rust job 同步加 checkout。

## 5. 治理与升级

- fork 仓保留 `upstream` remote；分支 `rs-iot/fixes` 只含台账 ID 对应的补丁，不夹带格式/重构改动。
- fork 仓内 `UPSTREAM.md` 记录：上游 commit、每个补丁的台账 ID、新增单测、退出条件。
- 每季度（或每次 `cargo update` 前）检查上游新版本是否已含修复：已含 → 删对应补丁；全部含 → 删 `[patch]` 切回 crates.io。
- 上游大版本升级时 rebase `rs-iot/fixes`，跑 fork 单测 + rs-iot 的 `s7_e2e`/`eip_client_e2e`。
- 许可：fork 保留 `LICENSE`/`NOTICE` 与文件头版权；tokio-s7 为 Apache-2.0，修改文件需保留声明。
- 安全：`cargo-audit` 不覆盖 git/path 依赖，fork 期间由季度巡检人工跟踪上游安全公告（写进 `UPSTREAM.md`）。

## 6. 被否选项

| 选项 | 否决理由 |
|---|---|
| 把上游源码拷进本仓（vendoring） | 升级 = 手工重放补丁；仓库膨胀；与 `../lux`/`../tsink_fork` 模式冲突；审计与缓存复杂化 |
| 只建 wrapper crate（shim） | 只能包"外部可做"的行为（重连、分片、错误归类），改不了库内解码/应答（S-1/S-4/S-5/E-2/E-3 无效），且多一层维护面 |
| 换协议库 | tokio-s7 的 COTP/PDU 协商部分可用，重写成本远高于 40 行补丁；AB 标签协议的成熟 Rust 替代品少 |
| 只提 PR、不建 fork | S-1（写失败静默成功）是 P0；tokio-s7 crates.io 停在 0.1.7（2026-07 后无新版），等待期不可控 |
| 四库全部 fork | 无必要：async-opcua/tokio-modbus 无必须改库项，rust-ethernet-ip 可先规避 + PR |

## 7. 后果与风险

- **可复现性**：`[patch]` 引入 git 依赖，`Cargo.lock` 的 source 变为 git；首次构建需能访问 fork 仓。缓解：fork 托管在团队自有 git 服务（与 rs-iot remote 同域），CI 可达性在落地时验证；不可达则切 Plan B。
- **安全审计盲区**：git/path 依赖不在 `cargo-audit` 覆盖内。缓解：季度巡检 + `UPSTREAM.md` 显式记录。
- **上游分叉成本**：补丁小且集中，rebase 成本低；若上游改版触及补丁区域，需重跑 mock PLC 探针（`/tmp/s7probe` 的用例应固化进 fork 仓测试）。
- **认知成本**：团队成员需要知道"为什么有个 fork"。缓解：本决策 + 台账"处置"列 + `UPSTREAM.md` 三处互链。

## 8. 执行顺序

1. **插件侧规避先行**：台账 43 条插件条目中"库缺陷可规避"的部分（S-2/S-3、E-1/E-4/E-5/E-6/E-10/E-11/E-12/E-13/E-15、OPC 全部库相关项）——不依赖任何 fork，先消掉 P0/P1 的大半。
2. **tokio-s7 fork**：建 `viccom/tokio-s7` 仓与 `rs-iot/fixes` 分支 → 4 补丁 + 单测 → `Cargo.toml` 加 `[patch]` → 本地 `just check` 与 `--features "s7,eip"` e2e 全绿 → 提上游 PR。
3. **rust-ethernet-ip**：提 issue/PR（E-2/E-3/E-16），插件侧先做配置期拒绝与规避；现场出现 DWORD 数组/WORD 需求时按 §4 建条件 fork。
4. **落 `UPSTREAM.md` 与季度巡检**（可并入 warden 的运维日历或 CI 提醒）。

**回退**：删 `[patch.crates-io]` 段即回到 crates.io 版本；fork 补丁对 rs-iot 插件侧无 API 变化（仅行为修正），回退不破坏插件代码。

## 9. 执行进展（2026-11-01 第二批）

- **插件侧规避已全部落地**（见台账 §9）：S-2/S-3、S-6~S-8、S-10~S-15、S-17~S-21、E 全部可规避项、OPC 全部库相关项。
- **tokio-s7 fork 已备好并本地验证**：`dist/vendor/tokio-s7-fork`（git 仓，基线 0.1.7 + 4 补丁 commit），补丁与单测：
  - S-1 写应答 item 级 return code 校验（`client.rs:check_write_return_items`，mock PLC 单测）；
  - S-4 头级错误区按 `(class, code)` 两字节解析（`header.rs`，`0x85/0x04` 帧单测）；
  - S-5 STRING Latin-1 解码（`read_string`，`0xB0 0x43 → "°C"` 单测）；
  - S-9 `connection_type` 编入 local TSAP（`tsaps()`，三连接类型单测）。
  - 验证：fork 内 `cargo test` 11 绿；rs-iot 以临时 path `[patch]` 联调 `s7_client` 40 绿 + `s7_e2e` 4 绿；试验后 `Cargo.toml/Cargo.lock` 已逐字节还原（未提交 path patch）。
- **未接入的原因**：`git.metme.top/viccom/tokio-s7` 尚不存在；创建远程仓属对外动作，待用户批准。
- **接入步骤**（用户建仓后）：
  ```bash
  cd dist/vendor/tokio-s7-fork
  git remote add origin https://git.metme.top/viccom/tokio-s7.git
  git branch -m rs-iot/fixes
  git push -u origin rs-iot/fixes
  ```
  ```toml
  # rs-iot/Cargo.toml
  [patch.crates-io]
  tokio-s7 = { git = "https://git.metme.top/viccom/tokio-s7.git", branch = "rs-iot/fixes" }
  ```
- **S-1 过渡防线**：在 fork 接入前，S7 插件 `verify_write`（默认 true）已消除"写失败静默成功"（写后读回校验）。
- **rust-ethernet-ip**：E-2/E-3/E-16 的插件侧拒绝已落地；上游 issue/PR 尚未提（需 GitHub 凭据/网络，待用户授权）。
