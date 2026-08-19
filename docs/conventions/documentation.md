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
| `ops/` | 怎么部署/维护（run） | 部署手册、现场测试手册、上游同步维护策略 | 开发向内容（→ guides） |

新文档**先选域再落笔**；跨域内容按"主要目的"归类，并在文首链接另一域的配套文档。

例外位置（不属于 `docs/`，但属文档体系）：

- 根 `README.md`（项目门面）、`CLAUDE.md`（AI 协作指令 + 项目地图）
- `examples/*/README.md`、`field-kit/README.md`（代码资产的就近说明，随资产移动）
- 子 crate（`crates/*`）与 `frontend/` 的 README 应随各自演进补建

## 2. 命名规范

- **活文档**（guides / reference / conventions / ops，随代码演进持续维护）：语义名 kebab-case，如 `lua-script-guide.md`、`rest-api.md`。不带日期——活文档的修改历史在 git。
- **一次性文档**（designs / decisions，定稿后基本不改）：`YYYY-MM-DD-<topic>[-design].md` 日期前缀，如 `2026-08-17-eip-client-design.md`。日期 = 定稿日；版本迭代（v1→v2→v3）新开文件，旧版归档。
- 禁止再使用无日期的 `plan-*.md` 平铺命名（2026-08-19 重组前的旧形态，已全部迁移）。

## 3. 头部元数据

每个文档标题下必须有状态块（blockquote 风格）：

```markdown
> **状态**：<状态>（一句话说明）
> **日期**：YYYY-MM-DD（一次性文档必填；活文档可省）
> **类型**：design / decision / guide / reference / convention / ops
```

状态取值与流转：

| 状态 | 含义 | 适用 |
|---|---|---|
| `draft` | 起草中，结论未定 | designs / decisions |
| `in-review` | 待评审 | designs / decisions |
| `accepted` | 定稿待实施 | designs / decisions |
| `implemented` | 已落地代码 | designs / decisions（落地后**回写**此状态） |
| `活文档` | 持续维护，随代码同步 | guides / reference / conventions / ops |
| `有效` / `已废止` | 规范类生效 / 停用 | conventions |
| `archived` | 过时归档（归档即 git rm，通常不再出现在仓库） | 全部 |

与代码冲突时以代码为准，并回改文档（既有约定，见 designs 文档）。

## 4. 索引义务

`docs/README.md` 是文档体系的唯一索引。**新增、移动、改名、归档任何文档，必须同一次提交内更新索引**；同时检查 `CLAUDE.md`（项目地图引用了多篇文档）与根 `README.md` 是否需要同步。

## 5. 引用规则

- 文档互链用**相对路径**（如 `guides/lua-script-guide.md` 链接 `../reference/rest-api.md`）。
- 禁止链接仓库外绝对路径（`E:\MyCode\...`）——外部资料以文字注明出处（如 eip 设计引用《RUST生态-工业协议研究报告》）。
- **单一权威源**：一个主题一个权威文档，其他文档只链接不复制。协议细节以 `reference/external-plugin-protocol.md` 为准，指南不复述字段表。
- 引用已归档文档时写明「已归档，git 历史 66945dd 可溯」式标注，不留死链接。

## 6. 生命周期与归档

1. **创建**：选域 → 命名 → 头部元数据 → 登记索引 → 提交。
2. **实施回写**：设计/决策落地后回写状态为 `implemented`（含偏差说明）。
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

## 8. 提交规范

沿用 Conventional Commits + 中文（与仓库一致）：

- 结构变更：`docs: 重组文档目录——<说明>`
- 新增/修改：`docs(<scope>): <说明>`（scope 用主题，如 `docs(lua)`、`docs(plan)`、`docs(spec)`）
- 归档：`chore(docs): 归档 N 个过时文档（<原因>）`
