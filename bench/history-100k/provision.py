#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Provision the history 100k-tag benchmark topology on a RUNNING rs-iot service.

Topology (see bench/history-100k/README context / master plan Phase 4):
  - model B100: 100 tags t000..t099, data_type f32, props {history: true}
  - model BCHK: 2 tags (chk_dead: history+deadband 1000.0, chk_skip: history=false)
  - devices: B100DEV0000..B100DEV0999 (model B100) + CHKDEV01 (model BCHK)
  - instances: bsim00..bsim99 (10 devices each, dataSim period=1s) + bchk (CHKDEV01)
  - expected series = 1000*100 + 1 = 100001

REST contracts (verified against src/api/*.rs):
  - POST /api/v1/user/login      body {"user_id","pass"} -> 200 {"status":"ok","token":...}
  - POST /api/v1/devmodel/set    body = bare DeviceModel JSON -> 200 {"status":"ok","model_id"}
         (create-only: duplicate model_id -> 409 plain text)
  - POST /api/v1/devreg/set      body = bare DeviceEntry JSON -> 200 {"status":"ok","devid"}
  - POST /api/v1/inst/set        body {"inst_id","object":<InstConfig>} ->
         ALWAYS HTTP 200 JSON: {"status":"ok"} or {"status":"error","msg":...}
         (business failure still returns 200 -> we must check body status!)
  - POST /api/v1/startinst       body {"inst_id"} -> 200 {"status":"ok"}; 409 = already running
         (inst/set does NOT start an instance; auto_start only applies at process boot,
          so we must start each instance explicitly for a running service)
  - GET  /api/v1/devices         -> JSON object keyed by devid (no pagination), JWT required

Wire-form note: app_type is "south" (lowercase, serde rename_all) and
host_kind is "builtin" (lowercase) - NOT "South"/"Builtin".

Usage:
  python provision.py [--base http://127.0.0.1:8791]

Exit code: 0 on success, 1 on any HTTP/business failure.
"""

import argparse
import sys
import time

import requests

# Connection-level retries (connection refused / timeout): 3 retries, 2s apart.
# 4xx/5xx responses are NOT retried (they are definitive server answers).
CONN_RETRIES = 3
CONN_RETRY_DELAY_S = 2.0

B100_TAG_COUNT = 100
B100_DEV_COUNT = 1000
SIM_INST_COUNT = 100  # bsim00..bsim99, 10 devices each
PROGRESS_EVERY = 100


def log(msg: str) -> None:
    print(msg, flush=True)


def request_with_retry(session: requests.Session, method: str, url: str, **kwargs):
    """Send a request, retrying only on connection-level failures.

    Returns the final requests.Response. Raises requests.RequestException if
    all connection attempts fail. HTTP error statuses are returned as-is
    (never retried).
    """
    last_exc = None
    for attempt in range(1 + CONN_RETRIES):
        try:
            return session.request(method, url, timeout=30, **kwargs)
        except (requests.exceptions.ConnectionError, requests.exceptions.Timeout) as e:
            last_exc = e
            if attempt < CONN_RETRIES:
                log(f"[retry] connection failure ({e.__class__.__name__}) on {method} {url} "
                    f"- retry {attempt + 1}/{CONN_RETRIES} in {CONN_RETRY_DELAY_S:.0f}s")
                time.sleep(CONN_RETRY_DELAY_S)
    raise last_exc


def die(context: str, resp: requests.Response = None, body: str = "") -> None:
    if resp is not None:
        log(f"FATAL: {context}: HTTP {resp.status_code} body={body or resp.text[:500]}")
    else:
        log(f"FATAL: {context}: {body}")
    sys.exit(1)


def login(session: requests.Session, base: str) -> str:
    resp = request_with_retry(
        session, "POST", base + "/api/v1/user/login",
        json={"user_id": "admin", "pass": "admin"},
    )
    if resp.status_code != 200:
        die("login failed", resp)
    data = resp.json()
    token = data.get("token")
    if data.get("status") != "ok" or not token:
        die("login business failure", resp, resp.text)
    return token


def expect_ok(session: requests.Session, method: str, url: str, what: str,
              token: str, payload: dict) -> None:
    """Assert HTTP 200 AND JSON body status == "ok" (inst/set returns 200 on
    business failure too, so the body must always be checked)."""
    resp = request_with_retry(session, method, url,
                              json=payload,
                              headers={"Authorization": f"Bearer {token}"})
    if resp.status_code != 200:
        die(f"{what} failed", resp)
    try:
        data = resp.json()
    except ValueError:
        die(f"{what} returned non-JSON body", resp)
    if data.get("status") != "ok":
        die(f"{what} business failure (HTTP 200 but status != ok)", resp)


def start_instance(session: requests.Session, base: str, inst_id: str, token: str) -> str:
    """Start an instance. Returns "started" or "already-running" (409 tolerated)."""
    resp = request_with_retry(session, "POST", base + "/api/v1/startinst",
                              json={"inst_id": inst_id},
                              headers={"Authorization": f"Bearer {token}"})
    if resp.status_code == 409:
        return "already-running"
    if resp.status_code != 200:
        die(f"startinst {inst_id} failed", resp)
    if resp.json().get("status") != "ok":
        die(f"startinst {inst_id} business failure", resp)
    return "started"


def b100_model() -> dict:
    tags = [
        {"name": f"t{i:03d}", "data_type": "f32", "props": {"history": True}}
        for i in range(B100_TAG_COUNT)
    ]
    return {
        "model_id": "B100",
        "vendor": "rs-iot-bench",
        "model_no": "B100",
        "category": "simulator",
        "desc": "100-tag sine simulator model for history 100k bench",
        "params": [],
        "tags": tags,
    }


def bchk_model() -> dict:
    return {
        "model_id": "BCHK",
        "vendor": "rs-iot-bench",
        "model_no": "BCHK",
        "category": "simulator",
        "desc": "deadband / whitelist check model for history bench",
        "params": [],
        "tags": [
            # sine 50+-40: adjacent-second delta is far below 1000 -> every
            # sample after the first is deadband-suppressed.
            {"name": "chk_dead", "data_type": "f32",
             "props": {"history": True, "deadband": 1000.0}},
            # history=false -> blocked by the archive whitelist (skipped counter).
            {"name": "chk_skip", "data_type": "f32",
             "props": {"history": False}},
        ],
    }


def device_entry(devid: str, model: str) -> dict:
    return {"devid": devid, "model": model, "params": {}, "overrides": {}}


def sim_instance(nn: int) -> tuple:
    """bsimNN binds devices B100DEV{NN*10}..B100DEV{NN*10+9}."""
    inst_id = f"bsim{nn:02d}"
    devices = [
        {"devid": f"B100DEV{d:04d}", "bind": {}}
        for d in range(nn * 10, nn * 10 + 10)
    ]
    cfg = {
        "inst_name": f"bench sim {nn:02d}",
        "app_code": "dataSim",
        "app_type": "south",
        "auto_start": True,
        "app_config": {"period": 1},
        "devices": devices,
        "host_kind": "builtin",
    }
    return inst_id, {"inst_id": inst_id, "object": cfg}


def chk_instance() -> tuple:
    inst_id = "bchk"
    cfg = {
        "inst_name": "bench check",
        "app_code": "dataSim",
        "app_type": "south",
        "auto_start": True,
        "app_config": {"period": 1},
        "devices": [{"devid": "CHKDEV01", "bind": {}}],
        "host_kind": "builtin",
    }
    return inst_id, {"inst_id": inst_id, "object": cfg}


def main() -> int:
    ap = argparse.ArgumentParser(description="Provision history 100k bench topology")
    ap.add_argument("--base", default="http://127.0.0.1:8791",
                    help="rs-iot REST base URL (default http://127.0.0.1:8791)")
    args = ap.parse_args()
    base = args.base.rstrip("/")

    session = requests.Session()
    t0 = time.time()

    log(f"[provision] base={base}")
    log("[provision] login as admin ...")
    token = login(session, base)
    log("[provision] login ok")

    # 1) models (create-only endpoints; a 409 here means a stale model from a
    #    previous run - delete it via /api/v1/devmodel/delete or wipe data dirs).
    log("[provision] creating models B100 / BCHK ...")
    expect_ok(session, "POST", base + "/api/v1/devmodel/set",
              "set model B100", token, b100_model())
    expect_ok(session, "POST", base + "/api/v1/devmodel/set",
              "set model BCHK", token, bchk_model())
    log("[provision] models created (2)")

    # 2) devices: 1000 x B100 + 1 x BCHK
    log(f"[provision] registering {B100_DEV_COUNT + 1} devices ...")
    for i in range(B100_DEV_COUNT):
        devid = f"B100DEV{i:04d}"
        expect_ok(session, "POST", base + "/api/v1/devreg/set",
                  f"set device {devid}", token, device_entry(devid, "B100"))
        if (i + 1) % PROGRESS_EVERY == 0:
            log(f"[provision] devices {i + 1}/{B100_DEV_COUNT}")
    expect_ok(session, "POST", base + "/api/v1/devreg/set",
              "set device CHKDEV01", token, device_entry("CHKDEV01", "BCHK"))
    log(f"[provision] devices created ({B100_DEV_COUNT + 1})")

    # 3) instances: bsim00..bsim99 + bchk, then start each of them
    #    (inst/set only stores config; start is explicit for a running service).
    total_inst = SIM_INST_COUNT + 1
    log(f"[provision] creating + starting {total_inst} instances ...")
    for nn in range(SIM_INST_COUNT):
        inst_id, payload = sim_instance(nn)
        expect_ok(session, "POST", base + "/api/v1/inst/set",
                  f"set instance {inst_id}", token, payload)
        state = start_instance(session, base, inst_id, token)
        if (nn + 1) % PROGRESS_EVERY == 0:
            log(f"[provision] instances {nn + 1}/{SIM_INST_COUNT} (last: {inst_id} {state})")
    inst_id, payload = chk_instance()
    expect_ok(session, "POST", base + "/api/v1/inst/set",
              f"set instance {inst_id}", token, payload)
    start_instance(session, base, inst_id, token)
    log(f"[provision] instances created + started ({total_inst})")

    # 4) final sanity: device count via GET /api/v1/devices (object keyed by devid).
    resp = request_with_retry(session, "GET", base + "/api/v1/devices",
                              headers={"Authorization": f"Bearer {token}"})
    if resp.status_code != 200:
        die("GET /api/v1/devices failed", resp)
    try:
        devices = resp.json()
        n_devices = len(devices) if isinstance(devices, dict) else -1
    except ValueError:
        die("GET /api/v1/devices returned non-JSON", resp)
    if n_devices < B100_DEV_COUNT + 1:
        die("device count check failed", resp,
            f"expected >= {B100_DEV_COUNT + 1}, got {n_devices}")

    elapsed = time.time() - t0
    log("=" * 62)
    log("[provision] SUMMARY")
    log(f"  models            : 2 (B100 100 tags, BCHK 2 tags)")
    log(f"  devices           : {n_devices} (expected >= {B100_DEV_COUNT + 1})")
    log(f"  instances         : {total_inst} (bsim00..bsim{SIM_INST_COUNT - 1:02d} + bchk)")
    log(f"  expected series   : {B100_DEV_COUNT * B100_TAG_COUNT + 1} "
        f"(1000*100 history tags + chk_dead)")
    log(f"  elapsed           : {elapsed:.1f}s")
    log("[provision] DONE - let it run, then use monitor.py / verify.py")
    return 0


if __name__ == "__main__":
    sys.exit(main())
