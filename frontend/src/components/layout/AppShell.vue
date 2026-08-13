<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watchEffect } from 'vue'
import { RouterLink, RouterView, useRoute, useRouter } from 'vue-router'
import AgentDrawer from '../chat/AgentDrawer.vue'
import { PRIMARY_ROUTES } from '../../lib/navigation'
import { useMonitorStore } from '../../stores/monitor'
import { useWorkspaceStore } from '../../stores/workspace'
import { useAuthStore } from '../../stores/auth'
import { applyTheme, persistTheme, readStoredTheme, type ThemeName } from '../../lib/theme'

const route = useRoute()
const router = useRouter()
const auth = useAuthStore()
const monitor = useMonitorStore()
const workspace = useWorkspaceStore()
const drawerOpen = ref(false)
const currentTheme = ref<ThemeName>(readStoredTheme())
const clockText = ref('--:--:--')
const dateText = ref('----/--/--')

const currentRoute = computed(() =>
  PRIMARY_ROUTES.find(item => item.key === route.name) ?? PRIMARY_ROUTES[0],
)
const runningCollectors = computed(() =>
  monitor.instances.filter(instance => instance.state === 'running').length,
)
const degradedCollectors = computed(() =>
  monitor.instances.filter(instance => instance.health?.status === 'degraded').length,
)
const latestLog = computed(() => monitor.logs[0]?.message ?? '暂无最新日志')
const deviceCount = computed(() => Object.keys(monitor.rtdata).length)
const tagCount = computed(() =>
  Object.values(monitor.rtdata).reduce((count, devicePoints) => count + Object.keys(devicePoints).length, 0),
)
const dbSizeText = computed(() => {
  const dbsize = Number(monitor.sysstatus.dbsize)

  if (!Number.isFinite(dbsize) || dbsize <= 0) {
    return '0'
  }

  return dbsize.toLocaleString('en-US')
})

let clockTimer: ReturnType<typeof setInterval> | null = null

watchEffect(() => {
  workspace.setRouteContext({
    routeKey: currentRoute.value.key,
    routeTitle: currentRoute.value.title,
    routePath: currentRoute.value.path,
  })
})

onMounted(() => {
  syncClock()
  clockTimer = setInterval(syncClock, 1000)
  applyTheme(currentTheme.value)
  monitor.start()
})

onUnmounted(() => {
  if (clockTimer) {
    clearInterval(clockTimer)
    clockTimer = null
  }
  monitor.stop()
})

async function logout() {
  auth.clear()
  await router.push('/login')
}

function syncClock() {
  const now = new Date()
  clockText.value = now.toLocaleTimeString('zh-CN', { hour12: false })
  dateText.value = now.toLocaleDateString('zh-CN')
}

function setThemeMode(nextTheme: ThemeName) {
  currentTheme.value = nextTheme
  persistTheme(nextTheme)
  applyTheme(nextTheme)
}
</script>

