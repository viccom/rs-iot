<!-- frontend/src/components/monitor/CollectorList.vue -->
<script setup lang="ts">
import { useMonitorStore } from '../../stores/monitor'
const monitor = useMonitorStore()
</script>
<template>
  <div class="collector-list">
    <h3>采集器</h3>
    <div v-for="inst in monitor.instances" :key="inst.inst_id"
         class="item" :class="{ active: monitor.selectedInstance === inst.inst_id, [inst.state]: true }"
         @click="monitor.selectInstance(inst.inst_id)">
      <span class="dot"></span>
      <span class="id">{{ inst.inst_id }}</span>
      <span class="code">{{ inst.app_code }}</span>
    </div>
    <p class="empty" v-if="!monitor.instances.length">无实例</p>
  </div>
</template>
<style scoped>
.collector-list { width: 240px; border-right: 1px solid var(--color-divider); padding: 12px; overflow-y: auto; }
.item { display: flex; align-items: center; gap: 8px; padding: 6px 8px; border-radius: 4px; cursor: pointer; font-size: 13px; }
.item:hover { background: var(--color-surface-hover); }
.item.active { background: var(--color-surface-active); }
.dot { width: 8px; height: 8px; border-radius: 50%; }
.running .dot { background: var(--color-success); }
.failed .dot { background: var(--color-danger); }
.stopped .dot, .starting .dot { background: var(--color-ink-muted); }
.id { flex: 1; font-family: monospace; }
.code { color: var(--color-ink-muted); font-size: 11px; }
.empty { color: var(--color-ink-muted); }
</style>
