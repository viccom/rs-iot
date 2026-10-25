// frontend/src/lib/tagops.test.ts
import { describe, expect, it } from 'vitest'
import {
  buildBatchProps,
  globMatch,
  reconcileTags,
  type EffectiveProps,
} from './tagops'

describe('globMatch（镜像后端 discovery::glob_match，仅 `*` 通配）', () => {
  it('全匹配 `*`（含空键边界）', () => {
    expect(globMatch('*', 'd1.temp')).toBe(true)
    expect(globMatch('*', '')).toBe(true)
  })
  it('前缀 `furnace*`', () => {
    expect(globMatch('furnace*', 'furnace01.temp')).toBe(true)
    expect(globMatch('furnace*', 'boiler01.temp')).toBe(false)
  })
  it('中缀与多星：`line1.*.temp*` / `f*.t*`', () => {
    expect(globMatch('line1.*.temp*', 'line1.zone2.temp_max')).toBe(true)
    expect(globMatch('line1.*.temp*', 'line1.zone2.hum')).toBe(false)
    expect(globMatch('f*.t*', 'f01.t')).toBe(true)
    expect(globMatch('a**b', 'axxb')).toBe(true) // 连续星等价单星
  })
  it('无星 = 精确等值', () => {
    expect(globMatch('d1.temp', 'd1.temp')).toBe(true)
    expect(globMatch('d1.temp', 'd1.temp2')).toBe(false)
  })
  it('问号不支持——按字面量匹配', () => {
    expect(globMatch('d?.temp', 'd1.temp')).toBe(false)
    expect(globMatch('d?.temp', 'd?.temp')).toBe(true)
  })
  it('零匹配：首段非前缀 / 末段非后缀 / 中段缺失', () => {
    expect(globMatch('a*b', 'xa')).toBe(false)      // 首段非前缀
    expect(globMatch('a*b', 'axxba')).toBe(false)   // 末段非后缀
    expect(globMatch('a*b*c', 'axb')).toBe(false)   // 中段缺失
  })
})

describe('buildBatchProps（tag 端口三态转换：null=恢复继承，与 devreg 口径相反）', () => {
  it('全不变 → 空对象（字段不携带 = 不动现有覆盖）', () => {
    expect(buildBatchProps('keep', 'keep', 0.5)).toEqual({})
  })
  it('history 开/关 → true/false', () => {
    expect(buildBatchProps('on', 'keep', 0)).toEqual({ history: true })
    expect(buildBatchProps('off', 'keep', 0)).toEqual({ history: false })
  })
  it('deadband 设值 → 数值；恢复继承 → null', () => {
    expect(buildBatchProps('keep', 'set', 0.5)).toEqual({ deadband: 0.5 })
    expect(buildBatchProps('keep', 'inherit', 0)).toEqual({ deadband: null })
  })
  it('两属性同改合并携带', () => {
    expect(buildBatchProps('on', 'set', 2)).toEqual({ history: true, deadband: 2 })
  })
})

describe('reconcileTags（本地平铺 vs 后端 effective 四属性对账）', () => {
  const base: EffectiveProps = { history: true, deadband: 0.5, unit: 'V', desc: null }
  it('全一致 → 空列表', () => {
    expect(reconcileTags({ 'd1.temp': base }, { 'd1.temp': { ...base } })).toEqual([])
  })
  it('四属性漂移逐项报（键+属性+两侧值）', () => {
    const drifts = reconcileTags(
      { 'd1.temp': base },
      { 'd1.temp': { history: false, deadband: null, unit: 'kPa', desc: 'x' } },
    )
    expect(drifts.map(d => d.field)).toEqual(['history', 'deadband', 'unit', 'desc'])
    expect(drifts[0]).toEqual({ key: 'd1.temp', field: 'history', local: 'true', remote: 'false' })
    expect(drifts[1]).toEqual({ key: 'd1.temp', field: 'deadband', local: '0.5', remote: 'null' })
  })
  it('键存在性漂移（仅本地 / 仅后端）', () => {
    const drifts = reconcileTags(
      { 'd1.temp': base, 'd2.temp': base },
      { 'd1.temp': { ...base }, 'd3.hum': { ...base } },
    )
    expect(drifts.map(d => [d.key, d.field])).toEqual([
      ['d2.temp', '存在性'],
      ['d3.hum', '存在性'],
    ])
  })
  it('后端缺省字段防御性视同 null（不误报）', () => {
    const remote = { history: true, deadband: 0.5, unit: 'V' } as EffectiveProps // desc 缺省
    expect(reconcileTags({ 'd1.temp': base }, { 'd1.temp': remote })).toEqual([])
  })
})