<template>
  <div class="app-shell">
    <div class="shell-noise"></div>
    <aside class="shell-sidebar">
      <div class="brand-block">
        <div class="brand-chip">
          <span class="brand-pulse"></span>
          <span>RS-IOT OPS</span>
        </div>
        <h1>运维驾驶舱</h1>
        <p class="brand-copy">让采集状态、链路诊断和 AI 协作在同一视图里闭环。</p>
        <div class="brand-meta">
          <div>
            <span>在线实例</span>
            <strong>{{ runningCollectors }}</strong>
          </div>
          <div>
            <span>当前用户</span>
            <strong>{{ auth.user || 'preview' }}</strong>
          </div>
        </div>
      </div>

      <nav class="nav-list" aria-label="主导航">
        <RouterLink
          v-for="(navItem, index) in PRIMARY_ROUTES"
          :key="navItem.key"
          :to="navItem.path"
          class="nav-item"
          :class="{ active: currentRoute.key === navItem.key }"
        >
          <span class="nav-index">0{{ index + 1 }}</span>
          <span class="nav-title">{{ navItem.shortTitle }}</span>
          <span class="nav-summary">{{ navItem.summary }}</span>
        </RouterLink>
      </nav>

      <section class="sidebar-panel">
        <p class="panel-label">当前聚焦</p>
        <strong>{{ monitor.selectedInstance || '未选中实例' }}</strong>
        <span>{{ monitor.selectedDevice || '未选中设备' }}</span>
      </section>

      <section class="sidebar-panel">
        <p class="panel-label">连接态势</p>
        <strong>{{ monitor.offline ? '重连中' : '在线' }}</strong>
        <span>{{ latestLog }}</span>
      </section>

      <section class="sidebar-footer">
        <span>Design Mode</span>
        <strong>Industrial Refined</strong>
      </section>
    </aside>

    <div class="shell-main">
      <div class="shell-topbar">
        <div class="topbar-brand">
          <span class="topbar-mark">RS-IOT</span>
          <div>
            <strong>{{ currentRoute.title }}</strong>
            <span>{{ currentRoute.summary }}</span>
          </div>
        </div>

        <div class="topbar-metrics">
          <div class="topbar-chip">
            <span class="label">DBSIZE</span>
            <strong>{{ dbSizeText }}</strong>
          </div>
          <div class="topbar-chip">
            <span class="label">DEVICES</span>
            <strong>{{ deviceCount }}</strong>
          </div>
          <div class="topbar-chip">
            <span class="label">TAGS</span>
            <strong>{{ tagCount }}</strong>
          </div>
          <div class="topbar-chip clock">
            <span class="label">{{ dateText }}</span>
            <strong>{{ clockText }}</strong>
          </div>
          <div class="theme-toggle" role="tablist" aria-label="主题切换">
            <button
              class="theme-button"
              :class="{ active: currentTheme === 'light' }"
              @click="setThemeMode('light')"
            >浅色</button>
            <button
              class="theme-button"
              :class="{ active: currentTheme === 'dark' }"
              @click="setThemeMode('dark')"
            >深色</button>
          </div>
        </div>
      </div>

      <header class="shell-header">
        <div class="header-copy">
          <p class="header-eyebrow">{{ currentRoute.title }}</p>
          <h2>{{ currentRoute.summary }}</h2>
          <p class="header-caption">
            以更清晰的视觉优先级组织数据，让巡检、诊断和处置可以连续完成。
          </p>
        </div>
        <div class="header-actions">
          <div class="metric-pill">
            <span>运行中</span>
            <strong>{{ runningCollectors }}</strong>
          </div>
          <div class="metric-pill warning">
            <span>降级</span>
            <strong>{{ degradedCollectors }}</strong>
          </div>
          <button class="ghost-button" @click="drawerOpen = !drawerOpen">
            {{ drawerOpen ? '收起 AI' : '打开 AI' }}
          </button>
          <button class="ghost-button" @click="logout">退出</button>
        </div>
      </header>

      <div v-if="monitor.offline" class="offline-banner" role="status" aria-live="polite">
        数据连接暂时中断，系统正在自动补拉日志并尝试重连。
      </div>

      <main class="shell-content">
        <div class="content-frame">
          <RouterView />
        </div>
      </main>
    </div>

    <AgentDrawer v-if="drawerOpen" @close="drawerOpen = false" />
  </div>
</template>

<style scoped>
.app-shell {
  min-height: 100vh;
  display: grid;
  grid-template-columns: 280px minmax(0, 1fr);
  background: var(--shell-app-bg);
  color: var(--color-ink);
  position: relative;
  overflow: hidden;
}

.shell-noise {
  position: absolute;
  inset: 0;
  pointer-events: none;
  opacity: var(--shell-noise-opacity);
  background-image:
    linear-gradient(var(--shell-noise-line) 1px, transparent 1px),
    linear-gradient(90deg, var(--shell-noise-line) 1px, transparent 1px);
  background-size: 120px 120px;
  mask-image: radial-gradient(circle at center, black 45%, transparent 88%);
}

.shell-sidebar {
  display: flex;
  flex-direction: column;
  gap: 18px;
  padding: 26px 18px 22px;
  background: var(--shell-sidebar-bg);
  border-right: 1px solid var(--shell-sidebar-border);
  backdrop-filter: blur(18px);
  position: relative;
  z-index: 1;
}

