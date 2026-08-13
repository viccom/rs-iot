#!/usr/bin/env python3
"""
模拟前端面板,走 reasonix serve 全流程端到端测试。
目的:拿到 agent 执行任务时的真实工具列表与决策,定位"为什么用 bash 而非 MCP"。

流程:带 cookie+token 鉴权 → /submit 提交任务 → /events SSE 流 → /approve 放行 → 观察工具调用。
不依赖 gateway,直连 serve(:8787),隔离最短路径。
"""
import json
import sys
import time
import urllib.request
import urllib.parse
import http.cookiejar

SERVE = "http://127.0.0.1:8787"
TOKEN = "test-serve-token-123"
# 先用 ?token= 触发 serve 种 cookie,cj 持久化后续请求自动带
cj = http.cookiejar.CookieJar()
opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(cj))


def get(path, headers=None):
    url = SERVE + path
    req = urllib.request.Request(url, headers=headers or {})
    try:
        with opener.open(req, timeout=30) as r:
            return r.status, dict(r.headers), r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read().decode("utf-8", "replace")


def post(path, body, headers=None):
    url = SERVE + path
    data = json.dumps(body).encode()
    h = {"Content-Type": "application/json"}
    if headers:
        h.update(headers)
    req = urllib.request.Request(url, data=data, headers=h, method="POST")
    try:
        with opener.open(req, timeout=30) as r:
            return r.status, dict(r.headers), r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read().decode("utf-8", "replace")


def auth_bootstrap():
    """POST /auth/token 换 cookie(serve Web UI 的真实鉴权契约),后续 opener 自动带。"""
    s, _, txt = post("/auth/token", {"token": TOKEN})
    # 退路:?token= 种 cookie
    if s != 200:
        s2, _, _ = get(f"/status?token={TOKEN}")
        print(f"[auth] /auth/token {s} → 退路 /status?token={s2}")
    cookies = [c.name for c in cj]
    print(f"[auth] bootstrap → {s}; cookies = {cookies}; body={txt[:100]}")
    return cookies


def submit(prompt):
    # serve Web UI 契约:fetch('/submit',{body: JSON.stringify({input: submitInput})})
    s, _, txt = post("/submit", {"input": prompt})
    print(f"[submit] input → {s}: {txt[:250]}")
    if s in (200, 202):
        # 200/202 可能无 body,task 从 SSE 流首事件里取
        return "", {}
    return None, None


def stream_events(task_id, max_wait=60):
    """拉 /events SSE 流,打印每条事件 + 收集 tool 列表与 dispatch。
    serve 202 不返 task_id,EventSource('/events') 走 cookie 关联 session(无 task_id 参数)。"""
    if task_id:
        qs = urllib.parse.urlencode({"task_id": task_id, "token": TOKEN})
        url = f"{SERVE}/events?{qs}"
    else:
        url = f"{SERVE}/events"
    req = urllib.request.Request(url)
    seen_tools = set()
    dispatched = []
    approval_payload = None
    t0 = time.time()
    try:
        with opener.open(req, timeout=max_wait) as r:
            buf = b""
            while time.time() - t0 < max_wait:
                chunk = r.read(1)
                if not chunk:
                    break
                buf += chunk
                while b"\n" in buf:
                    line, buf = buf.split(b"\n", 1)
                    line = line.decode("utf-8", "replace").strip()
                    if not line or line.startswith(":"):
                        continue
                    # serve 用裸 NDJSON(无 event: 头),每行一个 JSON 对象
                    if line.startswith("data:"):
                        line = line[5:].strip()
                    try:
                        ev = json.loads(line)
                    except Exception:
                        continue
                    # 归一化打印
                    kind = ev.get("type") or ev.get("kind") or ev.get("event") or "?"
                    print(f"[event] {kind}: {json.dumps(ev, ensure_ascii=False)[:300]}")
                    # 收集工具清单
                    if "tools" in ev and isinstance(ev["tools"], list):
                        for t in ev["tools"]:
                            n = t.get("name") if isinstance(t, dict) else str(t)
                            if n:
                                seen_tools.add(n)
                    # tool dispatch
                    if "tool" in ev and kind in ("tool_dispatch", "tool_use", "tool_call"):
                        dispatched.append(ev.get("tool"))
                    # approval
                    if "approval" in ev or kind in ("approval_request", "permission_request"):
                        approval_payload = ev
                        return seen_tools, dispatched, approval_payload, True
    except Exception as e:
        print(f"[stream] end: {e}")
    return seen_tools, dispatched, approval_payload, False


def main():
    prompt = sys.argv[1] if len(sys.argv) > 1 else "用 rsiot_read_tags 工具读取 devA 的 temp。不要用 bash,不要用 redis-cli。"
    print(f"\n========== 任务: {prompt}\n")
    auth_bootstrap()
    # 开发调试:全局 yolo,所有工具调用自动放行(POST /tool-approval-mode)。
    # serve 无启动 flag 可 yolo,这是运行时唯一干净路径(源码 serve.go:1082)。
    s, _, _ = post("/tool-approval-mode", {"mode": "yolo"})
    print(f"[yolo] /tool-approval-mode → {s} (204=已切全局自动放行)")
    # 冷启动:开新 session,避免上一轮上下文干扰工具首选项观测。
    s2, _, _ = post("/new", {})
    print(f"[new] /new → {s2} (开全新 session)")
    tid, _ = submit(prompt)
    if tid is None:
        print("[FATAL] submit 失败")
        return
    print(f"\n----- SSE events -----")
    tools, dispatched, approval, has_approval = stream_events(tid, max_wait=45)
    print(f"\n----- 汇总 -----")
    print(f"可见工具({len(tools)}): {sorted(tools)}")
    print(f"rsiot_* 工具: {sorted(t for t in tools if 'rsiot' in t.lower())}")
    print(f"dispatched: {dispatched}")
    print(f"approval: {has_approval} {json.dumps(approval, ensure_ascii=False)[:200] if approval else ''}")


if __name__ == "__main__":
    main()
