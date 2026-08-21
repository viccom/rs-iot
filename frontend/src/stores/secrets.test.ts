// frontend/src/stores/secrets.test.ts
import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'
import { useSecretsStore } from './secrets'
import { useAuthStore } from './auth'

/** fetch 桩：按 URL 前缀路由响应。client.ts 的 api() 返回原生 Response，但只消费
 *  ok/status/json 面（401 特判走 token 态，测试不触达），桩照此最小面提供。 */
function stubFetch(resps: Record<string, { ok: boolean; status: number; body: unknown }>) {
  const calls: { url: string; init?: RequestInit }[] = []
  vi.stubGlobal('fetch', vi.fn(async (url: string, init?: RequestInit) => {
    calls.push({ url, init })
    const r = Object.entries(resps).find(([k]) => url.startsWith(k))?.[1]
    if (!r) throw new Error('unexpected url: ' + url)
    return { ok: r.ok, status: r.status, json: async () => r.body } as unknown as Response
  }))
  return calls
}

describe('secrets store', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    // 照 monitor.test.ts：node 环境无 localStorage，auth 初始化读、setAuth 写都需桩。
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

  it('load 解析掩码清单', async () => {
    const calls = stubFetch({
      '/api/rsiot/api/v1/secret/list': {
        ok: true, status: 200,
        body: [{ ref: 'secret:onenet_key', masked: 'sk-****key' }],
      },
    })
    const s = useSecretsStore()
    await s.load()
    expect(s.items).toEqual([{ ref: 'secret:onenet_key', masked: 'sk-****key' }])
    expect(s.error).toBe('')
    expect(calls[0].url).toBe('/api/rsiot/api/v1/secret/list')
  })

  it('load 失败置 error 不抛', async () => {
    stubFetch({
      '/api/rsiot/api/v1/secret/list': { ok: false, status: 500, body: null },
    })
    const s = useSecretsStore()
    await expect(s.load()).resolves.toBeUndefined()
    expect(s.error).toContain('500')
    expect(s.items).toEqual([])
  })

  it('set 发 POST {ref,value}', async () => {
    // set 成功后重拉 list（写后刷新），桩需同时覆盖两个端点。
    const calls = stubFetch({
      '/api/rsiot/api/v1/secret/set': { ok: true, status: 200, body: { status: 'ok' } },
      '/api/rsiot/api/v1/secret/list': { ok: true, status: 200, body: [] },
    })
    const s = useSecretsStore()
    await expect(s.set('secret:onenet_key', 'plain-value')).resolves.toBe(true)
    const call = calls.find(c => c.url.includes('/secret/set'))
    expect(call).toBeTruthy()
    expect(call!.init!.method).toBe('POST')
    expect(JSON.parse(String(call!.init!.body))).toEqual({ ref: 'secret:onenet_key', value: 'plain-value' })
  })

  it('remove 发 DELETE secret?ref=', async () => {
    const ref = 'secret:onenet_key'
    const calls = stubFetch({
      '/api/rsiot/api/v1/secret?ref=': { ok: true, status: 200, body: { status: 'ok' } },
      '/api/rsiot/api/v1/secret/list': { ok: true, status: 200, body: [] },
    })
    const s = useSecretsStore()
    await expect(s.remove(ref)).resolves.toBe(true)
    const call = calls.find(c => c.url.includes('/secret?ref='))
    expect(call).toBeTruthy()
    expect(call!.url).toContain(encodeURIComponent(ref))
    expect(call!.init!.method).toBe('DELETE')
  })
})
