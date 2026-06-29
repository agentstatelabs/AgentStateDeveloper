"""ASD runtime effect tracer.

Usage:
    python3 tools/asd_tracer.py -- <command> [args...]
    python3 tools/asd_tracer.py --script path/to/script.py [args...]

The tracer installs a ``sys.settrace`` hook to maintain a stack of active
user-code fully-qualified names (qname), and monkeypatches a small set of
stdlib entry points so each side-effecting call is attributed to the
currently-executing user function.

At exit the tracer writes a JSON report to the path from ``ASD_TRACE_OUT``
(default ``.asd-trace.json`` in the cwd). The report shape is:

    {
      "command": [...],
      "started_at": "<iso>",
      "finished_at": "<iso>",
      "observations": [
        {"qname": "...", "observed_effects": [{"effect": "...", "note": "..."}], "call_count": N},
        ...
      ]
    }

Limitations (M3):
  * Single-process only. Child processes spawned via ``subprocess``/``os.system``
    are not traced; we only record that the parent invoked them.
  * Threads other than the main thread are not traced (``sys.settrace`` only
    installs on the current thread).
  * Monkeypatching coverage: ``open``, ``print``, ``logging`` module functions,
    any ``Logger`` method, ``time.sleep``, ``time.time``, ``time.monotonic``,
    ``subprocess`` / ``os.system`` / ``os.exec*`` / ``os.spawn*`` (proc.spawn),
    ``os.environ`` reads (env.read), ``urllib`` / ``http.client`` (io.net.out),
    ``random`` / ``secrets`` (random). Enough to demonstrate the pipeline.
"""

from __future__ import annotations

import builtins
import datetime as _datetime
import json
import logging
import os
import runpy
import sys
import sysconfig
import time
import traceback
from typing import Any


# ---------------------------------------------------------------------------
# Path filtering: only attribute effects to *user code*, not stdlib / venv.
# ---------------------------------------------------------------------------


def _normalized(p: str) -> str:
    try:
        return os.path.realpath(p)
    except OSError:
        return p


_STDLIB_PREFIXES: tuple[str, ...] = tuple(
    _normalized(p)
    for p in (
        sysconfig.get_paths().get("stdlib"),
        sysconfig.get_paths().get("platstdlib"),
        sysconfig.get_paths().get("purelib"),
        sysconfig.get_paths().get("platlib"),
        getattr(sys, "prefix", ""),
        getattr(sys, "base_prefix", ""),
        getattr(sys, "exec_prefix", ""),
    )
    if p
)

# The tracer's own file — never attribute effects to our own frames,
# otherwise our monkey-patched builtins see asd_tracer as the current
# qname and record effects against ourselves (e.g. when we open the
# output JSON file or call time.time() for timestamps).
_TRACER_FILE: str = _normalized(__file__)


def _is_user_file(path: str | None) -> bool:
    if not path:
        return False
    if path.startswith("<") and path.endswith(">"):
        # e.g. "<string>", "<frozen importlib._bootstrap>"
        return False
    norm = _normalized(path)
    if norm == _TRACER_FILE:
        return False
    if "site-packages" in norm or "dist-packages" in norm:
        return False
    for prefix in _STDLIB_PREFIXES:
        if prefix and norm.startswith(prefix):
            return False
    return True


# ---------------------------------------------------------------------------
# qname resolution from a frame.
# ---------------------------------------------------------------------------


def _frame_qname(frame) -> str | None:
    code = frame.f_code
    filename = code.co_filename
    if not _is_user_file(filename):
        return None

    # Module name: prefer __name__ from frame globals, fallback to basename.
    module = frame.f_globals.get("__name__")
    if not module or module == "__main__":
        module = os.path.splitext(os.path.basename(filename))[0]

    qual = getattr(code, "co_qualname", None) or code.co_name
    if qual == "<module>":
        return None  # module-level; we don't attribute effects to "module"
    return f"{module}.{qual}"


# ---------------------------------------------------------------------------
# Global tracer state.
# ---------------------------------------------------------------------------


