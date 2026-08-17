<!-- frontend/src/components/chat/ToolCallCard.vue
  工具调用卡：标题行（状态点 + 工具名 + 状态文案 + 展开箭头）+ 折叠详情（参数/结果/错误）。
  对标 zacp ToolCallCard 视觉与交互（折叠详情、状态色、呼吸动画），适配本项目
  ChatMessage.toolCalls 元素类型（name/args/result/error/errorMsg/durationMs）。
  状态推断：有 result 或 error 为终态（停止呼吸）；有 id 无 result/error 为运行中。
-->
<script setup lang="ts">
import { computed, ref } from 'vue'
import type { ToolCallEntry } from '../../types'

const props = defineProps<{ toolCall: ToolCallEntry }>()

/** 详情展开/折叠（默认收起；无参数/结果/错误时禁用，仅展示标题行）。 */
const expanded = ref(false)

/** 终态判定：有 result 或 error 标记视为已完成（停止呼吸动画）。 */
const isTerminal = computed(() => props.toolCall.result !== undefined || !!props.toolCall.error)

/** 失败态（result.err 非空）：红色状态点 + 错误文案。 */
const isFailed = computed(() => !!props.toolCall.error)

/** 状态点 CSS 类：失败红 / 完成绿 / 运行中蓝（对标 zacp statusDotClass）。 */
const statusDotClass = computed(() => {
  if (isFailed.value) return 'dot-red'
  if (isTerminal.value) return 'dot-green'
  return 'dot-blue'
})

/** 状态文案：失败 → 错误 / 完成 → 完成 / 运行中 → 进行中。 */
const statusText = computed(() => {
  if (isFailed.value) return '错误'
  if (isTerminal.value) return '完成'
  return '进行中'
})

/** 详情文本最大长度（字符），超出截断防止超大输出撑爆渲染。 */
const MAX_DETAIL_CHARS = 50000

/** 把任意值格式化为展示文本：对象/数组 JSON 美化，字符串原样，空值空串。 */
function formatDetail(value: unknown): string {
  if (value === undefined || value === null) return ''
  if (typeof value === 'string') {
    // 尝试 JSON 美化（serve tool.args/output 常为 JSON 编码串）；非 JSON 原样返回。
    const trimmed = value.trim()
    if (trimmed.startsWith('{') || trimmed.startsWith('[')) {
      try {
        return JSON.stringify(JSON.parse(trimmed), null, 2)
      } catch {
        // 非合法 JSON：原样展示
      }
    }
    return value
  }
  try {
    return JSON.stringify(value, null, 2) ?? ''
  } catch {
    return String(value)
  }
}

/** 原始值非空判断（标题行箭头/禁用判断用）。 */
function hasValue(value: unknown): boolean {
  if (value === undefined || value === null) return false
  return typeof value !== 'string' || value.length > 0
}

/** 详情展示文本：折叠时返回 null 不执行格式化（避免大输出占用主线程）。 */
const details = computed(() => {
  if (!expanded.value) return null
  const input = formatDetail(props.toolCall.args)
  // 失败时优先展示 errorMsg（result 可能为空）；成功展示 result。
  const output = isFailed.value
    ? formatDetail(props.toolCall.errorMsg)
    : formatDetail(props.toolCall.result)
  return { input, output }
})

/** 是否有详情可展开（无参数/结果/错误时保持纯标题行）。 */
const hasDetail = computed(() =>
  hasValue(props.toolCall.args) ||
  hasValue(props.toolCall.result) ||
  !!props.toolCall.errorMsg,
)
</script>

