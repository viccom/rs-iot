<script setup lang="ts">
import { computed, watch } from 'vue'
import RtdataPanel from '../components/monitor/RtdataPanel.vue'
import AlarmPanel from '../components/monitor/AlarmPanel.vue'
import { useWorkspacePage } from '../composables/useWorkspacePage'
import { useMonitorStore } from '../stores/monitor'
import { buildDeviceOwnership, unclaimedDevices } from '../lib/instRelations'

const monitor = useMonitorStore()

/** 设备归属映射（配置声明 ∪ 运行时上报 ∪ 后端归属表 owner_inst，lib 纯函数派生）。 */
const ownership = computed(() => buildDeviceOwnership(monitor.instanceConfigs, monitor.roster, monitor.deviceOwners))

/** 实例设备集：配置 devices ∪ 运行时上报 runtime_devices ∪ 归属表 owner_inst
 *  （实例过滤的收窄口径——归属表多宿主逗号串拆分）。 */
function instanceDeviceSet(instId: string): Set<string> {
  const cfg = monitor.instanceConfigs[instId]
  const s = new Set<string>(cfg?.devices.map(d => d.value).filter(Boolean) ?? [])
  const rt = monitor.roster.find(i => i.inst_id === instId)?.health?.runtime_devices ?? []
  for (const d of rt) if (d) s.add(d)
  for (const [devId, owners] of Object.entries(monitor.deviceOwners)) {
    if ((owners ?? '').split(',').map(x => x.trim()).includes(instId)) s.add(devId)
  }
  return s
}

// setup 拉一次后端归属表（dev2inst 全插件口径；缺席时退化为配置+runtime_devices）。
monitor.loadDeviceOwners()

/** 孤儿设备（有数据、无任何归属——多为已删实例残留）。仅"全部实例"视图单独分组展示。 */
const orphans = computed(() => unclaimedDevices(monitor.rtdata, ownership.value))
/** 孤儿 Set（visibleDevices 过滤用，O(1) 查找防 O(n²)）。 */
const orphanSet = computed(() => new Set(orphans.value))

/** 可见设备 chips（过滤空 devId——实例配置中设备 ID 未填时 rtdata 会产生空键设备，
 *  显示为无名 chip 且点位无法定位，属配置错误，入口处已加校验）。
 *  - 全部实例：rtdata 全部设备（孤儿除外，另有分组）。
 *  - 选实例 X：X 的设备集（配置 ∪ 运行时；配置未采到数据的也显示，弱化标注）。 */
const visibleDevices = computed(() =>
  monitor.selectedInstance
    ? [...instanceDeviceSet(monitor.selectedInstance)].sort()
    : Object.keys(monitor.rtdata).filter(d => d && !orphanSet.value.has(d)).sort(),
)

/** 设备是否有实时数据（无数据弱化标注）。 */
function hasData(d: string): boolean {
  const t = monitor.rtdata[d]
  return Boolean(t) && Object.keys(t).length > 0
}

/** 选实例（复用全局 selectedInstance：切页后诊断/采集器/AI 上下文自然联动）。
 *  联动守卫：当前设备选中不在新实例设备集内 → 清空（防两维过滤组合出空集）。 */
function selectInst(id: string) {
  monitor.selectInstance(id)
  if (id && monitor.selectedDevice && !instanceDeviceSet(id).has(monitor.selectedDevice)) {
    monitor.selectDevice('')
  }
}

