<script setup lang="ts">
import { computed } from 'vue'
import RtdataPanel from '../components/monitor/RtdataPanel.vue'
import { useWorkspacePage } from '../composables/useWorkspacePage'
import { useMonitorStore } from '../stores/monitor'

const monitor = useMonitorStore()

/** 可见设备（过滤空 devId——实例配置中设备 ID 未填时 rtdata 会产生空键设备，
 *  显示为无名 chip 且点位无法定位，属配置错误，入口处已加校验）。 */
const visibleDevices = computed(() => Object.keys(monitor.rtdata).filter(d => d).sort())
const selectedDeviceTags = computed(() => {
  if (!monitor.selectedDevice) {
    // 全部设备：聚合所有设备的点位键（此前恒返回 []，点位数量永远显示 0）。
    return Object.values(monitor.rtdata).flatMap(tags => Object.keys(tags))
  }
  return Object.keys(monitor.rtdata[monitor.selectedDevice] || {})
})

useWorkspacePage(() => ({
  pageSummary: monitor.selectedDevice
    ? `正在查看设备 ${monitor.selectedDevice} 的实时点位变化`
    : '正在查看全站设备的实时点位刷新情况',
  facts: [
    `可见设备 ${visibleDevices.value.length} 个`,
    `当前实例 ${monitor.selectedInstance || '未选中'}`,
    `当前设备 ${monitor.selectedDevice || '未选中'}`,
    `当前设备点位 ${selectedDeviceTags.value.length} 个`,
  ],
}))
</script>

<template>
  <section class="telemetry-page">
    <article class="page-panel device-panel">
      <div class="panel-head">
        <div>
          <p class="panel-label">设备焦点</p>
          <h3>实时设备范围</h3>
        </div>
        <span>{{ visibleDevices.length }} 个 devId</span>
      </div>
      <div class="device-list">
        <button
          class="device-chip"
          :class="{ active: !monitor.selectedDevice }"
          @click="monitor.selectDevice('')"
        >
          全部设备
        </button>
        <button
          v-for="deviceId in visibleDevices"
          :key="deviceId"
          class="device-chip"
          :class="{ active: monitor.selectedDevice === deviceId }"
          @click="monitor.selectDevice(deviceId)"
        >
          {{ deviceId }}
        </button>
      </div>
      <div class="focus-card">
        <p class="panel-label">当前观察</p>
        <strong>{{ monitor.selectedDevice || '全部设备' }}</strong>
        <span>点位数量 {{ selectedDeviceTags.length }}</span>
      </div>
    </article>

    <article class="page-panel">
      <div class="panel-head">
        <div>
          <p class="panel-label">点位面板</p>
          <h3>实时点位列表</h3>
        </div>
        <span>适合配合 AI 提问当前页面异常</span>
      </div>
      <RtdataPanel />
    </article>
  </section>
</template>

<style scoped>
.telemetry-page {
  display: grid;
  gap: 18px;
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
  background: linear-gradient(135deg, var(--panel-highlight), transparent 44%);
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

.panel-label {
  margin: 0 0 10px;
  font-size: 11px;
  letter-spacing: 0.18em;
  text-transform: uppercase;
  color: var(--color-ink-dim);
}

.panel-head h3,
.focus-card strong {
  margin: 0;
  font-family: "Bahnschrift", sans-serif;
}

.device-list {
  display: flex;
  flex-wrap: wrap;
  gap: 10px;
  position: relative;
  z-index: 1;
}

.device-chip {
  padding: 11px 15px;
  border-radius: 999px;
  border: 1px solid var(--chip-border);
  background: var(--card-bg);
  color: var(--color-ink);
  box-shadow: inset 0 1px 0 var(--panel-inner-border);
}

.device-chip.active {
  background: linear-gradient(135deg, color-mix(in srgb, var(--color-primary) 18%, white), var(--card-bg));
  border-color: var(--color-divider-strong);
  color: var(--color-ink);
}

.focus-card {
  margin-top: 16px;
  display: grid;
  gap: 6px;
  padding: 18px;
  border-radius: var(--radius-card);
  background: var(--card-bg);
  border: 1px solid var(--chip-border);
  position: relative;
  z-index: 1;
}
</style>
