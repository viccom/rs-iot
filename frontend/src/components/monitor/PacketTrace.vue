<!-- frontend/src/components/monitor/PacketTrace.vue -->
<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { useMonitorStore } from '../../stores/monitor'
const monitor = useMonitorStore()

/** 抓样窗口剩余秒数（>0 表示 trace 激活中；后端 60s deadline 到期自动失效，前端只作提示）。 */
const remaining = ref(0)
/** 倒计时句柄。 */
let countdown: ReturnType<typeof setInterval> | null = null

// PacketEntry.inst_id 真的是 inst_id（非 devId），按 selectedInstance 过滤正确。
const filtered = computed(() => {
  if (!monitor.selectedInstance) return monitor.packets
  return monitor.packets.filter(p => p.inst_id === monitor.selectedInstance)
})

/** 选中实例的运行态（抓样要求实例 Running——后端对未运行实例返回 404）。 */
const selectedRunning = computed(() =>
  monitor.roster.find(i => i.inst_id === monitor.selectedInstance)?.state === 'running',
)
/** 激活失败提示（实例未运行等；下一次尝试或切换实例时清除）。 */
const armError = ref('')

/** 启动本地倒计时（不 POST——用于挂载时对齐 store 已激活的窗口）。 */
function startCountdown(sec: number) {
  remaining.value = sec
  if (countdown) clearInterval(countdown)
  countdown = setInterval(() => {
    remaining.value -= 1
    if (remaining.value <= 0 && countdown) {
      clearInterval(countdown)
      countdown = null
    }
  }, 1000)
}

/** 激活/续抓当前实例报文采集（60s 窗口）：仅后端确认成功才启动本地倒计时——
 *  未运行实例后端 404，若照跑倒计时会显示"已激活"误导现场（假激活修复）。 */
async function arm() {
  armError.value = ''
  const ok = await monitor.startPacketTrace(60)
  if (ok) startCountdown(60)
  else armError.value = '激活失败：实例须处于运行中（stopped 实例先到采集器页启动）'
}

// 挂载即处于订阅态：store.subscribePacket 已对当前实例 POST 过 trace（60s），此处仅同步倒计时。
onMounted(() => {
  if (monitor.packetSubscribed && selectedRunning.value) startCountdown(60)
})

// 订阅期间切换实例：先显式停旧实例 trace（后端 per-instance 注册表，不停会推到
// deadline 自然过期），再激活新实例——否则旧实例帧继续涌入（仅被显示层过滤）。
watch(() => monitor.selectedInstance, (cur, prev) => {
  armError.value = ''
  if (!monitor.packetSubscribed) return
  if (prev && prev !== cur) void monitor.stopPacketTrace(prev)
  void arm()
})

onUnmounted(() => {
  if (countdown) clearInterval(countdown)
})
</script>
<template>
  <div class="packet-trace">
    <div class="toolbar">
      <p class="hint" v-if="monitor.selectedInstance">实例: {{ monitor.selectedInstance }}</p>
      <p class="hint" v-else>未选中实例（先在采集器页选择）</p>
      <button
        class="activate"
        :disabled="!monitor.selectedInstance || !selectedRunning"
        :title="selectedRunning ? '激活/续抓 60s 报文窗口' : '实例未运行，先启动实例'"
        @click="arm"
      >
        {{ remaining > 0 ? '续抓 60s（剩 ' + remaining + 's）' : '激活抓样 60s' }}
      </button>
    </div>
    <p v-if="armError" class="arm-error">{{ armError }}</p>
    <div class="list">
      <div v-for="(p, i) in filtered" :key="i" class="pkt" :class="p.dir">
        <span class="dir">{{ p.dir.toUpperCase() }}</span>
        <span class="inst">{{ p.inst_id }}</span>
        <span class="hex">{{ p.hex }}</span>
        <span class="ts">{{ new Date(p.ts).toLocaleTimeString() }}</span>
      </div>
      <p v-if="!filtered.length" class="empty">
        {{ !selectedRunning && monitor.selectedInstance
          ? '实例未运行，无报文可抓（先到采集器页启动实例）'
          : remaining > 0
            ? '已激活抓样，等待收发帧…'
            : '抓样窗口未激活或已过期（点上方按钮激活/续抓 60s 窗口）' }}
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
.arm-error {
  margin: 0 0 8px;
  padding: 8px 12px;
  font-size: 12px;
  color: var(--color-danger);
  background: color-mix(in srgb, var(--color-danger) 8%, transparent);
  border: 1px solid color-mix(in srgb, var(--color-danger) 25%, transparent);
  border-radius: 10px;
}
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
