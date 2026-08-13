<!-- frontend/src/components/chat/AskModal.vue
  AI 结构化提问决策弹窗（ask_request 事件）。AI 主动向用户提问（如遇到权限闸门/需要
  方向决策），回答前 turn 挂起——此前前端未接 ask 事件，AI 卡"思考中"且用户无决策
  入口（现场卡死 bug）。参照 PermissionModal 视觉（mask/card/actions），多问题逐个
  展示，单选/多选（multi）由问题声明。回答走 POST /api/agent/answer
  （serve ctl.AnswerQuestion，body {id, answers:[{question_id, selected}]}）。
-->
<script setup lang="ts">
import { computed, ref } from 'vue'
import type { AskQuestion } from '../../types'

const props = defineProps<{
  /** 待回答的提问载荷（agent store ask）。 */
  ask: { id: string; questions: AskQuestion[] }
}>()
const emit = defineEmits<{
  /** 提交回答（父组件调 store.answer）。 */
  (e: 'answer', answers: { question_id: string; selected: string[] }[]): void
  /** 关闭（不回答，turn 保持挂起——AI 仍在等，用户可稍后再答）。 */
  (e: 'close'): void
}>()

/** 每题当前选中 label 列表（单选存 1 项，多选可多项）。 */
const selected = ref<Record<string, string[]>>({})

/** 每题是否已选（提交按钮使能条件：全部问题至少 1 项）。 */
const allAnswered = computed(() =>
  props.ask.questions.every(q => (selected.value[q.id]?.length ?? 0) > 0),
)

/** 切换单选：替换整组。 */
function pickSingle(qid: string, label: string) {
  selected.value[qid] = [label]
}
/** 切换多选：toggle 项。 */
function toggleMulti(qid: string, label: string, multi: boolean) {
  if (!multi) {
    pickSingle(qid, label)
    return
  }
  const cur = selected.value[qid] ?? []
  selected.value[qid] = cur.includes(label) ? cur.filter(l => l !== label) : [...cur, label]
}

/** 提交。 */
function submit() {
  const answers = props.ask.questions
    .filter(q => (selected.value[q.id]?.length ?? 0) > 0)
    .map(q => ({ question_id: q.id, selected: selected.value[q.id] }))
  if (!answers.length) return
  emit('answer', answers)
}
</script>

<template>
  <div class="modal-mask" role="dialog" aria-modal="true">
    <div class="modal-card">
      <h3 class="title">AI 需要你的决策</h3>
      <p class="subtitle">回答前任务保持挂起。</p>

      <div class="questions">
        <section v-for="q in ask.questions" :key="q.id" class="question">
          <p v-if="q.header" class="q-header">{{ q.header }}</p>
          <p class="q-prompt">{{ q.prompt }}</p>
          <div class="options">
            <button
              v-for="opt in q.options"
              :key="opt.label"
              class="option"
              :class="{ selected: (selected[q.id] ?? []).includes(opt.label) }"
              @click="toggleMulti(q.id, opt.label, !!q.multi)"
            >
              <span class="mark">{{ q.multi ? '☐' : '○' }}</span>
              <span class="opt-copy">
                <strong>{{ opt.label }}</strong>
                <em v-if="opt.description">{{ opt.description }}</em>
              </span>
            </button>
          </div>
        </section>
      </div>

      <div class="actions">
        <button class="btn btn-later" @click="emit('close')">稍后再说</button>
        <button class="btn btn-answer" :disabled="!allAnswered" @click="submit">提交回答</button>
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
  width: 560px; max-width: calc(100vw - 32px);
  max-height: calc(100vh - 64px); overflow-y: auto;
  background: var(--color-surface-raised);
  border-radius: 14px;
  box-shadow: 0 10px 30px rgba(15, 23, 42, 0.2);
  padding: 20px;
  display: flex; flex-direction: column; gap: 14px;
}
.title { margin: 0; font-size: 16px; font-weight: 600; color: var(--color-ink); }
.subtitle { margin: 0; font-size: 12px; color: var(--color-ink-muted); }

.questions { display: flex; flex-direction: column; gap: 14px; }
.question {
  padding: 12px;
  border-radius: 10px;
  background: var(--color-surface-hover);
}
.q-header { margin: 0 0 4px; font-size: 13px; font-weight: 600; color: var(--color-ink); }
.q-prompt { margin: 0 0 10px; font-size: 13px; color: var(--color-ink-secondary); line-height: 1.6; }

.options { display: flex; flex-direction: column; gap: 6px; }
.option {
  display: flex; gap: 10px; align-items: flex-start;
  padding: 10px 12px; border-radius: 8px; cursor: pointer; text-align: left;
  border: 1px solid var(--color-divider); background: var(--card-bg);
  transition: border-color 0.12s, background-color 0.12s;
}
.option:hover { border-color: var(--color-divider-strong); }
.option.selected {
  border-color: var(--color-primary);
  background: color-mix(in srgb, var(--color-primary) 8%, var(--card-bg));
}
.mark { color: var(--color-ink-muted); line-height: 1.5; }
.option.selected .mark { color: var(--color-primary); }
.opt-copy { display: flex; flex-direction: column; gap: 2px; }
.opt-copy strong { font-size: 13px; font-weight: 500; color: var(--color-ink); }
.opt-copy em { font-size: 12px; font-style: normal; color: var(--color-ink-muted); line-height: 1.5; }

.actions { display: flex; gap: 8px; justify-content: flex-end; }
.btn {
  padding: 9px 18px; font-size: 13px; font-weight: 500;
  border: 1px solid transparent; border-radius: 8px; cursor: pointer;
}
.btn-later {
  background: var(--color-surface-hover); color: var(--color-ink-secondary);
  border-color: var(--color-divider);
}
.btn-answer { background: var(--color-primary); color: white; }
.btn-answer:disabled { opacity: 0.45; cursor: not-allowed; }
</style>
