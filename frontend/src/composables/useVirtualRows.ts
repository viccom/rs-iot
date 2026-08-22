// frontend/src/composables/useVirtualRows.ts
// 固定行高虚拟滚动（LogStream/PacketTrace 共用）：只渲染可视区 ±overscan 行，
// 上下 pad spacer 撑总高。日志/报文高频前插（最新在顶）下配合 compensatePrepend
// 保持"看历史时视口内容不动"。
import { computed, onBeforeUnmount, onMounted, ref, type Ref } from 'vue'
import { visibleRange } from '../lib/virtualRows'

export function useVirtualRows(total: Ref<number>, rowHeight: number, overscan = 8) {
  /** 滚动容器（模板 ref 绑定 .list）。 */
  const containerRef = ref<HTMLElement | null>(null)
  const scrollTop = ref(0)
  const viewportH = ref(480)
  let ro: ResizeObserver | null = null

  // 面板高度随窗口变化（clamp 布局），ResizeObserver 跟随——首帧 mounted 即回调取值。
  onMounted(() => {
    const el = containerRef.value
    if (!el || typeof ResizeObserver === 'undefined') return
    ro = new ResizeObserver(() => { viewportH.value = el.clientHeight })
    ro.observe(el)
  })
  onBeforeUnmount(() => { ro?.disconnect(); ro = null })

  const range = computed(() =>
    visibleRange(scrollTop.value, viewportH.value, total.value, rowHeight, overscan),
  )
  const start = computed(() => range.value.start)
  const end = computed(() => range.value.end)
  const padTop = computed(() => start.value * rowHeight)
  const padBottom = computed(() => (total.value - end.value) * rowHeight)

  function onScroll(e: Event) {
    scrollTop.value = (e.target as HTMLElement).scrollTop
  }

  /** 回顶（过滤条件变化后内容全换，保持滚动位置无意义）。 */
  function toTop() {
    scrollTop.value = 0
    if (containerRef.value) containerRef.value.scrollTop = 0
  }

  /** prepend 补偿：用户不在顶部（正在回看历史）时前插 added 行，抬 scrollTop
   *  使视口内索引区间保持原内容不跳；在顶部则不动（新条目自然进入视口）。 */
  function compensatePrepend(added: number) {
    const el = containerRef.value
    if (!el || added <= 0 || el.scrollTop <= 0) return
    el.scrollTop += added * rowHeight
    scrollTop.value = el.scrollTop
  }

  return { containerRef, onScroll, start, end, padTop, padBottom, toTop, compensatePrepend }
}
