<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watchEffect } from 'vue'
import { RouterLink, RouterView, useRoute, useRouter } from 'vue-router'
import AgentDrawer from '../chat/AgentDrawer.vue'
import { PRIMARY_ROUTES } from '../../lib/navigation'
import { useMonitorStore } from '../../stores/monitor'
import { useWorkspaceStore } from '../../stores/workspace'
import { useAuthStore } from '../../stores/auth'
import { applyTheme, persistTheme, readStoredTheme, applyDensity, persistDensity, readStoredDensity, type ThemeName, type DensityName } from '../../lib/theme'

const route = useRoute()
const router = useRouter()
const auth = useAuthStore()
const monitor = useMonitorStore()
const workspace = useWorkspaceStore()
const drawerOpen = ref(false)
const currentTheme = ref<ThemeName>(readStoredTheme())
const currentDensity = ref<DensityName>(readStoredDensity())
const clockText = ref('--:--:--')
const dateText = ref('----/--/--')

const currentRoute = computed(() =>
  PRIMARY_ROUTES.find(item => item.key === route.name) ?? PRIMARY_ROUTES[0],
)
const runningCollectors = computed(() =>
  monitor.roster.filter(instance => instance.state === 'running').length,
)
const degradedCollectors = computed(() =>
  monitor.roster.filter(instance => instance.health?.status === 'degraded').length,
)
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
  applyDensity(currentDensity.value)
  monitor.start()
  // 一次性补拉最近日志（REST，非 SSE 订阅——不违背"日志推送默认关闭、诊断页手动激活"）：
  // 侧栏"连接态势"与总览"现场脉搏"读 logs 缓冲，否则默认恒空、显示误导。
  void monitor.loadLogsRecent()
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

function setDensityMode(nextDensity: DensityName) {
  currentDensity.value = nextDensity
  persistDensity(nextDensity)
  applyDensity(nextDensity)
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
          <!-- 当前聚焦 + 连接状态：值守最关注的信息放顶栏首位（原在侧栏最下方，易被忽略）。 -->
          <div class="topbar-chip focus" title="当前聚焦的实例/设备（诊断抓样与 AI 上下文联动）">
            <span class="label">聚焦</span>
            <strong>{{ monitor.selectedInstance || '—' }}<template v-if="monitor.selectedDevice"> / {{ monitor.selectedDevice }}</template></strong>
          </div>
          <div class="topbar-chip" :class="monitor.offline ? 'down' : 'up'" title="rs-iot SSE 数据连接状态">
            <span class="label">连接</span>
            <strong>{{ monitor.offline ? '重连中' : '在线' }}</strong>
          </div>
          <div class="topbar-chip" title="运行中实例数 / 实例总数（roster：配置全集 ∪ 运行态）">
            <span class="label">运行</span>
            <strong>{{ runningCollectors }}/{{ monitor.roster.length }}</strong>
          </div>
          <div class="topbar-chip" :class="{ warn: degradedCollectors > 0 }" title="health 降级的实例数">
            <span class="label">降级</span>
            <strong>{{ degradedCollectors }}</strong>
          </div>
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
          <div class="theme-toggle" role="tablist" aria-label="界面密度切换">
            <button
              class="theme-button"
              :class="{ active: currentDensity === 'cozy' }"
              @click="setDensityMode('cozy')"
            >舒适</button>
            <button
              class="theme-button"
              :class="{ active: currentDensity === 'compact' }"
              @click="setDensityMode('compact')"
            >紧凑</button>
          </div>
          <button class="ghost-button" @click="drawerOpen = !drawerOpen">
            {{ drawerOpen ? '收起 AI' : '打开 AI' }}
          </button>
          <button class="ghost-button" @click="logout">退出</button>
        </div>
      </div>

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

.brand-block h1 {
  margin: 0;
  font-family: "Bahnschrift", "Segoe UI Variable Display", sans-serif;
  letter-spacing: 0.04em;
}

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

.shell-main {
  display: grid;
  grid-template-rows: auto auto 1fr;
  min-width: 0;
  position: relative;
  z-index: 1;
}

.shell-topbar {
  display: flex;
  justify-content: space-between;
  gap: 16px;
  align-items: center;
  padding: 14px 30px 10px;
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

/* 聚焦 chip：实例/设备 ID 等宽展示，超长省略 */
.topbar-chip.focus {
  min-width: 120px;
  max-width: 260px;
}
.topbar-chip.focus strong {
  display: block;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
/* 连接状态 chip：在线绿 / 断连红（与 offline 横幅一致语义）；降级 chip 警示色 */
.topbar-chip.up strong { color: var(--color-success); }
.topbar-chip.down strong { color: var(--color-danger); }
.topbar-chip.warn strong { color: var(--color-warning); }

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

.ghost-button {
  padding: 11px 16px;
  color: var(--color-ink);
  border-radius: 999px;
  border: 1px solid var(--shell-card-border);
  background: var(--shell-button-bg);
  box-shadow: var(--shadow-glow);
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

  /* C4 轻量缓解：窄屏侧栏堆叠在内容上方时，收起品牌文案/元信息/导航摘要/页脚，
     只留品牌标识 + 导航主项——缩短到达数据的滚动距离（完整抽屉化侧栏另行排期）。 */
  .brand-copy,
  .brand-meta,
  .nav-summary,
  .sidebar-footer {
    display: none;
  }

  .shell-topbar {
    flex-direction: column;
    align-items: start;
  }

  .topbar-metrics {
    justify-content: start;
  }
}
</style>
