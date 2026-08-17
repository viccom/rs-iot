<script setup lang="ts">
// frontend/src/views/Diagnostics.vue
// 诊断页（模块 M3）：Tab 化（运行日志 / 链路抓样）+ 手动激活订阅。
// 默认不推送——用户选实例 + 点激活才 subscribeLog/subscribePacket（store 层按需 SSE）。
// 系统指标保留为下方常驻 panel。语义变化：原"登录即看日志"→"Tab 手动激活才看"。
import { computed, onUnmounted, ref } from 'vue'
import { useWorkspacePage } from '../composables/useWorkspacePage'
import { useMonitorStore } from '../stores/monitor'
import LogStream from '../components/monitor/LogStream.vue'
import PacketTrace from '../components/monitor/PacketTrace.vue'
import SystemStatus from '../components/monitor/SystemStatus.vue'

const monitor = useMonitorStore()

/** 当前 Tab：'log' | 'packet'。 */
const tab = ref<'log' | 'packet'>('log')

/** 切到日志 Tab：若未激活，提示用户激活（不自动订阅——遵守"默认不推送"）。 */
function switchLog() {
  tab.value = 'log'
}
/** 切到报文 Tab。 */
function switchPacket() {
  tab.value = 'packet'
}

/** 激活/关闭日志订阅。 */
function toggleLog() {
  if (monitor.logSubscribed) monitor.unsubscribeLog()
  else monitor.subscribeLog()
}
/** 激活/关闭报文订阅。 */
async function togglePacket() {
  if (monitor.packetSubscribed) monitor.unsubscribePacket()
  else await monitor.subscribePacket()
}

/** 页内切换抓样目标实例（选择器直改 store 联动；PacketTrace watch 负责停旧实例
 *  trace + 激活新实例）。 */
function onInstChange(e: Event) {
  monitor.selectInstance((e.target as HTMLSelectElement).value)
}

/** 选中实例是否运行中（抓样要求 Running，后端否则 404——按钮按此禁用防假激活）。 */
const selectedRunning = computed(() =>
  monitor.roster.find(i => i.inst_id === monitor.selectedInstance)?.state === 'running',
)

/** 离开页面时关闭诊断订阅（避免后台空跑 SSE）。rtdata 轮询由 store.start/stop 管。 */
onUnmounted(() => {
  monitor.unsubscribeLog()
  monitor.unsubscribePacket()
})

useWorkspacePage(() => ({
  pageSummary: '正在联合查看日志、报文定位故障（按需激活推送）',
  facts: [
    `当前 Tab ${tab.value === 'log' ? '运行日志' : '链路抓样'}`,
    `日志订阅 ${monitor.logSubscribed ? '已激活' : '未激活'}`,
    `报文订阅 ${monitor.packetSubscribed ? '已激活' : '未激活'}`,
    `当前实例 ${monitor.selectedInstance || '未选中'}`,
  ],
}))
</script>

<template>
  <section class="diagnostics-page">
    <article class="page-panel diag-panel">
      <!-- Tab 头 -->
      <div class="tabs">
        <button class="tab" :class="{ active: tab === 'log' }" @click="switchLog">运行日志</button>
        <button class="tab" :class="{ active: tab === 'packet' }" @click="switchPacket">链路抓样</button>
      </div>

      <!-- 激活控制条（默认不推送，手动激活才订阅 SSE）-->
      <div class="activate-bar">
        <p class="activate-hint">
          <template v-if="tab === 'log'">
            日志为全进程流（含所有实例，经级别/target 过滤），默认不推送，点击激活开始订阅。
          </template>
          <template v-else>
            报文抓样按实例激活（60s 窗口）；切换实例将停止旧实例推送并激活新实例。
          </template>
        </p>
        <div class="activate-actions">
          <!-- 页内实例选择器（此前须去采集器页选择，诊断流程被打断）。
               名册含未启动实例（state 一并展示）；抓样激活要求实例运行中。 -->
          <select
            class="inst-select"
            :value="monitor.selectedInstance"
            title="选择抓样目标实例（日志流不受影响，为全进程推送）"
            @change="onInstChange"
          >
            <option value="" disabled>选择实例</option>
            <option v-for="inst in monitor.roster" :key="inst.inst_id" :value="inst.inst_id">
              {{ inst.inst_id }}（{{ inst.state }}）
            </option>
          </select>
          <button
            v-if="tab === 'log'"
            class="activate-btn"
            :class="{ on: monitor.logSubscribed }"
            @click="toggleLog"
          >
            {{ monitor.logSubscribed ? '停止订阅' : '激活日志推送' }}
          </button>
          <button
            v-else
            class="activate-btn"
            :class="{ on: monitor.packetSubscribed }"
            :disabled="!monitor.selectedInstance || !selectedRunning"
            :title="selectedRunning ? '激活抓样（60s 窗口）' : '实例未运行，先到采集器页启动'"
            @click="togglePacket"
          >
            {{ monitor.packetSubscribed ? '停止抓样' : '激活报文抓样' }}
          </button>
        </div>
      </div>

      <!-- Tab 内容 -->
      <div v-show="tab === 'log'" class="tab-content">
        <p v-if="!monitor.logSubscribed" class="inactive-hint">
          日志推送未激活。点上方「激活日志推送」开始实时查看（激活后自动补拉最近 200 条历史）。
        </p>
        <LogStream v-else />
      </div>

      <div v-show="tab === 'packet'" class="tab-content">
        <p v-if="!monitor.packetSubscribed" class="inactive-hint">
          报文抓样未激活。选择实例后点上方「激活报文抓样」开始收发帧抓样（60s 自动过期）。
        </p>
        <PacketTrace v-else />
      </div>
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
  position: absolute; inset: 0;
  background: linear-gradient(140deg, var(--panel-highlight), transparent 46%);
  pointer-events: none;
}

