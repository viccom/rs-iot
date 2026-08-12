<!-- frontend/src/components/chat/MessageItem.vue
  单条消息渲染。user 右对齐气泡；assistant 左对齐含思考折叠面板 + 流式文本 + 工具卡。
  对标 zacp MessageItem 的视觉结构与交互（角色布局、思考折叠、流式占位动画、工具卡），
  适配本项目简化的 ChatMessage（text/reasoning/toolCalls 为直接字段，无 events JSON）。
  与 zacp 的关键差异：
  - 无 IncremarkContent（markdown 渲染库），文本走 whitespace-pre-wrap 纯文本；
  - 无 sessionStore / vue-i18n / 按需加载 thoughts，reasoning 直接来自 message.reasoning（流式实时累加）；
  - 无 events 时间线分块（本项目 message 已扁平化，文本与工具调用分别渲染，不交错）。
-->
<script lang="ts">
import { inject, type InjectionKey, type Ref } from 'vue'
/** AgentDrawer 向消息子组件广播 turn-running 态的注入键（summary 活跃色 + 加载占位）。 */
export const runningKey: InjectionKey<Ref<boolean>> = Symbol('agent-running')
</script>

<script setup lang="ts">
import { computed, nextTick, ref, watch } from 'vue'
import type { ChatMessage } from '../../types'
import ToolCallCard from './ToolCallCard.vue'

const props = defineProps<{ message: ChatMessage }>()

/** 是否用户消息（右对齐气泡）。 */
const isUser = computed(() => props.message.role === 'user')

/** 思考过程滚动容器（展开后限高滚动，避免长思考撑开页面）。 */
const reasoningBodyRef = ref<HTMLElement | null>(null)

/** 是否有思考过程（决定折叠面板是否展示）。 */
const hasReasoning = computed(() => !!props.message.reasoning && props.message.reasoning.length > 0)

/**
 * 是否为流式进行中的 assistant 消息（turn 未结束）。
 * AgentDrawer 经 provide(runningKey) 广播一个 Ref<boolean>；无注入源时回退静态 false。
 * 用于：思考面板 summary 显示「思考中…」活跃态 + 文本未到达时显示加载圆点。
 */
const runningRef = inject(runningKey, ref(false))
/** running 的 boolean 视图（模板与 watch 用，避免 .value 散落）。 */
const running = computed(() => runningRef.value)

/** 文本未到达时的加载态：assistant + running + 无 text + 无 toolCalls。 */
const showLoading = computed(() =>
  !isUser.value &&
  running.value &&
  !props.message.text &&
  !(props.message.toolCalls && props.message.toolCalls.length > 0),
)

/** 流式期间 reasoning 持续追加：若用户已展开面板，滚动容器跟随到底。 */
watch(
  () => props.message.reasoning?.length ?? 0,
  async () => {
    if (!running.value) return
    await nextTick()
    reasoningBodyRef.value?.scrollTo({ top: reasoningBodyRef.value.scrollHeight })
  },
)
</script>

<template>
  <div class="msg-row" :class="isUser ? 'is-user' : 'is-assistant'">
    <!-- 思考折叠面板（assistant + 有 reasoning 才显示） -->
    <details v-if="!isUser && hasReasoning" class="reasoning">
      <summary class="reasoning-summary" :class="{ active: running }">
        <span>{{ running ? '思考中' : '思考过程' }}</span>
        <span v-if="running" class="dots" aria-hidden="true">
          <span class="dot dot-sm" /><span class="dot dot-sm" /><span class="dot dot-sm" />
        </span>
      </summary>
      <div ref="reasoningBodyRef" class="reasoning-body">{{ message.reasoning }}</div>
    </details>

    <!-- user：右对齐气泡 -->
    <div v-if="isUser" class="bubble-user">{{ message.text }}</div>

    <!-- assistant 文本 -->
    <div v-else-if="message.text" class="bubble-assistant">{{ message.text }}</div>

    <!-- 工具调用卡片（每条独立卡，紧跟文本之后） -->
    <ToolCallCard
      v-for="tc in message.toolCalls"
      :key="tc.id"
      :tool-call="tc"
      class="tool-wrap"
    />

    <!-- 流式加载占位（assistant + running + 内容未到达） -->
    <div v-if="showLoading" class="loading" aria-label="loading">
      <span class="dot" /><span class="dot" /><span class="dot" />
    </div>
  </div>
