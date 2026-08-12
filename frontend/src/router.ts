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

router.beforeEach((to) => {
  if (to.meta.auth && !useAuthStore().token) return '/login'
})
