<!-- frontend/src/components/monitor/RtdataPanel.vue
  实时点位表（模块 M2c）：虚拟滚动 + tag/类型过滤。
  - 虚拟滚动：固定行高，只渲染可视区行（大点数不卡），scrollTop 计算可视范围 + 上下 padding 占位。
  - 过滤：dev（沿用 store.selectedDevice）+ tag 模糊 + 类型（从 monitor.instanceConfigs 建 devId→{tag→type} 映射）。
  - 类型不在 rtdata cell（lux 不存原始类型），从 ChildSpec 反查（同模块 2 后端决策）。
-->
<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { RouterLink } from 'vue-router'
import { useMonitorStore } from '../../stores/monitor'

const monitor = useMonitorStore()

/** 行高（虚拟滚动固定值，改时同步 CSS .row 的 height）。 */
const ROW_H = 40
/** 可视区上下缓冲行数（防快速滚动闪白）。 */
const BUFFER = 6

const tagFilter = ref('')
const typeFilter = ref('')
/** 滚动容器 scrollTop（虚拟滚动计算可视范围）。 */
const scrollTop = ref(0)
/** 滚动容器可视高度（决定渲染行数）。 */
const viewportH = ref(480)

/** devId → { tag → type } 映射（从 instanceConfigs 的 devices.children 建）。
 *  rtdata cell 无类型，类型在 ChildSpec（实例配置）。 */
const typeMap = computed(() => {
  const m: Record<string, Record<string, string>> = {}
  for (const cfg of Object.values(monitor.instanceConfigs)) {
    for (const dev of cfg.devices) {
      const devId = dev.value
      if (!devId) continue
      m[devId] ??= {}
      for (const ch of dev.children) {
        const tag = ch.value.split('.').pop() || ch.value
        if (ch.type) m[devId][tag] = ch.type
      }
    }
  }
  return m
})

/** devId → 归属实例 id 映射（C 修复：设备页展示设备归属哪个实例）。
 *  与 typeMap 同源遍历（instanceConfigs），同 dev 多实例引用时首个胜出。 */
const devOwner = computed(() => {
  const m: Record<string, string> = {}
  for (const cfg of Object.values(monitor.instanceConfigs)) {
    for (const dev of cfg.devices) {
      if (!dev.value) continue
      m[dev.value] ??= cfg.inst_id
    }
  }
  return m
})

/** 已知类型集合（类型下拉选项，从 typeMap 收集）。 */
const knownTypes = computed(() => {
  const s = new Set<string>()
  for (const tags of Object.values(typeMap.value)) {
    for (const t of Object.values(tags)) s.add(t)
  }
  return Array.from(s).sort()
})

/** 扁平化 + 过滤后的行（计算属性，依赖 rtdata/selectedDevice/tagFilter/typeFilter）。 */
const rows = computed(() => {
  const all = monitor.rtdata
  const result: { dev: string; tag: string; value: string; ts: string; type: string; owner: string }[] = []
  for (const [dev, tags] of Object.entries(all)) {
    if (!dev) continue  // 空 devId 数据（实例配置设备 ID 未填）不展示，入口处已加校验
    if (monitor.selectedDevice && dev !== monitor.selectedDevice) continue
    const devTypes = typeMap.value[dev] ?? {}
    const owner = devOwner.value[dev] || ''
    for (const [field, cell] of Object.entries(tags)) {
      // field 形如 <devid>.<tag>，取末段还原 tag。
      const tag = field.split('.').pop() || field
      if (tagFilter.value && !tag.toLowerCase().includes(tagFilter.value.toLowerCase())) continue
      const type = devTypes[tag] || ''
      if (typeFilter.value && type !== typeFilter.value) continue
      const cellArr = cell as [number, string, string | number]
      result.push({ dev, tag, value: String(cellArr[2]), ts: cellArr[1], type, owner })
    }
  }
  return result
})

/** 虚拟滚动：总高度（占位撑开滚动条）。 */
const totalH = computed(() => rows.value.length * ROW_H)
/** 可视起始行（夹到 [0, len)）。 */
const startIdx = computed(() => Math.max(0, Math.floor(scrollTop.value / ROW_H) - BUFFER))
/** 可视行数（视口能容纳 + 双倍缓冲）。 */
const visibleCount = computed(() => Math.ceil(viewportH.value / ROW_H) + BUFFER * 2)
/** 实际渲染的行切片。 */
const visibleRows = computed(() => rows.value.slice(startIdx.value, startIdx.value + visibleCount.value))
/** 上方占位高度（把可视行推到正确位置）。 */
const padTop = computed(() => startIdx.value * ROW_H)
/** 下方占位高度（撑总高度）。 */
const padBottom = computed(() => Math.max(0, totalH.value - padTop.value - visibleRows.value.length * ROW_H))

/** 滚动事件：更新 scrollTop（虚拟滚动核心）。 */
function onScroll(e: Event) {
  const el = e.target as HTMLElement
  scrollTop.value = el.scrollTop
}