// 外部入口兜底（审查 P1 修复）：实例选中在别处变化（采集器页/脚本页跳转/诊断页选择器）
// 后，已选设备可能不再属于新实例——"实例 AND 设备"双重过滤会组合出空表。统一在
// 数据页感知变化时清掉不匹配的设备选中（本页 selectInst 的守卫只覆盖本页点击）。
watch(
  () => monitor.selectedInstance,
  inst => {
    if (inst && monitor.selectedDevice && !instanceDeviceSet(inst).has(monitor.selectedDevice)) {
      monitor.selectDevice('')
    }
  },
)

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
    `实例过滤 ${monitor.selectedInstance || '全部实例'}`,
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
          <p class="panel-label">实例与设备范围</p>
          <h3>实时采集焦点</h3>
        </div>
        <span>{{ visibleDevices.length + orphans.length }} 个 devId</span>
      </div>

      <!-- 实例 chip 行（复用全局 selectedInstance：选实例 = 全局聚焦，切页上下文连贯） -->
      <div class="inst-list">
        <button
          class="inst-chip"
          :class="{ active: !monitor.selectedInstance }"
          @click="selectInst('')"
        >
          全部实例
        </button>
        <button
          v-for="inst in monitor.roster"
          :key="inst.inst_id"
          class="inst-chip"
          :class="{ active: monitor.selectedInstance === inst.inst_id, [inst.state]: true }"
          :title="`${inst.inst_id}（${inst.app_code}，${inst.state}）`"
          @click="selectInst(inst.inst_id)"
        >
          <span class="chip-dot"></span>
          <span class="chip-id">{{ inst.inst_id }}</span>
          <span class="chip-code">{{ inst.app_code }}</span>
        </button>
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
          :class="{ active: monitor.selectedDevice === deviceId, nodata: !hasData(deviceId) }"
          :title="hasData(deviceId) ? deviceId : '配置声明但尚未采到数据（实例未运行或点位无返回）'"
          @click="monitor.selectDevice(deviceId)"
        >
          {{ deviceId }}<span v-if="!hasData(deviceId)" class="nodata-tag">无数据</span>
        </button>
      </div>

      <!-- 孤儿分组：仅"全部实例"视图展示（无法归属的 rtdata 残留） -->
      <div v-if="!monitor.selectedInstance && orphans.length" class="orphan-group">
        <p class="orphan-label" title="有实时数据但不在任何实例的配置或运行时上报中——多为已删实例的残留缓存">
          未声明归属（{{ orphans.length }}）
        </p>
        <div class="orphan-chips">
          <button
            v-for="deviceId in orphans"
            :key="deviceId"
            class="device-chip dim"
            :class="{ active: monitor.selectedDevice === deviceId }"
            @click="monitor.selectDevice(deviceId)"
          >
            {{ deviceId }}
          </button>
        </div>
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
        <span>{{
          monitor.selectedInstance
            ? `实例 ${monitor.selectedInstance} 范围内`
            : monitor.selectedDevice
              ? `设备 ${monitor.selectedDevice} 范围内`
              : '全部设备范围'
        }}</span>
      </div>
      <RtdataPanel />
    </article>

    <!-- 活跃告警（Q12 最小闭环）：rtdata 面板近旁——数据异常（点位越限）时告警就近可见。 -->
    <article class="page-panel">
      <div class="panel-head">
        <div>
          <p class="panel-label">告警面板</p>
          <h3>活跃告警</h3>
        </div>
        <span>脚本 fire / 手动或脚本 clear，SSE 实时收敛</span>
      </div>
      <AlarmPanel />
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

/* ---- 实例 chip 行（方向 2b：按实例过滤） ---- */
.inst-list {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
  margin-bottom: 14px;
  padding-bottom: 14px;
  border-bottom: 1px solid var(--color-divider);
  position: relative;
  z-index: 1;
}

.inst-chip {
  display: inline-flex;
  align-items: center;
  gap: 7px;
  padding: 9px 13px;
  border-radius: 999px;
  border: 1px solid var(--chip-border);
  background: var(--card-bg);
  color: var(--color-ink);
  box-shadow: inset 0 1px 0 var(--panel-inner-border);
  font-size: 12px;
}

.inst-chip.active {
  background: linear-gradient(135deg, color-mix(in srgb, var(--color-primary) 18%, white), var(--card-bg));
  border-color: var(--color-divider-strong);
}

.chip-dot { width: 7px; height: 7px; border-radius: 50%; background: var(--color-ink-muted); flex: none; }
.inst-chip.running .chip-dot { background: var(--color-success); }
.inst-chip.failed .chip-dot { background: var(--color-danger); }
.inst-chip.starting .chip-dot, .inst-chip.stopping .chip-dot { background: var(--color-warning); }
.chip-id { font-family: Consolas, monospace; }
.chip-code { font-size: 10px; color: var(--color-ink-muted); }

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

/* 配置声明但无数据的设备：弱化 + 标注（实例过滤下才可见——提醒"实例未运行/点位无返回"）。 */
.device-chip.nodata { opacity: 0.62; }
.nodata-tag {
  margin-left: 7px;
  padding: 1px 7px;
  border-radius: 999px;
  font-size: 10px;
  background: var(--chip-bg);
  color: var(--color-ink-muted);
}

/* 孤儿分组：无法归属的 rtdata 残留（全部实例视图尾部）。 */
.orphan-group {
  margin-top: 12px;
  position: relative;
  z-index: 1;
}
.orphan-label {
  margin: 0 0 8px;
  font-size: 11px;
  letter-spacing: 0.08em;
  text-transform: uppercase;
  color: var(--color-ink-muted);
}
.orphan-chips { display: flex; flex-wrap: wrap; gap: 8px; }
.device-chip.dim { opacity: 0.72; border-style: dashed; }

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
