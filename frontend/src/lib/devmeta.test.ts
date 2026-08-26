// frontend/src/lib/devmeta.test.ts
import { describe, expect, it } from 'vitest'
import {
  extractParams,
  isValidId,
  baseProps,
  mergeTagProps,
  triStateText,
  validateDevice,
  validateModel,
  type TagTemplate,
  type DeviceEntry,
  type DeviceModel,
} from './devmeta'

function model(): DeviceModel {
  return {
    model_id: 'vx_dtz341',
    vendor: null,
    model_no: null,
    category: null,
    desc: null,
    params: ['base'],
    tags: [
      {
        name: 'ua',
        data_type: 'f32',
        desc: 'A相电压',
        unit: 'V',
        addr: 'holding:{base}:f32',
        rw: 'r',
        props: { history: true, deadband: 0.5, desc: null, unit: 'V' },
      },
    ],
  }
}

describe('isValidId（与后端 valid_id 同口径）', () => {
  it('合法：常规/下划线/中文', () => {
    expect(isValidId('vx_dtz341')).toBe(true)
    expect(isValidId('_sys_rsiot')).toBe(true)
    expect(isValidId('总加')).toBe(true)
  })
  it('非法：空/空白/点/星/花括号/逗号', () => {
    expect(isValidId('')).toBe(false)
    expect(isValidId('a b')).toBe(false)
    expect(isValidId('a.b')).toBe(false)
    expect(isValidId('a*b')).toBe(false)
    expect(isValidId('{a}')).toBe(false)
    expect(isValidId('a,b')).toBe(false)
  })
})

describe('extractParams（占位符提取）', () => {
  it('多占位符保序，含偏移语法不干扰', () => {
    expect(extractParams('DB{db}.DBD{base}+4 x{db}')).toEqual(['db', 'base', 'db'])
    expect(extractParams('plain')).toEqual([])
  })
  it('坏括号返 null', () => {
    expect(extractParams('{unclosed')).toBeNull()
    expect(extractParams('{}')).toBeNull()
    expect(extractParams('{a{b}')).toBeNull()
  })
})

describe('validateModel（提交前镜像后端校验）', () => {
  it('合法模型零错误', () => expect(validateModel(model())).toEqual([]))
  it('非法 model_id / 重复 tag / 未声明参数均报', () => {
    const m = model()
    m.model_id = 'bad id'
    m.tags.push({ ...m.tags[0] })
    m.tags[1].addr = 'holding:{other}:f32'
    const errs = validateModel(m)
    expect(errs.some(e => e.includes('model_id'))).toBe(true)
    expect(errs.some(e => e.includes('重复'))).toBe(true)
    expect(errs.some(e => e.includes('未声明参数 {other}'))).toBe(true)
  })
})

describe('mergeTagProps（三态合并镜像后端）', () => {
  const tmpl = { history: true, deadband: 0.5, desc: '模板', unit: 'kPa' } as const
  const base = { history: true, deadband: 0.5, desc: '模板' as string | null, unit: 'kPa' as string | null }

  it('无覆盖：全继承', () => {
    expect(mergeTagProps(base, undefined)).toEqual({ ...base })
  })
  it('deadband 三态：继承 / 显式关(null) / 数值覆盖', () => {
    expect(mergeTagProps(base, {}).deadband).toBe(0.5)
    expect(mergeTagProps(base, { deadband: null }).deadband).toBeNull()
    expect(mergeTagProps(base, { deadband: 2 }).deadband).toBe(2)
  })
  it('desc/unit：undefined 继承、空串清除、值覆盖；history 布尔覆盖', () => {
    const m = mergeTagProps(base, { desc: '', unit: 'MPa', history: false })
    expect(m.desc).toBeNull()
    expect(m.unit).toBe('MPa')
    expect(m.history).toBe(false)
  })
  it('desc/unit 的 null=继承（后端 Option<String> None 语义）——仅空串=显式清除', () => {
    // 后端 GET /devreg 对未覆盖的 desc/unit 序列化为 JSON null（=继承模板），
    // 仅空串 Some("") 才是显式清除——null 误判清除会造成对账假漂移+单位列丢失
    // （2026-08-26 浏览器嵌入实测发现）。deadband 的 null 才是显式关（内层 Option）。
    const m = mergeTagProps(base, { desc: null, unit: null })
    expect(m.desc).toBe('模板')
    expect(m.unit).toBe('kPa')
  })
})

describe('validateDevice（params 齐套 + 覆盖 tag 白名单）', () => {
  it('齐套且覆盖合法 tag 零错误', () => {
    const dev: DeviceEntry = {
      devid: 'd1',
      model: 'vx_dtz341',
      params: { base: '100' },
      overrides: { ua: { deadband: 2 } },
      desc: null,
    }
    expect(validateDevice(dev, model())).toEqual([])
  })
  it('缺参 / 未知覆盖 / 非法 devid 均报', () => {
    const dev: DeviceEntry = {
      devid: 'd 1',
      model: 'vx_dtz341',
      params: {},
      overrides: { nope: {} },
      desc: null,
    }
    const errs = validateDevice(dev, model())
    expect(errs.some(e => e.includes('devid'))).toBe(true)
    expect(errs.some(e => e.includes('{base}'))).toBe(true)
    expect(errs.some(e => e.includes('nope'))).toBe(true)
  })
})

describe('triStateText（三态展示）', () => {
  it('undefined=继承 / null=显式关 / 值=覆盖', () => {
    expect(triStateText('继承值', undefined, '—')).toBe('继承值')
    expect(triStateText('继承值', null, '(已关)')).toBe('(已关)')
    expect(triStateText('继承值', 2.5, '—')).toBe('2.5')
    expect(triStateText('继承值', '', '(已清)')).toBe('(已清)')
  })
})

describe('baseProps（模板顶层 unit/desc 垫底，镜像后端 effective_tags）', () => {
  const tmpl = (topUnit: string | null, propsUnit: string | null): TagTemplate => ({
    name: 'ua', data_type: 'f32', desc: '顶层描述', unit: topUnit,
    addr: null, rw: 'r',
    props: { history: false, deadband: null, desc: null, unit: propsUnit },
  })
  it('props 有值优先，顶层垫底', () => {
    expect(baseProps(tmpl('V', 'props 单位')).unit).toBe('props 单位')
    expect(baseProps(tmpl('V', null)).unit).toBe('V')
    expect(baseProps(tmpl(null, null)).unit).toBeNull()
  })
  it('desc 同款垫底', () => {
    expect(baseProps(tmpl(null, null)).desc).toBe('顶层描述')
  })
})
