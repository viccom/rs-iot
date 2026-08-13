<script setup lang="ts">
import { computed } from 'vue'
import CollectorList from '../components/monitor/CollectorList.vue'
import { useWorkspacePage } from '../composables/useWorkspacePage'
import { useMonitorStore } from '../stores/monitor'

const monitor = useMonitorStore()

const selectedCollector = computed(() =>
  monitor.instances.find(instance => instance.inst_id === monitor.selectedInstance) ?? null,
)
const builtinCount = computed(() =>
  monitor.instances.filter(instance => instance.host_kind === 'builtin').length,
)

useWorkspacePage(() => ({
  pageSummary: monitor.selectedInstance
    ? `正在查看采集器 ${monitor.selectedInstance} 的运行信息`
    : '正在浏览所有采集器的运行分布',
  facts: [
    `实例总数 ${monitor.instances.length}`,
    `内建实例 ${builtinCount.value}`,
    `当前实例 ${monitor.selectedInstance || '未选中'}`,
    `当前实例状态 ${selectedCollector.value?.state || '未知'}`,
  ],
}))
</script>

<template>
  <section class="collectors-page">
    <article class="page-panel">
      <div class="panel-head">
        <div>
          <p class="panel-label">实例名册</p>
          <h3>采集器列表</h3>
        </div>
        <span>{{ monitor.instances.length }} 个实例</span>
      </div>
      <CollectorList />
    </article>

    <article class="page-panel detail-panel">
      <div class="panel-head">
        <div>
          <p class="panel-label">运行详情</p>
          <h3>{{ selectedCollector?.inst_id || '请选择一个采集器' }}</h3>
        </div>
        <span>{{ selectedCollector?.app_code || '等待选择' }}</span>
      </div>

      <div v-if="selectedCollector" class="detail-grid">
        <div class="detail-card">
          <span>进程状态</span>
          <strong>{{ selectedCollector.state }}</strong>
        </div>
        <div class="detail-card">
          <span>宿主类型</span>
          <strong>{{ selectedCollector.host_kind }}</strong>
        </div>
        <div class="detail-card">
          <span>健康状态</span>
          <strong>{{ selectedCollector.health?.status || 'unknown' }}</strong>
        </div>
        <div class="detail-card">
          <span>最近失败</span>
          <strong>{{ selectedCollector.health?.last_err || '无' }}</strong>
        </div>
      </div>

      <p v-else class="empty">左侧选择一个采集器后，这里会展示它的状态详情。</p>
    </article>
  </section>
</template>

<style scoped>
.collectors-page {
  display: grid;
  gap: 18px;
  grid-template-columns: minmax(260px, 320px) minmax(0, 1fr);
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
  background: linear-gradient(140deg, var(--panel-highlight), transparent 50%);
  pointer-events: none;
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
.detail-card strong {
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

.detail-grid {
  display: grid;
  gap: 14px;
  grid-template-columns: repeat(2, minmax(0, 1fr));
}

.detail-card {
  display: grid;
  gap: 8px;
  padding: 18px;
  border-radius: var(--radius-card);
  background: var(--card-bg);
  border: 1px solid var(--card-border);
  box-shadow: inset 0 1px 0 var(--panel-inner-border);
}

.detail-card span,
.empty {
  color: var(--color-ink-dim);
}

@media (max-width: 960px) {
  .collectors-page,
  .detail-grid {
    grid-template-columns: 1fr;
  }
}
</style>
