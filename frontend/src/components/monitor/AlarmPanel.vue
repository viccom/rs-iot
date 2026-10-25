<!-- frontend/src/components/monitor/AlarmPanel.vue
  活跃告警面板（Q12 告警最小闭环，spec §5）：level 徽标 / key+message / 来源 / 次数 /
  时间 / 手动清除。默认显示（告警是运维刚需，照 rtdata 面板模式不做按需激活）——
  数据来自 monitor.alarms（GET /alarms 登录即拉 + SSE alarm 帧收敛 + 重连补拉）。
  手动清除调 clearAlarm（REST DELETE），条目移除由后端发的 SSE Clear 帧收敛。
-->
<script setup lang="ts">
import { useMonitorStore } from '../../stores/monitor'
import type { AlarmItem } from '../../types'

const monitor = useMonitorStore()

/** level 徽标 class（info 蓝 / warn 黄 / error 红；非法值回落 info——入口校验在宿主，防御显示）。 */
function lvClass(lv: string): string {
  return lv === 'warn' || lv === 'error' ? lv : 'info'
}

/** ISO8601 → 本地时间（对齐 Lua.vue 版本历史的 toLocaleString 先例）。 */
function fmtTs(ts: string): string {
  const d = new Date(ts)
  return Number.isNaN(d.getTime()) ? ts : d.toLocaleString()
}

/** 手动清除一条告警（复审 P3：确认对话框防误触——清除后需脚本重新判定才会
 * 再现；失败 console.warn 可见——SSE 帧收敛是唯一移除路径，静默失败无感知）。 */
async function clear(item: AlarmItem) {
  if (!window.confirm(`清除告警「${item.key}」？（活跃清单移除，重新触发需脚本再判定）`)) return
  const ok = await monitor.clearAlarm(item.key)
  if (!ok) console.warn('AlarmPanel: 清除失败（REST 非 2xx/网络断）——面板经 SSE/30s 轮询收敛', item.key)
}

/** 风暴计数显示封顶（复审 P3：防大数撑破 ×count 列宽）。 */
function fmtCount(n: number): string {
  return n > 999 ? '999+' : String(n)
}
</script>

<template>
  <div class="alarm-panel">
    <div class="head">
      <span class="count">{{ monitor.alarms.length }} 条活跃</span>
      <button class="refresh" title="手动刷新（SSE 断线期间的丢失帧经全量拉取收敛）" @click="monitor.loadAlarms()">刷新</button>
    </div>

    <div class="scroll-area">
      <div v-for="a in monitor.alarms" :key="a.key" class="row">
        <span class="lv" :class="lvClass(a.level)">{{ a.level }}</span>
        <div class="what">
          <span class="key">{{ a.key }}</span>
          <span class="msg" :title="a.message">{{ a.message }}</span>
        </div>
        <span class="src" :title="`来源实例：${a.source}`">{{ a.source }}</span>
        <span class="times" :class="{ storm: a.count > 1 }" :title="`累计触发 ${a.count} 次`">×{{ fmtCount(a.count) }}</span>
        <span class="ts">{{ fmtTs(a.ts) }}</span>
        <button class="clear" @click="clear(a)">清除</button>
      </div>
    </div>

    <p v-if="!monitor.alarms.length" class="empty">无活跃告警</p>
  </div>
</template>

<style scoped>
.alarm-panel {
  display: flex;
  flex-direction: column;
  overflow: hidden;
  /* 内滚上限对齐 RtdataPanel 的 clamp 模式（视口自适应 + 兜底）。 */
  height: clamp(200px, calc(40vh - 160px), 480px);
}

.head {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: 0 0 12px;
  border-bottom: 1px solid var(--color-divider);
}

.count {
  padding: 4px 12px;
  border-radius: 999px;
  background: var(--chip-bg);
  border: 1px solid var(--chip-border);
  color: var(--chip-ink);
  font-size: 12px;
}

.refresh, .clear {
  padding: 5px 12px;
  border-radius: 8px;
  border: 1px solid var(--card-border);
  background: var(--card-bg);
  color: var(--color-ink-secondary);
  font-size: 12px;
  cursor: pointer;
}
.refresh:hover, .clear:hover { color: var(--color-ink); border-color: var(--color-divider-strong); }

.scroll-area { flex: 1; overflow-y: auto; scrollbar-width: thin; }

.row {
  display: flex;
  align-items: center;
  gap: 10px;
  min-height: 42px;
  padding: 6px 0;
  border-bottom: 1px solid var(--card-border);
}
.row:hover { background: var(--table-row-hover); }

/* level 徽标：info 蓝（primary）/ warn 黄 / error 红（LogStream 的语义色先例）。 */
.lv {
  flex: none;
  width: 52px;
  padding: 2px 0;
  border-radius: 999px;
  text-align: center;
  font-size: 11px;
  letter-spacing: 0.06em;
  text-transform: uppercase;
}
.lv.info { background: color-mix(in srgb, var(--color-primary) 14%, transparent); color: var(--color-primary); }
.lv.warn { background: color-mix(in srgb, var(--color-warning) 16%, transparent); color: var(--color-warning); }
.lv.error { background: color-mix(in srgb, var(--color-danger) 16%, transparent); color: var(--color-danger); }

.what { flex: 1; min-width: 0; display: grid; gap: 2px; }
.key { font-family: Consolas, monospace; font-size: 13px; color: var(--color-ink); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.msg { font-size: 12px; color: var(--color-ink-muted); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

.src { flex: none; width: 110px; font-size: 12px; color: var(--color-primary); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.times { flex: none; width: 44px; text-align: right; font-family: Consolas, monospace; font-size: 12px; color: var(--color-ink-muted); }
/* count>1（重复触发/风暴可见性）提色——运维一眼定位刷屏告警。 */
.times.storm { color: var(--color-warning); font-weight: 600; }
.ts { flex: none; width: 150px; font-family: Consolas, monospace; font-size: 12px; color: var(--color-ink-muted); }
.clear { flex: none; }

.empty { color: var(--color-ink-muted); padding: 18px; text-align: center; margin: 0; }
</style>
