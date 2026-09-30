# queue-minisite

Mobile-friendly Flask UI for the `session-task` work queue that
`claude-watch` ships. Renders the queue from `queue.json`, surfaces
running/pending/blocked items, and exposes Stop / Abandon / Force-start
buttons that mutate the queue via a host-mounted copy of
`session-task`.

Designed to sit BEHIND an upstream auth proxy (oauth2-proxy, nginx
`auth_request`, or similar). The app itself does NOT enforce access
control — it trusts the `X-Auth-Request-Email` header for display only.
Do not expose it to the public internet without a gate.

## Status sections (and why none may be silently dropped)

Items are bucketed into sections from a single table, `STATUS_SECTION` in
`app.py`. Section order is RUNNING → WEDGED → QUARANTINED → PENDING →
BLOCKED → OTHER → DONE → ABANDONED.

`WEDGED` and `QUARANTINED` sit directly under RUNNING because they are
in-flight items that **still hold their scope**: a pending peer in the same
scope cannot start until one of them ends.

* **wedged** — was running, the owning agent is stuck. Card shows the wedge
  reason and the two ways out (`queue unwedge`, `queue abandon`).
* **quarantined** — `queue abandon` was called on a scope-owning item without
  positive evidence the process is gone, so the scope stays locked and the
  item waits on a human. Card shows the quarantine reason, the fact that the
  scope is still held, and the three exits in descending order of evidence
  (`queue done`, `queue resurrect`, `queue release --reason ...`).

The exits are shown as copyable commands rather than one-click buttons on
purpose: each is an assertion about whether a process is still alive, and that
judgement is exactly what the quarantine state exists to stop the system from
making on an inference.

**OTHER is the structural guarantee.** Bucketing previously used a hardcoded
if/elif chain with no `else`, so any status it didn't name was dropped —
no row, no count, no log line. `wedged` and `quarantined` were both invisible
that way. Anything whose status has no declared section (including a missing
or null status) now lands in OTHER, which renders the raw status verbatim and
logs a one-time warning, so a status added to `session-task` tomorrow shows up
immediately instead of disappearing. Giving it a first-class section means
adding it to `STATUS_SECTION` plus a section in `templates/index.html` and
`static/refresh.js` — an upgrade, never a prerequisite for visibility.

Every section must exist in **both** renderers. The 5s morphdom refresh
rebuilds `#queue-root` from `static/refresh.js`, so a section present only in
the Jinja template flashes on first paint and vanishes on the first tick.
`test_status_sections.py` and `test_foldable_sections.py` pin that parity.

## Done view (archive union)

The **Done** section does NOT source solely from the `done` items still
resident in `queue.json`. It UNIONs those live items with the persistent
append-only completed-tasks archive (`completed-tasks.jsonl` — the record
`session-task` writes on every queue done/abandon), deduped by queue id
(the live `queue.json` entry wins over its archive echo). This makes the
view **reset-proof**: a `queue.json` corruption/reset wipes the live done
tail, but the historical record survives in the archive and keeps
rendering. Only DONE rows are pulled from the archive (abandon / merge /
block / … lifecycle rows are dropped). The rendered card list is capped at
`RECENT_DONE_LIMIT` (newest first); the section header's `N / M` count
reports `M` as the full union total. The archive path defaults to a
sibling of `QUEUE_JSON` (`COMPLETED_TASKS_JSONL` overrides it) and is
parsed once per file change (cached on mtime/size), so the growing archive
adds no per-request cost.

## Agent activity counters (tool calls + tokens)

Every RUNNING row carries a live cell in its item head — `11 calls · 82K tok`
in comfortable density, `11·82Kt` in compact (the head is the one line
compact never elides, so the counters stay visible there too; hover for
output tokens / last tool / last-write age) — and the header shows the
session totals as ONE outlined rounded pill in the TOP half-row, `● N agents
· C calls · K tok`, right-aligned within the stack so its right edge lines up
with the last status pill's; the status pills (running / blocked / pending)
sit in the row below, left-aligned. The pill is the botchat topbar agent-bar look: a live dot,
info-blue while at least one agent is live (`.active`), muted when none
(`.idle`), dashed with an amber dot and `n/a` numerals when the snapshot is
stale (`.stale`); under 480px the units collapse to `a` / `c` / `t`.

The agent COUNT is always "live right now". The other two numerals are the
live sums only while something IS running: with no live agent those sums are
structurally 0, so instead of `0 agents · 0 calls · 0 tok` — which reads as a
broken sensor rather than an idle minute — the pill shows the last window's
tool calls and the MAIN loop's context, tagged `main`
(`0 agents · 37 calls · main 157K tok`). The server picks that
(`pill_calls_text` / `pill_tok_text` / `pill_tok_pre`); `calls_text` /
`tok_text` stay the live sums for the popover and API consumers.

Click (pin) or hover (peek) the pill for the per-agent popover — `N live
agents — C calls · K ctx · O out`, a `last 15m` line covering every agent seen
in the live window (finished ones included, so a returned agent's work does not
vanish), one row per live agent (description; type · queue id
· last tool; calls / ctx / out / age since spawn) and a footer with the main
loop's own context tokens, the snapshot age and the host (`static/agent-bar.js`,
painted from the same `/api/queue` payload: `agent_stats.rows` /
`agent_stats.main`; the template embeds the first paint as a JSON seed). The
popover's right edge is anchored to the pill's (measured into the `--abp-right`
CSS property on open / repaint / resize, clamped inside the header on both
sides); under 480px it pins edge-to-edge instead.
The two rows are half-size and hard-nowrap at every width, so the header is
always exactly two rows; `agent_stats.label` still carries the long form
(`N agents · C calls · K tok`) for API consumers. The liveness dot next to
the controls is a matching small `live` / `error` pill.

