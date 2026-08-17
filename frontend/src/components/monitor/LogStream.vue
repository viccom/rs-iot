<!-- frontend/src/components/monitor/LogStream.vue -->
<script setup lang="ts">
import { ref, computed } from 'vue'
import { useMonitorStore } from '../../stores/monitor'
const monitor = useMonitorStore()
const level = ref('')
const target = ref('')
const filtered = computed(() => {
  // level 用 >= 语义（如选 warn → 含 error+warn），trace/debug 用户显式选。
  const lvRank: Record<string, number> = { error: 4, warn: 3, info: 2, debug: 1, trace: 0 }
  // 后端 tracing level 大写（"INFO"/"WARN"），比较前 lower()（对标 scada.html levelRank）。
  const want = lvRank[level.value] ?? undefined
  // target 模糊匹配（大小写不敏感 includes）——target 是全限定名（如 rsiot::plugin::...），
  // 全等匹配下输 "modbus" 会过滤不出任何行。
  const kw = target.value.trim().toLowerCase()
  return monitor.logs.filter(l => {
    const lv = (l.level || '').toLowerCase()
    if (want !== undefined && (lvRank[lv] ?? 0) < want) return false
    if (kw && !(l.target || '').toLowerCase().includes(kw)) return false
    return true
  })
})
</script>
<template>
  <div class="log-stream">
    <div class="filter">
      <select v-model="level"><option value="">全部级别</option><option>error</option><option>warn</option><option>info</option><option>debug</option><option>trace</option></select>
      <input v-model="target" placeholder="target 过滤（模糊，如 modbus / io_trace）">
    </div>
    <div class="list">
      <div v-for="(l, i) in filtered" :key="i" class="entry" :class="(l.level || '').toLowerCase()">
        <span class="ts">{{ new Date(l.ts).toLocaleTimeString() }}</span>
        <span class="lv">{{ l.level.toUpperCase() }}</span>
        <span class="tg">{{ l.target }}</span>
        <span class="msg">{{ l.message }}</span>
      </div>
      <p v-if="!filtered.length" class="empty">无日志</p>
    </div>
  </div>
</template>
<style scoped>
.log-stream { display: flex; flex-direction: column; height: 100%; }
.filter { display: flex; gap: 8px; padding: 10px 0 14px; border-bottom: 1px solid var(--color-divider); }
.filter select,
.filter input {
  padding: 12px 14px;
  border-radius: 14px;
}
.list { flex: 1; overflow-y: auto; padding: 10px 0 0; }
.entry {
  display: grid;
  grid-template-columns: 84px 56px 140px 1fr;
  gap: 10px;
  padding: 12px 10px;
  font-size: 12px;
  border-bottom: 1px solid var(--card-border);
  font-family: Consolas, monospace;
  border-radius: 14px;
}
.entry:hover { background: var(--table-row-hover); }
.entry.error { color: var(--color-danger); } .entry.warn { color: var(--color-warning); }
.ts, .tg { color: var(--color-ink-muted); }
.empty { color: var(--color-ink-muted); padding: 20px; text-align: center; }
</style>
