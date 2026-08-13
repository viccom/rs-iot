<!-- frontend/src/components/monitor/PacketTrace.vue -->
<script setup lang="ts">
import { computed } from 'vue'
import { useMonitorStore } from '../../stores/monitor'
const monitor = useMonitorStore()
// PacketEntry.inst_id 真的是 inst_id（非 devId），按 selectedInstance 过滤正确。
const filtered = computed(() => {
  if (!monitor.selectedInstance) return monitor.packets
  return monitor.packets.filter(p => p.inst_id === monitor.selectedInstance)
})
</script>
<template>
  <div class="packet-trace">
    <p class="hint" v-if="monitor.selectedInstance">过滤: {{ monitor.selectedInstance }}</p>
    <div class="list">
      <div v-for="(p, i) in filtered" :key="i" class="pkt" :class="p.dir">
        <span class="dir">{{ p.dir.toUpperCase() }}</span>
        <span class="inst">{{ p.inst_id }}</span>
        <span class="hex">{{ p.hex }}</span>
        <span class="ts">{{ new Date(p.ts).toLocaleTimeString() }}</span>
      </div>
      <p v-if="!filtered.length" class="empty">无报文（提示：在实例页 POST /api/rsiot/api/v1/inst/trace 激活报文采集）</p>
    </div>
  </div>
</template>
<style scoped>
.packet-trace { display: flex; flex-direction: column; height: 100%; }
.hint {
  padding: 10px 12px;
  color: var(--color-ink-muted);
  font-size: 12px;
  margin: 0 0 10px;
  border-radius: 14px;
  background: var(--chip-bg);
  border: 1px solid var(--chip-border);
}
.list { flex: 1; overflow-y: auto; padding: 0; }
.pkt {
  display: grid;
  grid-template-columns: 48px 130px 1fr 84px;
  gap: 10px;
  padding: 12px 10px;
  font-size: 12px;
  font-family: Consolas, monospace;
  border-bottom: 1px solid var(--card-border);
  border-radius: 14px;
}
.pkt:hover { background: var(--table-row-hover); }
.pkt.tx .dir { color: var(--color-primary); } .pkt.rx .dir { color: var(--color-success); }
.inst, .ts { color: var(--color-ink-muted); }
.hex { word-break: break-all; }
.empty { color: var(--color-ink-muted); padding: 20px; text-align: center; }
</style>