The minisite does NOT fold transcripts itself. It reads a small JSON
snapshot that the host-side producer in this repo —
[`tools/cw-agent-stats/cw-agent-stats`](../tools/cw-agent-stats/README.md),
run from cron (Linux) or the launchd plist beside it (macOS) — rewrites
atomically every few seconds
(`QUEUE_MINISITE_AGENT_STATS_FILE`; shape: `{generated_at, main:{context_tokens,…},
agents:[{agent_id, queue_id, tool_calls, context_tokens, output_tokens,
last_tool, age_seconds, finished,…}], totals:{agents, agents_spawned,
tool_calls, context_tokens, output_tokens, window_tool_calls,
window_context_tokens, window_output_tokens}}` — the bare totals are
live-only, the `window_*` ones cover the whole live window including agents
that already returned) and JOINS `agents[].queue_id` onto the
running rows. The parse is cached on the file's mtime/size and rides along
in the existing `/api/queue` 5s poll (no second timer); `/api/agent-stats`
exposes the normalised view (join maps, staleness verdict, totals) for
debugging.

Degradation rules, in order:

* empty env var → feature off (no read, no pill, no cell);
* file missing / unreadable / not JSON → hidden (same as off);
* snapshot older than `QUEUE_MINISITE_AGENT_STATS_STALE_SECONDS` (60s) →
  **stale**: every cell is blank and the pill's numerals read `n/a` (dashed
  `.stale` pill, popover shows no rows) — a frozen number is worse than
  none, so staleness is re-derived on every request even when the file has
  not changed;
* a running row with no live agent for its queue id → no cell.

**Mount the snapshot's DIRECTORY, not the file.** The producer replaces the
file atomically (tmp + rename); a single-file bind mount pins the original
inode and goes stale on the first rewrite — the same trap the
`session-task` mount documents. The producer's default `--out` is
`<claude-watch state dir>/agent-stats.json` (`$CLAUDE_WATCH_STATE_DIR`, else
`/var/lib/claude-watch` — beside the daemon's `active-agents.json`), and the
minisite's default path is the SIBLING of `AGENT_STATE_JSON`, so when the
compose stack bind-mounts that state dir at `/agents-state` (`CW_STATE_PATH`)
both sides already agree (`/agents-state/agent-stats.json`) with no env var.
Otherwise mount the producer's output dir elsewhere and point the env var at
the file inside it (the `CLAUDE_HOST_AGENT_STATS_DIR` pattern in
`examples/compose/docker-compose.yml`).

## Model tag (which model ran the item)

Every list entry — running, pending, blocked, wedged, quarantined, done,
abandoned, and the unrecognised-status fallback — carries a short chip in
its item head naming the model that ran the work (`opus`, `sonnet`, …),
with the raw transcript id (`claude-opus-5`) on hover. Like the agent
activity cell it lives in the HEAD, so compact density keeps it (one notch
smaller, never hidden) — that is the whole point: the compact row showed
queue id, priority, token count, age and creator, and no model anywhere.

The model is not carried in `queue.json` nor in the active-agents state, so
it is read from the transcript — the ARCHIVED one for finished items, the
owner's LIVE one for running items — from `message.model` on the first
non-synthetic assistant record, exactly as the detail modal's `model` row
already does (one resolver, so the row and the modal can never disagree).
A `model` string stamped on the queue record itself wins over both, which
lets tooling record the model explicitly for an item whose transcript has
since been rotated away.

`/api/queue` carries it per row as `model` (raw id) + `model_label` (family
shorthand, or the raw id verbatim when the family is unrecognised — no
invented labels). Both are `""` when no model is attributable: workload and
hostjob items ran no model at all, pending items have not run yet, and an
agent item whose transcript is gone has no truthful answer. All three
render as ABSENT — no chip, no "unknown" placeholder. Resolution is
memoised on each transcript's (mtime, size), so the archived transcripts
behind the done section are scanned once rather than on every 5s poll.

## Multitail mode (all running tails at once)

A whole-window mode that stacks one live tail per RUNNING item, so one glance
answers "what is everything doing" instead of opening the single-item log
modal N times in a row.

* **Toggle**: the `multitail` pill in the header, or the **`m`** key. `m`,
  `Esc` or the **exit** button leave it. `m` is inert while you are typing in
  a field and while another dialog (the log modal, the stop/abandon confirm)
  owns the keyboard.
* **Dismiss one pane**: the `×` in that pane's header. It does not leave the
  mode, and the pane does not come back while the mode stays open.
* **Settings**: the `options` button in the overlay header, or the **`o`** key.
  Every setting below lives in that dialog. `Esc`, a backdrop click or its `×`
  close it — and `Esc` closes the *dialog*, not the mode behind it. Settings
  apply as you make them; there is no OK/Cancel.

  They were pills on the header row and the row ran out of width. Measured in a
  real browser: at **320px the header overflowed by 28px**, and the item pushed
  past the right edge was **`exit`** — on a phone the only way out of a
  whole-window takeover, because `m` and `Esc` want a keyboard. The fix for that
  was to *wrap* the header, which cost a phone reader a second (in verbose, a
  third: **91px of a 700px viewport**) row of chrome in the one view whose whole
  design constraint is how many panes stay legible. With one entry point the row
  is **0px of overflow and a single 33px row at every width**, verbose on or off.
* **Line wrap**: in options, or the **`w`** key. Off by default.
* **Timestamps**: in options, or the **`t`** key. Off by default.
* **Verbose**: in options, or the **`v`** key. Off by default. See
  **Verbose mode** below.
* **Verbose cap**: in options, or the **`x`** key. Cycles `cap 4K` →
  `cap 8K` → `cap 16K` → `cap 32K`, default **8K** — how much of a single line
  verbose mode shows before clipping it. Its row is only on screen while
  verbose is on, since that is the only mode the cap bounds.
* **Subagent tails**: in options, or the **`s`** key. **On by default** — each
  running item's subagents get a nested pane under it, indented by their real
  depth in the spawn tree. One card can be collapsed on its own from the
  `N subagents` button on its pane, which overrides the default for that card
  for the rest of the sitting.

  This defaults ON because the children of a running item are part of "what is
  everything doing", and having to ask for them per card meant they were in
  practice never on screen. It does **not** buy itself more connections:
  `MAX_LIVE_STREAMS` is unchanged and stream slots are handed out **tiered** —
  every top-level tail first, then every nested one, each group in the order it
  is on screen. Without that, one item with three children would hold all four
  connections and the second running task would be dark.
* **Ended-pane retention**: in options, or the **`c`** key. Cycles
  `clear 1m` → `clear 5m` → `clear 15m` → `keep`, default **1m**. If finished
  panes are *not* clearing, check first whether the tab predates the deploy that
  added this — see **Is this page stale?** below.
* **Per-pane metrics**: model · tool calls · context · output · age · last
  tool, right-aligned on each pane's title line (task title left, numbers
  right). They used to be a footer strip under the stream; moving them onto a
  row the header was spending anyway gives every pane a log line back.
