<!-- frontend/src/views/Login.vue -->
<script setup lang="ts">
import { ref } from 'vue'
import { useRouter } from 'vue-router'
import { useAuthStore } from '../stores/auth'
import { api } from '../api/client'

const user_id = ref('admin')
const pass = ref('admin')
const err = ref('')
const router = useRouter()
const auth = useAuthStore()

async function login() {
  err.value = ''
  const r = await api('/api/gw/login', { method: 'POST', body: JSON.stringify({ user_id: user_id.value, pass: pass.value }) })
  const j = await r.json().catch(() => null)
  if (r.ok && j?.token) {
    auth.setAuth(j.token, user_id.value)
    router.push('/scada')
  } else {
    err.value = '登录失败: ' + (j?.status || r.status)
  }
}
</script>
<template>
  <div class="login">
    <h2>rs-iot</h2>
    <input v-model="user_id" placeholder="用户名">
    <input v-model="pass" type="password" placeholder="密码" @keyup.enter="login">
    <button @click="login">登录</button>
    <p class="err" v-if="err">{{ err }}</p>
  </div>
</template>
<style scoped>
.login { max-width: 300px; margin: 80px auto; display: flex; flex-direction: column; gap: 8px; }
input, button { padding: 8px; font-size: 14px; }
.err { color: #dc2626; font-size: 13px; }
</style>
