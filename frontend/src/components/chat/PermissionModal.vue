<!-- frontend/src/components/chat/PermissionModal.vue
  权限审批弹窗：写操作触发 approval_request 时弹出，用户决策 approve/deny。
  对标 zacp PermissionModal 视觉（模态遮罩、卡片标题、工具信息区、决策按钮），
  适配本项目 agent store 的 approval 字段（id/tool/subject/reason，来自 serve eventwire.Approval）。
  与 zacp 差异：不走 sessionStore.resolvePermission，直接 emit(true/false) 由父组件调 store.approve。
  不可遮罩关闭/不可 Esc（agent 在等待决策）——对标 zacp 行为。
-->
<script setup lang="ts">
defineProps<{
  /** 待审批载荷（agent store approval）。 */
  approval: { id: string; tool: string; subject?: string; reason?: string }
}>()
const emit = defineEmits<{
  /** 用户决策：true 放行 / false 拒绝。父组件转发给 agent.approve。 */
  (e: 'approve', decision: boolean): void
}>()
</script>

<template>
  <div class="modal-mask" role="dialog" aria-modal="true" aria-labelledby="perm-title">
    <div class="modal-card">
      <h3 id="perm-title" class="title">权限请求</h3>
      <div class="info">
        <p class="tool-name">{{ approval.tool }}</p>
        <p v-if="approval.subject" class="subject">{{ approval.subject }}</p>
        <p v-if="approval.reason" class="reason">{{ approval.reason }}</p>
      </div>
      <div class="actions">
        <button class="btn btn-deny" @click="emit('approve', false)">拒绝</button>
        <button class="btn btn-approve" @click="emit('approve', true)">批准</button>
      </div>
      <p class="hint">Agent 正在等待你的决定。</p>
    </div>
  </div>
</template>

<style scoped>
.modal-mask {
  position: fixed; inset: 0;
  display: flex; align-items: center; justify-content: center;
  background: rgba(15, 23, 42, 0.45);
  z-index: 1000;
}
.modal-card {
  width: 420px; max-width: calc(100vw - 32px);
  background: var(--color-surface-raised);
  border-radius: 12px;
  box-shadow: 0 10px 30px rgba(15, 23, 42, 0.2);
  padding: 20px;
  display: flex; flex-direction: column; gap: 16px;
}
.title { margin: 0; font-size: 16px; font-weight: 600; color: var(--color-ink); }
.info {
  max-height: 200px; overflow-y: auto;
  padding: 10px 12px;
  background: var(--color-surface-hover);
  border-radius: 8px;
  font-size: 13px;
}
.tool-name { margin: 0; font-weight: 600; word-break: break-all; color: var(--color-ink); }
.subject { margin: 4px 0 0; word-break: break-word; color: var(--color-ink-secondary); }
.reason { margin: 4px 0 0; word-break: break-word; font-size: 12px; color: var(--color-ink-muted); }
.actions { display: flex; gap: 8px; }
.btn {
  flex: 1; padding: 9px 16px;
  font-size: 14px; font-weight: 500;
  border: 1px solid transparent; border-radius: 8px;
  transition: background-color 0.15s;
}
.btn-deny {
  background: var(--color-surface-hover);
  color: var(--color-ink-secondary);
  border-color: var(--color-divider);
}
.btn-deny:hover { background: var(--color-surface-active); }
.btn-approve {
  background: var(--color-primary);
  color: white;
}
.btn-approve:hover { background: var(--color-primary-hover); }
.hint { margin: 0; font-size: 12px; color: var(--color-ink-muted); text-align: center; }
</style>
