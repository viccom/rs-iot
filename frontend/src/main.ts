// frontend/src/main.ts
import { createApp } from 'vue'
import { createPinia } from 'pinia'
import App from './App.vue'
import { router } from './router'
import './styles/main.css'
// incremark markdown 渲染主题（自带 CSS 变量 + .incremark-* 前缀类；深色经
// [data-theme="dark"] 适配，与 applyTheme 设置的 documentElement dataset 对齐）
import '@incremark/theme/styles.css'
import { applyTheme, readStoredTheme } from './lib/theme'

applyTheme(readStoredTheme())

createApp(App).use(createPinia()).use(router).mount('#app')
