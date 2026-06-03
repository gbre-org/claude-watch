# obligations

Generic obligations gate — enforces "must do X before Y" rules at the Claude
Code tool layer. Used together with the PreToolUse / PostToolUse hooks under
`../hooks/`.

This is the **canonical implementation**. Other repos that previously
shipped a copy of this script now reference the binary installed from
here (default `~/bin/obligations`).

## What it does

An obligation is a row of (tool_pattern, predicate, enforcement,
deny_message). The PreToolUse hook (`../hooks/pre-tool-obligations-gate-hook`)
calls `obligations check` on every tool invocation. If a gate-mode
obligation matches the tool but its predicate is unsatisfied, the call is
denied with a banner.

The PostToolUse hook (`../hooks/post-tool-obligations-update-hook`) calls
`obligations post-tool` after every tool, which:

  - auto-removes obligations whose `satisfied_by` pattern matches the tool
    that just ran (e.g. `watcher-restart` clears a watcher-restart
    obligation), and
  - evaluates `inform`-mode obligations and prints a banner for any whose
    predicate is currently failing (non-blocking).

## Subcommands

```
obligations add | list | show | satisfy | override | prune
                 check | post-satisfy | inform-check | post-tool
```

The first six are operator-facing. The last four are the hook hot-path —
they're called by the PreToolUse / PostToolUse hooks.

## Predicate vocabulary

