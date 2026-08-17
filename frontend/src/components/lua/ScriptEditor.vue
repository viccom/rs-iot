<!-- frontend/src/components/lua/ScriptEditor.vue
  lua 脚本编辑器（CodeMirror 6 wrapper）。
  - 支持 lua 语法高亮（@codemirror/legacy-modes/mode/lua，StreamLanguage 加载）+ 行号。
  - v-model 同步 content：外部切脚本时换 EditorState，编辑时 dispatch 通知父组件。
  - 轻量（~200KB，ESM tree-shake）；Monaco 过重不用。
-->
<script setup lang="ts">
import { onMounted, onUnmounted, ref, watch } from 'vue'
import { EditorState, type Extension } from '@codemirror/state'
import { EditorView, lineNumbers } from '@codemirror/view'
import { defaultHighlightStyle, syntaxHighlighting } from '@codemirror/language'
import { lua } from '@codemirror/legacy-modes/mode/lua'
import { StreamLanguage } from '@codemirror/language'

const props = defineProps<{
  /** 脚本内容（v-model）。 */
  modelValue: string
  /** 是否只读（加载中时）。 */
  readonly?: boolean
}>()
const emit = defineEmits<{
  /** 内容变更（v-model + dirty 标记）。 */
  (e: 'update:modelValue', value: string): void
  /** 内容改动（父组件标 dirty）。 */
  (e: 'dirty'): void
}>()

/** 编辑器挂载容器。 */
const host = ref<HTMLElement | null>(null)
let view: EditorView | null = null

/** CodeMirror 扩展：lua 高亮 + 行号 + 主题适配（用默认高亮 + 基础样式）。 */
function extensions(): Extension[] {
  return [
    lineNumbers(),
    StreamLanguage.define(lua),
    syntaxHighlighting(defaultHighlightStyle),
    EditorView.lineWrapping,
    EditorView.theme({
      '&': { height: '100%', fontSize: '13px', backgroundColor: 'var(--card-bg)' },
      '.cm-content': { fontFamily: 'Consolas, monospace', color: 'var(--color-ink)' },
      '.cm-gutters': { backgroundColor: 'var(--color-surface-hover)', color: 'var(--color-ink-muted)', border: 'none' },
      '&.cm-focused': { outline: 'none' },
    }),
    EditorView.updateListener.of(u => {
      if (u.docChanged) {
        emit('update:modelValue', u.state.doc.toString())
        emit('dirty')
      }
    }),
  ]
}

onMounted(() => {
  if (!host.value) return
  view = new EditorView({
    state: EditorState.create({ doc: props.modelValue, extensions: extensions() }),
    parent: host.value,
  })
})

/** 外部 modelValue 变化（切脚本/loadScript）时换 doc——仅当与当前 doc 不同才换，避免循环。 */
watch(() => props.modelValue, v => {
  if (!view) return
  if (view.state.doc.toString() !== v) {
    view.dispatch({
      changes: { from: 0, to: view.state.doc.length, insert: v },
    })
  }
})

/** readonly 变化时重设 extensions（CM 无直接 toggle，重新 dispatch facet 较繁，简化：重建）。 */
watch(() => props.readonly, () => {
  if (!view || !host.value) return
  const doc = view.state.doc.toString()
  view.destroy()
  view = new EditorView({
    state: EditorState.create({
      doc,
      extensions: [...extensions(), EditorState.readOnly.of(props.readonly)],
    }),
    parent: host.value,
  })
})

onUnmounted(() => view?.destroy())
</script>

<template>
  <div ref="host" class="cm-host" :class="{ readonly }" />
</template>

<style scoped>
.cm-host {
  height: 100%; min-height: 320px;
  border: 1px solid var(--card-border); border-radius: 10px;
  overflow: hidden; background: var(--card-bg);
}
.cm-host :deep(.cm-editor) { height: 100%; }
.cm-host :deep(.cm-scroller) { overflow: auto; }
.readonly { opacity: 0.7; }
</style>