class _Tracer:
    def __init__(self) -> None:
        # Stack of (qname, frame_id) for the currently active user frames.
        self.stack: list[tuple[str, int]] = []
        # qname -> { "observed_effects": [(effect, note), ...], "call_count": int }
        self.observations: dict[str, dict[str, Any]] = {}
        # (caller_qname, callee_qname) -> observed call count (t-013).
        self.edges: dict[tuple[str, str], int] = {}

    def current_qname(self) -> str | None:
        return self.stack[-1][0] if self.stack else None

    def record_call(self, qname: str) -> None:
        rec = self.observations.setdefault(
            qname, {"observed_effects": [], "call_count": 0}
        )
        rec["call_count"] += 1

    def record_edge(self, caller: str, callee: str) -> None:
        # A self-recursive call is a real edge but rarely interesting; keep it.
        key = (caller, callee)
        self.edges[key] = self.edges.get(key, 0) + 1

    def record_effect(self, effect: str, note: str) -> None:
        qname = self.current_qname()
        if qname is None:
            return
        rec = self.observations.setdefault(
            qname, {"observed_effects": [], "call_count": 0}
        )
        seen = rec.setdefault("_seen_effects", set())
        key = (effect, note)
        if key in seen:
            return
        seen.add(key)
        rec["observed_effects"].append({"effect": effect, "note": note})

    def dump(self) -> list[dict[str, Any]]:
        out = []
        for qname, rec in self.observations.items():
            out.append(
                {
                    "qname": qname,
                    "observed_effects": list(rec["observed_effects"]),
                    "call_count": rec["call_count"],
                }
            )
        # Stable ordering for tests/readability.
        out.sort(key=lambda o: o["qname"])
        return out

    def edges_dump(self) -> list[dict[str, Any]]:
        out = [
            {"caller": caller, "callee": callee, "count": count}
            for (caller, callee), count in self.edges.items()
        ]
        out.sort(key=lambda e: (e["caller"], e["callee"]))
        return out


TRACER = _Tracer()


# ---------------------------------------------------------------------------
# sys.settrace hook — maintains the qname stack.
# ---------------------------------------------------------------------------


def _trace_call(frame, event, arg):  # noqa: ARG001
    if event == "call":
        qname = _frame_qname(frame)
        if qname is None:
            # Not user code — still return the tracer so we see 'return' of
            # child frames, but don't push.
            return _trace_call
        # The current stack top (if any) is the caller of this new frame.
        caller = TRACER.current_qname()
        TRACER.stack.append((qname, id(frame)))
        TRACER.record_call(qname)
        if caller is not None:
            TRACER.record_edge(caller, qname)
        return _trace_return
    return _trace_call


def _trace_return(frame, event, arg):  # noqa: ARG001
    if event == "return":
        fid = id(frame)
        # Pop the top if it matches this frame (robust against re-entry).
        if TRACER.stack and TRACER.stack[-1][1] == fid:
            TRACER.stack.pop()
    return _trace_return


# ---------------------------------------------------------------------------
# Monkeypatches for common effect-producing stdlib entry points.
# ---------------------------------------------------------------------------


