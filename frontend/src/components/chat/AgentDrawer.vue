<!-- frontend/src/components/chat/AgentDrawer.vue
  AI 面板容器（右侧抽屉 570px，2026-08-13 自 380px 加宽 150%——AI 回复为 markdown
  排版，窄面板下表格/代码块换行严重）。对标 zacp 浅色风：
  - 头部：标题 + 状态（思考中/就绪） + 关闭按钮
  - 消息流：MessageItem 列表（user 右对齐 / assistant 含思考折叠 + 流式文本 + 工具卡）
  - 输入框：textarea + 发送（Enter 发送）
  - 权限弹窗：approval_request 触发 PermissionModal
  滚动跟随用 useChatScroll（自动贴底 + 上翻暂停 + 回到底部按钮）。
  running 态经 provide 注入 MessageItem（思考面板活跃色 + 流式加载占位）。
  T9 M2：submit 失败给 UI 反馈（catch 后在消息流标一条 error 提示，非静默 rejection）。
-->
<script setup lang="ts">
import { computed, onMounted, onUnmounted, provide, ref, watch } from 'vue'
import { useAgentStore } from '../../stores/agent'
import { useChatScroll } from './composables/useChatScroll'
import MessageItem, { runningKey } from './MessageItem.vue'
import PermissionModal from './PermissionModal.vue'

const agent = useAgentStore()
const input = ref('')
/** 本地错误提示（submit 失败等，非 agent 消息流的一部分——单独显示在输入区上方）。 */
const errorMsg = ref('')
const listRef = ref<HTMLElement | null>(null)

const emit = defineEmits<{ close: [] }>()

const { showBackToBottom, onScroll, snapToBottom, followIfAtBottom } = useChatScroll(listRef)

/** provide running 态给 MessageItem（思考面板活跃色 + 加载占位判断）。用 computed
 *  保持响应式——turn_started/turn_done 改变 agent.running 时子组件自动更新。 */
provide(runningKey, computed(() => agent.running))

onMounted(() => {
  agent.start()
  snapToBottom()
})
onUnmounted(() => agent.stop())

/**
 * 消息列表变化时贴底跟随（流式 token 追加、新 user 消息、工具结果到达均触发）。
 * 用户上翻阅读时暂停跟随（atBottom=false），点击「回到底部」恢复。
 */
watch(
  () => [
    agent.messages.length,
    agent.messages.at(-1)?.text.length ?? 0,
    agent.messages.at(-1)?.reasoning?.length ?? 0,
    agent.messages.at(-1)?.toolCalls?.length ?? 0,
  ],
  () => followIfAtBottom(),
)

/** 发送：submit 失败给 UI 反馈（T9 M2）。 */
async function send() {
  const text = input.value.trim()
  if (!text || agent.running) return
  errorMsg.value = ''
  try {
    await agent.submit(text)
    input.value = ''
    snapToBottom()
  } catch (e) {
    errorMsg.value = e instanceof Error ? e.message : '发送失败'
  }
}

/** 取消当前 turn。 */
async function cancel() {
  try {
    await agent.cancel()
  } catch {
    /* cancel 失败静默（用户已知 running 态，不强提示） */
  }
}

/** 审批决策（转发给 store，失败不静默——用户在等结果）。 */
async function onApprove(decision: boolean) {
  try {
    await agent.approve(decision)
  } catch (e) {
    errorMsg.value = e instanceof Error ? e.message : '审批提交失败'
  }
}
</script>

<template>
  <aside class="drawer" aria-label="AI 助手面板">
    <header class="head">
      <h3 class="title">AI 助手</h3>
      <span class="status" :class="{ on: agent.running }">
        {{ agent.running ? '思考中…' : '就绪' }}
      </span>
      <button v-if="agent.running" class="btn-text btn-cancel" @click="cancel">停止</button>
      <button class="btn-icon" aria-label="关闭" @click="emit('close')">✕</button>
    </header>

    <div ref="listRef" class="messages" @scroll="onScroll">
      <p v-if="!agent.messages.length" class="placeholder">
        问 AI 助手任何关于采集器/设备/实时数据的问题。<br />
        例如：「读 devA 当前所有实时数据」「devB 的连接状态正常吗？」
      </p>
      <MessageItem v-for="m in agent.messages" :key="m.id" :message="m" />
    </div>

    <button
      v-show="showBackToBottom"
      class="back-to-bottom"
      title="回到底部"
      aria-label="回到底部"
      @click="snapToBottom"
    >↓ 最新</button>

    <div class="input-area">
      <div v-if="errorMsg" class="error-banner" role="alert">
        <span>{{ errorMsg }}</span>
        <button class="btn-text" aria-label="关闭错误提示" @click="errorMsg = ''">✕</button>
      </div>
      <div class="input-row">
        <textarea
          v-model="input"
          class="textarea"
          rows="2"
          placeholder="问 AI…（如：读 devA 实时数据）"
          :disabled="agent.running"
          @keydown.enter.exact.prevent="send"
        />
        <button class="btn-send" :disabled="!input.trim() || agent.running" @click="send">发送</button>
      </div>
    </div>

    <PermissionModal v-if="agent.approval" :approval="agent.approval" @approve="onApprove" />
  </aside>
