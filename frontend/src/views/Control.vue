<script setup lang="ts">
import { useWorkspacePage } from '../composables/useWorkspacePage'
import { useMonitorStore } from '../stores/monitor'

const monitor = useMonitorStore()

useWorkspacePage(() => ({
  pageSummary: '正在准备巡检动作、人工刷新与 AI 协作建议',
  facts: [
    `当前实例 ${monitor.selectedInstance || '未选中'}`,
    `当前设备 ${monitor.selectedDevice || '未选中'}`,
    `系统在线状态 ${monitor.offline ? '重连中' : '在线'}`,
    `实例总数 ${monitor.roster.length}`,
  ],
}))
</script>

<template>
  <section class="control-page">
    <article class="page-panel command-panel">
      <div class="panel-head">
        <div>
          <p class="panel-label">人工操作</p>
          <h3>快速刷新</h3>
        </div>
        <span>适合巡检前手动对齐最新状态</span>
      </div>
      <div class="command-list">
        <button class="command-button" @click="monitor.loadInstances()">刷新实例状态</button>
        <button class="command-button" @click="monitor.loadRtdata()">刷新实时数据</button>
        <button class="command-button" @click="monitor.loadSysstatus()">刷新系统指标</button>
      </div>
    </article>

    <article class="page-panel">
      <div class="panel-head">
        <div>
          <p class="panel-label">AI 协作提示</p>
          <h3>建议提问</h3>
        </div>
        <span>打开右上角 AI 即可直接提问</span>
      </div>
      <div class="prompt-list">
        <article class="prompt-card">帮我总结当前页面最值得关注的异常线索。</article>
        <article class="prompt-card">结合当前实例和设备，判断是否存在断连或降级风险。</article>
        <article class="prompt-card">根据最近日志和报文，给出下一步排查建议。</article>
      </div>
    </article>

    <article class="page-panel wide">
      <div class="panel-head">
        <div>
          <p class="panel-label">当前范围</p>
          <h3>控制上下文</h3>
        </div>
        <span>AI 将自动感知这些信息</span>
      </div>
      <div class="scope-grid">
        <div class="scope-card">
          <span>实例</span>
          <strong>{{ monitor.selectedInstance || '未选中实例' }}</strong>
        </div>
        <div class="scope-card">
          <span>设备</span>
          <strong>{{ monitor.selectedDevice || '未选中设备' }}</strong>
        </div>
        <div class="scope-card">
          <span>连接</span>
          <strong>{{ monitor.offline ? '重连中' : '在线' }}</strong>
        </div>
      </div>
    </article>
  </section>
</template>

<style scoped>
.control-page {
  display: grid;
  gap: 18px;
  grid-template-columns: repeat(2, minmax(0, 1fr));
}

.page-panel {
  border-radius: var(--radius-panel);
  padding: 22px;
  background: var(--panel-bg);
  border: 1px solid var(--panel-border);
  box-shadow: var(--shadow-panel);
  position: relative;
  overflow: hidden;
}

.page-panel::before {
  content: "";
  position: absolute;
  inset: 0;
  background: linear-gradient(140deg, var(--panel-highlight), transparent 46%);
  pointer-events: none;
}

.page-panel.wide {
  grid-column: 1 / -1;
}

.panel-head {
  display: flex;
  justify-content: space-between;
  gap: 12px;
  align-items: end;
  margin-bottom: 16px;
  position: relative;
  z-index: 1;
}

.panel-head h3,
.scope-card strong {
  margin: 0;
  font-family: "Bahnschrift", sans-serif;
}

.panel-label {
  margin: 0 0 10px;
  font-size: 11px;
  letter-spacing: 0.18em;
  text-transform: uppercase;
  color: var(--color-ink-dim);
}

.command-list,
.prompt-list,
.scope-grid {
  display: grid;
  gap: 12px;
  position: relative;
  z-index: 1;
}

.command-button,
.prompt-card,
.scope-card {
  padding: 16px;
  border-radius: var(--radius-card);
  border: 1px solid var(--card-border);
  background: var(--card-bg);
  color: var(--color-ink);
  box-shadow: inset 0 1px 0 var(--panel-inner-border);
}

.command-button {
  text-align: left;
  font-weight: 600;
}

.scope-grid {
  grid-template-columns: repeat(3, minmax(0, 1fr));
}

.scope-card {
  display: grid;
  gap: 8px;
}

.scope-card span {
  color: var(--color-ink-dim);
}

@media (max-width: 1080px) {
  .control-page,
  .scope-grid {
    grid-template-columns: 1fr;
  }

  .page-panel.wide {
    grid-column: auto;
  }
}
</style>
