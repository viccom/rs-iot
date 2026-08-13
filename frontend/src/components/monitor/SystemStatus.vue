<!-- frontend/src/components/monitor/SystemStatus.vue -->
<script setup lang="ts">
import { useMonitorStore } from '../../stores/monitor'

/** sysstatus.process 子结构（rs-iot `GET /api/v1/sysstatus`）。 */
interface ProcessInfo {
  uptime_secs?: number
  cpu_pct?: number
  mem_bytes?: number
}
/** sysstatus.metrics 子结构。 */
interface MetricsInfo {
  pipeline_processed_count?: number
  hub?: { depth_last?: number }
}

const monitor = useMonitorStore()
const process = (): ProcessInfo => (monitor.sysstatus.process as ProcessInfo) || {}
const metrics = (): MetricsInfo => (monitor.sysstatus.metrics as MetricsInfo) || {}
</script>
<template>
  <div class="sys">
    <div class="row"><span>dbsize</span><b>{{ monitor.sysstatus.dbsize }}</b></div>
    <div class="row"><span>uptime</span><b>{{ process().uptime_secs }}s</b></div>
    <div class="row"><span>cpu</span><b>{{ process().cpu_pct?.toFixed(1) }}%</b></div>
    <div class="row"><span>mem</span><b>{{ ((process().mem_bytes || 0) / 1048576).toFixed(1) }}MB</b></div>
    <div class="row"><span>pipeline processed</span><b>{{ metrics().pipeline_processed_count }}</b></div>
    <div class="row"><span>hub depth</span><b>{{ metrics().hub?.depth_last }}</b></div>
  </div>
</template>
<style scoped>
.sys {
  padding: 2px 0 0;
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 12px;
}

.row {
  display: flex;
  justify-content: space-between;
  gap: 12px;
  padding: 16px 18px;
  background: var(--card-bg);
  border-radius: 18px;
  border: 1px solid var(--card-border);
  font-size: 13px;
  box-shadow: inset 0 1px 0 var(--panel-inner-border);
}

.row span {
  color: var(--color-ink-dim);
  text-transform: uppercase;
  letter-spacing: 0.08em;
  font-size: 11px;
}

.row b {
  font-family: Consolas, monospace;
  color: var(--chip-ink);
}
</style>
