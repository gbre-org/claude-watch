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
  - `no_pipe_pattern {regex}` — Bash command does NOT match regex (applied to
    a structure-only rendering of the command, so quoted-string / heredoc data
    can't trip it). For "never filter this tool's output" rules prefer
    `no_output_consumed` below — a regex has to enumerate every consumer, and
    the enumeration is always incomplete.
  - `no_output_consumed {commands, redirect_mode?, include_substitution?}` —
    fully AST-based BAN. Denies when a command whose effective HEAD matches
    one of `commands` (literal names, or globs naming a family such as
    `botchat-*`) has its stdout CONSUMED: piped into another command,
    redirected away (`redirect_mode`: `devnull` default, `any`, or `none`),
    or captured by a `$(...)` substitution (`include_substitution`, default
    true). A name that appears only as an argument or inside a quoted string
    / heredoc body is not a command head and never matches, so
    `grep -n 'botchat-show' Dockerfile | head` is allowed while
    `botchat-show 2008 | head` is denied. Unparseable command → DENY
    (fail-closed), with the `obligations override` hint in the `why`.
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

## `satisfied_by` — auto-clearing an obligation

An obligation may carry a `satisfied_by` block. After every tool call the
PostToolUse hook asks the CLI to remove any obligation whose `satisfied_by`
matches what just ran:

```json
"satisfied_by": {
  "tool_pattern": "Bash",
  "command_pattern": "^watcher-ctl run signal-wait-dm"
}
```

`command_pattern` is a regex over the Bash **command string**.

### Asserting the payload separately (`body_pattern`)

`command_pattern` alone is asked to do two unrelated jobs: identify *which*
command counts (so the rule does not clear on an unrelated invocation) and
assert *what it carried* (so the rule does not clear on a send that never
mentioned the thing the obligation is about). Written as one regex those two
halves fight each other.

The usual shape is `prog\b(?=.*A)(?=.*B)`. Both lookaheads are evaluated at
the offset just past the program token, and `.` does not cross newlines, so
they can only see the rest of *that line*. If the payload was written by an
**earlier line of the same command** — a `cat > "$f" <<EOF … EOF` heredoc
staging the body, which is exactly the shape forced on a CLI that forbids
stdin — then the payload text is sitting right there in the command string
and the pattern still cannot reach it. The obligation can never self-satisfy.

`body_pattern` splits the job in two:

```json
"satisfied_by": {
  "tool_pattern": "Bash",
  "command_pattern": "mysender\\b(?=.*--to\\s+alice\\b)",
  "body_pattern": "Order\\s*#\\s*42\\b"
}
```

```
obligations add ... \
  --satisfied-by-tool Bash \
  --satisfied-by-cmd-regex 'mysender\b(?=.*--to\s+alice\b)' \
  --satisfied-by-body-regex 'Order\s*#\s*42\b'
```

  - `command_pattern` keeps identifying the command.
  - `body_pattern` carries the payload anchor and is searched **on its own**
    — not anchored to wherever `command_pattern` matched — over the same
    haystacks: the command string first, then any `file_arg_flags` file
    bodies.
  - **Both must match.** Adding a `body_pattern` is strictly *narrowing*: an
    obligation that has one is harder to satisfy, never easier. All that
    changes is that each half is looked for where it actually lives.
  - Either pattern may be given alone. A pattern that does not compile is
    refused by `obligations add` (exit 2) rather than silently skipped at
    match time — a rule that can never fire is the bug this area exists to
    prevent.

An obligation that *cannot* self-satisfy is worse than no obligation at all:
the operator does the real work, the gate stays standing and blocks
everything it matches, and the only way out is an override. Train that
reflex often enough and the override stops being an exception — which
inverts the point of having a gate.

### Matching a file's contents (`file_arg_flags`)

A command string is the wrong place to look when the payload is not in the
command. Some CLIs take their input as `--file <path>` — sometimes because
piping into them is forbidden and an inline `"line1\nline2"` argument would
write a literal backslash-n rather than a newline, so any multi-line input
*must* go through a file. For those, the content the obligation cares about
never appears in the command string, the pattern can never match, and a
genuine action cannot clear its own gate.

`file_arg_flags` opts that obligation into also searching the named file:

```json
"satisfied_by": {
  "tool_pattern": "Bash:^mysender",
  "command_pattern": "delivered:",
  "file_arg_flags": ["-F", "--file"]
}
```

```
obligations add ... \
  --satisfied-by-tool 'Bash:^mysender' \
  --satisfied-by-cmd-regex 'delivered:' \
  --satisfied-by-file-flag=-F --satisfied-by-file-flag=--file
```

(Use the `=` spelling — a bare `--satisfied-by-file-flag -F` makes argparse
read `-F` as an option of its own.)

Semantics:

  - **Additive, never looser.** The command string is checked first and an
    old-style match still satisfies on its own. The regex is unchanged; the
    only difference is that it gets a second haystack.
  - **`--satisfied-by-file-flag` requires `--satisfied-by-cmd-regex` and/or
    `--satisfied-by-body-regex`** (the CLI exits 2 otherwise). Without a
    pattern there is nothing to find in the file, so the obligation would
    clear on the mere *shape* of the command — any file-carrying invocation
    at all. That is worse than the bug this exists to fix.
  - **Pair it with `body_pattern`, not `command_pattern` alone.** A payload
    file holds the message and nothing else — no program token, no
    addressing arguments — so a `command_pattern` anchored on command shape
    (i.e. every one that is safe to write) can never match a file body. On
    its own, `file_arg_flags` hands a command-shaped pattern a second
    haystack it cannot use.
  - Both `-F path` and `--file=path` spellings are recognised.
  - Paths are lifted from the **shell AST**, not scraped with a regex: a
    flag that appears inside a quoted argument or a heredoc body is data,
    not a flag.

Every failure mode declines to match, cheaply — reading a path lifted out
of a command string is a file read driven by matched text, so it fails
closed:

  - a word carrying unexpanded shell syntax (`$VAR`, backticks, globs) is
    not a resolvable path — we decline rather than guess which file ran;
  - relative paths are declined (the tool's working directory is not ours
    to assume);
  - the file must exist and be a **regular** file — directories, FIFOs,
    device nodes and sockets are refused, and the open uses `O_NONBLOCK` so
    a FIFO can never park the matcher;
  - files over 256 KiB are declined outright rather than partially read;
  - at most 4 files are consulted per command.

**A missing file is not a match.** Staged scratch files are often
short-lived and unlinked as soon as the command finishes; if the file is
gone by match time the obligation simply stays. "The evidence is gone" must
never read as "satisfied".

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
     `(pgrep|pkill|ps)`, `Read` (unconditional — reads are
     side-effect-free, can never mutate guarded state), `ToolSearch`
     (unconditional — tool-discovery is read-only and a session-start
     recovery prerequisite for loading deferred MCP recovery tools),
     `self-clear`. See
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

## `drain_before_dispatch` — force drain before task-dispatch

A full-block (`tool_pattern:"*"`) operator gate can opt in with
`--drain-before-dispatch` (manifest key `drain_before_dispatch: true`) to
FORCE the MAIN loop to clear the gate before it spawns or manages task
work. The motivating case is the botchat mark-read gate: the loop should
drain + ack operator messages BEFORE dispatching agents, not while messages
sit unread.

While such a gate is FIRING (its predicate unsatisfied), the framework floor
adjusts two ways for MAIN-loop calls (`_universal_recovery_exempt_match`):

  1. **Drops the task-dispatch surface** from the floor — the `Agent` tool +
     the mutating `session-task queue` subcommands in
     `MAIN_LOOP_DISPATCH_GATED_QUEUE_SUBCOMMANDS` (add/done/abandon/block/
     unblock/reprioritize/depend/promote/…). Those fall through to per-row
     evaluation, so the drain gate DENIES them. The loop MUST drain first.
  2. **Elevates the firing gate's own `exempt_patterns`** (its declared
     clear-path — e.g. the botchat read/ack CLIs) INTO the floor, so the
     clear-path punches through EVERY co-firing gate. Without this, a
     simultaneously-firing `queue_ready_unspawned` / `event_must_act` (which
     doesn't exempt botchat CLIs) would block the very command that clears
     the drain gate — a new deadlock.

What stays floored the whole time (so recovery is never lost): read-only +
`register` + `heartbeat` `session-task queue` subcommands, `obligations`,
watcher recovery, `Read`, `ToolSearch`, `self-clear`, and the gate's own
clear-path. So the loop can ALWAYS (a) drain+ack the gate, (b) run
recovery / override; ONLY new-task-dispatch is gated, and ONLY while the
gate is unsatisfied — pure ORDERING ("drain first"), never a standing
deadlock. Once drained, the full floor is restored.

Deadlock-safety (critical): this is STRICTLY OPT-IN. It NEVER keys on "any
firing `*` gate". Set it ONLY on a gate with a DISPATCH-INDEPENDENT
clear-path. NEVER set it on a dispatch-RECOVERY gate
(`queue_ready_unspawned` / `stale_ready_queue_present` / `event_must_act`)
whose only clear-path IS dispatch — that would deadlock (incident
2026-06-03).

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
