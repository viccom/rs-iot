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
.sys { padding: 12px; display: grid; grid-template-columns: repeat(2, 1fr); gap: 8px; }
.row { display: flex; justify-content: space-between; padding: 6px 8px; background: var(--color-surface-raised); border-radius: 4px; font-size: 13px; }
.row b { font-family: monospace; }
</style>
