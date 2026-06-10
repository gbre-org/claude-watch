# Authoring a custom watcher

How to write a new watcher that integrates cleanly with `claude-watch`'s
supervisor + the main loop's restart contract. Read this AFTER
[`docs/watchers.md`](watchers.md), which covers the operator-side
hygiene rules (the cardinal "main loop starts watchers" rule, the
30-second rule, the no-`&` rule, etc.). This file is the authoring
walkthrough: what a watcher IS on disk, what its lifecycle contract
looks like, and how to drop a new one into either the host-side or
container-side surface.

> **Before you start:** confirm you actually need a watcher. See
> [`docs/watchers.md` § Watcher vs. producer (cron)](watchers.md#watcher-vs-producer-cron--pick-the-right-tool).
> A cron producer is almost always simpler and should be the default.

## What a watcher is

A watcher is a **short-lived background process** that surfaces a single
external event burst into the Claude Code main loop, then exits. The
canonical shape is:

1. Block on an external signal (inotify, polling a remote API, tailing a
   log, etc.) until something interesting happens.
2. Drain whatever's pending — print one line per event to stdout — and
   APPEND each event's full payload to a ring-buffer log file the agent
   can read later.
3. Print a `WATCHER EXITED. RESTART NOW: watcher-ctl run <name>` banner.
4. Exit 0.

The main loop captures the process's stdout (because the watcher was
spawned with `run_in_background: true`), notices the RESTART banner,
respawns the watcher in parallel with whatever it does about the
surfaced events, and the cycle continues.

**Why fire-and-exit instead of a long-running poll loop?** Because the
main loop only re-receives the captured stdout AFTER the process exits.
A watcher that prints events and keeps running silently piles up stdout
the agent never sees until /clear flushes everything. Exit is the
delivery signal.

There are two physical homes for a watcher in this repo:

