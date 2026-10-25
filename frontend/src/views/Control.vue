<script setup lang="ts">
import { useWorkspacePage } from '../composables/useWorkspacePage'
import { useMonitorStore } from '../stores/monitor'
import CommandPanel from '../components/monitor/CommandPanel.vue'

const monitor = useMonitorStore()

useWorkspacePage(() => ({
  pageSummary: '正在下发写命令并核对控制上下文',
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
    <!-- 写命令下发（command/send 前端面）：面板自带标题区/表单/结果行。 -->
    <article class="page-panel wide">
      <CommandPanel />
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
  grid-template-columns: minmax(0, 1fr);
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

.scope-grid {
  display: grid;
  gap: 12px;
  position: relative;
  z-index: 1;
}

.scope-card {
  padding: 16px;
  border-radius: var(--radius-card);
  border: 1px solid var(--card-border);
  background: var(--card-bg);
  color: var(--color-ink);
  box-shadow: inset 0 1px 0 var(--panel-inner-border);
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

@media (max-width: 960px) {
  .scope-grid {
    grid-template-columns: 1fr;
  }
}
</style>
