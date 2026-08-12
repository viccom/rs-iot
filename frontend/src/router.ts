// frontend/src/router.ts
import { createRouter, createWebHistory } from 'vue-router'
import { useAuthStore } from './stores/auth'

export const router = createRouter({
  history: createWebHistory(),
  routes: [
    { path: '/login', component: () => import('./views/Login.vue') },
    { path: '/', redirect: '/scada' },
    { path: '/scada', component: () => import('./views/Monitor.vue'), meta: { auth: true } },
  ],
})

/**
 * 路由守卫：
 * - 鉴权：受保护路由无 token → 跳 /login。
 * - SSE 防僵尸：跳向 /login（登出 / 401 触发）时关闭 monitor + agent 的 SSE。
 *   EventSource 不随 SPA 导航自动关闭，会持续重连刷 401（token 已清）。
 *   动态 import 避免与 store 的静态 import 循环依赖（store 反向依赖 router 经 client.ts）。
 */
router.beforeEach(async (to) => {
  if (to.meta.auth && !useAuthStore().token) return '/login'
  if (to.path === '/login') {
    const { useMonitorStore } = await import('./stores/monitor')
    const { useAgentStore } = await import('./stores/agent')
    useMonitorStore().stop()
    useAgentStore().stop()
  }
})