* **All six settings are remembered** per viewer (localStorage, one key each),
  and the dialog shows the remembered state before the mode is first opened. A
  fresh viewer gets wrap off, timestamps off, verbose off, `cap 8K`, subagent
  tails **on**, `clear 1m`.
  Reads are guarded *and* validated — storage throws outright in some privacy
  modes and can hold an older build's value, so anything unrecognised means the
  default rather than a wedged view, and with storage unavailable the mode
  behaves exactly as it did before it remembered anything.
* `w`, `t`, `v`, `x`, `s`, `c` and `o` are mode-local — inert while the overlay
  is closed, while you are typing in a field, and while another dialog owns the
  keyboard. Modified chords pass straight through, so `Ctrl`/`Cmd`+`W` still
  closes the tab, `Ctrl`/`Cmd`+`V` still pastes, and `Ctrl`/`Cmd`+`C` still
  copies the log text you just selected.
* The MODE is not persisted: a full-window takeover that survived a reload
  would be a surprise rather than a convenience. Neither is anything scoped to a
  particular queue item — a dismissed pane and a hand-collapsed subagent tree
  both die with the mode, because they are statements about this sitting's stack
  rather than preferences about the view. The six settings in the dialog *are*
  persisted per viewer (e.g. `qsite_mt_retain`): picking `keep` because you read
  finished output carefully should not have to be repeated after every reload.

**Is this page stale?** This dashboard is built to be left open: it refreshes
itself by polling `/api/queue` and morphing the result in, and it never reloads.
So a deploy does **not** reach an open tab — the HTML, JS and CSS it is running
are the ones it fetched when it was opened, however long ago that was, and the
live-updating rows give no hint. That is not hypothetical: the ended-pane
retention above was merged, deployed and verified on the server, and reported as
not working half an hour later by a tab that predated the deploy and had made
447 API polls and zero page loads since. The feature was not broken; it was not
in the browser doing the looking.

Two halves address it. Every `url_for('static', …)` URL carries `?v=<mtime>`, so
a page load after a deploy cannot be served a cached script — and now every
render also stamps the front-end build that produced it on
`<body data-asset-version>`, the same value rides each `/api/queue` payload, and
`refresh.js` compares them on every tick. When they differ the page says so in a
small banner with a **Reload** button, above the multitail overlay (the mode a
stale build hides in) and below the modal band. It never reloads by itself: that
would discard open panes, scroll position, dismissed panes and any confirm
dialog mid-flight, so the viewer picks the moment. **When a front-end fix looks
absent, check that banner before debugging the feature.**

**Which items get a pane — and why some do not.** Exactly the rows the server
marks with a non-empty `live_log_mode` (`hostjob` / `workload` / `live`),
surfaced to both renderers as `data-live-log-mode`. That is deliberately
NARROWER than the `data-log-mode` every running card carries: every running
card is clickable, because clicking a `starting` item opens the modal in a
polling state and waits for its agent's first write — but an item with nothing
to read yet must not get a pane, since an empty pane spends a slice of the
viewport and one of the browser's scarce connections to say nothing. An item
that acquires a log joins on the next reconcile pass (2s). Precedence matches
the stream endpoint's own dispatch (hostjob → workload → agent), so a pane can
never advertise a tail the server would not serve.

