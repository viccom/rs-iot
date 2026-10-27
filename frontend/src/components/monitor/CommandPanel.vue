<!-- frontend/src/components/monitor/CommandPanel.vue
  写命令下发面板（command/send 前端面，routes_command.rs §4.7）：实例下拉只列
  运行名册（monitor.instances——未运行实例后端 404）；data 为插件自定义 JSON 文本
  （store 前置校验合法性）；发送带 confirm（写设备有物理副作用）。
-->
<script setup lang="ts">
import { ref } from 'vue'
import { useMonitorStore } from '../../stores/monitor'

const monitor = useMonitorStore()

const instance = ref('')
const devid = ref('')
const data = ref('')
const sending = ref(false)
const result = ref<{ ok: boolean; error?: string; status?: number } | null>(null)

/** 发送写命令：confirm 防误触（写寄存器/点位有物理副作用），结果行呈现成败。 */
async function send() {
  if (!instance.value || !devid.value || !data.value || sending.value) return
  if (!window.confirm(`确认向实例 ${instance.value} 下发写命令？`)) return
  sending.value = true
  try {
    result.value = await monitor.sendCommand(instance.value, devid.value, data.value)
  } finally {
    sending.value = false
  }
}
</script>

<template>
  <div class="command-panel">
    <div class="panel-head">
      <div>
        <p class="panel-label">写命令下发</p>
        <h3>命令控制</h3>
      </div>
      <span class="head-hint">data 为插件自定义 JSON：modbus 地址串 / opcua node_id / lua on_write，见 rest-api.md §4.7</span>
    </div>

    <form class="form" @submit.prevent="send">
      <select v-model="instance" class="ipt" :disabled="!monitor.instances.length">
        <option v-if="!monitor.instances.length" value="" disabled>无运行实例</option>
        <option v-for="i in monitor.instances" :key="i.inst_id" :value="i.inst_id">
          {{ i.inst_id }}（{{ i.app_code }}）
        </option>
      </select>
      <input v-model="devid" class="ipt" type="text" placeholder="设备 ID（devid）" />
      <textarea
        v-model="data"
        class="ipt data-ipt"
        rows="3"
        placeholder='{"address":"holding:10:u16","value":1}&#10;{"node_id":"ns=2;s=Points/p004_u16","value":777,"opc_type":"uint16"}'
      ></textarea>
      <button
        type="submit"
        class="send-btn"
        :disabled="!instance || !devid || !data || sending"
      >{{ sending ? '下发中…' : '发送写命令' }}</button>
    </form>

    <p v-if="result" class="result" :class="result.ok ? 'ok' : 'fail'">
      {{ result.ok ? `下发成功（status ok，无执行回执）` : `下发失败：${result.error}` }}
    </p>
  </div>
</template>

<style scoped>
.command-panel {
  display: flex;
  flex-direction: column;
  gap: 12px;
  position: relative;
  z-index: 1;
}

.panel-head {
  display: flex;
  justify-content: space-between;
  gap: 12px;
  align-items: end;
}
.panel-head h3 { margin: 0; font-family: "Bahnschrift", sans-serif; }
.panel-label {
  margin: 0 0 10px;
  font-size: 11px;
  letter-spacing: 0.18em;
  text-transform: uppercase;
  color: var(--color-ink-dim);
}
.head-hint {
  font-size: 12px;
  color: var(--color-ink-muted);
  font-family: Consolas, monospace;
  text-align: right;
}

.form {
  display: grid;
  grid-template-columns: minmax(0, 1fr) minmax(0, 1fr) auto;
  gap: 10px;
  align-items: start;
}
.ipt {
  padding: 8px 12px;
  border-radius: 10px;
  font-size: 13px;
  border: 1px solid var(--chip-border);
  background: var(--card-bg);
  color: var(--color-ink);
  font-family: Consolas, monospace;
}
.ipt:focus { outline: none; border-color: var(--color-primary); }
.ipt:disabled { opacity: 0.5; cursor: not-allowed; }
.data-ipt {
  grid-column: 1 / 3;
  resize: vertical;
  min-height: 74px;
}
.send-btn {
  padding: 8px 18px;
  border-radius: 10px;
  cursor: pointer;
  font-size: 13px;
  font-weight: 500;
  border: 1px solid var(--color-primary);
  background: var(--color-primary);
  color: white;
  white-space: nowrap;
}
.send-btn:disabled { opacity: 0.4; cursor: not-allowed; }

.result { margin: 0; font-size: 12px; font-family: Consolas, monospace; }
.result.ok { color: var(--color-primary); }
.result.fail { color: var(--color-danger); }
</style>