def _install_patches() -> None:
    # -- builtins.open -----------------------------------------------------
    _orig_open = builtins.open

    def open_patch(file, mode="r", *args, **kwargs):
        note = f"open({mode!r})"
        # Reads are implied by any open call; writes only when mode has w/a/x/+.
        m = mode if isinstance(mode, str) else "r"
        if any(c in m for c in ("w", "a", "x")) or "+" in m:
            TRACER.record_effect("io.fs.write", note)
        if "r" in m or "+" in m or not any(c in m for c in ("w", "a", "x")):
            TRACER.record_effect("io.fs.read", note)
        return _orig_open(file, mode, *args, **kwargs)

    builtins.open = open_patch  # type: ignore[assignment]

    # -- builtins.print ----------------------------------------------------
    _orig_print = builtins.print

    def print_patch(*args, **kwargs):
        TRACER.record_effect("log", "print")
        return _orig_print(*args, **kwargs)

    builtins.print = print_patch  # type: ignore[assignment]

    # -- logging module functions -----------------------------------------
    for name in (
        "debug",
        "info",
        "warning",
        "warn",
        "error",
        "critical",
        "exception",
        "log",
    ):
        if not hasattr(logging, name):
            continue
        orig = getattr(logging, name)

        def _wrap(orig=orig, name=name):
            def wrapper(*args, **kwargs):
                TRACER.record_effect("log", f"logging.{name}")
                return orig(*args, **kwargs)

            return wrapper

        try:
            setattr(logging, name, _wrap())
        except (AttributeError, TypeError):
            pass

    # -- logging.Logger methods -------------------------------------------
    for name in (
        "debug",
        "info",
        "warning",
        "warn",
        "error",
        "critical",
        "exception",
        "log",
    ):
        orig = getattr(logging.Logger, name, None)
        if orig is None:
            continue

        def _wrap_method(orig=orig, name=name):
            def wrapper(self, *args, **kwargs):
                TRACER.record_effect("log", f"Logger.{name}")
                return orig(self, *args, **kwargs)

            return wrapper

        try:
            setattr(logging.Logger, name, _wrap_method())
        except (AttributeError, TypeError):
            pass

    # -- time.sleep / time.time / time.monotonic --------------------------
    _orig_sleep = time.sleep

    def sleep_patch(secs):
        TRACER.record_effect("time.sleep", "time.sleep")
        return _orig_sleep(secs)

    time.sleep = sleep_patch  # type: ignore[assignment]

    _orig_time = time.time

    def time_patch():
        TRACER.record_effect("time.read", "time.time")
        return _orig_time()

    time.time = time_patch  # type: ignore[assignment]

    _orig_mono = time.monotonic

    def mono_patch():
        TRACER.record_effect("time.read", "time.monotonic")
        return _orig_mono()

    time.monotonic = mono_patch  # type: ignore[assignment]

    # -- subprocess / os.system / os.exec* / os.spawn* --------------------
    def _argv_note(args, kwargs) -> str:
        """Best-effort stringification of the first positional arg as a note."""
        try:
            if args:
                first = args[0]
            elif "args" in kwargs:
                first = kwargs["args"]
            elif "cmd" in kwargs:
                first = kwargs["cmd"]
            elif "path" in kwargs:
                first = kwargs["path"]
            else:
                return ""
            if isinstance(first, (list, tuple)):
                return " ".join(str(x) for x in first)
            return str(first)
        except Exception:  # pragma: no cover
            return ""

    import subprocess as _subprocess

    for _name in ("Popen", "run", "call", "check_call", "check_output"):
        _orig = getattr(_subprocess, _name, None)
        if _orig is None:
            continue

        def _wrap_subproc(orig=_orig, name=_name):
            def wrapper(*args, **kwargs):
                TRACER.record_effect(
                    "proc.spawn", f"subprocess.{name}({_argv_note(args, kwargs)})"
                )
                return orig(*args, **kwargs)

            return wrapper

        try:
            setattr(_subprocess, _name, _wrap_subproc())
        except (AttributeError, TypeError):
            pass

    # os.system / os.exec* / os.spawn*
    _os_proc_names = [
        "system",
        "execv",
        "execve",
        "execvp",
        "execvpe",
        "execl",
        "execle",
        "execlp",
        "execlpe",
        "spawnv",
        "spawnve",
        "spawnvp",
        "spawnvpe",
        "spawnl",
        "spawnle",
        "spawnlp",
        "spawnlpe",
        "posix_spawn",
        "posix_spawnp",
    ]
    for _name in _os_proc_names:
        _orig = getattr(os, _name, None)
        if _orig is None:
            continue

        def _wrap_os_proc(orig=_orig, name=_name):
            def wrapper(*args, **kwargs):
                TRACER.record_effect(
                    "proc.spawn", f"os.{name}({_argv_note(args, kwargs)})"
                )
                return orig(*args, **kwargs)

            return wrapper

        try:
            setattr(os, _name, _wrap_os_proc())
        except (AttributeError, TypeError):
            pass

    # -- env.read: os.environ + os.getenv ---------------------------------
    # Replacing os.environ entirely is risky; instead, monkey-patch
    # __getitem__/get on the class of the live mapping.
    _orig_environ = os.environ
    _orig_environ_getitem = type(_orig_environ).__getitem__
    _orig_environ_get = type(_orig_environ).get

    def _environ_getitem(self, key):
        try:
            note = str(key)
        except Exception:
            note = ""
        TRACER.record_effect("env.read", note)
        return _orig_environ_getitem(self, key)

    def _environ_get(self, key, *args, **kwargs):
        try:
            note = str(key)
        except Exception:
            note = ""
        TRACER.record_effect("env.read", note)
        return _orig_environ_get(self, key, *args, **kwargs)

    try:
        type(_orig_environ).__getitem__ = _environ_getitem  # type: ignore[assignment]
        type(_orig_environ).get = _environ_get  # type: ignore[assignment]
    except (AttributeError, TypeError):
        pass

    _orig_getenv = os.getenv

    def getenv_patch(key, *args, **kwargs):
        try:
            note = str(key)
        except Exception:
            note = ""
        TRACER.record_effect("env.read", note)
        return _orig_getenv(key, *args, **kwargs)

    os.getenv = getenv_patch  # type: ignore[assignment]

    # -- io.net.out: urllib + http.client ---------------------------------
    try:
        import urllib.request as _urllib_request
    except Exception:  # pragma: no cover
        _urllib_request = None  # type: ignore[assignment]

    if _urllib_request is not None:
        _orig_urlopen = _urllib_request.urlopen

        def urlopen_patch(url, *args, **kwargs):
            try:
                note = url.full_url if hasattr(url, "full_url") else str(url)
            except Exception:
                note = ""
            TRACER.record_effect("io.net.out", f"urlopen({note})")
            return _orig_urlopen(url, *args, **kwargs)

        try:
            _urllib_request.urlopen = urlopen_patch  # type: ignore[assignment]
        except (AttributeError, TypeError):
            pass

    try:
        import http.client as _http_client
    except Exception:  # pragma: no cover
        _http_client = None  # type: ignore[assignment]

    if _http_client is not None:
        for _name in ("HTTPConnection", "HTTPSConnection"):
            _orig_cls = getattr(_http_client, _name, None)
            if _orig_cls is None:
                continue
            _orig_init = _orig_cls.__init__

            def _wrap_init(orig_init=_orig_init, name=_name):
                def init_wrapper(self, host="", *args, **kwargs):
                    try:
                        note = str(host)
                    except Exception:
                        note = ""
                    TRACER.record_effect("io.net.out", f"{name}({note})")
                    return orig_init(self, host, *args, **kwargs)

                return init_wrapper

            try:
                _orig_cls.__init__ = _wrap_init()  # type: ignore[assignment]
            except (AttributeError, TypeError):
                pass

    # -- random: random + secrets -----------------------------------------
    import random as _random

    for _name in (
        "random",
        "randint",
        "choice",
        "randrange",
        "uniform",
        "shuffle",
        "sample",
    ):
        _orig = getattr(_random, _name, None)
        if _orig is None:
            continue

        def _wrap_random(orig=_orig, name=_name):
            def wrapper(*args, **kwargs):
                TRACER.record_effect("random", f"random.{name}")
                return orig(*args, **kwargs)

            return wrapper

        try:
            setattr(_random, _name, _wrap_random())
        except (AttributeError, TypeError):
            pass

    try:
        import secrets as _secrets
    except Exception:  # pragma: no cover
        _secrets = None  # type: ignore[assignment]

    if _secrets is not None:
        for _name in (
            "randbits",
            "token_bytes",
            "token_hex",
            "token_urlsafe",
            "choice",
        ):
            _orig = getattr(_secrets, _name, None)
            if _orig is None:
                continue

            def _wrap_secrets(orig=_orig, name=_name):
                def wrapper(*args, **kwargs):
                    TRACER.record_effect("random", f"secrets.{name}")
                    return orig(*args, **kwargs)

                return wrapper

            try:
                setattr(_secrets, _name, _wrap_secrets())
            except (AttributeError, TypeError):
                pass