/** 滚动容器元素（模板 ref，挂载后取初始高度 + ResizeObserver 跟踪窗口变化）。 */
const viewportEl = ref<HTMLElement | null>(null)
/** ResizeObserver 句柄（卸载时断开）。 */
let ro: ResizeObserver | null = null

onMounted(() => {
  const el = viewportEl.value
  if (!el) return
  viewportH.value = el.clientHeight || 480
  // 窗口/布局变化时同步视口高度——虚拟滚动渲染行数依赖它，不同步则放大窗口后下方空白。
  ro = new ResizeObserver(entries => {
    for (const en of entries) {
      if (en.contentRect.height > 0) viewportH.value = en.contentRect.height
    }
  })
  ro.observe(el)
})

onUnmounted(() => {
  ro?.disconnect()
  ro = null
})
</script>

<template>
  <div class="rtdata">
    <div class="filter">
      <input v-model="tagFilter" placeholder="按 tag 名过滤" />
      <select v-model="typeFilter">
        <option value="">全部类型</option>
        <option v-for="t in knownTypes" :key="t" :value="t">{{ t }}</option>
      </select>
      <span class="filter-count">{{ rows.length }} 条</span>
    </div>

    <div
      ref="viewportEl"
      class="scroll-area"
      @scroll="onScroll"
    >
      <div class="spacer" :style="{ height: totalH + 'px', paddingTop: padTop + 'px', paddingBottom: padBottom + 'px' }">
        <table>
          <thead>
            <tr><th>设备</th><th>Tag</th><th>值</th><th>类型</th><th>归属实例</th><th>时间</th></tr>
          </thead>
          <tbody>
            <tr v-for="r in visibleRows" :key="r.dev + '.' + r.tag" class="row">
              <td>{{ r.dev }}</td>
              <td>{{ r.tag }}</td>
              <td class="val">{{ r.value }}</td>
              <td class="type">{{ r.type || '—' }}</td>
              <td class="owner">{{ r.owner || '—' }}</td>
              <td class="ts">{{ r.ts }}</td>
            </tr>
          </tbody>
        </table>
      </div>
    </div>

    <p v-if="!rows.length" class="empty">
      无数据——<RouterLink to="/collectors" class="empty-link">检查采集器是否运行</RouterLink>或调整过滤条件
    </p>
  </div>
</template>

<style scoped>
.rtdata { display: flex; flex-direction: column; height: 100%; overflow: hidden; }

.filter {
  display: flex; gap: 10px; align-items: center;
  padding: 10px 0 14px;
  border-bottom: 1px solid var(--color-divider);
}
.filter input, .filter select {
  padding: 8px 12px; border-radius: 10px;
  border: 1px solid var(--card-border); background: var(--card-bg); color: var(--color-ink);
  font-size: 13px;
}
.filter input { flex: 1; }
.filter select { min-width: 110px; }

.filter-count {
  padding: 6px 12px; border-radius: 999px;
  background: var(--chip-bg); border: 1px solid var(--chip-border);
  color: var(--chip-ink); font-size: 12px; white-space: nowrap;
}

.scroll-area {
  flex: 1; overflow-y: auto;
  scrollbar-width: thin;
  scrollbar-color: rgba(148, 163, 184, 0.6) transparent;
  margin-top: 8px;
}

table { width: 100%; border-collapse: collapse; font-size: 13px; table-layout: fixed; }
/* 6 列定宽：时间/设备/归属固定，Tag/值弹性——避免均分下时间戳被挤压截断。
   文本超宽 ellipsis（long 字段不撑破行高）。 */
th:nth-child(1), td:nth-child(1) { width: 110px; }
th:nth-child(4), td:nth-child(4) { width: 56px; }
th:nth-child(5), td:nth-child(5) { width: 110px; }
th:nth-child(6), td:nth-child(6) { width: 148px; }
th:nth-child(n), td:nth-child(n) { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
thead th {
  position: sticky; top: 0; z-index: 1;
  background: var(--table-head-bg); color: var(--color-ink-secondary);
  letter-spacing: 0.06em; text-transform: uppercase; font-size: 11px;
  text-align: left; padding: 10px;
}
th, td { text-align: left; padding: 0 10px; }
/* 固定行高（虚拟滚动依赖，改时同步 ROW_H 常量）。 */
.row { height: 40px; border-bottom: 1px solid var(--card-border); transition: background-color 0.12s; }
.row:hover { background: var(--table-row-hover); }

.val { font-family: Consolas, monospace; color: var(--chip-ink); }
.type { color: var(--color-ink-muted); font-size: 12px; }
.owner { color: var(--color-primary); font-size: 12px; }
.ts { color: var(--color-ink-muted); font-family: Consolas, monospace; }

.empty { color: var(--color-ink-muted); padding: 20px; text-align: center; }
.empty-link { color: var(--color-primary); }
.empty-link:hover { text-decoration: underline; }
</style>
