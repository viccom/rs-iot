// frontend/src/stores/auth.test.ts
import { describe, it, expect, beforeEach, vi } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'
import { useAuthStore, bootstrapCookieSession } from './auth'

// jsdom 未装，手工 localStorage 桩（同 monitor.test.ts 模式）。
function stubLocalStorage(initial: Record<string, string> = {}) {
  const store: Record<string, string> = { ...initial }
  ;(globalThis as { localStorage?: Storage }).localStorage = {
    getItem: (k: string) => store[k] ?? null,
    setItem: (k: string, v: string) => { store[k] = v },
    removeItem: (k: string) => { delete store[k] },
    clear: () => { for (const k of Object.keys(store)) delete store[k] },
    key: (i: number) => Object.keys(store)[i] ?? null,
    get length() { return Object.keys(store).length },
  } as Storage
  return store
}

describe('auth store（统一会话键）', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
  })

  it('旧键 rsiot_* 一次性迁移到 webhmi_*，旧键清除', () => {
    const store = stubLocalStorage({ rsiot_token: 'legacy-jwt', rsiot_user: 'admin' })
    const auth = useAuthStore()
    expect(auth.token).toBe('legacy-jwt')
    expect(auth.user).toBe('admin')
    expect(store.webhmi_token).toBe('legacy-jwt')
    expect(store.rsiot_token).toBeUndefined()
  })

  it('跨 SPA 旧键 hmi_* 同样迁移（hmi-studio 登录过的用户直接互通）', () => {
    const store = stubLocalStorage({ hmi_token: 'hmi-jwt', hmi_user: 'op' })
    const auth = useAuthStore()
    expect(auth.token).toBe('hmi-jwt')
    expect(auth.user).toBe('op')
    expect(store.hmi_token).toBeUndefined()
  })

  it('webhmi_* 已存在时不被旧键覆盖', () => {
    const store = stubLocalStorage({ webhmi_token: 'new', rsiot_token: 'old' })
    const auth = useAuthStore()
    expect(auth.token).toBe('new')
    expect(store.rsiot_token).toBeUndefined()
  })

  it('clear 只清统一键', () => {
    const store = stubLocalStorage({ webhmi_token: 't', webhmi_user: 'u' })
    const auth = useAuthStore()
    auth.clear()
    expect(auth.token).toBe('')
    expect(store.webhmi_token).toBeUndefined()
    expect(store.webhmi_user).toBeUndefined()
  })

  it('webhmi_user 存的是 hmi-studio 的 JSON 对象时归一为显示名（username 优先）', () => {
    stubLocalStorage({ webhmi_token: 't', webhmi_user: '{"user_id":"admin","username":"管理员"}' })
    expect(useAuthStore().user).toBe('管理员')
  })

  it('JSON 对象无 username 时回落 user_id', () => {
    stubLocalStorage({ webhmi_token: 't', webhmi_user: '{"user_id":"op"}' })
    expect(useAuthStore().user).toBe('op')
  })

  it('纯字符串 user 原样透传（控制台自身写入格式）', () => {
    stubLocalStorage({ webhmi_token: 't', webhmi_user: 'plain-name' })
    expect(useAuthStore().user).toBe('plain-name')
  })

  it('bootstrapCookieSession：有镜像才带 Bearer 调 /user/session（旧会话补 cookie）', () => {
    stubLocalStorage({ webhmi_token: 'mirror-jwt' })
    const fetchMock = vi.fn(async (_url: string, _init?: RequestInit) => new Response('{"status":"ok"}'))
    vi.stubGlobal('fetch', fetchMock)
    bootstrapCookieSession()
    expect(fetchMock).toHaveBeenCalledTimes(1)
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit | undefined]
    expect(url).toBe('/api/rsiot/api/v1/user/session')
    expect(init?.headers).toMatchObject({ Authorization: 'Bearer mirror-jwt' })

    // 无镜像不调用
    const store = (globalThis as { localStorage?: Storage }).localStorage as Storage
    store.removeItem('webhmi_token')
    fetchMock.mockClear()
    bootstrapCookieSession()
    expect(fetchMock).not.toHaveBeenCalled()
    vi.unstubAllGlobals()
  })
})
