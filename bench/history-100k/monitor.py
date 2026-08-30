#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Metric sampler for the history 100k-tag benchmark.

Every --interval seconds it scrapes:
  - GET /metrics            (PUBLIC: no JWT needed; Prometheus text)
  - GET /api/v1/sysstatus   (JWT protected -> logs in once for a token)
and samples the rs-iot process RSS via PowerShell Get-Process.

Key metrics recorded to CSV (exact names from src/api/routes_metrics/render.rs):
  rs_iot_history_{sent,written,dropped,deadbanded,skipped,errors,ts_rejected}_total,
  rs_iot_history_series / archived_tags / memory_used_bytes / memory_budget_bytes,
  rs_iot_history_deadband_evaluated_total / deadband_reduction_ratio,
  rs_iot_hub_depth (gauge), rs_iot_hub_depth_max,
  rs_iot_pipeline_{processed,batch}_total, rs_iot_pipeline_workers,
  rs_iot_process_mem_bytes / process_cpu_pct, rs_iot_uptime_seconds.
Note: the history metric family is only rendered while the engine state is "ok";
while disabled/init_failed those columns stay empty.

Usage:
  python monitor.py --duration 600 --interval 15 \
      [--base http://127.0.0.1:8791] [--out metrics.csv]

Exit code 0 (also on Ctrl+C - prints the summary first).
"""

import argparse
import csv
import subprocess
import sys
import time

import requests

CONN_RETRIES = 3
CONN_RETRY_DELAY_S = 2.0

# CSV column -> /metrics metric name ("" = computed locally).
COLUMNS = [
    ("epoch_s", ""),
    ("hist_sent", "rs_iot_history_sent_total"),
    ("hist_written", "rs_iot_history_written_total"),
    ("hist_dropped", "rs_iot_history_dropped_total"),
    ("hist_deadbanded", "rs_iot_history_deadbanded_total"),
    ("hist_skipped", "rs_iot_history_skipped_total"),
    ("hist_errors", "rs_iot_history_errors_total"),
    ("hist_ts_rejected", "rs_iot_history_ts_rejected_total"),
    ("hist_series", "rs_iot_history_series"),
    ("hist_archived_tags", "rs_iot_history_archived_tags"),
    ("hist_mem_used", "rs_iot_history_memory_used_bytes"),
    ("hist_mem_budget", "rs_iot_history_memory_budget_bytes"),
    ("hist_db_evaluated", "rs_iot_history_deadband_evaluated_total"),
    ("hist_db_ratio", "rs_iot_history_deadband_reduction_ratio"),
    ("hub_depth", "rs_iot_hub_depth"),
    ("hub_depth_max", "rs_iot_hub_depth_max"),
    ("pipeline_processed", "rs_iot_pipeline_processed_total"),
    ("pipeline_batch", "rs_iot_pipeline_batch_total"),
    ("pipeline_workers", "rs_iot_pipeline_workers"),
    ("proc_mem_bytes", "rs_iot_process_mem_bytes"),
    ("proc_cpu_pct", "rs_iot_process_cpu_pct"),
    ("uptime_s", "rs_iot_uptime_seconds"),
    ("rss_bytes", ""),
    ("sys_engine", ""),
]

# Labeled metrics (e.g. rs_iot_history_write_errors_total{kind=...}) are summed
# per base name and appended as extra "<base>_sum" columns.
LABELED_BASES = ["rs_iot_history_write_errors_total"]


def log(msg: str) -> None:
    print(msg, flush=True)


def request_with_retry(session: requests.Session, method: str, url: str, **kwargs):
    """Connection-level failures retried 3x (2s apart); 4xx/5xx returned as-is."""
    last_exc = None
    for attempt in range(1 + CONN_RETRIES):
        try:
            return session.request(method, url, timeout=30, **kwargs)
        except (requests.exceptions.ConnectionError, requests.exceptions.Timeout) as e:
            last_exc = e
            if attempt < CONN_RETRIES:
                log(f"[retry] {e.__class__.__name__} on {method} {url} - "
                    f"retry {attempt + 1}/{CONN_RETRIES}")
                time.sleep(CONN_RETRY_DELAY_S)
    raise last_exc


def login(session: requests.Session, base: str) -> str:
    resp = request_with_retry(session, "POST", base + "/api/v1/user/login",
                              json={"user_id": "admin", "pass": "admin"})
    if resp.status_code != 200:
        raise RuntimeError(f"login failed: HTTP {resp.status_code} {resp.text[:200]}")
    token = resp.json().get("token")
    if not token:
        raise RuntimeError(f"login returned no token: {resp.text[:200]}")
    return token


def parse_prometheus(text: str):
    """Parse exposition text -> ({metric_name: value}, {base: value_sum_over_labels}).

    Unlabeled samples win; labeled samples are summed per base name.
    """
    plain = {}
    labeled_sums = {b: 0.0 for b in LABELED_BASES}
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.rsplit(" ", 1)
        if len(parts) != 2:
            continue
        name_part, raw_val = parts
        try:
            val = float(raw_val)
        except ValueError:
            continue
        if name_part.endswith("}"):
            brace = name_part.find("{")
            if brace <= 0:
                continue
            base = name_part[:brace]
            if base in labeled_sums:
                labeled_sums[base] += val
        else:
            plain[name_part] = val
    return plain, labeled_sums


def sample_rss() -> str:
    """Process working set in bytes via PowerShell; '' on any failure."""
    try:
        out = subprocess.run(
            ["powershell", "-NoProfile", "-Command",
             "Get-Process rs-iot | Select-Object -ExpandProperty WS"],
            capture_output=True, text=True, timeout=30,
        )
        vals = [int(x) for x in out.stdout.split() if x.strip().isdigit()]
        return str(max(vals)) if vals else ""
    except Exception:
        return ""


def fmt(v) -> str:
    if v is None or v == "":
        return "-"
    if isinstance(v, float):
        return f"{v:.2f}"
    return str(v)


def main() -> int:
    ap = argparse.ArgumentParser(description="history 100k bench metric sampler")
    ap.add_argument("--duration", type=float, default=600,
                    help="total sampling duration in seconds (default 600)")
    ap.add_argument("--interval", type=float, default=15,
                    help="sampling interval in seconds (default 15)")
    ap.add_argument("--base", default="http://127.0.0.1:8791",
                    help="rs-iot REST base URL")
    ap.add_argument("--out", default="metrics.csv", help="output CSV path")
    args = ap.parse_args()
    base = args.base.rstrip("/")

    session = requests.Session()
    try:
        token = login(session, base)
        log(f"[monitor] base={base} duration={args.duration:.0f}s "
            f"interval={args.interval:.0f}s out={args.out} (login ok)")
    except Exception as e:
        log(f"[monitor] FATAL: {e}")
        return 1

    headers = {"Authorization": f"Bearer {token}"}
    fieldnames = [c for c, _ in COLUMNS] + [f"{b}_sum" for b in LABELED_BASES]
    rows = []
    t_start = time.time()
    next_t = t_start

    try:
        while True:
            now = time.time()
            if now - t_start >= args.duration:
                break
            epoch = int(now)

            # /metrics (public - no auth header needed, but harmless).
            try:
                m_resp = request_with_retry(session, "GET", base + "/metrics")
                if m_resp.status_code != 200:
                    log(f"[monitor] WARN: /metrics HTTP {m_resp.status_code}")
                    plain, labeled_sums = {}, {b: 0.0 for b in LABELED_BASES}
                else:
                    plain, labeled_sums = parse_prometheus(m_resp.text)
            except requests.RequestException as e:
                log(f"[monitor] WARN: /metrics unreachable: {e}")
                plain, labeled_sums = {}, {b: 0.0 for b in LABELED_BASES}

            # /api/v1/sysstatus (JWT): engine state only; numbers come from /metrics.
            sys_engine = ""
            try:
                s_resp = request_with_retry(session, "GET",
                                            base + "/api/v1/sysstatus",
                                            headers=headers)
                if s_resp.status_code == 200:
                    sys_engine = str(s_resp.json().get("history", {})
                                     .get("engine", ""))
                else:
                    log(f"[monitor] WARN: /api/v1/sysstatus HTTP {s_resp.status_code}")
            except (requests.RequestException, ValueError) as e:
                log(f"[monitor] WARN: sysstatus failed: {e}")

            rss = sample_rss()

            row = {"epoch_s": epoch, "rss_bytes": rss, "sys_engine": sys_engine}
            for col, metric in COLUMNS:
                if metric:
                    row[col] = plain.get(metric)
            for b in LABELED_BASES:
                row[f"{b}_sum"] = labeled_sums.get(b)
            rows.append(row)

            # Single-line round summary + written rate vs previous round.
            rate = ""
            if len(rows) >= 2:
                prev, cur = rows[-2], rows[-1]
                dt = cur["epoch_s"] - prev["epoch_s"]
                if dt > 0 and prev.get("hist_written") is not None \
                        and cur.get("hist_written") is not None:
                    rate = (f" rate={((cur['hist_written'] - prev['hist_written']) / dt):.0f}pt/s")
            log(f"[monitor] t={epoch} engine={sys_engine or '-'} "
                f"written={fmt(row.get('hist_written'))}{rate} "
                f"sent={fmt(row.get('hist_sent'))} "
                f"dropped={fmt(row.get('hist_dropped'))} "
                f"series={fmt(row.get('hist_series'))} "
                f"hub_depth={fmt(row.get('hub_depth'))} "
                f"rss={fmt(row.get('rss_bytes'))}")

            next_t += args.interval
            sleep_for = next_t - time.time()
            if sleep_for > 0:
                time.sleep(sleep_for)
    except KeyboardInterrupt:
        log("[monitor] interrupted by user - writing collected samples")

    # Persist CSV.
    if rows:
        with open(args.out, "w", newline="", encoding="utf-8") as f:
            writer = csv.DictWriter(f, fieldnames=fieldnames)
            writer.writeheader()
            writer.writerows(rows)
        log(f"[monitor] wrote {len(rows)} samples to {args.out}")
    else:
        log("[monitor] no samples collected")

    # Summary over the observed window.
    log("=" * 66)
    log("[monitor] SUMMARY")
    valid = [r for r in rows if r.get("hist_written") is not None]
    if len(valid) >= 2:
        first, last = valid[0], valid[-1]
        dt = max(1, last["epoch_s"] - first["epoch_s"])
        delta = last["hist_written"] - first["hist_written"]
        log(f"  window            : {first['epoch_s']} -> {last['epoch_s']} ({dt}s, "
            f"{len(valid)} valid samples)")
        log(f"  written delta     : {delta:.0f} pts")
        log(f"  avg written rate  : {delta / dt:.0f} pts/s")
        log(f"  sent (last)       : {last.get('hist_sent', '-')}")
        log(f"  dropped (last)    : {last.get('hist_dropped', '-')}")
        log(f"  deadbanded (last) : {last.get('hist_deadbanded', '-')}")
        log(f"  skipped (last)    : {last.get('hist_skipped', '-')}")
        log(f"  series (last)     : {last.get('hist_series', '-')}")
        log(f"  errors (last)     : {last.get('hist_errors', '-')}")
        rss_vals = [int(r["rss_bytes"]) for r in rows
                    if str(r.get("rss_bytes", "")).isdigit()]
        if rss_vals:
            log(f"  rss peak          : {max(rss_vals)} bytes "
                f"({max(rss_vals) / (1024 * 1024):.1f} MiB)")
        else:
            log("  rss peak          : n/a (PowerShell sampling failed)")
    else:
        log("  not enough valid samples for a rate summary")
    return 0


if __name__ == "__main__":
    sys.exit(main())
