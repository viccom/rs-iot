#!/usr/bin/env python3
"""分层依赖门禁：生产段模块图必须无环 + 禁止已知越层引用。

背景（2026-11-01 分层耦合审查）：`docs/architecture.md` 曾声称"依赖矩阵严格单向、
无环"，实测模块图是一个 10 模块强连通分量——全部由 3 条「共享载荷类型定义在
消费方模块」的回边造成（`RsIotError::Model` / `EventPayload::State` /
`EventPayload::Log`）。三条回边切掉后整图变 DAG，本脚本把这件事变成机械门禁
（对应 hmi-studio `scripts/check-layers.mjs` 的做法）。

规则：
1. **无环**：生产段（排除注释与 `#[cfg(test)] mod` 段）的顶层模块引用图，任一
   强连通分量 > 1 即失败，并打印环上的具体边与首处 file:line。
2. **禁止越层**：少数"无环但方向错"的引用（见 FORBIDDEN），同样失败。

用法：仓库根执行 ``python3 scripts/check_layers.py``；违规打印清单并 exit 1。
"""
from __future__ import annotations

import re
import sys
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "src"

# (源模块, 目标前缀, 例外前缀, 理由)
FORBIDDEN = (
    (
        "api",
        "plugin::lua_core",
        "plugin::lua_core::manage",
        "api 只经 lua_core::manage 管理面，不得引用 Lua 引擎内部（loader/stdlib/reload…）",
    ),
    ("plugin", "history", "", "插件层不得依赖 history 子系统（计数经 HostContext::archive_counters）"),
    ("history", "api", "", "history 是数据面旁路，不得反向依赖 REST 层"),
    ("db", "api", "", "db 是存储层，不得反向依赖 REST 层"),
    ("model", "api", "", "model 是设备域，不得反向依赖 REST 层"),
    ("plugin", "api", "", "插件层不得反向依赖 REST 层"),
)

REF_RE = re.compile(r"\bcrate::([a-z_][a-z0-9_]*)")
TEST_MOD_RE = re.compile(r"\s*(pub )?mod \w+")


def top_modules() -> set[str]:
    """src/ 下的一级模块名（目录或 .rs 文件）。"""
    mods = set()
    for p in SRC.iterdir():
        if p.is_dir():
            mods.add(p.name)
        elif p.suffix == ".rs":
            mods.add(p.stem)
    return mods


def module_of(path: Path) -> str:
    rel = path.relative_to(SRC)
    first = rel.parts[0]
    return first[:-3] if first.endswith(".rs") else first


def prod_lines(path: Path) -> list[tuple[int, str]]:
    """去掉注释行与文件尾部 `#[cfg(test)] mod …` 段后的生产行。"""
    lines = path.read_text(encoding="utf8", errors="ignore").splitlines()
    cut = len(lines)
    for i, line in enumerate(lines):
        if line.strip().startswith("#[cfg(test)]"):
            j = i + 1
            while j < len(lines) and lines[j].strip().startswith("#["):
                j += 1
            if j < len(lines) and TEST_MOD_RE.match(lines[j]):
                cut = i
                break
    out = []
    for i, line in enumerate(lines[:cut], 1):
        if line.lstrip().startswith("//"):
            continue
        out.append((i, line))
    return out


def source_files():
    """全部生产源文件：排除 `tests.rs`（`#[cfg(test)] mod tests;` 引入的纯测试模块）。"""
    return [p for p in sorted(SRC.rglob("*.rs")) if p.name != "tests.rs"]


def build_graph(mods: set[str]):
    """返回 {(src, dst): [file:line, ...]}。"""
    edges: dict[tuple[str, str], list[str]] = defaultdict(list)
    for path in source_files():
        src = module_of(path)
        for lineno, line in prod_lines(path):
            for m in REF_RE.finditer(line):
                dst = m.group(1)
                if dst != src and dst in mods:
                    edges[(src, dst)].append(f"{path.relative_to(ROOT)}:{lineno}")
    return edges


def tarjan(nodes: set[str], edges: dict[tuple[str, str], list[str]]) -> list[list[str]]:
    graph: dict[str, set[str]] = defaultdict(set)
    for s, d in edges:
        graph[s].add(d)
    index: dict[str, int] = {}
    low: dict[str, int] = {}
    on_stack: set[str] = set()
    stack: list[str] = []
    out: list[list[str]] = []
    counter = [0]

    def strong(v: str) -> None:
        index[v] = low[v] = counter[0]
        counter[0] += 1
        stack.append(v)
        on_stack.add(v)
        for w in graph.get(v, ()):
            if w not in index:
                strong(w)
                low[v] = min(low[v], low[w])
            elif w in on_stack:
                low[v] = min(low[v], index[w])
        if low[v] == index[v]:
            comp = []
            while True:
                w = stack.pop()
                on_stack.discard(w)
                comp.append(w)
                if w == v:
                    break
            if len(comp) > 1:
                out.append(sorted(comp))

    for v in sorted(nodes):
        if v not in index:
            strong(v)
    return out


def check_forbidden(mods: set[str]) -> list[str]:
    problems = []
    for path in source_files():
        src = module_of(path)
        for lineno, line in prod_lines(path):
            for m in re.finditer(r"\bcrate::([a-z_][a-z0-9_:]*)", line):
                target = m.group(1)
                if target.split("::")[0] not in mods:
                    continue
                for s, prefix, allow, why in FORBIDDEN:
                    if src != s or not target.startswith(prefix):
                        continue
                    if allow and target.startswith(allow):
                        continue
                    problems.append(f"{path.relative_to(ROOT)}:{lineno}: {src} -> {target}：{why}")
    return problems


def main() -> int:
    mods = top_modules()
    edges = build_graph(mods)
    cycles = tarjan(mods, edges)
    problems = check_forbidden(mods)

    if cycles:
        print(f"layers check: {len(cycles)} 个模块环")
        for comp in cycles:
            print("  环上模块: " + " <-> ".join(comp))
            inside = set(comp)
            mutual = [
                (s, d)
                for (s, d) in sorted(edges)
                if s in inside and d in inside and (d, s) in edges
            ]
            others = [
                (s, d)
                for (s, d) in sorted(edges)
                if s in inside and d in inside and (d, s) not in edges
            ]
            for s, d in mutual:
                print(f"    互相引用 {s} <-> {d}：{edges[(s, d)][0]} / {edges[(d, s)][0]}")
            for s, d in others:
                print(f"    单向 {s} -> {d}  首处 {edges[(s, d)][0]}（共 {len(edges[(s, d)])} 处）")
    if problems:
        print(f"layers check: {len(problems)} 处越层引用")
        for p in problems:
            print(f"  - {p}")
    if cycles or problems:
        return 1

    print(
        f"layers check: OK（{len(mods)} 个顶层模块 / {len(edges)} 条生产依赖边，无环、无越层引用）"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
