# session-task

Cross-session work-queue + single-slot resume action CLI for Claude Code main-loop coordination.

This is the **canonical implementation**. Other repos that previously shipped a copy of this
script now contain a thin wrapper that exec's the binary installed from here.

## What it does

Three layers of task coordination:

1. **Layer 1 — `set / get / clear`** — single "top-of-mind" slot for the next resume action
   after `/clear`. Lives in `~/.config/session/resume-action.json`.

2. **Layer 2 — `queue ...`** — cross-session work queue with scope-based serialization
   groups. Items in the same scope group run one-at-a-time (priority + FIFO). Disjoint
   scope groups run in parallel. Lives in `~/.config/session/queue.json`, guarded by
   `fcntl.flock` on every read-modify-write.

3. **Layer 3** — process tracking — handled by `claude-watch active-agents` and
   `claude-watch task` (not this CLI).

## Spawn-gating workflow

Before invoking `Agent`:

```bash
# 1. Add to the queue. Always succeeds; if scope conflicts with a running peer
#    the new item is soft-serialized behind it (ready_now=false,
#    serialized_after records the peer) and the caller is told to wait.
session-task queue add "do the thing" --scope repo:foo --summary "~10 word"

# 2. If add returned ready_now=true, atomically claim it as running. If
#    ready_now=false, do NOT spawn an agent yet -- the main loop will pick
#    it up when the running peer finishes.
session-task queue register q-2026-05-01-XXXX

# 3. Spawn the Agent with `Queue item: q-2026-05-01-XXXX` in the prompt.

# 4. On completion, mark done (or abandon).
session-task queue done q-2026-05-01-XXXX
```

`session-task queue spawn-check <id>` is a read-only re-check (exit 0 = clear, exit 2 = blocked
or not found).

**Note on `--force-enqueue`**: dual purpose as of 2026-05-19 (rev 2):

  * **Bypass for the workload-scope hard-fail**. `queue add` REFUSES (exit 3,
    `QUEUE ADD REFUSED` banner) when any `--scope` token starts with `workload:`,
    because `workload run <label>` already auto-creates its own queue item with
    the matching scope. Manual `queue add --scope workload:<label>` calls
    produced two parallel queue rows tracking one tmux pane (label drift, e.g.
    `workload:promote-ready-foo` vs the runner's `workload:promote-foo`).
    `--force-enqueue` bypasses the refusal (the workload runner itself passes
    this on its auto-add path). `QUEUE_GATE_BYPASS=1` env var also bypasses.
  * **No-op for legacy non-workload calls**. Pre-2026-05-19 the default
    `queue add` hard-failed (exit 3) on scope overlap with a running peer, and
    `--force-enqueue` was needed to enqueue anyway. That default now
    soft-serializes, so the flag is a no-op for non-workload scopes.

**`repo:<name>` scope validation** (2026-07-16): `queue add` and
`queue update-scope` (add mode) REJECT (exit 1) a `repo:<name>` token whose
`<name>` is not a directory in the configured repos dir. This stops the
"invented scope name" failure mode where two agents meant to serialize on the
same repo used different fabricated scope names (`repo:botchat-ui`,
`repo:botchat-renderer`, ...) and silently failed to serialize.

  * Repos dir is `$SESSION_TASK_REPOS_DIR` (default `~/repos`).
  * **Only** the `repo:` prefix is validated — `resource:`, `path:`,
    `hostjob:`, `file:`, `task:`, `*`, and all free-form tokens are
    unrestricted.
  * **Fail-open**: when the repos dir doesn't exist / isn't a directory, no
    validation happens (allows stripped-down deploys and tmpdir test envs).
  * Reject message names the bad token, the repos dir, and the valid repo
    list, e.g. `scope 'repo:foo' invalid: no directory 'foo' in
    /home/you/repos. Valid repos: bar, baz. (...)`.
  * **Bypass** genuine edge cases (a repo not yet cloned, a scratch scope)
    with `SESSION_TASK_REPOS_NO_VALIDATE=1`, or repoint
    `SESSION_TASK_REPOS_DIR`.

**Scope/target-repo mismatch heuristic** (2026-07-24, botchat #2346):
distinct from the dir-validation above, this catches a task whose `--scope`
names a REAL repo dir but a *different* one from the repo the task text clearly
operates on (the canonical case: a task editing `<org>/platform-html-to-pdf`
scoped `repo:platform` — both real dirs, but the scope over-serializes/races
independent per-repo work). At `queue add` time, when the description/summary
names exactly ONE unambiguous target repo dir (via an `<org>/<repo>` /
`~/repos/<repo>` / `repo:<name>` mention, or a bare *distinctive* repo dir name)
that the scope doesn't cover, `queue add` emits a loud stderr **warning** and
still enqueues (rc 0).

  * **Warn, never reject** — the heuristic prefers a false-negative to a
    false-positive; the *enforcing* half is the spawn-time gate below.
  * Stays silent on ambiguity: 0 targets, >1 irreducible targets, `*` in
    scope, a short/generic bare name (`config`), or a missing repos dir.
  * Coverage: a scoped repo that is the target or *more specific* than it
    (`repo:platform-typesense` covers a bare `platform` mention) suppresses
    the warning; a scoped repo *less specific* than the target
    (`repo:platform` vs target `platform-html-to-pdf`) is the mismatch.
  * The `<org>/<repo>` prefixes are **configuration, not code**: set
    `CLAUDE_REPO_ORGS` to a comma/whitespace-separated list of the git-forge
    orgs this deploy works in (e.g. `CLAUDE_REPO_ORGS="acme-sf,acme-labs"`).
    The upstream project's own org is always recognised; unrecognised
    `<org>/<repo>` text simply isn't treated as a target (heuristic and gate
    both default open). The same variable configures the spawn-time gate.

The spawn-time enforcement lives in the `pre-agent-queue-gate-hook`
(`tools/hooks/`): it extracts the Agent prompt's target repo (same prefixed
forms) and **DENIES** the spawn on a clear mismatch with the queue item's repo
scope, defaulting open on any ambiguity.

