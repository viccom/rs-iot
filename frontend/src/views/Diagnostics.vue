<script setup lang="ts">
import LogStream from '../components/monitor/LogStream.vue'
import PacketTrace from '../components/monitor/PacketTrace.vue'
import SystemStatus from '../components/monitor/SystemStatus.vue'
import { useWorkspacePage } from '../composables/useWorkspacePage'
import { useMonitorStore } from '../stores/monitor'

const monitor = useMonitorStore()

useWorkspacePage(() => ({
  pageSummary: '正在联合查看日志、报文和系统指标以定位故障',
  facts: [
    `日志缓存 ${monitor.logs.length} 条`,
    `报文缓存 ${monitor.packets.length} 条`,
    `当前实例 ${monitor.selectedInstance || '未选中'}`,
    `连接状态 ${monitor.offline ? '重连中' : '在线'}`,
  ],
}))
</script>

<template>
  <section class="diagnostics-page">
    <article class="page-panel">
      <div class="panel-head">
        <div>
          <p class="panel-label">日志视图</p>
          <h3>运行日志</h3>
        </div>
        <span>适合追踪异常与目标 target</span>
      </div>
      <LogStream />
    </article>

    <article class="page-panel">
      <div class="panel-head">
        <div>
          <p class="panel-label">报文追踪</p>
          <h3>链路抓样</h3>
        </div>
        <span>按当前实例过滤收发帧</span>
      </div>
      <PacketTrace />
    </article>

    <article class="page-panel wide">
      <div class="panel-head">
        <div>
          <p class="panel-label">系统指标</p>
          <h3>进程健康</h3>
        </div>
        <span>sysstatus 快照</span>
      </div>
      <SystemStatus />
    </article>
  </section>
</template>

<style scoped>
.diagnostics-page {
  display: grid;
  gap: 18px;
  grid-template-columns: repeat(2, minmax(0, 1fr));
}

.page-panel {
  min-height: 320px;
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
  min-height: auto;
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

.panel-head h3 {
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

@media (max-width: 1080px) {
  .diagnostics-page {
    grid-template-columns: 1fr;
  }

  .page-panel.wide {
    grid-column: auto;
  }
}
</style>
