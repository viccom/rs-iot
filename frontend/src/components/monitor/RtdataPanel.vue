<!-- frontend/src/components/monitor/RtdataPanel.vue -->
<script setup lang="ts">
import { ref, computed } from 'vue'
import { useMonitorStore } from '../../stores/monitor'
const monitor = useMonitorStore()
// 手动 devId 文本过滤（替代错误的 selectedInstance 过滤：rtdata key 是 devId 非 inst_id，
// 1 实例可多设备，selectedInstance 联动用 inst_id 在此无意义）。
const devFilter = ref('')
const rows = computed(() => {
  const all = monitor.rtdata
  const result: { dev: string; tag: string; value: string; ts: string }[] = []
  for (const [dev, tags] of Object.entries(all)) {
    if (monitor.selectedDevice && dev !== monitor.selectedDevice) continue
    if (devFilter.value && !dev.includes(devFilter.value)) continue
    for (const [tag, cell] of Object.entries(tags)) {
      const [_ts, localTime, value] = cell as [number, string, string | number]
      // field 形如 <devid>.<tag>，devid 不含 '.'；取末段还原 tag。
      result.push({ dev, tag: tag.split('.').pop() || tag, value: String(value), ts: localTime })
    }
  }
  return result
})
</script>
<template>
  <div class="rtdata">
    <div class="filter">
      <input v-model="devFilter" placeholder="按 devId 过滤">
      <span class="filter-count">{{ rows.length }} 条点位</span>
    </div>
    <table>
      <thead><tr><th>设备</th><th>Tag</th><th>值</th><th>时间</th></tr></thead>
      <tbody><tr v-for="r in rows" :key="r.dev + r.tag"><td>{{ r.dev }}</td><td>{{ r.tag }}</td><td class="val">{{ r.value }}</td><td class="ts">{{ r.ts }}</td></tr></tbody>
    </table>
    <p v-if="!rows.length" class="empty">无数据</p>
  </div>
</template>
<style scoped>
.rtdata { display: flex; flex-direction: column; height: 100%; overflow: hidden; }

.filter {
  display: flex;
  gap: 10px;
  align-items: center;
  padding: 10px 0 14px;
  border-bottom: 1px solid var(--color-divider);
}

.filter input { flex: 1; padding: 12px 14px; border-radius: 14px; }

.filter-count {
  padding: 8px 12px;
  border-radius: 999px;
  background: var(--chip-bg);
  border: 1px solid var(--chip-border);
  color: var(--chip-ink);
  font-size: 12px;
}

table { width: 100%; border-collapse: collapse; font-size: 13px; margin-top: 10px; }

thead th {
  position: sticky;
  top: 0;
  z-index: 1;
  background: var(--table-head-bg);
  color: var(--color-ink-secondary);
  letter-spacing: 0.08em;
  text-transform: uppercase;
  font-size: 11px;
}

th, td { text-align: left; padding: 11px 10px; border-bottom: 1px solid var(--card-border); }

tbody tr {
  transition: background-color 0.16s ease;
}

tbody tr:hover {
  background: var(--table-row-hover);
}

.val { font-family: Consolas, monospace; color: var(--chip-ink); }
.ts { color: var(--color-ink-muted); }
.empty { color: var(--color-ink-muted); padding: 20px; }
</style>
