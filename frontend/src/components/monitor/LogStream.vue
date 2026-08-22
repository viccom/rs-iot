<!-- frontend/src/components/monitor/LogStream.vue -->
<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { useMonitorStore } from '../../stores/monitor'
import { matchesLogSource, type LogSource } from '../../lib/logSource'
import { useVirtualRows } from '../../composables/useVirtualRows'

const monitor = useMonitorStore()
const level = ref('')
const target = ref('')
const source = ref<LogSource>('all')

const sourceLabels: Record<LogSource, string> = { all: '全部', instance: '实例', system: '系统' }
const sources: LogSource[] = ['all', 'instance', 'system']

/** 固定行高（单行 ellipsis 后行高恒定——虚拟滚动的硬约束）。 */
const ROW_H = 36

const filtered = computed(() => {
  // level 用 >= 语义（如选 warn → 含 error+warn），trace/debug 用户显式选。
  // 后端 tracing level 大写（"INFO"/"WARN"），比较前 lower()（对标 scada.html levelRank）。
  const lvRank: Record<string, number> = { error: 4, warn: 3, info: 2, debug: 1, trace: 0 }
  const want = lvRank[level.value] ?? undefined
  // target 模糊匹配（大小写不敏感 includes）——target 是全限定名（如 rsiot::plugin::...），
  // 全等匹配下输 "modbus" 会过滤不出任何行。
  const kw = target.value.trim().toLowerCase()
  return monitor.logs.filter(l => {
    if (!matchesLogSource(l.target || '', source.value)) return false
    const lv = (l.level || '').toLowerCase()
    if (want !== undefined && (lvRank[lv] ?? 0) < want) return false
    if (kw && !(l.target || '').toLowerCase().includes(kw)) return false
    return true
  })
})

const { containerRef, onScroll, start, end, padTop, padBottom, toTop, compensatePrepend } =
  useVirtualRows(computed(() => filtered.value.length), ROW_H)
const windowRows = computed(() => filtered.value.slice(start.value, end.value))

// 过滤条件变化 → 回顶（内容全换，保持滚动位置无意义）。先于 length watch 注册，
// 同一 flush 周期两者同触发时先置 scrollTop=0，length 补偿随即跳过（非顶部才补）。
watch([level, target, source], () => toTop())
// 高频前插补偿：回看历史（非顶部）时新日志到来不让视口内容跳动。
watch(() => filtered.value.length, (n, o) => { if (n > o) compensatePrepend(n - o) })

/** 显示短名（去 rs_iot:: 前缀省 ~42px；匹配/悬停仍用全名）。 */
function shortTarget(t: string) {
  return t.startsWith('rs_iot::') ? t.slice('rs_iot::'.length) : t
}
</script>
<template>
  <div class="log-stream">
    <div class="filter">
      <div class="seg">
        <button
          v-for="s in sources"
          :key="s"
          :class="{ on: source === s }"
          :title="s === 'instance' ? '内置插件实例运行日志（target 前缀 plugin::builtin::）' : s === 'system' ? '框架/编排/脚本引擎等系统日志' : '不过滤'"
          @click="source = s"
        >{{ sourceLabels[s] }}</button>
      </div>
      <select v-model="level"><option value="">全部级别</option><option>error</option><option>warn</option><option>info</option><option>debug</option><option>trace</option></select>
      <input v-model="target" placeholder="target 过滤（模糊，如 modbus / io_trace）">
    </div>
    <div class="head">
      <span>时间</span><span>级别</span><span>来源</span><span>详情</span>
    </div>
    <div ref="containerRef" class="list" @scroll.passive="onScroll">
      <div :style="{ height: padTop + 'px' }"></div>
      <div v-for="l in windowRows" :key="l.seq" class="entry" :class="(l.level || '').toLowerCase()">
        <span class="ts">{{ new Date(l.ts).toLocaleTimeString() }}</span>
        <span class="lv">{{ l.level.toUpperCase() }}</span>
        <span class="tg" :title="l.target">{{ shortTarget(l.target) }}</span>
        <span class="msg" :title="l.message">{{ l.message }}</span>
      </div>
      <div :style="{ height: padBottom + 'px' }"></div>
      <p v-if="!filtered.length" class="empty">无日志</p>
    </div>
  </div>
</template>
<style scoped>
/* 确定高度（父级面板 auto 高使 height:100% 失效，列表随内容撑开、永不内滚——
   与 RtdataPanel 同款断链，同款 clamp 修复）。 */
.log-stream {
  display: flex;
  flex-direction: column;
  height: clamp(320px, calc(100vh - 340px), 720px);
}
.filter { display: flex; gap: 8px; padding: 10px 0 14px; border-bottom: 1px solid var(--color-divider); }
.filter select,
.filter input {
  padding: 12px 14px;
  border-radius: 14px;
}
.filter select { flex: 0 0 auto; }
.filter input { flex: 1; min-width: 120px; }

/* 来源切换（全部/实例/系统）：诊断多看实例日志，与系统日志分流（方案 A 按 target 前缀）。 */
.seg { display: flex; border: 1px solid var(--chip-border); border-radius: 14px; overflow: hidden; }
.seg button {
  padding: 12px 14px;
  border: none;
  background: transparent;
  color: var(--color-ink-muted);
  font-size: 12px;
  cursor: pointer;
}
.seg button + button { border-left: 1px solid var(--chip-border); }
.seg button.on { background: var(--color-primary); color: #fff; }

/* 表头与数据行同列模板（时间/级别固定宽，来源 220px，详情弹性）。 */
.head,
.entry {
  display: grid;
  grid-template-columns: 84px 56px 220px minmax(0, 1fr);
  gap: 10px;
  padding: 0 10px;
  font-size: 12px;
  font-family: Consolas, monospace;
}
.head {
  height: 30px;
  align-items: center;
  color: var(--color-ink-muted);
  border-bottom: 1px solid var(--color-divider);
  font-size: 11px;
  letter-spacing: 0.06em;
}
.list { flex: 1; overflow-y: auto; }
.entry {
  height: 36px; /* 与 ROW_H 常量一致（虚拟滚动固定行高） */
  align-items: center;
  border-bottom: 1px solid var(--card-border);
}
.entry:hover { background: var(--table-row-hover); }
.entry.error { color: var(--color-danger); } .entry.warn { color: var(--color-warning); }
.ts, .tg { color: var(--color-ink-muted); }
/* 单行截断：min-width:0 是 grid 项 overflow 生效的前提（默认 min-width:auto
   会让长 target 溢出盖住详情列——修"第 3 列覆盖"问题）。全文悬停 title 可看。 */
.ts, .lv, .tg, .msg {
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.empty { color: var(--color-ink-muted); padding: 20px; text-align: center; }
</style>
