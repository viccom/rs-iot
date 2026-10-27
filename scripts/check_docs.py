#!/usr/bin/env python3
"""文档体系校验：头部状态块 / 实施块字段 / 索引登记与状态对齐 / 相对链接有效性。

规则定义见 docs/conventions/documentation.md（§3 状态机、§4 索引义务、§5 引用规则）；
CI 挂载于 .github/workflows/ci.yml 的 docs job。用法：仓库根执行
``python scripts/check_docs.py``，违规打印清单并 exit 1。
"""
from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DOCS = ROOT / "docs"

# designs/decisions 的受控状态词（规范 §3 状态机）
STATUSES = ("implemented", "partially-implemented", "accepted", "in-review", "draft", "superseded")
# 头部状态 → docs/README.md 索引行应含的中文标记（登记与状态对齐校验用）
INDEX_MARK = {
    "implemented": "已实施",
    "partially-implemented": "部分实施",
    "accepted": "待实施",
    "in-review": "评审",
    "draft": "讨论中",
    "superseded": "已被取代",
}

STATUS_RE = re.compile(r"^> ?\*\*状态\*\*[：:]\s*(.+?)\s*$", re.M)
FIELD_RES = {
    name: re.compile(rf"^> ?\*\*{name}\*\*[：:]\s*(.*?)\s*$", re.M)
    for name in ("实施", "代码", "测试", "遗留")
}
LINK_RE = re.compile(r"\]\(([^)\s#]+)\)")

problems: list[str] = []


def err(msg: str) -> None:
    problems.append(msg)


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def tracked_md_files() -> list[Path]:
    out = subprocess.run(
        ["git", "ls-files", "*.md"], cwd=ROOT, capture_output=True, text=True, check=True
    ).stdout
    return sorted(ROOT / line for line in out.splitlines() if line.strip())


def check_header(path: Path, text: str) -> str | None:
    """docs/ 下每篇文档（索引除外）必有状态块；返回状态值（designs/decisions 受控）。"""
    rel = path.relative_to(ROOT).as_posix()
    m = STATUS_RE.search(text)
    if not m:
        err(f"{rel}: 缺少头部状态块（`> **状态**：…`，规范 §3）")
        return None
    state = m.group(1)
    if path.parent in (DOCS / "designs", DOCS / "decisions"):
        head = next((s for s in STATUSES if state.startswith(s)), None)
        if head is None:
            err(f"{rel}: 状态 '{state}' 不在受控集合 {STATUSES}（规范 §3）")
        return head
    return state


def check_impl_block(path: Path, text: str, state: str | None) -> None:
    """implemented/partially 必填实施+代码；partially 必填非空遗留（规范 §3/§6）。"""
    if state not in ("implemented", "partially-implemented"):
        return
    rel = path.relative_to(ROOT).as_posix()
    impl = FIELD_RES["实施"].search(text)
    code = FIELD_RES["代码"].search(text)
    if not impl:
        err(f"{rel}: 状态 {state} 必填 `**实施**` 字段（实施 commit/日期，规范 §3）")
    elif not impl.group(1):
        err(f"{rel}: `**实施**` 字段为空")
    if not code:
        err(f"{rel}: 状态 {state} 必填 `**代码**` 字段（落地代码位置，规范 §3）")
    if state == "partially-implemented":
        left = FIELD_RES["遗留"].search(text)
        if not left or left.group(1) in ("", "无"):
            err(f"{rel}: 部分实施必须列明 `**遗留**`（未实施部分，规范 §3）")


INDEXED_DIRS = ("designs", "decisions", "guides", "reference", "ops")


def check_index(path: Path, state: str | None) -> None:
    """**六域全部**文档必须在 docs/README.md 登记；designs/decisions 另校验状态标记对齐。

    索引行判定：该行必须**以本文件名链接开头**（`| [<name>](...)`），避免被"主题链路视图"
    里顺带提到文件名的长行掩盖（2026-10-27 审计：原"任一行含文件名"导致假阴性）。
    ops/releases/ 是发布档案，豁免索引与状态校验。
    """
    if path.parent.name not in INDEXED_DIRS or not state:
        return
    if "releases" in path.parts:
        return
    rel = path.relative_to(ROOT).as_posix()
    name = path.name
    # 独立分域索引行：以表格单元格形式出现（| [name](...) 或 - [name](...)）
    idx_re = re.compile(rf"^\s*[|\-]\s*\[{re.escape(name)}\]\(")
    index_lines = [ln for ln in read(DOCS / "README.md").splitlines() if idx_re.match(ln)]
    if not index_lines:
        err(f"{rel}: 未在 docs/README.md 的**独立索引行**登记（规范 §4；主题链路行不算）")
        return
    mark = INDEX_MARK.get(state)
    if mark and not any(mark in ln for ln in index_lines):
        err(f"{rel}: 索引状态与头部 '{state}' 不一致（索引行应含『{mark}』，规范 §4）")


def check_links(files: list[Path]) -> None:
    """全部 git 跟踪 md 的相对链接逐一解析，目标必须存在（规范 §5）。"""
    for path in files:
        text = read(path)
        for target in LINK_RE.findall(text):
            if target.startswith(("http://", "https://", "mailto:", "file:")):
                continue
            resolved = (path.parent / target).resolve()
            if not resolved.exists():
                err(f"{path.relative_to(ROOT).as_posix()}: 断链 -> {target}")


