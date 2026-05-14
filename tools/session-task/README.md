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
# 1. Add to the queue. Hard-fails (exit 3) if scope conflicts with a running item.
session-task queue add "do the thing" --scope repo:foo --summary "~10 word"

# 2. If add returned ready_now=true, atomically claim it as running.
session-task queue register q-2026-05-01-XXXX

# 3. Spawn the Agent with `Queue item: q-2026-05-01-XXXX` in the prompt.

# 4. On completion, mark done (or abandon).
session-task queue done q-2026-05-01-XXXX
```

`session-task queue spawn-check <id>` is a read-only re-check (exit 0 = clear, exit 2 = blocked
or not found).

## Files

- `~/.config/session/queue.json` — queue state (Layer 2)
- `~/.config/session/resume-action.json` — single resume slot (Layer 1)
- `~/.config/session/completed-tasks.jsonl` — completion log (both layers)

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
