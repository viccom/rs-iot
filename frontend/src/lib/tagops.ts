// frontend/src/lib/tagops.ts
// 标签运营（D2）纯逻辑——后端口径的 TS 镜像：
// - globMatch：镜像 src/model/discovery.rs::glob_match（POST /api/v1/tag/batch
//   的匹配语义：仅 `*` 通配，`?`/`[..]` 为字面量——预览与后端同一算法，非近似）；
// - buildBatchProps：批量覆盖表单三态 → /tag/batch body 字段。⚠️ tag 端口
//   deadband 三态 null=恢复继承、0.0=显式关（与 devreg overrides 口径相反，
//   见 docs/reference/rest-api.md §4.15 警示段）——本 UI 不提供显式关；
// - reconcileTags：effective 后端对账（本地 mergeTagProps 平铺 vs GET /tags）。

/** 对账用的 effective 四属性子集（TagProps 裁剪）。 */
export interface EffectiveProps {
  history: boolean
  deadband: number | null
  unit: string | null
  desc: string | null
}

/**
 * glob 匹配（匹配 `<devid>.<tag>` 全键）——与后端 `discovery::glob_match`
 * 逐行同构：pattern 按 `*` 切段，首段须为前缀、末段须为后缀、中段按序
 * 首次出现定位。仅 `*` 是通配符（`?`/`[..]` 按字面量比较）。
 */
export function globMatch(pattern: string, key: string): boolean {
  const parts = pattern.split('*')
  if (parts.length === 1) return pattern === key
  if (!key.startsWith(parts[0])) return false
  let rest = key.slice(parts[0].length)
  for (const part of parts.slice(1, -1)) {
    const pos = rest.indexOf(part)
    if (pos < 0) return false
    rest = rest.slice(pos + part.length)
  }
  return rest.endsWith(parts[parts.length - 1])
}

/** 批量覆盖表单：归档白名单三选。 */
export type HistoryChoice = 'keep' | 'on' | 'off'

/** 批量覆盖表单：死区三选（显式关 0.0 不提供——与无死区过滤等价，语义陷阱）。 */
export type DeadbandChoice = 'keep' | 'set' | 'inherit'

/**
 * 表单三选 → /tag/batch body 的 props 字段：不变不携带（后端语义=不动现有
 * 覆盖）；「恢复继承」提交 `deadband: null`（tag 端口语义，与 devreg 覆盖的
 * 双层 Option 相反——这里是唯一转换点）。
 */
export function buildBatchProps(
  history: HistoryChoice,
  deadband: DeadbandChoice,
  deadbandValue: number,
): { history?: boolean; deadband?: number | null } {
  const props: { history?: boolean; deadband?: number | null } = {}
  if (history === 'on') props.history = true
  else if (history === 'off') props.history = false
  if (deadband === 'set') props.deadband = deadbandValue
  else if (deadband === 'inherit') props.deadband = null
  return props
}

/** 对账漂移条目（field 为四属性名或"存在性"）。 */
export interface TagDrift {
  key: string
  field: string
  local: string
  remote: string
}

/** 展示值（null 统一显示 'null'，与 wire 值可对照）。 */
function fmt(v: boolean | number | string | null | undefined): string {
  return v === null || v === undefined ? 'null' : String(v)
}

/** 可空字段比较（undefined 防御性视同 null——后端 serde 恒输出，双保险）。 */
function eqNullable(a: number | string | null | undefined, b: number | string | null | undefined): boolean {
  return (a ?? null) === (b ?? null)
}

/**
 * 本地 effective 平铺 vs `GET /api/v1/tags` 后端视图按 key（`<devid>.<tag>`）
 * 对 history/deadband/unit/desc 四属性对账；返回漂移条目（空 = 全一致）。
 * 键仅存在于一侧记"存在性"漂移。
 */
export function reconcileTags(
  local: Record<string, EffectiveProps>,
  remote: Record<string, EffectiveProps>,
): TagDrift[] {
  const out: TagDrift[] = []
  for (const key of Object.keys(local)) {
    const r = remote[key]
    if (!r) {
      out.push({ key, field: '存在性', local: '本地有', remote: '后端无' })
      continue
    }
    const l = local[key]
    if (l.history !== r.history)
      out.push({ key, field: 'history', local: fmt(l.history), remote: fmt(r.history) })
    if (!eqNullable(l.deadband, r.deadband))
      out.push({ key, field: 'deadband', local: fmt(l.deadband), remote: fmt(r.deadband) })
    if (!eqNullable(l.unit, r.unit))
      out.push({ key, field: 'unit', local: fmt(l.unit), remote: fmt(r.unit) })
    if (!eqNullable(l.desc, r.desc))
      out.push({ key, field: 'desc', local: fmt(l.desc), remote: fmt(r.desc) })
  }
  for (const key of Object.keys(remote)) {
    if (!local[key]) out.push({ key, field: '存在性', local: '本地无', remote: '后端有' })
  }
  return out
}
