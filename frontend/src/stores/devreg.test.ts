// frontend/src/stores/devreg.test.ts
import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'
import { useDevregStore } from './devreg'
import { useAuthStore } from './auth'
import type { DeviceModel } from '../lib/devmeta'

/** fetch 桩（secrets.test.ts 模式；text() 面给 4xx/5xx 路径）。 */
function stubFetch(resps: Record<string, { ok: boolean; status: number; body?: unknown }>) {
  const calls: { url: string; init?: RequestInit }[] = []
  vi.stubGlobal('fetch', vi.fn(async (url: string, init?: RequestInit) => {
    calls.push({ url, init })
    const r = Object.entries(resps).find(([k]) => url.startsWith(k))?.[1]
    if (!r) throw new Error('unexpected url: ' + url)
    return {
      ok: r.ok,
      status: r.status,
      json: async () => r.body,
      text: async () => String(r.body ?? ''),
    } as unknown as Response
  }))
  return calls
}

const model: DeviceModel = {
  model_id: 'm1',
  vendor: null,
  model_no: null,
  category: null,
  desc: null,
  params: ['base'],
  tags: [{
    name: 'ua', data_type: 'f32', desc: null, unit: null,
    addr: 'holding:{base}:f32', rw: 'r',
    props: { history: true, deadband: null, desc: null, unit: null },
  }],
}

describe('devreg store', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    const store: Record<string, string> = {}
    ;(globalThis as { localStorage?: Storage }).localStorage = {
      getItem: (k: string) => store[k] ?? null,
      setItem: (k: string, v: string) => { store[k] = v },
      removeItem: (k: string) => { delete store[k] },
      clear: () => { for (const k of Object.keys(store)) delete store[k] },
      key: (i: number) => Object.keys(store)[i] ?? null,
      get length() { return Object.keys(store).length },
    } as Storage
    useAuthStore().setAuth('tok', 'admin')
  })
  afterEach(() => vi.unstubAllGlobals())

  it('loadAll 拉双表', async () => {
    stubFetch({
      '/api/rsiot/api/v1/devmodels': { ok: true, status: 200, body: { m1: model } },
      '/api/rsiot/api/v1/devreg': {
        ok: true, status: 200,
        body: { d1: { devid: 'd1', model: 'm1', params: {}, overrides: {}, desc: null } },
      },
    })
    const s = useDevregStore()
    await s.loadAll()
    expect(Object.keys(s.models)).toEqual(['m1'])
    expect(Object.keys(s.devices)).toEqual(['d1'])
    expect(s.loading).toBe(false)
  })

  it('createModel 409 报重名且不刷新', async () => {
    const calls = stubFetch({
      '/api/rsiot/api/v1/devmodel/set': { ok: false, status: 409 },
      '/api/rsiot/api/v1/devmodels': { ok: true, status: 200, body: {} },
      '/api/rsiot/api/v1/devreg': { ok: true, status: 200, body: {} },
    })
    const s = useDevregStore()
    const ok = await s.createModel(model)
    expect(ok).toBe(false)
    expect(s.error).toContain('已存在')
    // 失败路径不发 loadAll（仅 1 次写调用）
    expect(calls.filter(c => c.url.includes('devmodel/set'))).toHaveLength(1)
  })

  it('updateModel 409 带冲突明细', async () => {
    stubFetch({
      '/api/rsiot/api/v1/devmodel/update': {
        ok: false, status: 409,
        body: { status: 'conflict', conflicts: [{ devid: 'd1', reason: '缺参数 {extra}' }] },
      },
    })
    const s = useDevregStore()
    expect(await s.updateModel(model)).toBe(false)
    expect(s.error).toContain('d1')
    expect(s.error).toContain('{extra}')
  })

  it('deleteModel 409 带引用列表', async () => {
    stubFetch({
      '/api/rsiot/api/v1/devmodel/delete': {
        ok: false, status: 409,
        body: { status: 'conflict', referenced_by: ['dA', 'dB'] },
      },
    })
    const s = useDevregStore()
    expect(await s.deleteModel('m1')).toBe(false)
    expect(s.error).toContain('dA, dB')
  })

  it('saveDevice 400 带后端校验文案', async () => {
    stubFetch({
      '/api/rsiot/api/v1/devreg/set': {
        ok: false, status: 400,
        body: 'model error: 设备 d1 引用的模型 "m1" 不存在',
      },
    })
    const s = useDevregStore()
    expect(await s.saveDevice({ devid: 'd1', model: 'm1', params: {}, overrides: {}, desc: null })).toBe(false)
    expect(s.error).toContain('不存在')
    s.clearError()
    expect(s.error).toBe('')
  })

  it('migrateInline 返回计数摘要', async () => {
    stubFetch({
      '/api/rsiot/api/v1/migrate/inline-devices': {
        ok: true, status: 200,
        body: { converted_models: 3, converted_devices: 3, converted_instances: 2, failures: [] },
      },
    })
    const s = useDevregStore()
    const r = await s.migrateInline()
    expect(r).toEqual({ converted_models: 3, converted_devices: 3, converted_instances: 2, failures: [] })
  })
})