<template>
  <div class="tool-card" :class="{ failed: isFailed }">
    <button
      type="button"
      class="title-row"
      :disabled="!hasDetail"
      :title="toolCall.name"
      @click="expanded = !expanded"
    >
      <span class="status-dot" :class="[statusDotClass, { pulse: !isTerminal }]">
        <span class="dot-ring" :class="statusDotClass" />
        <span class="dot-core" :class="statusDotClass" />
      </span>
      <span class="tool-icon" aria-hidden="true">🔧</span>
      <span class="tool-name">{{ toolCall.name }}</span>
      <span v-if="toolCall.durationMs !== undefined && isTerminal" class="duration">{{ toolCall.durationMs }}ms</span>
      <span class="status-text" :class="{ 'text-failed': isFailed }">{{ statusText }}</span>
      <span v-if="hasDetail" class="chevron" :class="{ open: expanded }" aria-hidden="true">▾</span>
    </button>
    <div v-show="expanded && hasDetail" class="detail">
      <div v-if="details?.input" class="detail-block">
        <div class="detail-label">参数</div>
        <pre class="detail-pre">{{ details.input.length > MAX_DETAIL_CHARS ? details.input.slice(0, MAX_DETAIL_CHARS) + '…(已截断)' : details.input }}</pre>
      </div>
      <div v-if="details?.output" class="detail-block">
        <div class="detail-label" :class="{ 'text-failed': isFailed }">{{ isFailed ? '错误' : '结果' }}</div>
        <pre class="detail-pre" :class="{ 'pre-failed': isFailed }">{{ details.output.length > MAX_DETAIL_CHARS ? details.output.slice(0, MAX_DETAIL_CHARS) + '…(已截断)' : details.output }}</pre>
      </div>
    </div>
  </div>
</template>

<style scoped>
.tool-card {
  overflow: hidden;
  border: 1px solid var(--color-divider);
  border-radius: 8px;
  background: var(--color-surface-raised);
  box-shadow: 0 1px 2px rgba(15, 23, 42, 0.04);
}
.tool-card.failed { border-color: rgba(220, 38, 38, 0.3); }
.title-row {
  display: flex; width: 100%; align-items: center; gap: 8px;
  padding: 8px 12px; text-align: left;
  background: transparent; border: none; font-size: 13px;
  transition: background-color 0.15s;
}
.title-row:hover:not(:disabled) { background: var(--color-surface-hover); }
.title-row:disabled { cursor: default; }
.status-dot {
  position: relative; display: inline-flex;
  width: 8px; height: 8px; flex-shrink: 0;
}
.dot-ring, .dot-core {
  position: absolute; display: inline-block; border-radius: 9999px;
}
.dot-ring { width: 8px; height: 8px; opacity: 0.75; }
.dot-core { width: 8px; height: 8px; position: relative; }
.dot-blue { background-color: #3b82f6; }
.dot-green { background-color: #22c55e; }
.dot-red { background-color: #ef4444; }
.pulse .dot-ring { animation: dot-pulse 1.5s ease-in-out infinite; }
.tool-icon { flex-shrink: 0; font-size: 13px; opacity: 0.7; }
.tool-name {
  flex: 1; min-width: 0;
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  font-weight: 500; color: var(--color-ink-secondary);
}
.duration { flex-shrink: 0; font-size: 11px; color: var(--color-ink-muted); }
.status-text { flex-shrink: 0; font-size: 11px; color: var(--color-ink-muted); }
.text-failed { color: var(--color-danger); }
.chevron {
  flex-shrink: 0; font-size: 10px; color: var(--color-ink-muted);
  transition: transform 0.2s;
}
.chevron.open { transform: rotate(180deg); }
.detail { border-top: 1px solid var(--color-divider); padding: 10px 12px; }
.detail-block { margin-bottom: 10px; }
.detail-block:last-child { margin-bottom: 0; }
.detail-label { margin-bottom: 4px; font-size: 11px; font-weight: 500; color: var(--color-ink-muted); }
.detail-pre {
  max-height: 240px; overflow: auto;
  margin: 0; padding: 8px 10px;
  white-space: pre-wrap; word-break: break-all;
  border-radius: 6px; background: var(--color-surface-hover);
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 12px; line-height: 1.5; color: var(--color-ink-secondary);
}
.pre-failed { color: var(--color-danger); background: rgba(220, 38, 38, 0.05); }
@keyframes dot-pulse {
  0% { transform: scale(1); opacity: 0.75; }
  50% { transform: scale(2); opacity: 0; }
  100% { transform: scale(1); opacity: 0.75; }
}
</style>
