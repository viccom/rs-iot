// frontend/src/stores/auth.ts
import { defineStore } from 'pinia'
import { ref } from 'vue'

// 统一会话键（与 hmi-studio 共享）：同源两 SPA（/ 控制台、/hmi 组态）一次登录互通。
// webhmi_token 是 JWT 镜像——权威凭证是 login 下发的 HttpOnly cookie（浏览器对
// 同源 fetch 与 EventSource 自动携带），镜像只做守卫判据、Bearer 注入与显示。
export const TOKEN_KEY = 'webhmi_token'
export const USER_KEY = 'webhmi_user'

// 旧键一次性迁移（rsiot_* / hmi_* → webhmi_*）：已登录用户无感升级，跨 SPA 互通。
function migrateLegacyKeys() {
  if (!localStorage.getItem(TOKEN_KEY)) {
    const legacy = localStorage.getItem('rsiot_token') ?? localStorage.getItem('hmi_token')
    if (legacy) localStorage.setItem(TOKEN_KEY, legacy)
  }
  if (!localStorage.getItem(USER_KEY)) {
    const legacy = localStorage.getItem('rsiot_user') ?? localStorage.getItem('hmi_user')
    if (legacy) localStorage.setItem(USER_KEY, legacy)
  }
  localStorage.removeItem('rsiot_token')
  localStorage.removeItem('rsiot_user')
  localStorage.removeItem('hmi_token')
  localStorage.removeItem('hmi_user')
}

// 旧会话自愈：镜像 token 在而 cookie 缺失（旧 gateway 时代登录、Set-Cookie 被丢弃
// 的会话）——凭 Bearer 换发 cookie（POST /user/session）。SSE 靠 cookie 鉴权，缺它
// 会 401 重连循环；本调用异步尽力而为，进行中的 EventSource 重连数秒内自愈。
// 裸 fetch（不走 apiFetch）：模块加载早于 store 初始化，且不引入 fetch.ts 循环。
export function bootstrapCookieSession() {
  const t = localStorage.getItem(TOKEN_KEY)
  if (!t) return
  void fetch('/api/rsiot/api/v1/user/session', {
    method: 'POST',
    headers: { Authorization: `Bearer ${t}` },
  }).catch(() => {})
}

// webhmi_user 有两种写入方：控制台存纯显示名（user_id），hmi-studio 存 JSON
// 对象（{user_id, username, ...}）——统一键后互踩。读取时归一为显示名。
function readUserDisplay(raw: string | null): string {
  if (!raw) return ''
  if (raw.startsWith('{')) {
    try {
      const o = JSON.parse(raw) as { user_id?: string; username?: string }
      return o.username ?? o.user_id ?? ''
    } catch {
      return raw
    }
  }
  return raw
}

export const useAuthStore = defineStore('auth', () => {
  migrateLegacyKeys()
  const token = ref(localStorage.getItem(TOKEN_KEY) || '')
  const user = ref(readUserDisplay(localStorage.getItem(USER_KEY)))

  function setAuth(t: string, u: string) {
    token.value = t
    user.value = u
    localStorage.setItem(TOKEN_KEY, t)
    localStorage.setItem(USER_KEY, u)
  }

  function clear() {
    token.value = ''
    user.value = ''
    localStorage.removeItem(TOKEN_KEY)
    localStorage.removeItem(USER_KEY)
  }

  // 登出（用户主动）：先请服务端清 HttpOnly cookie 再清镜像。网络失败不阻断
  // 本地清理（尽力而为）；401 失效路径仍走 clear()（凭证已死，登出端点必 401）。
  async function logout() {
    try {
      await fetch('/api/rsiot/api/v1/user/logout', {
        method: 'POST',
        headers: token.value ? { Authorization: `Bearer ${token.value}` } : {},
      })
    } catch { /* 服务端尽力而为 */ }
    clear()
  }

  return { token, user, setAuth, clear, logout }
})
