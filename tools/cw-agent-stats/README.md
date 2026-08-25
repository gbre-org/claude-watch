# cw-agent-stats — live agent tool-call / token snapshot producer

Host-side producer for the per-agent activity counters the
[queue-minisite](../../queue-minisite/README.md#agent-activity-counters-tool-calls--tokens)
renders (the `N agt · C calls · K tok` header pills and the per-running-row
`11 calls · 82K tok` cell), plus the Prometheus textfile gauges
(`claude_agent_live_count`, `claude_agent_calls_total`,
`claude_agent_tokens_total`, `claude_tool_use_total`,
`claude_model_use_total`, `claude_agent_main_context_tokens`).

Self-contained, stdlib only:

| File | Role |
|------|------|
| `cw-agent-stats` | The CLI (`#!/usr/bin/env python3`). `--once` / `--loop`, writes the JSON snapshot + the `.prom` file atomically. |
| `agentstats.py` | The transcript survey / fold library the CLI imports (resolved relative to the script's real path, so the `~/bin` symlink works). |
| `tests/test_agent_stats.py` | pytest suite: the fold, the snapshot schema pin, `--out` resolution, prom rendering, an end-to-end `--once` run. `make test-cw-agent-stats`. |
| `org.claude-watch.cw-agent-stats.plist` | macOS LaunchAgent (`make install-cw-agent-stats-launchd`). |

History: written as `botchat/bin/botchat-agent-stats` + `botchat/src/botchat/agentstats.py`
for botchat's header badge (botchat #2955/#2956). The scheduling moved here
first (claude-watch #655); then the feature was removed from botchat
entirely (2026-08-22) and the library was vendored here, so **no botchat
checkout, `sys.path` hack or botchat env is involved any more**.

## What it does

Every tick it scans `~/.claude/projects/<slug>/<session>/subagents/agent-*.jsonl`
(`$CLAUDE_PROJECTS_DIR` overrides the root) plus the newest top-level
`<session>.jsonl` (the main loop), folds each subagent transcript into

* **tool calls** — `tool_use` content blocks in `assistant` entries;
* **context tokens** — latest `usage.input_tokens + cache_creation_input_tokens + cache_read_input_tokens`;
* **output tokens** — per-API-message FINAL `output_tokens` (Claude Code splits one
  message into several JSONL entries with cumulative usage; the fold adds the
  per-message delta);
* **live** — written within `--live-window` (900s) and the last entry is not a
  terminal `end_turn` without a tool call;
* **window** — every agent written within `--live-window`, FINISHED ONES
  INCLUDED, deduped by agent id (a session rollover leaves the same agent a
  frozen transcript under the old session dir and a live one under the new);

and writes ONE JSON snapshot atomically (tmp + `os.replace`). Per-file byte
offsets live in-process, so `--loop` only parses the appended tail between
ticks. Full definitions + the "why transcripts, not claude-watch/agent-ctl"
survey are in `agentstats.py`'s module docstring.

## Snapshot (schema v4)

```json
{"version": 4, "host": "…", "generated_at": 1755830000.1, "generated_at_iso": "…Z",
 "live_window_seconds": 900.0,
 "main":   {"session_id", "context_tokens", "last_write_at", "age_seconds"},
 "agents": [{"agent_id", "session_id", "description", "agent_type", "queue_id",
             "tool_calls", "context_tokens", "output_tokens", "last_tool",
             "started_at", "last_write_at", "age_seconds", "finished"}],
 "totals": {"agents", "agents_spawned", "tool_calls", "context_tokens", "output_tokens",
            "window_tool_calls", "window_context_tokens", "window_output_tokens"},
 "tool_totals": {"Bash": 12}, "model_totals": {"…": 3}, "tool_model_totals": {"Bash": {"…": 12}},
 "model_output_tokens_totals": {"…": 4731}}
```

`agents` and the bare `totals.agents` / `tool_calls` / `context_tokens` /
`output_tokens` cover only what is RUNNING right now — they are all 0 in any
minute when no subagent happens to be live. `agents_spawned` and the
`window_*` figures (and every `*_totals` breakdown) cover the whole live
window, finished agents included, so a consumer can show what just happened
instead of a row of zeros.

The consumer (`queue-minisite/app.py` `_load_agent_stats`) joins
`agents[].queue_id` onto running queue rows and treats a snapshot older than
`QUEUE_MINISITE_AGENT_STATS_STALE_SECONDS` (60s) as stale. Both test suites pin
the shape; a shape change bumps `SNAPSHOT_VERSION` and touches both.

## Where the snapshot goes (`--out`)

First match wins:

| Source | Path |
|--------|------|
| `--out PATH` | as given |
| `$CW_AGENT_STATS_OUT` | as given (file path) |
| `$CLAUDE_WATCH_STATE_DIR` | `<dir>/agent-stats.json` |
| `$BOTCHAT_DATA_DIR` | `<dir>/agent-stats.json` — **deprecated**, one-release fallback, prints a warning on stderr |
| default | `/var/lib/claude-watch/agent-stats.json` |

The default is claude-watch's state dir — the same resolution the Rust daemon
uses (`CLAUDE_WATCH_STATE_DIR`, else `/var/lib/claude-watch`) and the dir its
`active-agents.json` already lives in. The compose stack bind-mounts that dir
into the queue-minisite at `/agents-state` (`CW_STATE_PATH`), and the
minisite's own default snapshot path is the sibling of its
`AGENT_STATE_JSON`, so producer and consumer agree with zero extra config.
The old `~/.config/botchat/config` lookup is gone on purpose.

**Mount the directory, not the file** when a container reads it: the atomic
rename gives the file a new inode on every write, and a single-file bind
mount pins the old one.

## Prometheus textfile output

`--prom-file PATH` / `$CW_AGENT_STATS_PROM_FILE`; else a sibling
`claude_agent_stats.prom` of `$CLAUDE_WATCH_PROM_FILE` (the Rust
`claude-watch metrics` cron's own output, so both land in the one
node-exporter `--collector.textfile.directory`); else
`/var/lib/node-exporter/textfile/claude_agent_stats.prom`. `--no-prom`
disables it. All values are windowed gauges (the whole file is rewritten each
tick), not monotonic counters.

**Missing textfile-collector directory (fresh host):** the Prometheus output
is optional — the JSON snapshot is the thing the queue-minisite actually
needs. If `--prom-file` / `$CW_AGENT_STATS_PROM_FILE` /
`$CLAUDE_WATCH_PROM_FILE` were never set and the resulting HARDCODED default
directory (`/var/lib/node-exporter/textfile/`) doesn't exist — e.g. a fresh
host that hasn't installed node-exporter's textfile collector yet — the tool
does NOT try to create that system directory. It prints one warning to
stderr per run and skips just the prom write; the JSON snapshot (`--out`)
is still written every tick and the exit code stays 0. Once you point
`--prom-file` (or either env var) at a path explicitly, that path's
directory IS auto-created (`mkdir -p`) and a real write failure there is a
hard error (exit 1), same as always — explicit means opted in.

## Running it

```sh
# one tick, inspect
cw-agent-stats --once --print --no-write --no-prom

# the cron shape: ~4s freshness, no daemon to babysit (Linux). The canonical
# in-repo form is the commented-out `cw-agent-stats` row in cron.d/cw-host,
# tagged `# optional: cw-agent-stats`; enable it at install time (no template
# edit needed) with:
scripts/install-host-cron.sh --enable cw-agent-stats
# or: CW_HOST_CRON_ENABLE=cw-agent-stats scripts/install-host-cron.sh
# the equivalent by hand:
* * * * *  USER  /usr/bin/flock -n /tmp/cw-agent-stats.lock ~/bin/cw-agent-stats --loop --duration 58 --interval 4 2>&1 | logger -t cw-agent-stats

# macOS: the LaunchAgent
make install-cw-agent-stats-launchd
```

`make install` symlinks `~/bin/cw-agent-stats` at this script. Pass `--out`
explicitly in the cron/plist if the consumer reads from anywhere other than
the state dir. Exit codes: 0 ok (including the prom-dir-missing degrade
above); 2 bad args; 1 a file could not be written (snapshot, or an
explicitly-configured prom path).

## Tests

```sh
make test-cw-agent-stats          # this dir
make test-queue-minisite          # the consumer side (pins the same schema)
```
