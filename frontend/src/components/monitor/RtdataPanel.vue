<!-- frontend/src/components/monitor/RtdataPanel.vue -->
<script setup lang="ts">
import { computed } from 'vue'
import { useMonitorStore } from '../../stores/monitor'
const monitor = useMonitorStore()
// 按 selectedInstance 过滤；空则显示全部
const rows = computed(() => {
  const all = monitor.rtdata
  const devFilter = monitor.selectedInstance
  const result: { dev: string; tag: string; value: string; ts: string }[] = []
  for (const [dev, tags] of Object.entries(all)) {
    if (devFilter && dev !== devFilter) continue
    for (const [tag, cell] of Object.entries(tags)) {
      const [_ts, localTime, value] = cell as [number, string, string | number]
      result.push({ dev, tag: tag.split('.').pop() || tag, value: String(value), ts: localTime })
    }
  }
  return result
})
</script>
<template>
  <div class="rtdata">
    <table><thead><tr><th>设备</th><th>Tag</th><th>值</th><th>时间</th></tr></thead>
      <tbody><tr v-for="r in rows" :key="r.dev + r.tag"><td>{{ r.dev }}</td><td>{{ r.tag }}</td><td class="val">{{ r.value }}</td><td class="ts">{{ r.ts }}</td></tr></tbody>
    </table>
    <p v-if="!rows.length" class="empty">无数据</p>
  </div>
</template>
<style scoped>
.rtdata { padding: 12px; overflow: auto; }
table { width: 100%; border-collapse: collapse; font-size: 13px; }
th, td { text-align: left; padding: 4px 8px; border-bottom: 1px solid var(--color-divider); }
.val { font-family: monospace; }
.ts { color: var(--color-ink-muted); }
.empty { color: var(--color-ink-muted); padding: 20px; }
</style>
