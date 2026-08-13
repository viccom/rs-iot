// frontend/src/main.ts
import { createApp } from 'vue'
import { createPinia } from 'pinia'
import App from './App.vue'
import { router } from './router'
import './styles/main.css'
import { applyTheme, readStoredTheme } from './lib/theme'

applyTheme(readStoredTheme())

createApp(App).use(createPinia()).use(router).mount('#app')
