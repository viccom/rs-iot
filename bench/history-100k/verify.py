#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Functional assertions for the history 100k-tag benchmark.

Runs 10 checks against a live, provisioned rs-iot service and prints
PASS/FAIL/WARN per case with evidence; exit code is non-zero if any case
FAILed. WARN does not fail the run.

Contracts verified against source (src/api/routes_history.rs, engine.rs,
writer.rs, rows.rs):
  - GET /api/v1/history?dev_id&tag&start&end[&agg&interval][&limit] (JWT).
    200 body: {"dev_id","tag","points":[{"ts":<sec>,"value":<num>}],"count":N}
    (same shape for raw and downsample; bucket ts = bucket start, buckets are
    origin-aligned to the query start and empty buckets are omitted).
  - Negative paths return 400 PLAIN TEXT (e.g. "agg requires interval",
    "start must < end"); an empty/nonexistent series returns 200 with
    points=[] count=0 (NOT an error).
  - limit: raw = oldest n points; downsample = first n buckets.
  - Deadband (writer.rs): the FIRST sample of a series has no last_val and is
    written directly; later samples within |new-last| < deadband are counted
    as deadbanded -> chk_dead keeps <= 1 point per process lifetime.
  - dataSim: v = round2(50 + 40*sin(...)) -> value domain [10, 90].

