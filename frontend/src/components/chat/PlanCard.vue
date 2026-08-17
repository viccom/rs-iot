<!-- frontend/src/components/chat/PlanCard.vue
  计划卡（步骤列表 + 状态点）。
  当前 serve（DeepSeek-Reasonix eventwire）**无独立 plan 事件**：event.Kind 枚举里
  无 Plan，"plan" 仅是 Approval.Kind 的一个值（plan-mode 审批）；agent 的执行计划
  信息走 reasoning / text 流。故本组件**当前不在 MessageItem 中渲染**——保留作为
  未来 serve 新增 plan 事件时的落点（对标 zacp PlanCard 视觉先就位）。
  创建原因：Task10 文件清单要求；过度防御性不如等真实事件结构再补渲染接入点。
-->
<script setup lang="ts">
/** 计划步骤状态：pending 灰点 / in_progress 蓝色呼吸 / completed 绿勾 / failed 红点。 */
interface PlanStep {
  /** 步骤文本。 */
  text: string
  /** 步骤状态。 */
  status: 'pending' | 'in_progress' | 'completed' | 'failed'
}

defineProps<{
  /** 计划载荷（步骤列表）。 */
  plan: { steps: PlanStep[] }
}>()

/** 状态点 CSS 类。 */
function dotClass(status: PlanStep['status']): string {
  switch (status) {
    case 'completed': return 'dot-green'
    case 'in_progress': return 'dot-blue pulse'
    case 'failed': return 'dot-red'
    default: return 'dot-gray'
  }
}
</script>

<template>
  <div class="plan-card">
    <div class="plan-header">
      <span class="plan-icon" aria-hidden="true">📋</span>
      <span class="plan-title">执行计划</span>
    </div>
    <ol v-if="plan.steps.length" class="steps">
      <li v-for="(step, idx) in plan.steps" :key="idx" class="step">
        <span class="step-dot" :class="dotClass(step.status)">
          <span v-if="step.status === 'completed'" class="check" aria-hidden="true">✓</span>
        </span>
        <span class="step-text" :class="{ done: step.status === 'completed' }">{{ step.text }}</span>
      </li>
    </ol>
    <p v-else class="empty">暂无计划步骤</p>
  </div>
</template>

<style scoped>
.plan-card {
  overflow: hidden;
  border: 1px solid var(--color-divider);
  border-radius: 8px;
  background: var(--color-surface-raised);
  box-shadow: 0 1px 2px rgba(15, 23, 42, 0.04);
}
.plan-header {
  display: flex; align-items: center; gap: 8px;
  padding: 8px 12px;
  border-bottom: 1px solid var(--color-divider);
  font-size: 13px; font-weight: 500;
  color: var(--color-ink-secondary);
}
.plan-icon { opacity: 0.7; }
.steps { margin: 0; padding: 8px 12px; list-style: none; }
.step { display: flex; align-items: flex-start; gap: 10px; padding: 4px 0; font-size: 13px; }
.step-dot {
  flex-shrink: 0;
  display: inline-flex; align-items: center; justify-content: center;
  width: 16px; height: 16px; margin-top: 2px;
  border-radius: 9999px;
}
.check { font-size: 9px; color: white; }
.dot-green { background-color: #22c55e; }
.dot-blue { background-color: #3b82f6; }
.dot-red { background-color: #ef4444; }
.dot-gray { background-color: var(--color-divider); }
.pulse { animation: dot-pulse 1.5s ease-in-out infinite; }
.step-text {
  flex: 1; min-width: 0;
  word-break: break-word; line-height: 1.5;
  color: var(--color-ink-secondary);
}
.step-text.done { color: var(--color-ink-muted); text-decoration: line-through; }
.empty { margin: 0; padding: 12px; font-size: 13px; color: var(--color-ink-muted); }
@keyframes dot-pulse {
  0%, 100% { opacity: 1; }
  50% { opacity: 0.4; }
}
</style>
