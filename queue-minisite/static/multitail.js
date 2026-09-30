// Multitail mode for the queue minisite.
//
// A whole-window mode that stacks one live tail per RUNNING queue item that
// actually has a log, so a single glance answers "what is everything doing"
// instead of opening the single-item log modal N times in a row.
//
//   toggle on/off  the `multitail` pill in the header, or the `m` key
//   leave          `m`, Esc, or the exit button
//   dismiss one    the × in that pane's header (stays in the mode)
//   settings       the `options` button in the header, or the `o` key
//   line wrap      in options, or the `w` key
//   timestamps     in options, or the `t` key
//   verbose        in options, or the `v` key (`x` picks its cap)
//   subagent tails in options, or the `s` key — and per card, the
//                  `N subagents` button on that card's pane
//   ended-retention in options, or the `c` key
//
// ---------------------------------------------------------------------------
// WHY THE SETTINGS ARE IN A DIALOG AND NOT ON THE HEADER ROW
// ---------------------------------------------------------------------------
// They were pills on the header row, one per setting, and the row ran out of
// width. MEASURED in a real browser rather than reasoned about: at 320px the
// header overflowed by 28px and the item pushed past the right edge was
// `exit` — which on a phone is the only way out of a whole-window takeover,
// because `m` and Esc want a keyboard. Turning verbose on made it worse
// rather than better, because the fix for that overflow was to WRAP the
// header: three rows, 91px of a 700px viewport, spent on chrome by the window
// whose entire design constraint is how many panes stay legible.
//
// One `options` button replaces the five settings. What stayed on the row is
// what is not a setting — the title, the live count readout, the key hint and
// `exit` — and the header now fits 320px with room to spare, so the wrap rule
// is gone and the rows it cost go back to the panes.
//
// THE DIALOG IS THE SITE'S OWN MODAL. `.modal` + `.modal-backdrop` +
// `.modal-panel`, Esc / backdrop / close-button dismissal, a Tab focus trap,
// focus to the close button on open and back to the trigger on close — the
// same shape action.js's confirm dialog uses. It lives INSIDE the overlay, so
// it inherits the `data-no-morph` exemption the pills relied on and stays
// below the single-item log modal's z-index band, which is the precedence
// that was already true.
//
// SETTINGS APPLY LIVE. There is no OK/Cancel, because every control is a
// projection of the panes behind the dialog and already persists on the spot;
// a confirm step would mean holding a pending copy of six settings and a way
// to revert them, to buy nothing.
//
// ---------------------------------------------------------------------------
// WHICH ITEMS GET A PANE
// ---------------------------------------------------------------------------
// Exactly the rendered queue rows carrying `data-live-log-mode` — an attribute
// both renderers (templates/index.html and refresh.js renderRunningItem) emit
// from the server's `live_log_mode` field. That field is deliberately NARROWER
// than the `data-log-mode` every running card carries: a `starting` item whose
// agent has not written its transcript yet, and an item whose owner cannot be
// identified, have nothing to tail. They get NO pane, because an empty pane
// costs a slice of the viewport and one of the browser's scarce connections to
// say nothing. As soon as such an item acquires a log the next reconcile pass
// (2s) gives it a pane.
//
// Reading the eligibility off the DOM rather than re-fetching /api/queue keeps
// the mode consistent with what the page is showing by construction, and adds
// no second poller — refresh.js's existing 5s tick is what moves the
// attribute.
//
// ---------------------------------------------------------------------------
// A PANE WHOSE LOG DOES NOT EXIST YET MUST KEEP TRYING
// ---------------------------------------------------------------------------
// Eligibility and log-existence are NOT the same instant, and the gap is
// routine rather than rare:
//
//   * a `workload:` / `hostjob:` row is eligible the moment its scope is on
//     the queue record, which is BEFORE the runner has created
//     `<label>.output` / `<label>/log`;
//   * a `live` row is eligible as soon as an owner record names an agent_id,
//     which is BEFORE that agent has written the first line of its transcript.
//
// The server reports both as a one-shot in-stream error and closes:
// `open-failed` / `read-failed` for a missing plain-text log, `no-jsonl` for a
// transcript that does not exist yet, `no-agent` for a queue id the
// active-agents map has not caught up with. Every one of those means "not
// there YET", so a pane that receives one goes into a BACKOFF and reconnects
// (3s, 6s, 12s, 24s, then every 30s) until the log appears or the row stops
// being eligible. Treating them as terminal — which is what this module used
// to do — left a pane that was built one second too early permanently dead,
// fixable only by toggling the whole mode off and on again.
//
// Three states that look identical in a dead pane and must not be conflated:
//
//   log not there yet   an error frame, no data → retrying, status says so.
//   log there, empty    `stream-start` and then silence → `live · no output
//                       yet`, then the server's idle cap recycles the stream
//                       and EventSource reconnects on its own. NOT an error.
//   stream really broke an error kind outside the retryable set → terminal
//                       for this pane, named in the status, as before.
//
// Retrying does NOT fight the connection cap: a pane in backoff releases its
// stream slot immediately, so the slot goes to a pane that can use it now, and
// the pane re-acquires one through pumpSlots() when its backoff expires. A
// pane WAITING FOR A SLOT is therefore a distinct state from a pane HOLDING a
// slot and getting nothing, and only the latter is ever retried.
//
// `ended` still wins: when the row stops being eligible (job finished,
// abandoned, moved out of the running section) the pane is marked ended and
// becomes terminal, so a job that finished before its log ever appeared
// settles instead of retrying forever.
//
// ---------------------------------------------------------------------------
// WHY THE CONCURRENT-STREAM CAP EXISTS (MAX_LIVE_STREAMS)
// ---------------------------------------------------------------------------
// Every pane tails via EventSource, i.e. a long-lived HTTP connection, and a
// browser allows only ~6 concurrent connections per origin on HTTP/1.1. Open
// six tails and the site's OWN 5s /api/queue poll can no longer get a
// connection: the page silently freezes, which looks exactly like a server
// fault. So at most MAX_LIVE_STREAMS panes hold a connection at a time; the
// rest are built, visible and labelled "waiting for a stream slot", and get
// promoted the moment an earlier pane is closed or its job ends. Every
// eligible item still gets a pane — what is rationed is the socket, not the
// row.
//
// ---------------------------------------------------------------------------
// HOW MANY PANES STAY LEGIBLE
// ---------------------------------------------------------------------------
// Panes flex-share the viewport evenly down to a floor of --mt-pane-min
// (132px: a header plus ~10 monospace lines). Below that a tail shows one or
// two lines and stops being information, so instead of shrinking further the
// stack SCROLLS. On a typical laptop viewport that is an even split up to
// about five or six panes and a scrolling stack beyond.
//
// ---------------------------------------------------------------------------
// LINE WRAP (`w`) — AND WHY EVERY LINE IS KEPT AS A RECORD
// ---------------------------------------------------------------------------
// Default OFF, which is the original behaviour: one event is one visual row,
// clipped at MAX_LINE_CHARS with an ellipsis and cut off at the pane's right
// edge. Wrap ON shows the whole line over as many visual rows as it needs.
//
// That means the clip CANNOT be applied at append time and forgotten — the
// full text has to still be around when the reader presses `w`, including in
// a pane whose job already ended and will never emit another line. So each
// pane keeps its lines as {sigil, text, cls, ts} RECORDS and the DOM is a pure
// projection of them; toggling wrap or timestamps re-renders from the records.
//
// Two budgets, and they are different budgets:
//
//   MAX_LINES_PER_PANE (400) is a LINE budget, not a visual-row budget. It is
//   deliberately unchanged by wrap: how much history a pane retains should not
//   depend on a display toggle, and silently dropping 200 lines of scrollback
//   the moment someone presses `w` would be a worse surprise than a tall pane.
//   Wrapping changes how TALL the retained history renders, nothing else.
//
//   MAX_LINE_CHARS_WRAPPED (2000) bounds what is STORED per line, so "show the
//   full line" cannot make the retained buffer unbounded — a 400 KB single-line
//   tool result would otherwise be held in memory in four panes at once. A line
//   longer than that still ends in an ellipsis even when wrapped; the
//   single-item modal remains the place for the genuinely complete payload.
//
// Auto-scroll is measured in PIXELS, so wrap changes it. A wrapped line can be
// half a dozen visual rows tall — taller than the 40px NEAR_BOTTOM_PX slack —
// and a reader who scrolls back to one row from the bottom would then fail to
// re-arm auto-scroll and the pane would look stuck. So the slack widens to
// NEAR_BOTTOM_PX_WRAPPED while wrap is on.
//
// ---------------------------------------------------------------------------
// TIMESTAMPS (`t`) — SOURCE TIME ONLY, NEVER ARRIVAL TIME
// ---------------------------------------------------------------------------
// The three stream sources do not carry the same information, and the toggle
// is honest about it rather than papering over the difference:
//
//   agent transcript (`live`)  JSONL records carry a real per-entry
//                              `timestamp` (ISO8601 UTC). Shown, rendered in
//                              the viewer's local timezone by the same
//                              LocalTime.timeOnly() the single-item modal
//                              uses, so the two never disagree.
//   workload `.output`         plain text. The `workload_line` frames carry NO
//   hostjob log                timestamp of any kind, and whatever the
//                              producer may have printed inside the line text
//                              is the producer's business — we do not parse
//                              prose looking for something that looks like a
//                              clock.
//
// For those two sources the pane shows NOTHING in the timestamp column and
// says why: its header gains a `no ts` marker while timestamps are on. It does
// NOT fall back to the client's arrival time. Arrival time answers a different
// question ("when did my browser receive this frame"), and for the backfill —
// the 200 lines the server replays the moment a pane opens — it is uniformly
// wrong: every historical line would be stamped with roughly "now", flattening
// the very timing the reader opened the pane to see. Worse, in a window that
// stacks both kinds of source at once, a real-timestamp column and an
// arrival-time column are visually identical and invite exactly the
// side-by-side comparison that is invalid. An empty slot that explains itself
// is strictly better than a plausible fabrication.
//
// WHERE the stamp goes is a layout question with a hard constraint: it must
// cost the BODY no horizontal width. It used to sit in a left gutter, which
// was a slice of width off a one-row entry and much worse than that for a
// multi-row one, because the gutter indents every continuation row — a
// pretty-printed JSON payload wrapped into the right-hand three quarters of
// the pane (reported from botchat: "can you make it so timestamps dont take
// up horizontal width? make them part of the line on the right side or smth
// idk"). The stamp now renders on the RIGHT: flush to the pane edge in
// one-row mode, and a right FLOAT on the first visual row in the wrapped and
// verbose modes, where a float shortens exactly one line box and every
// continuation row keeps the full width. The mechanism is stylesheet-side
// (see the TIMESTAMP PLACEMENT block in style.css); what this module owes it
// is SOURCE ORDER — the cell is appended to the row BEFORE the sigil and the
// body, because a float can only shorten the line box it is declared on. Do
// not reorder the appends in renderRecord to "match what you see".
//
// ---------------------------------------------------------------------------
// VERBOSE (`v`) — STOP ELIDING
// ---------------------------------------------------------------------------
// A pane's whole value is density, so the default renderer elides hard: ONE
// LINE PER EVENT. Verbose mode turns each of those elisions off. They were
// worth inventorying, because "show attachments and Read output" is a request
// about the two the reader happened to notice, and shipping only those two
// leaves the next one to be reported as a bug:
//
//   first line only      a multi-line assistant message, thinking block, user
//                        message, system record or tool RESULT was cut to its
//                        first line. Verbose keeps the whole body and the pane
//                        renders it with `pre-wrap`, so the line breaks show.
//   one tool argument    a tool call showed the first interesting key's first
//                        line (`command`, `file_path`, …). Verbose shows the
//                        whole input, indented — a Bash command or an agent
//                        prompt is the thing being read, and a JSON-escaped
//                        `\n` inside a one-liner is not reading.
//   `[N block(s)]`       a tool result whose content array had no text block,
//                        or had one AFTER an image block, said nothing at all.
//                        Verbose describes every block.
//   `[image]`            verbose gives the count, media types and payload
//   `[attachment]`       sizes; an attachment gives its path plus the rest of
//                        its record. NO image is inlined even in verbose: a
//                        pane is ten rows tall and a data URI is megabytes, so
//                        what verbose owes the reader here is what it is and
//                        how big. The single-item modal renders the image.
//   per-line clip        400 chars, or 2000 wrapped, becomes the VERBOSE CAP
//                        (see below — the reader picks it).
//
// IT IS NOT UNBOUNDED, because a firehose is not a feature. Four live streams
// can each produce hundreds of lines a minute, and the browser has to lay every
// one of them out:
//
//   * THE VERBOSE CAP caps a single line. It DEFAULTS to 8000 characters —
//     roughly two screenfuls of wrapped text — and the `cap` pill offers
//     4K / 8K / 16K / 32K, because how much of a payload is worth reading in a
//     ten-row pane depends on what the panes are full of and that is the
//     reader's call, not ours ("make max output length for verbose mode
//     configurable. double current value as default"; 4000 was the previous
//     fixed value and is kept on the ladder). The choice persists per viewer
//     like every other setting in this header.
//   * MAX_PANE_CHARS caps the TEXT one pane retains, evicting from the head
//     like the line-count bound does. The count bound alone stops bounding
//     memory the moment a line can be ten times its normal size. It is NOT
//     raised by a bigger per-line cap: choosing 32K buys longer lines by
//     retaining fewer of them, which keeps the memory bound where it was.
//
// RETROACTIVE ONLY AS FAR AS THE RECORDS GO. Lines are stored clipped at the
// CURRENTLY CHOSEN verbose width, so switching verbose on immediately widens
// every retained line that a narrower setting had cut. RAISING THE CAP is the
// one thing that is not retroactive, and deliberately: storing every line at
// the largest cap on offer would make every viewer pay the memory of an option
// they did not choose. A bigger cap applies to the lines that arrive after it,
// and a smaller one takes effect at once because rendering clips again. What it cannot recover is what the formatter
// never kept: outside verbose mode a multi-line record is reduced to its first
// line when it ARRIVES. So verbose shows full detail for the lines that arrive
// after it, and the pill's title says as much. The alternative — retaining
// every raw payload in every pane against a toggle that may never be pressed —
// is a memory multiplier across four live streams, paid by everyone.
//
// ---------------------------------------------------------------------------
// THE SETTINGS ARE REMEMBERED (localStorage)
// ---------------------------------------------------------------------------
// All six settings in the options dialog — wrap, timestamps, verbose, the
// verbose cap, subagent tails and ended-pane retention — persist per viewer
// under one key each, through one guarded accessor pair. A fresh viewer gets
// the defaults (wrap off, timestamps off, verbose off, cap 8K, subagent tails
// ON, clear 1m); a returning one gets what they left set, reflected on the
// controls at load rather than after the mode is first opened.
//
// Reads are GUARDED AND VALIDATED, not trusted. localStorage throws outright in
// some privacy modes, comes back empty after cleared site data or during a
// thumbnail capture, and can hold anything at all — an older or newer build's
// spelling, a hand edit. A value this build does not recognise yields the
// default, so no persisted string can wedge the view, and with storage
// unavailable the mode works exactly as it did before it remembered anything.
//
// ---------------------------------------------------------------------------
// PER-PANE METRICS — WHOSE AGENT IS THIS, AND WHAT IS IT COSTING
// ---------------------------------------------------------------------------
// Every pane reports the model that is running the item, its tool-call count,
// its context size, output tokens, last tool and age. This mode is a
// whole-window takeover, so the queue rows that normally carry those numbers
// are not on screen — without them, the reader can see what an agent is DOING
// and nothing about what it is costing.
//
// THEY LIVE ON THE PANE'S TITLE LINE, NOT ON A ROW OF THEIR OWN. They started
// as a footer strip under the stream, and a strip is a whole row of chrome per
// pane — with five or six panes open that is five or six rows the logs do not
// get (reported from botchat: "move the footer line in multitail up to the
// title line (maybe right aligned for legibility). so left side is task title,
// right side is calls/ctx/runtime/model"). The header already had a row and
// spare width on it, so the title and the metrics share one: title flush left,
// metrics flush right.
//
// The mechanism is the one the timestamp placement already uses a few hundred
// lines down — in a flex row, make the thing that should fill the space the
// ONLY flexible item and everything after it lands against the far edge. Here
// the title is that item inside `.mt-pane-titlebar`, so the metrics need no
// `margin-left: auto`, no absolute positioning and no second alignment idiom.
// The title keeps a floor width and ellipsises; the metrics clip from their
// own right, shedding the two cells that already know how to give way.
//
// EVERY FIELD IS READ, NEVER DERIVED. The values come off the pane's own queue
// row (`.model-tag`, `.agent-stats`) as the strings the SERVER already
// formatted for the row cell and the header popover (app.py
// `_shape_agent_stat`), so a pane and a row can never disagree about a count,
// and nothing here re-implements a formatter. refresh.js rebuilds those rows
// every 5s and the metrics repaint on the reconcile tick, so the numbers move
// on their own.
//
// A field with no value is ABSENT, not zeroed. The server's formatters use `?`
// and `–` for "not known", and a cell is skipped for those exactly as it is
// for an empty string: a confident wrong context size is worse than a shorter
// line. Whole classes of pane legitimately have nothing to show — a workload
// or hostjob pane runs no model and has no agent counters, so it carries the
// workload/hostjob LABEL (which the rest of the header does not show) and
// stops there; an agent pane whose stats snapshot has not caught up yet shows
// only the model, then fills in. Metrics with nothing at all in them are
// hidden outright, and because they sit inside the title line that costs the
// pane no space at all rather than leaving an empty bar.
//
// The model is whatever the row says — never a pinned id. An alias like `opus`
// tracks whichever model is newest, so hardcoding one here would go stale
// silently and lie about what actually ran.
//
// Space: --mt-pane-min came back DOWN by the strip's height when the strip
// went away, so the reclaimed row goes to the window budget (one more pane
// before the stack scrolls) rather than being quietly kept. Below 560px the
// header wraps and the title line is the second row — the metrics ride along
// on it, still right-aligned, and the output-token and last-tool cells drop
// out so what is left (model, calls, ctx, age) fits beside a readable title.
//
// ---------------------------------------------------------------------------
// SUBAGENTS — A PANE IS NOT ALWAYS A QUEUE ITEM
// ---------------------------------------------------------------------------
// An agent that spawns children was invisible here. Every pane used to BE a
// queue item: the map was keyed by qid and the only stream it could open was
// `/api/queue/<qid>/stream`. A subagent has no queue id of its own — it is
// keyed by SUBAGENT ID under a parent session — so there was no shape in this
// module that could hold one, and the window that answers "what is everything
// doing" stopped at the top level of the tree.
//
// PANE IDENTITY IS NOW (kind, target), NOT A QID. A pane carries
// `kind` ('queue' | 'subagent'), `target` (the id its stream is opened on) and
// `key` (`q:<qid>` / `s:<subagent-id>`, what the map and the dismissed /
// cleared sets are keyed by). `qid` still means "the queue item this pane
// belongs to" for BOTH kinds — for a nested pane that is its PARENT item,
// which is what groups it. A subagent id is deliberately NOT smuggled into the
// qid field: two different kinds of identifier in one slot is a trap for the
// next reader, and it would have made `data-queue-id` a lie in the DOM.
//
//   kind       stream endpoint                        ends when
//   queue      /api/queue/<qid>/stream                its row stops being eligible
//   subagent   /api/subagent/<subagent-id>/stream     its tree node stops being listed
//
// WHERE THE SUBAGENTS COME FROM: the rendered card, not a new fetch. Every
// running card already carries its own nested subagent tree —
// `.subagent-node[data-subagent-id]`, emitted by BOTH row renderers
// (the `subagent_node` macro in templates/index.html and `renderSubagentNode`
// in refresh.js) from the server's `it.subagents` shape, which app.py builds
// from the real spawn graph. Reading the tree off the DOM is the same contract
// this module already uses for row eligibility: no second poller, no second
// source of truth, and refresh.js's 5s tick is what keeps it current. The
// per-subagent `/api/subagent/<id>/meta` endpoint is deliberately NOT fetched
// — the label and the age it would return are already on the tree node, from
// the same server shaping, and a per-pane fetch would spend the connection
// budget this module rations everywhere else to learn what it has.
//
// SHOWN BY DEFAULT — TWO LEVELS OF CONTROL. Nested tails shipped COLLAPSED,
// on the reasoning that a pane costs a slice of the viewport and one of the
// browser's scarce connections, and one item can own half a dozen
// descendants. Both costs are real; neither is unbounded, and neither is
// bounded by collapsing. The stack SCROLLS past --mt-pane-min rather than
// shrinking panes below legibility, and MAX_LIVE_STREAMS rations the sockets —
// machinery that does not care where a pane came from. What collapsing bought
// instead was that the children of a running item were, in practice, never on
// screen in the window whose whole question is "what is everything doing": you
// had to already know a card had a tree worth opening.
//
//   the DEFAULT       `subsShown` — a viewer preference, in the options
//                     dialog and on the `s` key, PERSISTED like every other
//                     setting there (DEFAULT_SUBS_SHOWN = on).
//   per CARD          `subsChoice` — the `N subagents` button on a card's own
//                     pane writes a deviation from that default for this
//                     sitting. Hiding removes those panes outright (the reader
//                     said "go away", which is what the per-pane × means too)
//                     and does NOT record a dismissal, so showing again brings
//                     them straight back.
//
// The split is the point. "Show subagent tails" is a statement about the VIEW
// and should survive a reload; "collapse THIS card" is a statement about one
// queue item's tree, and persisting a per-qid map would accumulate keys for
// items that stopped existing weeks ago and re-apply a judgment about a
// different agent's children. So openMode clears the per-card map exactly as
// it clears dismissals, and the default it falls back to persists.
// Consequently flipping the default moves every card the reader has not
// touched by hand, and leaves the ones they have.
//
// Whichever way a card is showing its tree, the whole subtree appears at once,
// each pane indented by its real depth, so the stack mirrors the hierarchy the
// card shows rather than inventing a second one.
//
// SLOT ORDER IS TIERED: EVERY QUEUE PANE, THEN EVERY SUBAGENT PANE. Nested
// panes are INSERTED next to their parent rather than appended, so the pane
// list is not in map-insertion order and the rule has to be stated against the
// screen (panesInDisplayOrder). It used to be plain display order, and a
// subagent the reader had just expanded therefore outranked a later card's
// pane — the honest reading of having ASKED for it. Nobody asks any more, and
// under plain display order one busy item would silently take the whole
// connection budget: an item with three children would hold all four slots and
// the SECOND running task would get nothing. So slotOrder() hands slots to
// every top-level pane first, each tier still in the order you see it. That
// only changes the case where the top-level panes do not already fill the cap;
// with four or more items running, a tree waited for a slot before and waits
// for one now.
//
// What none of this does is take a connection away from a pane that already
// has one: mid-read is mid-read, and a stack that reshuffled its live streams
// on every click would be worse than a wait. So a tree opened while the cap is
// already full builds the panes, shows them, and leaves the ones past the cap
// saying `waiting for a stream slot` until an earlier pane ends or is closed —
// the same state, and the same wording, a sixth card's pane has always had.
// Closing what you are not reading is how you get a slot back.
//
// AN ENDED SUBAGENT ENDS LIKE AN ENDED AGENT. There is no end frame on a
// transcript tail (an agent pane's `ended` comes from its ROW leaving the
// running section, never from the stream), so a subagent pane ends the same
// way: when the tree stops listing it, or when its parent item stops running
// and takes the whole group with it. Until then a finished subagent's pane
// keeps its output and simply stops producing lines, and once ended it goes
// through the same retention countdown as every other pane — it neither
// lingers forever nor vanishes mid-read.
//
// ---------------------------------------------------------------------------
// A PANE WHOSE JOB FINISHES WHILE THE MODE IS OPEN
// ---------------------------------------------------------------------------
// Its header flips to `ended` (with the exit code when the stream reported
// one), its stream slot is released so a waiting pane can connect, and it then
// GETS CLEARED once the retention delay has elapsed — because a window that
// only ever accumulates finished panes squeezes the running ones it exists to
// show. The delay is the reader's, not ours:
//
//   RETENTION_OPTIONS  1m (default) · 5m · 15m · keep
//
// A timed value counts from the moment the pane became ended, and the pane
// SAYS SO: its status reads `ended · exit 0 · clears in 42s`, so the pane about
// to go is the one announcing it, and anyone mid-read has a whole grace period
// to press `c` (or the `clear` pill) and switch to `keep`, which retains ended
// panes until they are closed by hand. Below a minute the grace stops being a
// grace, so no sub-minute option exists; above a quarter of an hour `keep` is
// the honest answer, so nothing between 15m and forever exists either.
//
// The sweep rides the existing RECONCILE_MS tick — there is no second timer —
// and it is careful in two ways:
//
//   * An auto-clear is NOT a manual dismissal. It never touches `dismissed`,
//     so a qid that becomes eligible again (a requeued job reusing it) gets a
//     fresh pane on the next pass. What it touches is `cleared`, which
//     suppresses an immediate rebuild ONLY while the row that pane came from
//     stays continuously eligible; the moment that row leaves the eligible set
//     the suppression is dropped, so it can never outlive the job it was for.
//     Without the suppression, a pane whose stream reported `workload-end`
//     while its queue row was still running would be cleared and rebuilt every
//     2 seconds.
//   * An ended pane holds no stream slot (markEnded released it), so clearing
//     one cannot disturb the 4-slot pump.
//
// The retention CHOICE persists (localStorage, `qsite_mt_retain`, per viewer —
// the same mechanism as the density and header-collapse pills), as every
// setting in the options dialog now does. It is a policy about how much
// finished output survives, and someone who picked `keep` because they read
// finished output carefully should not have to pick it again after every
// reload. Storage can throw or come back empty, so every access is guarded and
// an unreadable or unrecognised value simply means the default.
//
// The MODE itself is still not persisted: a full-window takeover that survived
// a reload would be a surprise, not a convenience. Nor is anything scoped to a
// particular queue item — dismissals and per-card subagent collapses die with
// the mode, because they are statements about this sitting's stack rather than
// preferences about the view.