Usage:
  python verify.py [--base http://127.0.0.1:8791]
                   [--window-start <epoch-sec>] [--window-end <epoch-sec>]
  (window defaults to now-15min .. now)
"""

import argparse
import math
import os
import sys
import time

import requests

CONN_RETRIES = 3
CONN_RETRY_DELAY_S = 2.0

EXPECTED_SERIES = 100_001          # 1000 devices x 100 tags + chk_dead
EXPECTED_ARCHIVED_TAGS = 100_001   # 1000x100 history=true + chk_dead (chk_skip excluded)
CONSERVATION_TOL = 0.02            # 2% for sent vs accounted
SINE_MIN, SINE_MAX = 10.0, 90.0    # 50 +- 40 sine, rounded to 2 decimals
EPS = 1e-6
BYTES_PER_PT_MIN, BYTES_PER_PT_MAX = 6.5, 11.0  # Phase 4 gate: 8-9B +-20%
DEFAULT_WINDOW_S = 900             # 15 minutes
HISTORY_DATA_DIR = "E:/Rs_Codes/rs-iot/bench/history-100k/data/history"

PASS, FAIL, WARN = "PASS", "FAIL", "WARN"


def log(msg: str) -> None:
    print(msg, flush=True)


class Case:
    def __init__(self, name: str):
        self.name = name
        self.status = None
        self.evidence = ""

    def done(self, status: str, evidence: str) -> None:
        self.status = status
        self.evidence = evidence.replace("\n", "; ")
        log(f"[{status}] {self.name}: {self.evidence}")


def request_with_retry(session: requests.Session, method: str, url: str, **kwargs):
    """Connection-level failures retried 3x (2s apart); 4xx/5xx returned as-is."""
    last_exc = None
    for attempt in range(1 + CONN_RETRIES):
        try:
            return session.request(method, url, timeout=120, **kwargs)
        except (requests.exceptions.ConnectionError, requests.exceptions.Timeout) as e:
            last_exc = e
            if attempt < CONN_RETRIES:
                log(f"[retry] {e.__class__.__name__} on {method} {url} - "
                    f"retry {attempt + 1}/{CONN_RETRIES}")
                time.sleep(CONN_RETRY_DELAY_S)
    raise last_exc


class Client:
    def __init__(self, base: str):
        self.base = base.rstrip("/")
        self.session = requests.Session()
        self.token = None

    def login(self):
        resp = request_with_retry(self.session, "POST",
                                  self.base + "/api/v1/user/login",
                                  json={"user_id": "admin", "pass": "admin"})
        if resp.status_code != 200 or not resp.json().get("token"):
            raise RuntimeError(f"login failed: HTTP {resp.status_code} {resp.text[:200]}")
        self.token = resp.json()["token"]

    def history(self, dev_id: str, tag: str, start: int, end: int,
                agg=None, interval=None, limit=None, auth=True):
        params = {"dev_id": dev_id, "tag": tag, "start": start, "end": end}
        if agg is not None:
            params["agg"] = agg
        if interval is not None:
            params["interval"] = interval
        if limit is not None:
            params["limit"] = limit
        headers = {"Authorization": f"Bearer {self.token}"} if auth else {}
        return request_with_retry(self.session, "GET",
                                  self.base + "/api/v1/history",
                                  params=params, headers=headers)

    def metrics(self) -> dict:
        resp = request_with_retry(self.session, "GET", self.base + "/metrics")
        resp.raise_for_status()
        out = {}
        for line in resp.text.splitlines():
            line = line.strip()
            if not line or line.startswith("#") or line.endswith("}"):
                continue
            parts = line.rsplit(" ", 1)
            if len(parts) == 2:
                try:
                    out[parts[0]] = float(parts[1])
                except ValueError:
                    pass
        return out


def check_observability(cli: Client, c: Case) -> None:
    m = cli.metrics()
    missing = [k for k in ("rs_iot_history_series", "rs_iot_history_archived_tags")
               if k not in m]
    if missing:
        c.done(FAIL, f"history family missing from /metrics: {missing} "
                     f"(engine not ok or binary built without --features history)")
        return
    series = int(m["rs_iot_history_series"])
    archived = int(m["rs_iot_history_archived_tags"])
    ok = series == EXPECTED_SERIES and archived == EXPECTED_ARCHIVED_TAGS
    # By-design counters (ts window domain, whitelist) are informational only.
    info = {k: int(m.get(k, -1)) for k in
            ("rs_iot_history_ts_rejected_total", "rs_iot_history_len_rejected_total",
             "rs_iot_history_lane_mismatch_total", "rs_iot_history_nonfinite_skipped_total",
             "rs_iot_history_dup_ts_skipped_total", "rs_iot_history_stale_ts_dropped_total")}
    c.done(PASS if ok else FAIL,
           f"series={series} (expect {EXPECTED_SERIES}), "
           f"archived_tags={archived} (expect {EXPECTED_ARCHIVED_TAGS}), "
           f"by-design counters={info}")
    errors = m.get("rs_iot_history_errors_total")
    if errors:
        log(f"[{WARN}] rs_iot_history_errors_total={int(errors)} - investigate write errors")
    tr = m.get("rs_iot_history_ts_rejected_total")
    if tr:
        log(f"[{WARN}] rs_iot_history_ts_rejected_total={int(tr)} "
            f"(by-design domain rejection, printed only)")


def check_conservation(cli: Client, c: Case) -> None:
    m = cli.metrics()
    sent = m.get("rs_iot_history_sent_total")
    if sent is None:
        c.done(FAIL, "rs_iot_history_sent_total missing (engine not running?)")
        return
    written = m.get("rs_iot_history_written_total", 0.0)
    deadbanded = m.get("rs_iot_history_deadbanded_total", 0.0)
    dropped = m.get("rs_iot_history_dropped_total", 0.0)
    errors = m.get("rs_iot_history_errors_total", 0.0)
    # Extra accounted buckets (all expected 0 with healthy sine data).
    extra = sum(m.get(k, 0.0) for k in
                ("rs_iot_history_lane_mismatch_total",
                 "rs_iot_history_nonfinite_skipped_total",
                 "rs_iot_history_dup_ts_skipped_total",
                 "rs_iot_history_stale_ts_dropped_total"))
    accounted = written + deadbanded + dropped + errors + extra
    diff = sent - accounted
    tol = sent * CONSERVATION_TOL
    skipped = m.get("rs_iot_history_skipped_total", 0.0)
    part1 = abs(diff) <= tol
    part2 = skipped >= 1
    status = PASS if (part1 and part2) else FAIL
    c.done(status,
           f"sent={int(sent)} accounted={int(accounted)} "
           f"(written={int(written)} deadbanded={int(deadbanded)} "
           f"dropped={int(dropped)} errors={int(errors)} extra={int(extra)}) "
           f"diff={int(diff)} tol={int(tol)}; skipped={int(skipped)} (expect >=1)")


def check_raw(cli: Client, c: Case, start: int, end: int) -> None:
    resp = cli.history("B100DEV0000", "t000", start, end, limit=100000)
    if resp.status_code != 200:
        c.done(FAIL, f"HTTP {resp.status_code} body={resp.text[:200]}")
        return
    data = resp.json()
    pts = data.get("points", [])
    expected = (end - start) * 0.95
    ok_count = len(pts) >= expected
    bad_ts = [p for p in pts if not (start <= p["ts"] < end)]
    bad_v = [p for p in pts
             if not (SINE_MIN - EPS <= float(p["value"]) <= SINE_MAX + EPS)]
    status = PASS if (ok_count and not bad_ts and not bad_v) else FAIL
    c.done(status, f"count={len(pts)} (expect >= {expected:.0f} for {end - start}s "
                   f"@1Hz), out-of-window ts={len(bad_ts)}, "
                   f"out-of-range values={len(bad_v)}")


def check_agg_avg(cli: Client, c: Case, start: int, end: int) -> None:
    resp = cli.history("B100DEV0000", "t000", start, end, agg="avg", interval=60)
    if resp.status_code != 200:
        c.done(FAIL, f"HTTP {resp.status_code} body={resp.text[:200]}")
        return
    pts = resp.json().get("points", [])
    min_buckets = math.floor((end - start) / 60) - 1
    bad_v = [p for p in pts
             if not (SINE_MIN - EPS <= float(p["value"]) <= SINE_MAX + EPS)]
    status = PASS if (len(pts) >= min_buckets and not bad_v) else FAIL
    c.done(status, f"buckets={len(pts)} (expect >= {min_buckets}), "
                   f"out-of-range avg values={len(bad_v)}")


def check_agg_count_last(cli: Client, c: Case, start: int, end: int) -> None:
    ev = []
    ok = True
    min_buckets = math.floor((end - start) / 60) - 1
    for agg in ("count", "last"):
        resp = cli.history("B100DEV0000", "t000", start, end, agg=agg, interval=60)
        if resp.status_code != 200:
            ok = False
            ev.append(f"{agg}: HTTP {resp.status_code} {resp.text[:100]}")
            continue
        pts = resp.json().get("points", [])
        good_count = len(pts) >= min_buckets
        if agg == "count":
            good_v = all(float(p["value"]) >= 1.0 for p in pts)
        else:
            good_v = all(SINE_MIN - EPS <= float(p["value"]) <= SINE_MAX + EPS
                         for p in pts)
        ok = ok and good_count and good_v
        ev.append(f"{agg}: buckets={len(pts)} (expect >= {min_buckets}) "
                  f"values_ok={good_v}")
    c.done(PASS if ok else FAIL, "; ".join(ev))


def check_negative_paths(cli: Client, c: Case, start: int, end: int) -> None:
    ev = []
    ok = True
    # agg without interval -> 400 plain text "agg requires interval".
    r1 = cli.history("B100DEV0000", "t000", start, end, agg="avg")
    ok1 = r1.status_code == 400
    ok = ok and ok1
    ev.append(f"agg-without-interval: HTTP {r1.status_code} body={r1.text[:60]!r}")
    # start >= end -> 400 plain text "start must < end".
    r2 = cli.history("B100DEV0000", "t000", end, start)
    ok2 = r2.status_code == 400
    ok = ok and ok2
    ev.append(f"start>=end: HTTP {r2.status_code} body={r2.text[:60]!r}")
    # Nonexistent tag -> 200 with empty points (tsink semantics, NOT an error).
    r3 = cli.history("B100DEV0000", "no_such_tag_xyz", start, end)
    if r3.status_code == 200:
        body = r3.json()
        ok3 = body.get("count") == 0 and body.get("points") == []
        ev.append(f"nonexistent-tag: HTTP 200 count={body.get('count')} "
                  f"points_len={len(body.get('points', []))}")
    else:
        ok3 = r3.status_code == 404
        ev.append(f"nonexistent-tag: HTTP {r3.status_code} (404 accepted)")
    ok = ok and ok3
    c.done(PASS if ok else FAIL, "; ".join(ev))


def check_whitelist_skip(cli: Client, c: Case, start: int, end: int) -> None:
    resp = cli.history("CHKDEV01", "chk_skip", start, end)
    if resp.status_code != 200:
        c.done(FAIL, f"HTTP {resp.status_code} body={resp.text[:200]}")
        return
    body = resp.json()
    empty = body.get("count") == 0 and body.get("points") == []
    c.done(PASS if empty else FAIL,
           f"chk_skip query: count={body.get('count')} "
           f"points_len={len(body.get('points', []))} (expect empty - not archived)")


def check_deadband(cli: Client, c: Case, start: int, end: int) -> None:
    resp = cli.history("CHKDEV01", "chk_dead", start, end)
    if resp.status_code != 200:
        c.done(FAIL, f"HTTP {resp.status_code} body={resp.text[:200]}")
        return
    pts = resp.json().get("points", [])
    m = cli.metrics()
    deadbanded = m.get("rs_iot_history_deadbanded_total", 0.0)
    # writer.rs: the FIRST sample of a series has no last_val -> written directly;
    # every later sample |new-last| < 1000 -> deadbanded. So <= 1 point per
    # process lifetime (a service restart writes one more first-sample).
    ok_pts = len(pts) <= 1
    ok_metric = deadbanded >= 1
    c.done(PASS if (ok_pts and ok_metric) else FAIL,
           f"chk_dead points={len(pts)} (expect <=1: first sample is written "
           f"directly, deadband has no last_val baseline), "
           f"deadbanded_total={int(deadbanded)} (expect >=1)")


def check_limit(cli: Client, c: Case, start: int, end: int) -> None:
    full = cli.history("B100DEV0000", "t000", start, end, limit=100000)
    if full.status_code != 200 or len(full.json().get("points", [])) < 5:
        c.done(FAIL, f"reference query failed or has <5 points "
                     f"(HTTP {full.status_code})")
        return
    ref = full.json()["points"]
    resp = cli.history("B100DEV0000", "t000", start, end, limit=5)
    if resp.status_code != 200:
        c.done(FAIL, f"HTTP {resp.status_code} body={resp.text[:200]}")
        return
    pts = resp.json().get("points", [])
    exactly5 = len(pts) == 5
    oldest = pts == ref[:5]  # limit takes the OLDEST n (tsink semantics)
    c.done(PASS if (exactly5 and oldest) else FAIL,
           f"limit=5 returned {len(pts)} points (expect exactly 5), "
           f"matches oldest-5 of full query: {oldest} "
           f"(first ts full={ref[0]['ts']} vs limited={pts[0]['ts'] if pts else 'n/a'})")


def check_disk(cli: Client, c: Case) -> None:
    total = 0
    files = 0
    for root, _dirs, names in os.walk(HISTORY_DATA_DIR):
        for n in names:
            try:
                total += os.path.getsize(os.path.join(root, n))
                files += 1
            except OSError:
                pass
    m = cli.metrics()
    written = m.get("rs_iot_history_written_total")
    if not written:
        c.done(FAIL, f"written_total={written} - cannot estimate bytes/point")
        return
    b_per_pt = total / written if written else 0.0
    in_gate = BYTES_PER_PT_MIN <= b_per_pt <= BYTES_PER_PT_MAX
    status = PASS if in_gate else WARN  # Phase 4 gate 8-9B +-20%; outside = WARN
    c.done(status,
           f"data_dir={HISTORY_DATA_DIR} files={files} total={total} bytes "
           f"({total / (1024 * 1024):.1f} MiB), written_total={int(written)}, "
           f"bytes/point={b_per_pt:.2f} "
           f"(gate {BYTES_PER_PT_MIN}-{BYTES_PER_PT_MAX}; outside gate = WARN)")


def main() -> int:
    ap = argparse.ArgumentParser(description="history 100k bench functional assertions")
    ap.add_argument("--base", default="http://127.0.0.1:8791")
    ap.add_argument("--window-start", type=int, default=None,
                    help="epoch seconds, inclusive (default: now-900)")
    ap.add_argument("--window-end", type=int, default=None,
                    help="epoch seconds, exclusive (default: now-2)")
    args = ap.parse_args()

    end = args.window_end if args.window_end is not None else int(time.time()) - 2
    start = args.window_start if args.window_start is not None else end - DEFAULT_WINDOW_S
    if start >= end:
        log("FATAL: --window-start must be < --window-end")
        return 1

    cli = Client(args.base)
    try:
        cli.login()
    except Exception as e:
        log(f"FATAL: {e}")
        return 1

    log(f"[verify] base={cli.base} window=[{start},{end}) ({end - start}s)")
    cases = []
    for name, fn in [
        ("1 observability (series/archived_tags)", lambda c: check_observability(cli, c)),
        ("2 conservation (sent vs accounted)", lambda c: check_conservation(cli, c)),
        ("3 raw query B100DEV0000/t000", lambda c: check_raw(cli, c, start, end)),
        ("4 agg avg interval=60", lambda c: check_agg_avg(cli, c, start, end)),
        ("5 agg count/last structure", lambda c: check_agg_count_last(cli, c, start, end)),
        ("6 negative paths (400/empty)", lambda c: check_negative_paths(cli, c, start, end)),
        ("7 whitelist skip (chk_skip empty)", lambda c: check_whitelist_skip(cli, c, start, end)),
        ("8 deadband path (chk_dead <=1pt)", lambda c: check_deadband(cli, c, start, end)),
        ("9 limit=5 takes oldest n", lambda c: check_limit(cli, c, start, end)),
        ("10 disk bytes/point estimate", lambda c: check_disk(cli, c)),
    ]:
        case = Case(name)
        cases.append(case)
        try:
            fn(case)
        except requests.RequestException as e:
            case.done(FAIL, f"request error: {e.__class__.__name__}: {e}")
        except (ValueError, KeyError) as e:
            case.done(FAIL, f"response parse error: {e!r}")

    n_pass = sum(1 for c in cases if c.status == PASS)
    n_warn = sum(1 for c in cases if c.status == WARN)
    n_fail = sum(1 for c in cases if c.status == FAIL)
    log("=" * 78)
    log(f"{'CASE':<40} {'STATUS':<7} EVIDENCE")
    for c in cases:
        log(f"{c.name:<40} {c.status:<7} {c.evidence}")
    log("=" * 78)
    log(f"TOTAL: {len(cases)}  PASS={n_pass}  WARN={n_warn}  FAIL={n_fail}")
    return 1 if n_fail else 0


if __name__ == "__main__":
    sys.exit(main())
