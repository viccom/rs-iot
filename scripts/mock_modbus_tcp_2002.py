"""Mock TCP Modbus server for B 层手动 E2E 测试。

127.0.0.1:2002, MBAP 头。
- unit 1 F=04 input → i16 [1010, 1020, 1030, 1040]
- unit 2 F=03 holding → f32 [12.5, 25.0, 37.5, 50.0]
"""
import socket
import struct
import threading


def handle_conn(sock, addr):
    print(f"[mock] conn from {addr}")
    try:
        while True:
            hdr = recv_exact(sock, 7)
            if not hdr:
                break
            tid = struct.unpack(">H", hdr[0:2])[0]
            uid = hdr[6]
            pdu_len = struct.unpack(">H", hdr[4:6])[0] - 1
            pdu = recv_exact(sock, pdu_len) if pdu_len > 0 else b""
            if not pdu:
                continue
            fc = pdu[0]
            qty = struct.unpack(">H", pdu[3:5])[0] if len(pdu) >= 5 else 0
            data = b""
            if (uid, fc) == (1, 4):
                regs = [1010, 1020, 1030, 1040]
                data = b"".join(struct.pack(">h", r) for r in regs[:qty])
            elif (uid, fc) == (2, 3):
                regs = [12.5, 25.0, 37.5, 50.0]
                data = b"".join(struct.pack(">f", r) for r in regs[:qty])
            else:
                continue
            resp = struct.pack(">H", tid) + struct.pack(">H", 0)
            resp += struct.pack(">H", 3 + len(data)) + bytes([uid, fc, len(data)]) + data
            sock.sendall(resp)
    except Exception as e:
        print(f"[mock] conn {addr} err: {e}")
    finally:
        sock.close()
        print(f"[mock] conn {addr} closed")


def recv_exact(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            return None
        buf += chunk
    return buf


def main():
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", 2002))
    srv.listen(8)
    print("[mock] listening on 127.0.0.1:2002")
    while True:
        conn, addr = srv.accept()
        threading.Thread(target=handle_conn, args=(conn, addr), daemon=True).start()


if __name__ == "__main__":
    main()
