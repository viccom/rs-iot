// frontend/src/api/client.ts
import { useAuthStore } from '../stores/auth'
import { router } from '../router'

/** fetch 封装：自动带 JWT。
 *  401 分两种：已登录态 token 失效（清 token 跳登录）；登录中业务 401（错误密码，返 Response 让调用方显示错误）。 */
export async function api(path: string, opts: RequestInit = {}) {
  const headers = new Headers(opts.headers)
  const auth = useAuthStore()
  if (auth.token) headers.set('Authorization', `Bearer ${auth.token}`)
  if (!headers.has('Content-Type') && opts.body) headers.set('Content-Type', 'application/json')
  const r = await fetch(path, { ...opts, headers })
  // 401 分两种：已登录态 token 失效（清 token 跳登录）；登录中业务 401（错误密码，返 Response 让调用方显示错误）。
  if (r.status === 401 && auth.token) {
    auth.clear()
    router.push('/login')
    throw new Error('unauthorized')
  }
  return r
}
