<script setup lang="ts">
import { computed } from 'vue'
import CollectorList from '../components/monitor/CollectorList.vue'
import { useWorkspacePage } from '../composables/useWorkspacePage'
import { useMonitorStore } from '../stores/monitor'

const monitor = useMonitorStore()

const runningCount = computed(() =>
  monitor.instances.filter(instance => instance.state === 'running').length,
)
const degradedCount = computed(() =>
  monitor.instances.filter(instance => instance.health?.status === 'degraded').length,
)
const latestLogs = computed(() => monitor.logs.slice(0, 5))
const latestPackets = computed(() => monitor.packets.slice(0, 4))

useWorkspacePage(() => ({
  pageSummary: '总览当前实例健康、日志态势与报文链路活跃度',
  facts: [
    `实例总数 ${monitor.roster.length}`,
    `运行中实例 ${runningCount.value}`,
    `降级实例 ${degradedCount.value}`,
    `最近日志 ${monitor.logs[0]?.message ?? '暂无'}`,
  ],
}))
</script>

<template>
  <section class="overview-page">
    <div class="hero-grid">
      <article class="hero-card accent">
        <p class="card-label">站点运行态势</p>
        <h3>{{ runningCount }}/{{ monitor.roster.length || 0 }}</h3>
        <p>当前在线采集器占比与整体运行体感。</p>
        <span class="hero-note">主控链路稳定，适合继续巡检</span>
      </article>
      <article class="hero-card">
        <p class="card-label">告警观察</p>
        <h3>{{ degradedCount }}</h3>
        <p>存在 health 降级的实例数量，适合优先巡检。</p>
        <span class="hero-note warn">建议优先跟进波动设备</span>
      </article>
      <article class="hero-card">
        <p class="card-label">链路活跃度</p>
        <h3>{{ monitor.packets.length }}</h3>
        <p>当前缓存报文帧数量，可用于快速判断链路是否静默。</p>
        <span class="hero-note">近 5 分钟链路保持活跃</span>
      </article>
    </div>

    <div class="content-grid">
      <section class="panel">
        <div class="panel-head">
          <div>
            <p class="card-label">采集实例</p>
            <h3>实例一览</h3>
          </div>
          <span>{{ monitor.roster.length }} 个实例</span>
        </div>
        <CollectorList />
      </section>

      <section class="panel">
        <div class="panel-head">
          <div>
            <p class="card-label">最新日志</p>
            <h3>现场脉搏</h3>
          </div>
          <span>{{ monitor.logs.length }} 条缓存</span>
        </div>
        <div class="feed-list">
          <article v-for="(entry, index) in latestLogs" :key="index" class="feed-entry">
            <strong>{{ entry.level.toUpperCase() }}</strong>
            <span>{{ entry.target }}</span>
            <p>{{ entry.message }}</p>
          </article>
          <p v-if="!latestLogs.length" class="empty">还没有日志进入缓冲区。</p>
        </div>
      </section>

      <section class="panel wide">
        <div class="panel-head">
          <div>
            <p class="card-label">链路样本</p>
            <h3>最近报文</h3>
          </div>
          <span>帮助判断采集器最近是否仍在收发数据</span>
        </div>
        <div class="packet-grid">
          <article v-for="(packet, index) in latestPackets" :key="index" class="packet-card">
            <p>{{ packet.dir.toUpperCase() }}</p>
            <strong>{{ packet.inst_id }}</strong>
            <code>{{ packet.hex }}</code>
          </article>
          <p v-if="!latestPackets.length" class="empty">暂无报文样本。</p>
        </div>
      </section>
    </div>
  </section>
</template>

<style scoped>
.overview-page,
.content-grid {
  display: grid;
  gap: 18px;
}

.hero-grid {
  display: grid;
  gap: 18px;
  grid-template-columns: repeat(3, minmax(0, 1fr));
}

.hero-card,
.panel {
  border-radius: var(--radius-panel);
  padding: 22px;
  background: var(--panel-bg);
  border: 1px solid var(--panel-border);
  box-shadow: var(--shadow-panel);
  position: relative;
  overflow: hidden;
}

.hero-card::before,
.panel::before {
  content: "";
  position: absolute;
  inset: 0;
  background: linear-gradient(130deg, var(--panel-highlight), transparent 42%);
  pointer-events: none;
}

.hero-card h3,
.panel-head h3 {
  margin: 0;
  font-family: "Bahnschrift", sans-serif;
}

.hero-card h3 {
  font-size: 38px;
  letter-spacing: 0.02em;
}

.hero-card.accent {
  background: linear-gradient(135deg, color-mix(in srgb, var(--color-primary) 24%, white), var(--panel-bg));
}

.card-label {
  margin: 0 0 10px;
  font-size: 11px;
  letter-spacing: 0.18em;
  text-transform: uppercase;
  color: var(--color-ink-dim);
}

.hero-card p,
.feed-entry p,
.packet-card p {
  position: relative;
  z-index: 1;
}

.hero-note {
  position: relative;
  z-index: 1;
  display: inline-flex;
  margin-top: 18px;
  padding: 8px 12px;
  border-radius: 999px;
  background: var(--chip-bg);
  border: 1px solid var(--chip-border);
  color: var(--chip-ink);
  font-size: 12px;
}

.hero-note.warn {
  color: var(--color-warning);
  border-color: var(--warning-border);
  background: var(--warning-soft);
}

.content-grid {
  grid-template-columns: minmax(260px, 320px) minmax(0, 1fr);
}

.panel.wide {
  grid-column: 1 / -1;
}

.panel-head {
  display: flex;
  justify-content: space-between;
  align-items: end;
  gap: 12px;
  margin-bottom: 18px;
  position: relative;
  z-index: 1;
}

.feed-list,
.packet-grid {
  display: grid;
  gap: 12px;
}

.feed-entry,
.packet-card {
  border-radius: var(--radius-card);
  padding: 16px;
  background: var(--card-bg);
  border: 1px solid var(--card-border);
  box-shadow: inset 0 1px 0 var(--panel-inner-border);
}

.feed-entry strong,
.packet-card strong {
  display: block;
  margin-bottom: 6px;
  font-size: 15px;
}

.feed-entry span {
  color: var(--color-ink-dim);
  font-size: 12px;
}

.feed-entry p,
.packet-card p {
  margin: 8px 0 0;
}

.packet-grid {
  grid-template-columns: repeat(4, minmax(0, 1fr));
}

.packet-card code {
  display: block;
  margin-top: 10px;
  color: var(--chip-ink);
  word-break: break-all;
  line-height: 1.6;
}

.empty {
  color: var(--color-ink-dim);
}

@media (max-width: 1080px) {
  .hero-grid,
  .content-grid,
  .packet-grid {
    grid-template-columns: 1fr;
  }
}
</style>
