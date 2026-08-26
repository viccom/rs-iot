<!-- frontend/src/components/monitor/SystemStatus.vue -->
<script setup lang="ts">
import { computed } from 'vue'
import { useMonitorStore } from '../../stores/monitor'

const monitor = useMonitorStore()

// ---- 历史归档行（sysstatus.history 块；旧后端无块 → "—" 降级）----

/** engine 态 → 着色分类（ok 绿 / off 灰 / fail 红 / none 默认）。 */
const histClass = computed(() => {
  switch (monitor.sysstatus.history?.engine) {
    case 'ok': return 'hist-ok'
    case 'disabled': return 'hist-off'
    case 'init_failed': return 'hist-fail'
    default: return 'hist-none'
  }
})

/** 行文案：ok 时带 written/dropped 两个关键计数（其余五计数器收进 title 悬浮）。 */
const histText = computed(() => {
  const h = monitor.sysstatus.history
  if (!h) return '—'
  switch (h.engine) {
    case 'ok': {
      const s = h.stats
      return s ? `正常 · 写 ${s.written} / 弃 ${s.dropped}` : '正常'
    }
    case 'disabled': return '未启用'
    case 'init_failed': return '初始化失败'
    default: return h.engine
  }
})

/** 悬浮全量：feature/enabled + 七计数器 + P6 治理字段（内存水位/series/
 *  archived_tags/死区削减率/errors 分型；缺字段省略行——旧后端降级，
 *  ed54a6d 同惯例。不撑爆小面板）。 */
const histTitle = computed(() => {
  const h = monitor.sysstatus.history
  if (!h) return '旧后端无 history 块'
  const lines: string[] = [`feature: ${h.feature} · enabled: ${h.enabled} · engine: ${h.engine}`]
  const s = h.stats
  if (!s) {
    lines.push('stats: null')
  } else {
    lines.push(
      `sent ${s.sent} · written ${s.written} · dropped ${s.dropped} · deadbanded ${s.deadbanded} · skipped ${s.skipped} · lane_mismatch ${s.lane_mismatch} · errors ${s.errors}`,
    )
    if (s.deadband_evaluated !== undefined) lines.push(`deadband_evaluated: ${s.deadband_evaluated}`)
    if (s.errors_by_kind) {
      const kinds = Object.entries(s.errors_by_kind)
        .filter(([, n]) => n > 0)
        .map(([k, n]) => `${k} ${n}`)
        .join(' · ')
      if (kinds) lines.push(`errors_by_kind: ${kinds}`)
    }
  }
  // P6 治理字段仅 ok 态输出（后端省略语义）；旧后端无 → 整体省略行
  if (h.engine === 'ok') {
    if (h.memory_used_bytes !== undefined && h.memory_budget_bytes !== undefined) {
      lines.push(
        `memory: ${(h.memory_used_bytes / 1048576).toFixed(1)} / ${(h.memory_budget_bytes / 1048576).toFixed(1)} MiB`,
      )
    }
    if (h.series !== undefined) lines.push(`series: ${h.series}`)
    if (h.archived_tags !== undefined) lines.push(`archived_tags: ${h.archived_tags}`)
    if (h.deadbands !== undefined) {
      lines.push(`deadband 削减率: ${(h.deadbands.ratio * 100).toFixed(1)}%（evaluated ${h.deadbands.evaluated}）`)
    }
  }
  return lines.join('\n')
})
</script>
<template>
  <div class="sys">
    <div class="row"><span>dbsize</span><b>{{ monitor.sysstatus.dbsize }}</b></div>
    <div class="row"><span>uptime</span><b>{{ monitor.sysstatus.process?.uptime_secs }}s</b></div>
    <div class="row"><span>cpu</span><b>{{ monitor.sysstatus.process?.cpu_pct?.toFixed(1) }}%</b></div>
    <div class="row"><span>mem</span><b>{{ ((monitor.sysstatus.process?.mem_bytes || 0) / 1048576).toFixed(1) }}MB</b></div>
    <div class="row"><span>pipeline processed</span><b>{{ monitor.sysstatus.metrics?.pipeline_processed_count }}</b></div>
    <div class="row"><span>hub depth</span><b>{{ monitor.sysstatus.metrics?.hub?.depth_last }}</b></div>
    <div class="row">
      <span>历史归档</span>
      <b :class="histClass" :title="histTitle">{{ histText }}</b>
    </div>
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

/* 历史引擎态着色（engine ok/disabled/init_failed）。 */
.row b.hist-ok { color: var(--color-success); }
.row b.hist-off { color: var(--color-ink-dim); }
.row b.hist-fail { color: var(--color-danger); }
</style>
