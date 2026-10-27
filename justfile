# rs-iot 本地质量门禁（背压优先：CI 不可达时的真实拦截层）
# 与 CI 的分工：本地门禁 = 每次改动必跑（秒~分钟级）；CI = 远端全量矩阵（依赖私库可达）。
# 历史教训（2026-10-27 审查 P1）：`.github/workflows/ci.yml` 的全部 Rust job 依赖
# `viccom/lux`，该库在 GitHub 上匿名 404 → CI 从未跑通，`-D warnings`/矩阵守护全是
# 纸面承诺。本文件让门禁在本机真实生效。

set shell := ["bash", "-c"]

# 默认：全套本地门禁（约 3~6 分钟）
default:
    @just check

# 全套：格式 + 文档 + lint 双档 + 单测
[group('quality')]
check: fmt docs lint test
    @echo "✅ rs-iot 本地门禁全过"

[group('quality')]
fmt:
    cargo fmt --all --check

# 文档宪法：状态块/实施块/索引/链接（documentation.md §8）
[group('quality')]
docs:
    python3 scripts/check_docs.py

# lint 双档：默认裁剪面（现场形态）+ 全开。history 档依赖 tsink path 依赖，
# 本机若为 stub 则跳过并显式提示（不是静默通过）。
[group('quality')]
lint:
    @echo "── clippy 默认档（default = lua,lua-http,opcua,modbus）──"
    cargo clippy --all-targets -- -D warnings
    @echo "── clippy 最小裁剪档（--no-default-features --features lua）──"
    cargo clippy --no-default-features --features lua --all-targets -- -D warnings
    @echo "── clippy all-features（history 依赖 tsink；stub 环境自动跳过）──"
    @if grep -q "stub:" ../tsink_fork/src/lib.rs 2>/dev/null; then \
        echo "⚠️  ../tsink_fork 为 stub（无真实 tsink）—— history 档不可编译，跳过 all-features"; \
    else \
        cargo clippy --all-features --all-targets -- -D warnings; \
    fi

# 单测 + e2e（默认 feature 面；历史档随 tsink 可用性）
[group('quality')]
test:
    cargo test --lib
    cargo test --tests --no-default-features --features lua

# 目标机器常用：协议裁剪档（s7/eip，非默认但 ADR 要求有门禁）
[group('quality')]
check-s7-eip:
    cargo test --features "s7,eip" --test s7_e2e --test eip_client_e2e --test feature_gate_e2e
