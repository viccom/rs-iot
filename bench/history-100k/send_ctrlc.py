#!/usr/bin/env python3
"""Send a real CTRL_C_EVENT to the console group of a target PID (Windows).

The graceful-shutdown path in rs-iot (lib.rs console_shutdown) listens for the
console ctrl event; a tmux/pty `send-keys C-c` never reaches it, so we attach
to the target's console and raise the event programmatically (windows-kill
technique: FreeConsole -> AttachConsole(pid) -> ignore ctrl in self ->
GenerateConsoleCtrlEvent to the group).

Usage: python send_ctrlc.py <pid>
"""
import ctypes
import sys
import time

CTRL_C_EVENT = 0
CTRL_BREAK_EVENT = 1


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__)
        return 2
    pid = int(sys.argv[1])
    k = ctypes.windll.kernel32
    k.FreeConsole()
    if not k.AttachConsole(pid):
        err = ctypes.get_last_error()
        print(f"AttachConsole({pid}) failed: {err}")
        return 1
    k.SetConsoleCtrlHandler(None, True)
    if not k.GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, 0):
        err = ctypes.get_last_error()
        print(f"GenerateConsoleCtrlEvent failed: {err}")
        return 1
    print(f"CTRL_BREAK_EVENT sent to console group of pid {pid}")
    # stay attached briefly so the event is fully delivered
    time.sleep(3)
    return 0


if __name__ == "__main__":
    sys.exit(main())