| Surface | Location | Wired via | Read |
|---------|----------|-----------|------|
| **Host-side** | `tools/watchers/<name>` + `~/.config/watchmen/watchers.conf` | `watcher-ctl run <name>` (supervisor in `src/watcher.rs`) | [Host-side authoring](#host-side-authoring) |
| **Container-side** | `container/watchers/<name>.sh` + `container/watchers/<name>.toml` | `/start-watchers` skill (probes `/opt/claude-container/watchers/*.toml`) | [Container-side authoring](#container-side-authoring) |

The lifecycle contract (fire → exit → main loop respawns) is the SAME
on both surfaces. The difference is purely how the supervisor finds and
launches the watcher.

## Lifecycle contract (mandatory)

Every watcher MUST follow these rules. Violations turn watchers into
orphans or duplicate-supervisor stacks and the diagnostics surfaced by
`watcher-status` get noisy.

### 1. Fire-and-exit, not poll-forever

The block-until-event loop lives INSIDE one watcher invocation. When
events arrive, drain them, print the restart banner, exit. The
supervisor (host: `watcher-ctl run`; container: `/start-watchers`'s
re-invocation) starts a fresh process for the next burst.

DO NOT write a `while true; do ... done` loop that prints events and
keeps running. The main loop only flushes captured stdout on process
exit; an immortal watcher silently buffers everything.

### 2. Exit cleanly on signal

The supervisor sends `SIGTERM` on shutdown (resume, /clear,
`watcher-restart`). A normal `trap 'echo RESTART_BANNER; exit 0' EXIT`
is enough — the daemon translates signal-killed exits to `128 + signo`
(e.g. SIGTERM → 143) so the operator can tell a real failure from a
clean shutdown, but the EXIT trap fires regardless.

### 3. Restart banner is mandatory

Print, verbatim, before exit:

```
====
WATCHER EXITED. RESTART NOW: watcher-ctl run <name>
====
```

The main loop greps for `WATCHER EXITED. RESTART NOW:` in captured
stdout to decide whether the watcher needs respawning. Without the
banner the agent has to guess from process state — fragile and racy.

### 4. PID file (host-side only) is written by the supervisor

`watcher-ctl run <name>` creates `/var/run/claude/<name>.pid` itself,
based on the spawned child's PID. The watcher script does NOT have to
write a PID file. (Older watchers like `memory-remind` still write
their own to `/var/run/claude/<name>.pid` as a belt-and-suspenders for
its heartbeat tracking — that's fine, but not required for new
watchers.)

### 5. One watcher per domain

Within a given event domain (e.g. surface inotify events from one
directory) keep ONE watcher. Two watchers tailing the same inotify
descriptor race for events and silently drop deliveries.

### 6. Output shape: one line per event

Stdout is shown to the agent verbatim with no rendering. Keep it
scannable:

- One event = one line.
- Prefix tag in `EVENT[<source>/<tag>] <first-60-chars-of-msg>…` style
  if you're emitting structured events; or just a plain `<source>:
  <summary>` if it's bespoke.
- Truncate long messages with `…`; the agent reads the ring-buffer log
  for full payloads via the matching `*-tail` tool.

### 7. `.output` file = the event channel

When the host-side supervisor (`watcher-ctl run`) wraps the watcher,
the captured stdout goes to a Claude Code background-task `.output`
file. The agent reads that file via `TaskOutput` (or just by waiting
for the auto-completion notification). **The `.output` file is the
inbound event channel** — not just diagnostic noise. Do not treat it as
"unstructured logs"; everything you print there is an event the agent
acts on. The restart banner at the bottom IS what tells the agent to
respawn the watcher.

The container-side `/start-watchers` skill writes the launcher's stdout
to the path declared in the watcher's `.toml` `log_path` field; same
semantics. The agent watches that path for new content.

## Host-side authoring

Host watchers live under `tools/watchers/<name>` in this repo and are
wired into the supervisor via `~/.config/watchmen/watchers.conf`.

### File layout

```
tools/watchers/<name>            # executable script (any shebang)
tools/watchers/<name>.md         # (optional) per-watcher doc
tools/watchers/tests/test_<name>.sh   # (optional) embedded test
```

Drop the binary or script as `tools/watchers/<name>`. Make it
executable (`chmod +x`). The shebang can be `#!/bin/bash`, `#!/usr/bin/env python3`,
or anything else PATH-resolvable. The supervisor execs it directly via
`tokio::process::Command::new(args[0]).args(&args[1..])`.

The supervisor expects the watcher to be on `$PATH` (or installed via
`make install`, which copies `tools/watchers/<name>` into `$BIN_DIR`,
default `~/bin/`). The convention in this repo is that `tools/watchers/`
contents are mirrored into `~/bin/` by `make install`.

### Registering with `watchers.conf`

`watcher-ctl` reads `~/.config/watchmen/watchers.conf` (override with
`$WATCHERS_CONFIG` for tests). Each non-comment line declares one
watcher:

```
name|pgrep_pattern|min_count|enabled|start_cmd[|on_restart_cmd]
```

Field semantics:

| Field | Required | Default | Meaning |
|-------|----------|---------|---------|
| `name` | yes | — | Identifier for `watcher-ctl run/enable/disable <name>` |
| `pgrep_pattern` | yes | — | `pgrep -f` pattern that matches the live watcher process. `watcher-status` uses this to detect DOWN / DUPLICATE |
| `min_count` | no | `1` | How many concurrent pollers should be alive. Almost always `1` |
| `enabled` | no | `true` | `true` / `false`. `watcher-ctl enable/disable` flips this |
| `start_cmd` | no | empty | Command line `watcher-ctl run <name>` execs. Whitespace-split (no shell expansion) |
| `on_restart_cmd` | no | empty | Optional history-dump command run when a stale PID file is detected. Useful for "show me what I missed" semantics |

Example entry (the canonical `claude-event-watch` line):

```
claude-event-watch|bin/claude-event-watch|1|true|claude-event-watch
```

The pattern `bin/claude-event-watch` matches any descendant of `~/bin/`
running the script; that's why `make install` symlinks the canonical
copy into `~/bin/`. If your watcher uses a different path,
substring-match against `pgrep -f` output (try `pgrep -af <pattern>`
before committing the line — the false-positive surface is what bites).

### Lifecycle, step by step

1. Main loop runs `watcher-ctl run <name>` as `run_in_background: true`.
2. Supervisor (`src/watcher.rs::watcher_run`) parses `watchers.conf`,
   resolves the entry, execs `start_cmd` as a child process.
3. Watcher blocks on its external signal; eventually an event arrives.
4. Watcher prints one line per event to stdout, appends full payloads
   to its log file, prints the RESTART banner, exits 0.
5. Supervisor returns the child's exit code via the multicall harness;
   main loop's `TaskOutput` shows the captured stdout.
6. Main loop reads the output, fires `watcher-ctl run <name>` again
   (in parallel with acting on the events), cycle continues.

### Minimum viable host watcher

```bash
#!/bin/bash
# tools/watchers/example-watcher
#
# Toy watcher that surfaces "file appeared in $WATCH_DIR" events.

set -uo pipefail

WATCH_DIR="${EXAMPLE_WATCH_DIR:-$HOME/example-events}"
LOG_FILE="${EXAMPLE_LOG_FILE:-$HOME/.config/example-watcher/consumed.jsonl}"
mkdir -p "$WATCH_DIR" "$(dirname "$LOG_FILE")"

trap 'echo "===="; echo "WATCHER EXITED. RESTART NOW: watcher-ctl run example-watcher"; echo "===="' EXIT

# Fast path: drain anything already pending.
drain() {
    local any=0
    while IFS= read -r -d '' f; do
        any=1
        local base
        base=$(basename "$f")
        echo "EVENT[example/file] $base"
        printf '{"file":"%s","ts":%s}\n' "$base" "$(date +%s)" >> "$LOG_FILE"
        rm -f "$f"
    done < <(find "$WATCH_DIR" -maxdepth 1 -type f -print0 2>/dev/null | sort -z)
    return $any
}

if find "$WATCH_DIR" -maxdepth 1 -type f -print -quit 2>/dev/null | grep -q .; then
    drain || true
    exit 0
fi

inotifywait -q -e create -e moved_to "$WATCH_DIR" >/dev/null 2>&1
sleep 2  # short settle to batch related events
drain || true
```

Register it in `watchers.conf`:

```
example-watcher|tools/watchers/example-watcher|1|false|example-watcher
```

(`enabled=false` while you iterate. Flip to `true` once the script is
stable; `watcher-ctl enable example-watcher` does this.)

Start it: in a Claude Code session, run `watcher-ctl run example-watcher`
as a background task. Drop a file in `$WATCH_DIR`; the watcher fires,
the main loop sees `EVENT[example/file] <name>` followed by the restart
banner, and respawns it.

### Config (operator-tunable knobs)

Per-watcher config dirs live under `~/.config/`. The convention is:

- `~/.config/<watcher-name>/` for state, logs, ring buffers.
- Env-var overrides for paths so tests can point at temp dirs.

Example: `claude-event-watch` reads `$CLAUDE_EVENT_QUEUE` (default
`~/claude-events/`) and `$CLAUDE_EVENT_LOG_DIR` (default
`~/.config/claude-events/`). Mirror that shape — env-var with a sane
default — so the watcher is unit-testable.

The supervisor itself reads ONLY `~/.config/watchmen/watchers.conf`
(override `$WATCHERS_CONFIG`). It does not pass anything else to the
child beyond the env it was invoked with.

### Tests

Drop unit tests at `tools/watchers/tests/test_<name>.{sh,py}` and wire
them into the `test-watchers` Makefile target:

```make
test-watchers:
	tools/watchers/tests/test_claude_event_watch.sh
	python3 tools/watchers/tests/test_self_clear_config.py
	tools/watchers/tests/test_<your-watcher>.sh
```

End-to-end "does the supervisor respawn it" tests need a live tmux
pane; cover only the portable config + drain paths in CI. See
`tools/watchers/tests/test_claude_event_watch.sh` for a copy-pasteable
template.

## Container-side authoring

Container watchers live under `container/watchers/` and are baked into
the image at `/opt/claude-container/watchers/`. They're discovered by the
`/start-watchers` skill (`container/skills/start-watchers.md`), which
parses each `.toml` file and launches the paired `.sh` via the
`Bash` tool with `run_in_background: true`.

### File layout

Each container watcher is a PAIR of files:

```
container/watchers/<name>.sh      # executable launcher (foreground forever)
container/watchers/<name>.toml    # metadata (parsed by /start-watchers)
```

### Metadata schema

```toml
name = "queue-event-tail"
description = "Tails ~/.claude-events/ for in-container handlers"
launcher = "/opt/claude-container/watchers/queue-event-tail.sh"
restart_policy = "on-failure"   # or "always" / "never"
log_path = "/tmp/claude-container-watchers/queue-event-tail.log"
```

All keys are REQUIRED.

- `name` — identifier surfaced by `/start-watchers`.
- `description` — one-line description.
- `launcher` — absolute baked path. Convention is
  `/opt/claude-container/watchers/<name>.sh` (mirrors the `name` field).
- `restart_policy` — `always` / `on-failure` / `never`. The skill
  consults this when the launcher exits.
- `log_path` — where `/start-watchers` writes the launcher's stdout
  + stderr.

### Lifecycle, step by step

1. Container session start. Baked CLAUDE.md instruction
   ([line 94](../container/baked-CLAUDE.md)) tells the agent to invoke
   `/claude-container:start-watchers`.
2. Skill runs `ls /opt/claude-container/watchers/*.toml`.
3. For each `.toml`, the skill parses metadata, then runs the
   `launcher` via `Bash` with `run_in_background: true`, capturing the
   `bash_id`.
4. Watcher does its thing (block / drain / restart-banner / exit).
5. On exit, the agent reads `bash_id`'s output (same as host), respawns
   if `restart_policy != never`.

### Differences vs host-side

- **No `watchers.conf`.** The skill enumerates files; metadata travels
  in each `.toml`.
- **No `pgrep_pattern`.** Liveness is tracked via the captured
  `bash_id`, not by `pgrep`.
- **Output goes to `log_path`** (declared in `.toml`), not to a Claude
  Code background-task buffer Resolved via `pgrep`. The agent reads the
  declared log path.
- **No host-side `/var/run/claude/` PID files.**

### Currently shipping

Zero. The container is deliberately quiet — see
[`container/watchers/README.md`](../container/watchers/README.md) for
the rationale. This authoring doc exists so the FIRST concrete watcher
that lands has a documented place to plug into.

### Adding a new container watcher (checklist)

1. Drop `container/watchers/<name>.sh` (executable; conforms to the
   lifecycle contract above).
2. Drop `container/watchers/<name>.toml` with all five required keys.
3. Update `container/watchers/README.md`'s "Currently shipping" section.
4. Rebuild the image: `make compose-build` (or
   `docker compose build claude-container`).
5. `docker compose up -d --force-recreate claude-container` and re-run
   `/start-watchers` in the in-container Claude Code session to pick
   up the new entry.
6. Extend `container/tests/baked-dirs.test` (and/or a new test) to
   assert `.sh` is executable, `.toml` parses, the metadata fields are
   present at the baked path.

## Cross-arch caveat (container only)

Container builds may target a different arch than the host. The
exec-hook bridge ([`container/hooks-shim/`](../container/hooks-shim/))
handles slash-command + skill invocation across arch boundaries via
host-bash MCP. If your watcher shells out to host-only binaries or
hard-codes an arch-specific path, account for that — the safest shape
is "watcher only touches paths inside the container's bind-mount
surface". Anything that needs host-side state (Jenkins API auth,
torrent client credentials, etc.) belongs on the **host** side, not
baked into the container.

## Worked example: a Jenkins build-failure watcher

The natural first concrete watcher: poll a Jenkins instance, surface
newly-failed builds. Drops cleanly into either surface.

### Container variant (`container/watchers/jenkins-build-failure.sh`)

```bash
#!/bin/bash
# Polls $JENKINS_URL/api/json for failed builds newer than the saved
# cursor. Fires when at least one new failure is detected.

set -uo pipefail

JENKINS_URL="${JENKINS_URL:?JENKINS_URL not set}"
JENKINS_AUTH="${JENKINS_AUTH:-}"   # "user:apitoken" if needed
STATE_DIR="${JENKINS_STATE_DIR:-/var/lib/claude-watch/jenkins-build-failure}"
LOG_FILE="${JENKINS_LOG_FILE:-/tmp/claude-container-watchers/jenkins-build-failure.log}"
CURSOR_FILE="$STATE_DIR/cursor"
POLL_INTERVAL="${JENKINS_POLL_INTERVAL:-60}"

mkdir -p "$STATE_DIR" "$(dirname "$LOG_FILE")"

trap 'echo "===="; echo "WATCHER EXITED. RESTART NOW: /start-watchers"; echo "===="' EXIT

curl_args=(-s --fail --max-time 30)
[[ -n "$JENKINS_AUTH" ]] && curl_args+=(-u "$JENKINS_AUTH")

last_cursor=0
[[ -f "$CURSOR_FILE" ]] && last_cursor=$(<"$CURSOR_FILE")

while :; do
    payload=$(curl "${curl_args[@]}" "$JENKINS_URL/api/json?tree=jobs[name,lastFailedBuild[number,timestamp,url]]" 2>/dev/null || true)
    [[ -z "$payload" ]] && { sleep "$POLL_INTERVAL"; continue; }

    new_failures=$(python3 - "$payload" "$last_cursor" <<'PYEOF'
import json, sys
payload, cursor = sys.argv[1], int(sys.argv[2])
try:
    doc = json.loads(payload)
except Exception:
    sys.exit(0)
new_cursor = cursor
hits = []
for job in doc.get("jobs", []):
    lfb = job.get("lastFailedBuild") or {}
    ts = lfb.get("timestamp", 0)
    if ts and ts > cursor:
        hits.append({
            "job": job.get("name"),
            "build": lfb.get("number"),
            "url": lfb.get("url"),
            "ts": ts,
        })
        new_cursor = max(new_cursor, ts)
for h in hits:
    print(json.dumps(h, separators=(",", ":")))
print(f"__CURSOR__ {new_cursor}", file=sys.stderr)
PYEOF
)
    new_cursor=$(grep -oP '(?<=__CURSOR__ )\d+' <<<"$new_failures" 2>/dev/null || echo "$last_cursor")
    failures=$(grep -v '^__CURSOR__' <<<"$new_failures" || true)

    if [[ -n "$failures" ]]; then
        while IFS= read -r line; do
            [[ -z "$line" ]] && continue
            job=$(jq -r .job <<<"$line" 2>/dev/null || echo unknown)
            build=$(jq -r .build <<<"$line" 2>/dev/null || echo "?")
            echo "EVENT[jenkins/build-failure] $job #$build"
            echo "$line" >> "$LOG_FILE"
        done <<<"$failures"
        echo "$new_cursor" > "$CURSOR_FILE"
        exit 0   # fire-and-exit; supervisor restarts us
    fi

    sleep "$POLL_INTERVAL"
done
```

### Container `.toml`

```toml
name = "jenkins-build-failure"
description = "Surface newly-failed Jenkins builds from $JENKINS_URL"
launcher = "/opt/claude-container/watchers/jenkins-build-failure.sh"
restart_policy = "on-failure"
log_path = "/tmp/claude-container-watchers/jenkins-build-failure.log"
```

### What the agent sees on a fire

```
EVENT[jenkins/build-failure] backend-deploy #4421
EVENT[jenkins/build-failure] integration-tests #998
====
WATCHER EXITED. RESTART NOW: /start-watchers
====
```

The agent's next move: respawn the watcher (in parallel with whatever
it does about the failures), then investigate the builds. Same shape
as every other event-surfacing watcher in the system.

