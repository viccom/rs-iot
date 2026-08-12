// frontend/src/api/client.ts
import { useAuthStore } from '../stores/auth'
import { router } from '../router'

/** fetch 封装：自动带 JWT，401 清 token 跳登录。 */
export async function api(path: string, opts: RequestInit = {}) {
  const headers = new Headers(opts.headers)
  if (useAuthStore().token) headers.set('Authorization', `Bearer ${useAuthStore().token}`)
  if (!headers.has('Content-Type') && opts.body) headers.set('Content-Type', 'application/json')
  const r = await fetch(path, { ...opts, headers })
  if (r.status === 401) {
    useAuthStore().clear()
    router.push('/login')
    throw new Error('unauthorized')
  }
  return r
}