**Which agent an agent-tail belongs to, and why it is not one lookup.** The
`agent_id ↔ queue_id` pairing in `active-agents.json` is parsed out of the
`Queue item: q-XXXX` marker in each agent's first user message, so it records
the queue id the agent was *spawned* with and never changes. Resuming a live
agent onto a new queue item — the normal way follow-up work is handed to an
agent that is already running, via `queue register <new-qid> --agent-id <id>` —
updates the **queue** side only. So a lookup keyed on the new queue id finds no
agent claiming it, even though the queue row names the owner correctly. Both the
card and the stream endpoint therefore resolve the owner through the same
three-rung ladder (`_classify_owner` → `_resolve_stream_agent_id`): an
active-agents record keyed on *this* queue id first, so a re-fired item's live
agent beats a stale stamp; then the register-time `agent_id` stamped on the
queue row; then the arm-hook spawn binding, which lands before the
active-agents poller does. Keying the stream on the first rung alone is what
made every *resumed* agent unwatchable — the card resolved the owner and
awarded the item a pane, then the pane's own stream reported `no-agent` for an
agent that was alive and writing. One ladder, consulted by both, is the
invariant; the queue row stays the authority on ownership rather than the
current queue id being copied back into the agent record, where it could drift
out of date again.

**How many panes stay legible.** Panes flex-share the viewport evenly, but only
down to `--mt-pane-min` (132px — a header and roughly ten monospace lines;
122px under 560px wide, where the header wraps onto two rows). Both numbers had
grown by 14px when the metrics were a footer strip, and both came back down by
it when the metrics moved onto the title line — the reclaimed row goes to the
window budget (one more pane before the stack scrolls), not quietly kept. Below that a tail shows a line or two and stops being
information, so past that point the stack **scrolls** instead of shrinking
further. On a laptop viewport that is an even split up to about five or six
tails and a scrolling stack beyond.

**Why only four stream at a time.** Each pane tails via `EventSource`, i.e. a
long-lived HTTP connection, and a browser allows only about six concurrent
connections per origin on HTTP/1.1. Six open tails and the site's own 5s
`/api/queue` poll can no longer get a connection: the page freezes in a way
that looks exactly like a server fault. So `MAX_LIVE_STREAMS` (4) panes hold a
connection at a time; the rest are built, visible, and labelled "waiting for a
stream slot", and are promoted the moment an earlier pane is closed or its job
ends. Every eligible item still gets a pane — what is rationed is the socket,
not the row.

**Panes render one line per event**, not the modal's full pretty-printed
transcript: the modal is where a single item gets read properly, and a rich
renderer inside a 132px pane shows one tool call. Agent transcript records
collapse to `▸ Bash <command>` / `← <first line of result>` / `· <text>`;
plain-text workload and hostjob logs pass through verbatim. Each pane retains
the last 400 lines.

**Line wrap (`w`).** Off by default: one event is one visual row, clipped at 400
characters with an ellipsis and cut off at the pane's right edge. On: the whole
line flows over as many rows as it needs (`overflow-wrap: anywhere`, because
these long lines are paths, JSON and base64 — single tokens that `break-word`
alone leaves overflowing). Because the toggle has to change lines that are
*already* on screen — including in a pane whose job has ended and will never
emit another line — each pane keeps its lines as records and the DOM is a
projection of them; `w` re-renders from the records. Two budgets, and they are
different: the 400-**line** retention is deliberately unchanged by wrap (how
much history a pane keeps should not depend on a display toggle, and dropping
200 lines of scrollback the moment someone presses `w` would be the worse
surprise), while per-line **storage** is bounded at 2000 characters so "show the
full line" cannot make the retained buffer unbounded. A longer line still ends
in an ellipsis even when wrapped; the single-item modal remains the place for the
genuinely complete payload. Auto-scroll's near-bottom slack is measured in
pixels, so it widens from 40px to 96px while wrapped — a wrapped line can be
taller than the unwrapped slack, and a reader who scrolled back to one row from
the bottom would otherwise never re-arm auto-scroll and the pane would look
stuck.

**Timestamps (`t`) show the log's OWN time, or nothing.** Agent transcripts are
JSONL and every record has a real ISO8601 `timestamp`, rendered in the viewer's
local timezone by the same helper the single-item modal uses. A plain-text log
carries a time exactly when its PRODUCER stamped it: `workload run` pipes its
payload through `claude-watch workload stamp`, so every line of `<label>.output`
begins with `date -Iseconds`, and the server splits that prefix off into the
frame's `source_ts` — one parser, shared by the panes, the single-item modal and
the archived-output view, so the time lands in the timestamp column instead of
being read as part of the line. A line with **no** stamp keeps its text verbatim
and gets no time: that is every log written before stamping existed, every
hostjob log (nothing on the host side stamps those), and the wrapper's own
header lines, which already print their absolute time in their text. Those panes
leave the column empty and say `no ts` in the header — a claim made by
OBSERVATION (has any line in this pane arrived with a stamp?) rather than by
guessing from the pane's kind, because one `workload` pane can be stamped and
the next one not. It does **not** fall back to the browser's arrival time. Arrival time answers a different question
("when did my browser receive this frame"), and for the 200-line backfill the
server replays the moment a pane opens it is uniformly wrong — every historical
line would read as roughly "now", flattening the very timing you opened the pane
to see. In a window that stacks both kinds of source at once, a real-timestamp
column and an arrival-time column look identical and invite exactly the
side-by-side comparison that is invalid. An empty column that explains itself
beats a plausible fabrication.

