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
session-task queue abandon <id> [--reason R] [--confirmed-dead [--force]]
session-task queue release <id> [--reason R] [--force]  # quarantine -> abandoned
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

### Abandoning a live scope: quarantine

`queue abandon` does **not** free the scope of an item that currently owns
one. A `running` / `wedged` item moves to **`quarantined`**: not terminal,
and still holding its scope lock so no replacement can register on it. A
`pending` or `blocked` item still goes straight to `abandoned` — neither can
have a live agent behind it.

Why: abandoning is decided by *inference* — no output file, stale mtime, "no
child process", it has been quiet too long. None of those observe the process
exiting. An agent that was presumed dead and abandoned kept running for
another ~48 minutes; because abandon freed the scope, a replacement was
spawned for the same work, both finished, and a duplicate reached a user. The
scope lock existed to prevent exactly that, and a guess dissolved it. Silence
is not death.

`queue list` renders a quarantined item with a `?` head marker (we do not
know if the agent is alive), the reason, and the release commands inline.

**Ending a quarantine** — three ways, in descending order of evidence:

| Command | Meaning | Outcome |
|---------|---------|---------|
| `queue done <id>` | the agent came back and finished | `done`; quarantine cleared, stamped `quarantine_released_by=agent-completed` |
| `queue resurrect <id>` | respawn a replacement | old row `abandoned`, new row carries the same scope — never unlocked in between |
| `queue release <id> --reason ...` | operator asserts the process is gone | `abandoned`, scope freed |

`resurrect` is the respawn path for an agent that genuinely died, and it
accepts a quarantined item directly — a real death costs no extra step and
never needs a maintainer to unwedge anything.

`queue abandon --confirmed-dead` skips quarantine for callers holding
**positive** evidence of exit (an exit code, a reaped child). The workload
reaper passes it because it reads the wrapped command's rc. Do not pass it
because an agent *looks* dead.

Both `release` and `--confirmed-dead` consult claude-watch's active-agents
state, but only in the sound direction: a **live** agent record refuses the
release. The **absence** of a record authorizes nothing — that is the same
weak inference the quarantine exists to stop trusting. `--force` overrides
the refusal when you know the state file is wrong.

There is deliberately **no auto-expiry timer**. A TTL is another inference
that the agent is dead, and inference is the bug being fixed — one that fires
while the agent is alive reproduces the incident, just later.

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

### Killing a workload — `workload kill` is TREE-WIDE

```
workload kill <label> [--grace SECS]
```

`workload kill` is **not** `tmux kill-pane`. It terminates the whole process
tree the workload created:

1. **Snapshot first.** Every descendant is read out of `/proc` *before* any
   signal is sent — the first kill reparents the rest to pid 1, so a walk
   done afterwards loses exactly the grandchildren you are trying to reap.
2. **Signal the process GROUP**, not just the pids. `workload run` launches
   its payload under `setsid` and records the resulting process-group id in
   `<label>.pgid` alongside the other workload artifacts; the killer signals
   that group, which also sweeps up anything the tree forked mid-teardown and
   anything that **double-forked** out of the ppid chain but kept the group.
3. **SIGTERM, then SIGKILL** after the grace period — `--grace SECS`,
   env `WORKLOAD_KILL_GRACE_SECS`, default 5 s, capped at 600 s. A driver
   script gets a chance to stop its own children cleanly first.
4. **Verify and report.** Survivors are re-checked after the SIGKILL sweep
   and printed by pid; a zombie counts as dead.

The pane still closes and shows the KILLED status, and the `workload-done`
event still carries `killed=true` exactly once — unchanged.

Exit codes: `0` killed (or already dead), `1` no such label, **`3` something
survived SIGKILL** (it names the pids; inspect with
`ps -o pid,ppid,pgid,stat,args -p <pid>`).

> Why it works this way: killing only the pane used to leave the payload
> running. During a CPU-temperature alert a render workload was killed, its
> driver script kept going under the reparented session, and eleven render
> processes had to be `pkill`ed by hand.

#### Destroying the whole session: `task init --recreate --force`

```
claude-watch task init --recreate --force [--grace SECS]
```

`--recreate` tears the `tasks` tmux session down and rebuilds it, so it
destroys every workload pane at once. `--force` is required while any
workload is running, and the labels it is about to kill are named on stdout
first.

**It runs the same tree-wide teardown, once per running workload, before it
destroys the session.** A bare `tmux kill-session` only hangs up the pane
*shells* — it has exactly the blind spot `workload kill` used to have, and at
N-workloads scale: every payload sits two sessions below its pane (`setsid`,
then `script`), so each one was reparented to pid 1 and kept running after the
session was gone, still burning CPU, still appending to a `.output` file
nothing was tailing, with no `workload-done` event ever emitted and its queue
item stuck in `running` forever.

