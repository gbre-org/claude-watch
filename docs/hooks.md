# Hooks + obligations gate

Claude Code hooks let the harness intercept tool invocations and either DENY
them (PreToolUse) or react after they finish (PostToolUse). The
`tools/hooks/` directory ships a set of canonical hook scripts that wire
into the obligations gate (`tools/obligations/`) and the session-task queue
(`tools/session-task/`).

## Wiring

In `~/.claude/settings.json` (after `make install` puts the binaries in
`~/bin/`):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Agent",
        "hooks": [
          {"type": "command", "command": "~/bin/pre-agent-queue-gate-hook", "timeout": 5}
        ]
      },
      {
        "matcher": "*",
        "hooks": [
          {"type": "command", "command": "~/bin/pre-tool-obligations-gate-hook", "timeout": 5}
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "Read",
        "hooks": [
          {"type": "command", "command": "~/bin/post-tool-mark-attachment-read-hook", "timeout": 5}
        ]
      },
      {
        "matcher": "*",
        "hooks": [
          {"type": "command", "command": "~/bin/post-tool-obligations-update-hook", "timeout": 5}
        ]
      }
    ]
  }
}
```

## Hook scripts

| Script | Hook event | Matcher | Purpose |
|--------|------------|---------|---------|
| `pre-agent-queue-gate-hook` | PreToolUse | `Agent` | Refuses `Agent` spawns missing a `Queue item: q-XXXX` marker, or whose marker isn't `running` in the queue. |
| `pre-tool-obligations-gate-hook` | PreToolUse | `*` | Calls `obligations check`; denies when a gate-mode obligation's predicate is unsatisfied. Also enforces built-in architectural gates: (a) bare `watcher-ctl run` (must be invoked via the harness `run_in_background:true`); (b) `Monitor` tool denied inside subagent context; (c) narrow ALLOW for the heartbeat-liveness `touch` (see "Hardcoded gates" below). |
| `post-tool-obligations-update-hook` | PostToolUse | `*` | Runs `obligations post-tool` (satisfy-by-completion + inform-mode advisories) and manages a sidecar registry for `no_pending_watcher_outputs`. |
| `post-tool-mark-attachment-read-hook` | PostToolUse | `Read` | Auto-marks external-messaging attachments as read via a host-specific `*-mark-read` shim when Claude opens a file under a configured attachment dir. Host-specific integration; safe no-op when neither the shim nor the dir is present. |

All hooks default-open on internal error. A broken hook must NEVER blackhole
the loop.

## Hardcoded architectural gates

`pre-tool-obligations-gate-hook` enforces a few architectural rules in
script-local code, BEFORE the obligations CLI dispatch. These run even
with an empty obligations store -- they encode invariants that must hold
regardless of stored-state health. Two are DENY gates (`watcher-ctl run`,
`Monitor`); one is a narrow ALLOW (the heartbeat-liveness `touch`).

### Bare `watcher-ctl run`

Watchers MUST be spawned via Claude Code's `run_in_background: true`
Bash parameter, never via shell `&` / pipes / subshells / compound
operators. Any of those constructs reparents the watcher to init,
orphaning it from Claude Code's process tree. The gate denies any
`watcher-ctl run` Bash command whose whole form isn't a bare
`watcher-ctl run <name>` (optionally with `2>&1`).

### `Monitor` tool in subagent context

The `Monitor` tool is denied inside subagent context (non-empty
`agent_id` on the PreToolUse payload). The harness fires a
task-completion event on every Monitor event, which conflicts with
subagent process semantics: subagents that use `Monitor` to wait on a
long-running background job (CI checks, container builds, etc.)
reliably return mid-wait with thin partial reports while the job is
still in flight -- the agent process dies and whoever spawned the
agent has to clean up.

Soft prompt clauses ("DO NOT use the Monitor tool for CI-wait") have
been observed to fail repeatedly across PR-shipping subagents. The
gate is the hard fix.

Main-loop Monitor calls (watcher captured-output, external-messaging
threads, etc.) are unaffected -- detection uses the same `agent_id`
signal as the obligations CLI's `is_main_loop` predicate.

Recovery path (use this instead of `Monitor` to wait for CI):

```bash
for i in $(seq 1 60); do
    rollup=$(gh pr view <PR#> --json statusCheckRollup \
        --jq '.statusCheckRollup')
    # break on all-green, exit 1 on any FAILURE conclusion
    sleep 30