**The per-pane metrics answer "whose agent is this, and what is it costing".**
The mode is a whole-window takeover, so the queue rows that normally carry an
agent's counters are not on screen; without them you can see what an agent is
*doing* and nothing about what it is spending. Each pane prints the model chip,
tool calls, context tokens, output tokens, age since the agent's first
transcript entry and its last tool — **read off that pane's own queue row**
(`.model-tag`, `.agent-stats`) as the strings the server already formatted for
the row cell and the header popover. Nothing is re-derived, so a pane and a row
can never disagree about a count, and the values move on their own because
`refresh.js` rebuilds those rows every 5s. A value that is not known is ABSENT:
the server's formatters print `?` and `–` for "unknown" and those cells are
skipped, because confidently showing a wrong context size is worse than showing
less. A workload or hostjob pane runs no model and has no agent counters, so it
carries the workload/hostjob label (which the rest of the header does not show)
and stops there; a pane with nothing true to say hides them entirely rather than
leaving empty cells. The model is whatever the row says — never a pinned id,
since an alias like `opus` tracks whichever model is newest and a hardcoded id
would go stale silently.

**They sit on the title line, not on a row of their own.** A strip under the
stream is a whole row of chrome per pane, and with five or six panes open that
is five or six rows the logs do not get. The pane header already had a row with
spare width, so the two share it: title flush left, metrics flush right. The
push-right mechanism is the one the timestamp cell already uses — in a flex row,
make the item that should absorb the slack the *only* flexible one and whatever
follows lands against the far edge by itself, with no `margin-left: auto` and no
absolute positioning. The title is that item and keeps a floor width, so long
metrics ellipsise the numbers rather than the task name. Under 560px the header
wraps, the title line becomes the second row (title and metrics together — they
are one box, so a wrap cannot separate them) and the output-token and last-tool
cells drop out, leaving model / calls / ctx / age beside a readable title.

**A pane whose log does not exist yet keeps trying.** Eligibility and
log-existence are different instants, routinely: a `workload:` / `hostjob:` row
is eligible as soon as its scope is on the queue record, which is *before* the
runner creates `<label>.output` / `<label>/log`; a `live` row is eligible as soon
as an owner record names an `agent_id`, which is *before* that agent writes the
first line of its transcript. The server reports both as a one-shot in-stream
error and closes — `open-failed` / `read-failed` for a missing plain-text log,
`no-jsonl` for a transcript that does not exist yet, `no-agent` for a queue id
the active-agents map has not caught up with. All four mean "not there **yet**",
so the pane backs off and reconnects (3s, 6s, 12s, 24s, then every 30s) until
the log appears or its row stops being eligible. There is no attempt cap: the
bound is eligibility, and a job that finished before its log ever appeared
settles as `ended` (terminal) instead of retrying forever. Three states that
look identical in a blank pane are kept distinct — *log not there yet* (retrying,
with a countdown in the status), *log there and empty* (`live · no output yet`,
then `idle · no output yet` when the server's idle cap recycles the stream; not
an error), and *stream genuinely broke* (an error kind outside the retryable
set — terminal, and named). Retrying does not fight the connection cap: a pane
in backoff releases its slot immediately, so "waiting for a stream slot" and
"waiting for log · retry Ns" are different states and only the latter is on a
timer. Backfill suppression keys on whether the pane has ever **shown data**,
not on whether it has seen a `stream-start` — the plain-text tails emit
`stream-start` *before* they try to open the file, so a stream-start rule would
silently swallow the first content a successful retry receives.

**A job that finishes gets a grace period, then its pane is cleared.** Its
header flips to `ended` (with the exit code when the stream reported one) and
its stream slot returns to the pool immediately; the pane itself stays for the
retention delay and then goes, because a window that only ever accumulates
finished panes squeezes the running ones it exists to show. The same is true for
an item that leaves the running section entirely.

The delay is the reader's choice — `clear 1m` (default), `clear 5m`,
`clear 15m`, or `keep`, on the `clear` pill and the `c` key. A timed value
counts from the moment the pane became `ended`, and the pane says so:
`ended · exit 0 · clears in 42s`, so the pane about to disappear is the one
announcing it and there is a whole grace period in which to press `c` and switch
to `keep`, which retains ended panes until they are closed by hand. There is no
sub-minute option, because a grace period shorter than that is not one; and
nothing between 15 minutes and forever, because `keep` is the honest answer
there. Closing a pane early is still the operator's call.

The sweep rides the existing 2s reconcile tick — no second timer — and an ended
pane holds no stream slot, so clearing one cannot disturb the four-slot pump. An
auto-clear is **not** a manual dismissal: it never touches the dismissed set, so
a qid that becomes eligible again (a requeued job reusing it) gets a fresh pane.
What it does touch is a separate suppression set that keeps the pane from being
rebuilt on the very next tick — needed only when a pane ended while its queue row
was still running — and that entry is dropped the first pass the row is not
eligible, so it can never outlive the job it was for.

The mode reads eligibility off the rendered rows rather than re-fetching
`/api/queue`, so it is consistent with what the page is showing by
construction and adds no second poller — the existing 5s tick is what moves
the attribute. `data-no-morph` on the overlay keeps that merge away from the
panes, whose live connections and scroll positions a re-render would destroy.

