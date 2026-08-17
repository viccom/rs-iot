#!/usr/bin/env python3
"""
rs-iot 外部插件 Python 样例（sidecarClient bridge 协议 v1）。

演示完整闭环：连接 + 握手 + 配置接收 + 模拟采集(DataUp) +
写命令(Command/Ack) + 心跳 + 断连缓存补传 + 指数退避重连。

用法：
    python main.py --inst ext1 --token <token> --host 127.0.0.1 --port 8781

要求 Python 3.10+，无第三方依赖（仅标准库 asyncio + struct + json）。
协议详见 ../../../docs/external-plugin-protocol.md。
"""
import argparse
import asyncio
import json
import random
import struct
import time

# 帧类型（与 docs/external-plugin-protocol.md 一致）
T_HELLO = 0x01
T_HELLO_ACK = 0x02
T_CONFIG_PUSH = 0x03
T_DATA_UP = 0x04
T_COMMAND = 0x05
T_COMMAND_ACK = 0x06
T_HEARTBEAT = 0x07

PROTO_VER = "1"


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
        self.devices: list[dict] = []  # ConfigPush 的 devices
        self.heartbeat_secs = 15
        # 断连缓存：未上报的 (devid, samples)，重连后按 ts 顺序补传。
        self.buffer: list[tuple[str, list[dict]]] = []
        self.buffer_max = 10000

    def sample(self) -> tuple[str, list[dict]]:
        """模拟采集：对 ConfigPush 里每个 device 的 child 产一个随机值。"""
        ts = int(time.time())
        if not self.devices:
            return "", []
        dev = self.devices[0]
        devid = dev["value"]
        samples = [
            {"tag": ch["value"], "value": round(random.uniform(0, 100), 2), "ts": ts}
            for ch in dev.get("children", [])
        ]
        return devid, samples

    async def run(self) -> None:
        """外层重连循环：指数退避。"""
        backoff = 1
        while True:
            try:
                await self.session()
            except (OSError, asyncio.IncompleteReadError, RuntimeError) as e:
                print(f"[{self.inst}] disconnected: {e}; reconnect in {backoff}s")
                await asyncio.sleep(backoff)
                backoff = min(backoff * 2, 30)

    async def session(self) -> None:
        """一次连接的完整生命周期：握手 → 配置 → 并发采集/命令/心跳。"""
        print(f"[{self.inst}] connecting {self.host}:{self.port}")
        reader, writer = await asyncio.open_connection(self.host, self.port)

        # 1. 握手
        send_frame(writer, T_HELLO, {"inst_id": self.inst, "token": self.token, "ver": PROTO_VER})
        await writer.drain()
        typ, body = await read_frame(reader)
        if typ != T_HELLO_ACK or not body.get("ok"):
            raise RuntimeError(f"handshake rejected: {body}")
        self.heartbeat_secs = body.get("heartbeat_secs", 15)
        print(f"[{self.inst}] handshake ok, heartbeat={self.heartbeat_secs}s")

        # 2. 收 ConfigPush
        typ, body = await read_frame(reader)
        if typ != T_CONFIG_PUSH:
            raise RuntimeError(f"expected ConfigPush, got type={typ}")
        self.devices = body.get("devices", [])
        print(f"[{self.inst}] config received: {len(self.devices)} device(s)")

        # 3. 并发：采集+上报 / 收命令 / 心跳。任一异常 → 本次 session 结束 → 外层重连。
        await asyncio.gather(
            self.collect_loop(writer),
            self.command_loop(reader, writer),
            self.heartbeat_loop(writer),
        )

    async def collect_loop(self, writer: asyncio.StreamWriter) -> None:
        """周期采集 → DataUp；断连时入缓存，重连后先补传缓存。"""
        period = 2
        while True:
            await asyncio.sleep(period)
            devid, samples = self.sample()
            if not devid:
                continue
            try:
                # 先补传缓存（按 ts 顺序）
                while self.buffer:
                    buf_devid, buf_samples = self.buffer.pop(0)
                    send_frame(writer, T_DATA_UP, {"devid": buf_devid, "samples": buf_samples})
                # 再发当前
                send_frame(writer, T_DATA_UP, {"devid": devid, "samples": samples})
                await writer.drain()
            except OSError:
                self._cache(devid, samples)
                raise

    def _cache(self, devid: str, samples: list[dict]) -> None:
        self.buffer.append((devid, samples))
        if len(self.buffer) > self.buffer_max:
            self.buffer.pop(0)  # 丢最旧

    async def command_loop(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        """收 Command → 执行（此处仅打印）→ 回 Ack。"""
        while True:
            typ, body = await read_frame(reader)
            if typ == T_COMMAND:
                print(
                    f"[{self.inst}] CMD #{body.get('cmd_id')} "
                    f"{body.get('cmd')}: {body.get('data')}"
                )
                # TODO: 实际执行写设备；此处仅回成功。
                send_frame(writer, T_COMMAND_ACK, {"cmd_id": body["cmd_id"], "ok": True})
                await writer.drain()

    async def heartbeat_loop(self, writer: asyncio.StreamWriter) -> None:
        while True:
            await asyncio.sleep(max(1, self.heartbeat_secs // 2))
            send_frame(writer, T_HEARTBEAT, {"ts": int(time.time())})
            await writer.drain()


def main() -> None:
    ap = argparse.ArgumentParser(description="rs-iot external plugin (Python sidecarClient sample)")
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
