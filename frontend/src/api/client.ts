// frontend/src/api/client.ts
import { useAuthStore } from '../stores/auth'
import { router } from '../router'
import { apiFetch } from './fetch'

/** fetch 封装：apiFetch（纯头注入，见 fetch.ts）+ 401 导航副作用。
 *  401 分两种：已登录态 token 失效（清 token 跳登录）；登录中业务 401
 *  （错误密码，返 Response 让调用方显示错误）。
 *  组件/视图用本档；stores 用 apiFetch（router 链破坏 vitest，见其注释）。 */
export async function api(path: string, opts: RequestInit = {}) {
  const r = await apiFetch(path, opts)
  const auth = useAuthStore()
  if (r.status === 401 && auth.token) {
    auth.clear()
    router.push('/login')
    throw new Error('unauthorized')
  }
  return r
}