done
gh pr merge <PR#> --squash --delete-branch
```

Run that loop with `run_in_background: true` so the agent blocks in a
normal tool-call wait state (not a Monitor-event-driven async state).
The harness's "agent is done" semantics correctly distinguish the two.

### Heartbeat-liveness `touch` (narrow ALLOW)

When claude-watch detects a stale heartbeat it injects an instruction to
run `touch /var/run/claude/heartbeat` as a single Bash tool call to
restore liveness. But a stale heartbeat frequently coincides with a
watcher being momentarily down -- so the `watchers_healthy` (and often
`no_pending_watcher_outputs`) predicate would DENY that very touch. That
is a catch-22: the liveness-recovery touch the daemon is begging for
can't run until watchers are restarted first.

The fix is a narrow ALLOW evaluated BEFORE the obligations CLI dispatch:
a Bash command whose SOLE effective action is `touch
/var/run/claude/heartbeat` passes the gate regardless of obligation
state. It is tightly scoped via the AST matcher -- the command must be a
single top-level segment (no `&&` / `||` / `;` / `|` / `&` / newline),
its effective head (after stripping env-assignments / `sudo` / wrappers)
must be `touch`, and its only non-flag operand must be exactly
`/var/run/claude/heartbeat`. A bundled command (`touch
/var/run/claude/heartbeat && rm -rf x`), a different path, a second
operand, or a value-taking flag (`-r`/`-t`/`-d`) all fall through to the
normal gate. The liveness touch is a pure state-restore with no
destructive side effect, so allowing it is safe. Parse failure fails
CLOSED (the command is treated as non-exempt).

### Bypass

Both hardcoded DENY gates honor `OBLIGATIONS_BYPASS=1` (audited to
`~/.config/claude/obligations-bypass.log`) and per-obligation overrides
fired via `obligations override "<reason>" --duration <60|5m|1h>`.

## Obligations gate

`obligations` is a generic "must do X before Y" enforcement layer for the
harness — the structural fix for "I'll always X" verbal commitments that
evaporate from context. State at `~/.config/claude/obligations.json` (0600,
fcntl.flock-protected).

CLI: `obligations add | list | show | satisfy | override | prune | check |
post-satisfy | inform-check | post-tool` (`obligations --help` for full
surface).

### Predicate vocabulary (BOUNDED)

| Predicate | Meaning |
|-----------|---------|
| `file_mtime_within {path, max_age_secs}` | file was modified recently |
| `file_exists {path, negate?}` | file is/isn't present |
| `marker_file_present {path, negate?}` | alias of `file_exists` |
| `env_present {var, value?}` | env var set (and optionally equals value) |
| `queue_status {id, status}` | queue item in expected state |
| `no_pipe_pattern {regex}` | BAN regex against Bash command |
| `process_alive {pid_file}` | PID in file is alive |
| `process_in_pgrep {pattern}` | pattern matches via `pgrep -f` |
| `watchers_healthy {}` | `watcher-status --unhealthy-only` is empty |
| `is_main_loop {negate?}` | main-loop call vs subagent (scope guard) |
| `agent_inbox_empty {path}` | `agent-msg` inbox has no unread messages |
| `stale_ready_queue_present {threshold_secs?, queue_path?}` | BAN — true iff NO ready-now queue item has been waiting `>= threshold_secs` (default 300s). Failure carries the offending ids in `why`. |
| `all_of {predicates: [...]}` | meta-predicate (logical AND, with `is_main_loop` scope-guard short-circuit) |
| `no_pending_watcher_outputs {}` | every `tasks/*.output` sidecar has been Read |

Extend the CLI when you need more — don't shoehorn.

### Tool patterns

`*` | `Bash` | `Bash:<regex>` (Bash whose command matches regex) | `<ToolName>`.

### Enforcement modes

- `gate` (default): PreToolUse hook DENIES the matching tool call when the
  predicate is unsatisfied. The classic obligation: "must do X before Y".
- `inform`: PreToolUse never blocks. Instead, PostToolUse evaluates the
  predicate after every matching tool call and prints a single-line
  stderr banner if it's unsatisfied. Use for soft surfacing ("watcher
  health is degraded") that should be visible without blocking forward
  progress.

Pass `--enforcement inform` on `obligations add` to register a non-blocking
advisory.

### Predicate composition

To scope an obligation to the main loop only, register
`predicate: all_of [is_main_loop {}, <other>]` — the `is_main_loop` child
acts as a scope-guard.

`all_of` semantics: standard logical AND, with one short-circuit. If any
child is `is_main_loop` and that child FAILS (context doesn't match), the
entire `all_of` returns satisfied=True ("scope-guard inactive"); the
obligation does not block. The natural "main-loop only enforce X" pattern.

Detection signal: per Claude Code's hook contract, the PreToolUse /
PostToolUse JSON payload carries an `agent_id` field ONLY when the call is
from inside a subagent; main-loop calls have no `agent_id` (or empty).
Both hooks extract `payload.agent_id` and forward it to the obligations CLI
via `--agent-id`.

### Auto-satisfaction

Pass `--satisfied-by-tool X --satisfied-by-cmd-regex Y` to `obligations add`,
and the PostToolUse hook removes the obligation as soon as a matching tool
call completes.

### `exempt_patterns`

Each obligation may carry an `exempt_patterns` list (same syntax as
`tool_pattern`). The obligation applies iff its `tool_pattern` matches AND
no entry in `exempt_patterns` matches. Use to encode "this gate exists but
the recovery path must always be allowed even when the predicate is
failing". Pass `--exempt-tool-pattern <pat>` (repeatable) on `add`.

### Universal recovery exempts (deadlock floor)

Per-obligation `exempt_patterns` is opt-in: an author can forget to list a
recovery surface, and two such obligations whose exempt sets do not
overlap form a deadlock-in-waiting. To prevent this structurally, the
framework applies a fixed list of `UNIVERSAL_RECOVERY_EXEMPT_PATTERNS`
BEFORE per-obligation evaluation. Tools matching this list are allowed
past every active obligation regardless of per-row configuration.

The universal recovery surface:

- `Bash:^obligations\b` — the escape hatch itself (override/satisfy/prune/
  list/show/check/post-tool). MUST always work to break a deadlock.
- `Bash:^session-task\b` — the dispatcher's queue control surface (queue
  register/spawn-check/done/abandon/add/promote/heartbeat/show/list/
  banner/prune/set-summary plus the layer-1 set/clear/get helpers).
- `Agent` — spawning subagents is the dispatcher's primary recovery
  action.
- `Bash:^watcher-(ctl|status|restart)\b` — watcher-health recovery.
- `Bash:^(pgrep|pkill|ps)\b` — process diagnosis.
- `Read:tasks/[^"]+\.output` — captured-watcher-output Read (the
  satisfier for `no_pending_watcher_outputs`).
- `Bash:^self-clear\b` — controlled context-clear path.

Inform-mode obligations honor the universal exempts too: when the caller
is on the recovery path, repeating "watcher X is DOWN" is noise. Per-row
overrides (audited overrides) are still the preferred targeted bypass
when something OUTSIDE the recovery surface needs to fire while a gate
is active.

Design rule for any new obligation: the row-level `exempt_patterns` is
about ROW-SPECIFIC accommodations (e.g. "the SATISFIER for THIS gate is
Bash:^foo"). The universal recovery floor handles the cross-cutting
escape hatch + dispatcher + watcher recovery cases. Don't duplicate them
on every row.

### Audited overrides

```
obligations override "<reason>" --duration <60|5m|1h>
```

Short-TTL bypass that disables ALL gate-mode obligations for the duration
(hard cap 24h). Self-clears via TTL; cancel early via
`obligations satisfy <ov-id>`. Audited at create time AND on every call it
bypasses to `~/.config/claude/obligations-bypass.log`. Surfaces in
`obligations list` as "ACTIVE OVERRIDES". Inform-mode advisories are NOT
silenced (overrides gate forward progress, not visibility).

Override creation also fires a low-priority push notification via
`pingme` (when present on `$PATH`) carrying `<ov-id> (<duration>): <reason>`,
so an audited bypass surfaces on the operator's phone in addition to the
log. The `pingme` shim is host-pluggable — point it at whatever
notification service you use. Suppress in tests / CI via
`OBLIGATIONS_DISABLE_PINGME=1`.

### Emergency bypass (legacy)

- `OBLIGATIONS_BYPASS=1` — allows any tool call, audited to
  `~/.config/claude/obligations-bypass.log`.
- `QUEUE_GATE_BYPASS=1` — bypasses the `pre-agent-queue-gate-hook`,
  audited to `~/.config/claude/queue-gate-bypass.log`.

Prefer `obligations override` — scoped TTL + audit reason, vs always-on
env-var pollution.

### Default-open

If the `obligations` CLI is missing, JSON parse fails, or any internal
error happens, the hook logs to `~/.config/claude/obligations-hook-errors.log`
and allows the call. Same semantics for `watchers_healthy` if
`watcher-status` is missing or hangs.

## Tests

```
make test-hooks            # 70+ cases covering every predicate,
                           # enforcement mode, exempt-patterns,
                           # overrides, and the watcher-ctl cardinal-rule
                           # gate. Runs against an isolated $HOME tmpdir.
```

## Event-reading enforcement (`event_must_act`)

The obligations gate documented above is generic — it enforces
arbitrary predicates against arbitrary tool calls. The `event_must_act`
obligation is one concrete instance of that gate, wired specifically to
the `claude-event` bus to enforce that the main loop triages actionable
events instead of letting them pile up unread. It's seeded by
`tools/obligations/obligations-init`, evaluated by
`container/bin/eval-event-must-act`, and acked via
`container/bin/event-ack`. See [event-must-act.md](event-must-act.md)
for the four-tier classification model, the CLI surface, the
N-tool-call escalation semantics, and the container deploy + smoke-test
path. The behaviour is baked into the container image, so workbot and
any other container-driven Claude Code deployment picks it up on the
next `make container-build && make compose-up` cycle without per-host
configuration.
