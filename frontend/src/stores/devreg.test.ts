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
