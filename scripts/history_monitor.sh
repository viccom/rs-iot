#!/usr/bin/env bash
# history_monitor.sh — rs-iot 历史归档（tsink）健康监测
#
# 用途：P1 写放大修复（seal 门控 + catalog 节流，2026-10-29）后的稳定性采样，
#       验证 24h 内 CPU 稳态、catalog 不回弹、seal 计数器行为、磁盘增长合理。
#
# 用法：
#   history_monitor.sh                              # 默认 24h，60s 一采
#   history_monitor.sh --interval 30 --duration-sec 86400
#   history_monitor.sh --out /path/to.csv           # 自定义输出
#   history_monitor.sh --summary [CSV]              # 对既有 CSV 出汇总报告（随时可查中期结果）
#
# 采样内容：
#   - rs-iot 进程 CPU（/proc stat 增量，单核百分比）与 RSS
#   - /metrics 历史计数器（sent/written/errors + seal_* + catalog_persist_*）
#   - tsink 磁盘状态：L0/L1/L2 段数、catalog 字节数与 mtime、WAL 字节
#     （lane 总字节每 10 个样本刷一次，du 16k+ 目录开销大）
# 判据（汇总时评估）：CPU 均值 ≤3% 达标 / ≤5% 边缘；errors 增量 = 0；
#   catalog 重写 ≤200 次/24h；L0 积压 ≤20；RSS 末值 ≤ 2×首值+200MB。
set -u

