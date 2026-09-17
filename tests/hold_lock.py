#!/usr/bin/env python3
import fcntl
import pathlib
import sys
import time

lock_path, marker_path = sys.argv[1:3]
with open(lock_path, "a+") as lock:
    fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
    pathlib.Path(marker_path).touch()
    time.sleep(30)