# 文档中反引号包裹的仓库内路径（src/... tests/... crates/... frontend/src/...）
CODE_PATH_RE = re.compile(r"`((?:src|tests|crates|frontend/src|frontend)/[A-Za-z0-9_./{}\-]+\.(?:rs|ts|vue|toml|md))`")

# tsink fork 的源码目录特征（本仓 src/ 下不存在这些顶层目录）
TSINK_TOPS = ("src/engine/", "src/storage", "src/segment", "src/wal", "src/compactor", "src/l0")


# 文档里的跨仓路径上下文：同一行/邻近出现这些词时，`src/...` 指向兄弟仓库而非本仓
SIBLING_RE = re.compile(r"tsink|tsink_fork|\.\./lux|lux/|lux 仓库|上游 lux", re.I)


def check_code_paths(path: Path, text: str) -> None:
    """implemented/partially-implemented 文档中引用的代码路径必须真实存在（规范 §3 实施块）。

    抓"重构后旧文档仍指向已删文件"这类漂移（2026-10-27 审计：tag_registry.rs / bench/ 等）。
    容忍：{a,b} 展开式路径（拆开逐一校验）；跨仓路径（同行出现 tsink/lux 语境的）跳过。
    """
    rel = path.relative_to(ROOT).as_posix()
    for m in CODE_PATH_RE.finditer(text):
        raw = m.group(1)
        # tsink fork 源码路径：本仓 src/ 无这些顶层目录，一律视为兄弟仓库
        if raw.startswith(TSINK_TOPS):
            continue
        # 取该行上下文判断是否跨仓（tsink fork / lux 上游的源码路径本仓必然不存在）
        line_start = text.rfind("\n", 0, m.start()) + 1
        line_end = text.find("\n", m.end())
        line = text[line_start: line_end if line_end != -1 else len(text)]
        if SIBLING_RE.search(line):
            continue
        # 显式标注"已删除/已退役/已移除"的路径是历史记录（规范允许，用于保留演进脉络）
        if re.search(r"已删除|已退役|已移除|已弃用|deleted|retired", line):
            continue
        # 计划稿中的"候选/假设"路径（"可提为"、"若…则"、"照 … helper"）
        if re.search(r"可提为|若 .{0,30} 则|拟建|计划新增|拟新增|candidate", line):
            continue
        # 第三方 crate 源码路径（mlua/axum/lux 等上游）：本仓同名前缀不存在即是上游
        # 判定依据：路径首段 src/ 且本仓 src/ 下无该直接子项 → 上游源码
        # 上游/第三方 crate 源码路径：本仓同前缀不存在 + **该行内**确指上游（严格同行，
        # 不做全文放行——否则一篇提到 lux 的文档会放行自身所有死路径，检查形同虚设）
        prefix = "src/" if raw.startswith("src/") else "tests/" if raw.startswith("tests/") else ""
        first = raw[len(prefix):].split("/")[0].rstrip(".") if prefix else ""
        if prefix and first and not (ROOT / prefix / first).exists() \
                and not (ROOT / prefix / f"{first}.rs").exists():
            if SIBLING_RE.search(line) or re.search(
                r"mlua|axum|pingora|上游|upstream|官方|第三方", line, re.I
            ):
                continue
        # {a,b,c} 展开
        if "{" in raw:
            import itertools
            pre, rest = raw.split("{", 1)
            mid, post = rest.split("}", 1)
            cands = [pre + opt + post for opt in mid.split(",")]
        else:
            cands = [raw]
        for c in cands:
            if not (ROOT / c).exists():
                err(f"{rel}: 引用不存在的代码路径 `{c}`（规范 §3：实施块/正文须指向真实文件）")


def check_no_plans_dir() -> None:
    """docs/plans/ 已于 2026-10-27 并入 designs/（宪法 §1 六分域）；禁止再长出该目录。"""
    if (DOCS / "plans").exists():
        err("docs/plans/ 存在：宪法 §1 只定义六类分域，plans/ 已于 2026-10-27 并入 designs/"
            "（新计划请放 designs/ 并加日期前缀 + 状态块）")


def main() -> int:
    files = tracked_md_files()
    for path in files:
        if not path.is_file():
            continue
        text = read(path)
        # 1) 头部状态块 + 2) 实施块字段（仅 docs/ 体系文档；索引自身豁免状态块）
        if path.parent == DOCS or path.parent in (DOCS / "designs", DOCS / "decisions",
                                                  DOCS / "guides", DOCS / "reference",
                                                  DOCS / "conventions", DOCS / "ops"):
            if path.name == "README.md":
                continue
            if "releases" in path.parts:
                # 发布档案：仅要求有状态块（豁免状态词与索引）
                if not STATUS_RE.search(text):
                    err(f"{path.relative_to(ROOT).as_posix()}: 发布档案缺少头部状态块（规范 §1 ops/releases）")
                continue
            state = check_header(path, text)
            check_impl_block(path, text, state)
            check_index(path, state)
    # 3) 相对链接有效性（全部 md，含根 README/CLAUDE 与 examples）
    check_links(files)
    # 4) 代码路径存在性（实施块/正文里的 src|tests|crates 路径）
    for path in files:
        if path.is_file() and path.suffix == ".md":
            check_code_paths(path, read(path))
    # 5) 分域约束：plans/ 不得复活
    check_no_plans_dir()

    if problems:
        print(f"docs check: {len(problems)} 个违规")
        for p in problems:
            print(f"  - {p}")
        return 1
    print(f"docs check: OK（{len(files)} 个 md，状态块/实施块/索引/链接全部通过）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
