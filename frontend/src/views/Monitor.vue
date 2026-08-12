<!-- frontend/src/views/Monitor.vue -->
<script setup lang="ts">
import { ref, onMounted, onUnmounted } from 'vue'
import { useMonitorStore } from '../stores/monitor'
import CollectorList from '../components/monitor/CollectorList.vue'
import RtdataPanel from '../components/monitor/RtdataPanel.vue'
import SystemStatus from '../components/monitor/SystemStatus.vue'
import LogStream from '../components/monitor/LogStream.vue'
import PacketTrace from '../components/monitor/PacketTrace.vue'
import AgentDrawer from '../components/chat/AgentDrawer.vue'

const monitor = useMonitorStore()
const tab = ref<'data' | 'log' | 'packet' | 'sys'>('data')
const drawerOpen = ref(false)

const TAB_LABELS: Record<typeof tab.value, string> = {
  data: '数据',
  log: '日志',
  packet: '报文',
  sys: '系统',
}

onMounted(() => monitor.start())
onUnmounted(() => monitor.stop())
</script>
<template>
  <div class="monitor">
    <header class="topbar">
      <h1>rs-iot 监控</h1>
      <button @click="drawerOpen = !drawerOpen">AI</button>
    </header>
    <!-- 离线横幅（SSE 断开）：fixed top 覆盖，z-index 200（高于 AgentDrawer 100） -->
    <div v-if="monitor.offline" class="offline-banner" role="status" aria-live="polite">
      rs-iot 连接断开，重连中…
    </div>
    <div class="body">
      <CollectorList />
      <main class="main">
        <nav class="tabs">
          <button v-for="t in (['data','log','packet','sys'] as const)" :key="t"
                  :class="{ active: tab === t }" @click="tab = t">{{ TAB_LABELS[t] }}</button>
        </nav>
        <section class="content">
          <RtdataPanel v-if="tab === 'data'" />
          <LogStream v-else-if="tab === 'log'" />
          <PacketTrace v-else-if="tab === 'packet'" />
          <SystemStatus v-else />
        </section>
      </main>
    </div>
    <AgentDrawer v-if="drawerOpen" @close="drawerOpen = false" />
  </div>
</template>
<style scoped>
.monitor { display: grid; grid-template-rows: 48px 1fr; height: 100vh; }
.topbar {
  display: flex; align-items: center; justify-content: space-between;
  padding: 0 16px;
  background: var(--color-surface-raised);
  border-bottom: 1px solid var(--color-divider);
}
.topbar h1 { font-size: 15px; margin: 0; }
.body { display: grid; grid-template-columns: auto 1fr; overflow: hidden; }
.main { display: grid; grid-template-rows: 40px 1fr; overflow: hidden; }
.tabs {
  display: flex; gap: 4px; padding: 6px 12px;
  border-bottom: 1px solid var(--color-divider);
  background: var(--color-surface-raised);
}
.tabs button {
  border: none; background: none;
  padding: 6px 12px; border-radius: 4px; font-size: 13px;
}
.tabs button.active { background: var(--color-primary); color: white; }
.content { overflow: auto; }
/* 离线横幅：fixed top 全宽覆盖（对标 scada.html markLink DOWN），z-index 200 高于 AgentDrawer。 */
.offline-banner {
  position: fixed; top: 0; left: 0; right: 0;
  background: var(--color-danger); color: white;
  padding: 6px; text-align: center; font-size: 13px;
  z-index: 200;
}
</style>
