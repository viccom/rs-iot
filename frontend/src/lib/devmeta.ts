// frontend/src/lib/devmeta.ts
// 设备模型注册表（物模型）前端域类型与纯逻辑——后端语义的 TS 镜像：
// - 三态合并 effective = 模板默认 ∪ 覆盖（deadband 双层：缺省继承/显式关/数值覆盖）；
// - id 字符集校验（与 Rust `model::validate::valid_id` 同口径）；
// - 参数化地址占位符提取（与 Rust `model::addr::placeholders` 同口径）。

/** 标签读写语义（wire 小写 r/rw）。 */
export type TagRw = 'r' | 'rw'

/** 标签模板默认属性（后端 TagProps；owner_inst 运行时记账，模板默认通常空）。 */
export interface TagProps {
  history: boolean
  deadband: number | null
  desc: string | null
  unit: string | null
  owner_inst?: string
}

/** 标签模板（乐高最小对象）。 */
export interface TagTemplate {
  name: string
  data_type: string
  desc: string | null
  unit: string | null
  addr: string | null
  rw: TagRw
  props: TagProps
}

/** 设备模型（物模型）。 */
export interface DeviceModel {
  model_id: string
  vendor: string | null
  model_no: string | null
  category: string | null
  desc: string | null
  params: string[]
  tags: TagTemplate[]
}

/** 实例级标签覆盖（三态：字段缺省/undefined = 继承模板）。 */
export interface TagOverride {
  history?: boolean | null
  /** 双层：undefined = 继承；null = 显式关；数值 = 覆盖。 */
  deadband?: number | null
  /** 空串 = 显式清除；undefined = 继承。 */
  desc?: string | null
  unit?: string | null
}

/** 全局设备实体（devreg 条目）。 */
export interface DeviceEntry {
  devid: string
  model: string
  params: Record<string, string>
  overrides: Record<string, TagOverride>
  desc: string | null
  /** GET /devreg 追加的 effective 摘要（tag → TagProps）。 */
  effective?: Record<string, TagProps>
}

/** id/tag 名合法性：非空、无空白、不含 `. * { } ,`（与后端 valid_id 同口径）。 */
export function isValidId(s: string): boolean {
  return s.length > 0 && !/[\s.*{},]/.test(s)
}

/** 提取地址模板全部占位符名（`{name}`，与后端 placeholders 同口径；坏括号返 null）。 */
export function extractParams(addr: string): string[] | null {
  const out: string[] = []
  let rest = addr
  while (true) {
    const open = rest.indexOf('{')
    if (open < 0) break
    const after = rest.slice(open + 1)
    const close = after.indexOf('}')
    if (close < 0) return null // 未闭合
    const name = after.slice(0, close)
    if (name.length === 0 || name.includes('{')) return null
    out.push(name)
    rest = after.slice(close + 1)
  }
  return out
}

/** 模型表单校验（提交前镜像后端 validate_model；返回错误信息列表，空 = 通过）。 */
export function validateModel(m: DeviceModel): string[] {
  const errs: string[] = []
  if (!isValidId(m.model_id)) errs.push('model_id 非法（非空，不含空白与 . * { } ,）')
  const seen = new Set<string>()
  const declared = new Set(m.params)
  for (const t of m.tags) {
    if (!isValidId(t.name)) errs.push(`tag 名「${t.name || '(空)'}」非法`)
    if (seen.has(t.name)) errs.push(`tag 名「${t.name}」重复`)
    seen.add(t.name)
    if (t.addr) {
      const names = extractParams(t.addr)
      if (!names) errs.push(`tag「${t.name}」地址模板括号非法`)
      else for (const n of names)
        if (!declared.has(n)) errs.push(`tag「${t.name}」地址引用未声明参数 {${n}}`)
    }
  }
  return errs
}

/** 单 tag 三态合并（镜像后端 effective::merge_tag_props）。 */
export function mergeTagProps(tmpl: TagProps, ov: TagOverride | undefined): TagProps {
  if (!ov) return { ...tmpl }
  let deadband = tmpl.deadband
  if (ov.deadband !== undefined) deadband = ov.deadband
  const desc = ov.desc === undefined ? tmpl.desc : ov.desc === '' ? null : ov.desc
  const unit = ov.unit === undefined ? tmpl.unit : ov.unit === '' ? null : ov.unit
  return {
    history: ov.history ?? tmpl.history,
    deadband,
    desc,
    unit,
    owner_inst: tmpl.owner_inst,
  }
}

/** 模板 props 基准：顶层 `unit`/`desc` 为 props 同名字段垫底默认
 *  （镜像后端 effective::effective_tags——建模两处任写一处 effective 均可见）。 */
export function baseProps(t: TagTemplate): TagProps {
  const base = { ...t.props }
  if (base.unit == null) base.unit = t.unit
  if (base.desc == null) base.desc = t.desc
  return base
}

/** 设备表单校验（镜像后端 validate_device_entry：params 齐套 + 覆盖 tag ∈ 模型）。 */
export function validateDevice(dev: DeviceEntry, model: DeviceModel): string[] {
  const errs: string[] = []
  if (!isValidId(dev.devid)) errs.push('devid 非法（非空，不含空白与 . * { } ,）')
  for (const p of model.params)
    if (!(p in dev.params)) errs.push(`缺少模型声明参数 {${p}}`)
  const known = new Set(model.tags.map(t => t.name))
  for (const tag of Object.keys(dev.overrides))
    if (!known.has(tag)) errs.push(`覆盖的 tag「${tag}」不在模型 tags`)
  return errs
}

/** 三态展示辅助：覆盖值 → 展示文本（undefined = 继承占位）。 */
export function triStateText(
  inherited: string,
  ov: string | number | null | undefined,
  empty: string,
): string {
  if (ov === undefined) return inherited
  if (ov === null || ov === '') return empty
  return String(ov)
}
