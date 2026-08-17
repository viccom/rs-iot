<!-- frontend/src/components/monitor/CollectorList.vue -->
<script setup lang="ts">
import { useMonitorStore } from '../../stores/monitor'
const monitor = useMonitorStore()
</script>
<template>
  <div class="collector-list">
    <h3>采集器</h3>
    <div v-for="inst in monitor.roster" :key="inst.inst_id"
         class="item" :class="{ active: monitor.selectedInstance === inst.inst_id, [inst.state]: true }"
         @click="monitor.selectInstance(inst.inst_id)">
      <span class="dot"></span>
      <div class="copy">
        <span class="id">{{ inst.inst_id }}</span>
        <span class="code">{{ inst.app_code }}</span>
      </div>
      <span class="state">{{ inst.state }}</span>
    </div>
    <p class="empty" v-if="!monitor.roster.length">无实例</p>
  </div>
</template>
<style scoped>
.collector-list {
  width: 100%;
  padding: 8px 0 0;
  overflow-y: auto;
}

h3 {
  margin: 0 0 14px;
  font-size: 12px;
  letter-spacing: 0.18em;
  text-transform: uppercase;
  color: var(--color-ink-dim);
}

.item {
  display: grid;
  grid-template-columns: auto 1fr auto;
  gap: 10px;
  align-items: center;
  padding: 12px 14px;
  border-radius: 18px;
  cursor: pointer;
  font-size: 13px;
  border: 1px solid var(--card-border);
  background: var(--card-bg);
}

.item + .item {
  margin-top: 10px;
}

.item:hover {
  background: var(--color-surface-hover);
  border-color: var(--color-divider);
}

.item.active {
  background: linear-gradient(135deg, color-mix(in srgb, var(--color-primary) 18%, white), var(--card-bg));
  border-color: var(--color-divider-strong);
  box-shadow: var(--shadow-soft);
}

.dot { width: 10px; height: 10px; border-radius: 50%; }
.running .dot { background: var(--color-success); }
.failed .dot { background: var(--color-danger); }
.stopped .dot { background: var(--color-ink-muted); }
/* 过渡态（starting/stopping）：黄色闪烁，与 stopped 灰点区分（C5） */
.starting .dot, .stopping .dot {
  background: var(--color-warning);
  animation: dot-pulse 1s ease-in-out infinite;
}
@keyframes dot-pulse {
  0%, 100% { opacity: 1; }
  50% { opacity: 0.25; }
}

.copy {
  display: grid;
  gap: 4px;
}

.id { font-family: Consolas, monospace; }
.code { color: var(--color-ink-muted); font-size: 11px; }

.state {
  padding: 5px 9px;
  border-radius: 999px;
  background: var(--soft-overlay);
  color: var(--color-ink-dim);
  font-size: 11px;
  letter-spacing: 0.08em;
  text-transform: uppercase;
}

.empty { color: var(--color-ink-muted); }
</style>
