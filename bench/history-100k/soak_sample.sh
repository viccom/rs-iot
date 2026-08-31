#!/usr/bin/env bash
# N5 retest sampler: every 60s disk sizes + L0/L1/L2 segment counts + WAL file
# count + RSS; every 15min metrics snapshot.
cd /e/Rs_Codes/rs-iot/bench/history-100k
SEGS=data/history/lane_numeric/segments
i=0
while true; do
  ts=$(date +%s)
  wal=$(du -sk data/history/wal 2>/dev/null | cut -f1)
  segkb=$(du -sk data/history/lane_numeric 2>/dev/null | cut -f1)
  l0=$(find $SEGS/L0 -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l)
  l1=$(find $SEGS/L1 -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l)
  l2=$(find $SEGS/L2 -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l)
  walfiles=$(find data/history/wal -name "*.log" 2>/dev/null | wc -l)
  rss=$(powershell -NoProfile -Command "[Math]::Round((Get-Process rs-iot -ErrorAction SilentlyContinue).WS/1MB,0)" 2>/dev/null | tr -d '\r')
  echo "$ts wal=$wal walfiles=$walfiles segs=$segkb L0=$l0 L1=$l1 L2=$l2 rssMB=$rss" >> soak_samples.log
  if [ $((i % 15)) -eq 0 ]; then
    {
      curl -s -m 60 http://127.0.0.1:8791/metrics \
        | grep -E "^rs_iot_history_(sent_total|written_total|dropped_total|errors_total|series|memory_used_bytes) " \
        | tr '\n' ' '
      echo "t=$(date +%s)"
    } >> soak_metrics.log 2>&1
  fi
  i=$((i+1))
  sleep 60
done
