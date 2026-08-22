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
one of four tiers. **The producer is the source of truth**: an event
source ships its own tier in the event's `data.tier`, and that decision is
NOT duplicated in `event-classify`'s `CLASSIFICATIONS` table or in local
config. The table is the FALLBACK for producers that ship nothing, plus a
place for genuine consumer policy.

### Classification precedence

| # | Rung | Where it lives |
|---|------|----------------|
| 0 | EXCLUDED consumer policy — **absolute** | `CLASSIFICATIONS` (`signal/*` only) |
| 1 | **User-side override** | `~/.config/claude-events/tier-overrides.json` |
| 2 | **Producer-shipped tier** | the event's own `data.tier` |
| 3 | `CLASSIFICATIONS` table — fallback | `event-classify` |
| 4 | fail-LOUD default (`actionable`, marked `UNCLASSIFIED`) | `event-classify` |

Inspect every rung with `event-classify --list-rules`.

**Adding a new event source?** Prefer rung 2 — stamp `data.tier` on the
event in the producer. Only add a `CLASSIFICATIONS` row when you do not
control the producer. Duplicating a producer's decision in the table is a
two-sources-of-truth bug.

#### Producer-shipped tier (rung 2)

A producer sets `data.tier` to `actionable` / `ambient` / `excluded`:

```bash
claude-event "PR #637: CI still RED" --tag pr-ci-red --source cron \
    --data tier=actionable
```

`claude-event-watch` reads `data.tier` off the event and passes it to
`event-ack ingest --tier`, which hands it to `event-classify`. An invalid
value (a typo, or a severity word like `high` — that is the separate
top-level `priority` field) is IGNORED, and the event falls through to the
table rather than being mis-routed.

#### User-side overrides (rung 1)

The operator can re-tier any event **without editing the producer and
without editing `event-classify`**. Overrides outrank the producer.

The file is OPTIONAL and never auto-created; absent means no overrides.
Path: `$EVENT_CLASSIFY_OVERRIDES`, else
`~/.config/claude-events/tier-overrides.json`.

```json
{"overrides": [
  {"source": "cron", "tag": "pr-ci-red", "tier": "ambient",
   "reason": "I watch PRs myself; the nudge is noise"},
  {"source": "*", "tag": "pr-status-change", "contains": "merged",
   "tier": "ambient"}
]}
```

- `source` / `tag` — exact, `*`, or `prefix*`. Default `*`.
- `contains` — OPTIONAL message substring narrowing the rule. Data, not
  code: operator config never executes a predicate.
- `tier` — `actionable` | `ambient` | `excluded` (required).
- First match wins. A malformed file is ignored **wholesale** (default-open
  — a typo must never break routing or half-apply); `--list-rules` prints
  the parse error.

An override can move anything **to** `excluded`, but cannot pull a
`signal/*` event **out** of it: rung 0 stays absolute.

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

An event whose `(source, tag)` pair matches no rule in the
`event-classify` table **and** whose producer shipped no `data.tier`
falls through to the default tier, which is **actionable**. Fail-LOUD
posture — a genuinely unknown event must be handled or get a
classification, never silently swallowed as context. Every
deliberately-ambient pair already has an explicit rule above the
catch-alls, so only TRULY-unmatched pairs hit this default.

#### The "add a rule" signal

A fall-through is a **missing classification**, not a mystery gate, so it
is surfaced as one. Such an event is marked `unclassified` in
`pending-actions.json` and the `event-must-act` deny banner renders it
distinctly:

