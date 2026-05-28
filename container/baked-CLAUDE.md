# claude-container — runtime environment

This file is the **managed-policy CLAUDE.md** baked into the
[claude-container](https://github.com/hndrewaall/claude-watch/tree/main/container)
image at `/etc/claude-code/CLAUDE.md`. Claude Code loads it at session start,
before any user-level (`~/.claude/CLAUDE.md`) or project-level
(`<cwd>/CLAUDE.md`) instructions. It exists so every session inside the
container starts with a load-bearing description of the runtime — what's
real, what's a bind-mount, what doesn't work — without depending on host
config the operator may or may not have wired up.

It is **container-owned, not user-owned**: do not edit
`/etc/claude-code/CLAUDE.md` from a session. The source of truth lives at
`container/baked-CLAUDE.md` in the claude-watch repo; rebuild the image to
pick up changes.

---

## Dispatcher, not worker — ABSOLUTE PRIORITY

**Any operation that needs more than ONE tool call MUST be delegated to a
subagent via the Task / Agent tool.**

No Read→Edit→Bash→Edit sequences in the main session. If you find yourself
reaching for a second tool call in the same turn, STOP and spawn an agent
instead.

Examples that are MUST-delegate:

- Investigating a bug (multiple Read + grep)
- Implementing a feature (Edit + test + commit + push)
- Anything involving git commits, PRs, or pushed artifacts
- Validating CI / waiting for external state

Examples that are OK inline (single tool call):

- A single Read to check a file path
- A single Bash to query state (`ls`, `git status`, single curl)
- A single Edit when the change is one localized hunk and you've already
  read the file in a prior turn

The session's job is to DISPATCH work, not perform it. The Task agent
handles the work; the session orchestrates.

## claude-watch alerts — STOP EVERYTHING — NON-NEGOTIABLE

When claude-watch injects an alert into the tmux pane — prolonged thinking,
context warning, watcher down — STOP immediately. Do NOT finish the current
operation. Do NOT complete the in-flight reply. DROP IT ALL and attend the
alert.

Compaction or context clearing doesn't kill background tasks but you LOSE
HANDLES on them. Delaying the alert means the situation is WORSE when the
hard clear comes (unpredictable context loss, no chance to save state). A
controlled pause lets you save state cleanly via `session-task set` + commit
+ log update before clearing.

When you see a `[CLAUDE-WATCH]` line:

1. Commit + push any in-flight repo work.
2. Update today's daily log if substantive activity has happened.
3. `session-task set "what to continue doing"` with enough context for the
   next session.
4. Self-clear if the alert says to.

This rule has the same standing as the dispatcher rule: NON-NEGOTIABLE. The
alert is the highest-priority message the session can receive.

---

## You are running inside a Linux container

If you are reading this file via the standard CLAUDE.md load path, you are
**inside the `claude-container` Docker image**, not on the operator's host
machine. The distinction matters for many decisions:

- `uname -a` returns `Linux <hostname> ... GNU/Linux` regardless of what
  the host OS is. A macOS host bind-mounts files into this Linux userland.
- Binaries built for the host architecture (typically macOS Mach-O / arm64
  on developer laptops) **cannot execute inside this container**. Linux
  rejects them with "Exec format error". See "Cross-arch binaries" below
  for the shim that handles this gracefully.
- The container user is `hndrewaall` (uid 1000, gid 1000). This is an
  in-container identity, hardcoded in the Dockerfile to match a typical
  uid-1000 host user so bind-mounted files round-trip without root-owned
  artifacts. The host user can have any name.
- Hostname is typically `claude-container-<rand>` or whatever
  `docker run --name` was passed; do not infer the host identity from it.

**Quick self-check**: if you need to confirm "am I in the container?",
run `cat /etc/claude-code/CLAUDE.md | head -3`. If you see this file's
header, you are in the container. The host has no `/etc/claude-code/`
unless the operator explicitly created one.

## Session-start checklist — MANDATORY first action

**ON EVERY SESSION START (including `/clear`, restart, or context
compaction): run this checklist BEFORE doing anything else.** The whole
point is to surface what the container exposes — and what it doesn't —
in this session, so the rest of the conversation doesn't drift into
assumptions about a host-side surface that isn't here.

This is the container equivalent of a host-side "resume checklist".
The list is intentionally short — the container is a sandbox for code
work, not the host's full automation stack, so the checks below are
all that's needed.

1. **Self-id**: run `cat /etc/claude-code/CLAUDE.md | head -3`. Confirm
   you see the "claude-container — runtime environment" header. If you
   don't, you are NOT in this container — stop and re-check before
   continuing (some host-side instructions are unsafe to run in a
   container; some container-side ones are unsafe on the host).
2. **MCP bridges reachable**: run `claude mcp list`. Expected to see at
   least `mcp-adaptor` and (if the operator configured it) `host-bash`,
   each with a `Connected` status. If a bridge shows as failed, note it
   for the operator — many corp workflows depend on these.
3. **Hook fate**: run `audit-hooks` (no args). The summary line reports
   how many host-bound hooks land as `ok-elf`/`ok-script` vs
   `silent-no-op`/`missing`. A non-zero `silent-no-op` count is normal
   for cross-arch host (e.g. Mac) telemetry binaries — that's
   `exec-hook` doing its job. The check is informational; you don't have
   to act on it unless the operator asks.
4. **Probe host OS via `host-bash`** (if `host-bash` is connected): a
   single `uname -s` (or `powershell -Command "$PSVersionTable.OS"` if
   `uname` isn't present) tells you whether the host is Linux, macOS,
   or Windows. The answer shapes which scheduler / package-manager
   guidance below applies. Skip if `host-bash` is unavailable.
5. **Announce scope**: in your first response of the session, state
   one line summarizing where you're running (claude-container, the
   bind-mount surface from the table below, MCP bridges available, hook
   audit summary, host OS if probed) so the operator can see at a
   glance what you have to work with. Keep it concise — one or two
   sentences.
6. **List baked skills + agents + watchers**: `ls
   /etc/claude-code/skills/ /etc/claude-code/agents/
   /etc/claude-code/watchers/`. Skills land at
   `/claude-container:<name>` (e.g. `/claude-container:restart`,
   `/claude-container:start-watchers`); agents are spawned with
   `Agent(subagent_type="claude-container:<name>", ...)`; watchers are
   shell scripts the agent launches via the `Bash` tool with
   `run_in_background: true`. The full convention + how-to-add lives in
   the per-dir READMEs at the repo's
   [`container/skills/`](https://github.com/hndrewaall/claude-watch/tree/main/container/skills),
   [`container/agents/`](https://github.com/hndrewaall/claude-watch/tree/main/container/agents),
   [`container/watchers/`](https://github.com/hndrewaall/claude-watch/tree/main/container/watchers).
7. **Start event watchers via `/claude-container:start-watchers`**.
   Watchers are **session-scoped `run_in_background` Bash tasks** that
   must be (re)started on every session start, `/clear`, resume, or
   context compaction. They do NOT survive across sessions — there is
   no long-lived supervisor process.

   The canonical watcher is `claude-event-watch` (block-print-exit
   pattern):
   - Blocks on `inotifywait` until a new `.json` event file appears
     in `~/claude-events/` (or `$CLAUDE_EVENT_QUEUE`)
   - Debounces (default 30s) to batch burst events
   - Prints all pending events as one-liners:
     `EVENT[source/tag] message`
   - Deletes processed event files
   - Prints a restart banner and **EXITS**

   Claude Code delivers the watcher's stdout back to the session as a
   background-task completion notification. **On receiving watcher
   output, IMMEDIATELY restart the watcher** (before processing the
   events) to avoid missing events during processing.

   The `/claude-container:start-watchers` skill starts (or restarts)
   all watchers. Run it at step 7 of this checklist and again whenever
   a watcher exits with output.

**Event watchers inside this container are scoped narrowly.**
The container is a code-writing sandbox, not a host automation hub.
Don't try to start signal watchers, torrent watchers, podcast watchers,
or anything else from the host's resume-checklist playbook; the
relevant tools and services aren't installed here. The baked watcher
(`claude-event-watch`) covers the in-container event bus at
`~/claude-events/`.

If the operator gives you a job that genuinely needs a host-side
watcher / notifier, run it on the host instead (via the operator's host
Claude Code session) or bridge the watch event over `host-bash`.

> **Watcher vs. cron decision:** before adding a new watcher, confirm
> a watcher is actually needed. Cron is almost always simpler: it has
> no persistent footprint, no restart cycles, and no DOWN-state alerts.
> A dedicated watcher is justified only when sub-minute reactivity is
> required AND no kernel event mechanism (inotify, systemd path units)
> fits. See [`docs/watchers.md` § Watcher vs. cron](https://github.com/hndrewaall/claude-watch/blob/main/docs/watchers.md#watcher-vs-cron--pick-the-right-tool)
> for the full decision framework, alternatives (kernel events, extending
> claude-watch, cron + internal poll loop), and a concrete example.

## Main loop is a coordinator, not a worker

The in-container Claude Code session has two execution tiers, and the
default tier for substantive work is **not** the main loop:

- **Agent tool calls** — semantic LLM work with bounded scope. Reading
  multiple files, multi-file edits, running tests, shipping a PR,
  investigating a bug, drafting prose with research, anything that
  would chain more than ~1 tool call. Agents are subject to the
  queue-protocol PreToolUse hook (see next section).
- **Main loop** — dispatcher. Single bounded commands. Reads a
  notification, classifies it, decides what to do, and **delegates**.
  Validates the agent's return value. Composes the operator-facing
  reply. That's it.

**Bias toward delegation.** Any operation that involves more than ~1
tool call, OR that reads multiple files, OR that makes multi-file
edits, OR that runs tests, OR that ships code through review →
delegate it to an Agent. Do not do it inline in the main loop.

Why this matters even when nothing is forcing the choice:

- **Context is precious in the main loop.** Every tool result the
  main loop sees costs context the operator can never get back. A
  subagent runs in its own context window — large reads, long test
  output, verbose CI logs all stay there, not in the main loop's
  transcript. When the agent returns, the main loop sees only the
  agent's final summary.
- **Failures are easier to recover from when bounded.** If a
  subagent goes sideways (wrong direction, infinite loop, bad
  edit), the main loop can abandon the queue item and try again
  from a clean slate. An inline failure pollutes the main loop's
  state — the operator sees the half-finished work, the wrong
  edits, the dead-end exploration.
- **Parallelism.** While an agent is working, the main loop can
  handle inbound (queue events, notifications, operator messages)
  instead of blocking. Many in-flight subagents at once is normal
  and healthy.
- **The queue is the audit trail.** Every queue item is a
  durable record of "the main loop decided to spawn an agent for
  X scope at Y time." Inline work leaves no such record — it's
  invisible to the operator and to anyone reviewing what the
  session did.

Tier choice in practice:

- **Interpret / decide / multi-file edit / validate / ship a PR**
  → Agent.
- **Single bounded command + check the result** → main loop.
- **External wait** (CI run, long build, sleep-based poll) →
  spawn an Agent that does the wait, not the main loop. The main
  loop should never sit in a polling sleep loop.

**One concern per agent.** Each agent handles ONE task — never batch
unrelated work into a single agent prompt. If you have 3 independent
things to do, queue 3 items and spawn 3 agents. Batching unrelated work
means: a failure on task 2 loses task 3, the queue audit trail is
useless, and parallelizable work gets serialized. The signal you're
batching wrong: your agent prompt has numbered sections for unrelated
concerns. Split them.

If you're in the main loop and find yourself about to chain
`Read` → `Edit` → `Edit` → `Bash` → `Bash`, **stop and queue an
Agent for the whole sequence instead.** The PreToolUse queue-gate
hook (next section) enforces "Agent spawns require a queue item"
— this section enforces the upstream policy that the spawn should
happen in the first place.

## Queue protocol — every Agent tool call

Before firing **any** `Agent` tool call, you MUST first add a queue
item via `session-task queue`. The queue serializes work that touches
overlapping scopes (e.g. two agents editing the same repo at once),
and the in-container scope namespace is **shared with the host** —
`repo:claude-watch` covers BOTH host-side and container-side work
touching that repo. An in-container agent that skips the queue can
race against host-side work on the same repo, lose edits to a parallel
agent, or stomp on a long-running build.

**The `pre-agent-queue-gate-hook` PreToolUse hook IS active inside
this container** when `CLAUDE_CONTAINER_OBLIGATIONS=1` (the default,
set by the entrypoint). The hook is baked at
`/usr/local/bin/pre-agent-queue-gate-hook` and wired into Claude
Code's PreToolUse cascade via the entrypoint-generated
`/tmp/claude-shim/settings.json` (matcher `"Agent"`). Any `Agent`
tool call that lacks a `Queue item: q-XXXX` marker in its prompt
— or carries an unknown / non-`running` queue id — is HARD-DENIED
at dispatch time, exactly like on the host. The model never sees
the spawn happen; it gets the deny banner back as a tool-use
permission denial.

The hook resolves queue state by shelling out to
`session-task queue show <id>`. That CLI ships in the bind-mounted
`~/repos/claude-watch/tools/session-task/` tree. When the bind-mount
is absent (stripped-down `docker run` without `~/repos`), the
lookup returns "not found" and the hook still DENIES — the deny
reason names `session-task` so the operator can see why. If you
hit that in a fresh container, ask the operator to bind-mount
`~/repos/claude-watch` (the example compose does this by default).
The hook only default-opens on TRULY unexpected internal errors
(broad-except fail-safe), not on the routine "CLI missing" path.

The five-step protocol (mirrors the host CLAUDE.md `## Resume Actions`
spawn workflow):

1. `session-task queue add "<task description>" --scope <scope> --summary "~10 word headline"`
   → returns JSON with a queue id (`q-YYYY-MM-DD-XXXX`). **Exit 3 =
   HARD REFUSED for scope overlap; DO NOT spawn.** Wait or pick a
   different scope.
2. Read `ready_now` from the JSON. If `false`, DO NOT FIRE — another
   item with overlapping scope is in flight. Wait for blockers and
   re-check via `session-task queue spawn-check <id>`.
3. If `ready_now=true`: `session-task queue register <id>` to claim
   the slot.
4. **Include the line `Queue item: q-XXXX` in the Agent's prompt.**
   The hook DENIES the spawn without it.
5. Fire the Agent. On completion: `session-task queue done <id>`
   (success) or `session-task queue abandon <id> --reason "..."`
   (failure / cancelled).

Quick reference: `session-task queue --help` for the full subcommand
surface (`add | list | spawn-check | register | done | abandon | show`).
The `session-task` CLI is bind-mounted in via `~/repos/claude-watch`;
if it's not on PATH, the operator hasn't wired the bind-mount and you
should flag that before spawning agents at all.

### Verify agent success before marking done

**Never call `session-task queue done <id>` until you have received the
agent's task-notification AND verified the agent reported success.** The
main loop receives many `<task-notification>` messages (from watchers,
other background tasks, etc.) — only the one carrying the agent's
`task-id` signals that agent's completion. Specifically:

- Wait for the `<task-notification>` whose `task-id` matches the agent
  you spawned (not any other background task).
- Verify `<status>completed</status>` in that notification (not
  `failed`, not `cancelled`).
- Only THEN call `session-task queue done <id>`.
- If the agent failed or you cannot confirm success, call
  `session-task queue abandon <id> --reason "agent failed: <reason>"`
  instead.

Marking a queue item `done` prematurely (before agent completion or
on a misidentified notification) releases the scope lock and allows
conflicting work to start — which races against the still-running
agent or silently drops failed work on the floor.

### Agent completion ack obligation (enforced)

The `agent_ack_pending` obligation **enforces** the verify-before-done
rule above. When a task-notification arrives for a completed agent,
the main loop MUST follow this protocol:

1. `agent-ack register <queue-id> [--agent-id <id>]` — register that
   you received a task-notification for this queue item.
2. Read the agent's output. Verify success or failure.
3. `session-task queue done <queue-id>` (success) or
   `session-task queue abandon <queue-id> --reason "..."` (failure).
4. `agent-ack done <queue-id>` — clear the pending-ack entry.

**The evaluator fires after 2 consecutive non-exempt Bash calls**
(configurable via `$AGENT_ACK_N`) while pending-ack entries exist.
This means you have a very short window to process the completion
before the gate blocks you. The intent is that agent completions are
high-priority: process them before moving on to other work.

Quick reference:

```sh
agent-ack register <queue-id> [--agent-id <id>]  # step 1
agent-ack done <queue-id>                         # step 4
agent-ack list [--json]                           # inspect state
agent-ack status                                  # one-line summary
agent-ack clear                                   # escape hatch
```

### Queue IMMEDIATELY — never defer

**Queue items the moment you intend to do the work.** Never say "I'll
queue it once X finishes." If you intend to do something, queue it NOW.
Use scopes and the blocking mechanism to prevent it from RUNNING until
the right time — that's what scopes are for. Holding a task in your head
instead of the queue means it gets lost on compaction/clear.

Wrong:
```
# scope conflict — "I'll queue it later"
```

Right:
```
session-task queue add "..." --scope <non-conflicting-scope> --summary "..."
# OR if the scope genuinely conflicts:
session-task queue add "..." --scope <same-scope> --force-enqueue
# (it'll be serialized behind the running item automatically)
```

### Continuous subagent queue-discipline enforcement

The `pre-agent-queue-gate-hook` above only fires at SPAWN time. A
second gate, the `subagent_queue_item_running` obligations predicate,
continues to enforce queue discipline THROUGHOUT a subagent's
lifetime. The predicate is seeded as a default-bundled obligation row
by `obligations-init` (run from the entrypoint when
`CLAUDE_CONTAINER_OBLIGATIONS=1`).

How it works:

  - `post-tool-agent-arm-hook` fires on every successful Agent spawn
    (`PostToolUse:Agent` with `async_launched=true`), reads the
    `Queue item: q-XXXX` marker from the spawn prompt + the new
    subagent's `agentId` from the tool response, and writes a binding
    record to `~/.config/claude/agent-queue-bindings.json`.
  - On every subsequent **subagent** tool call, the
    `subagent_queue_item_running` predicate looks up the agent's q-id
    in the queue:
      - `status=running` → **ALLOW**.
      - `status=done` / `status=abandoned` → **DENY** with a banner
        naming the q-id + current status.
      - q-id has vanished from the queue entirely → **DENY**
        (the canonical "main loop abandoned this work" case).
  - Main-loop calls are always allowed (the row is scoped via
    `is_main_loop {negate: true}` inside an `all_of`).

**As a subagent, when you hit this gate:** your queue item has been
finished, abandoned, or pruned. The right responses are usually:

  - **Re-register**: if the main loop just rotated the queue id (rare
    but possible during dispatch transitions), `session-task queue
    register <new-q-id>` is exempt from the gate, so you can run it
    to pick up the new id.
  - **Stop**: if your work is genuinely done from the main loop's
    perspective, return your final value and let your process exit.
    Don't try to keep working past a `done` queue state — the main
    loop is no longer tracking your scope.

The exempt set lets you reach `session-task queue
{status,spawn-check,register,show,list}`, `obligations
{list,show,status,check,override,satisfy}`, `claude-watch-ack`,
`claude-watch-dispatch`, and `agent-msg {ack,inbox,gc,disarm}` /
`agent-tail` even while the gate is firing, so you can always inspect
state + recover.

Default-open contracts (predicate inert, tool call ALLOWED):

  - Call is from the main loop (no `agent_id`) — predicate doesn't
    apply.
  - Binding file missing / corrupt / unreadable — defensive.
  - No binding entry for this agent_id — subagent was either spawned
    before the predicate rolled out OR carries no `Queue item: q-XXXX`
    marker.

A hook bug can never blackhole a real subagent.

### Generic `evaluator` predicate — delegate gate decisions to a script

`evaluator` is a general-purpose obligation predicate that runs an
external subprocess and uses its result to allow or deny a tool call.
Use it whenever a gate needs to defer to an outside decision-maker —
a deterministic script, an LLM call, an HTTP probe to a policy
service, a regex audit, etc. The predicate is deliberately
implementation-agnostic; the obligation row carries the `cmd` and the
operator supplies whatever the gate should consult.

Register one obligation row per use case:

```sh
obligations add \
  --tool-pattern '<tool>' \
  --predicate evaluator \
  --params '{
    "cmd": "/path/to/evaluator-script",
    "timeout_ms": 5000,
    "stdin_field": "tool_input.command",
    "decision_mode": "exit_code"
  }' \
  --ttl 0 \
  --deny-msg "<message shown when the evaluator denies>"
```

Params:

  - `cmd` (required): shell-style string (run via `/bin/sh -c`) or
    argv list. Receives `stdin_field` content on stdin and the
    current tool / command preview via the
    `OBLIGATIONS_EVAL_TOOL` and `OBLIGATIONS_EVAL_COMMAND_PREVIEW`
    env vars. Empty / missing => allow + audit-log.
  - `timeout_ms` (default 5000): hard subprocess timeout. Timeout =>
    allow + audit-log.
  - `stdin_field` (default null): which `tool_input` field to pipe to
    the evaluator. Accepts `tool_input.command`, `command`,
    `tool_input.prompt`, `prompt`, etc. Null => empty stdin.
  - `decision_mode` (default `exit_code`):
      * `exit_code`: allow iff the subprocess exits 0 (flip with
        `allow_on_zero_exit: false`).
      * `stdout_pattern`: capture stdout, run `re.search` against
        `allow_pattern` / `deny_pattern`. `allow` wins on
        simultaneous match; neither matching => default-open allow +
        audit-log.
  - `env` (optional dict): extra env vars merged into the
    subprocess environment.

Decision contract:

  - Allow => predicate satisfied; obligation does NOT block.
  - Deny  => predicate fails; obligation blocks the tool call.
  - Subprocess stderr is captured (truncated at ~2KB) and surfaced
    verbatim inside the deny banner / `permissionDecisionReason`,
    so the operator sees the evaluator's own diagnostic right next
    to the deny.

Default-open posture (a misconfigured evaluator must never blackhole
the loop):

  - Missing `cmd`, spawn error (file not found / EACCES), timeout,
    invalid regex, unknown `decision_mode`, undecided
    `stdout_pattern` match, or any uncaught exception => ALLOW.
  - Every default-open event is audited to
    `~/.config/claude/obligations-hook-errors.log` with `source:
    "obligations:evaluator"` so post-mortems can recover the lost
    decisions.

Bypass: the standard surface applies. `obligations override
"<reason>" --duration <N>` bypasses every gate-mode obligation
including evaluator-backed ones. `OBLIGATIONS_BYPASS=1` also
bypasses. There is no per-row evaluator env-var bypass — instance-
specific escape hatches belong inside the evaluator script (the
operator owns that surface, the primitive stays small).

Use-case sketches (all separate obligation rows, all reusing this one
primitive):

  - Outbound Signal pronoun audit on `signal-send` invocations —
    evaluator script parses the staged body, queries the local
    members.json, denies on pronoun mismatch.
  - Dispatcher-quality reviewer on every `Agent` spawn — evaluator
    invokes an LLM-backed audit script that scores the prompt.
  - Security-classification triage on outbound `gh issue comment` —
    deterministic grep against a private-path block-list.
  - HTTP probe to an external policy service — evaluator is a curl
    wrapper that exits 0 / 1 based on the response.

Each use case is one obligation row with its own `cmd`,
decision-mode, and patterns. The primitive itself stays
LLM-agnostic.

## Agent communication channels — two distinct inbound paths

A spawned subagent has TWO distinct inbound channels you must
understand. Both surface at the same `PreToolUse` boundary, but they
come from different senders and behave differently.

### Channel 1: `agent-msg` — main loop -> subagent inbox

`agent-msg` is the **CLI inbox protocol**. When the main loop wants to
direct a running subagent (scope correction, status update from a peer
agent, pivot instruction), it calls:

```sh
agent-msg send <agent-id> "<message text>"
```

That appends the message to the subagent's inbox file at
`~/.config/claude/agent-inbox/<agent-id>.json` and registers a
**gate-mode obligation** scoped to that agent. The subagent's next
non-exempt tool call is HARD-DENIED by the existing
`pre-tool-obligations-gate-hook` (already wired by the entrypoint),
with the message body in the deny banner.

**As a subagent, when you see a deny banner that includes the message
text, run:**

```sh
agent-msg inbox <agent-id> --all   # read the message (always exempt)
agent-msg ack <agent-id>           # flip every unread message to read
```

After `ack` the inbox is empty, the gate stops firing, and your next
tool call goes through. Message bodies persist on disk so you can
re-read them later via `agent-msg inbox --all`.

Subcommand surface:

```
agent-msg list                    # show currently tracked agents
agent-msg show <id>               # metadata for one agent
agent-msg arm <id>                # main-loop-only: register inbox gate
agent-msg disarm <id>             # main-loop-only: tear down gate
agent-msg send <id> <text>        # main-loop-only: deliver a message
agent-msg inbox <id>              # read inbox (default: unread only)
agent-msg ack <id>                # subagent-side: clear unread
agent-msg gc <id>                 # drop read messages older than TTL
agent-msg gc-dead                 # sweep obligations for dead agents
```

`agent-msg ack | inbox | gc | disarm | list | status | show` is on the
exempt list of every gate (inbox gate itself, alert gate, dispatch
gate) so the subagent can always reach its own inbox. `send` and
`arm` are NOT exempt — those are main-loop operations.

The `pre-tool-obligations-gate-hook` and the `obligations` CLI it
shells out to are baked at `/usr/local/bin/`, so the inbox gate
operates even in stripped-down `docker run` containers without
`~/repos/claude-watch` bind-mounted.

### Channel 2: Claude Code's built-in agent-chat curses UI — user -> subagent

The second channel is **the operator typing directly to a running
subagent** via Claude Code's built-in interactive chat panel (a TUI
released May 2026; not a CLI we ship). The operator opens the chat
panel against a specific subagent and sends free-form text. That
text arrives in the subagent's context as a user message, distinct
from the original spawn prompt and distinct from `agent-msg` inbox
deliveries.

Critically: **a curses-chat message can override the main loop's
intent.** If the operator opens the chat panel and tells you to
pivot, change scope, abandon the task, or surface state, that
direction outranks the queue item / spawn prompt that brought you
here. Treat it the same way you'd treat a direct DM from the
operator on the host side. Examples:

  - Operator types "stop the PR you're working on, instead audit X"
    -> drop the PR work, audit X, return.
  - Operator types "what's your current state?" -> respond with a
    status summary (use `agent-msg send` if you also want the main
    loop to see it; but the operator's curses panel sees your normal
    return text).
  - Operator types "abandon" -> `session-task queue abandon <id>
    --reason "user-direct: abandon"` and return.

If the curses-chat direction CONFLICTS with an `agent-msg` inbox
message from the main loop, the curses-chat direction wins (it's the
operator; the main loop is an automation layer the operator
delegated to). Document the conflict in your return value so the
main loop can reconcile.

### Quick triage: which channel is this from?

  - **`PreToolUse` deny with `agent-msg/inbox:` banner**: Channel 1.
    Run `agent-msg inbox <agent-id> --all` then `agent-msg ack <id>`.
    Source: main loop.
  - **Free-form user message in your context with no `Queue item:`
    line**: Channel 2. Source: operator via curses-chat. Treat as
    direct user direction.

Both channels are SYNCHRONOUS at the boundary: you receive them, you
must process them before continuing. Don't poll your inbox between
tool calls — the gate hook surfaces messages automatically. Don't
ignore curses-chat messages — they're the operator talking to you
directly.

### Subagent transcript: `agent-tail`

Companion CLI for inspecting a running subagent's tool history.
Reads the JSONL transcript at
`~/.claude/projects/<slug>/<session>/subagents/agent-<id>.jsonl`.
The main loop uses this for visibility into a subagent's progress;
subagents themselves rarely need it (you're already inside the
transcript).

```sh
agent-tail <id>           # one-shot pretty-print
agent-tail <id> --follow  # tail -f mode
agent-tail --list         # enumerate active subagent transcripts
agent-tail <id> --json    # raw JSONL passthrough
agent-tail <id> --path    # print resolved transcript path
```

Both `agent-msg` and `agent-tail` are baked at `/usr/local/bin/` and
on PATH by default; no bind-mount required.

## Avoid `sudo` — fingerprint prompt is prohibitive

On the operator's host (typically macOS), every `sudo` invocation
triggers a Touch ID / fingerprint prompt. That's prohibitive when an
agent loop chains many short commands, so **prefer non-sudo paths in
this container whenever possible**.

The container user is uid 1000 (`hndrewaall`) and is in the right
groups (including `docker`, where applicable) so the following commands
**never need `sudo`** inside the container:

- `docker compose ...` — when docker socket is bind-mounted, the
  container user has docker-group access; bare `docker compose` works.
- `git` — repo trees are bind-mounted with the container user as
  owner; `git status`, `git diff`, `git log` etc. don't need root.
- `claude`, `claude-watch`, `claude mcp ...`, `claude-event`,
  `session-task`, `obligations`, `agent-msg`, `agent-tail` — all run
  as the container user.
- `npm`, `yarn`, `pnpm`, `node`, `cargo`, `rustc`, `python`, `pip`,
  `uv`, `go`, `make` — language toolchains run as the container user.
- `audit-hooks`, `trust-workspace` — container-baked helpers, both
  run as the container user.

If you find yourself wanting `sudo` for something that isn't on this
list (e.g. `apt install`, writing to `/etc/`, editing a system service
unit), **pause and ask the operator first**. The fingerprint prompt
makes silent retries painful, and most "I need sudo" instincts inside
the container are a sign of either a missing bind-mount or a
container-vs-host confusion that's better resolved by talking to the
operator than by working around it.

The lone documented exception is the `cw` host shim referenced in
`examples/compose/bin/cw`, which falls back to `sudo docker` only if
bare `docker ps` fails on the host. That fallback runs on the host,
not in the container, and is a one-time setup decision the operator
made about their host docker permissions — not a pattern the
container session should imitate.

## Self-update — `cwsr` rolls the inner `claude` without container restart

When Anthropic ships a new `@anthropic-ai/claude-code` version, you do
NOT need the operator to `docker compose restart` the whole container
to pick it up. Run `cwsr` (in-container; baked at
`/usr/local/bin/cwsr`) and the inner claude rolls in-place:

```sh
cwsr                    # npm install -g @latest, then respawn pane 0
cwsr --version 2.1.150  # pin a specific npm version
cwsr --no-upgrade       # respawn current claude (rare; for testing)
cwsr --upgrade-only     # install without rolling (operator can `cwsr --no-upgrade` later)
cwsr --print            # dry-run; print planned NPM + TMUX argv
```

What survives the roll: the tmux session (`claude-container:0.0`), the
wrapping container, every MCP bridge that was up, the named-volume
`~/.local/share/claude/versions/` directory, the operator's tmux
attach. What rolls: the claude process inside pane 0.

When you should run `cwsr`:
- The operator says "upgrade to latest" or asks you to pick up a
  specific version they reference.
- You see (e.g. via `claude --version`) that the in-container version
  has fallen behind a release the operator wants.

When `cwsr` is NOT the right tool:
- Container itself is down — use `docker compose up -d` (or `cw --up`
  from the host); that path installs the freshest baked version.
- You need to change `CLAUDE_AUTO_CONTINUE`, `CLAUDE_CONTAINER_REWRITE_HOOKS`,
  `CLAUDE_HOST_PROJECT_DIR`, or any other entrypoint-time env var —
  those decisions are baked at container start; cwsr only rolls the
  inner process with whatever shape entrypoint.sh already chose. Ask
  the operator to `docker compose up -d --force-recreate` for those.

The package name (`@anthropic-ai/claude-code`) and install command
(`npm install -g`) are cross-platform — same shape works whether the
host is Linux, macOS, or Windows. The in-container npm itself runs as
uid 1000 against a writable global path, no sudo needed.

## Container redeploy

To redeploy: `make redeploy` from the repo root (via host-bash).
Equivalent: `cd examples/compose && docker compose up -d --force-recreate -t 5 claude-container`

The `stop_grace_period: 5s` in docker-compose.yml ensures the old
container is killed quickly even without the explicit `-t 5` flag.

This kills the current session. The next session starts with the new
image and picks up via the resume prompt.

## What is bind-mounted from the host

The
[example compose stack](https://github.com/hndrewaall/claude-watch/blob/main/examples/compose/docker-compose.yml)
documents the standard mount surface. Defaults (operator can override
each via `CLAUDE_HOST_*` env vars):

| In-container path | Host source | Mode | Purpose |
| --- | --- | --- | --- |
| `/home/hndrewaall/.claude/` | `${HOME}/.claude/` | rw | session JSONL, project state, settings, agents/, hooks-referenced files |
| `/home/hndrewaall/.claude.json` | `${HOME}/.claude.json` | rw | top-level Claude Code config (MCP server registry, project allow-lists) |
| `/home/hndrewaall/repos/` | `${HOME}/repos/` | ro | host repo trees (read-only so the container can't scribble on working trees) |
| `/home/hndrewaall/bin/` | `${HOME}/bin/` | ro | operator-curated launcher scripts |
| `/etc/claude-code/` | host managed-settings dir if `CLAUDE_HOST_MANAGED_SETTINGS_DIR` set | ro | host MDM / enterprise policy |
| `${CLAUDE_HOST_PROJECT_DIR}` | same path on host | rw | project cwd (so the project-memory key matches the host's) |
| `${CLAUDE_HOST_HOOKS_DIR}` | same path on host | ro | corp telemetry hook scripts referenced by `~/.claude/settings.json` |

`${HOME}/repos` is **read-only**. Do not try to `git commit` from inside
the container against a path under `/home/hndrewaall/repos/`. Use
`${CLAUDE_HOST_PROJECT_DIR}` (rw) for development work, or `git push`
from the host.

## Operator-specific bind-mounts (override pattern)

The public `examples/compose/docker-compose.yml` is intentionally
**personal-paths-FREE** — it ships with the host-state surface that's
universal to any operator (`~/.claude`, `~/.claude.json`, `~/repos`,
`~/bin`, `~/claude-events`, `~/.config/session`, plus the optional
`CLAUDE_HOST_*` env-driven mounts) and nothing else. Personal paths
(`gh` token dir, `gitconfig`, `ssh-agent` socket, work-private
bare-repo paths under Google Drive / external SSDs / etc.) live in a
**gitignored** sibling file: `examples/compose/docker-compose.override.yml`.
Docker Compose auto-merges any `docker-compose.override.yml` into the
main file at `up` time, so no extra `-f` flag is needed.

The shape:

| File | Tracked? | Purpose |
| --- | --- | --- |
| `examples/compose/docker-compose.yml` | yes | Universal services + bind-mounts. Personal-paths-free. |
| `examples/compose/docker-compose.override.yml.example` | yes | Canonical template with commented-out mount blocks. Operators copy this to `.override.yml` and uncomment what applies. |
| `examples/compose/docker-compose.override.yml` | **no** (gitignored) | The operator's actual personal mounts. Generated from the template (manually, or via the `/edit-host-mounts` skill). |

**Why the override pattern instead of hardcoding?** Personal paths
differ per operator (`/Users/<you>/.config/gh` vs `/home/<you>/.config/gh`),
per host OS (Docker Desktop's magic `/run/host-services/ssh-auth.sock`
vs Linux `/run/user/<uid>/keyring/ssh`), and per work setup (work-private
repo paths leak company / project names). Baking any one operator's
shape into the public compose would either (a) leak personal paths into
a public artifact, or (b) silently mis-mount on every other operator's
host. The override file keeps the personal surface local.

### `/claude-container:edit-host-mounts` — generate / update the override

The baked skill `/claude-container:edit-host-mounts` automates the
override-file lifecycle:

1. Reads the existing override (if any) via `host-bash`.
2. Probes the host for standard candidates (`gh` token dir, gitconfig,
   ssh-agent socket, common Google Drive bare-repo paths, etc.).
3. Diffs against the existing override → proposes adds / removes / keeps.
4. Confirms with the operator before writing.
5. Writes the updated override on the host via `host-bash`.
6. Reminds the operator to `docker compose up -d --force-recreate
   claude-container` to pick up the new mounts.

Re-runnable: invoking the skill on a host that already has an override
**updates** the file (preserves comments, merges new mounts) rather than
overwriting it. Operators can also use the skill to add an ad-hoc path
(e.g. "mount `/Users/x/work/scripts` as `~/work-scripts`") without
hand-editing YAML.

**The skill needs `host-bash`.** If `claude mcp list` doesn't show
`host-bash` as Connected, tell the operator before invoking the skill —
without `host-bash`, you'd be guessing host paths blindly. Fall back to
hand-editing from the `.example` template.

**No private keys are bind-mounted.** The override pattern includes the
host's `ssh-agent` socket (forwarded via `SSH_AUTH_SOCK` env var) so
`ssh git@github.com` and `git push git@...` use the host agent for key
signing on the host side. The container never sees private key files —
that's deliberate, and `/edit-host-mounts` won't propose a private-key
mount even if asked.

**If `gh auth status` says "not logged in" inside the container**: the
override either isn't wired (no `~/.config/gh` mount) or the host's
`~/.config/gh/hosts.yml` is empty. Run `/edit-host-mounts` to wire it
up, or re-auth on the **host** (not the container — keep the credential
surface where the operator's keychain lives). The mount is RW so a host-
side `gh auth login` propagates into the container immediately.

## CLAUDE.md load order inside the container

Claude Code walks several locations at session start. In the container,
the cascade resolves like this (broadest first, narrowest last; later
files take precedence on adherence but all are concatenated into
context):

1. **Managed policy** — `/etc/claude-code/CLAUDE.md` (this file).
2. **User** — `~/.claude/CLAUDE.md` (bind-mounted from the host's
   `${HOME}/.claude/CLAUDE.md`, if present).
3. **Project** — `<cwd>/CLAUDE.md` or `<cwd>/.claude/CLAUDE.md`
   (whichever the operator's `CLAUDE_HOST_PROJECT_DIR` points at).
4. **Local** — `<cwd>/CLAUDE.local.md` (gitignored by convention).

This file (the managed-policy one) **cannot be excluded** by user or
project settings — that's by design and matches the
[Claude Code managed-CLAUDE.md contract](https://code.claude.com/docs/en/memory#deploy-organization-wide-claude-md).

## MCP servers

MCP server definitions live in `~/.claude.json` `mcpServers` on the host,
which is bind-mounted in. Claude Code's MCP discovery path is gated on
the `user` settings tier being in `--setting-sources`. When
`CLAUDE_CONTAINER_REWRITE_HOOKS=1` is set, the entrypoint drops the
`user` tier (to suppress cross-arch host hooks; see "Hooks" below) and
instead writes a project-tier `.mcp.json` inside
`${CLAUDE_HOST_PROJECT_DIR}` that mirrors the host's `mcpServers` with
each `command` wrapped in `exec-hook`. Run `/mcp` to see what loaded.

**Common bridged MCP servers**:

- **HTTP-bridge for cross-arch MCP binaries** —
  `CLAUDE_MCP_HTTP_BRIDGE=name=url:other=url` rewrites a stdio MCP
  server entry to Claude Code's native HTTP transport, so the
  in-container claude dials a host-side adapter (e.g.
  `http://host.docker.internal:8765/mcp`) instead of trying to exec a
  cross-arch binary. The host adapter is the operator's responsibility
  (`mcp-proxy`, `mcphost`, etc.); the container only rewrites the
  in-container `.mcp.json`. Full surface in
  [container/README.md](https://github.com/hndrewaall/claude-watch/blob/main/container/README.md#blast-radius).
- **`host-bash`** — generic "run a safe command on the host" MCP server,
  shipped as an off-the-shelf
  [`cli-mcp-server`](https://github.com/MladenSU/cli-mcp-server) +
  [`mcp-proxy`](https://github.com/sparfenyuk/mcp-proxy) combo with an
  env-var-driven allow-list. Default allow-list (`CW_PROFILE=corp-dev`,
  the conservative read-only set):
  `ls,cat,pwd,git,gh,head,tail,grep,find,echo`, no shell operators,
  `$HOME` boundary, 30s timeout. Trust-profile `CW_PROFILE=corp-dev-trusted`
  widens this with host-scheduling tooling (see the
  "Host-side scheduled tasks" section below). **Reach for host-bash as
  a normal tool, not a last resort** — it's the supported way to do
  host-side work from inside the container. If it's not available
  (`/mcp` doesn't list it), the operator hasn't wired up the host-side
  launcher. See
  [examples/compose/bin/mcp-host-bash](https://github.com/hndrewaall/claude-watch/tree/main/examples/compose/bin).

  **Boundary discipline**: host-bash is a *window* to the host, not
  the host. When you report what you did, frame it as "I ran X on the
  host via host-bash" — not "I ran X" (ambiguous) and not "I'm on the
  host" (false; you remain inside the container the whole time). The
  in-container claude orchestrates host-side work; the host-side
  shell executes it. Keep that distinction crisp in self-reports so
  the operator never has to guess where a command actually ran.

If `/mcp` shows "No MCP servers configured" inside the container, either
`CLAUDE_CONTAINER_REWRITE_HOOKS` is off (so user-tier MCP discovery is
suppressed by-default — the host's `mcpServers` simply don't load), or
the host's `~/.claude.json` has none defined.

## Hooks

The container ships [`exec-hook`](https://github.com/hndrewaall/claude-watch/blob/main/container/hooks-shim/exec-hook),
a safe-exec wrapper for `settings.json` hook commands whose target
binary may not be Linux-native. It inspects magic bytes, exec's ELF /
shebang-script targets transparently, and silently no-ops on Mach-O /
unknown formats with a single stderr heads-up per target per container
lifetime (so cross-arch hook references don't spam the log on every
event).

When `CLAUDE_CONTAINER_REWRITE_HOOKS=1`, the entrypoint generates a
container-local copy of `~/.claude/settings.json` with every hook command
wrapped in `exec-hook` and launches claude with
`--setting-sources project,local --settings /tmp/claude-shim/settings.json`
so the host file is never mutated.

**Realistic hook fate inside the container** (per hook event type):

| Target binary | Fate | Notes |
| --- | --- | --- |
| Linux-native ELF | exec'd transparently | Behaves identically to no shim. |
| `#!/usr/bin/env <interpreter>` shebang script | exec'd transparently | Standard scripts (Python, Bash, Node) work fine. |
| macOS Mach-O / Windows PE / unknown | silent no-op, exit 0 | One stderr line per unique target path per container lifetime. |
| Missing file | silent no-op, exit 0 | Same dedup behavior. |

**Implication for corporate telemetry hooks**: a Mac-host telemetry
binary referenced from `~/.claude/settings.json` (typical pattern: under
`~/.devbar/bin/` or similar) by default **does not fire inside the
container**. exec-hook detects the Mach-O and silently no-ops — the
alternative ("Exec format error" on every hook event) is worse. If your
team requires telemetry from container sessions, the options are:

1. Ship a Linux-amd64 build of the hook binary and bind-mount it at the
   same path the host config references. (Coordinate with the team that
   owns the hook.)
2. **Enable the host-bash bridge** (`CLAUDE_HOST_HOOK_BRIDGE=1`). When
   set, exec-hook hands every Mach-O / wrong-arch hook off to
   `exec-hook-bridge`, which marshals the call across the host-bash MCP
   server (`mcp-host-bash` at `host.docker.internal:8766/mcp` by
   default) so the REAL host binary runs with the same env + args and
   its exit code propagates back into the in-container claude session.
   The operator must also add the hook binary basename to the
   `mcp-host-bash` allow-list via `CLAUDE_HOOK_BRIDGE_BINS=telemetry-hook`
   (comma-separated for multiple). Bridge failures (host-bash
   unreachable, allow-list reject) fall back to the legacy silent-no-op
   contract — a misconfigured bridge never brings the session down.
3. Accept that in-container sessions are not telemetered into the host's
   pipeline. Coordinate with your team's privacy / observability stance.

The container does **not** carry corp telemetry pipelines into a
sandboxed Linux environment by default — that's an explicit design
choice. Make this decision with your team.

**Verifying hooks are reaching the right fate**: with
`CLAUDE_CONTAINER_REWRITE_HOOKS=1` and `verbose=true` in settings.json,
Claude Code logs each hook invocation. exec-hook writes its
"skipped non-ELF hook" heads-up to stderr on first occurrence per target
path. Tail `/tmp/exec-hook-skipped` inside the container for the list of
skipped binaries (one line per target).

## Workflow boundaries

This Claude Code session runs inside an isolated container. Its strengths
and limits:

- **Strong fit**: writing code in `${CLAUDE_HOST_PROJECT_DIR}`, talking
  to APIs the operator has bridged in (corp gateways via mcp-adaptor,
  off-the-shelf MCP servers, the Anthropic API). All TLS chains terminate
  at the in-container Node / Python; corporate-CA bundles forward
  through `NODE_EXTRA_CA_CERTS` etc. when the operator wires them up.
- **Weak fit**: anything that requires the host's full toolchain, the
  host's keychain, or commands not on the `host-bash` allow-list. Use
  `host-bash` (when available) for those — its allow-list is
  intentionally conservative.
- **Not in scope**: managing services on the host machine itself. If you
  need to restart a host daemon, edit host cron, or touch a host service,
  ask the operator on their host session; the container is a code-writing
  sandbox, not a host-administration tool.

## Semantic search — query eichi before grepping

The container has access to [eichi](https://github.com/hndrewaall/eichi),
a local sqlite-vec + sentence-transformers semantic search index. Use it
as the **default first lookup** for open-ended recall questions ("where
is X", "what did we decide about Y", "find the conversation where Z").

Decision tree:

1. **Concept-level question** (fuzzy, semantic) -> query eichi first.
2. **Exact-string question** (function name, error code, config key) ->
   `grep -r` or code search.
3. **Structured data** (metrics, timestamps, statuses) -> domain-specific
   tool (Prometheus, DB query, etc.).

If eichi returns no results or all `[distant]` scores, THEN fall back to
grep — not before.

### How to invoke

**From inside the container** (web API — the CLI venv is host-only):

```sh
curl -s "http://eichi-search:8000/api/search?q=alerting+tiers&k=5" | jq .
```

Query params: `q` (required), `k` (top-K, default 20), `source`
(filter tag), `added_since` (duration: `1d`, `7d`, `30d`), `retrieval`
(`hybrid`|`vector`|`bm25`).

**From the host** (via `host-bash`, if the CLI venv is bootstrapped):

```sh
# host-bash run_command:
eichi query "alerting tier design decisions" -k 5
eichi query "docker networking" --added-since 7d
eichi query "PR feedback" --sort added -k 10
```

### Interpreting results

Each result includes a score with a human-readable label: `[strong]` >
`[moderate]` > `[weak]` > `[distant]`. Treat `[distant]` as noise
unless the query is highly specialized. Results also carry a source tag
(`[file]`, `[obsidian]`, etc.) and a timestamp (last modified or added).

### When to re-index

The operator maintains the index via `eichi index <path>` on the host
(delta-only, idempotent). If `eichi stats` shows `last indexed at` is
stale relative to recent corpus activity, flag it to the operator —
re-indexing is a host-side operation (the container reads the index
read-only via the bind-mounted DB at `~/.local/share/eichi/index.db`).

## Quick reference for common in-container surprises

- **`claude` resumes a prior conversation**: when `CLAUDE_AUTO_CONTINUE`
  is set, the entrypoint appends `--continue <value>` to the claude
  invocation. Default is unset (bare `claude`).
- **`session-task`, `claude-event` on PATH**: only when the operator
  bind-mounts `~/repos/claude-watch` (the example compose does this).
  Missing bind mount = these two CLIs are unavailable; that's
  expected for a stripped-down `docker run`. (`obligations`,
  `agent-msg`, and `agent-tail` are baked at `/usr/local/bin/` so
  they're always available; the bind-mounted source wins on PATH
  when present for live dev iteration.)
- **Permission denied writing into `${HOME}/.local/share/claude/`**:
  the in-container claude binary's auto-update path. Backed by a named
  volume (`claude-container-versions`); should Just Work after the
  one-shot Dockerfile chown. If it doesn't, check that the named volume
  is mounted (the example compose does this) and that uid 1000 owns it.
- **`tmux` session is `claude-container:0.0`** — not `dashboard:main`
  like a typical host install. claude-watch's in-container config pins to
  this session name.

## Event response protocol — four-tier model

When `claude-event-watch` delivers events, the container classifies each
event into one of four tiers based on its `source` and `tag`. The tiers
escalate from "purely informational" to "blocking" and exist so the LLM
sees the right level of pressure for each event class.

### Tier 1 — Ambient (info-only, context-inject only)

Routine, non-actionable events: alerts that Andrew already gets push for,
cron ticks, routine queue transitions (running/done/abandoned), workload-
done, non-fatal claude-watch alerts, routine PR status (push/pending/
mergeable), etc.

  - Routed by `event-ack ingest` into `ambient-context.json`.
  - Surfaced by the `user-prompt-ambient-inject-hook` (UserPromptSubmit)
    on the NEXT user prompt as additional context.
  - **Non-blocking**. No gate. The LLM sees them, can act if anything
    stands out, otherwise just absorbs context.

### Tier 2 — Actionable (pending list + N-call gate)

Events that demand a response within a reasonable window: torrent-
completed (needs agent spawn), manual/request-fulfilled (needs requester
DM), queue/queue-api-dead (needs respawn decision), fatal claude-watch
alerts (CONTEXT CRITICALLY LOW, main pane crashed), PR CI failure /
success, workbot-prompt, queue-stale-ready, slack-unread.

  - Routed by `event-ack ingest` into `pending-actions.json`.
  - The `event_must_act` obligation evaluator counts CONSECUTIVE non-
    exempt Bash tool calls while pending. **Default N=3**: under
    threshold = ALLOW + bump counter; threshold reached = DENY.
    Override via `$EVENT_MUST_ACT_N`.
  - **Each `event-ack` transaction resets the counter to 0**, so the
    LLM gets a fresh N-call grace window after every ack.
  - The gate does NOT fire immediately on every actionable event — only
    after the LLM has missed N consecutive opportunities to triage. This
    is the q-2026-05-21-856d refinement (Andrew: "only TRULY actionable
    events go into pending, and the gate escalates after N missed calls
    rather than firing immediately").

### Tier 3 — Signal (distinct, NOT migrated)

Signal-DM inbound and signal-group inbound stay on their existing
per-thread obligation path. The `signal-wait-*` watcher records inbound
DMs, and the per-thread `signal-send` ack-gate blocks outbound until the
inbound is acked via `signal-ack`.

  - Routed by `event-ack ingest` as `excluded` (no-op).
  - **NOT migrated to the new shared event-must-act infrastructure.**
    Andrew (2026-05-21): "for now keep signal distinct even if it fits
    conceptually. too mission critical to risk on new shared infra".
  - Long-term: a separate later PR may migrate Signal once the new
    infra is proven.

### Tier 4 — Unknown (defaults to ambient)

Any event whose source/tag pair doesn't match a rule in the
`event-classify` table falls through to the default tier (ambient).
Conservative posture — unknown events become context, never block.

### Event classification table

The mapping is DATA, in `event-classify`'s `CLASSIFICATIONS` table.
Inspect with:

```sh
event-classify --list-rules
event-classify --source <src> --tag <tag> [--message <text>] --json
```

Adding a new event source = appending a row to the table. No code
change in the gate logic itself.

### Workflow

1. **Watcher fires** — `claude-event-watch` prints `EVENT[source/tag]
   message` lines and exits.
2. **Restart watcher immediately** (before processing).
3. **For each event line**, call `event-ack ingest --source <src>
   --tag <tag> --message "<msg>"`. The classifier routes it to the
   right queue automatically.
4. **For actionable events**, queue an agent / act directly / dismiss,
   then ack with `event-ack ack "<key>" --action "<what you did>"`.
   Each ack resets the N-counter.
5. **Ambient events** require no action — they'll appear in the next
   prompt's context automatically via the UserPromptSubmit hook.

### CLI reference

```sh
# Route an event through the classifier + into the correct queue.
event-ack ingest --source <src> --tag <tag> --message "<msg>"

# Pending-actions surface (actionable tier).
event-ack add "<key>" [--source "<src>"]   # Manual add (rare)
event-ack ack "<key>" --action "<text>"    # Ack -> resets N-counter
event-ack list                             # Show pending + counter
event-ack clear                            # Clear all (escape hatch)

# Counter knobs (rarely-used).
event-ack reset-counter

# Hook-internal (drains ambient queue for UserPrompt inject).
event-ack drain-ambient

# Classifier introspection.
event-classify --source <s> --tag <t> [--message <m>] [--json]
event-classify --list-rules
```

### Gate behavior (Tier 2 actionable)

- **Default-open**: missing state file, corrupt JSON, empty list,
  python unavailable — all ALLOW.
- **N-counter**: tracks CONSECUTIVE missed non-exempt Bash calls while
  pending. Reset on any `event-ack` mutation. Threshold default 3;
  configurable via `$EVENT_MUST_ACT_N`.
- **Exempt commands** (never increment counter, never blocked):
  `event-ack`, `event-classify`, `session-task queue`, `obligations`,
  `claude-watch-ack`, `claude-watch-dispatch`, `agent-msg`,
  `agent-tail`, `signal-history`, `signal-ack`, `signal-mark-read`.
- **Concurrency**: every state read-modify-write goes through `flock(2)`
  on a sidecar lockfile (`.lock` next to the state file). Two parallel
  `event-ack` invocations cannot race.
- **Scope**: main loop only (the existing obligation row scopes via
  `is_main_loop`). Subagents are not gated.
- **Override**: `obligations override "reason" --duration <N>` bypasses
  this gate (and every other) for the documented escape-hatch window.

### Signal-distinct guarantee (audit-trail)

This refactor explicitly does NOT touch any Signal code path. Verify
via grep:

```sh
grep -rE "signal[-_]" container/bin/eval-event-must-act \
                       container/bin/event-ack \
                       container/bin/event-classify \
                       container/bin/user-prompt-ambient-inject-hook
```

The matches you SHOULD see:

  - `event-classify` carries `signal-*` exclusion rules so any signal-
    tagged event is classified as `excluded` (no-op).
  - `eval-event-must-act` exempts `signal-history`, `signal-ack`,
    `signal-mark-read` so its gate never blocks Signal investigation
    when an unrelated actionable event is pending.
  - `signal-send` is NOT exempted by THIS gate (it has its own
    per-thread ack-gate elsewhere).

No code in this refactor calls into `signal-send`, modifies
`signal-wait-*`, or mutates the per-thread Signal obligation rows.

## Host-side scheduled tasks (via `host-bash`)

The container has no built-in cron / launchd / systemd — it's a
sandbox for code work, not a host automation hub. But the operator
sometimes needs periodic work that fires events the in-container claude
reacts to (e.g. "every 10 minutes, check Slack for tags addressing
me and reply if needed"). The supported pattern is:

1. **In-container claude orchestrates the schedule definition** via
   `host-bash` — edit a small script under `~/repos` (which is
   bind-mounted into the container) and use the host's native
   scheduler to fire it on a cadence.
2. **The host-side script writes a `claude-event` JSON** (or whatever
   signaling mechanism the container has wired up for inbound events)
   into a bind-mounted path. The container picks it up on its next
   pass.
3. **In-container claude reacts** to the event when it surfaces.

This requires `CW_PROFILE=corp-dev-trusted` (or an operator-specified
ALLOWED_COMMANDS override) so `host-bash` will actually exec the
scheduler binaries. The conservative default profile blocks them on
purpose — opt in.

### Host OS detection (always do this first)

The host could be **Linux** (cron, systemd user timers), **macOS**
(launchd via `launchctl`), or **Windows** (Task Scheduler via
`schtasks` / `Register-ScheduledTask`). Don't assume — probe via
host-bash before reaching for any specific scheduler:

```sh
# host-bash run_command:  uname -s
#   → "Linux"     → cron / systemd
#   → "Darwin"    → launchd
#   → "MINGW*" / "MSYS*" / "CYGWIN*" / "Windows_NT" → Task Scheduler
```

If `uname` isn't available (Windows without WSL), try
`powershell -Command "$PSVersionTable.OS"` or
`schtasks /Query /TN \\` as a probe.

### Worked example: periodic Slack tag-check

Operator wants the in-container claude to check Slack every 10 minutes
for messages tagging them and reply if needed. The orchestration:

1. **In-container claude** writes
   `~/repos/<some-host-accessible-path>/check-slack-tags.sh` — a small
   script that calls the operator's slack CLI on the host, looks for
   tags, and emits a `claude-event` if any are found.
2. **In-container claude** uses host-bash to wire that script into the
   host's scheduler. Pseudocode per host OS:

   ```sh
   # Linux (cron):
   #   echo "*/10 * * * * /home/$USER/repos/check-slack-tags.sh" \
   #     | host-bash crontab -

   # macOS (launchd, user agent):
   #   host-bash tee ~/Library/LaunchAgents/com.local.slack-tag-check.plist <<'EOF'
   #   <plist>... StartInterval 600 ... ProgramArguments slack-tag-check.sh ...</plist>
   #   EOF
   #   host-bash launchctl load -w ~/Library/LaunchAgents/com.local.slack-tag-check.plist

   # Windows (Task Scheduler):
   #   host-bash schtasks /Create /TN "ClaudeSlackTagCheck" \
   #     /TR "C:\path\to\check-slack-tags.bat" /SC MINUTE /MO 10
   ```

   (Actual scheduler argv depends on the host. The above is the
   *shape*. Use the OS probe to pick which branch to run.)
3. **The script** emits `claude-event` via the bind-mounted path that
   the in-container watcher infrastructure consumes.
4. **In-container claude** picks up the event on its next pass.

### Always document the dismantle

A scheduled job is durable on the host long after the container
session ends. Whenever you wire one, document the dismantle command in
the same conversation (so the operator can clean up):

```sh
# Linux:   host-bash crontab -l | grep -v slack-tag-check | host-bash crontab -
# macOS:   host-bash launchctl unload -w ~/Library/LaunchAgents/com.local.slack-tag-check.plist
#          host-bash rm ~/Library/LaunchAgents/com.local.slack-tag-check.plist
# Windows: host-bash schtasks /Delete /TN "ClaudeSlackTagCheck" /F
```

### Boundary reminder

Host-side schedulers are running on **the host**, not in the
container. The container is the orchestrator: it writes the
definition files (via host-bash), it consumes the resulting events,
but the cron / launchd / systemd / Task Scheduler process itself
lives outside. When reporting "I set up a recurring Slack check",
frame it as "I wrote a host-side <scheduler> job that fires every N
minutes" — not "I'm running every N minutes" (the container session
isn't; the host scheduler is).

## Where to learn more

- [Top-level claude-watch README](https://github.com/hndrewaall/claude-watch/blob/main/README.md)
- [container/ README](https://github.com/hndrewaall/claude-watch/blob/main/container/README.md) — full Dockerfile / entrypoint / blast-radius reference
- [examples/compose/ README](https://github.com/hndrewaall/claude-watch/blob/main/examples/compose/README.md) — fresh-laptop developer stack walkthrough
- [docs/watchers.md](https://github.com/hndrewaall/claude-watch/blob/main/docs/watchers.md) — operator-side hygiene rules for background tasks and watchers, including the **watcher-vs-cron decision framework** (when cron suffices, when a watcher is justified, and alternatives)
- [docs/adding-watchers.md](https://github.com/hndrewaall/claude-watch/blob/main/docs/adding-watchers.md) — authoring walkthrough for new watchers (fire-and-exit contract, host- and container-side layouts, worked Jenkins example)
- [Claude Code memory docs](https://code.claude.com/docs/en/memory) — canonical CLAUDE.md hierarchy reference
- [Claude Code hooks docs](https://code.claude.com/docs/en/hooks) — full hook event list + exit-code semantics
