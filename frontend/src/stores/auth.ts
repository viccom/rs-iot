// frontend/src/stores/auth.ts
import { defineStore } from 'pinia'
import { ref } from 'vue'

export const useAuthStore = defineStore('auth', () => {
  const token = ref(localStorage.getItem('rsiot_token') || '')
  const user = ref(localStorage.getItem('rsiot_user') || '')

  function setAuth(t: string, u: string) {
    token.value = t
    user.value = u
    localStorage.setItem('rsiot_token', t)
    localStorage.setItem('rsiot_user', u)
  }

  function clear() {
    token.value = ''
    user.value = ''
    localStorage.removeItem('rsiot_token')
    localStorage.removeItem('rsiot_user')
  }

  return { token, user, setAuth, clear }
})