```
Pending events:
  - torrent-completed:Some.Release.mkv
  - novel-tag:something new  [UNCLASSIFIED]

NOTE: 1 of the above are [UNCLASSIFIED] -- they matched no rule in
event-classify's CLASSIFICATIONS table AND their producer shipped no
`data.tier`, so they hit the fail-loud ACTIONABLE default. Acking clears
the event, NOT the cause -- the next one lands here too. Fix the
classification (in preference order):
  1. PREFERRED - have the PRODUCER stamp `data.tier=actionable|ambient`
     on the event. Classification belongs with the source, not here.
  2. Add a rule to CLASSIFICATIONS in tools/event-must-act/event-classify.
  3. Add a user-side override (outranks the producer):
     ~/.config/claude-events/tier-overrides.json

  Inspect the current rules: event-classify --list-rules
  Unclassified (source/tag): novel-source/novel-tag
```

This is **observability only** — the fail-loud ACTIONABLE default is
deliberate and unchanged. Acking an unclassified event clears that event
but not the cause; the next one lands in the same place until the
classification is fixed.

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

### Non-container (systemd host) install

The four scripts above are deployment-agnostic — all their state lives
under `~/.config/claude-events/` (override with `$CLAUDE_EVENT_STATE_DIR`)
— so a host deployment uses the exact same copies rather than a fork.
`make install` symlinks them (plus `obligations-init`) into `$BIN_DIR`
(default `~/bin`).

Two things differ from the container and both are easy to get silently
wrong:

1. **The evaluator path.** The seeded row stores an absolute `cmd`. On a
   host that is `$BIN_DIR/eval-event-must-act`, not the baked
   `/usr/local/bin/...`. Export `CW_EVAL_BIN_DIR` before seeding.
   Getting this wrong fails **open and silently**: the `evaluator`
   predicate allows on spawn error, so the row exists, `obligations list`
   shows it, and it enforces nothing.
2. **Seed one row, not all of them.** Bare `obligations-init` seeds every
   default row — right for a fresh container, wrong for switching on one
   gate on a host that is already running, where the other rows become
   live gates on the next tool call. Use `--only`.

```sh
make install                                   # symlinks into ~/bin
export CW_EVAL_BIN_DIR="$HOME/bin"
obligations-init --only event_must_act -n      # inspect the exact add
obligations-init --only event_must_act -v      # then seed it
```

Verify the row actually points somewhere real, rather than trusting that
it was seeded:

```sh
obligations list --json \
  | python3 -c 'import json,sys,os;
rows=[o for o in json.load(sys.stdin)["obligations"]
      if o.get("deny_message")=="[default-seed] event_must_act"]
print(rows and os.access(
    rows[0]["predicate_params"]["predicates"][1]["params"]["cmd"], os.X_OK))'
```

`claude-event-watch` auto-ingests every delivered event through
`event-ack ingest` (disable with `CLAUDE_EVENT_WATCH_AUTO_INGEST=0`), and
it resolves the CLI by `command -v` at startup — so ingestion begins on
the first watcher run after `event-ack` lands on `PATH`, which is
typically **before** you seed the row. Check `event-ack list` (and
`event-ack clear` if a backlog accumulated) immediately before seeding,
or the gate denies on its very first evaluation.

Note that `keepalive` is classified **actionable**, so on a host that emits
it the gate is what forces the loop to clear the pending entry. The
clear-path is the same one used for every batch:

```sh
event-ack ack-batch
```

One bare command: it acks every pending entry, resets the N-counter, and
stamps `last-ack-timestamp` -- whose age is claude-watch's liveness signal
(`[ack] stale_minutes`). `event-ack` is exempt in this evaluator (and in
`pre-tool-dispatch-gate-hook`), and `event-ack ack-batch` specifically is
hardcoded-ALLOWed in `pre-tool-obligations-gate-hook`, so the command that
discharges the event can never be the command a gate denies.

Two things this replaced, both retired: a `heartbeat-ack` wrapper (gone
2026-08-21, once a plain ack was enough) and a `touch
/var/run/claude/heartbeat` fallback (gone 2026-08-22 with the file itself).
The per-key `event-ack ack "<key>" --action "..."` still exists for the case
where you handled part of a batch and want the rest left pending.

### Container redeploy

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
