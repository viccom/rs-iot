# 默认编译面收敛（datasim/lua/modbus/opcua）

> **状态**：implemented（2026-08-26 裁决并落地；Cargo.toml default 已收敛）
> **实施**：2026-08-26 裁决并落地（Cargo.toml `default = ["lua","lua-http","opcua","modbus"]`）
> **代码**：`Cargo.toml` [features] default；裁剪档门禁见 `tests/feature_gate_e2e.rs`

- **日期**：2026-08-26
- **裁决**：用户（项目负责人）明示——"插件体系不需要每次都编译，目前默认编译 datasim、lua 脚本、Modbus、opcua 即可"
- **变更**：`Cargo.toml` default 自 `["transform","lua","lua-http","opcua","modbus","s7","eip"]` 收敛为 `["lua","lua-http","opcua","modbus"]`
  - s7/eip 出默认：显式 `--features s7,eip` 启用；运行期建实例返回 "source plugin not found" 优雅降级不变（[2026-08-07-feature-gating-protocols.md](2026-08-07-feature-gating-protocols.md) 先例）
  - transform（Q7 冻结不接线）出默认，feature 保留供未来解冻
  - datasim/self_monitor/sidecar 本就无条件编译，不在 feature 面内
- **冲突与风险**：无层级冲突（用户明文指令）。风险 = 依赖默认全开的既有 CI/脚本若隐式用 s7/eip 会静默失能——缓解 = 本仓库门禁新增 `--features "s7,eip"` 档跑 s7_e2e/eip_client_e2e/feature_gate_e2e（本次已全过：2/2、2/2、4/4；**2026-10-27 审查修正**：该档当时只记在决策文档里、ci.yml 并无对应 job——现已在 ci.yml `minimal` job 与本地 `just check-s7-eip` 双落地）
- **回退**：default 数组加回 `"s7","eip","transform"` 单行即恢复
