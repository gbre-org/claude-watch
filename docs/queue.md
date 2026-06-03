# session-task queue + resume action

`session-task` provides two cross-session task-coordination layers:

- **Layer 1 — resume action**: a single "top-of-mind" slot for the next
  resume after `/clear`.
- **Layer 2 — work queue**: any number of items, grouped by overlapping
  scope, running one-at-a-time within a group and in parallel across
  disjoint groups.

A third layer (background processes) is handled by `claude-watch
active-agents` and `claude-watch task` (separate from this CLI).

## Layer 1 — resume action

One slot. Written before `/clear` / `self-clear` / exit so the next session
picks up where the previous one left off. Lives in
`~/.config/session/resume-action.json`.

```
session-task set "<text>"        # store the slot
session-task get | show          # read it back
session-task append "<more>"     # add to existing
session-task clear               # mark as completed (logged)
session-task complete "<text>"   # one-shot: log + clear
session-task history             # past completions
```

## Layer 2 — work queue with scope groups

Items have a `--scope <token>...` list. Two items "overlap" iff any pair of
their scope tokens overlap. Overlapping items end up in the same group;
within a group they run one-at-a-time (priority, FIFO tiebreak). Disjoint
groups run in parallel.

State: `~/.config/session/queue.json` (fcntl.flock-protected).

### Scope tokens

| Token | Match | Meaning |
|-------|-------|---------|
| `file:<path>` | prefix | "this file" (or directory tree if you suffix `**`) |
| `repo:<name>` | exact | "this repo" |
| `resource:<name>` | exact | named lockable resource (e.g. a single backend) |
| `book:<name>` | exact | named book (used by ebook pipelines) |
| `agent-proto:<name>` | exact | named agent prompt / sub-skill |
| `*` | universal | overlaps with everything — use sparingly |

### Subcommand surface

```
session-task queue add "..." --scope <s> [--summary "..."] [--priority N]
session-task queue list [--ready] [--running] [--blocked]
session-task queue show <id>
session-task queue scope <id>             # show effective scope
session-task queue groups                 # show group membership
session-task queue ready                  # which items can run now
session-task queue pop [--id <id>]        # mark next/specific as running
session-task queue spawn-check <id>       # rc=0 if clear, rc=2 if blocked
session-task queue register <id>          # atomic ready→running
session-task queue done <id>              # mark completed
session-task queue abandon <id> [--reason R]
session-task queue promote <id>           # raise priority
session-task queue set-summary <id> "..."
session-task queue prune                  # drop completed/abandoned
session-task queue banner                 # one-line top-of-resume hint
session-task queue migrate                # one-shot v1→v2 migration
```

### Mandatory spawn workflow

Before the main loop fires ANY `Agent` tool call:

1. `session-task queue add "..." --scope <s> --summary "~10 word headline"` —
   get the queue item id. Scope overlap with a running peer SOFT-SERIALIZES
   (exit 0, `ready_now=false`, `serialized_after` records the running peer).
   **Exit 3 = HARD REFUSED** is reserved for `--scope workload:<label>` —
   the `workload run <label>` runner auto-creates its own queue item with
   that scope, so manual `workload:` queueing produces double queue rows
   tracking one tmux pane. Use `workload run <label>` instead. Bypass:
   `--force-enqueue` flag (the runner itself passes this) or
   `QUEUE_GATE_BYPASS=1` env var.
2. Read `ready_now` and `spawn_instruction` from the returned JSON.
3. If `ready_now=true`: `session-task queue register <id>` (or
   `pop --id <id>`) to atomically mark it running.
4. **Include `Queue item: q-XXXX` in the Agent prompt.** The
   `pre-agent-queue-gate-hook` PreToolUse hook DENIES the spawn if the
   marker is missing or the id isn't `running`.
5. ONLY THEN fire the Agent tool.
6. On agent completion: `session-task queue done <id>` (or
   `abandon <id> --reason R` if it failed).

If `ready_now=false`: do NOT fire the Agent. Wait for the blocking items in
`serialized_after` to finish. When a blocker's `queue done` lands, re-check
with `session-task queue spawn-check <id>` (exit 0 = ok, exit 2 = still
blocked) — only when it exits 0 may you `register` and spawn.

Emergency bypass: `QUEUE_GATE_BYPASS=1` env var (audited to
`~/.config/claude/queue-gate-bypass.log`).

### Other rules

- **Never append to ad-hoc todo files** — use `queue add`. The whole point
  of the queue is structured scope serialization across sessions.
- When an agent declares scope, it may only WIDEN — never narrow the
  main-loop's pre-declared scope.
- No cross-group preemption: a higher-priority item in a different group
  does NOT kill anything.
- `queue add` JSON output includes `spawn_instruction`:
  `"READY: register-and-spawn (...)"` or `"BLOCKED: do not spawn, wait
  for ..."` — read it, don't guess.

### Waiting on a long workload — use `workload babysit`, not tight-poll

When an agent or the main loop has kicked off a long `workload run <label>`
(media-promote, rsync, ffmpeg, a remux) and needs to wait for it to finish,
**block in-process with `workload babysit` — do NOT loop `workload list` /
`workload log` across separate LLM turns.** Repeated polling burns thousands
of tokens per turn for no progress; that's exactly the failure mode babysit
exists to fix.

```
workload babysit <label> --qid q-XXXX [--heartbeat 60] [--max-block 540] [--poll 15]
```

- Blocks **in-process** waiting for `<label>` to finish — zero LLM turns
  while it waits.
- Pats the bound queue item's heartbeat (`session-task queue heartbeat
  <qid>`) every `--heartbeat` seconds (default 60) so `last_heartbeat_at`
  stays fresh and the item is never mistaken for orphaned/stuck.
- **Returns 0** once the workload reaches `done (exit N)` (the workload's
  own exit code is also propagated as the process exit code).
- **Returns 75** (EX_TEMPFAIL) at `--max-block` seconds (default 540, kept
  under the Bash 600 s cap) if the workload is still running, printing
  `still-running ... — rerun to keep waiting`.

**Intended pattern**: call `workload babysit`, and on **exit 75 re-invoke it**
to keep waiting. Each re-invocation is the only LLM-turn cost of the whole
wait (≈ once per `--max-block`), versus a fresh turn per poll. Exit 1 = no
such label; exit 2 = malformed `--qid` (must look like `q-XXXX`).

## Schema (v2)

```json
{
  "schema_version": 2,
  "items": [
    {
      "id":           "q-YYYY-MM-DD-XXXX",
      "description":  "...",
      "summary":      "~10 word headline",
      "scope":        ["repo:foo", "file:src/bar.py"],
      "group_id":     "g-...",
      "group_head":   "q-...",
      "status":       "pending|running|completed|abandoned",
      "priority":     0,
      "created_at":   "ISO8601",
      "created_by":   "...",
      "started_at":   "ISO8601 | null",
      "registered_at":"ISO8601 | null",
      "completed_at": "ISO8601 | null",
      "abandoned_at": "ISO8601 | null",
      "abandon_reason":"... | null",
      "pid":          12345,
      "last_heartbeat_at": "ISO8601 | null",
      "context":      {...}
    }
  ]
}
```

## When to use which layer

| Need | Layer |
|------|-------|
| "Do X after `/clear`; I will be mid-thought" | 1 (`set`) |
| "Queue up a follow-up that conflicts with something currently in-flight" | 2 (`queue add`) |
| "Track a background process I just spawned" | (handled by `claude-watch active-agents` / `claude-watch task` — separate) |

## Tests

```
make test-session-task         # ~52 cases via pytest
make test-hooks                # exercises the queue gate end-to-end
```