</template>

<style scoped>
.drawer {
  position: fixed; right: 0; top: 0; bottom: 0;
  width: 570px; max-width: 100vw;
  background: var(--panel-bg);
  border-left: 1px solid var(--panel-border);
  display: grid;
  grid-template-rows: 44px 1fr auto;
  z-index: 100;
  box-shadow: -20px 0 60px color-mix(in srgb, var(--color-surface) 70%, black 30%);
}

/* 头部 */
.head {
  display: flex; align-items: center; gap: 8px;
  padding: 0 12px;
  border-bottom: 1px solid var(--color-divider);
  background: var(--card-bg);
}
.title { margin: 0; flex: 1; font-size: 14px; font-weight: 600; color: var(--color-ink); }
.status {
  font-size: 12px;
  color: var(--color-ink-muted);
  padding: 5px 10px;
  border-radius: 999px;
  background: var(--soft-overlay);
}
.status.on {
  color: var(--chip-ink);
  background: var(--chip-bg);
}
.btn-text {
  background: transparent; border: none;
  font-size: 12px; color: var(--color-ink-muted);
  padding: 2px 4px; cursor: pointer;
}
.btn-text:hover { color: var(--color-ink-secondary); }
.btn-cancel { color: var(--color-danger); }
.btn-icon {
  background: transparent; border: none;
  font-size: 14px; color: var(--color-ink-muted);
  padding: 4px 8px; cursor: pointer; border-radius: 4px;
}
.btn-icon:hover { background: var(--color-surface-hover); color: var(--color-ink); }

/* 消息流 */
.messages {
  overflow-y: auto;
  padding: 12px;
  background:
    radial-gradient(circle at top left, var(--panel-highlight), transparent 22%),
    var(--page-bg);
  scrollbar-width: thin;
  scrollbar-color: rgba(148, 163, 184, 0.6) transparent;
}
.placeholder {
  margin: 40px 16px;
  font-size: 13px; line-height: 1.7;
  color: var(--color-ink-muted);
  text-align: center;
}

/* 回到底部悬浮按钮 */
.back-to-bottom {
  position: absolute;
  bottom: 130px; right: 16px;
  padding: 7px 12px;
  font-size: 12px;
  background: var(--card-bg);
  border: 1px solid var(--card-border);
  border-radius: 999px;
  box-shadow: var(--shadow-soft);
  color: var(--color-ink-secondary);
  cursor: pointer;
  z-index: 10;
}
.back-to-bottom:hover { background: var(--color-surface-hover); }

/* 输入区 */
.input-area {
  border-top: 1px solid var(--color-divider);
  background: var(--card-bg);
}
.error-banner {
  display: flex; align-items: center; justify-content: space-between; gap: 8px;
  padding: 6px 12px;
  font-size: 12px; color: var(--color-danger);
  background: rgba(220, 38, 38, 0.06);
  border-bottom: 1px solid rgba(220, 38, 38, 0.15);
}
.input-row { display: flex; gap: 8px; padding: 10px; }
.textarea {
  flex: 1; resize: none;
  padding: 12px 14px;
  font-size: 13px; line-height: 1.5;
  font-family: inherit;
  border-radius: 14px;
}
.textarea:focus { outline: none; border-color: var(--color-primary); }
.textarea:disabled { background: var(--color-surface-hover); cursor: not-allowed; }
.btn-send {
  align-self: stretch;
  padding: 0 16px;
  font-size: 13px; font-weight: 500;
  background: linear-gradient(135deg, var(--color-primary), var(--color-primary-strong));
  color: var(--shell-theme-active-ink);
  border: none; border-radius: 8px;
  cursor: pointer;
  transition: background-color 0.15s;
  box-shadow: var(--shadow-soft);
}
.btn-send:hover:not(:disabled) { background: var(--color-primary-hover); }
.btn-send:disabled { opacity: 0.5; cursor: not-allowed; }
</style>
