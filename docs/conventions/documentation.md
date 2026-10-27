# rs-iot 文档管理规范

> **状态**：有效（宪法级约束，新增/移动/归档文档必读）
> **日期**：2026-08-19（随 2026-08-19 文档重组建立）
> **类型**：convention

本规范定义 rs-iot 仓库文档的目录分域、命名、元数据、索引与生命周期。2026-08-17 归档 30 个过时文档（commit 66945dd）时因无引用清理约定遗留 8 组断链——本规范的归档流程即为吸取该教训而设。

## 1. 目录结构（六类分域）

| 目录 | 定位（回答的问题） | 放什么 | 不放什么 |
|---|---|---|---|
| `guides/` | 怎么用 / 怎么开发（how-to） | 面向使用者和开发者的操作指南 | 接口字段逐条罗列（→ reference） |
| `reference/` | 是什么（what） | REST API 全表、二进制协议规约等权威接口描述 | 教程式引导（→ guides） |
| `conventions/` | 必须遵守什么（must） | 埋点规范、本规范等约束性文档 | 可选建议、背景介绍 |
| `decisions/` | 为什么选 X（why，ADR） | 技术选型、方案取舍的决策记录，结论导向 | 完整实施方案（→ designs） |
| `designs/` | 怎么建（what to build） | 设计定稿：架构、模块、子系统设计文档 | 部署操作（→ ops）、决策论证（→ decisions） |
| `ops/` | 怎么部署/维护（run） | 部署手册、现场测试手册、上游同步维护策略、调优记录 | 开发向内容（→ guides） |
| `ops/releases/` | **发布档案**（不可变历史凭证） | 每个里程碑的发布说明：升级必读/新能力/已知限制 | 活文档（发布后不再改写，勘误以新档案追加） |

新文档**先选域再落笔**；跨域内容按"主要目的"归类，并在文首链接另一域的配套文档。

例外位置（不属于 `docs/`，但属文档体系）：

- 根 `README.md`（项目门面）、`CLAUDE.md`（AI 协作指令 + 项目地图）
- `examples/*/README.md`、`field-kit/README.md`（代码资产的就近说明，随资产移动）
- 子 crate（`crates/*`）与 `frontend/` 的 README 应随各自演进补建

## 2. 命名规范

- **活文档**（guides / reference / conventions / ops，随代码演进持续维护）：语义名 kebab-case，如 `lua-script-guide.md`、`rest-api.md`。不带日期——活文档的修改历史在 git。
- **一次性文档**（designs / decisions，定稿后基本不改）：`YYYY-MM-DD-<topic>[-design].md` 日期前缀，如 `2026-08-17-eip-client-design.md`。日期 = 定稿日；版本迭代（v1→v2→v3）新开文件，旧版归档。
- 禁止再使用无日期的 `plan-*.md` 平铺命名（2026-08-19 重组前的旧形态，已全部迁移）。

## 3. 头部元数据与实施块

每个文档标题下必须有状态块（blockquote 风格）。**designs / decisions（一次性文档）用完整块**——实施块字段是"单看文档即知实施状态"的关键，也是 `scripts/check_docs.py` 的校验对象：

```markdown
> **状态**：implemented / partially-implemented / accepted / in-review / draft / superseded（+ 一句话说明）
> **日期**：YYYY-MM-DD（定稿）
> **实施**：YYYY-MM-DD `commit-hash` 说明（→ `hash` 收尾）；未实施写"未开始"
> **代码**：落地代码位置（`src/...` / `crates/...`）
> **测试**：测试位置（`tests/...` / crate 内单测）
> **遗留**：未实施部分；全部实施写"无"
```

**guides / reference / conventions / ops（活文档）用简块**：

```markdown
> **状态**：活文档 / 有效 / implemented（+ 随什么演进/待什么核对的一句话）
```

状态取值与流转（designs / decisions 受控，scripts 校验）：

| 状态 | 含义 | 实施块要求 |
|---|---|---|
| `draft` | 起草/讨论中，结论未定 | 实施写"未开始" |
| `in-review` | 待评审 | 同上 |
| `accepted` | 定稿待实施 | 同上 + **遗留**写"全部（…）" |
| `partially-implemented` | 部分落地（最常见形态：分阶段设计走完第一阶段） | 必填实施/代码 + **遗留列明未实施部分** |
| `implemented` | 已全部落地（落地后**回写**此状态 + 实施 commit） | 必填实施/代码；遗留写"无" |
| `已发布` | `ops/releases/` 专用（非受控状态机） | 仅需状态块；豁免索引登记与状态词校验 |
| `superseded` | **被后续设计/决策取代**（架构级替换的唯一合法标记） | 必填 **`> **取代者**：**` 指针（新文档相对链接 + 一句话差异）；索引标记「已被取代」 |
| `活文档` / `有效` / `已废止` | 活文档持续维护 / 规范生效、停用 | 不适用（简块） |
| `archived` | 过时归档（git rm，不在仓库出现） | 不适用 |

**实施凭证挖法**：`git log --format="%h %ad %s" --date=short --grep="<关键词>"` 或 `--diff-filter=A -- <代码路径>`。挖不到精确 commit 的用里程碑号（M10/P1#2）代替，但优先 commit——它可点击追溯。