.brand-block h1,
.shell-header h2 {
  margin: 0;
  font-family: "Bahnschrift", "Segoe UI Variable Display", sans-serif;
  letter-spacing: 0.04em;
}

.header-eyebrow,
.panel-label {
  margin: 0 0 6px;
  font-size: 11px;
  letter-spacing: 0.22em;
  text-transform: uppercase;
  color: var(--color-ink-dim);
}

.brand-copy,
.sidebar-panel span,
.nav-summary {
  color: var(--color-ink-dim);
  line-height: 1.6;
}

.brand-chip {
  display: inline-flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 14px;
  padding: 7px 12px;
  border-radius: 999px;
  border: 1px solid var(--shell-chip-border);
  background: var(--shell-chip-bg);
  font-size: 11px;
  letter-spacing: 0.16em;
  text-transform: uppercase;
  color: var(--color-ink-secondary);
}

.brand-pulse {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  background: var(--color-success);
  box-shadow: 0 0 14px color-mix(in srgb, var(--color-success) 55%, transparent);
}

.brand-meta {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 10px;
  margin-top: 16px;
}

.brand-meta div,
.sidebar-footer {
  padding: 12px 14px;
  border-radius: 16px;
  background: var(--shell-card-bg);
  border: 1px solid var(--shell-card-border);
  box-shadow: var(--shadow-glow);
}

.brand-meta span,
.sidebar-footer span {
  display: block;
  margin-bottom: 6px;
  font-size: 11px;
  letter-spacing: 0.16em;
  text-transform: uppercase;
  color: var(--color-ink-dim);
}

.brand-meta strong,
.sidebar-footer strong {
  font-size: 15px;
}

.nav-list {
  display: grid;
  gap: 10px;
}

.nav-item {
  display: grid;
  grid-template-columns: auto 1fr;
  grid-template-areas:
    "index title"
    "index summary";
  column-gap: 12px;
  row-gap: 6px;
  padding: 15px 16px;
  border: 1px solid var(--shell-nav-border);
  border-radius: 18px;
  background: var(--shell-nav-bg);
  text-decoration: none;
  position: relative;
  overflow: hidden;
  transition:
    transform 0.18s ease,
    border-color 0.18s ease,
    background 0.18s ease,
    box-shadow 0.18s ease;
}

.nav-item:hover,
.nav-item.active {
  transform: translateX(4px);
  border-color: var(--shell-nav-active-border);
  background: var(--shell-nav-active-bg);
  box-shadow: var(--shell-nav-active-shadow);
}

.nav-item::after {
  content: "";
  position: absolute;
  inset: 0;
  background: linear-gradient(90deg, var(--shell-nav-highlight), transparent 48%);
  opacity: 0;
  transition: opacity 0.18s ease;
}

.nav-item:hover::after,
.nav-item.active::after {
  opacity: 1;
}

.nav-index {
  grid-area: index;
  align-self: start;
  padding-top: 2px;
  font-family: Consolas, monospace;
  font-size: 12px;
  color: var(--color-primary-hover);
}

.nav-title {
  grid-area: title;
  font-size: 15px;
  font-weight: 700;
  color: var(--color-ink);
}

.nav-summary {
  grid-area: summary;
}

.sidebar-panel {
  display: grid;
  gap: 6px;
  padding: 16px;
  border-radius: 20px;
  background: var(--shell-card-bg);
  border: 1px solid var(--shell-card-border);
  box-shadow: var(--shadow-glow);
}

.sidebar-panel strong {
  font-size: 16px;
}

.shell-main {
  display: grid;
  grid-template-rows: auto auto auto 1fr;
  min-width: 0;
  position: relative;
  z-index: 1;
}

.shell-topbar {
  display: flex;
  justify-content: space-between;
  gap: 16px;
  align-items: center;
  padding: 14px 30px 0;
}

.topbar-brand {
  display: flex;
  align-items: center;
  gap: 12px;
}