## Files

- `~/.config/session/queue.json` — queue state (Layer 2)
- `~/.config/session/resume-action.json` — single resume slot (Layer 1)
- `~/.config/session/completed-tasks.jsonl` — completion log (both layers)
- `~/.config/session/queue-logs/` — per-completed-item transcript archives
- `~/.config/session/completed-archive/` — dated gz segments of rolled
  `completed-tasks.jsonl` history (see "Rotation / archival" below)

The schema is **stable**: `{"schema_version": 2, "items": [...]}`. Items have:
`id, description, summary, scope, group_id, group_head, status, priority, created_at,
created_by`, plus optional `started_at, registered_at, completed_at, abandoned_at,
abandon_reason, pid, last_heartbeat_at, context`.

## Implementation note

This is a Python 3 script (no third-party runtime deps). It was previously vendored in the
private dotfiles repo and lives here so deployments from this public repo (e.g. work
laptops) can pick it up directly.

The Rust daemon `claude-watch` itself does NOT consume `queue.json`. It is intentionally
schema-agnostic: `claude-watch active-agents` exposes live process facts and lets
`session-task` own the queue model. Keeping the queue model in Python avoided rewriting
~2400 lines of carefully-tuned scope-overlap and lock semantics that already work.

## Tests

```bash
cd tools/session-task
uv run --python 3.11 --with pytest pytest tests/ -v
```

165 cases, ~36s. All tests are self-contained — each runs against a
tempdir `$HOME` so the live `~/.config/session/queue.json` is never
touched. CI runs the same suite via `make test-session-task`.

### Archive-on-done behavior

`session-task queue done <id>` / `queue abandon <id>` copy the
spawning subagent's JSONL transcript (or workload `.output` file, for
workload-bound items) into `~/.config/session/queue-logs/<id>.jsonl`
and stamp `log_archive_path` on the item. The queue-minisite UI
surfaces a "View log" affordance on historical entries via that field.

