// frontend/src/lib/virtualRows.ts
// 固定行高虚拟滚动窗口计算（纯函数，composables/useVirtualRows 消费）。
// 日志/报文列表高频前插 + 数百条规模下，只渲染可视区 ±overscan 行。

export interface VirtualRange {
  start: number
  end: number
}

/** 计算可视窗口 [start, end)。
 *  - scrollTop/viewportH：滚动容器当前值；rowHeight 固定行高；overscan 上下各多渲染行数。
 *  - 头尾夹取不越界；total=0 / rowHeight=0 / 负 scrollTop 防御返回空窗口。 */
export function visibleRange(
  scrollTop: number,
  viewportH: number,
  total: number,
  rowHeight: number,
  overscan = 8,
): VirtualRange {
  if (total <= 0 || rowHeight <= 0) return { start: 0, end: 0 }
  const start = Math.max(0, Math.floor(scrollTop / rowHeight) - overscan)
  const end = Math.min(
    total,
    Math.ceil((Math.max(0, scrollTop) + viewportH) / rowHeight) + overscan,
  )
  return { start, end: Math.max(start, end) }
}