.topbar-mark {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  min-width: 84px;
  padding: 7px 10px;
  border-radius: 999px;
  border: 1px solid var(--shell-chip-border);
  background: var(--shell-chip-bg);
  color: var(--color-primary-strong);
  font-size: 11px;
  letter-spacing: 0.16em;
  text-transform: uppercase;
}

.topbar-brand strong {
  display: block;
  font-size: 14px;
}

.topbar-brand span:last-child {
  display: block;
  margin-top: 2px;
  color: var(--color-ink-dim);
  font-size: 12px;
}

.topbar-metrics {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
  justify-content: end;
}

.topbar-chip {
  display: grid;
  gap: 4px;
  padding: 9px 12px;
  border-radius: 14px;
  background: var(--shell-card-bg);
  border: 1px solid var(--shell-card-border);
  min-width: 90px;
}

.topbar-chip .label {
  font-size: 10px;
  letter-spacing: 0.16em;
  text-transform: uppercase;
  color: var(--color-ink-dim);
}

.topbar-chip strong {
  font-family: Consolas, monospace;
  font-size: 14px;
}

.topbar-chip.clock {
  min-width: 138px;
}

.theme-toggle {
  display: inline-flex;
  border-radius: 14px;
  overflow: hidden;
  border: 1px solid var(--shell-card-border);
  background: var(--shell-card-bg);
}

.theme-button {
  padding: 10px 14px;
  border: none;
  border-right: 1px solid var(--shell-card-border);
  background: transparent;
  color: var(--color-ink-dim);
}

.theme-button:last-child {
  border-right: none;
}

.theme-button.active {
  background: var(--shell-theme-active-bg);
  color: var(--shell-theme-active-ink);
}

.shell-header {
  display: flex;
  justify-content: space-between;
  gap: 16px;
  padding: 30px 30px 20px;
  align-items: end;
}

.header-copy {
  max-width: 640px;
}

.shell-header h2 {
  max-width: 620px;
  font-size: 30px;
  line-height: 1.18;
}

.header-caption {
  margin: 12px 0 0;
  max-width: 580px;
  color: var(--color-ink-dim);
  line-height: 1.7;
}

.header-actions {
  display: flex;
  align-items: center;
  gap: 12px;
  flex-wrap: wrap;
}

.metric-pill,
.ghost-button {
  border-radius: 999px;
  border: 1px solid var(--shell-card-border);
  background: var(--shell-card-bg-strong);
  box-shadow: var(--shadow-glow);
}

.metric-pill {
  display: flex;
  gap: 10px;
  align-items: center;
  padding: 11px 15px;
}

.metric-pill span {
  color: var(--color-ink-dim);
  font-size: 12px;
}

.metric-pill strong {
  font-family: "Bahnschrift", sans-serif;
  font-size: 18px;
}

.metric-pill.warning strong {
  color: var(--color-warning);
}

.ghost-button {
  padding: 11px 16px;
  color: var(--color-ink);
  background: var(--shell-button-bg);
}

.ghost-button:hover {
  border-color: var(--shell-nav-active-border);
  box-shadow: var(--shell-nav-active-shadow);
}

.shell-content {
  min-width: 0;
  padding: 0 30px 30px;
}

.content-frame {
  padding: 20px;
  border-radius: 32px;
  background: var(--shell-content-bg);
  border: 1px solid var(--shell-content-border);
  box-shadow: inset 0 1px 0 var(--shell-content-highlight);
}

.offline-banner {
  margin: 0 30px 18px;
  padding: 13px 16px;
  border-radius: 18px;
  color: var(--shell-warning-ink);
  background: var(--shell-warning-bg);
  box-shadow: var(--shell-warning-shadow);
}

@media (max-width: 1080px) {
  .app-shell {
    grid-template-columns: 1fr;
  }

  .shell-sidebar {
    border-right: none;
    border-bottom: 1px solid rgba(148, 163, 184, 0.16);
  }

  .shell-header {
    flex-direction: column;
    align-items: start;
  }

  .shell-topbar {
    flex-direction: column;
    align-items: start;
  }

  .topbar-metrics {
    justify-content: start;
  }

  .brand-meta {
    grid-template-columns: 1fr;
  }
}
</style>
