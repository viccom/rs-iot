<!-- frontend/src/components/monitor/PacketTrace.vue -->
<script setup lang="ts">
import { computed, ref } from 'vue'
import { useMonitorStore } from '../../stores/monitor'
const monitor = useMonitorStore()
/** 激活后的提示（激活成功由后端 deadline 语义自动过期，前端仅展示状态）。 */
const activated = ref(false)
// PacketEntry.inst_id 真的是 inst_id（非 devId），按 selectedInstance 过滤正确。
const filtered = computed(() => {
  if (!monitor.selectedInstance) return monitor.packets
  return monitor.packets.filter(p => p.inst_id === monitor.selectedInstance)
})

/** 激活当前实例报文采集（60s）：经 store POST /api/rsiot/api/v1/inst/trace。 */
async function activate() {
  await monitor.startPacketTrace(60)
  activated.value = true
  // 60s 后提示自动失效（对齐 duration_secs）。
  setTimeout(() => { activated.value = false }, 60_000)
}
</script>
<template>
  <div class="packet-trace">
    <div class="toolbar">
      <p class="hint" v-if="monitor.selectedInstance">实例: {{ monitor.selectedInstance }}</p>
      <p class="hint" v-else>未选中实例（先在采集器页选择）</p>
      <button
        class="activate"
        :disabled="!monitor.selectedInstance"
        @click="activate"
      >
        {{ activated ? '已激活(60s)' : '激活报文采集' }}
      </button>
    </div>
    <div class="list">
      <div v-for="(p, i) in filtered" :key="i" class="pkt" :class="p.dir">
        <span class="dir">{{ p.dir.toUpperCase() }}</span>
        <span class="inst">{{ p.inst_id }}</span>
        <span class="hex">{{ p.hex }}</span>
        <span class="ts">{{ new Date(p.ts).toLocaleTimeString() }}</span>
      </div>
      <p v-if="!filtered.length" class="empty">
        {{ activated ? '已激活，等待收发帧…' : '无报文（点上方"激活报文采集"开始抓样）' }}
      </p>
    </div>
  </div>
</template>
<style scoped>
.packet-trace { display: flex; flex-direction: column; height: 100%; }
.toolbar {
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: 10px;
  margin-bottom: 10px;
}
.toolbar .hint {
  flex: 1;
  margin: 0;
}
.hint {
  padding: 10px 12px;
  color: var(--color-ink-muted);
  font-size: 12px;
  border-radius: 14px;
  background: var(--chip-bg);
  border: 1px solid var(--chip-border);
}
.activate {
  padding: 10px 16px;
  border-radius: 14px;
  border: 1px solid var(--color-primary);
  background: var(--color-primary);
  color: #fff;
  font-size: 12px;
  cursor: pointer;
  white-space: nowrap;
}
.activate:disabled {
  opacity: 0.45;
  cursor: not-allowed;
}
.list { flex: 1; overflow-y: auto; padding: 0; }
.pkt {
  display: grid;
  grid-template-columns: 48px 130px 1fr 84px;
  gap: 10px;
  padding: 12px 10px;
  font-size: 12px;
  font-family: Consolas, monospace;
  border-bottom: 1px solid var(--card-border);
  border-radius: 14px;
}
.pkt:hover { background: var(--table-row-hover); }
.pkt.tx .dir { color: var(--color-primary); } .pkt.rx .dir { color: var(--color-success); }
.inst, .ts { color: var(--color-ink-muted); }
.hex { word-break: break-all; }
.empty { color: var(--color-ink-muted); padding: 20px; text-align: center; }
</style>
