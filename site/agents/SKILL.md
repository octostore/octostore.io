---
name: octostore
description: Coordinate independent agents so one leads a group or owns one exact task before side effects.
metadata:
version: 0.14.4
octostore-cli: ">=0.14.4 <0.15.0"
octostore-api: ">=0.14.4 <0.15.0"
---

# Coordinate agent work with OctoStore

Use OctoStore only when two or more independent processes might act on the same role or work item.

Choose exactly one primitive:

- **Election** — one live coordinator for a group. The hosted path needs no login.
- **Lock** — one temporary owner for one exact item. Locks require an authenticated hosted or self-hosted OctoStore.

Acquire authority before creating a branch, changing files, calling a side-effecting tool, deploying, or merging. Treat `lost`, `uncertain`, a missing response, or an unexpected hold-process exit as **not owned**. Stop issuing side effects and ask the supervisor to cancel or fence the worker.

## First result: one shared hosted election

Create one room outside the candidates:

```sh
octostore election create --json
```

Take the returned `election_id` and pass the **same ID** to every candidate. Never let each candidate create its own room for the same role.

Each candidate provides an executable worker and starts the installed supervisor with a unique candidate ID:

```sh
octostore-supervisor election "$OCTOSTORE_ELECTION" "$AGENT_ID" "$AGENT_WORKER" -- \
  octostore election hold "$OCTOSTORE_ELECTION" \
    --candidate "$AGENT_ID" --ttl 30 \
    --acquire-timeout 60 --json
```

Set `AGENT_WORKER` to the protected executable before running this command. The supervisor starts it only after a `leader` event and does not publish terminal lifecycle output until group containment completes. A `waiting` event means another candidate leads. On `lost`, `uncertain`, malformed output, or unexpected exit, stop work immediately. A hold process never reacquires after it has lost authority; start a new supervised attempt only under host policy.

The hosted authority defaults to `https://api.octostore.io`. Use `--server` or `OCTOSTORE_URL` only when all candidates intentionally share another authority. Election hold receives a secret leader capability, so custom authorities must use HTTPS or loopback HTTP; only an explicitly approved development setup may use `--allow-insecure-http`.

## Exact task ownership with a lock

Derive one stable key from durable work identity, for example `repo/octostore/issue-1842`. Reuse it on retries. Do not use a random key, prompt text, a secret, or a per-agent key.

For hosted locks, have the human sign in at <https://octostore.io/dashboard.html>, save the issued bearer token in an owner-only file, and select the hosted authority explicitly. Self-host when coordination traffic or identity must stay on your network. Set `AGENT_WORKER` to the protected executable, then run the installed supervisor:

```sh
chmod 600 /path/to/octostore-token
OCTOSTORE_URL=https://api.octostore.io \
OCTOSTORE_TOKEN_FILE=/path/to/octostore-token \
octostore-supervisor lock "repo/octostore/issue-1842" - "$AGENT_WORKER" -- \
  octostore lock hold "repo/octostore/issue-1842" \
    --ttl 120 --acquire-timeout 60 --json
```

Wait for `acquired` before protected work. `waiting` with reason `held` or `delayed` means another process owns the item. The CLI follows server retry guidance with bounded jitter. Never pass a bearer token as an argument or put it in metadata. OctoStore refuses credentialed cleartext HTTP except loopback unless the caller explicitly allows insecure development traffic.

Quickstarts bound acquisition at 60 seconds. Omit `--acquire-timeout` only when the host intentionally supervises an indefinite daemon wait.

## Supervisor contract

A heartbeat process cannot stop an uncoupled worker. The host must:

1. start `hold` before the worker;
2. validate `schema_version`, `kind`, `name`, the expected candidate where applicable, and positive `authority_remaining_ms`, `authority_observed_unix_ms`, and `authority_observed_continuous_ms` values on authority events;
3. start protected work only after `leader` or `acquired`;
4. continuously monitor JSONL and process exit; and
5. reject future or stale timestamps, subtract the greater of wall-clock queue age and same-host suspend-inclusive queue age from `authority_remaining_ms`, cancel or fence work before the adjusted budget elapses, and treat `released`, `lost`, `uncertain`, `error`, malformed output, or unexpected exit as terminal.

Run the supervisor and its `hold` child on the same host and boot; never relay or persist authority events across machines or restarts because continuous-clock timestamps are host-local. The installer provides `octostore-supervisor`; `scripts/reference-supervisor.sh` in the repository and crate is the same tested reference. It requires `jq` and Perl with `Time::HiRes`, starts a detached lifetime guardian before the hold can gain authority, and preserves group identity through containment even if a wrapper leader dies. Pass the emitted `term` to downstream systems that can reject stale writes. Already-issued external effects may still require reconciliation.

The portable reference contains cooperative hold and worker process groups, not a security sandbox. The worker and its descendants must not daemonize, call `setsid`, create a new process group, or hand work to another service. Use a platform containment boundary such as a dedicated container or cgroup for workers that may detach or are not trusted to follow that contract.

The reference supervisor re-emits `leader` or `acquired` only after the protected worker process is launched. If a worker has a real application-readiness point, set `OCTOSTORE_SUPERVISOR_REQUIRE_WORKER_READY=1`; the worker must touch the path in `OCTOSTORE_SUPERVISOR_READY_FILE` only after its handlers and work gate are ready. The supervisor will not report authority to its caller before that acknowledgement.

## Lifecycle contract

JSONL events use `schema_version: 1`. Known events are `waiting`, `leader`, `acquired`, `renewed`, `released`, `lost`, `uncertain`, and `error`. `leader`, `acquired`, and `renewed` include `authority_remaining_ms`, a non-secret relative safety budget calculated from the CLI's scheduler and suspend-inclusive deadlines; `authority_observed_unix_ms`, its wall-clock emission time; and `authority_observed_continuous_ms`, its same-host suspend-inclusive emission time. Ignore unknown additive fields. Fail closed on a future or stale emission time, unknown schema version, or event.

Exit codes:

- `0`: read succeeded, or authority was cleanly released;
- `11`: timed out before acquisition — do not do the work;
- `20`: authority was lost or uncertain — stop side effects;
- `64`: invalid command or configuration — fix it before retrying;
- `70`: unexpected failure — treat the work as not owned;
- `130` / `143`: interrupted or terminated after a bounded release attempt.

For read-only observation, use `election status|watch` or `lock status|watch`. Watch streams are best-effort hints; the CLI reconciles current status after signals and reconnects.

## Safety boundaries

- A lease is temporary advisory authority, not exactly-once execution or permission for irreversible work.
- Ask for human approval wherever the underlying task requires it.
- Make downstream writes idempotent and fencing-aware where possible.
- Public room IDs are hard-to-guess coordination addresses, not private authentication. Do not put sensitive metadata in them.
- OctoStore does not assign tasks, execute tools, store branches, merge work, or prove downstream correctness.
- Use one authority. The supported server topology is one OctoStore process backed by its SQLite database, not a high-availability consensus cluster.

## Installation and version

Report what you read as `octostore skill 0.14.4`. Check the CLI with `octostore --version`; this skill supports CLI/API `0.14.x`.

If the CLI is missing, ask before installing software. Prefer a pinned release, inspect the installer first, and use its checksum-verified path. Never silently pipe an unreviewed network response into a shell. The repository is <https://github.com/octostore/octostore.io>; exact HTTP contracts are at <https://api.octostore.io/docs>.