</template>

<style scoped>
.msg-row { display: flex; flex-direction: column; gap: 8px; margin-bottom: 12px; }
.msg-row.is-user { align-items: flex-end; }
.msg-row.is-assistant { align-items: flex-start; }

/* user 气泡：天蓝底 + 右下圆角收敛（对标 zacp sky-50） */
.bubble-user {
  max-width: 85%;
  white-space: pre-wrap; word-break: break-word;
  padding: 8px 12px;
  font-size: 13px; line-height: 1.6; color: var(--color-ink);
  background: #e0f2fe;
  border: 1px solid #bae6fd;
  border-radius: 16px;
  border-bottom-right-radius: 6px;
}

/* assistant 文本：白底卡片，占整宽（对标 zacp AI 内容框） */
.bubble-assistant {
  width: 100%; min-width: 0;
  white-space: pre-wrap; word-break: break-word;
  padding: 12px 16px;
  font-size: 13px; line-height: 1.6; color: var(--color-ink);
  background: var(--color-surface-raised);
  border: 1px solid var(--color-divider);
  border-radius: 12px;
  box-shadow: 0 1px 2px rgba(15, 23, 42, 0.04);
}

/* 思考折叠面板：琥珀色高亮（对标 zacp amber-50）。 */
.reasoning {
  width: 100%;
  border-radius: 8px;
  background: rgba(254, 243, 199, 0.6);
  border: 1px solid rgba(252, 211, 77, 0.3);
  padding: 6px 12px;
  font-size: 12px;
  color: var(--color-ink-muted);
}
.reasoning-summary {
  cursor: pointer; user-select: none;
  font-weight: 500;
  color: var(--color-ink-muted);
  display: inline-flex; align-items: center; gap: 6px;
}
.reasoning-summary.active { color: var(--color-warning); }
.reasoning-body {
  margin-top: 6px;
  max-height: 320px; overflow-y: auto;
  white-space: pre-wrap; word-break: break-word;
  padding-right: 8px;
  line-height: 1.6;
  scrollbar-width: thin;
  scrollbar-color: rgba(203, 213, 225, 0.9) transparent;
}

.tool-wrap { width: 100%; }

/* 加载动画圆点（流式占位 + 思考中 summary） */
.loading {
  display: flex; align-items: center; gap: 6px;
  padding: 14px 16px; width: 100%;
  background: var(--color-surface-raised);
  border: 1px solid var(--color-divider);
  border-radius: 12px;
  box-shadow: 0 1px 2px rgba(15, 23, 42, 0.04);
}
.dot {
  width: 6px; height: 6px; border-radius: 9999px;
  background-color: #94a3b8;
  animation: dot-bounce 1.2s ease-in-out infinite;
}
.dot:nth-child(2) { animation-delay: 0.15s; }
.dot:nth-child(3) { animation-delay: 0.3s; }
.dot-sm { width: 4px; height: 4px; background-color: var(--color-warning); }
.dots { display: inline-flex; align-items: center; gap: 3px; }
.dots .dot { animation-delay: 0s; }
.dots .dot:nth-child(2) { animation-delay: 0.15s; }
.dots .dot:nth-child(3) { animation-delay: 0.3s; }
@keyframes dot-bounce {
  0%, 60%, 100% { transform: translateY(0); opacity: 0.45; }
  30% { transform: translateY(-4px); opacity: 1; }
}
</style>