`test_multitail.py` pins the server-side eligibility rules, the rendered
attributes and display pills, and (by grep, since it is the CI-gating suite)
the fact that `refresh.js` mirrors both the attribute and the toggle. It also
holds the two **cross-side** contracts the client cannot pin by itself: what the
stream endpoint actually emits when a log does not exist yet (asserted against
the real endpoint for all three shapes, then matched against the client's
retryable-kind set, so renaming one server-side fails here rather than quietly
producing a pane that never streams), and which sources carry a per-line
timestamp — that an agent record's `timestamp` survives the parse, and that
`workload_line` frames carry no time field at all, which is what makes the `no
ts` marker honest. Retention is client-side, so what that suite pins is the
contract between the three files which have to agree about it — the served
pill's default against the module's `DEFAULT_RETENTION_KEY`, the option table
(four values, ascending, `keep` = `ms: 0`, nothing under a minute), the
persistence key, the CSS, and two structural facts that are easy to break by
accident: that the sweep adds no third timer, and that the only writer of the
dismissed set is still the pane's `×`.

`static/multitail.test.js` drives the module itself under jsdom — pane
construction, the connection cap and slot promotion, manual close, the mode
toggles, the terminal-event handling, the compact formatter, both display
toggles (including that they re-render lines already on screen, that the line
budget is unaffected by wrap, and that a plain-text pane gets no timestamp
cells), the retry path (backoff shape, slot release, the replayed-backfill
regression guard, and `ended` winning over a pending retry) and ended-pane
retention (the countdown, the exact clear boundary, `keep` holding a
day-old pane, switching off `keep` applying at once, a requeued qid getting a
fresh pane, and the three storage boots: a restored choice, an unrecognised
stored value, and storage that throws) — plus the same parity checks against the
real `refresh.js` builders. It also covers a stamped plain-text line (a real
time in the column, the prefix gone from the body, and the pane's `no ts` marker
coming down for that pane only) and the per-pane metrics (the cells they print,
the unknown values they omit rather than placeholders, the workload label as the
one thing a workload pane can truthfully add, metrics hidden when nothing is
known, the counters following the row on the next tick, and where the group
lives in the pane — inside the title line, with no footer row left behind).

`static/multitail-refresh.test.js` loads **both** modules in one page, which is
the only place the handover between them can be tested: a running item moved to
`done` through the real `refresh.js` morph, and then the pane noticing its row is
no longer eligible, going `ended`, and clearing when retention elapses. Each
module's own suite owns half of that path and would pass while the other half was
broken. The same file pins the stale-build banner's comparison, including that
the banner is not a `data-no-morph` element — multitail treats any visible one as
a dialog that owns the keyboard, so marking it would silently kill `w` / `t` /
`c` whenever it was up.

## Verbose mode (`v`) — stop eliding

A pane's whole value is density, so the default renderer shows **one line per
event** and elides hard. `v` turns each of those elisions off:

| Elided by default | With `v` |
|---|---|
| A multi-line assistant message, thinking block, user message, system record or tool **result** is cut to its first line | The whole body, rendered with `pre-wrap` so the line breaks show |
| A tool call shows the first interesting argument's first line (`command`, `file_path`, …) | The whole input, indented |
| A tool result whose content array has no text block — or has one *after* an image block — reads `[N block(s)]` | Every block described |
| `[image]` / `[attachment]` | Count, media types and payload sizes; an attachment's path plus the rest of its record |
| Lines clipped at 400 characters (2000 with `wrap`) | Clipped at the **verbose cap** — the `x` pill, 8K by default |

Two things it deliberately does **not** do:

* **It is not unbounded.** Four live streams can each produce hundreds of lines
  a minute and the browser lays every one of them out, so a single line is still
  capped and so is the total text one pane retains (`MAX_PANE_CHARS`, evicting
  from the head like the line-count bound). A line budget alone stops bounding
  memory the moment one line can be ten times its normal size. The single-item
  log view remains the place for a genuinely complete payload.

  The per-line cap is the **reader's choice** (`x`), because how much of a
  payload is worth reading in a ten-row pane depends on what the panes are full
  of. `VERBOSE_CAP_OPTIONS` is the ladder — 4K / 8K / 16K / 32K — and 4000 was
  the fixed value this shipped with, kept on the ladder rather than deleted.
  `MAX_PANE_CHARS` is deliberately **not** raised alongside it: picking 32K buys
  longer lines by retaining fewer of them, which leaves the memory bound where
  it was.
* **It does not inline images.** A pane is ten rows tall and a data URI is
  megabytes; what verbose owes the reader there is what the thing is and how
  big. The single-item modal renders the image itself.

It is retroactive only as far as the retained records go. Lines are stored
clipped at the *currently chosen* verbose width, so switching `v` on immediately
widens every retained line a narrower setting had cut — but a multi-line record
was reduced to its first line when it *arrived*, so full detail applies to lines
received from then on. The alternative, retaining every raw payload in every
pane against a toggle that may never be pressed, is a memory multiplier paid by
everyone.

For the same reason, **raising** the cap is the one change that is not
retroactive: the characters past the old cap were never kept, so a bigger cap
applies to lines received from then on. **Lowering** it takes effect at once,
because rendering clips again. Storing every line at the largest cap on offer
would make every viewer pay the memory of an option they did not choose.

## Colourised output (ANSI escape sequences)

Most of what gets tailed here is colourised CLI output — `docker compose`,
`cargo`, `pytest`, `ffmpeg` — and those producers write SGR escape sequences
into their stdout, which the workload wrapper stamps and appends verbatim. Both
log views (multitail panes and the single-item modal) render line text through
one converter, `static/ansi.js`, so a colourised line looks the same in both.

* **Escaping comes first.** A log line is untrusted text. The tokenizer splits
  the raw string into text runs plus a style state, escapes each run (or writes
  it with `textContent`), and only then wraps it in a span whose classes come
  from the module's own tables. A line containing `<script>` stays inert and
  visible, and no line content ever reaches an attribute.
