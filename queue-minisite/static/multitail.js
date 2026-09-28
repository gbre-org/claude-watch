// Multitail mode for the queue minisite.
//
// A whole-window mode that stacks one live tail per RUNNING queue item that
// actually has a log, so a single glance answers "what is everything doing"
// instead of opening the single-item log modal N times in a row.
//
//   toggle on/off  the `multitail` pill in the header, or the `m` key
//   leave          `m`, Esc, or the exit button
//   dismiss one    the × in that pane's header (stays in the mode)
//   line wrap      the `wrap` pill, or the `w` key
//   timestamps     the `time` pill, or the `t` key
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
// side-by-side comparison that is invalid. An empty column that explains
// itself is strictly better than a plausible fabrication.
//
// ---------------------------------------------------------------------------
// A PANE WHOSE JOB FINISHES WHILE THE MODE IS OPEN
// ---------------------------------------------------------------------------
// It is NOT removed. Yanking a pane away is guaranteed to delete the output
// the operator was reading at the moment it became final, which is the worst
// possible time. The pane keeps its content, its header flips to `ended` (with
// the exit code when the stream reported one), and its stream slot is released
// so a waiting pane can connect. Closing it is the operator's call.
//
// Nothing here is persisted: a full-window takeover that survived a reload
// would be a surprise, not a convenience. The wrap / timestamp preferences
// live as long as the page does — they outlast closing and reopening the mode,
// which is the scope a preference for a non-persisted mode can honestly have.

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

  // Rows the server marked as having a tailable log, in render order.
  const ROW_SELECTOR = '.item[data-live-log-mode]';
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
  // hostjob tails are plain text and carry none.
  const TS_SOURCE_MODES = { live: true };

  let open = false;
  let reconcileTimer = null;
  // Display preferences. Module-level, so they survive closing and reopening
  // the mode within a page load, and reset on reload like the mode itself.
  let wrapOn = false;
  let tsOn = false;
  // qid -> pane record. Insertion order is the order panes were added, which
  // is the order they get stream slots.
  const panes = new Map();
  // Panes the operator dismissed by hand. They must NOT come back on the next
  // reconcile pass — "I closed that" has to stick while the mode is open.
  const dismissed = new Set();

  // --- eligibility ---------------------------------------------------------

  function eligibleRows() {
    return Array.prototype.slice.call(document.querySelectorAll(ROW_SELECTOR));
  }

  function rowInfo(row) {
    return {
      qid: row.getAttribute('data-queue-id') || '',
      mode: (row.getAttribute('data-live-log-mode') || '').toLowerCase(),
      summary: row.getAttribute('data-queue-summary') || '',
    };
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
    return wrapOn ? NEAR_BOTTOM_PX_WRAPPED : NEAR_BOTTOM_PX;
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

  // Does this pane's SOURCE carry per-line timestamps at all? See the header
  // comment: we never substitute the client's arrival time for a source that
  // has none.
  function paneHasSourceTimestamps(pane) {
    return !!TS_SOURCE_MODES[pane.mode];
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

  function toolArgPreview(input) {
    if (!input || typeof input !== 'object') return '';
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
      for (const part of c) {
        if (part && part.type === 'text' && typeof part.text === 'string') {
          return part.text;
        }
      }
      return '[' + c.length + ' block(s)]';
    }
    return '';
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
  // transcript records have an ISO8601 `timestamp`; plain-text workload and
  // hostjob frames have no such field and get '' — never a synthesised one.
  function sourceTs(payload) {
    const rec = payload && payload.rec;
    if (rec && typeof rec.timestamp === 'string' && rec.timestamp) {
      return rec.timestamp;
    }
    return '';
  }

  // Returns { sigil, text, cls, ts } for one stream payload, or null to skip
  // it. `ts` is the SOURCE timestamp (ISO8601) or '' when the source has none.
  function formatPayload(payload) {
    if (!payload || typeof payload !== 'object') return null;

    if (payload.type === 'raw') {
      return { sigil: '', text: firstLine(payload.line || ''), cls: 'mt-raw', ts: '' };
    }
    if (payload.kind === 'workload_line') {
      // Plain-text tail (workload / hostjob / archived output): verbatim, and
      // carrying no timestamp of its own.
      return { sigil: '', text: payload.text || '', cls: 'mt-plain', ts: '' };
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
        const body = firstLine(textOfToolResult(tr)).trim();
        return {
          sigil: '←',
          text: body || '(empty result)',
          cls: tr.is_error ? 'mt-result mt-err' : 'mt-result',
          ts: ts,
        };
      }
      case 'assistant_text':
        return { sigil: '·', text: firstLine(joinedText(rec)).trim(), cls: 'mt-text', ts: ts };
      case 'thinking':
        return { sigil: '~', text: firstLine(joinedText(rec)).trim(), cls: 'mt-think', ts: ts };
      case 'user':
        return { sigil: '»', text: firstLine(joinedText(rec)).trim(), cls: 'mt-user', ts: ts };
      case 'user_image':
        return { sigil: '»', text: '[image]', cls: 'mt-user', ts: ts };
      case 'attachment':
        return { sigil: '»', text: '[attachment]', cls: 'mt-user', ts: ts };
      case 'system':
        return { sigil: '·', text: firstLine(rec.content || rec.subtype || 'system'), cls: 'mt-sys', ts: ts };
      case 'progress':
        return { sigil: '·', text: firstLine(rec.message || 'progress'), cls: 'mt-sys', ts: ts };
      default:
        // An unrecognised record is still evidence the agent is alive, so it
        // gets a line rather than being swallowed.
        return { sigil: '·', text: String(payload.kind || 'event'), cls: 'mt-sys', ts: ts };
    }
  }

  // --- pane construction ---------------------------------------------------

  function modeBadgeText(mode) {
    if (mode === 'workload') return 'workload';
    if (mode === 'hostjob') return 'hostjob';
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

  function buildPane(info) {
    const wrap = el('section', 'mt-pane');
    wrap.setAttribute('data-queue-id', info.qid);
    wrap.setAttribute('data-live-log-mode', info.mode);

    const head = el('header', 'mt-pane-head');
    head.appendChild(el('span', 'mt-pane-badge mt-badge-' + info.mode, modeBadgeText(info.mode)));
    head.appendChild(el('code', 'mt-pane-id', info.qid));
    head.appendChild(el('span', 'mt-pane-summary', info.summary));
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
    closeBtn.setAttribute('aria-label', 'Close the ' + info.qid + ' tail');
    closeBtn.title = 'Close this tail (stays in multitail mode)';
    head.appendChild(closeBtn);
    wrap.appendChild(head);

    const stream = el('pre', 'mt-pane-stream');
    stream.tabIndex = 0;
    wrap.appendChild(stream);

    const pane = {
      qid: info.qid,
      mode: info.mode,
      el: wrap,
      streamEl: stream,
      statusEl: status,
      noTsEl: noTs,
      es: null,
      streaming: false,   // holds a connection right now
      terminal: false,    // stream reported a real end; never reconnect
      ended: false,       // item is no longer running / stream finished
      autoscroll: true,
      // The pane's lines, as records. The DOM is a projection of these, so
      // toggling wrap / timestamps can re-render lines that have already
      // scrolled past (or a pane whose job has ended and will emit no more).
      records: [],
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
      // Stream-retry backoff. retryAt is an epoch-ms deadline (0 = none);
      // retryAttempts drives the delay and resets once data actually arrives.
      retryAt: 0,
      retryAttempts: 0,
      retryNoteShown: false,
    };

    closeBtn.addEventListener('click', () => {
      dismissed.add(pane.qid);
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
    return pane;
  }

  // --- line rendering -----------------------------------------------------

  // Build the DOM row for one record under the CURRENT display settings. The
  // timestamp cell is emitted only when the column is on AND this record
  // actually has a source timestamp — an empty cell is never padded with
  // anything borrowed.
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
    const limit = wrapOn ? MAX_LINE_CHARS_WRAPPED : MAX_LINE_CHARS;
    row.appendChild(el('span', 'mt-body', clip(rec.text, limit)));
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
      // Stored bounded, not clipped to the unwrapped width: the wrap toggle
      // has to be able to show more of this line later.
      text: clip(fmt.text, MAX_LINE_CHARS_WRAPPED),
      cls: fmt.cls || '',
      ts: fmt.ts || '',
    };
    pane.records.push(rec);
    pane.streamEl.appendChild(renderRecord(rec));
    while (pane.records.length > MAX_LINES_PER_PANE) {
      pane.records.shift();
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

  function connectPane(pane) {
    if (pane.es || pane.terminal) return;
    pane.streaming = true;
    pane.retryAt = 0;
    setPaneStatus(pane, 'connecting…', '');
    let es;
    try {
      es = new EventSource('/api/queue/' + encodeURIComponent(pane.qid) + '/stream');
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
    setPaneStatus(pane, label, cls || 'mt-done');
    releaseSlot(pane);
  }

  // A pane that would take a connection right now: not already streaming, not
  // finished, and not inside a retry backoff. A pane in backoff is distinct
  // from a pane waiting for a slot — only the former is on a timer.
  function wantsSlot(pane) {
    if (pane.streaming || pane.terminal || pane.es) return false;
    if (pane.retryAt && pane.retryAt > Date.now()) return false;
    return true;
  }

  // Give connections to the earliest panes that want one, up to the cap.
  function pumpSlots() {
    let live = 0;
    for (const pane of panes.values()) {
      if (pane.streaming) live += 1;
    }
    for (const pane of panes.values()) {
      if (live >= MAX_LIVE_STREAMS) break;
      if (!wantsSlot(pane)) continue;
      connectPane(pane);
      live += 1;
    }
    // Everything still unconnected is explicitly waiting, and the two reasons
    // are different: a backoff counts down, a slot queue does not.
    for (const pane of panes.values()) {
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
    panes.delete(pane.qid);
  }

  // --- reconcile + chrome --------------------------------------------------

  function paintCount() {
    const total = panes.size;
    let live = 0;
    let ended = 0;
    let retrying = 0;
    for (const pane of panes.values()) {
      if (pane.streaming) live += 1;
      if (pane.ended) ended += 1;
      if (!pane.terminal && pane.retryAt) retrying += 1;
    }
    const bits = [total + (total === 1 ? ' tail' : ' tails')];
    if (live < total - ended) {
      bits.push(live + ' streaming (cap ' + MAX_LIVE_STREAMS + ')');
    }
    if (retrying) bits.push(retrying + ' waiting for a log');
    if (ended) bits.push(ended + ' ended');
    if (countEl) countEl.textContent = bits.join(' · ');
    if (emptyEl) emptyEl.hidden = total > 0;
  }

  function reconcile() {
    if (!open) return;
    const seen = new Set();
    for (const row of eligibleRows()) {
      const info = rowInfo(row);
      if (!info.qid || !info.mode) continue;
      seen.add(info.qid);
      if (dismissed.has(info.qid)) continue;
      const existing = panes.get(info.qid);
      if (!existing) {
        const pane = buildPane(info);
        panes.set(pane.qid, pane);
        panesEl.appendChild(pane.el);
        continue;
      }
      // Summary text can change (the queue record is editable); keep it fresh
      // without touching the stream.
      const sumEl = existing.el.querySelector('.mt-pane-summary');
      if (sumEl && sumEl.textContent !== info.summary) sumEl.textContent = info.summary;
    }
    // A pane whose row stopped being eligible (finished, abandoned, moved out
    // of the running section) is marked ENDED, never removed — see the header
    // comment. Its slot goes back to the pool.
    for (const pane of panes.values()) {
      if (!seen.has(pane.qid) && !pane.ended) {
        markEnded(pane, 'ended · no longer running', 'mt-done');
      }
    }
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
    overlay.classList.toggle('mt-wrap', wrapOn);
    overlay.classList.toggle('mt-show-ts', tsOn);
  }

  function setWrap(on) {
    const next = !!on;
    if (next === wrapOn) return;
    wrapOn = next;
    syncDisplayButtons();
    rerenderAllPanes();
  }

  function setTimestamps(on) {
    const next = !!on;
    if (next === tsOn) return;
    tsOn = next;
    syncDisplayButtons();
    rerenderAllPanes();
  }

  function toggleWrap() { setWrap(!wrapOn); }
  function toggleTimestamps() { setTimestamps(!tsOn); }

  function openMode() {
    if (open) return;
    open = true;
    dismissed.clear();
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
    if (reconcileTimer !== null) {
      clearInterval(reconcileTimer);
      reconcileTimer = null;
    }
    for (const pane of Array.from(panes.values())) destroyPane(pane);
    panes.clear();
    dismissed.clear();
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
    panes,
    dismissed,
    isOpen: () => open,
    setWrap,
    setTimestamps,
    toggleWrap,
    toggleTimestamps,
    isWrap: () => wrapOn,
    isTimestamps: () => tsOn,
    retryDelayMs,
    wantsSlot,
    paneHasSourceTimestamps,
    MAX_LIVE_STREAMS,
    MAX_LINES_PER_PANE,
    MAX_LINE_CHARS,
    MAX_LINE_CHARS_WRAPPED,
    NEAR_BOTTOM_PX,
    NEAR_BOTTOM_PX_WRAPPED,
    RECONCILE_MS,
    STREAM_RETRY_BASE_MS,
    STREAM_RETRY_MAX_MS,
    RETRYABLE_ERROR_KINDS,
  };
})();
