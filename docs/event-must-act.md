# Event-reading enforcement (`event_must_act`)

`event_must_act` is the obligation-gate layer that ensures the main loop
actually triages [actionable claude-events](events.md) instead of letting
them pile up unread. It is an instance of the generic
[obligations gate](hooks.md) wired to a four-tier event-response model.

The infrastructure is baked into the container build (see
`container/Dockerfile`) so workbot and any other container-driven Claude
Code deployment gets it without per-host configuration. The seed row is
installed by `tools/obligations/obligations-init` on every container
entrypoint run; the evaluator script and CLIs live in `container/bin/`.

## Four-tier event-response model

When `claude-event-watch` delivers events, each event is classified into
one of four tiers based on its `source` and `tag` (see the mapping in
`event-classify`'s `CLASSIFICATIONS` table; inspect with
`event-classify --list-rules`).

### Tier 1 — Ambient (info-only, context-inject only)

Routine, non-actionable events: cron ticks, routine queue transitions
(running/done/abandoned), workload-done, non-fatal claude-watch alerts,
routine PR status (push/pending/mergeable), and any alert the operator
already receives push notification for.

- Routed by `event-ack ingest` into `ambient-context.json`.
- Surfaced by the `user-prompt-ambient-inject-hook` (UserPromptSubmit) on
  the next user prompt as additional context.
- **Non-blocking.** No gate. The LLM sees them, can act if anything
  stands out, otherwise just absorbs context.

### Tier 2 — Actionable (pending list + N-call gate)

Events that demand a response within a reasonable window: torrent-
completed (needs agent spawn), manual/request-fulfilled (needs requester
DM), queue/queue-api-dead (needs respawn decision), fatal claude-watch
alerts (CONTEXT CRITICALLY LOW, main pane crashed), PR CI failure /
success, workbot-prompt, queue-stale-ready, slack-unread.

- Routed by `event-ack ingest` into `pending-actions.json`.
- The `event_must_act` obligation evaluator counts CONSECUTIVE non-
  exempt Bash tool calls while pending. **Default N=3**: under threshold
  = ALLOW + bump counter; threshold reached = DENY.
- Override the threshold via `$EVENT_MUST_ACT_N`.
- **Each `event-ack` transaction resets the counter to 0**, so the LLM
  gets a fresh N-call grace window after every ack.
- The gate does NOT fire immediately on every actionable event — only
  after the LLM has missed N consecutive opportunities to triage. This
  prevents a single event from blocking unrelated work while still
  catching genuine ignore-the-events drift.

### Tier 3 — Signal-related (distinct, not migrated)

Signal-DM inbound and signal-group inbound stay on their existing
per-thread obligation path. The `signal-wait-*` watcher records inbound
DMs, and the per-thread `signal-send` ack-gate blocks outbound until the
inbound is acked via `signal-ack`.

- Routed by `event-ack ingest` as `excluded` (no-op for this gate).
- The Signal ack-gate is mission-critical and intentionally kept on its
  own gate path; this gate never touches it.
- `eval-event-must-act` exempts `signal-history`, `signal-ack`,
  `signal-mark-read` so its own gate never blocks Signal investigation
  when an unrelated actionable event is pending.

### Tier 4 — Unknown (defaults to ACTIONABLE — fail-LOUD)

Any event whose `(source, tag)` pair doesn't match a rule in the
`event-classify` table falls through to the default tier, which is now
**actionable** (flipped from ambient). Fail-LOUD posture — a genuinely
unknown event must be handled or get a classifier rule, never silently
swallowed as context. Every deliberately-ambient pair already has an
explicit rule above the catch-alls, so only TRULY-unmatched pairs hit
this default.

## Workflow

1. **Watcher fires** — `claude-event-watch` prints `EVENT[source/tag]
   message` lines and exits.
2. **Restart watcher immediately** (before processing).
3. **For each event line**, call:

   ```sh
   event-ack ingest --source <src> --tag <tag> --message "<msg>"
   ```

   The classifier routes it into the right queue automatically.
4. **For actionable events**, queue an agent / act directly / dismiss,
   then ack with:

   ```sh
   event-ack ack "<key>" --action "<what you did>"
   ```

   Each ack resets the N-counter.
5. **Ambient events** require no action — they appear in the next
   prompt's context automatically via the UserPromptSubmit hook.

## CLI reference

