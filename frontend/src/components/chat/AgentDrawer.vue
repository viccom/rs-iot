<!-- frontend/src/components/chat/AgentDrawer.vue（占位，M3 实现） -->
<script setup lang="ts">
defineEmits<{ close: [] }>()
</script>
<template>
  <aside class="drawer">
    <button @click="$emit('close')">关闭</button>
    AI 面板（M3 实现）
  </aside>
</template>
<style scoped>
.drawer {
  position: fixed;
  right: 0; top: 0; bottom: 0;
  width: 380px;
  background: var(--color-surface-raised);
  border-left: 1px solid var(--color-divider);
  padding: 12px;
}
</style>
