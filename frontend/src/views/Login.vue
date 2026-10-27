<!-- frontend/src/views/Login.vue -->
<script setup lang="ts">
import { ref } from 'vue'
import { useRouter } from 'vue-router'
import { useAuthStore } from '../stores/auth'
import { api } from '../api/client'
import { PREVIEW_AUTH_TOKEN } from '../lib/previewSession'

const user_id = ref('')
const pass = ref('')
const err = ref('')
/** 登录请求进行中（按钮禁用 + 文案切换，防重复提交）。 */
const logging = ref(false)
const router = useRouter()
const auth = useAuthStore()
const isPreviewAvailable = import.meta.env.DEV

async function login() {
  if (logging.value) return
  if (!user_id.value.trim() || !pass.value) {
    err.value = '请输入用户名和密码'
    return
  }
  logging.value = true
  err.value = ''
  try {
    const r = await api('/api/gw/login', { method: 'POST', body: JSON.stringify({ user_id: user_id.value, pass: pass.value }) })
    const j = await r.json().catch(() => null)
    if (r.ok && j?.token) {
      auth.setAuth(j.token, user_id.value)
      router.push('/overview')
    } else {
      err.value = '登录失败: ' + (j?.status || r.status)
      if (isPreviewAvailable) {
        err.value += '。可使用下方“演示进入”直接预览主页面'
      }
    }
  } catch {
    err.value = '登录请求失败（网络不可达或网关未启动）'
  } finally {
    logging.value = false
  }
}

async function enterPreview() {
  auth.setAuth(PREVIEW_AUTH_TOKEN, 'preview-reviewer')
  err.value = ''
  await router.push('/overview')
}
</script>
<template>
  <div class="login-page">
    <div class="login-hero">
      <p class="eyebrow">Industrial Control Console</p>
      <h1>进入运维驾驶舱</h1>
      <p class="hero-copy">
        面向工业现场的统一监控入口。把实例状态、链路诊断、实时点位与 AI 协作收拢到同一块工作台里。
      </p>
      <div class="hero-grid">
        <div class="hero-stat">
          <span>场景定位</span>
          <strong>实时运维</strong>
        </div>
        <div class="hero-stat">
          <span>设计语言</span>
          <strong>精致工业风</strong>
        </div>
      </div>
    </div>

    <div class="login">
      <p class="eyebrow">RS-IOT PREVIEW</p>
      <h2>rs-iot</h2>
      <p class="copy">登录后进入运维驾驶舱。开发预览环境可直接进入样例界面审阅布局与交互。</p>
      <input v-model="user_id" placeholder="用户名" @keyup.enter="login">
      <input v-model="pass" type="password" placeholder="密码" @keyup.enter="login">
      <button :disabled="logging" @click="login">{{ logging ? '登录中…' : '登录' }}</button>
      <button v-if="isPreviewAvailable" class="preview-button" @click="enterPreview">演示进入</button>
      <p class="err" v-if="err">{{ err }}</p>
    </div>
  </div>
</template>
<style scoped>
.login-page {
  min-height: 100vh;
  display: grid;
  grid-template-columns: minmax(0, 1.15fr) minmax(340px, 420px);
  align-items: center;
  gap: 40px;
  padding: 40px 56px;
}

.login-hero {
  padding: 28px 0;
}

.login-hero h1 {
  margin: 0;
  font-size: clamp(40px, 5vw, 68px);
  line-height: 1.02;
  font-family: "Bahnschrift", "Segoe UI Variable Display", sans-serif;
  letter-spacing: 0.02em;
}

.login {
  display: flex;
  flex-direction: column;
  gap: 12px;
  padding: 28px;
  border-radius: 30px;
  background: var(--panel-bg);
  border: 1px solid var(--panel-border);
  box-shadow: var(--shadow-panel);
  position: relative;
  overflow: hidden;
}
.login::before {
  content: "";
  position: absolute;
  inset: 0;
  background: linear-gradient(140deg, var(--panel-highlight), transparent 46%);
  pointer-events: none;
}
.eyebrow {
  margin: 0;
  font-size: 11px;
  letter-spacing: 0.2em;
  text-transform: uppercase;
  color: var(--color-ink-dim);
}
.hero-copy {
  max-width: 620px;
  margin: 22px 0 0;
  color: var(--color-ink-muted);
  line-height: 1.8;
  font-size: 16px;
}

.hero-grid {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 220px));
  gap: 14px;
  margin-top: 26px;
}

.hero-stat {
  padding: 16px 18px;
  border-radius: 20px;
  background: var(--card-bg);
  border: 1px solid var(--card-border);
  box-shadow: var(--shadow-soft);
}

.hero-stat span {
  display: block;
  margin-bottom: 8px;
  font-size: 11px;
  letter-spacing: 0.16em;
  text-transform: uppercase;
  color: var(--color-ink-dim);
}

.hero-stat strong {
  font-size: 18px;
}
.copy {
  margin: 0 0 8px;
  color: var(--color-ink-dim);
  line-height: 1.6;
  font-size: 13px;
}
input, button { padding: 12px 14px; font-size: 14px; border-radius: 14px; }
input {
  position: relative;
  z-index: 1;
}
button {
  position: relative;
  z-index: 1;
  border: 1px solid transparent;
  background: linear-gradient(135deg, var(--color-primary), var(--color-primary-strong));
  color: var(--shell-theme-active-ink);
  font-weight: 700;
  box-shadow: var(--shadow-soft);
}
button:disabled { opacity: 0.6; cursor: not-allowed; }
.preview-button {
  background: var(--card-bg);
  color: var(--color-ink);
  border-color: var(--chip-border);
  box-shadow: none;
}
.err { color: var(--color-danger); font-size: 13px; }

@media (max-width: 980px) {
  .login-page {
    grid-template-columns: 1fr;
    padding: 28px 20px 40px;
  }

  .hero-grid {
    grid-template-columns: 1fr;
  }
}
</style>