# ---------------------------------------------------------------------------
# Entry point.
# ---------------------------------------------------------------------------


def _parse_args(argv: list[str]) -> tuple[list[str], str | None]:
    """Return ``(command_argv, script_path_or_none)``.

    Supports:
      * ``-- cmd arg1 arg2``  → passthrough command.
      * ``--script path [args...]`` → run a Python file.
      * Bare ``path.py [args...]`` (if no ``--`` and first token ends in .py).
    """
    if not argv:
        raise SystemExit(
            "asd_tracer: expected `-- <command>` or `--script <path>`"
        )

    if argv[0] == "--":
        return argv[1:], None
    if argv[0] == "--script":
        if len(argv) < 2:
            raise SystemExit("asd_tracer: --script requires a path")
        return argv[1:], argv[1]
    # Fallback: treat first arg as script.
    if argv[0].endswith(".py"):
        return argv, argv[0]
    # Fallback: passthrough whole argv as a command.
    return argv, None


def _write_report(command: list[str], started: str, finished: str) -> str:
    out_path = os.environ.get("ASD_TRACE_OUT", ".asd-trace.json")
    report = {
        "command": command,
        "started_at": started,
        "finished_at": finished,
        "observations": TRACER.dump(),
        "observed_edges": TRACER.edges_dump(),
    }
    with open(out_path, "w") as f:
        json.dump(report, f, indent=2)
    return out_path


