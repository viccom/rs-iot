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
STATUSES = ("implemented", "partially-implemented", "accepted", "in-review", "draft")
# 头部状态 → docs/README.md 索引行应含的中文标记（登记与状态对齐校验用）
INDEX_MARK = {
    "implemented": "已实施",
    "partially-implemented": "部分实施",
    "accepted": "待实施",
    "in-review": "评审",
    "draft": "讨论中",
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


def check_index(path: Path, state: str | None) -> None:
    """designs/decisions 每篇必须在 docs/README.md 登记且状态标记对齐（规范 §4）。"""
    if path.parent not in (DOCS / "designs", DOCS / "decisions") or not state:
        return
    rel = path.relative_to(ROOT).as_posix()
    name = path.name
    index_lines = [ln for ln in read(DOCS / "README.md").splitlines() if name in ln]
    if not index_lines:
        err(f"{rel}: 未在 docs/README.md 索引登记（规范 §4）")
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
            state = check_header(path, text)
            check_impl_block(path, text, state)
            check_index(path, state)
    # 3) 相对链接有效性（全部 md，含根 README/CLAUDE 与 examples）
    check_links(files)

    if problems:
        print(f"docs check: {len(problems)} 个违规")
        for p in problems:
            print(f"  - {p}")
        return 1
    print(f"docs check: OK（{len(files)} 个 md，状态块/实施块/索引/链接全部通过）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
