#!/usr/bin/env python3
import errno
import os
import pty
import select
import sys
import time


def main() -> int:
    if len(sys.argv) < 2:
        raise SystemExit("usage: pty_runner.py command [args ...]")
    pid, master = pty.fork()
    if pid == 0:
        os.execvpe(sys.argv[1], sys.argv[1:], os.environ)

    keys = os.environ.get("SKILLSYNC_PTY_KEYS", "q")
    delay = float(os.environ.get("SKILLSYNC_PTY_DELAY", "0.3"))
    schedule = [(delay, b"\r" if "r" in keys else b"")]
    if "q" in keys:
        schedule.append((delay + 0.05, b"q"))
    start = time.monotonic()
    next_key = 0
    status = None
    while True:
        now = time.monotonic() - start
        while next_key < len(schedule) and now >= schedule[next_key][0]:
            if schedule[next_key][1]:
                os.write(master, schedule[next_key][1])
            next_key += 1
        readable, _, _ = select.select([master], [], [], 0.05)
        if readable:
            try:
                data = os.read(master, 4096)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                data = b""
            if data:
                sys.stdout.buffer.write(data)
                sys.stdout.buffer.flush()
        waited, child_status = os.waitpid(pid, os.WNOHANG)
        if waited:
            status = child_status
            break
    try:
        os.set_blocking(master, False)
        idle_deadline = time.monotonic() + 0.5
        while time.monotonic() < idle_deadline:
            readable, _, _ = select.select([master], [], [], 0.05)
            if not readable:
                continue
            try:
                data = os.read(master, 4096)
            except OSError as error:
                if error.errno in (errno.EAGAIN, errno.EWOULDBLOCK, errno.EIO):
                    break
                raise
            if not data:
                break
            sys.stdout.buffer.write(data)
            sys.stdout.buffer.flush()
            idle_deadline = time.monotonic() + 0.5
    finally:
        os.close(master)
    return os.waitstatus_to_exitcode(status)


if __name__ == "__main__":
    raise SystemExit(main())