(function () {
  'use strict';

  const overlay = document.getElementById('multitail');
  if (!overlay) return;

  const panesEl = document.getElementById('multitail-panes');
  const emptyEl = document.getElementById('multitail-empty');
  const countEl = document.getElementById('multitail-count');
  const exitBtn = document.getElementById('multitail-exit');
  const wrapBtn = document.getElementById('multitail-wrap');
  const tsBtn = document.getElementById('multitail-ts');
  const retainBtn = document.getElementById('multitail-retain');
  const verboseBtn = document.getElementById('multitail-verbose');
  const vcapBtn = document.getElementById('multitail-vcap');
  // The verbose cap's whole ROW in the options dialog, not just its control:
  // the setting's name and note have to disappear with it, or the sheet keeps
  // a labelled row explaining a control that is not there.
  const vcapRow = document.getElementById('multitail-vcap-row');
  const subsModeBtn = document.getElementById('multitail-subs');
  // The options dialog and its one entry point on the header row.
  const optionsBtn = document.getElementById('multitail-options');
  const optionsModal = document.getElementById('multitail-options-modal');
  const optionsPanel = document.getElementById('multitail-options-panel');
  const optionsCloseBtn = document.getElementById('multitail-options-close');

  // Rows the server marked as having a tailable log, in render order.
  const ROW_SELECTOR = '.item[data-live-log-mode]';
  // The nested subagent tree inside a running card, in tree order. Emitted by
  // BOTH row renderers from the server's `it.subagents` shape — see the
  // SUBAGENTS block in the header comment for why this is read off the DOM
  // rather than fetched.
  const SUB_NODE_SELECTOR = '.subagent-node[data-subagent-id]';
  // How many levels of tree depth the nested panes are allowed to indent for.
  // Depth is unbounded on the server (the spawn graph is walked to arbitrary
  // depth); an unbounded INDENT would spend a great-great-grandchild's whole
  // pane width on its own left margin. Past this the panes stay at the same
  // offset and the tree order carries the hierarchy.
  const MAX_SUB_INDENT_DEPTH = 4;
  // Max simultaneous EventSource connections — see the header comment. Four
  // leaves two of the browser's ~6 per-origin HTTP/1.1 connections for the
  // queue poll and any action POST.
  const MAX_LIVE_STREAMS = 4;
  // Lines retained per pane. A tail is a window on the recent past; keeping
  // an unbounded transcript in N panes is how a long-lived tab runs out of
  // memory. Unchanged by wrap on purpose — see the header comment.
  const MAX_LINES_PER_PANE = 400;
  // How often the pane list is reconciled against the rendered rows, and the
  // tick that also expires stream-retry backoffs. Twice the refresh tick's
  // resolution so a newly-eligible item shows up promptly without polling
  // anything itself.
  const RECONCILE_MS = 2000;
  // Re-arm auto-scroll once the reader is back within this many px of the
  // bottom (same rule the single-item log modal uses). The WRAPPED value is
  // wider because one wrapped line can be taller than the unwrapped slack,
  // which would make auto-scroll impossible to re-arm.
  const NEAR_BOTTOM_PX = 40;
  const NEAR_BOTTOM_PX_WRAPPED = 96;
  // Longest single rendered line before it is clipped with an ellipsis. Panes
  // are short; one 4000-character tool result would otherwise be the whole
  // pane. The WRAPPED value is both the wrap-on clip and the per-line storage
  // bound — see the header comment.
  const MAX_LINE_CHARS = 400;
  const MAX_LINE_CHARS_WRAPPED = 2000;
  // VERBOSE mode's per-line ceiling, and the one bound in this module the
  // READER sets. Verbose exists to stop eliding, but "no limit" is not a
  // limit: one 2MB tool result would be the pane, the tab's memory and a
  // layout pass. What the right number is, though, is not ours to know — it
  // depends on what the panes are full of, which is why it is a choice
  // (botchat: "make max output length for verbose mode configurable. double
  // current value as default").
  //
  // The default is 8000, twice the 4000 this shipped with, which is what was
  // asked for. The rest of the ladder brackets it by doubling in both
  // directions: 4000 is the old behaviour kept as a real choice rather than
  // deleted, and 32000 is about eight screenfuls of wrapped text — past that
  // reading has certainly become searching and the single-item log view has
  // the complete payload anyway.
  //
  // Raising it does NOT raise what a pane retains in total: MAX_PANE_CHARS is
  // unchanged, so a 32K cap on a firehose buys longer lines by keeping fewer
  // of them. That is the right trade for a reader who went looking for one
  // long payload, and it keeps the memory bound where it was.
  const VERBOSE_CAP_OPTIONS = [
    { key: '4k', chars: 4000, label: 'cap 4K', aria: '4,000 characters' },
    { key: '8k', chars: 8000, label: 'cap 8K', aria: '8,000 characters' },
    { key: '16k', chars: 16000, label: 'cap 16K', aria: '16,000 characters' },
    { key: '32k', chars: 32000, label: 'cap 32K', aria: '32,000 characters' },
  ];
  const VERBOSE_CAP_BY_KEY = {};
  for (const opt of VERBOSE_CAP_OPTIONS) VERBOSE_CAP_BY_KEY[opt.key] = opt;
  // Stated here rather than implied by array position, and templates/index.html
  // renders the pill with this same key — test_multitail.py pins the two
  // together, exactly as it does for the retention default.
  const DEFAULT_VERBOSE_CAP_KEY = '8k';
  // Hard ceiling on the TEXT one pane retains, across however many lines that
  // is. MAX_LINES_PER_PANE alone bounds the line COUNT, and in verbose mode a
  // line can be ten times its normal size, so the count bound stops bounding
  // memory. Lines are dropped from the head until the pane is back under
  // budget — the same eviction the count bound uses, for the same reason.
  // 400 lines x 2000 chars is the pre-verbose worst case; this keeps verbose
  // panes inside it instead of multiplying it by four.
  const MAX_PANE_CHARS = 800000;
  // Stream-retry backoff: attempt N waits BASE * 2^(N-1), capped at MAX.
  // 3s / 6s / 12s / 24s / 30s / 30s… — fast enough that a log appearing a
  // moment after the pane does is picked up while the operator is still
  // looking, slow enough that a genuinely absent log costs two requests a
  // minute. There is no attempt CAP: the bound is row eligibility, because a
  // long-running job whose log is slow to appear must still be picked up, and
  // an item that stops running is marked ended (terminal) by reconcile().
  const STREAM_RETRY_BASE_MS = 3000;
  const STREAM_RETRY_MAX_MS = 30000;
  // In-stream error kinds that mean "not there YET" rather than "broken".
  // Every one of these is emitted by a server path that gave up because a file
  // or a state-file row did not exist at that instant, which is precisely the
  // situation a moment later resolves. Anything NOT listed here stays terminal
  // for the pane: an error shape we cannot reason about should be reported, not
  // spun on.
  const RETRYABLE_ERROR_KINDS = {
    'no-agent': true,     // active-agents.json has no row for this qid yet
    'no-jsonl': true,     // agent record exists, transcript file does not
    'open-failed': true,  // plain-text log (.output / hostjob log) missing
    'read-failed': true,  // transient read error on an existing log
  };
  // Stream modes whose frames carry a real per-entry source timestamp. Agent
  // transcripts are JSONL with a `timestamp` on each record; workload and
  // hostjob tails are plain text and carry none. A SUBAGENT tail is the same
  // JSONL through the same server-side `_tail_jsonl`, so it carries the same
  // per-record timestamp — leaving it out here would have shown `no ts` on a
  // pane whose every line has one.
  const TS_SOURCE_MODES = { live: true, subagent: true };
  // How long an ENDED pane is kept before it is cleared, in the order the
  // `clear` pill cycles. `ms: 0` means "never clear" — see the header comment
  // for why the set stops at 15m and has nothing below a minute.
  const RETENTION_OPTIONS = [
    { key: '1m', ms: 60 * 1000, label: 'clear 1m', aria: 'cleared 1 minute after they end' },
    { key: '5m', ms: 5 * 60 * 1000, label: 'clear 5m', aria: 'cleared 5 minutes after they end' },
    { key: '15m', ms: 15 * 60 * 1000, label: 'clear 15m', aria: 'cleared 15 minutes after they end' },
    { key: 'keep', ms: 0, label: 'keep', aria: 'kept until you close them' },
  ];
  const RETENTION_BY_KEY = {};
  for (const opt of RETENTION_OPTIONS) RETENTION_BY_KEY[opt.key] = opt;
  // The default is the one that was asked for. It is stated here rather than
  // implied by array position, and templates/index.html renders the pill with
  // this same key — test_multitail.py pins the two together.
  const DEFAULT_RETENTION_KEY = '1m';
  const RETENTION_STORAGE_KEY = 'qsite_mt_retain';
  // The display toggles persist the same way the retention value does — one
  // localStorage key each, read through the same guarded accessors below.
  const WRAP_STORAGE_KEY = 'qsite_mt_wrap';
  const TS_STORAGE_KEY = 'qsite_mt_ts';
  const VERBOSE_STORAGE_KEY = 'qsite_mt_verbose';
  const VERBOSE_CAP_STORAGE_KEY = 'qsite_mt_vcap';
  const SUBS_STORAGE_KEY = 'qsite_mt_subs';
  // Nested subagent tails are SHOWN by default. This flipped: they shipped
  // collapsed, on the reasoning that a pane costs a slice of the viewport and
  // one of the browser's scarce connections. Both costs are real, and both are
  // already bounded by machinery that does not care where a pane came from —
  // the stack SCROLLS past --mt-pane-min rather than shrinking, and
  // MAX_LIVE_STREAMS rations the sockets. What collapsing bought instead was
  // that the children of a running item were, in practice, never on screen in
  // the window whose whole question is "what is everything doing" — you had to
  // already know a card had a tree worth opening. Stated here rather than
  // implied, and templates/index.html renders the control with this same
  // value; test_multitail.py pins the two together, as it does for the
  // retention and cap defaults.
  const DEFAULT_SUBS_SHOWN = true;

  let open = false;
  let reconcileTimer = null;
  // PANE KEY -> pane record. The key is `q:<qid>` for a queue item's own tail
  // and `s:<subagent-id>` for a nested subagent tail (paneKey below) — a pane
  // is a (kind, target) pair, not a qid, since a subagent has no queue id of
  // its own. Map order is the order panes were CREATED; the order they get
  // stream slots is the order they are on SCREEN (panesInDisplayOrder), which
  // differs once a nested pane is inserted beside its parent.
  const panes = new Map();
  // Panes the operator dismissed by hand, by pane key. They must NOT come back
  // on the next reconcile pass — "I closed that" has to stick while the mode
  // is open.
  const dismissed = new Set();
  // Panes RETENTION cleared while their row was still eligible. Deliberately a
  // different set from `dismissed`: an auto-clear is not a decision, so its
  // suppression lasts only as long as that row's current eligibility streak
  // (reconcile drops the entry the first pass the row is not eligible). Without
  // it, a pane whose stream said `workload-end` while the queue row was still
  // running would be cleared and rebuilt on every tick.
  const cleared = new Set();
  // Queue items whose nested subagent tails were explicitly shown or hidden
  // FROM THEIR OWN PANE, by qid. An entry is a per-card DEVIATION from the
  // `subsShown` default below; a qid that is absent simply follows that
  // default, which is what makes flipping the default move every card that has
  // not been touched by hand and leave the ones that have.
  //
  // Session-scoped on purpose, while the default it deviates from persists.
  // "Show subagent tails" is a preference about the VIEW and survives reloads
  // like every other setting in the sheet; "collapse THIS card" is a statement
  // about one queue item, and persisting a per-qid map would accumulate keys
  // for items that stopped existing weeks ago and re-apply a judgment about a
  // different agent's tree. Same lifetime as `dismissed`, for the same reason:
  // openMode clears it.
  const subsChoice = new Map();

  // --- ended-pane retention ------------------------------------------------

  // --- persisted display preferences ---------------------------------------
  //
  // Every setting in this mode's header is remembered per viewer: the
  // retention value and the three toggles all go through these two accessors.
  //
  // EVERY ACCESS IS GUARDED, AND EVERY READ IS VALIDATED. localStorage throws
  // outright in some privacy modes, comes back empty after cleared site data
  // or during a thumbnail capture, and can hold anything at all — an older or
  // newer build's spelling, a hand edit. So a read that does not produce a
  // value this build recognises yields the DEFAULT rather than being trusted:
  // a persisted string must never be able to wedge the view. The mode renders
  // correctly with storage entirely unavailable; the choice then simply
  // applies to the current page.
  function readStored(key) {
    try {
      return window.localStorage.getItem(key);
    } catch (_) {
      return null;  // storage unavailable — caller's default applies
    }
  }

  function writeStored(key, value) {
    try {
      window.localStorage.setItem(key, value);
    } catch (_) {
      /* storage unavailable — the choice still applies to this page */
    }
  }

  // Flags are stored as '1' / '0'. Anything else — absent, '', 'true', an
  // older build's spelling — is the default, never a coerced truthy string.
  function readStoredFlag(key, dflt) {
    const v = readStored(key);
    if (v === '1') return true;
    if (v === '0') return false;
    return dflt;
  }

  function storeFlag(key, on) {
    writeStored(key, on ? '1' : '0');
  }

  function readStoredRetention() {
    const v = readStored(RETENTION_STORAGE_KEY);
    return (v && RETENTION_BY_KEY[v]) ? v : DEFAULT_RETENTION_KEY;
  }

  function storeRetention(key) {
    writeStored(RETENTION_STORAGE_KEY, key);
  }

  // Same shape as the retention reader, and for the same reason: an
  // unrecognised stored key — an older build's spelling, a hand edit — is the
  // DEFAULT, never a number coerced out of a string.
  function readStoredVerboseCap() {
    const v = readStored(VERBOSE_CAP_STORAGE_KEY);
    return (v && VERBOSE_CAP_BY_KEY[v]) ? v : DEFAULT_VERBOSE_CAP_KEY;
  }

  function storeVerboseCap(key) {
    writeStored(VERBOSE_CAP_STORAGE_KEY, key);
  }

  let retentionKey = readStoredRetention();
  // The three display toggles. A FRESH viewer gets the old defaults — wrap
  // off, timestamps off, verbose off — because off is the behaviour that
  // existed before each of them did; a returning viewer gets what they left
  // set. They are read here, before any pane exists, so the first paint is
  // already in the remembered state.
  let wrapOn = readStoredFlag(WRAP_STORAGE_KEY, false);
  let tsOn = readStoredFlag(TS_STORAGE_KEY, false);
  let verboseOn = readStoredFlag(VERBOSE_STORAGE_KEY, false);
  let verboseCapKey = readStoredVerboseCap();
  // Whether a card's nested subagent tails are shown WITHOUT being asked for.
  // Read through the same guarded accessor as the other flags, so a viewer who
  // turned it off keeps it off and unreadable storage just means the default.
  let subsShown = readStoredFlag(SUBS_STORAGE_KEY, DEFAULT_SUBS_SHOWN);

  // Does THIS card show its nested tails right now? The per-card choice wins
  // over the default; absent means follow the default.
  function subsShownFor(qid) {
    const v = subsChoice.get(qid);
    return v === undefined ? subsShown : v;
  }

  function verboseCapOption() {
    return VERBOSE_CAP_BY_KEY[verboseCapKey] ||
      VERBOSE_CAP_BY_KEY[DEFAULT_VERBOSE_CAP_KEY];
  }

  // The chosen verbose ceiling in characters. Read even while verbose is OFF,
  // because it is also the STORAGE bound below: a line has to be retained at
  // the widest width any setting can ask for, or turning verbose on would
  // widen nothing.
  function verboseCapChars() {
    return verboseCapOption().chars;
  }

  function retentionOption() {
    return RETENTION_BY_KEY[retentionKey] || RETENTION_BY_KEY[DEFAULT_RETENTION_KEY];
  }

  // 0 = keep forever.
  function retentionMs() {
    return retentionOption().ms;
  }

  // --- eligibility ---------------------------------------------------------

  function eligibleRows() {
    return Array.prototype.slice.call(document.querySelectorAll(ROW_SELECTOR));
  }

  // The identity of a pane. Two namespaces, both prefixed, so neither kind of
  // id can ever be mistaken for the other — and so a queue id is never asked
  // to stand in for a subagent id.
  function paneKey(kind, target) {
    return (kind === 'subagent' ? 's:' : 'q:') + target;
  }

  function rowInfo(row) {
    const qid = row.getAttribute('data-queue-id') || '';
    return {
      kind: 'queue',
      key: paneKey('queue', qid),
      // What the stream is opened on. For a queue pane that IS the qid; the
      // two are separate fields because for a subagent pane they are not.
      target: qid,
      qid: qid,
      depth: 0,
      mode: (row.getAttribute('data-live-log-mode') || '').toLowerCase(),
      summary: row.getAttribute('data-queue-summary') || '',
      meta: rowMetaInfo(row),
      // How many subagents this card is currently showing. Drives the header
      // button's count and whether it is shown at all.
      subCount: row.querySelectorAll(SUB_NODE_SELECTOR).length,
    };
  }

  // A subagent node's depth in ITS card's tree. The nodes come back from
  // querySelectorAll in document (tree) order, flat; the nesting is carried by
  // the `.subagent-children` lists between the node and the card, which is
  // what this counts. Depth 1 = a direct child of the item's owner agent.
  function subNodeDepth(row, node) {
    let depth = 1;
    let p = node.parentNode;
    while (p && p !== row) {
      if (p.classList && p.classList.contains('subagent-children')) depth += 1;
      p = p.parentNode;
    }
    return depth;
  }

  // One nested pane's descriptor, read off a rendered tree node. The label and
  // the age are the server's own strings (app.py `_list_session_subagents`),
  // exactly as the row metrics are — nothing here re-derives either.
  function subInfo(row, node, qid) {
    const sid = node.getAttribute('data-subagent-id') || '';
    const labelEl = node.querySelector('.subagent-label');
    const ageEl = node.querySelector('.subagent-age');
    const label = labelEl ? String(labelEl.textContent || '').trim() : '';
    return {
      kind: 'subagent',
      key: paneKey('subagent', sid),
      target: sid,
      // The queue item this tail hangs off: its PARENT card. A subagent has no
      // queue id of its own, and this one is never written to `data-queue-id`.
      qid: qid,
      depth: subNodeDepth(row, node),
      mode: 'subagent',
      summary: label || sid,
      subCount: 0,
      meta: {
        age: ageEl ? String(ageEl.textContent || '').trim() : '',
        // A `peer` node is co-bound to the same queue item with NO recorded
        // spawn edge to the owner agent (app.py `_build_subagent_tree`). The
        // card marks that distinction; a pane that dropped it would imply a
        // parent/child relationship the data never recorded.
        peer: (node.classList && node.classList.contains('subagent-peer'))
          ? 'peer' : '',
      },
    };
  }

  // Values for the pane's title-line metrics, read off the rendered row.
  // `.agent-stats` and `.model-tag` live in the row's HEAD — the one part of a
  // card compact density never elides — and both are rebuilt by refresh.js
  // every 5s, which is what keeps the numbers live. Missing element or missing
  // attribute means missing value, and a missing value is simply not rendered.
  function rowMetaInfo(row) {
    const head = row.querySelector('.item-head') || row;
    const model = head.querySelector('.model-tag');
    const stats = head.querySelector('.agent-stats');
    const attr = (el, name) => (el ? el.getAttribute(name) || '' : '');
    return {
      model: model ? String(model.textContent || '') : '',
      modelTitle: attr(model, 'title'),
      calls: attr(stats, 'data-calls-text'),
      ctx: attr(stats, 'data-ctx-text'),
      out: attr(stats, 'data-out-text'),
      lastTool: attr(stats, 'data-last-tool'),
      age: attr(stats, 'data-age-text'),
      statsTitle: attr(stats, 'title'),
      label: row.getAttribute('data-workload-label') ||
        row.getAttribute('data-hostjob-label') || '',
    };
  }

  // The server's formatters print `?` for a counter it could not read and `–`
  // for an absent one. Neither is a value, so neither gets a cell — see the
  // header comment on why a pane says less rather than guessing.
  function metaValue(v) {
    const s = String(v === undefined || v === null ? '' : v).trim();
    if (!s || s === '?' || s === '–' || s === '-') return '';
    return s;
  }

  // --- small DOM helpers ---------------------------------------------------

  function el(tag, cls, text) {
    const node = document.createElement(tag);
    if (cls) node.className = cls;
    // textContent only — every string in here comes from a queue record or an
    // agent transcript, i.e. untrusted prose. No innerHTML anywhere in this
    // module.
    if (text !== undefined && text !== null) node.textContent = String(text);
    return node;
  }

  // --- terminal escape sequences (static/ansi.js) --------------------------
  //
  // Pane text arrives with the producer's ANSI sequences intact, and most of
  // what gets tailed here is colourised CLI output (docker compose, cargo,
  // pytest, ffmpeg). Rendered as plain text those sequences read as literal
  // `[32m` garbage. ansi.js converts them to themed spans — and it does it
  // WITHOUT innerHTML: `toFragment` builds text nodes and spans whose classes
  // come from the module's own tables, so this module's no-innerHTML
  // invariant is intact and a line containing `<script>` stays inert text.
  //
  // `limit` is a budget in VISIBLE characters, so a colourised line is not
  // clipped to a couple of words by its own markup.
  function setLineText(node, text, limit) {
    const A = window.AnsiText;
    if (A && typeof A.toFragment === 'function') {
      const frag = A.toFragment(text, limit ? { limit: limit } : undefined);
      if (frag) {
        node.appendChild(frag);
        return node;
      }
    }
    // ansi.js absent: the pre-existing behaviour, escapes and all. Ugly beats
    // blank.
    node.textContent = clip(text, limit);
    return node;
  }

  // Bound a line for STORAGE. Same job as clip(), counting visible characters
  // and keeping the escape sequences whole — a sequence cut in half by the
  // storage bound would render as visible garbage at every later width.
  function clipStore(text, limit) {
    const A = window.AnsiText;
    if (A && typeof A.clip === 'function') return A.clip(text, limit);
    return clip(text, limit);
  }

  // The per-line ceiling under the CURRENT display settings. Verbose wins over
  // wrap: it is the setting that says "stop eliding".
  function lineCharLimit() {
    if (verboseOn) return verboseCapChars();
    return wrapOn ? MAX_LINE_CHARS_WRAPPED : MAX_LINE_CHARS;
  }

  function clip(str, max) {
    const s = String(str === undefined || str === null ? '' : str);
    const lim = max || MAX_LINE_CHARS;
    return s.length > lim ? s.slice(0, lim) + '…' : s;
  }

  function firstLine(str) {
    const s = String(str === undefined || str === null ? '' : str);
    const nl = s.indexOf('\n');
    return nl === -1 ? s : s.slice(0, nl);
  }

  function nearBottomSlackPx() {
    // Verbose takes the wider slack for the same reason wrap does: one record
    // can now be many visual rows tall, and a reader sitting one row off the
    // bottom of a tall record would never re-arm auto-scroll.
    return (wrapOn || verboseOn) ? NEAR_BOTTOM_PX_WRAPPED : NEAR_BOTTOM_PX;
  }

  // Render an ISO8601 UTC source timestamp as a local wall-clock time, the
  // same way the single-item modal's fmtTs does, so the two surfaces never
  // disagree. Falls back to slicing HH:MM:SS out of the UTC string when
  // local-time.js has not loaded.
  function fmtTs(iso) {
    if (!iso) return '';
    if (window.LocalTime && typeof window.LocalTime.timeOnly === 'function') {
      const local = window.LocalTime.timeOnly(iso);
      if (local) return local;
    }
    const m = String(iso).match(/T(\d\d:\d\d:\d\d)/);
    return m ? m[1] : '';
  }

  // Does this pane's SOURCE carry per-line timestamps? OBSERVED, not assumed
  // from the mode: agent transcripts always carry one, and a plain-text
  // workload log carries one exactly when its producer stamped it — the same
  // `workload` mode covers a stamped log and a file written before stamping
  // existed. So the answer is "has any line in this pane arrived with a source
  // timestamp", which starts at the mode's baseline and flips the first time a
  // stamped line shows up. Either way we never substitute the client's arrival
  // time for a source that has none — see the header comment.
  function paneHasSourceTimestamps(pane) {
    return !!(pane.sawTs || TS_SOURCE_MODES[pane.mode]);
  }

  // --- compact event formatting -------------------------------------------
  //
  // Panes render ONE LINE PER EVENT on purpose. The single-item modal is the
  // place for the full pretty-printed transcript (expandable tool inputs,
  // thinking blocks, images); a multitail's whole value is density, and a rich
  // renderer in a 132px pane shows one tool call. Plain-text workload and
  // hostjob logs are already line-oriented and pass through verbatim.

  const TOOL_ARG_KEYS = [
    'command', 'file_path', 'path', 'pattern', 'query', 'url',
    'description', 'prompt', 'notebook_path', 'skill',
  ];

  // The record's text, reduced to its first line — or kept whole in verbose
  // mode, where the pane renders it with `pre-wrap` so the line breaks show.
  // EVERY elision in this formatter goes through this function or through
  // `toolArgPreview` / the two block formatters below, which is what makes
  // "show everything" a single switch rather than a per-case audit.
  function bodyText(str) {
    const s = String(str === undefined || str === null ? '' : str);
    return verboseOn ? s : firstLine(s);
  }

  function toolArgPreview(input) {
    if (!input || typeof input !== 'object') return '';
    if (verboseOn) {
      // Verbose: the WHOLE input, indented, rather than one interesting key's
      // first line. Indented (not compact) on purpose — a Bash command or an
      // agent prompt is the thing being read, and JSON-escaped `\n` inside a
      // one-liner is not reading.
      try {
        const j = JSON.stringify(input, null, 2);
        if (j && j !== '{}') return j;
      } catch (_) {
        /* circular / unserialisable — fall through to the preview below */
      }
    }
    for (const k of TOOL_ARG_KEYS) {
      const v = input[k];
      if (typeof v === 'string' && v.trim()) return firstLine(v.trim());
    }
    try {
      const j = JSON.stringify(input);
      return j && j !== '{}' ? j : '';
    } catch (_) {
      return '';
    }
  }

  function contentBlocks(rec) {
    const msg = (rec && rec.message) || {};
    const content = msg.content;
    return Array.isArray(content) ? content : [];
  }

  function blockOfType(rec, type) {
    for (const b of contentBlocks(rec)) {
      if (b && b.type === type) return b;
    }
    return null;
  }

  function textOfToolResult(block) {
    const c = block && block.content;
    if (typeof c === 'string') return c;
    if (Array.isArray(c)) {
      if (verboseOn) {
        // Verbose: every block, not the first text one — a result whose text
        // block is preceded by an image block is exactly the case where
        // `[2 block(s)]` hides the answer.
        const parts = [];
        for (const part of c) {
          if (part && part.type === 'text' && typeof part.text === 'string') {
            parts.push(part.text);
          } else {
            parts.push(describeBlock(part));
          }
        }
        if (parts.length) return parts.join('\n');
      }
      for (const part of c) {
        if (part && part.type === 'text' && typeof part.text === 'string') {
          return part.text;
        }
      }
      return '[' + c.length + ' block(s)]';
    }
    return '';
  }

  // A non-text content block, described rather than summarised away. Binary
  // payloads (an image's base64) are reported by TYPE AND SIZE and never
  // inlined: a pane is 10 rows tall and a data URI is megabytes, so the thing
  // verbose mode owes the reader here is what it is and how big, not a
  // thumbnail. The single-item log modal is where an image renders.
  function describeBlock(part) {
    if (!part || typeof part !== 'object') return String(part);
    const src = part.source || {};
    if (part.type === 'image' || src.media_type) {
      const mt = src.media_type || 'image/?';
      const size = typeof src.data === 'string'
        ? ' ' + src.data.length + ' base64 chars' : '';
      return '[' + mt + ' ' + (src.type || 'unknown') + size + ']';
    }
    try {
      return JSON.stringify(part);
    } catch (_) {
      return '[' + (part.type || 'block') + ']';
    }
  }

  function joinedText(rec) {
    const out = [];
    for (const b of contentBlocks(rec)) {
      if (b && b.type === 'text' && typeof b.text === 'string') out.push(b.text);
      if (b && b.type === 'thinking' && typeof b.thinking === 'string') {
        out.push(b.thinking);
      }
    }
    if (out.length) return out.join(' ');
    const msg = (rec && rec.message) || {};
    return typeof msg.content === 'string' ? msg.content : '';
  }

  // The record's OWN timestamp, or '' when the source carries none. Agent
  // transcript records have an ISO8601 `timestamp`; a plain-text line carries
  // one only when its producer stamped it, in which case the server has
  // already split the prefix off into `source_ts` (app.py `_plain_line_event`).
  // Neither is ever synthesised: no timestamp means no time, not "now".
  function sourceTs(payload) {
    const rec = payload && payload.rec;
    if (rec && typeof rec.timestamp === 'string' && rec.timestamp) {
      return rec.timestamp;
    }
    if (payload && typeof payload.source_ts === 'string' && payload.source_ts) {
      return payload.source_ts;
    }
    return '';
  }

  // Returns { sigil, text, cls, ts } for one stream payload, or null to skip
  // it. `ts` is the SOURCE timestamp (ISO8601) or '' when the source has none.
  function formatPayload(payload) {
    if (!payload || typeof payload !== 'object') return null;

    if (payload.type === 'raw') {
      return { sigil: '', text: bodyText(payload.line || ''), cls: 'mt-raw', ts: '' };
    }
    if (payload.kind === 'workload_line') {
      // Plain-text tail (workload / hostjob / archived output): verbatim. A
      // stamped producer's time arrives as `source_ts` (split off the text
      // server-side); an unstamped line gets '' and shows no time.
      return {
        sigil: '',
        text: payload.text || '',
        cls: 'mt-plain',
        ts: sourceTs(payload),
      };
    }

    const rec = payload.rec || {};
    const ts = sourceTs(payload);
    switch (payload.kind) {
      case 'tool_use': {
        const tu = blockOfType(rec, 'tool_use') || {};
        const arg = toolArgPreview(tu.input);
        return {
          sigil: '▸',
          text: (tu.name || 'tool') + (arg ? ' ' + arg : ''),
          cls: 'mt-tool',
          ts: ts,
        };
      }
      case 'tool_result': {
        const tr = blockOfType(rec, 'tool_result') || {};
        const body = bodyText(textOfToolResult(tr)).trim();
        return {
          sigil: '←',
          text: body || '(empty result)',
          cls: tr.is_error ? 'mt-result mt-err' : 'mt-result',
          ts: ts,
        };
      }
      case 'assistant_text':
        return { sigil: '·', text: bodyText(joinedText(rec)).trim(), cls: 'mt-text', ts: ts };
      case 'thinking':
        return { sigil: '~', text: bodyText(joinedText(rec)).trim(), cls: 'mt-think', ts: ts };
      case 'user':
        return { sigil: '»', text: bodyText(joinedText(rec)).trim(), cls: 'mt-user', ts: ts };
      case 'user_image':
        return { sigil: '»', text: imageText(rec), cls: 'mt-user', ts: ts };
      case 'attachment':
        return { sigil: '»', text: attachmentText(rec), cls: 'mt-user', ts: ts };
      case 'system':
        return { sigil: '·', text: bodyText(rec.content || rec.subtype || 'system'), cls: 'mt-sys', ts: ts };
      case 'progress':
        return { sigil: '·', text: bodyText(rec.message || 'progress'), cls: 'mt-sys', ts: ts };
      default:
        // An unrecognised record is still evidence the agent is alive, so it
        // gets a line rather than being swallowed.
        return { sigil: '·', text: String(payload.kind || 'event'), cls: 'mt-sys', ts: ts };
    }
  }

  // `[image]` normally, and in verbose mode the count, media types and
  // payload sizes — the two cases Andrew named when asking for verbose
  // (attachments and Read output) are these two functions.
  function imageText(rec) {
    const blocks = contentBlocks(rec).filter((b) => b && b.type === 'image');
    if (!verboseOn || !blocks.length) {
      return blocks.length > 1 ? '[' + blocks.length + ' images]' : '[image]';
    }
    return '[' + blocks.length + (blocks.length === 1 ? ' image] ' : ' images] ') +
      blocks.map(describeBlock).join(' ');
  }

  // An attachment's PATH is the useful field, so verbose leads with it and
  // appends the rest of the record for the fields the path does not carry.
  function attachmentText(rec) {
    const a = (rec && rec.attachment) || {};
    const path = a.path || a.file_path || a.filename || '';
    if (!verboseOn) return path ? '[attachment] ' + path : '[attachment]';
    let json = '';
    try {
      json = JSON.stringify(a, null, 2);
    } catch (_) {
      json = '';
    }
    const head = '[attachment ' + (a.type || 'attachment') + ']' +
      (path ? ' ' + path : '');
    return json && json !== '{}' ? head + '\n' + json : head;
  }

  // --- pane construction ---------------------------------------------------

  function modeBadgeText(mode) {
    if (mode === 'workload') return 'workload';
    if (mode === 'hostjob') return 'hostjob';
    if (mode === 'subagent') return 'subagent';
    return 'agent';
  }

  function setPaneStatus(pane, text, cls) {
    pane.statusEl.textContent = text;
    pane.statusEl.className = 'mt-pane-status ' + (cls || '');
  }

  // Show / hide the "this source carries no timestamps" marker. Only visible
  // while the timestamp column is ON, because otherwise there is no empty
  // column to explain.
  function syncPaneTsMarker(pane) {
    if (!pane.noTsEl) return;
    pane.noTsEl.hidden = !(tsOn && !paneHasSourceTimestamps(pane));
  }

  // Repaint one pane's title-line metrics from `meta` (a rowMetaInfo shape).
  // Cells are appended in a fixed order and only for values that exist; the
  // whole group is hidden when nothing does, so a workload pane with no label
  // and an agent pane with no snapshot both leave the title line to the title.
  // textContent only.
  function paintPaneMeta(pane, meta) {
    const bar = pane.metaEl;
    if (!bar) return;
    while (bar.firstChild) bar.removeChild(bar.firstChild);
    const f = meta || {};
    let cells = 0;

    const model = metaValue(f.model);
    if (model) {
      const chip = el('span', 'mt-meta-model', model);
      // The row's own title is already `model: <raw id>`; pass it through
      // rather than composing a second wording for the same fact.
      if (f.modelTitle) chip.title = f.modelTitle;
      bar.appendChild(chip);
      cells += 1;
    }

    // The value is validated BEFORE the unit word is attached. Composing first
    // and checking after turns the formatter's `–` ("not known") into a cell
    // reading just `out`, which is a placeholder wearing a unit.
    const add = (cls, raw, decorate, title) => {
      const value = metaValue(raw);
      if (!value) return;
      const cell = el('span', 'mt-meta-cell ' + cls,
        decorate ? decorate(value) : value);
      if (title) cell.title = title;
      bar.appendChild(cell);
      cells += 1;
    };
    // `calls_text` / `ctx_text` / `out_text` / `age_text` are the server's
    // strings; the unit words are ours and match the header popover's columns.
    add('mt-meta-calls', f.calls, (v) => v + ' calls',
      'Tool calls this agent has made');
    add('mt-meta-ctx', f.ctx, (v) => v + ' ctx',
      'Context size (tokens) at the agent\'s last transcript write');
    add('mt-meta-out', f.out, (v) => v + ' out',
      'Output tokens this agent has produced');
    add('mt-meta-age', f.age, null,
      'Age since this agent\'s first transcript entry');
    add('mt-meta-tool', f.lastTool, (v) => 'last ' + v,
      'The last tool this agent invoked');
    // A workload / hostjob pane runs no model and has no agent counters; its
    // label is the one thing it can truthfully add, and the rest of the pane
    // header does not carry it.
    add('mt-meta-label', f.label, null,
      'The workload / hostjob label being tailed');
    // Nested panes only: this subagent is co-bound to the item rather than
    // spawned by its owner agent. Carried through from the card's own marking
    // so the two surfaces cannot disagree about which it is.
    add('mt-meta-peer', f.peer, null,
      'Co-bound to this queue item, with no recorded spawn edge to its ' +
      'owner agent — not a child of it');

    bar.hidden = cells === 0;
    if (f.statsTitle) {
      bar.title = f.statsTitle;
    } else {
      bar.removeAttribute('title');
    }
  }

  // The `N subagents` button on a queue pane. Hidden outright when the card
  // has no subagent tree, so a pane only carries the control when there is
  // something behind it. The count comes from the card and moves with it.
  //
  // It counts EVERY node in the tree, not the card's top-level `Subagents (N)`
  // number, because the two answer different questions: the card is counting
  // the roots it is about to indent, and this button is counting the panes the
  // reader is about to get. A grandchild costs a pane just like a child does.
  //
  // The word is a child element rather than part of the button's text because
  // the stylesheet drops it at phone width, where the pane header is already
  // sharing one line with the badge, the id, the status and the close button.
  // The aria-label keeps the full wording at every width.
  function paintPaneSubs(pane, count) {
    const btn = pane.subsEl;
    if (!btn) return;
    const n = count || 0;
    if (!n) {
      btn.hidden = true;
      return;
    }
    const on = subsShownFor(pane.qid);
    const noun = n === 1 ? 'subagent' : 'subagents';
    btn.hidden = false;
    btn.setAttribute('aria-expanded', on ? 'true' : 'false');
    btn.setAttribute('aria-label',
      (on ? 'Collapse the ' : 'Expand the ') + n + ' ' + noun + ' of ' + pane.qid);
    btn.title = on
      ? 'Collapse this item\'s nested subagent tails'
      : 'Tail this item\'s ' + n + ' ' + noun + ' as nested panes';
    pane.subsCaretEl.textContent = on ? '\u25be' : '\u25b8';
    pane.subsCountEl.textContent = String(n);
    pane.subsWordEl.textContent = noun;
  }

  // Show / hide ONE card's nested subagent tails, from that card's own pane.
  // Showing lets the next reconcile build them (which is also what keeps a
  // tree that GROWS while shown up to date); hiding removes them at once
  // rather than waiting a tick, and records NO dismissal — see the header
  // comment. Either way it writes a per-card deviation from the default, so a
  // card the reader collapsed by hand stays collapsed even if the default is
  // toggled under it.
  function toggleSubagents(qid) {
    const next = !subsShownFor(qid);
    subsChoice.set(qid, next);
    if (!next) {
      for (const pane of subPanesOf(qid)) destroyPane(pane);
    }
    reconcile();
  }

  // Every nested pane belonging to one card, in display order.
  function subPanesOf(qid) {
    const out = [];
    for (const pane of panesInDisplayOrder()) {
      if (pane.kind === 'subagent' && pane.qid === qid) out.push(pane);
    }
    return out;
  }

  function buildPane(info) {
    const isSub = info.kind === 'subagent';
    const wrap = el('section', 'mt-pane' + (isSub ? ' mt-pane-sub' : ''));
    wrap.setAttribute('data-pane-kind', info.kind);
    wrap.setAttribute('data-pane-key', info.key);
    if (isSub) {
      // A nested pane carries its subagent id and the qid of the card it hangs
      // off — NEVER `data-queue-id`, which would make two panes answer to the
      // same selector and would claim this tail is that queue item's own.
      wrap.setAttribute('data-subagent-id', info.target);
      wrap.setAttribute('data-parent-queue-id', info.qid);
      // Indent by real tree depth, capped. The stylesheet owns the step size
      // (and shrinks it on a phone); this only says how deep the node is.
      wrap.style.setProperty('--mt-sub-depth',
        String(Math.min(info.depth || 1, MAX_SUB_INDENT_DEPTH)));
    } else {
      wrap.setAttribute('data-queue-id', info.qid);
    }
    wrap.setAttribute('data-live-log-mode', info.mode);

    const head = el('header', 'mt-pane-head');
    head.appendChild(el('span', 'mt-pane-badge mt-badge-' + info.mode, modeBadgeText(info.mode)));
    // A subagent id is a 32-hex handle; the card shows its first 12 and so
    // does the pane, with the whole thing on the title attribute. A qid is
    // already short enough to show in full.
    const idEl = el('code', 'mt-pane-id', isSub ? info.target.slice(0, 12) : info.qid);
    if (isSub) idEl.title = info.target;
    head.appendChild(idEl);
    // THE TITLE LINE: title left, metrics right, one row for both. The title
    // is the only flexible item in this little flex row, so it takes every
    // pixel the metrics do not and the metrics end up against the far edge
    // without an alignment rule of their own — the same trick the timestamp
    // cell uses inside a log row. Wrapping the pair in one element is what
    // keeps them TOGETHER when the header wraps on a phone: the wrap moves one
    // box, and the title does not end up on a line the metrics left behind.
    const titlebar = el('div', 'mt-pane-titlebar');
    const summary = el('span', 'mt-pane-summary', info.summary);
    titlebar.appendChild(summary);
    // Metrics: whose agent this is and what it is costing. Filled from the row
    // below, hidden while there is nothing true to put in it — and hidden here
    // costs the pane nothing, because the row belongs to the title either way.
    const meta = el('div', 'mt-pane-meta');
    meta.hidden = true;
    titlebar.appendChild(meta);
    head.appendChild(titlebar);
    // Nested-tail control. Built for every queue pane and shown only when the
    // card actually has a tree (paintPaneSubs); a subagent pane never gets one
    // — expansion is per CARD and its descendants are already in the stack.
    let subsBtn = null;
    let subsCaret = null;
    let subsCount = null;
    let subsWord = null;
    if (!isSub) {
      subsBtn = el('button', 'mt-pane-subs');
      subsBtn.type = 'button';
      subsBtn.hidden = true;
      subsBtn.setAttribute('aria-expanded', 'false');
      subsCaret = el('span', 'mt-subs-caret', '\u25b8');
      subsCount = el('span', 'mt-subs-n', '0');
      subsWord = el('span', 'mt-subs-word', 'subagents');
      subsBtn.appendChild(subsCaret);
      subsBtn.appendChild(subsCount);
      subsBtn.appendChild(subsWord);
      subsBtn.addEventListener('click', (ev) => {
        ev.preventDefault();
        toggleSubagents(info.qid);
      });
      head.appendChild(subsBtn);
    }
    const noTs = el('span', 'mt-pane-nots', 'no ts');
    noTs.title =
      'This log is plain text and carries no per-line timestamps. ' +
      'Nothing is invented here — the arrival time in your browser is not ' +
      'the log\'s own time, so no time is shown.';
    noTs.hidden = true;
    head.appendChild(noTs);
    const status = el('span', 'mt-pane-status', 'waiting…');
    head.appendChild(status);
    const closeBtn = el('button', 'mt-pane-close', '×');
    closeBtn.type = 'button';
    closeBtn.setAttribute('aria-label', 'Close the ' + info.target + ' tail');
    closeBtn.title = 'Close this tail (stays in multitail mode)';
    head.appendChild(closeBtn);
    wrap.appendChild(head);

    const stream = el('pre', 'mt-pane-stream');
    stream.tabIndex = 0;
    wrap.appendChild(stream);

    const pane = {
      // Identity: what this pane IS (kind + the id its stream is opened on),
      // and which queue item it belongs to. For a queue pane target === qid;
      // for a nested one the target is a subagent id and qid is its parent.
      key: info.key,
      kind: info.kind,
      target: info.target,
      qid: info.qid,
      depth: info.depth || 0,
      mode: info.mode,
      el: wrap,
      streamEl: stream,
      statusEl: status,
      noTsEl: noTs,
      metaEl: meta,
      subsEl: subsBtn,
      subsCaretEl: subsCaret,
      subsCountEl: subsCount,
      subsWordEl: subsWord,
      es: null,
      streaming: false,   // holds a connection right now
      terminal: false,    // stream reported a real end; never reconnect
      ended: false,       // item is no longer running / stream finished
      // When the pane became ended (epoch ms, 0 = still live), plus the status
      // it settled on. Retention counts from endedAt, and the countdown suffix
      // is recomposed from the label each tick rather than parsed back out of
      // the rendered status.
      endedAt: 0,
      endedLabel: '',
      endedCls: '',
      autoscroll: true,
      // The pane's lines, as records. The DOM is a projection of these, so
      // toggling wrap / timestamps can re-render lines that have already
      // scrolled past (or a pane whose job has ended and will emit no more).
      records: [],
      // Total characters across `records`. Maintained incrementally so the
      // pane's memory bound costs an addition per line rather than a walk of
      // the whole buffer per line.
      chars: 0,
      // Set while the server is replaying its historical backfill on a
      // RECONNECT. Plain-text tails carry no resume cursor, so without this a
      // quiet workload re-prints its last 200 lines every time the server's
      // idle cap recycles the stream.
      suppressBackfill: false,
      // Whether this pane has ever rendered a real DATA line. This — not
      // "have we seen a stream-start" — is what gates backfill suppression:
      // the workload and hostjob tails emit stream-start BEFORE they try to
      // open the log, so a pane that failed to open and then retried
      // successfully would otherwise suppress its very first content.
      sawData: false,
      // Whether any line in this pane has arrived carrying a source timestamp.
      // Plain-text logs are stamped by their producer or not at all, and both
      // shapes are `workload` mode, so this is observed rather than assumed —
      // it is what decides whether the `no ts` marker is telling the truth.
      sawTs: false,
      // Stream-retry backoff. retryAt is an epoch-ms deadline (0 = none);
      // retryAttempts drives the delay and resets once data actually arrives.
      retryAt: 0,
      retryAttempts: 0,
      retryNoteShown: false,
    };

    closeBtn.addEventListener('click', () => {
      dismissed.add(pane.key);
      // Closing a card's pane takes its nested tails with it (destroyPane
      // cascades): they are presented AS a detail of this pane, and leaving
      // them behind would orphan a tail under a card that is no longer shown.
      if (pane.kind === 'queue') subsChoice.delete(pane.qid);
      destroyPane(pane);
      pumpSlots();
      paintCount();
    });

    stream.addEventListener('scroll', () => {
      const nearBottom =
        stream.scrollHeight - stream.scrollTop - stream.clientHeight < nearBottomSlackPx();
      pane.autoscroll = nearBottom;
    });

    syncPaneTsMarker(pane);
    paintPaneMeta(pane, info.meta);
    paintPaneSubs(pane, info.subCount);
    return pane;
  }

  // --- line rendering -----------------------------------------------------

  // Build the DOM row for one record under the CURRENT display settings. The
  // timestamp cell is emitted only when the toggle is on AND this record
  // actually has a source timestamp — an empty cell is never padded with
  // anything borrowed.
  //
  // ORDER IS LOAD-BEARING, and it is not the order you see: the stamp is
  // appended FIRST and renders on the right. In the wrapped / verbose modes
  // the stylesheet floats it right so that it shortens only the first line
  // box and the body's continuation rows keep the whole pane width, and a
  // float can only do that for content that FOLLOWS it in the markup. In the
  // one-row mode `order: 3` moves it back to the right-hand edge visually.
  // Appending it after the body would quietly restore a full-height gutter in
  // exactly the modes the placement exists for.
  function renderRecord(rec) {
    const row = el('div', 'mt-line ' + (rec.cls || ''));
    if (tsOn && rec.ts) {
      const shown = fmtTs(rec.ts);
      if (shown) {
        const tsEl = el('span', 'mt-ts', shown);
        tsEl.title = rec.ts;
        row.appendChild(tsEl);
      }
    }
    if (rec.sigil) row.appendChild(el('span', 'mt-sigil', rec.sigil));
    row.appendChild(setLineText(el('span', 'mt-body'), rec.text, lineCharLimit()));
    return row;
  }

  // Append one formatted event. Returns true when a row was actually added,
  // which is what the caller uses to decide whether real data has arrived.
  function appendPaneLine(pane, fmt) {
    if (!fmt) return false;
    // A formatter that produced nothing at all would render an empty row that
    // eats a line of a very short pane for no information.
    if (!fmt.text && !fmt.sigil) return false;
    const rec = {
      sigil: fmt.sigil || '',
      // Stored bounded, not clipped to the current width: the wrap and verbose
      // toggles have to be able to show MORE of this line later, so the
      // storage bound is the widest any setting can ask for. The bound counts
      // VISIBLE characters, so a heavily-coloured line is not thrown away as
      // if its escape sequences were text.
      //
      // What a later toggle CANNOT recover is what the formatter never kept:
      // outside verbose mode a multi-line record is reduced to its first line
      // when it ARRIVES, so switching verbose on shows full detail for the
      // lines that arrive after it, not retroactively. Retaining every raw
      // payload against a toggle that may never be pressed is a memory
      // multiplier on four live streams; the pill's title says so.
      text: clipStore(fmt.text, verboseCapChars()),
      cls: fmt.cls || '',
      ts: fmt.ts || '',
    };
    pane.records.push(rec);
    // First stamped line in a plain-text pane: the source DOES carry times
    // after all, so the `no ts` marker has to come down.
    if (rec.ts && !pane.sawTs) {
      pane.sawTs = true;
      syncPaneTsMarker(pane);
    }
    pane.streamEl.appendChild(renderRecord(rec));
    pane.chars = (pane.chars || 0) + rec.text.length;
    // TWO bounds, both evicting from the head: the line COUNT (a tail is a
    // window on the recent past) and the pane's total TEXT (a verbose line can
    // be ten times a normal one, so the count bound alone stops bounding
    // memory). Neither is allowed to empty the pane — the newest line always
    // survives, or a single oversized record would leave a blank tail.
    while (pane.records.length > 1 &&
           (pane.records.length > MAX_LINES_PER_PANE ||
            pane.chars > MAX_PANE_CHARS)) {
      const dropped = pane.records.shift();
      pane.chars -= (dropped && dropped.text ? dropped.text.length : 0);
      if (pane.streamEl.firstChild) {
        pane.streamEl.removeChild(pane.streamEl.firstChild);
      }
    }
    if (pane.autoscroll) pane.streamEl.scrollTop = pane.streamEl.scrollHeight;
    return true;
  }

  function appendPaneNote(pane, text) {
    appendPaneLine(pane, { sigil: '', text: text, cls: 'mt-note', ts: '' });
  }

  // Re-project every retained record under the current display settings.
  // Cheap enough for a keypress (≤400 rows per pane) and the only way a
  // toggle can affect lines that have already been rendered — including in a
  // pane whose job has ended and will never emit another line.
  function rerenderPane(pane) {
    pane.streamEl.textContent = '';
    for (const rec of pane.records) {
      pane.streamEl.appendChild(renderRecord(rec));
    }
    // Wrapping changes every scroll offset in the pane, so a preserved
    // scrollTop would land somewhere arbitrary. Anyone who was following the
    // tail keeps following it; anyone who had scrolled back keeps whatever the
    // browser clamps their offset to, which is the least surprising of the
    // available answers.
    if (pane.autoscroll) pane.streamEl.scrollTop = pane.streamEl.scrollHeight;
    syncPaneTsMarker(pane);
  }

  function rerenderAllPanes() {
    for (const pane of panes.values()) rerenderPane(pane);
  }

  // --- streams -------------------------------------------------------------

  function retryDelayMs(attempt) {
    const n = Math.max(1, attempt | 0);
    // 2^(n-1) grows fast; clamp the exponent before Math.pow so a long-lived
    // pane cannot produce Infinity.
    const steps = Math.min(n - 1, 20);
    return Math.min(STREAM_RETRY_BASE_MS * Math.pow(2, steps), STREAM_RETRY_MAX_MS);
  }

  function retryStatusText(pane) {
    const left = Math.max(0, Math.ceil((pane.retryAt - Date.now()) / 1000));
    return 'waiting for log · retry ' + left + 's';
  }

  // The log is not there YET. Back off and try again — do NOT mark the pane
  // terminal, and release the stream slot so a pane that can use one now gets
  // it (a pane in backoff is not a pane waiting for a slot).
  function scheduleStreamRetry(pane, kind, detail) {
    pane.retryAttempts += 1;
    pane.retryAt = Date.now() + retryDelayMs(pane.retryAttempts);
    // One note, not one per attempt: the status line carries the countdown, so
    // repeating the explanation every few seconds would push the pane's real
    // output off the top of a 10-line viewport.
    if (!pane.retryNoteShown) {
      appendPaneNote(pane, (detail || kind) + ' — no log yet, retrying');
      pane.retryNoteShown = true;
    }
    setPaneStatus(pane, retryStatusText(pane), 'mt-idle');
    releaseSlot(pane);
  }

  function handlePayload(pane, payload) {
    if (payload.type === 'meta') {
      switch (payload.kind) {
        case 'stream-start':
          // Suppress a replayed backfill only when this pane has already
          // shown data. A stream-start on a pane that has shown NOTHING is a
          // first look at the log (including the first look after a retry),
          // and its backfill is content the reader has not seen.
          if (pane.sawData) pane.suppressBackfill = true;
          setPaneStatus(pane, pane.sawData ? 'live' : 'live · no output yet', 'mt-ok');
          return;
        case 'backfill-begin':
          return;
        case 'backfill-end':
          pane.suppressBackfill = false;
          return;
        case 'resumed':
          pane.suppressBackfill = false;
          setPaneStatus(pane, pane.sawData ? 'live' : 'live · no output yet', 'mt-ok');
          return;
        case 'workload-end': {
          const code = payload.exit_code;
          const label = (code === null || code === undefined)
            ? 'ended'
            : 'ended · exit ' + code;
          markEnded(pane, label, code ? 'mt-err' : 'mt-done');
          return;
        }
        case 'archive-end':
          markEnded(pane, 'ended', 'mt-done');
          return;
        case 'idle-timeout':
          // The server's idle cap recycled the stream; EventSource
          // reconnects on its own. Not an error, and not an end. A pane that
          // has shown nothing yet says so rather than implying it is stuck.
          setPaneStatus(pane, pane.sawData ? 'idle' : 'idle · no output yet', 'mt-idle');
          return;
        case 'lifetime-timeout':
          setPaneStatus(pane, 'reconnecting', 'mt-idle');
          return;
        default:
          return;
      }
    }
    if (payload.type === 'error') {
      const kind = String(payload.kind || 'error');
      if (RETRYABLE_ERROR_KINDS[kind]) {
        // "Not there YET" — the log file or the state-file row did not exist
        // at that instant. Keep trying; see the header comment.
        scheduleStreamRetry(pane, kind, payload.error);
        return;
      }
      // An error shape we cannot reason about IS terminal for this attempt:
      // the server closed after one event, so letting EventSource retry
      // forever would just spin, and spinning on an unknown fault is worse
      // than naming it.
      setPaneStatus(pane, kind, 'mt-err');
      appendPaneNote(pane, payload.error || kind);
      pane.terminal = true;
      releaseSlot(pane);
      return;
    }
    if (pane.suppressBackfill) return;
    if (appendPaneLine(pane, formatPayload(payload))) {
      // Real content arrived: the log exists, so the backoff has served its
      // purpose and resets (a later transient failure starts from 3s again).
      if (!pane.sawData) {
        pane.sawData = true;
        setPaneStatus(pane, 'live', 'mt-ok');
      }
      pane.retryAttempts = 0;
      pane.retryNoteShown = false;
    }
  }

  // The endpoint a pane tails. Both shapes emit the IDENTICAL SSE wire format
  // (the server tails a JSONL transcript through the same `_tail_jsonl` either
  // way), which is why everything below this line — the formatter, the retry
  // set, the backfill suppression — is unchanged by there being two of them.
  function streamUrl(pane) {
    if (pane.kind === 'subagent') {
      return '/api/subagent/' + encodeURIComponent(pane.target) + '/stream';
    }
    return '/api/queue/' + encodeURIComponent(pane.target) + '/stream';
  }

  function connectPane(pane) {
    if (pane.es || pane.terminal) return;
    pane.streaming = true;
    pane.retryAt = 0;
    setPaneStatus(pane, 'connecting…', '');
    let es;
    try {
      es = new EventSource(streamUrl(pane));
    } catch (_) {
      pane.streaming = false;
      pane.terminal = true;
      setPaneStatus(pane, 'unavailable', 'mt-err');
      return;
    }
    pane.es = es;
    es.onmessage = (ev) => {
      // A closed-and-replaced stream can still deliver a queued event; ignore
      // anything that is no longer this pane's live connection.
      if (pane.es !== es) return;
      let payload;
      try {
        payload = JSON.parse(ev.data);
      } catch (_) {
        return;
      }
      handlePayload(pane, payload);
    };
    es.onerror = () => {
      if (pane.es !== es) return;
      // EventSource retries by itself unless the stream ended for a reason we
      // already recorded. Say so rather than leaving a stale "live".
      if (pane.terminal) {
        try { es.close(); } catch (_) {}
        pane.es = null;
        releaseSlot(pane);
        return;
      }
      setPaneStatus(pane, 'reconnecting', 'mt-idle');
    };
  }

  function releaseSlot(pane) {
    if (pane.es) {
      try { pane.es.close(); } catch (_) {}
      pane.es = null;
    }
    pane.streaming = false;
    pumpSlots();
  }

  function markEnded(pane, label, cls) {
    pane.ended = true;
    pane.terminal = true;
    // An item that stopped running never gets another connection attempt,
    // however hopeful its backoff was — a job that finished before its log
    // ever appeared settles here instead of retrying forever.
    pane.retryAt = 0;
    pane.el.classList.add('mt-ended');
    // First end wins the clock. A stream can report `workload-end` and then
    // have its row leave the running section a tick later; the retention delay
    // is measured from when the reader first saw `ended`, not from the last
    // bookkeeping event about it.
    if (!pane.endedAt) pane.endedAt = Date.now();
    pane.endedLabel = label;
    pane.endedCls = cls || 'mt-done';
    paintEndedStatus(pane);
    releaseSlot(pane);
  }

  // When this ended pane is due to be cleared (epoch ms), or 0 when it never
  // is — either because retention is `keep` or because it has not ended.
  function endedClearAt(pane) {
    const ms = retentionMs();
    if (!ms || !pane.ended || !pane.endedAt) return 0;
    return pane.endedAt + ms;
  }

  // `ended · exit 0 · clears in 42s`. The countdown is what makes the clear
  // predictable instead of startling: the pane that is about to go is the one
  // saying so, in time to switch the pill to `keep`.
  function paintEndedStatus(pane) {
    if (!pane.ended) return;
    const base = pane.endedLabel || 'ended';
    const due = endedClearAt(pane);
    if (!due) {
      setPaneStatus(pane, base, pane.endedCls);
      return;
    }
    const left = Math.max(0, Math.ceil((due - Date.now()) / 1000));
    setPaneStatus(pane, base + ' · clears in ' + left + 's', pane.endedCls);
  }

  // Remove every ended pane whose retention has elapsed, and refresh the
  // countdown on the ones that are still within it. Driven by reconcile()'s
  // existing tick — see the header comment. `seen` is the set of currently
  // eligible qids; it decides whether a clear needs the rebuild suppression.
  function sweepEndedPanes(seen) {
    const now = Date.now();
    let removed = 0;
    for (const pane of Array.from(panes.values())) {
      if (!pane.ended) continue;
      const due = endedClearAt(pane);
      if (due && now >= due) {
        // Only a row that is STILL eligible could be rebuilt on the next pass,
        // and only that case needs suppressing. `dismissed` is not touched:
        // this was a timer, not the operator saying "I closed that".
        if (seen && seen.has(pane.key)) cleared.add(pane.key);
        destroyPane(pane);
        removed += 1;
        continue;
      }
      paintEndedStatus(pane);
    }
    return removed;
  }

  // A pane that would take a connection right now: not already streaming, not
  // finished, and not inside a retry backoff. A pane in backoff is distinct
  // from a pane waiting for a slot — only the former is on a timer.
  function wantsSlot(pane) {
    if (pane.streaming || pane.terminal || pane.es) return false;
    if (pane.retryAt && pane.retryAt > Date.now()) return false;
    return true;
  }

  // The panes in the order they appear ON SCREEN, which is the order stream
  // slots are handed out. Map order is creation order, and a nested subagent
  // pane is INSERTED beside its parent rather than appended, so the two
  // diverge the moment a tree is expanded. Reading the DOM keeps one rule —
  // "the order you see is the order slots are given" — instead of two.
  // A pane not yet placed in the DOM still gets a turn, last.
  function panesInDisplayOrder() {
    const out = [];
    const kids = panesEl ? panesEl.children : [];
    for (let i = 0; i < kids.length; i++) {
      const pane = panes.get(kids[i].getAttribute('data-pane-key') || '');
      if (pane) out.push(pane);
    }
    if (out.length !== panes.size) {
      for (const pane of panes.values()) {
        if (out.indexOf(pane) === -1) out.push(pane);
      }
    }
    return out;
  }

  // THE ORDER STREAM SLOTS ARE HANDED OUT: every QUEUE pane first, then every
  // SUBAGENT pane, each group in the order it appears on screen.
  //
  // This used to be plain display order, on the reasoning that a subagent the
  // reader had just EXPANDED outranks a later card's pane — which is the
  // honest reading of having asked for it. Subagent tails are now shown
  // without being asked for, so nobody asked, and plain display order would
  // have meant one busy item silently taking the whole connection budget: an
  // item with three children would hold all four slots and the SECOND running
  // task in the list would get nothing, in the window whose entire question is
  // "what is everything doing".
  //
  // Tiering is the smallest fix that keeps both properties. Within a tier the
  // order you see is still the order slots are given; across tiers, top-level
  // coverage comes first. It only changes the case where the top-level panes
  // do not already fill the cap — with four or more items running, an expanded
  // tree waited for a slot before this change and waits for one now.
  function slotOrder() {
    const ordered = panesInDisplayOrder();
    const out = [];
    for (const pane of ordered) if (pane.kind !== 'subagent') out.push(pane);
    for (const pane of ordered) if (pane.kind === 'subagent') out.push(pane);
    return out;
  }

  // Give connections to the earliest panes that want one, up to the cap.
  function pumpSlots() {
    const ordered = slotOrder();
    let live = 0;
    for (const pane of ordered) {
      if (pane.streaming) live += 1;
    }
    for (const pane of ordered) {
      if (live >= MAX_LIVE_STREAMS) break;
      if (!wantsSlot(pane)) continue;
      connectPane(pane);
      live += 1;
    }
    // Everything still unconnected is explicitly waiting, and the two reasons
    // are different: a backoff counts down, a slot queue does not.
    for (const pane of ordered) {
      if (pane.streaming || pane.terminal || pane.es) continue;
      if (pane.retryAt && pane.retryAt > Date.now()) {
        setPaneStatus(pane, retryStatusText(pane), 'mt-idle');
      } else {
        setPaneStatus(pane, 'waiting for a stream slot', 'mt-idle');
      }
    }
    // The header count reports how many panes are streaming / waiting for a
    // log, so it has to be repainted whenever those counts move — including
    // the retry path, which reaches here via releaseSlot() rather than through
    // the reconcile tick.
    paintCount();
  }

  function destroyPane(pane) {
    if (pane.es) {
      try { pane.es.close(); } catch (_) {}
      pane.es = null;
    }
    pane.streaming = false;
    if (pane.el.parentNode) pane.el.parentNode.removeChild(pane.el);
    panes.delete(pane.key);
    // A card's nested tails are shown AS a detail of its pane, so they go with
    // it however it went — dismissed by hand, cleared by retention, or torn
    // down when the mode closes. A nested pane left behind would be a tail
    // hanging under nothing.
    if (pane.kind === 'queue') {
      for (const sub of subPanesOf(pane.qid)) destroyPane(sub);
    }
  }

  // --- reconcile + chrome --------------------------------------------------

  function paintCount() {
    const total = panes.size;
    let live = 0;
    let ended = 0;
    let retrying = 0;
    let nested = 0;
    for (const pane of panes.values()) {
      if (pane.streaming) live += 1;
      if (pane.ended) ended += 1;
      if (!pane.terminal && pane.retryAt) retrying += 1;
      if (pane.kind === 'subagent') nested += 1;
    }
    const bits = [total + (total === 1 ? ' tail' : ' tails')];
    // Named rather than folded into the total: a reader who expanded a tree
    // should be able to see how much of the stack is the tree.
    if (nested) bits.push(nested + (nested === 1 ? ' subagent' : ' subagents'));
    if (live < total - ended) {
      bits.push(live + ' streaming (cap ' + MAX_LIVE_STREAMS + ')');
    }
    if (retrying) bits.push(retrying + ' waiting for a log');
    if (ended) bits.push(ended + ' ended');
    if (countEl) countEl.textContent = bits.join(' · ');
    if (emptyEl) emptyEl.hidden = total > 0;
  }

  // Everything that should have a pane right now, in DISPLAY order: each
  // eligible row, followed by the nested subagent tails of the cards that are
  // showing them. One pass, so the order panes are built in is the order they
  // are placed in. It is NOT the order they are given stream slots any more —
  // see slotOrder().
  function eligibleInfos() {
    const out = [];
    for (const row of eligibleRows()) {
      const info = rowInfo(row);
      if (!info.qid || !info.mode) continue;
      out.push(info);
      if (!subsShownFor(info.qid)) continue;
      // A card's nested tails are eligible only while the card's OWN pane is:
      // a nested tail under a pane the reader dismissed (or retention cleared)
      // would be attributed to nothing on screen.
      if (dismissed.has(info.key) || cleared.has(info.key)) continue;
      const nodes = row.querySelectorAll(SUB_NODE_SELECTOR);
      for (let i = 0; i < nodes.length; i++) {
        const sub = subInfo(row, nodes[i], info.qid);
        if (sub.target) out.push(sub);
      }
    }
    return out;
  }

  // Put a new pane where it belongs: a queue pane at the end of the stack, a
  // nested one immediately after its parent and after any nested panes that
  // parent already has, so a card and its subtree stay one contiguous group.
  function placePane(pane) {
    if (pane.kind !== 'subagent') {
      panesEl.appendChild(pane.el);
      return;
    }
    const parent = panes.get(paneKey('queue', pane.qid));
    if (!parent || parent.el.parentNode !== panesEl) {
      panesEl.appendChild(pane.el);
      return;
    }
    let anchor = parent.el;
    let next = anchor.nextSibling;
    while (next && next.getAttribute &&
           next.getAttribute('data-parent-queue-id') === pane.qid) {
      anchor = next;
      next = anchor.nextSibling;
    }
    panesEl.insertBefore(pane.el, anchor.nextSibling);
  }

  function reconcile() {
    if (!open) return;
    const infos = eligibleInfos();
    const seen = new Set();
    for (const info of infos) seen.add(info.key);
    // An auto-clear suppresses a rebuild only for as long as the row it came
    // from stays continuously eligible. The moment it is not, the entry goes,
    // so a requeued job that reuses the qid gets a fresh pane rather than being
    // permanently suppressed by a stale clear.
    for (const key of Array.from(cleared)) {
      if (!seen.has(key)) cleared.delete(key);
    }
    for (const info of infos) {
      if (dismissed.has(info.key) || cleared.has(info.key)) continue;
      const existing = panes.get(info.key);
      if (!existing) {
        const pane = buildPane(info);
        panes.set(pane.key, pane);
        placePane(pane);
        continue;
      }
      // Summary text can change (the queue record is editable); keep it fresh
      // without touching the stream.
      const sumEl = existing.el.querySelector('.mt-pane-summary');
      if (sumEl && sumEl.textContent !== info.summary) sumEl.textContent = info.summary;
      // Same for the title-line counters: refresh.js rebuilt this row (and its
      // agent-stats cell) since the last tick, so re-read it. An ENDED pane is
      // not in `infos` at all, so its metrics keep the last values they had —
      // which is the honest answer for an agent that has returned.
      paintPaneMeta(existing, info.meta);
      // And the subagent count, which moves as the card's tree grows.
      paintPaneSubs(existing, info.subCount);
    }
    // A pane whose row stopped being eligible (finished, abandoned, moved out
    // of the running section) is marked ENDED, never removed — see the header
    // comment. Its slot goes back to the pool. A nested pane ends the same way
    // and for the same reason: its node left its card's tree, or the card
    // itself stopped running and took the whole group with it.
    for (const pane of Array.from(panes.values())) {
      if (!seen.has(pane.key) && !pane.ended) {
        markEnded(
          pane,
          pane.kind === 'subagent'
            ? 'ended · no longer listed'
            : 'ended · no longer running',
          'mt-done',
        );
      }
    }
    // Ended-pane retention rides this tick too — an ended pane holds no stream
    // slot, so clearing one cannot disturb the pump below.
    sweepEndedPanes(seen);
    // pumpSlots is also what expires stream-retry backoffs, which is why this
    // tick is the retry clock and no second timer exists.
    pumpSlots();
    paintCount();
  }

  function syncToggleButton() {
    const btn = document.getElementById('multitail-toggle');
    if (btn) btn.setAttribute('aria-pressed', open ? 'true' : 'false');
  }

  function syncDisplayButtons() {
    if (wrapBtn) wrapBtn.setAttribute('aria-pressed', wrapOn ? 'true' : 'false');
    if (tsBtn) tsBtn.setAttribute('aria-pressed', tsOn ? 'true' : 'false');
    if (verboseBtn) {
      verboseBtn.setAttribute('aria-pressed', verboseOn ? 'true' : 'false');
    }
    if (subsModeBtn) {
      subsModeBtn.setAttribute('aria-pressed', subsShown ? 'true' : 'false');
    }
    overlay.classList.toggle('mt-wrap', wrapOn);
    overlay.classList.toggle('mt-show-ts', tsOn);
    overlay.classList.toggle('mt-verbose', verboseOn);
    syncVerboseCapButton();
    syncRetainButton();
  }

  // The verbose CAP pill. It is shown only while verbose is ON, because the
  // cap is verbose mode's ceiling and does nothing at all with verbose off —
  // the same judgment the `no ts` marker makes about explaining a column that
  // is not there. Pressing `v` reveals it right beside the pill just pressed,
  // which is where a reader who wants more output is already looking.
  //
  // A VALUE, not a toggle, so no aria-pressed: the label is the state, and the
  // accessible name spells out what that state means rather than leaving
  // `cap 16K` to be guessed at.
  function syncVerboseCapButton() {
    // The whole ROW goes with the control. In the options sheet the control
    // has a name and a note beside it, and hiding only the button would leave
    // a labelled row describing something that is not there.
    if (vcapRow) vcapRow.hidden = !verboseOn;
    if (!vcapBtn) return;
    const opt = verboseCapOption();
    vcapBtn.hidden = !verboseOn;
    vcapBtn.textContent = opt.label;
    vcapBtn.setAttribute('data-verbose-cap', opt.key);
    vcapBtn.setAttribute(
      'aria-label',
      'Verbose mode shows at most ' + opt.aria +
      ' of a single line. Activate to change (x).');
    vcapBtn.title =
      'How much of one line verbose mode shows before clipping it (x). ' +
      'Cycles ' + VERBOSE_CAP_OPTIONS.map((o) => o.label).join(' → ') +
      '. Raising it applies to lines received from now on; lowering it ' +
      'applies at once. Your choice is remembered in this browser.';
  }

  // The retention pill shows a VALUE, so it is not an aria-pressed toggle: the
  // label is the state, and the accessible name spells out what that state
  // means rather than leaving `clear 5m` to be guessed at.
  function syncRetainButton() {
    if (!retainBtn) return;
    const opt = retentionOption();
    retainBtn.textContent = opt.label;
    retainBtn.setAttribute('data-retention', opt.key);
    retainBtn.setAttribute(
      'aria-label', 'Ended tails are ' + opt.aria + '. Activate to change (c).');
    retainBtn.title =
      'How long a finished tail stays in the window (c). Cycles ' +
      RETENTION_OPTIONS.map((o) => o.label).join(' → ') +
      '. Your choice is remembered in this browser.';
  }

  function setRetention(key) {
    const next = RETENTION_BY_KEY[key] ? key : DEFAULT_RETENTION_KEY;
    if (next !== retentionKey) {
      retentionKey = next;
      storeRetention(next);
    }
    syncRetainButton();
    // Apply at once instead of waiting up to RECONCILE_MS: switching to `keep`
    // must drop a visible countdown immediately, and switching to a shorter
    // delay must clear what that delay has already elapsed for.
    if (open) {
      const seen = new Set();
      for (const row of eligibleRows()) {
        const qid = row.getAttribute('data-queue-id');
        if (qid) seen.add(qid);
      }
      sweepEndedPanes(seen);
      paintCount();
    }
  }

  // Lowering the cap takes effect on what is already on screen (rendering
  // clips again); raising it widens the lines that arrive after, because the
  // stored text was bounded at the cap in force when it arrived. Either way
  // the panes are re-projected, so the change is visible immediately rather
  // than on the next line.
  function setVerboseCap(key) {
    const next = VERBOSE_CAP_BY_KEY[key] ? key : DEFAULT_VERBOSE_CAP_KEY;
    if (next !== verboseCapKey) {
      verboseCapKey = next;
      storeVerboseCap(next);
      syncVerboseCapButton();
      rerenderAllPanes();
      return;
    }
    syncVerboseCapButton();
  }

  function cycleVerboseCap() {
    let idx = 0;
    for (let i = 0; i < VERBOSE_CAP_OPTIONS.length; i++) {
      if (VERBOSE_CAP_OPTIONS[i].key === verboseCapKey) { idx = i; break; }
    }
    setVerboseCap(VERBOSE_CAP_OPTIONS[(idx + 1) % VERBOSE_CAP_OPTIONS.length].key);
  }

  function cycleRetention() {
    let idx = 0;
    for (let i = 0; i < RETENTION_OPTIONS.length; i++) {
      if (RETENTION_OPTIONS[i].key === retentionKey) { idx = i; break; }
    }
    setRetention(RETENTION_OPTIONS[(idx + 1) % RETENTION_OPTIONS.length].key);
  }

  function setWrap(on) {
    const next = !!on;
    if (next === wrapOn) return;
    wrapOn = next;
    storeFlag(WRAP_STORAGE_KEY, wrapOn);
    syncDisplayButtons();
    rerenderAllPanes();
  }

  function setTimestamps(on) {
    const next = !!on;
    if (next === tsOn) return;
    tsOn = next;
    storeFlag(TS_STORAGE_KEY, tsOn);
    syncDisplayButtons();
    rerenderAllPanes();
  }

  // VERBOSE. Retroactive for what the records still hold — the stored text is
  // bounded at the verbose width, so turning it on immediately widens every
  // retained line that was clipped by the narrower one — and fully in effect
  // for every line that arrives after, which is where the un-elided
  // multi-line bodies, tool arguments and attachment records come from.
  function setVerbose(on) {
    const next = !!on;
    if (next === verboseOn) return;
    verboseOn = next;
    storeFlag(VERBOSE_STORAGE_KEY, verboseOn);
    syncDisplayButtons();
    rerenderAllPanes();
  }

  // THE DEFAULT every card starts from, not a command that overrides the ones
  // the reader has already collapsed by hand: `subsChoice` entries survive it,
  // so turning it off closes the trees nobody touched and leaves a card that
  // was explicitly shown stay shown.
  //
  // Turning it OFF destroys the newly-unwanted nested panes here rather than
  // leaving them to reconcile, because reconcile's job for a pane that left
  // the eligible set is to mark it ENDED (a job that finished keeps its
  // output) — which is the wrong answer for a pane the reader just switched
  // off. Same reason toggleSubagents() destroys rather than waits.
  function setSubagents(on) {
    const next = !!on;
    if (next === subsShown) return;
    subsShown = next;
    storeFlag(SUBS_STORAGE_KEY, subsShown);
    syncDisplayButtons();
    if (open) {
      for (const pane of Array.from(panes.values())) {
        if (pane.kind !== 'subagent') continue;
        if (!subsShownFor(pane.qid)) destroyPane(pane);
      }
      reconcile();
    }
  }

  function toggleWrap() { setWrap(!wrapOn); }
  function toggleTimestamps() { setTimestamps(!tsOn); }
  function toggleVerbose() { setVerbose(!verboseOn); }
  function toggleSubagentsDefault() { setSubagents(!subsShown); }

  // --- the options dialog --------------------------------------------------
  //
  // The site's own modal, wired the way action.js wires its confirm dialog:
  // Esc, backdrop click and an explicit close button all dismiss; Tab is
  // trapped inside the panel; focus goes to the close button on open and back
  // to the button that opened it on close.
  //
  // It deliberately does NOT add `body.modal-open`. That class exists to stop
  // the page behind a dialog scrolling, and `body.multitail-open` — which is
  // on for as long as this dialog can exist at all — already does exactly
  // that. Adding a second owner of one `overflow: hidden` means whichever
  // dialog closes last decides whether the page scrolls again.
  //
  // Settings apply LIVE. There is no OK/Cancel because every control here is
  // a projection of the panes behind the dialog and already persists on the
  // spot; a confirm step would mean holding a pending copy of six settings
  // and a way to revert them, to buy nothing.
  let optionsOpen = false;
  let optionsTrigger = null;

  function syncOptionsButton() {
    if (optionsBtn) {
      optionsBtn.setAttribute('aria-expanded', optionsOpen ? 'true' : 'false');
    }
  }

  // Focusable controls inside the panel, for the Tab trap. Read live rather
  // than cached: the verbose-cap row comes and goes with verbose.
  function optionsFocusables() {
    if (!optionsPanel) return [];
    const nodes = optionsPanel.querySelectorAll(
      'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])');
    return Array.prototype.filter.call(nodes, (n) => {
      if (n.disabled) return false;
      if (n.hidden) return false;
      // A control inside the hidden verbose-cap row is not reachable either.
      return !(n.closest && n.closest('[hidden]'));
    });
  }

  function openOptions() {
    if (!optionsModal || optionsOpen) return;
    optionsOpen = true;
    optionsTrigger = optionsBtn;
    optionsModal.hidden = false;
    syncOptionsButton();
    if (optionsCloseBtn) {
      setTimeout(() => { try { optionsCloseBtn.focus(); } catch (_) {} }, 0);
    }
  }

  function closeOptions() {
    if (!optionsModal || !optionsOpen) return;
    optionsOpen = false;
    optionsModal.hidden = true;
    syncOptionsButton();
    const back = optionsTrigger;
    optionsTrigger = null;
    if (back && typeof back.focus === 'function') {
      try { back.focus(); } catch (_) {}
    }
  }

  function toggleOptions() {
    if (optionsOpen) closeOptions();
    else openOptions();
  }

  function openMode() {
    if (open) return;
    open = true;
    dismissed.clear();
    cleared.clear();
    // A fresh open starts from the DEFAULT for every card, like it starts with
    // nothing dismissed: a card collapsed by hand in an earlier sitting was a
    // statement about that sitting's tree, not a standing preference. The
    // standing preference is `subsShown`, which persists.
    subsChoice.clear();
    overlay.hidden = false;
    document.body.classList.add('multitail-open');
    syncToggleButton();
    syncDisplayButtons();
    reconcile();
    if (reconcileTimer === null) {
      reconcileTimer = setInterval(reconcile, RECONCILE_MS);
    }
    if (exitBtn) {
      setTimeout(() => { try { exitBtn.focus(); } catch (_) {} }, 0);
    }
  }

  function closeMode() {
    if (!open) return;
    open = false;
    // The settings sheet cannot outlive the window it configures — left open,
    // it would be the first thing the next `m` showed.
    closeOptions();
    if (reconcileTimer !== null) {
      clearInterval(reconcileTimer);
      reconcileTimer = null;
    }
    for (const pane of Array.from(panes.values())) destroyPane(pane);
    panes.clear();
    dismissed.clear();
    cleared.clear();
    subsChoice.clear();
    if (panesEl) panesEl.textContent = '';
    overlay.hidden = true;
    document.body.classList.remove('multitail-open');
    syncToggleButton();
    paintCount();
  }

  function toggleMode() {
    if (open) closeMode();
    else openMode();
  }

  // --- wiring --------------------------------------------------------------

  if (exitBtn) exitBtn.addEventListener('click', closeMode);
  // The overlay header is inside the `data-no-morph` subtree, so unlike the
  // topbar pill below these buttons are never rebuilt and can be bound
  // directly (same as the exit button).
  if (wrapBtn) {
    wrapBtn.addEventListener('click', (ev) => { ev.preventDefault(); toggleWrap(); });
  }
  if (tsBtn) {
    tsBtn.addEventListener('click', (ev) => { ev.preventDefault(); toggleTimestamps(); });
  }
  if (retainBtn) {
    retainBtn.addEventListener('click', (ev) => { ev.preventDefault(); cycleRetention(); });
  }
  if (verboseBtn) {
    verboseBtn.addEventListener('click', (ev) => { ev.preventDefault(); toggleVerbose(); });
  }
  if (vcapBtn) {
    vcapBtn.addEventListener('click', (ev) => { ev.preventDefault(); cycleVerboseCap(); });
  }
  if (subsModeBtn) {
    subsModeBtn.addEventListener('click', (ev) => {
      ev.preventDefault();
      toggleSubagentsDefault();
    });
  }
  if (optionsBtn) {
    optionsBtn.addEventListener('click', (ev) => { ev.preventDefault(); toggleOptions(); });
  }
  // Backdrop, the × and anything else tagged [data-modal-dismiss] — the same
  // delegated dismissal action.js and live-log.js use, so a control added to
  // the panel later needs no new listener to be able to close it.
  if (optionsModal) {
    optionsModal.addEventListener('click', (ev) => {
      if (ev.target.closest && ev.target.closest('[data-modal-dismiss]')) {
        ev.preventDefault();
        closeOptions();
      }
    });
  }
  syncOptionsButton();
  // Every pill is server-rendered in its DEFAULT state, so a stored choice has
  // to be reflected before the mode is ever opened — otherwise the header says
  // `wrap` is off while the panes wrap. The overlay is hidden until then, so
  // there is nothing to flash.
  syncDisplayButtons();

  // Delegated toggle click: #topbar-meta is rebuilt by refresh.js every tick,
  // so a listener bound to the button itself would die on the first merge
  // (same reason the density toggle is delegated).
  document.addEventListener('click', (ev) => {
    const btn = ev.target && ev.target.closest && ev.target.closest('#multitail-toggle');
    if (!btn) return;
    ev.preventDefault();
    toggleMode();
  });

  // Keep the rebuilt button's pressed state honest.
  setInterval(syncToggleButton, 1000);

  function isTypingTarget(el2) {
    if (!el2) return false;
    const tag = el2.tagName;
    if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return true;
    return !!el2.isContentEditable;
  }

  // Another dialog is up (single-item log modal, stop/abandon confirm). Those
  // own the keyboard; `m` must not yank the window out from under them.
  function otherDialogOpen() {
    const dialogs = document.querySelectorAll('[data-no-morph]');
    for (const d of dialogs) {
      if (d !== overlay && !d.hidden) return true;
    }
    return false;
  }

  document.addEventListener('keydown', (ev) => {
    if (ev.ctrlKey || ev.metaKey || ev.altKey) return;
    if (isTypingTarget(document.activeElement)) return;
    // THE OPTIONS SHEET OWNS TAB AND ESC WHILE IT IS UP. Esc closes the sheet
    // and NOT the mode behind it — dismissing a dialog should undo the last
    // thing that opened, not two things — and Tab cycles inside the panel, the
    // same trap action.js puts on its confirm dialog. The setting keys below
    // are deliberately NOT gated on the sheet being closed: they are the same
    // settings the sheet shows, the sheet repaints as they change, and the
    // panes behind it are the feedback.
    if (optionsOpen) {
      if (ev.key === 'Escape') {
        ev.preventDefault();
        closeOptions();
        return;
      }
      if (ev.key === 'Tab') {
        const nodes = optionsFocusables();
        if (!nodes.length) return;
        const first = nodes[0];
        const last = nodes[nodes.length - 1];
        if (ev.shiftKey && document.activeElement === first) {
          ev.preventDefault();
          last.focus();
        } else if (!ev.shiftKey && document.activeElement === last) {
          ev.preventDefault();
          first.focus();
        }
        return;
      }
      // `m` would take the whole window out from under the dialog configuring
      // it. The dialog is what Esc dismisses; the mode is what Esc dismisses
      // once the dialog is gone.
      if (ev.key === 'm' || ev.key === 'M') {
        ev.preventDefault();
        return;
      }
    }
    if (open && ev.key === 'Escape') {
      ev.preventDefault();
      closeMode();
      return;
    }
    // `w` (wrap) and `t` (timestamps) are MODE-LOCAL: they only act while the
    // overlay is up, and they stay out of the way when another dialog owns the
    // keyboard. Both were free — the site binds j/k (row + stream scroll),
    // Enter/Space (open), g/G (jump), `/` (search focus), Tab (modal focus
    // trap), Esc (dismiss) and `m` (this mode), and nothing binds w or t.
    // Modified chords are returned above untouched, so Ctrl/Cmd+W still closes
    // the tab and Ctrl/Cmd+T still opens one.
    if (ev.key === 'w' || ev.key === 'W') {
      if (!open || otherDialogOpen()) return;
      ev.preventDefault();
      toggleWrap();
      return;
    }
    if (ev.key === 't' || ev.key === 'T') {
      if (!open || otherDialogOpen()) return;
      ev.preventDefault();
      toggleTimestamps();
      return;
    }
    // `v` is verbose. Mode-local like the others, and free on this site
    // (nothing binds a bare v); Ctrl/Cmd+V is returned above untouched, so
    // paste still works.
    if (ev.key === 'v' || ev.key === 'V') {
      if (!open || otherDialogOpen()) return;
      ev.preventDefault();
      toggleVerbose();
      return;
    }
    // `x` cycles the VERBOSE CAP — "ma(x) output". Gated on verbose being on,
    // the same condition that decides whether its pill is visible: a key that
    // silently changes a setting whose control is not on screen is a key that
    // looks broken. Free on this site, and Ctrl/Cmd+X is returned above
    // untouched so cut still works.
    if (ev.key === 'x' || ev.key === 'X') {
      if (!open || !verboseOn || otherDialogOpen()) return;
      ev.preventDefault();
      cycleVerboseCap();
      return;
    }
    // `c` cycles the ended-pane retention. Also mode-local, also a free key
    // (nothing on the site binds it), and Ctrl/Cmd+C is returned above
    // untouched so copying selected log text still works.
    if (ev.key === 'c' || ev.key === 'C') {
      if (!open || otherDialogOpen()) return;
      ev.preventDefault();
      cycleRetention();
      return;
    }
    // `s` flips whether cards show their nested subagent tails by default.
    // Mode-local like the rest, and free on this site; Ctrl/Cmd+S is returned
    // above untouched so the browser's save still works.
    if (ev.key === 's' || ev.key === 'S') {
      if (!open || otherDialogOpen()) return;
      ev.preventDefault();
      toggleSubagentsDefault();
      return;
    }
    // `o` opens the options sheet — the single entry point that replaced the
    // header's row of pills. Mode-local, and free on this site.
    if (ev.key === 'o' || ev.key === 'O') {
      if (!open || otherDialogOpen()) return;
      ev.preventDefault();
      toggleOptions();
      return;
    }
    // `m` is the mode toggle. Chosen because it is a free single key, and
    // unlike `/` — which is Firefox's quick-find — a bare `m` has no default
    // browser action to swallow.
    if (ev.key !== 'm' && ev.key !== 'M') return;
    if (!open && otherDialogOpen()) return;
    ev.preventDefault();
    toggleMode();
  });

  // --- test hooks ----------------------------------------------------------
  // Same contract as __liveLog / __queueRefresh: for automated tests and
  // console debugging, not for app code.
  window.__multitail = {
    openMode,
    closeMode,
    toggleMode,
    reconcile,
    pumpSlots,
    formatPayload,
    handlePayload,
    buildPane,
    appendPaneLine,
    eligibleRows,
    eligibleInfos,
    panes,
    dismissed,
    cleared,
    subsChoice,
    paneKey,
    streamUrl,
    toggleSubagents,
    subPanesOf,
    panesInDisplayOrder,
    slotOrder,
    SUB_NODE_SELECTOR,
    MAX_SUB_INDENT_DEPTH,
    isOpen: () => open,
    setWrap,
    setTimestamps,
    setVerbose,
    toggleWrap,
    toggleTimestamps,
    toggleVerbose,
    isWrap: () => wrapOn,
    isTimestamps: () => tsOn,
    isVerbose: () => verboseOn,
    setSubagents,
    toggleSubagentsDefault,
    subsShownFor,
    isSubagentsShown: () => subsShown,
    SUBS_STORAGE_KEY,
    DEFAULT_SUBS_SHOWN,
    openOptions,
    closeOptions,
    toggleOptions,
    isOptionsOpen: () => optionsOpen,
    optionsFocusables,
    lineCharLimit,
    setVerboseCap,
    cycleVerboseCap,
    verboseCap: () => verboseCapKey,
    verboseCapChars,
    VERBOSE_CAP_OPTIONS,
    DEFAULT_VERBOSE_CAP_KEY,
    VERBOSE_CAP_STORAGE_KEY,
    setRetention,
    cycleRetention,
    sweepEndedPanes,
    retention: () => retentionKey,
    retentionMs,
    RETENTION_OPTIONS,
    DEFAULT_RETENTION_KEY,
    RETENTION_STORAGE_KEY,
    WRAP_STORAGE_KEY,
    TS_STORAGE_KEY,
    VERBOSE_STORAGE_KEY,
    retryDelayMs,
    wantsSlot,
    paneHasSourceTimestamps,
    MAX_LIVE_STREAMS,
    MAX_LINES_PER_PANE,
    MAX_LINE_CHARS,
    MAX_LINE_CHARS_WRAPPED,
    MAX_PANE_CHARS,
    NEAR_BOTTOM_PX,
    NEAR_BOTTOM_PX_WRAPPED,
    RECONCILE_MS,
    STREAM_RETRY_BASE_MS,
    STREAM_RETRY_MAX_MS,
    RETRYABLE_ERROR_KINDS,
  };
})();