.diag-panel { position: relative; z-index: 1; min-height: 420px; display: flex; flex-direction: column; }

/* Tab */
.tabs {
  display: flex; gap: 4px;
  border-bottom: 1px solid var(--color-divider);
  margin-bottom: 14px;
  position: relative; z-index: 1;
}
.tab {
  padding: 10px 20px; background: transparent; border: none; cursor: pointer;
  font-size: 14px; font-weight: 500; color: var(--color-ink-muted);
  border-bottom: 2px solid transparent; transition: all 0.15s;
}
.tab:hover { color: var(--color-ink-secondary); }
.tab.active { color: var(--color-primary); border-bottom-color: var(--color-primary); }

/* 激活控制条 */
.activate-bar {
  display: flex; justify-content: space-between; align-items: center; gap: 12px;
  padding: 12px 14px; margin-bottom: 14px;
  border-radius: 12px; background: var(--card-bg); border: 1px solid var(--card-border);
  position: relative; z-index: 1;
}
.activate-hint { margin: 0; font-size: 12px; color: var(--color-ink-muted); flex: 1; }
.activate-actions { display: flex; align-items: center; gap: 10px; }
.inst-chip {
  padding: 5px 10px; border-radius: 999px; font-size: 12px;
  background: var(--chip-bg); color: var(--chip-ink); border: 1px solid var(--chip-border);
  font-family: Consolas, monospace;
}
.inst-chip.warn { color: var(--color-warning); border-color: var(--color-warning); }
/* 页内实例选择器 */
.inst-select {
  max-width: 240px; padding: 6px 10px; border-radius: 10px; font-size: 12px;
  border: 1px solid var(--chip-border); background: var(--card-bg); color: var(--color-ink);
  font-family: Consolas, monospace;
}
.inst-select:focus { outline: none; border-color: var(--color-primary); }
.activate-btn {
  padding: 8px 16px; border-radius: 10px; cursor: pointer; font-size: 13px; font-weight: 500;
  border: 1px solid var(--color-primary); background: var(--color-primary); color: white; white-space: nowrap;
}
.activate-btn:disabled { opacity: 0.4; cursor: not-allowed; }
.activate-btn.on { background: var(--color-surface-hover); color: var(--color-ink-secondary); border-color: var(--color-divider); }

.tab-content { flex: 1; min-height: 300px; overflow: hidden; position: relative; z-index: 1; }
.inactive-hint {
  display: flex; align-items: center; justify-content: center; height: 100%;
  color: var(--color-ink-muted); font-size: 13px; text-align: center; padding: 20px;
}

.panel-head {
  display: flex; justify-content: space-between; gap: 12px; align-items: end;
  margin-bottom: 16px; position: relative; z-index: 1;
}
.panel-head h3 { margin: 0; font-family: "Bahnschrift", sans-serif; }
.panel-label {
  margin: 0 0 10px; font-size: 11px; letter-spacing: 0.18em; text-transform: uppercase;
  color: var(--color-ink-dim);
}
</style>