So a recreate now, for each workload whose pane is still alive:

1. emits its `workload-done` with `killed=true` — **exactly once** — and
   transitions its queue item to `abandoned`, before touching any process;
2. runs the identical snapshot -> SIGTERM (pids + pgids) -> grace ->
   re-snapshot -> SIGKILL -> verify sequence described above;
3. closes the pane and drops the label from the registry.

`--grace SECS` is the same budget as `workload kill --grace` and honours the
same `WORKLOAD_KILL_GRACE_SECS` env override; it applies to each workload's
teardown. Survivors are named on stderr and **`task init` exits `3`** — as
does a workload that vanished from the registry mid-teardown, i.e. anything
the recreate cannot account for. The session is recreated either way; what
exit `3` refuses is calling that outcome clean, because "recreated" must not
be allowed to mean "and something is still running". Exit `1` is the
pre-existing refusal when workloads are running and `--force` was not passed.

> Exactly-once, concretely: the killer drops a `<label>.kill-emitted`
> sentinel next to the other workload artifacts when it emits. The wrapper
> returns from `setsid --wait` the instant its payload dies and can reach its
> own `workload emit-done` before the pane is destroyed; that call consumes
> the sentinel and declines to emit, so one killed run produces one
> completion rather than a `killed=true` event followed by a `killed=false`
> one. `workload run` clears the sentinel when the label starts again.

#### Re-running a live label: `workload run` REPLACES, tree-wide

```
workload run <label> -- <cmd>      # while <label> is already running
```

`workload run` on a label whose previous run is still going **replaces**
it — that has always been the behaviour, and it still is. What changed is
that the previous run is now actually torn down: **the same tree-wide
teardown described above**, not a bare `tmux kill-pane`. Killing only the
pane left the payload (two sessions down, `setsid` then `script`)
reparented to pid 1 and *still running* — appending to the very
`<label>.output` the new run was about to publish under, and clobbering
the `<label>.pgid` sidecar the next `workload kill` would aim at. Two
payloads, one label, and the older one invisible.

A replace must not masquerade as the run that replaced it: it is a
`killed=true` completion on the same label with the same log path,
arriving moments before the new run starts. Three rules keep them apart:

1. **The replaced run's completion is marked.** It gets its usual
   exactly-once `workload-done` (`killed=true`, `exit_code=-15`) plus:

   | field | meaning |
   |-------|---------|
   | `data.reason` | `"replaced"` — this run was displaced, not killed by an operator |
   | `data.replaced_by` | the `started_at` of the run that took the label over, byte-identical to the new registry entry's, so the two line up exactly |
   | `data.carried_over_queue_id` | present only in case 2 below |

   The message reads `workload <label> replaced (previous run killed
   rc=-15, new run started <ts>, log=...)`. `tag` and `source` are
   unchanged, so every existing consumer keeps working.

2. **The new run's queue binding happens only after the teardown
   returns.** Auto-create + `register` used to run *first*, which left a
   window where the item the teardown was about to abandon was the item
   the new run had just claimed. Now:
   - **auto-created / different qid** → the replaced run's own item is
     abandoned (`--confirmed-dead`, reason *"workload X replaced by a new
     run started ..."*), and the new run's item is created afterwards;
   - **same `--queue-id` as the dying run** → the item is **carried
     over**: left `running` for the new run, reported as
     `data.carried_over_queue_id`, and deliberately NOT named in
     `data.queue_id` so nothing correlating on that field reads live work
     as dead.

3. **Survivors refuse the new run.** If anything outlives the SIGKILL
   sweep, `workload run` names the pids and exits **`3`** without
   starting — running anyway would put two payloads on one label. Reap
   them (`ps -o pid,ppid,pgid,stat,args -p <pid>`) and re-run.

The teardown uses the standard grace (`WORKLOAD_KILL_GRACE_SECS`, default
5 s); `workload run` has no `--grace` flag of its own. The sidecar reset
(`.exit`, `.output`, heartbeats, `.pgid`, `.kill-emitted`) also happens
*after* the teardown, so the kill can still read the `.pgid` it needs and
the new run still starts on a clean slate — including a cleared
`kill-emitted` sentinel, so the replaced run's exactly-once guard cannot
swallow the NEW run's completion.

> To kill a label without starting anything in its place, use `workload
> kill <label>` — the replace path exists for *re-running*, and it always
> ends with either a new run or exit `3`.

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
