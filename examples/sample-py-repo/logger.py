"""File-backed log with size-based rotation. Exercises fs + time + env effects."""

import os
import time


def write_log(path: str, msg: str) -> None:
    """Append ``msg`` with a unix timestamp to ``path``."""
    ts = time.time()
    f = open(path, "a")
    f.write(f"{ts} {msg}\n")
    f.close()


def read_log(path: str) -> list[str]:
    """Return all lines from the log at ``path``."""
    f = open(path)
    lines = f.readlines()
    f.close()
    return lines


def rotate_if_big(path: str, max_bytes: int = 0) -> bool:
    """Rotate ``path`` to ``path + ".1"`` if it exceeds ``max_bytes``.

    If ``max_bytes`` is 0, fall back to the ``LOG_MAX_BYTES`` env var.
    """
    if max_bytes <= 0:
        max_bytes = int(os.environ.get("LOG_MAX_BYTES", "1048576"))
    size = os.path.getsize(path)
    if size > max_bytes:
        os.rename(path, path + ".1")
        return True
    return False