* **SGR is rendered**: the 8 base colours and their bright variants,
  256-colour and 24-bit truecolor (including the colon-delimited spellings),
  bold, dim, italic, underline, strike and inverse.
* **Everything else is dropped, not printed**: cursor movement, cursor
  show/hide, erase-line, OSC strings (an OSC-8 hyperlink keeps its text and
  loses the URL wrapper), charset designators, stray C0 controls. They address a
  terminal grid the page does not have.
* **A carriage return redraws the line**, so a progress bar collapses to its
  final frame — what the terminal would have left on screen. (Most `\r`s never
  reach the client: the server already splits plain-text tails on `\r` and marks
  each frame `transient` so the front end replaces the row in place.)
* **Colour state does not carry across lines.** Each frame renders from a clean
  state, because panes trim their head and join a stream mid-flight, so there is
  no reliable previous line to inherit from — and a sticky unterminated colour
  would paint the rest of the pane. A producer that opens a colour on one line
  and closes it on the next loses it on the continuation line: a wrong-but-
  bounded line beats a wrong-forever pane.
* **Both themes.** A palette chosen against a dark terminal background is
  regularly unreadable on a light page, so the 16 base colours map to CSS custom
  properties defined once per theme at contrast that reads on that theme's
  background — the hues are Solarized's, not its canonical terminal mapping
  (which turns `bright green`, a success marker in most CLI output, into body
  text). Indexed-cube and truecolor values have no such table, so the module
  computes a light and a dark variant per colour, memoised, and the stylesheet
  picks.
* **Cost**: one left-to-right pass per line, no re-parse of the buffer, and a
  single regex test for the common case of a line with no sequences at all.
  Adjacent runs sharing a style collapse into one span. Because the sequences
  are gone rather than hidden, selecting and copying a rendered line yields
  clean text.

`static/ansi.test.js` pins the converter (plain node, no jsdom — `make
test-minisite-ansi`, and it runs in CI): the escaping guarantee, SGR coverage,
the dropped sequences, `\r` collapsing, visible-character clipping, and the real
`docker compose` output shape taken from a live workload log.

## Layout

| Path | Purpose |
|------|---------|
| `app.py` | Single-file Flask app (read endpoints + Stop/Abandon/Force-start writers + SSE live-log stream). |
| `claude_agents.py` | Shared helpers for parsing `claude-watch active-agents` JSON state (agent\_id, queue-id join, dedup). |
| `templates/index.html` | Solarized-themed queue view. |
| `static/` | JS modules (`refresh.js`, `live-log.js`, `multitail.js`, `ansi.js`, `keyboard.js`, etc.), CSS, icons. |
| `claude-event` | Vendored event-emitter CLI used by `session-task` lifecycle hooks. |
| `obligations` | Vendored obligations-gate CLI used by the force-start endpoint. |
| `Dockerfile` | Build (python:3.12-alpine + gunicorn). |
| `test_*.py` | End-to-end tests (run in-process against a tempdir-rooted queue.json). |

## Run standalone

```bash
cd queue-minisite
docker build -t queue-minisite .
docker run --rm -p 8000:8000 \
  -e QUEUE_JSON=/queue-home/.config/session/queue.json \
  -e AGENT_STATE_JSON=/agents-state/active-agents.json \
  -e QUEUE_SITE_TITLE="my queue" \
  -e QUEUE_SITE_LOGO_DEFAULT=1 \
  -v "$HOME/.config/session:/queue-home/.config/session:rw" \
  -v "$HOME/claude-events:/queue-home/claude-events:rw" \
  -v "/var/lib/claude-watch:/agents-state:ro" \
  -v "$HOME/.claude/projects:/agents-jsonl:ro" \
  -v "$PWD/../tools/session-task/session-task:/app/session-task:ro" \
  queue-minisite
```

Then open `http://localhost:8000/`.

## Branding

The minisite ships a generic `claude-watch` build with the bundled eye-glyph
logo at `static/claude-watch-logo.png`. The page title defaults to `queue`
and no header logo is rendered unless one of the following is set.

To swap in a private brand without forking, set the `QUEUE_SITE_*` env
vars below — typically by mounting an `env_file` on the container so the
brand identity lives outside the public image.

| Var | Default | Purpose |
|-----|---------|---------|
| `QUEUE_SITE_TITLE` | `queue` | `<title>` + header label. |
| `QUEUE_SITE_LOGO_URL` | (empty) | Header logo URL (absolute or under `/static/`). Empty = no logo unless `QUEUE_SITE_LOGO_DEFAULT=1`. |
| `QUEUE_SITE_LOGO_DEFAULT` | (unset) | Set to `1`/`true` to render the bundled `static/claude-watch-logo.png` when `QUEUE_SITE_LOGO_URL` is empty. |
| `QUEUE_SITE_BRAND` | (empty) | Footer brand string. Empty = no footer. |
| `QUEUE_SITE_FAVICON_URL` | (empty) | Favicon override. Empty falls back to the bundled generic favicons in `static/branding/`. |

### Overriding the whole icon set: `static/branding/`

The favicon set and `logo.svg` live in **`static/branding/`**, not `static/`
itself, so a deploy can replace all of them by mounting ONE read-only folder
over `static/branding` — shadowing nothing else. Two constraints make that the
only safe shape:

