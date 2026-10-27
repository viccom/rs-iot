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
    # v1.25.x:提交后同步准入检查——会话 rotating/closed/finishing 时静默丢弃并返
    # 409 "not admitted"(serve.go submit)。轮转通常秒级完成,短退避重试即可穿过。
    tries = 0
    while True:
        tries += 1
        s, _, txt = post("/submit", {"input": prompt})
        print(f"[submit] input → {s}: {txt[:250]}")
        if s in (200, 202):
            # 200/202 可能无 body,task 从 SSE 流首事件里取
            return "", {}
        if s == 409 and "not admitted" in txt and tries < 6:
            print(f"[submit-retry] 会话轮转中,2s 后重试 {tries}/5 ...")
            time.sleep(2)
            continue
        return None, None


def sse_lines(resp, deadline):
    """逐行迭代 SSE/NDJSON 流。

    必须逐行读:http.client 的 r.read(N) 会阻塞到凑满 N 字节或 EOF——空闲流上
    serve 只发稀疏心跳,凑不满 N 字节且心跳阻止 socket 超时 → 永久挂死
    (v1.25.2/1.25.3 实测两轮挂死根因)。readline 随每条心跳/事件即时返回。"""
    for raw in resp:
        if time.time() > deadline:
            return
        line = raw.decode("utf-8", "replace").strip()
        if not line or line.startswith(":"):
            continue
        if line.startswith("data:"):
            line = line[5:].strip()
        yield line


def stream_events(task_id, max_wait=60):
    """拉 /events SSE 流,打印每条事件 + 收集工具列表与 dispatch。
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
        with opener.open(req, timeout=30) as r:
            for line in sse_lines(r, t0 + max_wait):
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
                    tool = ev.get("tool")
                    dispatched.append(tool)
                    # v1.25.3:capability proxy 分发,真实工具名在 resolvedName
                    #(如 mcp__rsiot__rsiot_read_tags)——只看 name 会全是 use_capability。
                    if isinstance(tool, dict) and tool.get("resolvedName"):
                        seen_tools.add(tool["resolvedName"])
                # turn 结束自然收尾(不硬等 max_wait)
                if kind == "turn_done":
                    break
                # approval
                if "approval" in ev or kind in ("approval_request", "permission_request"):
                    approval_payload = ev
                    return seen_tools, dispatched, approval_payload, True
    except Exception as e:
        print(f"[stream] end: {e}")
    return seen_tools, dispatched, approval_payload, False


def warmup_mcp_connect(server_name="rsiot", max_wait=20):
    """启动预热:POST /submit 发管理动词 `/mcp connect <name>`,零 LLM token 触发 MCP 握手。

    v1.25.3 实测两个坑(源码 internal/serve/serve.go submit):
    1. 管理动词不置 ctrl.Running → submit 后置准入检查恒误报 409 "not admitted"
       (动词实际已执行)——成败只能靠 SSE Notice 判定,状态码仅排除 400/401/500。
    2. SSE 无 replay:必须**先挂 /events 流再提交**,后挂流会错过 Notice。
    watcher 线程用独立 cookiejar(urllib opener 非线程安全,不与主流程共用)。
    """
    import threading

    result = {}

    def watch():
        cj2 = http.cookiejar.CookieJar()
        op2 = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(cj2))
        try:
            req = urllib.request.Request(
                f"{SERVE}/auth/token",
                data=json.dumps({"token": TOKEN}).encode(),
                headers={"Content-Type": "application/json"},
            )
            op2.open(req, timeout=10)
            with op2.open(f"{SERVE}/events", timeout=30) as r:
                for raw in r:
                    line = raw.decode("utf-8", "replace").strip()
                    if not line or line.startswith(":"):
                        continue
                    if line.startswith("data:"):
                        line = line[5:].strip()
                    try:
                        ev = json.loads(line)
                    except Exception:
                        continue
                    text = (ev.get("text") or "") if isinstance(ev, dict) else ""
                    if f"connected {server_name}" in text:
                        result["ok"] = text
                        return
        except Exception:
            pass

    t = threading.Thread(target=watch, daemon=True)
    t.start()
    time.sleep(0.8)  # 等流挂稳(SSE 无 replay,错过即失)
    s, _, txt = post("/submit", {"input": f"/mcp connect {server_name}"})
    if s not in (200, 202, 409):
        print(f"[warmup] FATAL: submit 返 {s}(非 200/202/409,握手未发起); body={txt[:200]}")
        raise SystemExit(1)
    print(f"[warmup] submit → {s} (409=管理动词误报态,实际执行与否靠 Notice); body={txt[:120]}")
    t.join(timeout=max_wait)
    if result.get("ok"):
        print(f"[warmup] ✓ Notice: {result['ok'][:200]}")
        return True
    print("[warmup] 未捕获确认 Notice(握手可能完成但 Notice 错过)")
    return False


def main():
    prompt = sys.argv[1] if len(sys.argv) > 1 else "用 rsiot_read_tags 工具读取 devA 的 temp。不要用 bash,不要用 redis-cli。"
    print(f"\n========== 任务: {prompt}\n")
    auth_bootstrap()
    # 开发调试:全局 yolo,所有工具调用自动放行(POST /tool-approval-mode)。
    # serve 无启动 flag 可 yolo,这是运行时唯一干净路径(源码 serve.go:1082)。
    s, _, _ = post("/tool-approval-mode", {"mode": "yolo"})
    print(f"[yolo] /tool-approval-mode → {s} (204=已切全局自动放行)")
    # 启动预热:零 token 触发 MCP plugin spawn+握手,当前 session catalog 立即有 rsiot 工具
    # + schema 缓存落盘(下次冷启动根治)。须在 /new 之前(session 空闲时)。
    # False = 未确认(可能 busy 未执行)——打醒目警告,本次结果不可信(但继续跑,
    # 因为预热只是提高首轮 MCP 命中率,不是硬依赖)。
    warmup_ok = warmup_mcp_connect("rsiot", max_wait=20)
    if not warmup_ok:
        print("[WARN] 预热未确认——首轮工具选择结果不可信(可能走 bash 而非 MCP)")
    # 冷启动:开新 session,避免上一轮上下文干扰工具首选项观测。
    s2, _, txt2 = post("/new", {})
    if s2 not in (200, 202, 204):
        # busy 等异常时 /new 失败——本次观测受旧上下文污染,如实警告(审查 Minor)。
        print(f"[WARN] /new → {s2}(非 2xx,新 session 未开,结果可能受旧上下文污染); body={txt2[:100]}")
    else:
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