def _run_script(path: str, extra_argv: list[str]) -> int:
    old_argv = sys.argv
    sys.argv = [path, *extra_argv]
    sys.path.insert(0, os.path.dirname(os.path.abspath(path)) or ".")
    try:
        runpy.run_path(path, run_name="__main__")
        return 0
    except SystemExit as e:
        code = e.code if isinstance(e.code, int) else (0 if e.code is None else 1)
        return code
    except BaseException:
        traceback.print_exc()
        return 1
    finally:
        sys.argv = old_argv


def _run_command(cmd: list[str]) -> int:
    """Run an external command. We can't trace external processes, so if the
    command is ``python ... script.py`` we rewrite it to run the script
    in-process (so the tracer sees it). Otherwise we just exec it untraced —
    which is basically useless for tracing, so we warn.
    """
    if not cmd:
        return 0
    # Common case: ``python3 script.py`` or ``python -m module``.
    first = os.path.basename(cmd[0])
    if first.startswith("python"):
        rest = cmd[1:]
        if rest and rest[0] == "-m":
            # Run module in-process via runpy.
            if len(rest) < 2:
                sys.stderr.write("asd_tracer: -m requires a module name\n")
                return 2
            module = rest[1]
            extra = rest[2:]
            old_argv = sys.argv
            sys.argv = [module, *extra]
            try:
                runpy.run_module(module, run_name="__main__", alter_sys=True)
                return 0
            except SystemExit as e:
                return e.code if isinstance(e.code, int) else (0 if e.code is None else 1)
            except BaseException:
                traceback.print_exc()
                return 1
            finally:
                sys.argv = old_argv
        # Filter out interpreter flags like -u, -B; find first .py file or module.
        for i, tok in enumerate(rest):
            if tok.endswith(".py"):
                return _run_script(tok, rest[i + 1:])
        # Nothing recognizable — fall through.
    # Non-python or unrecognized: we cannot trace an external process.
    sys.stderr.write(
        "asd_tracer: cannot trace external command {!r} — only in-process "
        "Python scripts/modules are supported in M3\n".format(cmd)
    )
    import subprocess

    return subprocess.call(cmd)


def main(argv: list[str] | None = None) -> int:
    argv = list(sys.argv[1:] if argv is None else argv)
    cmd_argv, script = _parse_args(argv)
    started = _datetime.datetime.now(_datetime.timezone.utc).isoformat().replace(
        "+00:00", "Z"
    )
    _install_patches()
    sys.settrace(_trace_call)

    exit_code = 0
    try:
        if script is not None:
            # --script path [args...] OR bare path.py [args...]
            extra = cmd_argv[1:] if cmd_argv and cmd_argv[0] == script else cmd_argv
            if extra and extra[0] == script:
                extra = extra[1:]
            exit_code = _run_script(script, extra)
        else:
            exit_code = _run_command(cmd_argv)
    finally:
        sys.settrace(None)
        finished = _datetime.datetime.now(_datetime.timezone.utc).isoformat().replace(
            "+00:00", "Z"
        )
        try:
            path = _write_report(cmd_argv, started, finished)
            sys.stderr.write(f"asd_tracer: wrote {path}\n")
        except Exception as e:  # pragma: no cover
            sys.stderr.write(f"asd_tracer: failed to write report: {e}\n")

    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