- Mounting the files individually pins each host inode, so a later atomic
  rewrite on the host (write-temp-then-rename, which is how most tooling
  updates a file) never becomes visible inside the container.
- Mounting `static/` itself would shadow the app's own frontend —
  `refresh.js`, `live-log.js`, `style.css`, the vendored morphdom — straight
  out of the image.

The files in `static/branding/` are the bundled generic defaults; a mount
layers over them, and `QUEUE_SITE_LOGO_URL` should then point at
`/static/branding/<file>`. `static/claude-watch-logo.png` stays outside that
folder on purpose: it is the app's own default logo, not a brand slot.

## Environment

| Var | Default | Purpose |
|-----|---------|---------|
| `QUEUE_JSON` | `/queue/queue.json` | Path to `session-task` queue.json inside the container. |
| `AGENT_STATE_JSON` | `/agents-state/active-agents.json` | `claude-watch active-agents` JSON. |
| `AGENTS_JSONL_ROOT` | `/agents-jsonl` | Root of `~/.claude/projects/`; SSE live-log tails subagent transcripts here. |
| `QUEUE_LOG_ARCHIVE_DIR` | (unset) | Persistent archive dir for spawning-subagent transcripts. |
| `WORKLOAD_LOG_DIR` | `/workloads` | Workload `.output` archive dir, tailed by SSE for `workload:<label>` queue items. |
| `HOSTJOB_LOG_DIR` | `/hostjobs` | Hostjob log dir, tailed by SSE for `hostjob:<label>` queue items. NOTE per-label-dir layout: the tail target is `<HOSTJOB_LOG_DIR>/<label>/log` (not a flat `<label>.output`). |
| `CACHE_TTL_SECONDS` | `5` | Server-side cache TTL for the queue read. |
| `SSE_TAIL_MAX_IDLE_SECONDS` | `30` | Idle cap on SSE live-log streams. |
| `SSE_TAIL_MAX_LIFETIME_SECONDS` | `3600` | Lifetime cap on SSE live-log streams. |
| `SSE_TAIL_BACKFILL_LINES` | `200` | Historical-context backfill cap when a client first connects. |
| `QUEUE_MINISITE_AGENT_STATS_FILE` | sibling of `AGENT_STATE_JSON` (`/agents-state/agent-stats.json`) | Per-agent activity snapshot (tool calls + tokens) written by `tools/cw-agent-stats/cw-agent-stats`, joined onto running rows + summed in the header — see "Agent activity counters" below. Empty = feature off. |
| `QUEUE_MINISITE_AGENT_STATS_STALE_SECONDS` | `60` | Snapshot older than this (by `generated_at` or file mtime) renders as stale: blank cells + `n/a` numerals on the header pill, never a frozen number. |
| `PINGME_SESSION_TASK` | `0` | Set to `1` to suppress pingme chatter from `session-task` lifecycle. |
| `CLAUDE_EVENT_SESSION_TASK` | `0` | Set to `1` to suppress claude-event chatter from `session-task` lifecycle. |

## Tests

Run the whole suite the way CI does — from the repo root, one
interpreter per file, flask supplied by `uv`:

```bash
make test-queue-minisite
```

A single file, without the make wrapper:

```bash
cd queue-minisite
python3 -m venv .venv
.venv/bin/pip install flask gunicorn
.venv/bin/python test_depend.py
```

Tests spawn the Flask app in-process against a tempdir-rooted queue.json
and a vendored `session-task` (auto-located under `../tools/session-task/`;
override with `SESSION_TASK_BIN`). Each file gets its own process because
they rewrite `os.environ` and reload the `app` module at class setup.

These suites run in CI, in the `Queue-minisite Python tests` job.

The browser-side modules have their own suites in `static/*.test.js`. One of
them needs no DOM and therefore no dependency, so it runs in that same job:

```bash
make test-minisite-ansi      # static/ansi.test.js, plain node
```

The rest drive the modules under jsdom, and they run in CI too, in their own
`Queue-minisite jsdom tests` job:

```bash
make test-minisite-jsdom     # every static/*.test.js that needs a DOM
```

That target installs a pinned jsdom into `/tmp/queue-minisite-test` (override
with `MINISITE_JSDOM_DIR`) and runs every suite that reads `QM_NODE_MODULES`, so
a new jsdom suite joins the gate just by existing. It refuses to run rather than
skipping when node/npm are missing, and asserts a minimum suite count, because a
check that cannot fail is worse than no check. Measured cost: ~6s for all ten
files, plus a couple of seconds to fetch jsdom.

A single file, against a jsdom you already have:

```bash
QM_NODE_MODULES=/tmp/queue-minisite-test/node_modules \
  node static/multitail.test.js
```

**Running in CI and gating a merge are different things.** A job that is not
named in the branch's required-status-check list goes red without blocking
anything, which is the same silent rot wearing more logs:
`static/multitail-refresh.test.js` sat broken on `main` for hours because these
suites were local-only, and a non-required check would have let that happen
again. Which checks actually gate is a repository setting, not a property of
this file — read it live rather than trusting this paragraph:

```bash
gh api repos/gbre-org/claude-watch/branches/main/protection \
  --jq '.required_status_checks.checks[].context'
```

The doubled coverage the old local-only situation forced is still worth keeping:
anything a jsdom suite proves that must not regress silently also has a
grep-or-render assertion in a `test_*.py` file — which is why `test_multitail.py`
checks things like "no stylesheet rule hides the pane title" and "the ANSI
palette is defined in both themes" rather than leaving them to the client suites
alone.
