// frontend/src/stores/secrets.ts
import { defineStore } from 'pinia'
import { ref } from 'vue'
import { useAuthStore } from './auth'

/** secret/list 掩码条目（后端 routes_secret.rs::list_masked——永不含明文）。 */
export interface SecretItem { ref: string; masked: string }

/**
 * 平台凭证管理 store（阶段 B Q4 的前端面）。契约：GET secret/list → [{ref,masked}]；
 * POST secret/set {ref,value}；DELETE secret?ref=（写/删服务端有审计日志，ref 级不落值）。
 *
 * 刻意绕过 api/client.ts 的 `api()` 走裸 fetch + Bearer（照 monitor.ts 先例，见其
 * postJson 注释）：client.ts `import { router }` 链在 vitest node 环境炸
 * （createWebHistory 读 window）；401 语义（清 token + 跳登录）在本 store 内等价兜底。
 */
export const useSecretsStore = defineStore('secrets', () => {
  const items = ref<SecretItem[]>([])
  const loading = ref(false)
  const error = ref('')

  /** 401 会话失效：清 token + 硬跳登录（照 monitor.ts handleUnauthorized，不 import router）。 */
  function handleUnauthorized() {
    const auth = useAuthStore()
    if (!auth.token) return
    auth.clear()
    if (typeof window !== 'undefined') window.location.href = '/login'
  }

  async function load() {
    loading.value = true; error.value = ''
    try {
      const r = await fetch('/api/rsiot/api/v1/secret/list', {
        headers: { Authorization: `Bearer ${useAuthStore().token}` },
      })
      if (r.status === 401) { handleUnauthorized(); error.value = '登录已过期，请重新登录'; return }
      if (!r.ok) { error.value = `加载失败 HTTP ${r.status}`; return }
      items.value = (await r.json()) as SecretItem[]
    } catch (e) { error.value = e instanceof Error ? e.message : '加载失败' }
    finally { loading.value = false }
  }

  async function set(ref: string, value: string): Promise<boolean> {
    error.value = ''
    try {
      const r = await fetch('/api/rsiot/api/v1/secret/set', {
        method: 'POST',
        headers: {
          Authorization: `Bearer ${useAuthStore().token}`,
          'Content-Type': 'application/json',
        },
        body: JSON.stringify({ ref, value }),
      })
      if (r.status === 401) { handleUnauthorized(); return false }
      if (!r.ok) { error.value = `写入失败 HTTP ${r.status}`; return false }
      await load(); return true
    } catch (e) { error.value = e instanceof Error ? e.message : '写入失败'; return false }
  }

  async function remove(ref: string): Promise<boolean> {
    error.value = ''
    try {
      const r = await fetch('/api/rsiot/api/v1/secret?ref=' + encodeURIComponent(ref), {
        method: 'DELETE',
        headers: { Authorization: `Bearer ${useAuthStore().token}` },
      })
      if (r.status === 401) { handleUnauthorized(); return false }
      if (!r.ok) { error.value = `删除失败 HTTP ${r.status}`; return false }
      await load(); return true
    } catch (e) { error.value = e instanceof Error ? e.message : '删除失败'; return false }
  }

  return { items, loading, error, load, set, remove }
})