The lookup chain for the spawning agent is:

1. **State file** — `$CLAUDE_AGENTS_STATE` (default
   `/var/lib/claude-watch/active-agents.json`). Maintained by a cron
   that runs `claude-watch active-agents --json --write-state` every
   minute on canonical homelab deploys. Cheap (one open + json.load)
   and current within ~60s.

2. **Binary fallback** — when the state file is missing / unreadable
   / empty AND `$CLAUDE_AGENTS_STATE_FALLBACK_BIN` resolves on PATH
   (default `claude-watch`), shell out to `<bin> active-agents
   --json` and parse the result inline. This is the container-deploy
   path where no cron exists — the in-container claude-watch binary
   walks the bind-mounted `~/.claude/projects/` tree on demand.

Both paths are best-effort: failures (missing binary, malformed JSON,
non-zero exit, subprocess timeout) yield a `[archive] no agent
record` stderr warning and skip the archive step. The lifecycle
transition (done / abandon) always completes regardless.

Set `CLAUDE_AGENTS_STATE_FALLBACK_BIN=""` to disable the fallback.

### Rotation / archival (`queue rotate`)

Both `queue-logs/` and `completed-tasks.jsonl` grow **unbounded** — nothing
pruned them (queue-logs reached ~2500 entries; completed-tasks grew multi-MB,
which made the 2026-08-16 queue.json corruption incident bigger). `session-task
queue rotate` bounds both:

```bash
session-task queue rotate                 # apply defaults
session-task queue rotate --dry-run --json # preview, mutate nothing
```

1. **queue-logs prune** — deletes transcript archives (files OR dirs) older
   than `--queue-logs-max-age` days (default 30), then enforces a hard
   `--queue-logs-max-count` floor (default 500), deleting the oldest-by-mtime
   beyond it. Recent transcripts stay so the q-site "View log" affordance keeps
   working on recent Done cards.

2. **completed-tasks roll** — once the live file exceeds `--completed-max-mb`
   (default 5 MB), the oldest rows move into a dated
   `completed-archive/completed-tasks-<UTC>.jsonl.gz` segment and the most
   recent `--completed-retain` lines (default 2000) stay in the **live** file.
   Old gz segments are themselves capped (default 20; `ROTATE_COMPLETED_ARCHIVE_MAX`).

**q-site DONE-view coordination (#581):** the DONE view reads the live
`completed-tasks.jsonl`. Rotation deliberately keeps the recent tail *in that
live file*, so a roll never hides recent done items from the view — no minisite
change is required. Deep history lives in the gz segments.

**Safety** (reuses the #580 atomic-write/lock patterns): the completed-tasks
roll holds the same `fcntl.flock` the append path (`log_completed`) takes, so a
concurrent `queue done`/`complete` is never lost mid-roll. The gz segment is
written (temp + `os.replace`) **before** the live file is truncated, so a crash
leaves a benign superset, never a gap; the live rewrite is atomic
(`_atomic_write_text`).

Every threshold is overridable via env var (`ROTATE_QUEUE_LOGS_MAX_AGE_DAYS`,
`ROTATE_QUEUE_LOGS_MAX_COUNT`, `ROTATE_COMPLETED_MAX_BYTES`,
`ROTATE_COMPLETED_RETAIN`, `ROTATE_COMPLETED_ARCHIVE_MAX`; malformed values
fall back to defaults) as well as per-invocation CLI flag.

**Scheduling:** wired as a daily cron-producer, NOT a watcher (per the repo's
watcher-vs-producer guidance) — job-name `session-rotate` in
`container/cron.d/cw-default` (04:17 daily), toggle via
`cw-cron-toggle disable|enable session-rotate`. A commented equivalent row
ships in `cron.d/cw-host` for host/systemd deploys, tagged
`# optional: session-rotate`; enable it at install time with
`scripts/install-host-cron.sh --enable session-rotate` (or
`CW_HOST_CRON_ENABLE=session-rotate`) — no template edit needed.