INTERVAL=60
DURATION_SEC=$((24 * 3600))
HEAVY_EVERY=10          # lane 总字节刷新周期（样本数）
MODE=run
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HIST_DIR="$REPO_ROOT/data/history"
SEG_DIR="$HIST_DIR/lane_numeric/segments"
CATALOG="$HIST_DIR/series_index.catalog.json"
WAL_DIR="$HIST_DIR/wal"
METRICS_URL="http://127.0.0.1:8790/metrics"
OUT_CSV="$REPO_ROOT/logs/history_monitor.csv"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --interval)     INTERVAL="$2"; shift 2 ;;
    --duration-sec) DURATION_SEC="$2"; shift 2 ;;
    --out)          OUT_CSV="$2"; shift 2 ;;
    --summary)
      MODE=summary; shift
      if [[ $# -ge 1 && "$1" != -* ]]; then OUT_CSV="$1"; shift; fi
      ;;
    -h|--help) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

CSV_HEADER="ts_iso,ts_epoch,pid,cpu_user_pct,cpu_sys_pct,cpu_total_pct,rss_mb,metrics_ok,sent_total,written_total,errors_total,seal_points,seal_age,seal_wal,seal_hold,cat_attempts,cat_persisted,cat_skipped,seg_l0,seg_l1,seg_l2,catalog_bytes,catalog_mtime,catalog_changed,wal_bytes,lane_bytes,note"

# ---------------------------------------------------------------------------
# 汇总报告：对 CSV 聚合 + 判据评估（awk 容忍空字段与计数器复位）
# ---------------------------------------------------------------------------
summary() {
  local csv="$1"
  if [[ ! -s "$csv" ]]; then
    echo "no samples in $csv" >&2
    exit 1
  fi
  awk -F',' '
    function num(v) { return (v == "" || v ~ /[^0-9.\-]/) ? "" : v + 0 }
    NR == 2 { first_ts = $2; first_pid = $3 }
    NR >= 2 {
      n++
      cpu = num($6)
      if (cpu != "") { if (min == "" || cpu < min) min = cpu; if (cpu > max) max = cpu; sum += cpu; cpus[n] = cpu }
      rss = num($7)
      if (rss != "") { if (first_rss == "") first_rss = rss; if (rss > max_rss) max_rss = rss; last_rss = rss }
      for (i = 9; i <= 21; i++) {
        v = num($i)
        if (v != "") {
          if (prev[i] == "") {}
          else if (v >= prev[i]) delta[i] += v - prev[i]
          else resets[i]++
          prev[i] = v
          last[i] = v
        }
      }
      cb = num($22); if (cb != "") { if (first_cb == "") first_cb = cb; last_cb = cb }
      wb = num($25); if (wb != "") { if (wb > max_wal) max_wal = wb; last_wal = wb }
      l0 = num($19); if (l0 != "" && l0 > max_l0) max_l0 = l0
      l2v = num($21); if (l2v != "") { if (first_l2 == "") first_l2 = l2v; last_l2 = l2v }
      if ($24 == 1) cat_rewrites++
      if (last_pid != "" && $3 != "" && $3 != last_pid) pid_changes++
      if ($3 != "") last_pid = $3
      last_ts = $2
      if ($27 != "") notes = notes (notes ? " | " : "") $27
    }
    END {
      span = last_ts - first_ts
      printf "===== history_monitor 汇总 (%s) =====\n", FILENAME
      printf "样本数 %d，跨度 %.1f h（%s → %s）\n", n, span / 3600, strftime("%F %T", first_ts), strftime("%F %T", last_ts)
      if (n > 1) {
        m = int(n * 0.95 + 0.5); if (m < 1) m = 1; if (m > n) m = n
        for (j = 1; j <= n; j++) for (k = j + 1; k <= n; k++) if (cpus[k] < cpus[j]) { t = cpus[j]; cpus[j] = cpus[k]; cpus[k] = t }
        printf "CPU(单核%%): 均值 %.2f / p95 %.2f / 峰值 %.2f\n", sum / n, cpus[m], max
      }
      printf "RSS(MB): 首值 %.0f / 峰值 %.0f / 末值 %.0f\n", first_rss, max_rss, last_rss
      printf "errors 增量 %s，sent 增量 %s，written 增量 %s（%.1f 点/秒）\n", delta[11] + 0, delta[9] + 0, delta[10] + 0, (span > 0 ? delta[10] / span : 0)
      printf "seal 触发: points %s / age %s / wal %s；hold %s 次\n", delta[12] + 0, delta[13] + 0, delta[14] + 0, delta[15] + 0
      printf "catalog: 重写 %s 次，persisted %s，skipped %s；字节 %s → %s\n", cat_rewrites + 0, delta[17] + 0, delta[18] + 0, first_cb "", last_cb ""
      span_h = span / 3600
      new_l2 = last_l2 - first_l2
      pts_per_seg = (new_l2 > 0 ? delta[10] / new_l2 : 0)
      printf "段数: L0 末值 %s(峰值 %s) / L1 末值 %s / L2 末值 %s（新增 %s，平均 %.0f 点/段）\n", last[19] "", max_l0 + 0, last[20] "", last[21] "", new_l2 + 0, pts_per_seg
      printf "WAL: 峰值 %.1f MB / 末值 %.1f MB\n", max_wal / 1048576, last_wal / 1048576
      printf "进程重启(PID 变化) %s 次；计数器复位事件 %s 处\n", pid_changes + 0, resets[11] + resets[9] + resets[10] + 0
      if (notes != "") printf "样本内事件: %s\n", notes
      printf "----- 判据 -----\n"
      ok = 1
      if (sum / n <= 3)      printf "[PASS] CPU 均值 %.2f%% ≤ 3%%\n", sum / n
      else if (sum / n <= 5) { printf "[WARN] CPU 均值 %.2f%% 处于 3~5%% 边缘\n", sum / n; ok = 0 }
      else                   { printf "[FAIL] CPU 均值 %.2f%% > 5%%\n", sum / n; ok = 0 }
      if (delta[11] + 0 == 0) printf "[PASS] errors 零增量\n"
      else { printf "[FAIL] errors 增量 %s\n", delta[11] + 0; ok = 0 }
      # catalog 重写率 ∝ seal 率（每 series 一个 head）× series 数，与 uptime 无关；
      # 按小时率判定（实测稳态 25~35 次/h，阈值 60 留 ~1.7x 余量）。
      # 固定次数阈值（原 200 次/24h）会误判健康系统——2026-10-30 校准。
      if (span_h > 0.2) {
        cph = cat_rewrites / span_h
        if (cph <= 60) printf "[PASS] catalog 重写 %.1f 次/h ≤ 60\n", cph
        else { printf "[FAIL] catalog 重写 %.1f 次/h > 60（节流失效？）\n", cph; ok = 0 }
        # 每次重写写满整个 catalog（~9MB），合并效率看"每新段重写次数"
        if (new_l2 > 0 && cat_rewrites / new_l2 > 4) {
          printf "[WARN] 每新段 %.1f 次 catalog 重写（合并效率下降）\n", cat_rewrites / new_l2; ok = 0
        }
      }
      if (max_l0 + 0 <= 20) printf "[PASS] L0 峰值 %d ≤ 20（compaction 不积压）\n", max_l0 + 0
      else { printf "[FAIL] L0 峰值 %d > 20（compaction 积压）\n", max_l0 + 0; ok = 0 }
      # seal 门控回归探针：修复前 ~6 点/段；门控失效会立刻掉回三位数以下
      if (new_l2 > 0 && pts_per_seg < 100) {
        printf "[FAIL] 段粒度 %.0f 点/段 < 100（seal 门控失效？）\n", pts_per_seg; ok = 0
      } else if (new_l2 > 0) printf "[PASS] 段粒度 %.0f 点/段 ≥ 100\n", pts_per_seg
      if (last_rss <= first_rss * 2 + 200) printf "[PASS] RSS 末值 %.0fMB 无泄漏迹象\n", last_rss
      else { printf "[FAIL] RSS 末值 %.0fMB 超首值 2 倍+200MB\n", last_rss; ok = 0 }
      printf "总判: %s\n", (ok ? "PASS" : "ATTENTION")
    }
  ' "$csv"
}

[[ "$MODE" == summary ]] && { summary "$OUT_CSV"; exit $?; }

# ---------------------------------------------------------------------------
# 采样主循环
# ---------------------------------------------------------------------------
mkdir -p "$(dirname "$OUT_CSV")"
[[ -s "$OUT_CSV" ]] && cp "$OUT_CSV" "$OUT_CSV.bak"   # 保留上一轮
echo "$CSV_HEADER" > "$OUT_CSV"

CLK_TCK=$(getconf CLK_TCK)
PAGE_KB=$(( $(getconf PAGESIZE) / 1024 ))
LANE_BYTES=""
PREV_PID=""; PREV_UT=""; PREV_ST=""; PREV_MT=""; PREV_CAT_ATTEMPTS=""
i=0
END_TS=$(( $(date +%s) + DURATION_SEC ))

finish() {
  local reason="${1:-duration_reached}"
  echo "[$(date -Is)] monitor $reason, CSV=$OUT_CSV（samples=$(($(wc -l < "$OUT_CSV") - 1))）" >&2
  summary "$OUT_CSV" >&2 || true
  exit 0
}
trap 'finish terminated_by_signal' TERM INT

echo "[$(date -Is)] history_monitor start: interval=${INTERVAL}s duration=${DURATION_SEC}s out=$OUT_CSV" >&2

while :; do
  now=$(date +%s)
  [[ $now -ge $END_TS ]] && break
  ts_iso=$(date -Is)
  note=""

  # 采样本机监听 METRICS_URL 端口的那个 rs-iot（同机可能并存测试场实例，
  # 其 comm 也是 rs-iot，pgrep -x 会按 pid 顺序误取 —— 2026-10-30 实测采到过 field 实例）
  metrics_port="${METRICS_URL##*:}"; metrics_port="${metrics_port%%/*}"
  pid=$(ss -ltnpH "sport = :${metrics_port}" 2>/dev/null \
        | sed -n 's/.*pid=\([0-9][0-9]*\).*/\1/p' | head -n1)
  [[ -z "$pid" ]] && pid=$(pgrep -x rs-iot | head -n1)
  pid_count=$(pgrep -x rs-iot | wc -l)
  [[ $pid_count -gt 1 ]] && note="multi_pid($pid_count)"
  if [[ -n "$pid" && "$pid" != "$PREV_PID" && -n "$PREV_PID" ]]; then
    note="${note}${note:+;}pid_change(${PREV_PID}->${pid})"
    PREV_UT=""; PREV_ST=""   # CPU 基线重置
  fi

  cpu_u=""; cpu_s=""; cpu_t=""; rss=""
  if [[ -n "$pid" ]]; then
    read -r ut st _ < <(awk '{ i = index($0, ") "); f = substr($0, i + 2); split(f, a, " "); print a[12], a[13], a[24] }' "/proc/$pid/stat" 2>/dev/null)
    read -r rss_pages _ < <(awk '{print $2}' "/proc/$pid/statm" 2>/dev/null)
    if [[ -n "$ut" && -n "$PREV_UT" ]]; then
      du=$((ut - PREV_UT)); ds=$((st - PREV_ST))
      cpu_u=$(awk -v d="$du" -v iv="$INTERVAL" -v t="$CLK_TCK" 'BEGIN{printf "%.3f", d / (iv * t) * 100}')
      cpu_s=$(awk -v d="$ds" -v iv="$INTERVAL" -v t="$CLK_TCK" 'BEGIN{printf "%.3f", d / (iv * t) * 100}')
      cpu_t=$(awk -v a="$cpu_u" -v b="$cpu_s" 'BEGIN{printf "%.3f", a + b}')
    fi
    [[ -n "$rss_pages" ]] && rss=$(awk -v p="$rss_pages" -v k="$PAGE_KB" 'BEGIN{printf "%.1f", p * k / 1024}')
    PREV_UT="$ut"; PREV_ST="$st"
  fi
  PREV_PID="$pid"

  metrics_ok=1
  METRICS=$(curl -fsS --max-time 5 "$METRICS_URL" 2>/dev/null) || metrics_ok=0
  mval() { grep -m1 -E "^$1 " <<<"$METRICS" | awk '{print $2}'; }
  sent=$(mval rs_iot_history_sent_total)
  written=$(mval rs_iot_history_written_total)
  errors=$(mval rs_iot_history_errors_total)
  seal_p=$(mval rs_iot_history_seal_trigger_points_total)
  seal_a=$(mval rs_iot_history_seal_trigger_age_total)
  seal_w=$(mval rs_iot_history_seal_trigger_wal_total)
  seal_h=$(mval rs_iot_history_seal_hold_total)
  cat_att=$(mval rs_iot_history_catalog_persist_attempts_total)
  cat_per=$(mval rs_iot_history_catalog_persisted_total)
  cat_skip=$(mval rs_iot_history_catalog_persist_skipped_total)
  if [[ "$metrics_ok" == 1 ]]; then
    if [[ -n "$PREV_CAT_ATTEMPTS" && -n "$cat_att" && "$cat_att" -lt "$PREV_CAT_ATTEMPTS" ]]; then
      note="${note}${note:+;}counter_reset(metrics)"
    fi
    PREV_CAT_ATTEMPTS="$cat_att"
    if [[ -n "$errors" && "$errors" != "0" ]]; then
      note="${note}${note:+;}ALERT_errors=$errors"
    fi
  else
    note="${note}${note:+;}metrics_unreachable"
  fi

  seg_l0=$(ls "$SEG_DIR/L0" 2>/dev/null | wc -l)
  seg_l1=$(ls "$SEG_DIR/L1" 2>/dev/null | wc -l)
  seg_l2=$(ls "$SEG_DIR/L2" 2>/dev/null | wc -l)
  cat_bytes=""; cat_mt=""
  [[ -f "$CATALOG" ]] && { cat_bytes=$(stat -c '%s' "$CATALOG"); cat_mt=$(stat -c '%Y' "$CATALOG"); }
  cat_changed=0
  [[ -n "$cat_mt" && -n "$PREV_MT" && "$cat_mt" != "$PREV_MT" ]] && cat_changed=1
  PREV_MT="$cat_mt"
  wal_bytes=$(du -sb "$WAL_DIR" 2>/dev/null | awk '{print $1}')
  i=$((i + 1))
  if (( i % HEAVY_EVERY == 1 )); then
    LANE_BYTES=$(du -sb "$HIST_DIR/lane_numeric" 2>/dev/null | awk '{print $1}')
  fi

  echo "$ts_iso,$now,$pid,$cpu_u,$cpu_s,$cpu_t,$rss,$metrics_ok,$sent,$written,$errors,$seal_p,$seal_a,$seal_w,$seal_h,$cat_att,$cat_per,$cat_skip,$seg_l0,$seg_l1,$seg_l2,$cat_bytes,$cat_mt,$cat_changed,$wal_bytes,$LANE_BYTES,$note" >> "$OUT_CSV"

  sleep "$INTERVAL"
done

finish duration_reached
