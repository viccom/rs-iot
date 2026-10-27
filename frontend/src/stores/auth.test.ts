// frontend/src/stores/auth.test.ts
import { describe, it, expect, beforeEach } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'
import { useAuthStore } from './auth'

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
})