```sh
# Route an event through the classifier into the correct queue.
event-ack ingest --source <src> --tag <tag> --message "<msg>"

# Pending-actions surface (Tier 2).
event-ack add "<key>" [--source "<src>"]   # Manual add (rare)
event-ack ack "<key>" --action "<text>"    # Ack -> resets N-counter
event-ack list                             # Show pending + counter
event-ack clear                            # Clear all (escape hatch)

# Counter knobs (rarely used).
event-ack reset-counter

# Hook-internal (drains ambient queue for UserPrompt inject).
event-ack drain-ambient

# Classifier introspection.
event-classify --source <s> --tag <t> [--message <m>] [--json]
event-classify --list-rules
```

## Gate behavior (Tier 2 actionable)

- **Default-open**: missing state file, corrupt JSON, empty pending
  list, python unavailable — all ALLOW. The gate's failure mode is
  permissive, never restrictive.
- **N-counter**: tracks CONSECUTIVE missed non-exempt Bash calls while
  pending is non-empty. Reset on any `event-ack` mutation. Threshold
  default 3; configurable via `$EVENT_MUST_ACT_N`.
- **Exempt commands** (never increment counter, never blocked):
  `event-ack`, `event-classify`, `session-task queue`, `obligations`,
  `claude-watch-ack`, `claude-watch-dispatch`, `agent-msg`,
  `agent-tail`, `signal-history`, `signal-ack`, `signal-mark-read`.
- **Concurrency**: every state read-modify-write goes through `flock(2)`
  on a sidecar lockfile (`.lock` next to the state file). Two parallel
  `event-ack` invocations cannot race.
- **Scope**: main loop only (the seeded obligation row uses
  `is_main_loop` as a scope guard). Subagents are not gated.
- **Override**: `obligations override "<reason>" --duration <N>`
  bypasses this gate (and every other) for the documented escape-hatch
  window.

## Deploying and verifying

The seed row, evaluator, classifier, and ack CLI are all baked into the
container image:

- `tools/obligations/obligations-init` registers the `event_must_act`
  obligation on every entrypoint run (idempotent — already-seeded rows
  are detected by a marker tag in `deny_message`).
- `tools/event-must-act/eval-event-must-act` is the evaluator script the
  obligation row points at (`/usr/local/bin/eval-event-must-act`).
- `tools/event-must-act/event-classify` and `tools/event-must-act/event-ack` are the
  classifier + ack CLI. Both are copied to `/usr/local/bin/` by the
  Dockerfile.
- `tools/event-must-act/user-prompt-ambient-inject-hook` drains the ambient
  queue on every `UserPromptSubmit`.

To pick up changes to any of the above on workbot, rebuild and
redeploy the container:

```sh
cd ~/repos/claude-watch
git pull
make container-build         # rebuild image
make compose-up              # bounce the running container
```

Smoke-test the gate after a redeploy:

```sh
# Inside the container (or via docker exec):

# 1. Confirm the obligation is seeded.
obligations list | grep -A2 event_must_act

# 2. Confirm the evaluator + CLIs are on PATH.
which eval-event-must-act event-ack event-classify

# 3. Inject a synthetic actionable event and watch the counter.
event-classify --source manual --tag workbot-prompt --json
event-ack ingest --source manual --tag workbot-prompt \
    --message "smoke test"
event-ack list                   # pending should show the entry

# 4. Run a few non-exempt Bash calls to bump the counter.
ls; ls; ls                       # threshold-1, threshold-2, threshold-3
ls                               # this should DENY with the gate banner

# 5. Ack and confirm the gate releases.
event-ack ack "<key>" --action "smoke test complete"
ls                               # should ALLOW again

# 6. Final cleanup.
event-ack clear
```

## Tests

```
make test-hooks
# Includes pre-tool-obligations-gate-hook tests; the gate fires through
# the same obligations cascade that event_must_act uses.

# Container baked-wiring assertions:
container/tests/event-must-act-wired.test
container/tests/baked-obligations-hooks.test
```

## Where the rules live (single source of truth)

- **Tier mapping**: `tools/event-must-act/event-classify` (`CLASSIFICATIONS`
  table). Add a new event source = append a row; no gate-logic change.
- **Gate behavior**: `tools/event-must-act/eval-event-must-act` (counter,
  exempts, default-open posture).
- **Pending / counter state**:
  `~/.config/claude-events/pending-actions.json` and
  `~/.config/claude-events/tool-call-counter.json`. Both flock-guarded.
- **Ambient queue**: `~/.config/claude-events/ambient-context.json`,
  drained by `user-prompt-ambient-inject-hook`.
- **Seed row**: `tools/obligations/obligations-init`
  (`EVENT_MUST_ACT_TAG`).

If something looks wrong — gate firing when it shouldn't, not firing
when it should, an event being classified into the wrong tier — start
at the relevant single-source file above. None of the behavior is
spread across multiple files; every knob has exactly one home.
