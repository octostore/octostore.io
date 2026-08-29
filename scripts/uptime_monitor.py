#!/usr/bin/env python3
"""Standalone OctoStore uptime monitor.

Runs under Hermes cron rather than GitHub Actions.  It writes probe records to
monitoring-data, manages one outage issue, and emits Markdown only on an outage
state transition or when generating the daily report.
"""
from __future__ import annotations

import argparse
import fcntl
import json
import os
import re
import statistics
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any

REPO = os.environ.get("OCTOSTORE_REPO", "octostore/octostore.io")
API_BASE = os.environ.get("OCTOSTORE_API_BASE", "https://api.octostore.io").rstrip("/")
SITE_BASE = os.environ.get("OCTOSTORE_SITE_BASE", "https://octostore.io").rstrip("/")
STATE = Path(os.environ.get("OCTOSTORE_MONITOR_STATE_DIR", str(Path.home() / ".hermes/state/octostore-monitor")))
CHECKOUT = STATE / "metrics"
METRICS_FILE = "data/uptime.jsonl"
MARKER = "[uptime-monitor]"
ISSUE_TITLE = "🚨 OctoStore uptime check failing"


def run(*args: str, cwd: Path | None = None, check: bool = True) -> str:
    result = subprocess.run(args, cwd=cwd, text=True, capture_output=True, timeout=60)
    if check and result.returncode:
        raise RuntimeError(f"{' '.join(args[:3])} failed: {result.stderr.strip() or result.stdout.strip()}")
    return result.stdout


def request(url: str, attempts: int = 3) -> tuple[int, bytes, int]:
    """Return status, body and elapsed ms; retry transient network/5xx failures."""
    last = (0, b"", 0)
    for attempt in range(attempts):
        started = time.monotonic()
        try:
            req = urllib.request.Request(url, headers={"User-Agent": "octostore-monitor/1"})
            with urllib.request.urlopen(req, timeout=15) as response:
                body = response.read(1_000_000)
                last = (response.status, body, round((time.monotonic() - started) * 1000))
        except urllib.error.HTTPError as exc:
            body = exc.read(1_000_000)
            last = (exc.code, body, round((time.monotonic() - started) * 1000))
        except (urllib.error.URLError, TimeoutError, OSError):
            last = (0, b"", round((time.monotonic() - started) * 1000))
        if 200 <= last[0] < 500:
            return last
        if attempt + 1 < attempts:
            time.sleep(2 * (attempt + 1))
    return last


def health_ok(code: int, body: bytes) -> bool:
    if code != 200:
        return False
    text = body.decode("utf-8", "replace").strip()
    if text == "OK":
        return True
    try:
        return json.loads(text).get("status") == "ok"
    except json.JSONDecodeError:
        return False


def ensure_checkout() -> None:
    STATE.mkdir(parents=True, exist_ok=True)
    if not (CHECKOUT / ".git").is_dir():
        if CHECKOUT.exists():
            raise RuntimeError(f"monitor checkout is not a git repository: {CHECKOUT}")
        run("git", "clone", "--branch", "monitoring-data", "--single-branch", f"https://github.com/{REPO}.git", str(CHECKOUT))
    run("git", "fetch", "origin", "monitoring-data", cwd=CHECKOUT)
    run("git", "reset", "--hard", "origin/monitoring-data", cwd=CHECKOUT)


def append_metric(record: dict[str, Any]) -> None:
    ensure_checkout()
    metrics_path = Path(METRICS_FILE)
    if metrics_path.is_absolute() or ".." in metrics_path.parts:
        raise RuntimeError("unsafe metrics target")
    root = CHECKOUT.resolve()
    target = CHECKOUT / metrics_path
    current = CHECKOUT
    for part in metrics_path.parts[:-1]:
        current /= part
        if current.is_symlink() or (current.exists() and not current.is_dir()):
            raise RuntimeError("unsafe metrics target")
        current.mkdir(exist_ok=True)
    if target.is_symlink() or (target.exists() and not target.is_file()):
        raise RuntimeError("unsafe metrics target")
    resolved_parent = target.parent.resolve()
    if resolved_parent != root and root not in resolved_parent.parents:
        raise RuntimeError("unsafe metrics target")
    with target.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps(record, separators=(",", ":")) + "\n")
    run("git", "add", "--", METRICS_FILE, cwd=CHECKOUT)
    run("git", "-c", "user.name=octostore-monitor", "-c", "user.email=monitor@octostore.io", "commit", "-m", f"probe {record['ts']} status={'ok' if record['ok'] else 'fail'}", cwd=CHECKOUT)
    for attempt in range(3):
        result = subprocess.run(["git", "push", "origin", "monitoring-data"], cwd=CHECKOUT, text=True, capture_output=True, timeout=60)
        if result.returncode == 0:
            return
        if attempt == 2:
            raise RuntimeError(f"metrics push failed: {result.stderr.strip()}")
        run("git", "pull", "--rebase", "origin", "monitoring-data", cwd=CHECKOUT)


def open_outage_issue() -> dict[str, Any] | None:
    raw = run("gh", "api", f"repos/{REPO}/issues?state=open&per_page=100")
    for issue in json.loads(raw):
        if MARKER in (issue.get("body") or ""):
            return issue
    return None


def gh_issue_number_from_url(url: str) -> str:
    return url.rstrip("/").rsplit("/", 1)[-1]