describe('devreg store 发现队列（M-4）', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    const store: Record<string, string> = {}
    ;(globalThis as { localStorage?: Storage }).localStorage = {
      getItem: (k: string) => store[k] ?? null,
      setItem: (k: string, v: string) => { store[k] = v },
      removeItem: (k: string) => { delete store[k] },
      clear: () => { for (const k of Object.keys(store)) delete store[k] },
      key: (i: number) => Object.keys(store)[i] ?? null,
      get length() { return Object.keys(store).length },
    } as Storage
    useAuthStore().setAuth('tok', 'admin')
  })
  afterEach(() => vi.unstubAllGlobals())

  it('loadDiscoveries 拉队列（数组载荷）', async () => {
    stubFetch({
      '/api/rsiot/api/v1/discoveries': {
        ok: true, status: 200,
        body: [{ devid: 'd1', tag: 'ghost', first_ts: 1, last_ts: 2, count: 3, sample_value: '42.5' }],
      },
    })
    const s = useDevregStore()
    await s.loadDiscoveries()
    expect(s.discoveries).toHaveLength(1)
    expect(s.discoveries[0].tag).toBe('ghost')
  })

  it('clearDiscoveries 带 dev_id 作用域', async () => {
    const calls = stubFetch({
      '/api/rsiot/api/v1/discovery/clear': { ok: true, status: 200, body: { status: 'ok', cleared: 1 } },
      '/api/rsiot/api/v1/discoveries': { ok: true, status: 200, body: [] },
    })
    const s = useDevregStore()
    const ok = await s.clearDiscoveries('d1')
    expect(ok).toBe(true)
    const clear = calls.find(c => c.url.includes('/discovery/clear'))
    expect(JSON.parse(String(clear?.init?.body))).toEqual({ dev_id: 'd1' })
    expect(s.discoveries).toHaveLength(0)
  })

  it('adopt 成功刷新双表与队列 / 404 报目标模型缺失', async () => {
    const calls = stubFetch({
      '/api/rsiot/api/v1/discovery/adopt': { ok: true, status: 200, body: { status: 'ok', merged: 1, swept: 2 } },
      '/api/rsiot/api/v1/devmodels': { ok: true, status: 200, body: {} },
      '/api/rsiot/api/v1/devreg': { ok: true, status: 200, body: {} },
      '/api/rsiot/api/v1/discoveries': { ok: true, status: 200, body: [] },
    })
    const s = useDevregStore()
    const r = await s.adopt('m1', [{ name: 'ghost', data_type: 'f32' }])
    expect(r).toEqual({ merged: 1, swept: 2 })
    const adoptCall = calls.find(c => c.url.includes('/discovery/adopt'))
    expect(adoptCall?.init?.method).toBe('POST')
    expect(JSON.parse(String(adoptCall?.init?.body))).toEqual({
      model_id: 'm1', tags: [{ name: 'ghost', data_type: 'f32' }],
    })

    vi.stubGlobal('fetch', vi.fn(async () => ({
      ok: false, status: 404,
      json: async () => ({}),
      text: async () => 'model not found',
    }) as unknown as Response))
    const r404 = await s.adopt('nope', [{ name: 'x', data_type: 'f32' }])
    expect(r404).toBeNull()
    expect(s.error).toContain('nope')
  })
})

describe('devreg store 标签运营（D2：tag/batch + effective 视图）', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    const store: Record<string, string> = {}
    ;(globalThis as { localStorage?: Storage }).localStorage = {
      getItem: (k: string) => store[k] ?? null,
      setItem: (k: string, v: string) => { store[k] = v },
      removeItem: (k: string) => { delete store[k] },
      clear: () => { for (const k of Object.keys(store)) delete store[k] },
      key: (i: number) => Object.keys(store)[i] ?? null,
      get length() { return Object.keys(store).length },
    } as Storage
    useAuthStore().setAuth('tok', 'admin')
  })
  afterEach(() => vi.unstubAllGlobals())

  it('tagBatch 提交 glob + 三态 props（不变字段不携带；null=恢复继承）并刷新双表', async () => {
    const calls = stubFetch({
      '/api/rsiot/api/v1/tag/batch': { ok: true, status: 200, body: { status: 'ok', matched: 3, updated: 2 } },
      '/api/rsiot/api/v1/devmodels': { ok: true, status: 200, body: {} },
      '/api/rsiot/api/v1/devreg': { ok: true, status: 200, body: {} },
    })
    const s = useDevregStore()
    const r = await s.tagBatch({ pattern: 'line1.*.temp*', history: true, deadband: null })
    expect(r).toEqual({ matched: 3, updated: 2 })
    const call = calls.find(c => c.url.includes('/tag/batch'))
    expect(call?.init?.method).toBe('POST')
    expect(JSON.parse(String(call?.init?.body))).toEqual({
      pattern: 'line1.*.temp*', history: true, deadband: null,
    })
  })

  it('tagBatch 400 带后端文案且不刷新', async () => {
    const calls = stubFetch({
      '/api/rsiot/api/v1/tag/batch': { ok: false, status: 400, body: 'pattern 不能为空' },
    })
    const s = useDevregStore()
    expect(await s.tagBatch({ pattern: '' })).toBeNull()
    expect(s.error).toContain('pattern 不能为空')
    expect(calls).toHaveLength(1) // 失败不发 loadAll
  })

  it('fetchEffectiveTags 拉全量 effective 视图（_source 标注）', async () => {
    stubFetch({
      '/api/rsiot/api/v1/tags': {
        ok: true, status: 200,
        body: { 'd1.ua': { history: true, deadband: null, desc: null, unit: 'V', _source: 'model' } },
      },
    })
    const s = useDevregStore()
    const v = await s.fetchEffectiveTags()
    expect(v?.['d1.ua']?._source).toBe('model')
    expect(v?.['d1.ua']?.unit).toBe('V')
  })
})
