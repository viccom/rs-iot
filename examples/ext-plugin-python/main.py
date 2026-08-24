#!/usr/bin/env python3
"""
rs-iot 外部插件 Python 样例（sidecarClient bridge 协议 v2，M-3）。

与 v1 的差异（协议文档 docs/reference/external-plugin-protocol.md）：
- HELLO 携带 ver="2"（v1 被 bridge 拒连，带升级提示）；
- ConfigPush v2 载荷：{inst_id, v:2, devices:[{devid, model, tags:[{name,type,
  addr,unit,rw}]}]}——tags 为设备模型点表（已解析地址），外部按名采集/上报；
- DataUp 校验闸：devid/tag 必须 ∈ 模型点表（未建模 sample 被 bridge 拒收）。

用法：
    python main.py --inst ext1 --token <token> --host 127.0.0.1 --port 8781

要求 Python 3.10+，无第三方依赖（仅标准库 asyncio + struct + json）。
"""
import argparse
import asyncio
import json
import random
import struct
import time

# 帧类型（与 bridge 侧一致）
T_HELLO = 0x01
T_HELLO_ACK = 0x02
T_CONFIG_PUSH = 0x03
T_DATA_UP = 0x04
T_COMMAND = 0x05
T_COMMAND_ACK = 0x06
T_HEARTBEAT = 0x07

PROTO_VER = "2"


def send_frame(writer: asyncio.StreamWriter, typ: int, body: dict | None) -> None:
    """组帧 [4B BE len][1B type][JSON body] 并写入（不 drain）。"""
    payload = bytes([typ])
    if body is not None:
        payload += json.dumps(body, ensure_ascii=False).encode("utf-8")
    writer.write(struct.pack(">I", len(payload)) + payload)


async def read_frame(reader: asyncio.StreamReader) -> tuple[int, dict]:
    hdr = await reader.readexactly(4)
    (length,) = struct.unpack(">I", hdr)
    payload = await reader.readexactly(length)
    typ = payload[0]
    body = json.loads(payload[1:].decode("utf-8")) if len(payload) > 1 else {}
    return typ, body


class ExternalPlugin:
    """外部插件：纯运行时，配置/数据在 rs-iot，本地仅缓存数据待补传。"""

    def __init__(self, inst: str, token: str, host: str, port: int):
        self.inst = inst
        self.token = token
        self.host = host
        self.port = port
        self.devices: list[dict] = []  # ConfigPush v2 的 devices（含 tags 模型点表）
        self.heartbeat_secs = 15
        self.buffer: list[tuple[str, list[dict]]] = []
        self.buffer_max = 10000

    def sample_all(self) -> list[tuple[str, list[dict]]]:
        """对每个 device 的 tags 产一个变化值（v2：按模型点表名采集）。"""
        ts = int(time.time())
        out = []
        for i, dev in enumerate(self.devices):
            samples = [
                {"tag": t["name"], "value": round(random.uniform(0, 100) + i, 2), "ts": ts}
                for t in dev.get("tags", [])
            ]
            out.append((dev["devid"], samples))
        return out

    async def run(self) -> None:
        """外层重连循环：指数退避。"""
        backoff = 1
        while True:
            try:
                await self.session()
            except (OSError, asyncio.IncompleteReadError, RuntimeError) as e:
                print(f"[{self.inst}] disconnected: {e}; reconnect in {backoff}s", flush=True)
                await asyncio.sleep(backoff)
                backoff = min(backoff * 2, 30)

    async def session(self) -> None:
        """一次连接的完整生命周期：握手 → 配置 → 并发采集/命令/心跳。"""
        print(f"[{self.inst}] connecting {self.host}:{self.port}", flush=True)
        reader, writer = await asyncio.open_connection(self.host, self.port)

        # 1. 握手（v2：ver 字段必须为 "2"，否则 bridge 拒连）
        send_frame(writer, T_HELLO, {"inst_id": self.inst, "token": self.token, "ver": PROTO_VER})
        await writer.drain()
        typ, body = await read_frame(reader)
        if typ != T_HELLO_ACK or not body.get("ok"):
            raise RuntimeError(f"handshake rejected: {body}")
        self.heartbeat_secs = body.get("heartbeat_secs", 15)
        print(f"[{self.inst}] handshake ok, heartbeat={self.heartbeat_secs}s", flush=True)

        # 2. 收 ConfigPush v2（devices[].tags 为模型点表）
        typ, body = await read_frame(reader)
        if typ != T_CONFIG_PUSH:
            raise RuntimeError(f"expected ConfigPush, got type={typ}")
        self.devices = body.get("devices", [])
        total_tags = sum(len(d.get("tags", [])) for d in self.devices)
        print(
            f"[{self.inst}] config v2: {len(self.devices)} device(s), {total_tags} tag(s)",
            flush=True,
        )

        # 3. 并发：采集+上报 / 收命令 / 心跳。任一异常 → 本次 session 结束 → 外层重连。
        await asyncio.gather(
            self.collect_loop(writer),
            self.command_loop(reader, writer),
            self.heartbeat_loop(writer),
        )

    async def collect_loop(self, writer: asyncio.StreamWriter) -> None:
        """周期采集 → 逐设备 DataUp；断连时入缓存，重连后先补传缓存。"""
        period = 2
        while True:
            await asyncio.sleep(period)
            try:
                while self.buffer:
                    buf_devid, buf_samples = self.buffer.pop(0)
                    send_frame(writer, T_DATA_UP, {"devid": buf_devid, "samples": buf_samples})
                for devid, samples in self.sample_all():
                    send_frame(writer, T_DATA_UP, {"devid": devid, "samples": samples})
                await writer.drain()
            except OSError:
                for devid, samples in self.sample_all():
                    self._cache(devid, samples)
                raise

    def _cache(self, devid: str, samples: list[dict]) -> None:
        self.buffer.append((devid, samples))
        if len(self.buffer) > self.buffer_max:
            self.buffer.pop(0)

    async def command_loop(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        """收 Command → 执行（此处仅打印）→ 回 Ack。"""
        while True:
            typ, body = await read_frame(reader)
            if typ == T_COMMAND:
                print(
                    f"[{self.inst}] CMD #{body.get('cmd_id')} {body.get('cmd')}: {body.get('data')}",
                    flush=True,
                )
                send_frame(writer, T_COMMAND_ACK, {"cmd_id": body["cmd_id"], "ok": True})
                await writer.drain()

    async def heartbeat_loop(self, writer: asyncio.StreamWriter) -> None:
        while True:
            await asyncio.sleep(max(1, self.heartbeat_secs // 2))
            send_frame(writer, T_HEARTBEAT, {"ts": int(time.time())})
            await writer.drain()


def main() -> None:
    ap = argparse.ArgumentParser(description="rs-iot external plugin (Python sidecarClient v2 sample)")
    ap.add_argument("--inst", required=True, help="instance id（rs-iot 中注册的 inst_id）")
    ap.add_argument("--token", required=True, help="plugin token（建实例时 rs-iot 返回一次）")
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=8781)
    args = ap.parse_args()

    plugin = ExternalPlugin(args.inst, args.token, args.host, args.port)
    try:
        asyncio.run(plugin.run())
    except KeyboardInterrupt:
        print("\nbye")


if __name__ == "__main__":
    main()
