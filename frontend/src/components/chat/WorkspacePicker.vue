<!-- frontend/src/components/chat/WorkspacePicker.vue
  工作区引用选择器：从 reasonix 工作区挑文件/文件夹，以 @ 引用插入消息。
  一次只列一层（gateway /api/agent/workspace 沙箱 + 噪音目录过滤），
  目录点击下钻；引用当前文件夹由底部按钮承担。视觉约定对齐 ProviderModal。
-->
<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useAgentStore } from '../../stores/agent'

const agent = useAgentStore()
const emit = defineEmits<{ select: [path: string]; close: [] }>()

/** 当前目录（工作区相对路径，"" = 根）。 */
const path = ref('')
const entries = ref<{ name: string; is_dir: boolean }[]>([])
const loading = ref(false)
const errorMsg = ref('')

const crumbs = computed(() => {
  const segs = path.value ? path.value.split('/') : []
  return [{ label: '工作区', p: '' }, ...segs.map((s, i) => ({ label: s, p: segs.slice(0, i + 1).join('/') }))]
})

async function load() {
  loading.value = true
  errorMsg.value = ''
  try {
    entries.value = await agent.listWorkspace(path.value)
  } catch (e) {
    errorMsg.value = e instanceof Error ? e.message : '读取目录失败'
  } finally {
    loading.value = false
  }
}

function enter(p: string) {
  if (p === path.value) return
  path.value = p
  void load()
}

/** 选中条目：文件直接引用；目录下钻。 */
function onEntry(e: { name: string; is_dir: boolean }) {
  if (e.is_dir) {
    enter(path.value ? `${path.value}/${e.name}` : e.name)
  } else {
    emit('select', path.value ? `${path.value}/${e.name}` : e.name)
  }
}

onMounted(load)
</script>

<template>
  <div class="modal-mask" role="dialog" aria-modal="true">
    <div class="modal-card">
      <div class="head">
        <h3 class="title">引用文件或文件夹</h3>
        <button class="btn-icon" aria-label="关闭" @click="emit('close')">✕</button>
      </div>
      <p class="subtitle">从当前工作区选择上下文——选中的路径以 @ 引用插入消息，AI 可读取其内容。</p>

      <nav class="crumbs" aria-label="目录层级">
        <template v-for="(c, i) in crumbs" :key="c.p">
          <button class="crumb" :disabled="loading" @click="enter(c.p)">{{ c.label }}</button>
          <span v-if="i < crumbs.length - 1" class="crumb-sep">/</span>
        </template>
      </nav>

      <p v-if="errorMsg" class="error" role="alert">{{ errorMsg }}</p>
      <p v-if="loading" class="empty">读取中…</p>

      <div class="entry-list" v-else>
        <p v-if="!entries.length && !errorMsg" class="empty">此目录为空。</p>
        <button
          v-for="e in entries"
          :key="e.name"
          class="entry"
          :title="e.is_dir ? '打开目录' : '引用此文件'"
          @click="onEntry(e)"
        >
          <span class="entry-icon">{{ e.is_dir ? '📁' : '📄' }}</span>
          <span class="entry-name">{{ e.name }}</span>
        </button>
      </div>

      <div class="actions">
        <button class="btn btn-later" @click="emit('close')">取消</button>
        <button class="btn btn-answer" :disabled="!path || loading" title="以 @ 引用整个当前目录" @click="emit('select', path)">
          引用此文件夹
        </button>
      </div>
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
  width: 480px; max-width: calc(100vw - 32px);
  max-height: calc(100vh - 64px); overflow-y: auto;
  background: var(--color-surface-raised);
  border-radius: 14px;
  box-shadow: 0 10px 30px rgba(15, 23, 42, 0.2);
  padding: 20px;
  display: flex; flex-direction: column; gap: 12px;
}
.head { display: flex; align-items: center; justify-content: space-between; }
.title { margin: 0; font-size: 16px; font-weight: 600; color: var(--color-ink); }
.subtitle { margin: 0; font-size: 12px; color: var(--color-ink-muted); }
.btn-icon {
  background: transparent; border: none; font-size: 14px; cursor: pointer;
  color: var(--color-ink-muted); padding: 4px 8px; border-radius: 6px;
}
.btn-icon:hover { background: var(--color-surface-hover); color: var(--color-ink); }

.crumbs {
  display: flex; align-items: center; gap: 2px; flex-wrap: wrap;
  font-size: 12px; padding: 6px 8px;
  background: var(--color-surface-hover); border-radius: 8px;
}
.crumb {
  background: transparent; border: none; font-size: 12px; cursor: pointer;
  color: var(--color-ink-secondary); padding: 2px 6px; border-radius: 4px;
}
.crumb:hover:not(:disabled) { background: var(--card-bg); color: var(--color-ink); }
.crumb:disabled { opacity: 0.5; cursor: not-allowed; }
.crumb-sep { color: var(--color-ink-muted); }

.error {
  margin: 0; padding: 8px 12px; font-size: 12px;
  color: var(--color-danger); background: rgba(220, 38, 38, 0.06);
  border: 1px solid rgba(220, 38, 38, 0.2); border-radius: 8px;
}

.entry-list {
  display: flex; flex-direction: column; gap: 2px;
  min-height: 120px; max-height: 320px; overflow-y: auto;
}
.entry {
  display: flex; align-items: center; gap: 8px;
  background: transparent; border: none; cursor: pointer;
  font-size: 13px; color: var(--color-ink);
  padding: 7px 10px; border-radius: 8px; text-align: left;
}
.entry:hover { background: var(--color-surface-hover); }
.entry-icon { flex-shrink: 0; }
.entry-name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.empty { margin: 8px 0; font-size: 13px; color: var(--color-ink-muted); text-align: center; }

.actions { display: flex; justify-content: flex-end; gap: 10px; }
.btn {
  font-size: 13px; padding: 8px 16px; border-radius: 8px; cursor: pointer;
  border: 1px solid var(--card-border);
}
.btn-later { background: var(--card-bg); color: var(--color-ink-secondary); }
.btn-answer {
  background: var(--color-primary); color: #fff; border-color: var(--color-primary);
}
.btn-answer:disabled { opacity: 0.5; cursor: not-allowed; }
</style>