Bounded set, NOT Turing-complete:

  - `file_mtime_within {path, max_age_secs}` — path is fresher than N seconds
  - `file_exists {path, negate?}` — file present (or absent if negate)
  - `env_present {var, value?}` — env var set, optionally to a specific value
  - `queue_status {id, status}` — `session-task queue show <id>` reports given status
  - `no_pipe_pattern {regex}` — Bash command does NOT match regex
  - `marker_file_present {path, negate?}` — alias of `file_exists`
  - `process_alive {pid_file}` — pid_file contains a live PID
  - `process_in_pgrep {pattern}` — `pgrep -f <pattern>` returns a match
  - `watchers_healthy {}` — `watcher-status --unhealthy-only` produces no output
  - `no_pending_watcher_outputs {}` — no captured-but-unread watcher output sidecars
  - `agent_inbox_empty {path}` — agent-msg inbox has no UNREAD messages
  - `is_main_loop {negate?}` — caller is the main session loop (no agent_id)
  - `evaluator {cmd, timeout_ms?, stdin_field?, decision_mode?,
    allow_on_zero_exit?, allow_pattern?, deny_pattern?, env?}` —
    generic delegation primitive. Runs `cmd` (shell string or argv list)
    and decides allow/deny from its exit code (`decision_mode=exit_code`,
    default) or stdout regex (`decision_mode=stdout_pattern`). Stderr is
    captured into the `why` field so the operator sees the evaluator's
    own diagnostic in the deny banner. Default-open on every failure
    mode (missing cmd, timeout, spawn error, invalid regex, undecided
    pattern match); each default-open event is audited to
    `~/.config/claude/obligations-hook-errors.log`. Use this when an
    obligation needs to defer to an external decision-maker (script,
    LLM call, HTTP probe, ...) — one obligation row per use case, the
    evaluator script is the implementation.
  - `all_of {predicates: [...]}` — meta-predicate; logical AND with
    `is_main_loop` short-circuit semantics (a failing `is_main_loop`
    inside `all_of` returns satisfied=True, i.e. "this rule does not
    apply in the current context").

## Enforcement modes

  - `gate` (default): PreToolUse hook DENIES the matching tool call when
    the predicate is unsatisfied. Classic "must do X before Y."
  - `inform`: PreToolUse never denies. PostToolUse prints a single-line
    advisory banner if the predicate is currently failing.

## Bypass / overrides

Three flavors (in order of precedence at gate-evaluation time):

  1. Universal recovery exempts (framework-level deadlock floor) —
     a fixed list of tool patterns that ALWAYS pass, regardless of per-row
     configuration. Covers `obligations override / satisfy / prune`,
     `session-task queue *`, `Agent`, `watcher-(ctl|status|restart)`,
     `(pgrep|pkill|ps)`, `Read:tasks/<id>.output`, `self-clear`. See
     `UNIVERSAL_RECOVERY_EXEMPT_PATTERNS` in the source for the
     authoritative list + rationale.
  2. Per-obligation `exempt_patterns` — list of tool_pattern strings; if
     any match, the obligation auto-allows even when tool_pattern matches.
     Used to encode "this gate exists but the row-specific satisfier
     (e.g. the per-watcher recovery for THIS predicate) must always be
     allowed."
  3. Per-call audited overrides — `obligations override <reason>
     --duration <60|5m|1h> [--scope all|infra]` registers a short-TTL
     override. Audited to `~/.config/claude/obligations-bypass.log`, fires
     a Pushover via `pingme`, AND emits a loud `claude-event` (tag
     `obligations-bypass`, source `claude-watch`) so the bypass surfaces
     to the main loop on the next UserPromptSubmit. 24h cap.

     Override `--scope` (decouples the infra escape hatch from policy):
       - `all` (default): bypasses EVERY gate-mode obligation. Refused
         (exit 4) while ANY mandatory obligation is active — including a
         policy mandatory one (e.g. the AskUserQuestion ban, a
         `marker_file_present` row).
       - `infra`: bypasses ONLY infrastructure-wedge obligations —
         predicate trees composed entirely of `INFRA_PREDICATE_KINDS`
         (`watchers_healthy`, `no_pending_watcher_outputs`,
         `process_alive`, `process_in_pgrep`, `agent_inbox_empty`). It is
         NOT refused by an unrelated POLICY mandatory obligation, and it
         does NOT bypass that policy obligation — only the health wedge.
         It IS still refused by an *infra-class* mandatory obligation.

     Why two scopes (incident 2026-06-03): a single non-critical reminder
     watcher going DOWN wedged `watchers_healthy`, blocking every tool.
     `obligations override` was then refused outright because the
     AskUserQuestion ban (a policy mandatory `marker_file_present` row)
     was active — coupling two unrelated obligations and leaving no
     in-band escape. `--scope infra` clears the health wedge without
     touching, or being blocked by, the policy obligation.

Env-var emergency bypass: `OBLIGATIONS_BYPASS=1` plus a non-empty
`OBLIGATIONS_BYPASS_REASON=<text>`. Both must be set; the hook DENIES with
an explanatory banner if `OBLIGATIONS_BYPASS=1` is set without a reason
(so the env-var path is not reflex-prepended). On allow, the call is
audited to `obligations-bypass.log` AND a loud `claude-event` (tag
`obligations-bypass`, source `claude-watch`) is emitted so the next
UserPromptSubmit surfaces the bypass to the main loop. The env-var path
is single-call (one bypass = one allowed tool call); use `obligations
override` for multi-call windows. Honored in the hook script's process
env only, NOT propagated from a Bash command's inline
`OBLIGATIONS_BYPASS=1 cmd` prefix.

Design rule: obligations form a logical CONJUNCTION (every active
gate must allow a tool for it to fire). Two obligations whose exempt
sets do not overlap form a deadlock. The universal recovery floor is
the structural guarantee that the recovery surface always overlaps —
no obligation author can accidentally close it off.

## State files

  - `~/.config/claude/obligations.json` (0600) — persistent state.
    Schema: `{"obligations": [...], "overrides": [...]}`. Lock-protected
    via `fcntl.flock` on every read-modify-write.
  - `~/.config/claude/obligations-bypass.log` — audit log for overrides
    and env-var bypass invocations.
  - `~/.config/claude/obligations-hook-errors.log` — hook diagnostics
    (default-open events, missing CLI, bad JSON, etc.).
  - `/tmp/claude-watcher-output-pending/<task_id>.json` — sidecar files
    used by the `no_pending_watcher_outputs` predicate.

## Tests

The hook test suite (which exercises the CLI as well) lives at
`../hooks/tests/pre-tool-obligations-gate-hook.test`. Run from the
repo root with `make test-hooks`.

## Implementation note

Python 3, no third-party runtime deps. Default-open on every internal
error: a broken hook must NEVER blackhole the loop.
