#!/usr/bin/env python3
"""Regression checks for standalone monitor failure isolation."""
from __future__ import annotations

import importlib.util
from pathlib import Path

SOURCE = Path(__file__).with_name("uptime_monitor.py")
spec = importlib.util.spec_from_file_location("uptime_monitor", SOURCE)
assert spec and spec.loader
monitor = importlib.util.module_from_spec(spec)
spec.loader.exec_module(monitor)


def probe_with(responses: list[tuple[int, bytes, int]], verbose: bool = False) -> str:
    iterator = iter(responses)
    setattr(monitor, "request", lambda _url: next(iterator))
    setattr(monitor, "append_metric", lambda _record: None)
    setattr(
        monitor,
        "handle_outage",
        lambda _summary, _failed, _dry_run: (_ for _ in ()).throw(
            RuntimeError("GitHub API temporarily unavailable")
        ),
    )
    return monitor.probe(dry_run=False, verbose=verbose)


# GitHub issue metadata must not turn a healthy endpoint probe into a failed cron job.
assert probe_with([
    (200, b'{"status":"ok"}', 10),
    (401, b"", 10),
    (200, b"info:\n  version: 0.14.4\n", 10),
    (200, b"<title>OctoStore</title>", 10),
]) == ""

# A real service failure must still notify even when GitHub issue management is unavailable.
alert = probe_with([
    (503, b"unavailable", 10),
    (401, b"", 10),
    (200, b"info:\n  version: 0.14.4\n", 10),
    (200, b"<title>OctoStore</title>", 10),
])
assert alert.startswith("🚨 **OctoStore down**")
assert "Issue management unavailable" in alert

print("uptime monitor failure-isolation regression checks passed")