与代码冲突时以代码为准，并回改文档（既有约定，见 designs 文档）。

## 4. 索引义务

`docs/README.md` 是文档体系的唯一索引，含两个视图：

- **主题链路视图**（表 1）：按功能主题串起 决策 → 设计 → 代码 → 测试 → 指南，每环带实施状态。这是团队成员"一眼看清哪些已实现/未实现、链路在哪"的入口。**新设计文档定稿或实施状态变化时，必须同 PR 更新对应主题行**；出现新主题（如新南向协议成体系）时加行。
- **分域索引**（表 2+）：六个域的文件级清单，新增/移动/改名/归档必须同步。

索引更新与 `CLAUDE.md`（项目地图）、根 `README.md` 的同步检查在同一次提交内完成；`scripts/check_docs.py` 会校验索引登记与状态标记对齐。

## 5. 引用规则

- 文档互链用**相对路径**（如 `guides/lua-script-guide.md` 链接 `../reference/rest-api.md`）。
- 禁止链接仓库外绝对路径（`E:\MyCode\...`）——外部资料以文字注明出处（如 eip 设计引用《RUST生态-工业协议研究报告》）。
- **单一权威源**：一个主题一个权威文档，其他文档只链接不复制。协议细节以 `reference/external-plugin-protocol.md` 为准，指南不复述字段表。
- 引用已归档文档时写明「已归档，git 历史 66945dd 可溯」式标注，不留死链接。
- **文档 ↔ 代码双向链接**：设计文档头部 `**代码**` 字段指向落地代码（doc → code）；落地代码的模块头 `//!` doc 注释注明设计文档路径（code → doc，如 `src/io_trace.rs:3`）。新模块/子系统落地时必须补反向链接，让追溯双向可走。

## 6. 生命周期与归档

1. **创建**：选域 → 命名 → 头部元数据 → 登记索引 → 提交。
2. **实施回写**：设计/决策落地后回写状态为 `implemented`（含偏差说明）。
2b. **取代回写**：任何"架构级替换"（模块搬迁/机制换型/依赖替换）发生后，旧文档必须回写
   `superseded` + `**取代者**` 指针——**禁止**让旧文档保持 `implemented` 而与新事实并存
   （这是"同名文档两套事实"的唯一根因，2026-10-27 审查实证）。
3. **归档**：被新版本替代或内容全面过时 → `git rm`（git 历史即归档，不建 `.bak/` 物理副本——2026-08-17 的 `.bak/` 物理保留实践已被证明无人维护）→ **清理全部引用**（见第 7 节清单）→ 提交信息注明 `chore(docs): 归档 N 个过时文档（<原因>）`。

## 7. 移动 / 改名 / 归档检查清单

结构变更（mv / rename / rm）前，按此清单搜索并同步更新引用：

- [ ] `CLAUDE.md`（最大引用源，12+ 处）
- [ ] 根 `README.md`
- [ ] `docs/README.md` 索引
- [ ] 其他 `docs/**/*.md` 互链（`rg '\]\([^)]*\.md' docs/`）
- [ ] 源码 `//!` / `///` doc 注释（`rg 'docs/' src/ crates/`）
- [ ] `Cargo.toml`、`.github/workflows/*.yml` 注释
- [ ] `examples/**`（README + py/go/lua 源码注释，相对路径最脆弱）
- [ ] `field-kit/README.md`

验证：`rg '<旧路径>'` 全仓库零残留 + 索引链接逐一可解析。

## 8. 校验（scripts/check_docs.py）

`python3 scripts/check_docs.py`（仓库根执行，CI `docs` job 强制）校验：

1. docs/ 全部文档（索引除外）必有头部状态块；designs/decisions 状态词受控
   （implemented / partially-implemented / accepted / in-review / draft / **superseded**）；
2. `implemented` / `partially-implemented` / `superseded` 必填 **实施** + **代码** 字段；
   部分实施必列 **遗留**，superseded 必填 **取代者** 指针；
3. designs/decisions/guides/reference/ops 每篇在 `docs/README.md` 有**独立索引行**（表格行以文件名链接单元格开头），designs/decisions 另校验状态标记与头部一致；`ops/releases/`
   为不可变发布档案，豁免索引与状态词（但仍须状态块）；
4. 全部 git 跟踪 md 的相对链接目标存在；
5. **正文反引号内的 `src|tests|crates|frontend/src` 路径必须真实存在**（跨仓 tsink/lux、
   上游 crate、显式标注"已删除/已退役"、计划稿候选路径豁免——按**同行**判定）；
6. **`docs/plans/` 不得存在**（2026-10-27 已并入 `designs/`；新计划放 designs + 日期前缀）。

改文档后本地先跑一遍再提交；CI 红了按报错逐条修（规范 §3/§4/§5）。

## 9. 提交规范

沿用 Conventional Commits + 中文（与仓库一致）：

- 结构变更：`docs: 重组文档目录——<说明>`
- 新增/修改：`docs(<scope>): <说明>`（scope 用主题，如 `docs(lua)`、`docs(plan)`、`docs(spec)`）
- 归档：`chore(docs): 归档 N 个过时文档（<原因>）`
