// Multitail mode for the queue minisite.
//
// A whole-window mode that stacks one live tail per RUNNING queue item that
// actually has a log, so a single glance answers "what is everything doing"
// instead of opening the single-item log modal N times in a row.
//
//   toggle on/off  the `multitail` pill in the header, or the `m` key
//   leave          `m`, Esc, or the exit button
//   dismiss one    the × in that pane's header (stays in the mode)
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
// A PANE WHOSE JOB FINISHES WHILE THE MODE IS OPEN
// ---------------------------------------------------------------------------
// It is NOT removed. Yanking a pane away is guaranteed to delete the output
// the operator was reading at the moment it became final, which is the worst
// possible time. The pane keeps its content, its header flips to `ended` (with
// the exit code when the stream reported one), and its stream slot is released
// so a waiting pane can connect. Closing it is the operator's call.
//
// Nothing here is persisted: a full-window takeover that survived a reload
// would be a surprise, not a convenience.

(function () {
  'use strict';

  const overlay = document.getElementById('multitail');
  if (!overlay) return;

  const panesEl = document.getElementById('multitail-panes');
  const emptyEl = document.getElementById('multitail-empty');
  const countEl = document.getElementById('multitail-count');
  const exitBtn = document.getElementById('multitail-exit');

  // Rows the server marked as having a tailable log, in render order.
  const ROW_SELECTOR = '.item[data-live-log-mode]';
  // Max simultaneous EventSource connections — see the header comment. Four
  // leaves two of the browser's ~6 per-origin HTTP/1.1 connections for the
  // queue poll and any action POST.
  const MAX_LIVE_STREAMS = 4;
  // Lines retained per pane. A tail is a window on the recent past; keeping
  // an unbounded transcript in N panes is how a long-lived tab runs out of
  // memory.
  const MAX_LINES_PER_PANE = 400;
  // How often the pane list is reconciled against the rendered rows. Twice
  // the refresh tick's resolution so a newly-eligible item shows up promptly
  // without polling anything itself.
  const RECONCILE_MS = 2000;
  // Re-arm auto-scroll once the reader is back within this many px of the
  // bottom (same rule the single-item log modal uses).
  const NEAR_BOTTOM_PX = 40;
  // Longest single rendered line before it is clipped with an ellipsis. Panes
  // are short; one 4000-character tool result would otherwise be the whole
  // pane.
  const MAX_LINE_CHARS = 400;

  let open = false;
  let reconcileTimer = null;
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

  // Returns { sigil, text, cls } for one stream payload, or null to skip it.
  function formatPayload(payload) {
    if (!payload || typeof payload !== 'object') return null;

    if (payload.type === 'raw') {
      return { sigil: '', text: firstLine(payload.line || ''), cls: 'mt-raw' };
    }
    if (payload.kind === 'workload_line') {
      // Plain-text tail (workload / hostjob / archived output): verbatim.
      return { sigil: '', text: payload.text || '', cls: 'mt-plain' };
    }

    const rec = payload.rec || {};
    switch (payload.kind) {
      case 'tool_use': {
        const tu = blockOfType(rec, 'tool_use') || {};
        const arg = toolArgPreview(tu.input);
        return {
          sigil: '▸',
          text: (tu.name || 'tool') + (arg ? ' ' + arg : ''),
          cls: 'mt-tool',
        };
      }
      case 'tool_result': {
        const tr = blockOfType(rec, 'tool_result') || {};
        const body = firstLine(textOfToolResult(tr)).trim();
        return {
          sigil: '←',
          text: body || '(empty result)',
          cls: tr.is_error ? 'mt-result mt-err' : 'mt-result',
        };
      }
      case 'assistant_text':
        return { sigil: '·', text: firstLine(joinedText(rec)).trim(), cls: 'mt-text' };
      case 'thinking':
        return { sigil: '~', text: firstLine(joinedText(rec)).trim(), cls: 'mt-think' };
      case 'user':
        return { sigil: '»', text: firstLine(joinedText(rec)).trim(), cls: 'mt-user' };
      case 'user_image':
        return { sigil: '»', text: '[image]', cls: 'mt-user' };
      case 'attachment':
        return { sigil: '»', text: '[attachment]', cls: 'mt-user' };
      case 'system':
        return { sigil: '·', text: firstLine(rec.content || rec.subtype || 'system'), cls: 'mt-sys' };
      case 'progress':
        return { sigil: '·', text: firstLine(rec.message || 'progress'), cls: 'mt-sys' };
      default:
        // An unrecognised record is still evidence the agent is alive, so it
        // gets a line rather than being swallowed.
        return { sigil: '·', text: String(payload.kind || 'event'), cls: 'mt-sys' };
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

  function buildPane(info) {
    const wrap = el('section', 'mt-pane');
    wrap.setAttribute('data-queue-id', info.qid);
    wrap.setAttribute('data-live-log-mode', info.mode);

    const head = el('header', 'mt-pane-head');
    head.appendChild(el('span', 'mt-pane-badge mt-badge-' + info.mode, modeBadgeText(info.mode)));
    head.appendChild(el('code', 'mt-pane-id', info.qid));
    head.appendChild(el('span', 'mt-pane-summary', info.summary));
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
      es: null,
      streaming: false,   // holds a connection right now
      terminal: false,    // stream reported a real end; never reconnect
      ended: false,       // item is no longer running / stream finished
      autoscroll: true,
      lines: 0,
      // Set while the server is replaying its historical backfill on a
      // RECONNECT (2nd+ stream-start). Plain-text tails carry no resume
      // cursor, so without this a quiet workload re-prints its last 200 lines
      // every time the server's idle cap recycles the stream.
      suppressBackfill: false,
      sawStreamStart: false,
    };

    closeBtn.addEventListener('click', () => {
      dismissed.add(pane.qid);
      destroyPane(pane);
      pumpSlots();
      paintCount();
    });

    stream.addEventListener('scroll', () => {
      const nearBottom =
        stream.scrollHeight - stream.scrollTop - stream.clientHeight < NEAR_BOTTOM_PX;
      pane.autoscroll = nearBottom;
    });

    return pane;
  }

  function appendPaneLine(pane, fmt) {
    if (!fmt) return;
    const text = clip(fmt.text);
    // A formatter that produced nothing at all would render an empty row that
    // eats a line of a very short pane for no information.
    if (!text && !fmt.sigil) return;
    const row = el('div', 'mt-line ' + (fmt.cls || ''));
    if (fmt.sigil) row.appendChild(el('span', 'mt-sigil', fmt.sigil));
    row.appendChild(el('span', 'mt-body', text));
    pane.streamEl.appendChild(row);
    pane.lines += 1;
    while (pane.lines > MAX_LINES_PER_PANE && pane.streamEl.firstChild) {
      pane.streamEl.removeChild(pane.streamEl.firstChild);
      pane.lines -= 1;
    }
    if (pane.autoscroll) pane.streamEl.scrollTop = pane.streamEl.scrollHeight;
  }

  function appendPaneNote(pane, text) {
    appendPaneLine(pane, { sigil: '', text: text, cls: 'mt-note' });
  }

  // --- streams -------------------------------------------------------------

  function handlePayload(pane, payload) {
    if (payload.type === 'meta') {
      switch (payload.kind) {
        case 'stream-start':
          if (pane.sawStreamStart) pane.suppressBackfill = true;
          pane.sawStreamStart = true;
          setPaneStatus(pane, 'live', 'mt-ok');
          return;
        case 'backfill-begin':
          return;
        case 'backfill-end':
          pane.suppressBackfill = false;
          return;
        case 'resumed':
          pane.suppressBackfill = false;
          setPaneStatus(pane, 'live', 'mt-ok');
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
          // reconnects on its own. Not an error, and not an end.
          setPaneStatus(pane, 'idle', 'mt-idle');
          return;
        case 'lifetime-timeout':
          setPaneStatus(pane, 'reconnecting', 'mt-idle');
          return;
        default:
          return;
      }
    }
    if (payload.type === 'error') {
      // Stream-shaped errors (no agent record, unreadable file, …) are
      // terminal for this attempt: the server closed after one event, so
      // letting EventSource retry forever would just spin.
      setPaneStatus(pane, payload.kind || 'error', 'mt-err');
      appendPaneNote(pane, payload.error || String(payload.kind || 'stream error'));
      pane.terminal = true;
      releaseSlot(pane);
      return;
    }
    if (pane.suppressBackfill) return;
    appendPaneLine(pane, formatPayload(payload));
  }

  function connectPane(pane) {
    if (pane.es || pane.terminal) return;
    pane.streaming = true;
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
      let payload;
      try {
        payload = JSON.parse(ev.data);
      } catch (_) {
        return;
      }
      handlePayload(pane, payload);
    };
    es.onerror = () => {
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
    pane.el.classList.add('mt-ended');
    setPaneStatus(pane, label, cls || 'mt-done');
    releaseSlot(pane);
  }

  // Give connections to the earliest panes that want one, up to the cap.
  function pumpSlots() {
    let live = 0;
    for (const pane of panes.values()) {
      if (pane.streaming) live += 1;
    }
    for (const pane of panes.values()) {
      if (live >= MAX_LIVE_STREAMS) break;
      if (pane.streaming || pane.terminal) continue;
      connectPane(pane);
      live += 1;
    }
    // Everything still unconnected is explicitly waiting, not broken.
    for (const pane of panes.values()) {
      if (!pane.streaming && !pane.terminal && !pane.es) {
        setPaneStatus(pane, 'waiting for a stream slot', 'mt-idle');
      }
    }
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
    for (const pane of panes.values()) {
      if (pane.streaming) live += 1;
      if (pane.ended) ended += 1;
    }
    const bits = [total + (total === 1 ? ' tail' : ' tails')];
    if (live < total - ended) {
      bits.push(live + ' streaming (cap ' + MAX_LIVE_STREAMS + ')');
    }
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
    pumpSlots();
    paintCount();
  }

  function syncToggleButton() {
    const btn = document.getElementById('multitail-toggle');
    if (btn) btn.setAttribute('aria-pressed', open ? 'true' : 'false');
  }

  function openMode() {
    if (open) return;
    open = true;
    dismissed.clear();
    overlay.hidden = false;
    document.body.classList.add('multitail-open');
    syncToggleButton();
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
    // `m` is the toggle. Chosen because it is a free single key: the site
    // already binds j/k (row + stream scroll), Enter/Space (open), g/G (jump),
    // `/` (search focus) and Esc (dismiss), and unlike `/` — which is
    // Firefox's quick-find — a bare `m` has no default browser action to
    // swallow. Modified chords are left alone above so Cmd/Ctrl+M still
    // minimises.
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
    MAX_LIVE_STREAMS,
    MAX_LINES_PER_PANE,
    MAX_LINE_CHARS,
    RECONCILE_MS,
  };
})();
