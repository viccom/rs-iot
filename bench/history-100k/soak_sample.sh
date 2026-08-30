#!/usr/bin/env bash
# Soak sampler (24h): every 60s disk sizes, every 15min metrics snapshot.
cd /e/Rs_Codes/rs-iot/bench/history-100k
i=0
while true; do
  ts=$(date +%s)
  wal=$(du -sk data/history/wal 2>/dev/null | cut -f1)
  segs=$(du -sk data/history/lane_numeric 2>/dev/null | cut -f1)
  echo "$ts wal=$wal segs=$segs" >> soak_samples.log
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