### Host-side variant

Same script, dropped at `tools/watchers/jenkins-build-failure`,
chmod +x, with a `watchers.conf` line:

```
jenkins-build-failure|jenkins-build-failure|1|false|jenkins-build-failure
```

Set `enabled=true` when ready (`watcher-ctl enable jenkins-build-failure`).
The RESTART banner changes to `watcher-ctl run jenkins-build-failure`.

## Anti-patterns

- **Long-running poll loops** (no exit between event bursts). Stdout
  buffers; agent sees nothing. Fix: exit after each drain.
- **Spawning the watcher via `nohup` / `systemd-run` / shell `&`**. The
  main loop loses its handle; the watcher becomes an orphan invisible
  to obligations and `watcher-status` count. The cardinal rule in
  [`docs/watchers.md`](watchers.md) is non-negotiable: only the main
  loop spawns watchers, via `run_in_background: true`.
- **No restart banner**. The main loop has no signal to respawn.
- **Daemonizing (`setsid` / forking into background)**. Heartbeat /
  liveness signals break — claude-watch's threshold detectors can't
  tell a dead main loop from a daemonized watcher that's still
  "running".
- **Skipping the `.output` file as event channel**. Don't write events
  to a side log without also printing them to stdout — the agent reads
  the `.output` file, not your log.
