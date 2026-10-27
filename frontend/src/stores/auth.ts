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

export const useAuthStore = defineStore('auth', () => {
  migrateLegacyKeys()
  const token = ref(localStorage.getItem(TOKEN_KEY) || '')
  const user = ref(localStorage.getItem(USER_KEY) || '')

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