def handle_outage(summary: str, failed: bool, dry_run: bool) -> str:
    existing = open_outage_issue()
    now = datetime.now(timezone.utc).isoformat(timespec="seconds")
    if failed:
        if existing:
            if not dry_run:
                run("gh", "issue", "comment", gh_issue_number_from_url(existing["html_url"]), "--repo", REPO, "--body", f"Probe still failing at {now}.\n\nSummary: {summary}")
            return ""
        if dry_run:
            return f"🚨 **OctoStore down** (dry run)\n{summary}"
        body = f"{MARKER}\n\n**Probe failed** — standalone monitor.\n\n- Summary: {summary}\n- Timestamp: {now}"
        url = run("gh", "issue", "create", "--repo", REPO, "--title", ISSUE_TITLE, "--label", "bug", "--body", body).strip()
        return f"🚨 **OctoStore down**\n\n{summary}\n\nIssue: {url}"
    if not existing:
        return ""
    if dry_run:
        return f"✅ **OctoStore recovered** (dry run)\n{summary}"
    number = gh_issue_number_from_url(existing["html_url"])
    run("gh", "issue", "comment", number, "--repo", REPO, "--body", f"✅ Recovered {now} UTC\n\nSummary: {summary}")
    run("gh", "issue", "close", number, "--repo", REPO)
    return f"✅ **OctoStore recovered**\n\n{summary}\n\nIssue closed: {existing['html_url']}"


def probe(dry_run: bool, verbose: bool) -> str:
    health_code, health_body, health_ms = request(f"{API_BASE}/health")
    locks_code, _, _ = request(f"{API_BASE}/locks")
    openapi_code, openapi, _ = request(f"{API_BASE}/openapi.yaml")
    site_code, site_body, _ = request(f"{SITE_BASE}/")
    version = "none"
    if openapi_code == 200:
        for line in openapi.decode("utf-8", "replace").splitlines():
            if line.strip().startswith("version:"):
                candidate = line.split(":", 1)[1].strip().strip("'\"")
                if re.fullmatch(r"\d+\.\d+\.\d+", candidate):
                    version = candidate
                break
    title_ok = b"octostore" in site_body.lower()
    ok = health_ok(health_code, health_body) and locks_code == 401 and version != "none" and site_code == 200 and title_ok
    summary = f"health={health_code}/{int(health_ok(health_code, health_body))} latency={health_ms}ms locks={locks_code} version={version} site={site_code}"
    record = {"ts": datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z"), "ok": int(ok), "health_code": health_code, "response_ms": health_ms, "locks_code": locks_code, "site_code": site_code, "version": version}
    if not dry_run:
        append_metric(record)
    try:
        message = handle_outage(summary, not ok, dry_run)
    except RuntimeError as exc:
        # GitHub issue management is auxiliary. A transient API outage must not
        # make a healthy availability probe look failed, nor suppress a real
        # service-down alert.
        if ok:
            return f"healthy (GitHub metadata unavailable): {summary}" if verbose else ""
        return f"🚨 **OctoStore down**\n\n{summary}\n\nIssue management unavailable: {exc}"
    if not message and verbose:
        return f"healthy: {summary}" if ok else f"degraded (existing incident): {summary}"
    return message


def daily_report(dry_run: bool) -> str:
    ensure_checkout()
    cutoff = datetime.now(timezone.utc) - timedelta(hours=24)
    records: list[dict[str, Any]] = []
    target = CHECKOUT / METRICS_FILE
    if target.exists():
        for line in target.read_text(encoding="utf-8").splitlines():
            try:
                row = json.loads(line)
                if datetime.fromisoformat(str(row["ts"]).replace("Z", "+00:00")) >= cutoff:
                    records.append(row)
            except (ValueError, KeyError, json.JSONDecodeError):
                continue
    total = len(records)
    uptime = round(100 * sum(int(row.get("ok", 0)) for row in records) / total, 2) if total else None
    latencies = [int(row["response_ms"]) for row in records if isinstance(row.get("response_ms"), (int, float))]
    avg = round(statistics.mean(latencies)) if latencies else None
    p95 = sorted(latencies)[min(len(latencies) - 1, int(len(latencies) * .95))] if latencies else None
    releases = json.loads(run("gh", "api", f"repos/{REPO}/releases"))
    assets = [asset for release in releases for asset in release.get("assets", [])]
    downloads = sum(int(asset.get("download_count", 0)) for asset in assets)
    top = sorted(assets, key=lambda a: int(a.get("download_count", 0)), reverse=True)[:3]
    repo = json.loads(run("gh", "api", f"repos/{REPO}"))
    uptime_text = f"{uptime}%" if uptime is not None else "n/a (probe data unavailable)"
    lines = [
        f"📊 **OctoStore Daily Report — {datetime.now(timezone.utc).date().isoformat()}**",
        "",
        f"⬆️ Uptime (24h): **{uptime_text}**",
        f"⚡ Avg latency: **{avg if avg is not None else 'n/a'}ms**",
        f"📈 p95 latency: **{p95 if p95 is not None else 'n/a'}ms**",
        f"🔍 Checks run: **{total}**",
        f"❌ Downtime events: **{(total - sum(int(row.get('ok', 0)) for row in records)) if total else 'n/a'}**",
        "",
        f"📦 Total downloads: **{downloads}**",
        f"⭐ Stars: **{repo['stargazers_count']}** · 🍴 Forks: **{repo['forks_count']}** · 🐛 Open issues: **{repo['open_issues_count']}**",
        "🏆 Top downloads:",
    ]
    lines.extend(f"• {asset['name']}: {asset.get('download_count', 0)}" for asset in top if asset.get("download_count", 0))
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--mode", choices=("probe", "daily"), required=True)
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--verbose", action="store_true")
    args = parser.parse_args()
    STATE.mkdir(parents=True, exist_ok=True)
    with (STATE / ".lock").open("a+") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return 0
        message = probe(args.dry_run, args.verbose) if args.mode == "probe" else daily_report(args.dry_run)
    if message:
        print(message)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:
        print(f"🚨 **OctoStore monitor error**\n{type(exc).__name__}: {exc}")
        raise SystemExit(1)