- **One watcher per event**. Spawn one watcher and let it drain
  multiple events per fire; don't spin a fresh process per event.
- **Multiple supervisors for one watcher**. `pgrep -f "watcher-ctl run
  <name>"` should always return exactly one PID. If it returns more,
  `watcher-status` reports DUPLICATE and `watcher-restart` is the fix.

## Tests

Watchers ship with two test layers:

- **Unit / config tests** under `tools/watchers/tests/` (host) or
  `container/tests/` (container). Cover the drain path, the
  config-resolution path, malformed-event handling. Fast, no tmux
  required.
- **Integration tests** require a live Claude Code tmux pane and live
  outside CI (run manually or as a `#[ignore]`-gated cargo test).

Wire new test files into `make test-watchers` (host) or
`make test-container` (container) so the pre-commit hook + CI catch
regressions.

## Pointers

- Lifecycle hygiene rules (the operator's contract): [`docs/watchers.md`](watchers.md)
- Supervisor implementation: [`src/watcher.rs`](../src/watcher.rs)
- Config parser: [`src/status.rs`](../src/status.rs) `parse_watchers_config`
- Canonical host watcher: [`tools/watchers/claude-event-watch`](../tools/watchers/claude-event-watch)
- Container watcher dir + schema:
  [`container/watchers/README.md`](../container/watchers/README.md)
- `/start-watchers` skill: [`container/skills/start-watchers.md`](../container/skills/start-watchers.md)
- Cardinal rule reference (why watchers are main-loop-only):
  [`docs/watchers.md#cardinal-rule-watchers-belong-to-the-main-loop`](watchers.md#cardinal-rule-watchers-belong-to-the-main-loop)
