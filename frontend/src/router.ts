// frontend/src/router.ts
import { createRouter, createWebHistory } from 'vue-router'
import { useAuthStore } from './stores/auth'

export const router = createRouter({
  history: createWebHistory(),
  routes: [
    { path: '/login', component: () => import('./views/Login.vue') },
    { path: '/', redirect: '/overview' },
    {
      path: '/',
      component: () => import('./components/layout/AppShell.vue'),
      meta: { auth: true },
      children: [
        { path: 'overview', name: 'overview', component: () => import('./views/Overview.vue'), meta: { auth: true } },
        { path: 'telemetry', name: 'telemetry', component: () => import('./views/Telemetry.vue'), meta: { auth: true } },
        { path: 'collectors', name: 'collectors', component: () => import('./views/Collectors.vue'), meta: { auth: true } },
        { path: 'diagnostics', name: 'diagnostics', component: () => import('./views/Diagnostics.vue'), meta: { auth: true } },
        { path: 'control', name: 'control', component: () => import('./views/Control.vue'), meta: { auth: true } },
        { path: 'lua', name: 'lua', component: () => import('./views/Lua.vue'), meta: { auth: true } },
      ],
    },
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
    // agent.stop(true)：登出同时取消进行中的 turn（会话留给下个用户前清理）。
    // 关抽屉路径（AgentDrawer 卸载）用无参 stop()——只断 SSE，后台任务继续。
    useAgentStore().stop(true)
  }
})
