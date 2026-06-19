// Live agent log stream modal.
//
// Click on a running item card -> open a modal with a live tail of the
// owning agent's JSONL transcript. Backend is /api/queue/<id>/stream
// (SSE). Each event is pretty-printed into a monospace <pre> element.
//
// Pretty-printing is intentionally simple — the wire format is the raw
// claude-code transcript JSONL, not a curated DTO. We pull out the
// most-relevant fields per event kind and render a compact one-liner;
// the full record is exposed in a collapsed <details> for debugging.
//
// Accessibility:
//   - role=dialog + aria-modal=true on the wrapper
//   - aria-live="polite" on the stream so screenreaders announce new lines
//   - Esc closes; click on backdrop / close button closes
//   - focus restoration to the originating row on close
//
// Co-existence with other handlers:
//   - The click handler ignores clicks inside .action-btn (stop button)
//     and inside .drag-handle so existing interactions are preserved.
//   - drag/drop events on the row continue to work — we only intercept
//     'click' and 'keydown'.

(function () {
  'use strict';

  const modal = document.getElementById('log-modal');
  if (!modal) return;

  const titleIdEl = document.getElementById('log-modal-id');
  const modeLabelEl = document.getElementById('log-modal-mode-label');
  const summaryEl = document.getElementById('log-modal-summary');
  const statusEl = document.getElementById('log-modal-status');
  const streamEl = document.getElementById('log-modal-stream');
  const closeBtn = document.getElementById('log-modal-close');
  const autoscrollBtn = document.getElementById('log-modal-autoscroll');
  const jumpTopBtn = document.getElementById('log-modal-jump-top');
  const jumpBottomBtn = document.getElementById('log-modal-jump-bottom');
  const promptDetailsEl = document.getElementById('log-modal-prompt');
  const promptLabelEl = document.getElementById('log-modal-prompt-label');
  const promptBodyEl = document.getElementById('log-modal-prompt-body');
  // Per-modal "Summary" header block — populated from /api/queue/<id>/meta.
  // See templates/index.html for the row scaffolding. Each row is shown
  // ONLY when the corresponding meta field is present so the header
  // stays tight (e.g. pending items get no runtime row, items with no
  // depends_on get no deps row).
  const metaSummaryEl = document.getElementById('log-modal-meta-summary');
  const metaRowEls = {
    status:     document.getElementById('log-meta-row-status'),
    runtime:    document.getElementById('log-meta-row-runtime'),
    times:      document.getElementById('log-meta-row-times'),
    scope:      document.getElementById('log-meta-row-scope'),
    deps:       document.getElementById('log-meta-row-deps'),
    dependents: document.getElementById('log-meta-row-dependents'),
    by:         document.getElementById('log-meta-row-by'),
    group:      document.getElementById('log-meta-row-group'),
    usage:      document.getElementById('log-meta-row-usage'),
    abandon:    document.getElementById('log-meta-row-abandon'),
  };
  const metaValEls = {
    status:     document.getElementById('log-meta-status'),
    runtime:    document.getElementById('log-meta-runtime'),
    times:      document.getElementById('log-meta-times'),
    scope:      document.getElementById('log-meta-scope'),
    deps:       document.getElementById('log-meta-deps'),
    dependents: document.getElementById('log-meta-dependents'),
    by:         document.getElementById('log-meta-by'),
    group:      document.getElementById('log-meta-group'),
    usage:      document.getElementById('log-meta-usage'),
    abandon:    document.getElementById('log-meta-abandon'),
  };
  const returnDetailsEl = document.getElementById('log-modal-return');
  const returnLabelEl = document.getElementById('log-modal-return-label');
  const returnBodyEl = document.getElementById('log-modal-return-body');
  // Captured-script disclosure block. Populated from
  // /api/queue/<id>/meta when the workload-bound queue item carries a
  // script_capture sidecar on disk. Default-collapsed (most users only
  // expand it when debugging) and hidden entirely when no capture is
  // present. Header line shows path / size / sha256; body is the
  // script content (truncated to 1 MiB on the server side).
  const scriptCaptureDetailsEl = document.getElementById('log-modal-script-capture');
  const scriptCaptureLabelEl = document.getElementById('log-modal-script-capture-label');
  const scriptCaptureHeaderEl = document.getElementById('log-modal-script-capture-header');
  const scriptCaptureBodyEl = document.getElementById('log-modal-script-capture-body');
  // Collapse/expand wrapper around the 10 metadata rows. Default state
  // is viewer-controlled (persisted in localStorage); first-visit
  // fallback is collapsed on mobile, expanded otherwise. See
  // setMetaToggleInitialState() / persistMetaToggleState() below.
  const metaToggleEl = document.getElementById('log-meta-toggle');

  // localStorage key + helpers for the metadata-block expanded state.
  // Guarded against private-mode / disabled-storage environments — the
  // getter returns null and the setter no-ops on any thrown error, so
  // the section still toggles, just won't survive a reload. The
  // narrow-viewport breakpoint matches the brief (< 768px = mobile-ish).
  const META_TOGGLE_STORAGE_KEY = 'queue-minisite.metadataExpanded';
  const META_TOGGLE_MOBILE_BREAKPOINT_PX = 768;

  function readMetaToggleStored() {
    try {
      const v = window.localStorage.getItem(META_TOGGLE_STORAGE_KEY);
      if (v === '1' || v === 'true') return true;
      if (v === '0' || v === 'false') return false;
      return null;
    } catch (_e) {
      return null;
    }
  }

  function writeMetaToggleStored(isOpen) {
    try {
      window.localStorage.setItem(META_TOGGLE_STORAGE_KEY, isOpen ? '1' : '0');
    } catch (_e) {
      /* private mode / quota / disabled — silently degrade to session-only */
    }
  }

  // Apply the persisted-or-default state to the metadata <details> on
  // modal open. Called from resetMetaSummary() so every open re-syncs
  // with current localStorage (handles the case where the user toggled
  // in another tab / window).
  function setMetaToggleInitialState() {
    if (!metaToggleEl) return;
    const stored = readMetaToggleStored();
    if (stored === null) {
      // First visit — collapse on narrow viewports, expand otherwise.
      const w = window.innerWidth || document.documentElement.clientWidth || 0;
      metaToggleEl.open = w >= META_TOGGLE_MOBILE_BREAKPOINT_PX;
    } else {
      metaToggleEl.open = stored;
    }
  }

  // Persist on every toggle. Wired once on script load (the listener
  // outlives any number of modal open/close cycles).
  if (metaToggleEl) {
    metaToggleEl.addEventListener('toggle', () => {
      writeMetaToggleStored(metaToggleEl.open);
    });
  }

  let triggerEl = null;
  let evtSource = null;
  let autoscroll = true;
  // Mode set by `open()`: 'live' uses /stream (SSE tail of agent JSONL),
  // 'archive' uses /archive (one-shot replay), 'workload' uses /stream
  // but the server tails /tmp/claude-workloads/<label>.output instead of
  // an agent JSONL — same wire format, simpler line-oriented payload.
  let mode = 'live';
  // Subagent id for mode==='subagent' — the per-subagent live-log
  // tree under each running card. In that mode the EventSource points
  // at /api/subagent/<subagentId>/stream (NOT the queue stream); the
  // SSE wire format is identical so the renderer is unchanged.
  let subagentId = null;
  // Starting-state polling: when a queue row carries
  // data-queue-starting="1" the modal opens but no agent record / JSONL
  // exists yet. The backend's /stream endpoint emits a one-shot error
  // event (kind=no-agent or kind=no-jsonl) and closes the connection.
  // We treat that as "still warming up", close the EventSource, wait
  // POLL_INTERVAL_MS, and retry. As soon as a real `meta:stream-start`
  // event arrives we clear the polling state and transition into the
  // normal live-tail UI. `pollingQid` is the queue id we're polling
  // for; non-null means we're in polling mode. `pollTimer` is the
  // pending setTimeout handle (cleared on close + on successful
  // transition).
  let pollingQid = null;
  let pollTimer = null;
  const POLL_INTERVAL_MS = 2000;
  // Cap the number of rendered lines — long-running agents can ship
  // thousands of events; keeping them all in the DOM is pointless and
  // eats memory. We trim from the top once we exceed this.
  const MAX_LINES = 2000;

  // Runtime ticker — re-renders the RUNTIME meta row every
  // RUNTIME_TICK_MS while the open modal's item is running. Without
  // this the runtime string is whatever the server computed at modal
  // open and stays frozen for the entire viewing session (Andrew
  // screenshotted a "19m 26s" frozen value, 2026-05-13).
  //
  // State:
  //   runtimeTickerTimer  — setInterval handle; null when not ticking.
  //   runtimeStartedMs    — Date.parse(started_at_iso); the anchor we
  //                         subtract from Date.now() each tick.
  //
  // Lifecycle:
  //   start  — applyMetaSummary() with status === 'running' AND a
  //            parseable started_at calls startRuntimeTicker().
  //   stop   — resetMetaSummary() (modal re-open / status change) +
  //            close() (modal close) call stopRuntimeTicker(). Belt
  //            + braces: if applyMetaSummary() lands a non-running
  //            status on a subsequent meta refresh, the ticker also
  //            stops.
  const RUNTIME_TICK_MS = 1000;
  let runtimeTickerTimer = null;
  let runtimeStartedMs = null;

  function stopRuntimeTicker() {
    if (runtimeTickerTimer) {
      clearInterval(runtimeTickerTimer);
      runtimeTickerTimer = null;
    }
    runtimeStartedMs = null;
  }

  function renderRuntimeFromAnchor() {
    if (runtimeStartedMs === null || isNaN(runtimeStartedMs)) return;
    const secs = Math.max(0, (Date.now() - runtimeStartedMs) / 1000);
    setMetaRow('runtime', fmtRuntime(secs), false);
  }

  function startRuntimeTicker(startedIso) {
    stopRuntimeTicker();
    if (!startedIso) return;
    const parsed = Date.parse(startedIso);
    if (isNaN(parsed)) return;
    runtimeStartedMs = parsed;
    // Render once immediately so the displayed value matches the
    // ticking source (avoids a 1-second visual lag between the
    // server-computed string and the first ticker frame).
    renderRuntimeFromAnchor();
    runtimeTickerTimer = setInterval(renderRuntimeFromAnchor, RUNTIME_TICK_MS);
  }

  function setStatus(label, kind) {
    if (!statusEl) return;
    statusEl.textContent = label;
    statusEl.className = 'log-status log-status-' + (kind || 'pending');
  }

  // Tracks the last appended row when the source segment was \r-
  // terminated (a "transient" progress frame). The next workload line
  // — transient or permanent — replaces the row in place rather than
  // stacking. Reset by close() / open() / mode change.
  //
  // We anchor on the DOM node (not an index) so MAX_LINES head-trim or
  // any unrelated appendLine call between two workload frames doesn't
  // shift the target. The presence of a ref means "the prior workload
  // segment was \r-terminated" — i.e. the next one is allowed to
  // replace it.
  let lastTransientRow = null;

  function appendLine(html, classes) {
    const el = document.createElement('div');
    el.className = 'log-line ' + (classes || '');
    el.innerHTML = html;
    streamEl.appendChild(el);
    // Trim the head if we've blown the cap — drop the oldest 10% so we
    // don't thrash on every single new line.
    const lines = streamEl.children;
    if (lines.length > MAX_LINES) {
      const drop = Math.max(1, Math.floor(MAX_LINES * 0.1));
      for (let i = 0; i < drop && streamEl.firstChild; i++) {
        streamEl.removeChild(streamEl.firstChild);
      }
    }
    if (autoscroll) {
      streamEl.scrollTop = streamEl.scrollHeight;
    }
    return el;
  }

  // Replace the rendered content of an existing row in place, used by
  // the workload-line transient-replace path so \r-terminated progress
  // frames update one DOM row instead of stacking thousands. Honors
  // autoscroll so a long progress run still keeps the bottom-pinned
  // view tracking the latest frame.
  function replaceLineContent(row, html, classes) {
    if (!row) return;
    row.className = 'log-line ' + (classes || '');
    row.innerHTML = html;
    if (autoscroll) {
      streamEl.scrollTop = streamEl.scrollHeight;
    }
  }

  // --- Meta summary rendering --------------------------------------------
  // Helpers + state for the per-modal Summary header (populated from
  // /api/queue/<id>/meta on open). Kept up here next to the other DOM
  // helpers so the open() function reads as a linear pipeline.

  // Format an integer number of seconds as a short human-readable string
  // (e.g. "300.5s" → "5m 0s"). Matches the style of `_humanize_age`
  // server-side but for finite durations rather than ages.
  function fmtRuntime(secs) {
    if (secs === null || secs === undefined || isNaN(secs)) return '';
    let n = Math.max(0, Math.round(Number(secs)));
    if (n < 60) return n + 's';
    if (n < 3600) {
      const m = Math.floor(n / 60);
      const s = n % 60;
      return m + 'm ' + s + 's';
    }
    const h = Math.floor(n / 3600);
    const m = Math.floor((n % 3600) / 60);
    const s = n % 60;
    return h + 'h ' + m + 'm ' + s + 's';
  }

  // Pretty-print an ISO8601 UTC timestamp in the viewer's local timezone.
  // Reuses the local-time helper so the format matches the rest of the
  // UI; falls back to the raw UTC string when the helper isn't loaded.
  function fmtLocalIso(iso) {
    if (!iso) return '';
    if (window.LocalTime && typeof window.LocalTime.dateTime === 'function') {
      const out = window.LocalTime.dateTime(iso);
      if (out) return out;
    }
    return iso;
  }

  // Set a single meta row's visible text and toggle the row's hidden
  // attribute. The wrapper handles the common pattern of "render this
  // field only if the backend provided it".
  function setMetaRow(key, html, isHtml) {
    const row = metaRowEls[key];
    const val = metaValEls[key];
    if (!row || !val) return;
    if (html === null || html === undefined || html === '') {
      row.hidden = true;
      val.textContent = '';
      val.innerHTML = '';
      return;
    }
    if (isHtml) {
      val.innerHTML = html;
    } else {
      val.textContent = html;
    }
    row.hidden = false;
  }

  // Render the small status pill matching the home-page chips.
  function statusPillHtml(status) {
    if (!status) return '';
    const safe = esc(status);
    return '<span class="badge state-' + safe + '">' + safe + '</span>';
  }

  // Render a "depends on" / "dependents" list as clickable anchors.
  // Each anchor links back to the home-page row (#queue-<id>).
  function depListHtml(deps) {
    if (!deps || !deps.length) return '';
    return deps.map((d) => {
      if (d && typeof d === 'object') {
        const id = d.id || '';
        const st = d.status || '';
        return '<a class="log-meta-dep" href="#queue-' + encodeURIComponent(id) + '">' +
          esc(id) +
          (st ? ' <span class="log-meta-dep-status">[' + esc(st) + ']</span>' : '') +
          '</a>';
      }
      const id = String(d || '');
      return '<a class="log-meta-dep" href="#queue-' + encodeURIComponent(id) + '">' + esc(id) + '</a>';
    }).join(' ');
  }

  // Clear every meta-summary row + return block + the section's hidden
  // flag. Called on modal open BEFORE the fetch lands so a slow request
  // doesn't show stale data from the previous opening. Also re-applies
  // the persisted metadata-collapse state so a fresh open reflects the
  // viewer's latest choice (including a toggle made in another tab).
  function resetMetaSummary() {
    if (metaSummaryEl) metaSummaryEl.hidden = true;
    Object.keys(metaRowEls).forEach((k) => setMetaRow(k, ''));
    if (returnDetailsEl) {
      returnDetailsEl.hidden = true;
      returnDetailsEl.open = true;
    }
    if (returnBodyEl) returnBodyEl.textContent = '';
    // Tear down the captured-script block so a slow meta fetch on the
    // next open doesn't show stale content from the previous modal.
    if (scriptCaptureDetailsEl) {
      scriptCaptureDetailsEl.hidden = true;
      scriptCaptureDetailsEl.open = false;
    }
    if (scriptCaptureHeaderEl) {
      scriptCaptureHeaderEl.textContent = '';
      scriptCaptureHeaderEl.innerHTML = '';
    }
    if (scriptCaptureBodyEl) scriptCaptureBodyEl.textContent = '';
    if (scriptCaptureLabelEl) scriptCaptureLabelEl.textContent = 'Script contents';
    setMetaToggleInitialState();
    // Tear down any prior runtime ticker — a stale interval from the
    // previous modal-open would otherwise keep updating #log-meta-runtime
    // with the previous item's start anchor.
    stopRuntimeTicker();
    const runtimeRow = metaRowEls.runtime;
    const runtimeVal = metaValEls.runtime;
    if (runtimeRow) runtimeRow.removeAttribute('data-started-at');
    if (runtimeVal) runtimeVal.removeAttribute('data-started-at');
  }

  // Apply a parsed /api/queue/<id>/meta payload to the summary block.
  // Treats every field as optional (missing rows stay hidden). Logs
  // and ignores anything that's the wrong shape — the modal still
  // shows the prompt + live transcript even when the meta fetch fails.
  function applyMetaSummary(meta) {
    if (!meta || typeof meta !== 'object' || !meta.ok) return;
    if (!metaSummaryEl) return;

    // Subagent meta payload (/api/subagent/<id>/meta) is a different,
    // smaller shape: { ok, subagent_id, parent_session_id, label, age,
    // age_seconds }. It has none of the queue-state fields, so render a
    // compact subagent-specific header and return early rather than
    // showing a row of "undefined" pills. The live transcript below is
    // identical to the agent stream.
    if (meta.subagent_id) {
      stopRuntimeTicker();
      if (meta.label) {
        setMetaRow('status', esc(meta.label), true);
      } else {
        setMetaRow('status', esc(meta.subagent_id), true);
      }
      if (meta.parent_session_id) {
        setMetaRow('group', 'session ' + esc(meta.parent_session_id), true);
      } else {
        setMetaRow('group', '');
      }
      if (meta.age) {
        setMetaRow('runtime', esc(meta.age), false);
      } else {
        setMetaRow('runtime', '');
      }
      // Hide the queue-only rows in subagent mode.
      setMetaRow('times', '');
      setMetaRow('scope', '');
      setMetaRow('deps', '');
      setMetaRow('dependents', '');
      setMetaRow('by', '');
      setMetaRow('usage', '');
      setMetaRow('abandon', '');
      metaSummaryEl.hidden = false;
      return;
    }

    // status pill (always visible when meta loaded)
    setMetaRow('status', statusPillHtml(meta.status), true);

    // runtime — "5m 23s (started 16:57:03, completed 17:02:03)" style.
    //
    // For running items we ALSO start a 1Hz ticker that recomputes
    // (now - started_at) locally each second so the displayed value
    // updates live (otherwise it stays frozen at the modal-open
    // value — Andrew flagged a "19m 26s" frozen value 2026-05-13).
    // The ticker reads `started_at` from this same meta payload, so
    // it's accurate even when the server's runtime_seconds is
    // already a few hundred ms stale by the time the JSON lands.
    //
    // Non-running items (done / abandoned / pending / starting)
    // intentionally render only the static server-computed value
    // and keep the ticker stopped — their runtime no longer advances.
    if (meta.runtime_seconds !== null && meta.runtime_seconds !== undefined) {
      setMetaRow('runtime', fmtRuntime(meta.runtime_seconds), false);
    } else {
      setMetaRow('runtime', '');
    }
    if (meta.status === 'running' && meta.started_at) {
      // Stamp data-started-at on the row element so an external test
      // (or future feature) can find the live runtime anchor without
      // needing to call into __liveLog hooks.
      const runtimeRow = metaRowEls.runtime;
      const runtimeVal = metaValEls.runtime;
      if (runtimeRow) runtimeRow.setAttribute('data-started-at', meta.started_at);
      if (runtimeVal) runtimeVal.setAttribute('data-started-at', meta.started_at);
      startRuntimeTicker(meta.started_at);
    } else {
      stopRuntimeTicker();
      const runtimeRow = metaRowEls.runtime;
      const runtimeVal = metaValEls.runtime;
      if (runtimeRow) runtimeRow.removeAttribute('data-started-at');
      if (runtimeVal) runtimeVal.removeAttribute('data-started-at');
    }

    // timestamps — created / started / completed / abandoned (whichever exist)
    const tsParts = [];
    if (meta.created_at) tsParts.push('created ' + fmtLocalIso(meta.created_at));
    if (meta.started_at) tsParts.push('started ' + fmtLocalIso(meta.started_at));
    if (meta.completed_at) tsParts.push('completed ' + fmtLocalIso(meta.completed_at));
    if (meta.abandoned_at) tsParts.push('abandoned ' + fmtLocalIso(meta.abandoned_at));
    setMetaRow('times', tsParts.length ? tsParts.join(' · ') : '');

    // scope chips
    if (meta.scope && meta.scope.length) {
      const chips = meta.scope.map((s) => '<span class="chip">' + esc(s) + '</span>').join(' ');
      setMetaRow('scope', chips, true);
    } else {
      setMetaRow('scope', '');
    }

    // depends_on
    if (meta.depends_on_status && meta.depends_on_status.length) {
      setMetaRow('deps', depListHtml(meta.depends_on_status), true);
    } else if (meta.depends_on && meta.depends_on.length) {
      setMetaRow('deps', depListHtml(meta.depends_on), true);
    } else {
      setMetaRow('deps', '');
    }

    // dependents
    if (meta.dependents && meta.dependents.length) {
      setMetaRow('dependents', depListHtml(meta.dependents), true);
    } else {
      setMetaRow('dependents', '');
    }

    // created_by
    setMetaRow('by', meta.created_by || '');

    // group_id + group_head
    if (meta.group_id) {
      const headSuffix = meta.group_head ? ' (head)' : '';
      setMetaRow('group', meta.group_id + headSuffix);
    } else {
      setMetaRow('group', '');
    }

    // usage (token count / tool uses / duration). Comes from the agent's
    // task-notification payload — only present for done items spawned
    // as background subagents.
    const agent = meta.agent || null;
    if (agent) {
      const usageParts = [];
      if (agent.usage_total_tokens != null) {
        usageParts.push(agent.usage_total_tokens.toLocaleString() + ' tokens');
      }
      if (agent.usage_tool_uses != null) {
        usageParts.push(agent.usage_tool_uses + ' tool calls');
      }
      if (agent.usage_duration_ms != null) {
        usageParts.push(fmtRuntime(agent.usage_duration_ms / 1000) + ' agent runtime');
      }
      if (agent.return_status) {
        usageParts.push('status=' + agent.return_status);
      }
      setMetaRow('usage', usageParts.length ? usageParts.join(' · ') : '');

      // Return text — surface as a default-open <details> so the most
      // important piece of "what did this agent do?" is right there.
      if (returnDetailsEl && returnBodyEl) {
        if (agent.return_text) {
          returnBodyEl.textContent = agent.return_text;
          if (returnLabelEl) {
            const tag = agent.return_status ? ' (' + agent.return_status + ')' : '';
            returnLabelEl.textContent = 'Agent return value' + tag +
              ' (' + agent.return_text.length + ' chars)';
          }
          returnDetailsEl.hidden = false;
          returnDetailsEl.open = true;
        } else {
          returnDetailsEl.hidden = true;
          returnBodyEl.textContent = '';
        }
      }
    } else {
      setMetaRow('usage', '');
    }

    // abandon reason (only set when the item was abandoned).
    setMetaRow('abandon', meta.abandon_reason || '');

    // Captured script contents — only present for workload-bound
    // items whose command parsed as `<interpreter> <path>`. The
    // backend serves a `null` payload (or omits the key for older
    // server versions) when nothing was captured; we treat both as
    // "hide the section". Body is rendered as plain text (esc()'d)
    // into a <pre> so script content with HTML-looking tokens
    // doesn't get interpreted.
    applyScriptCapture(meta.script_capture);

    metaSummaryEl.hidden = false;
  }

  // Render the captured-script disclosure block from the meta payload.
  // The capture object shape (from /api/queue/<id>/meta):
  //
  //   {
  //     path: "/tmp/foo.sh",
  //     interpreter: "bash",
  //     size_bytes: 42,
  //     truncated: false,
  //     binary: false,
  //     content: "#!/bin/bash\necho hi\n" | null,
  //     sha256: "abc123...",
  //   }
  //
  // When `binary` is true the server omits `content`; we keep the
  // disclosure visible (so users can see WHAT ran) but render a
  // "(binary content, body omitted)" placeholder in the body.
  function applyScriptCapture(cap) {
    if (!scriptCaptureDetailsEl || !scriptCaptureHeaderEl || !scriptCaptureBodyEl) return;
    if (!cap || typeof cap !== 'object') {
      scriptCaptureDetailsEl.hidden = true;
      scriptCaptureHeaderEl.textContent = '';
      scriptCaptureBodyEl.textContent = '';
      return;
    }
    // Small header line: "<interpreter> <path> · <size> bytes · sha256
    // <12-char prefix>". Keeps the body uncluttered while still giving
    // the viewer enough to verify identity / compare against an
    // on-disk copy.
    const headerParts = [];
    const interp = typeof cap.interpreter === 'string' ? cap.interpreter : '';
    const p = typeof cap.path === 'string' ? cap.path : '';
    if (interp || p) headerParts.push(esc(interp + ' ' + p).trim());
    if (typeof cap.size_bytes === 'number') {
      headerParts.push(cap.size_bytes.toLocaleString() + ' bytes');
    }
    if (typeof cap.sha256 === 'string' && cap.sha256.length >= 12) {
      headerParts.push('sha256 ' + esc(cap.sha256.slice(0, 12)) + '…');
    }
    if (cap.truncated) {
      headerParts.push('<strong>(truncated to 1 MiB)</strong>');
    }
    if (cap.binary) {
      headerParts.push('<strong>(binary)</strong>');
    }
    scriptCaptureHeaderEl.innerHTML = headerParts.join(' · ');

    if (cap.binary || cap.content === null || cap.content === undefined) {
      scriptCaptureBodyEl.textContent =
        '(binary content — body omitted; ' +
        (typeof cap.size_bytes === 'number' ? cap.size_bytes : '?') +
        ' bytes total)';
    } else {
      // Plain text — esc by way of textContent (avoids any HTML
      // interpretation, no need to call esc() manually here).
      scriptCaptureBodyEl.textContent = String(cap.content);
    }
    scriptCaptureDetailsEl.hidden = false;
    // Stay collapsed by default — most viewers don't need to see the
    // script body unless they're debugging.
    scriptCaptureDetailsEl.open = false;
  }

  // Fire the meta fetch in the background — we don't block the
  // EventSource open on it. If the fetch fails we silently leave the
  // summary block hidden; the prompt + transcript still render.
  function fetchMetaSummary(qid) {
    if (!qid) return;
    // Subagent mode resolves its own cheap meta endpoint; the payload shape
    // differs (subagent_id / parent_session_id / label) so applyMetaSummary
    // tolerates missing queue-only fields gracefully.
    const metaUrl = (mode === 'subagent')
      ? '/api/subagent/' + encodeURIComponent(qid) + '/meta'
      : '/api/queue/' + encodeURIComponent(qid) + '/meta';
    fetch(metaUrl, {
      headers: { 'Accept': 'application/json' },
      credentials: 'same-origin',
    })
      .then((r) => {
        if (!r.ok) return null;
        return r.json();
      })
      .then((meta) => {
        if (meta) applyMetaSummary(meta);
      })
      .catch(() => {
        // Best-effort — silent on failure.
      });
  }

  // --- HTML escaping (basic) ---
  function esc(str) {
    if (str === null || str === undefined) return '';
    return String(str)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;');
  }

  // Build a collapsed-by-default expandable view for a long text blob.
  // Reuses the .prompt-toggle / .prompt-summary / .prompt-body classes
  // that the index.html prompt section already styles in style.css —
  // visual consistency is the whole point. Click the summary to expand
  // into a max-height-scrollable <pre>.
  //
  //   summaryText  short label rendered next to the disclosure caret
  //                (e.g. "Bash command (3.2 KB)" or first ~100 chars).
  //   fullText     raw text the user wants to read; HTML-escaped here.
  //   inline       optional HTML to put NEXT TO the toggle (uncollapsed,
  //                always visible). Used for event-line summaries that
  //                should still be readable at a glance.
  //
  // Returns a single-string of HTML that a formatter can drop into its
  // `body` field.
  function expandable(summaryText, fullText, inline) {
    const inlinePart = inline ? inline + ' ' : '';
    return (
      inlinePart +
      '<details class="prompt-toggle log-expand">' +
      '<summary class="prompt-summary">' + esc(summaryText) + '</summary>' +
      '<pre class="prompt-body">' + esc(fullText) + '</pre>' +
      '</details>'
    );
  }

  // Decide whether `text` warrants the expandable treatment. Below the
  // threshold we just return the escaped text inline — no chrome, no
  // disclosure caret. Above, we wrap in <details> with a tease summary.
  //   threshold   max chars before we collapse
  //   teaseLen    length of the inline preview shown next to the caret
  //   labelKind   short prefix for the summary line ("text", "output",
  //               "args", "json")
  function bodyOrExpandable(text, threshold, teaseLen, labelKind) {
    text = String(text == null ? '' : text);
    if (text.length <= threshold && !text.includes('\n')) {
      return esc(text);
    }
    const tease = text.replace(/\s+/g, ' ').slice(0, teaseLen);
    const more = text.length > teaseLen ? '…' : '';
    const summary = (labelKind ? '[' + labelKind + ' ' + text.length + ' chars] ' : '') + tease + more;
    return expandable(summary, text);
  }

  // Build a "metadata" disclosure exposing the per-record fields that
  // are useful for debugging but noisy by default: assistant `model`,
  // `usage` token counts, `stop_reason`, `requestId`, `attributionAgent`
  // (which subagent type spawned the call), `uuid`/`parentUuid` (turn
  // linkage), `caller` (direct vs subagent tool dispatch), `slug`
  // (claude-watch's friendly session label). Caller passes whichever
  // subset applies to its event kind.
  //
  // Returns an HTML string ready to drop into the body, or '' when no
  // metadata is available (so callers can `body += ' ' + meta;`
  // without conditionals).
  function metaDisclosure(rec) {
    if (!rec || typeof rec !== 'object') return '';
    const msg = rec.message || {};
    const fields = [];
    if (msg.model) fields.push(['model', msg.model]);
    if (msg.stop_reason) fields.push(['stop_reason', msg.stop_reason]);
    if (msg.stop_sequence) fields.push(['stop_sequence', msg.stop_sequence]);
    if (msg.stop_details) {
      fields.push(['stop_details', JSON.stringify(msg.stop_details)]);
    }
    if (msg.usage) {
      const u = msg.usage;
      const parts = [];
      if (u.input_tokens != null) parts.push('in=' + u.input_tokens);
      if (u.output_tokens != null) parts.push('out=' + u.output_tokens);
      if (u.cache_creation_input_tokens) parts.push('cache_create=' + u.cache_creation_input_tokens);
      if (u.cache_read_input_tokens) parts.push('cache_read=' + u.cache_read_input_tokens);
      if (u.service_tier) parts.push('tier=' + u.service_tier);
      fields.push(['usage', parts.join(' ')]);
      // The cache_creation breakdown (5m vs 1h) is useful for prompt
      // caching audits — surface it as a second line.
      if (u.cache_creation && typeof u.cache_creation === 'object') {
        const cc = u.cache_creation;
        const ccParts = [];
        if (cc.ephemeral_5m_input_tokens != null) ccParts.push('5m=' + cc.ephemeral_5m_input_tokens);
        if (cc.ephemeral_1h_input_tokens != null) ccParts.push('1h=' + cc.ephemeral_1h_input_tokens);
        if (ccParts.length) fields.push(['cache_creation', ccParts.join(' ')]);
      }
    }
    if (rec.requestId) fields.push(['requestId', rec.requestId]);
    if (rec.attributionAgent) fields.push(['attributionAgent', rec.attributionAgent]);
    if (rec.agentId) fields.push(['agentId', rec.agentId]);
    if (rec.uuid) fields.push(['uuid', rec.uuid]);
    if (rec.parentUuid) fields.push(['parentUuid', rec.parentUuid]);
    if (rec.sourceToolAssistantUUID) fields.push(['sourceToolAssistantUUID', rec.sourceToolAssistantUUID]);
    if (rec.slug) fields.push(['slug', rec.slug]);
    if (rec.gitBranch) fields.push(['gitBranch', rec.gitBranch]);
    if (rec.cwd) fields.push(['cwd', rec.cwd]);
    if (rec.version) fields.push(['version', rec.version]);
    if (rec.userType) fields.push(['userType', rec.userType]);
    if (rec.entrypoint) fields.push(['entrypoint', rec.entrypoint]);
    if (rec.sessionId) fields.push(['sessionId', rec.sessionId]);
    if (rec.promptId) fields.push(['promptId', rec.promptId]);
    if (rec.isSidechain != null) fields.push(['isSidechain', String(rec.isSidechain)]);
    if (rec.isMeta != null) fields.push(['isMeta', String(rec.isMeta)]);
    if (!fields.length) return '';
    const summary = 'metadata (' + fields.length + ' fields)';
    const body = fields.map((kv) => kv[0] + ': ' + kv[1]).join('\n');
    return ' ' + expandable(summary, body);
  }

  // Tool-use input is rendered as a single-line headline plus an
  // optional "full input" disclosure. When the tool exposes a `caller`
  // field (direct vs subagent), surface that as a small badge so the
  // viewer can tell which dispatcher invoked the call.
  function callerBadge(tu) {
    if (!tu || typeof tu !== 'object') return '';
    const c = tu.caller;
    if (!c || typeof c !== 'object') return '';
    const t = c.type;
    if (!t) return '';
    return ' <span class="log-caller">via ' + esc(t) + '</span>';
  }

  // --- Pretty-printers per event kind ---
  // Each takes the parsed `rec` and returns an HTML string + CSS class
  // hint. We render extra detail in a <details> wrapper so the line
  // stays compact by default.

  function fmtTs(rec) {
    const ts = rec && rec.timestamp;
    if (!ts) return '';
    // Render in the viewer's local timezone via LocalTime.timeOnly().
    // Backend ships UTC ISO8601; conversion is purely frontend.
    if (window.LocalTime && typeof window.LocalTime.timeOnly === 'function') {
      const local = window.LocalTime.timeOnly(ts);
      if (local) return local;
    }
    // Fallback: extract HH:MM:SS from the UTC string if LocalTime is
    // unavailable (script load failure / file:// preview / etc).
    const m = String(ts).match(/T(\d\d:\d\d:\d\d)/);
    return m ? m[1] : '';
  }

  // Collapse a string into a single-line headline preview — strip newlines,
  // collapse whitespace, slice to `maxLen` chars, append "…" when truncated.
  // Returns the (escaped) HTML string ready to drop into the headline span.
  function headlinePreview(text, maxLen) {
    if (text === null || text === undefined) return '';
    const s = String(text).replace(/\s+/g, ' ').trim();
    if (!s) return '';
    const limit = maxLen || 100;
    const truncated = s.length > limit ? s.slice(0, limit) + '…' : s;
    return esc(truncated);
  }

  function fmtUser(rec) {
    const msg = rec.message || {};
    const content = msg.content;
    let text = '';
    if (typeof content === 'string') {
      text = content;
    } else if (Array.isArray(content)) {
      text = content
        .filter((c) => c && c.type === 'text')
        .map((c) => c.text || '')
        .join(' ');
    }
    return {
      cls: 'log-user',
      label: 'USER',
      headline: headlinePreview(text, 100),
      body: bodyOrExpandable(text, 240, 100, 'text') + metaDisclosure(rec),
    };
  }

  function fmtAssistantText(rec) {
    const msg = rec.message || {};
    const content = msg.content || [];
    // Render *every* text + thinking block in the order they appear so
    // we don't drop a thinking block that was emitted in the same turn
    // as the assistant text. Each block gets its own row so the visual
    // hierarchy matches the JSONL.
    let body = '';
    let headlineText = '';
    if (Array.isArray(content)) {
      const blocks = content.filter((c) => c && (c.type === 'text' || c.type === 'thinking'));
      const parts = blocks.map((c) => {
        if (c.type === 'thinking') {
          const t = String(c.thinking || '');
          if (!headlineText) headlineText = '[thinking] ' + t;
          return '<div class="log-thinking">' +
            '<span class="log-thinking-label">[thinking]</span> ' +
            bodyOrExpandable(t, 320, 120, 'thinking') +
            '</div>';
        }
        const t = String(c.text || '');
        if (!headlineText) headlineText = t;
        return bodyOrExpandable(t, 320, 100, 'text');
      });
      body = parts.join(' ');
    }
    return {
      cls: 'log-assistant',
      label: 'ASSISTANT',
      headline: headlinePreview(headlineText, 100),
      body: body + metaDisclosure(rec),
    };
  }

  // Thinking-only assistant turns (no text, no tool_use in the same
  // record). The kind hint comes from the server when the first block
  // type is `thinking`.
  function fmtThinking(rec) {
    const msg = rec.message || {};
    const content = msg.content || [];
    let text = '';
    if (Array.isArray(content)) {
      text = content
        .filter((c) => c && c.type === 'thinking')
        .map((c) => c.thinking || '')
        .join('\n\n');
    }
    return {
      cls: 'log-thinking-line',
      label: 'THINKING',
      headline: headlinePreview(text, 100),
      body: bodyOrExpandable(text, 320, 120, 'thinking') + metaDisclosure(rec),
    };
  }

  function fmtToolUse(rec) {
    const msg = rec.message || {};
    const content = msg.content || [];
    const tu = Array.isArray(content)
      ? content.find((c) => c && c.type === 'tool_use')
      : null;
    if (!tu) return { cls: 'log-tool-use', label: 'TOOL', headline: '(unknown)', body: '(unknown)' };
    // Co-occurring thinking block in the same assistant record — keep it
    // visible. Claude-sonnet rarely emits both in one record but
    // claude-opus does, and dropping the thinking would hide rationale.
    const thinkingBlocks = Array.isArray(content)
      ? content.filter((c) => c && c.type === 'thinking')
      : [];
    const thinkingHtml = thinkingBlocks.map((c) => {
      const t = String(c.thinking || '');
      return '<div class="log-thinking">' +
        '<span class="log-thinking-label">[thinking]</span> ' +
        bodyOrExpandable(t, 320, 120, 'thinking') +
        '</div>';
    }).join('');
    const name = tu.name || '?';
    const input = tu.input || {};
    // The headline summary is the most-useful field per tool — Bash
    // command, file path, search pattern. Path-style fields are kept
    // intact (no truncation) so users can read the whole filename.
    //
    // Bash callers usually pass a short imperative `description` field
    // alongside `command` (e.g. {"command": "ls ...", "description":
    // "List MKV files in season dir"}). When present we prefer the
    // description for the inline summary — it's purpose-written for a
    // human glance and matches what the task-watch dashboard renders
    // as the green `$ <description>` line (see
    // src/task_filters.rs::format_line — Bash branch). This keeps the
    // queue-minisite live-log headline at parity with task-watch.
    let summary = '';
    let summarySource = '';
    if (name === 'Bash') {
      const desc = typeof input.description === 'string' ? input.description.trim() : '';
      if (desc) {
        summary = desc;
        summarySource = 'description';
      } else {
        summary = input.command || '';
        summarySource = 'command';
      }
    } else if (name === 'Read' || name === 'Edit' || name === 'Write') {
      summary = input.file_path || '';
    } else if (name === 'Grep' || name === 'Glob') {
      summary = input.pattern || input.glob || '';
    } else {
      // Generic dump of small input keys — show key names, full values
      // get the expandable treatment via the all-input details below.
      const keys = Object.keys(input).slice(0, 3);
      summary = keys.join(', ');
    }
    // Inline summary: short fields show inline; long fields collapse to
    // an expandable. The full tool input JSON is ALWAYS available via a
    // second disclosure so users can see every argument, not just the
    // headline field. Wrap in <code> only when it stays a short inline
    // string — for long values bodyOrExpandable returns a <details>
    // block which can't sit inside <code> (block-in-inline).
    const isShortHeadline = summary.length <= 240 && !summary.includes('\n');
    const inline = isShortHeadline
      ? '<code>' + esc(summary) + '</code>'
      : bodyOrExpandable(summary, 240, 100, name);
    let body = inline;
    // For Bash with a description-driven headline, also surface the
    // actual `command` inline (right after the description) so users
    // don't have to expand "full input" just to see what ran. Mirrors
    // how task-watch shows the green `$ <description>` line and then
    // bash_progress output below — here the command itself fills that
    // "what actually ran" slot.
    if (name === 'Bash' && summarySource === 'description' &&
        typeof input.command === 'string' && input.command.length > 0) {
      const cmd = input.command;
      const isShortCmd = cmd.length <= 240 && !cmd.includes('\n');
      const cmdInline = isShortCmd
        ? '<code>' + esc(cmd) + '</code>'
        : bodyOrExpandable(cmd, 240, 100, 'Bash');
      body += ' <span class="log-tool-cmd">$ ' + cmdInline + '</span>';
    }
    // Only attach the "full input" disclosure if there's more than the
    // headline field(s) we've already surfaced inline — avoids redundant
    // <details> for Bash where input.command IS the entire input.
    const inputKeys = Object.keys(input);
    const headlineFields = (name === 'Bash')
      ? (summarySource === 'description' ? ['command', 'description'] : ['command'])
      : (name === 'Read' || name === 'Edit' || name === 'Write') ? ['file_path']
      : (name === 'Grep' || name === 'Glob') ? [input.pattern ? 'pattern' : 'glob']
      : [];
    const hasExtra = inputKeys.some((k) => !headlineFields.includes(k));
    if (hasExtra && inputKeys.length > 0) {
      const fullJson = JSON.stringify(input, null, 2);
      body += ' ' + expandable(
        'full input (' + inputKeys.length + ' keys, ' + fullJson.length + ' chars)',
        fullJson
      );
    }
    body += callerBadge(tu);
    body += metaDisclosure(rec);
    // Surface the tool_use id so users can correlate with the
    // matching tool_result line. Compact and inline.
    if (tu.id) {
      body += ' <span class="log-tool-id">[' + esc(tu.id) + ']</span>';
    }
    // Prepend any thinking block that shared this assistant record.
    body = thinkingHtml + body;
    // One-line headline — `ToolName(short arg summary)`, mirrors agent-tail's
    // `TOOL_CALL <Name>(<one-line-args>)` format. For Bash/Read/etc. we use
    // the primary input field; for generic tools we render a JSON one-liner.
    let headlineArgs;
    if (name === 'Bash' || name === 'Read' || name === 'Edit' || name === 'Write' ||
        name === 'Grep' || name === 'Glob') {
      headlineArgs = summary;
    } else {
      // Compact JSON repr (no whitespace) — same shape as task-watch.
      try {
        headlineArgs = JSON.stringify(input);
      } catch (_) {
        headlineArgs = summary;
      }
    }
    const headline = esc(name) + '(' + headlinePreview(headlineArgs, 80) + ')';
    return {
      cls: 'log-tool-use',
      label: 'TOOL ' + esc(name),
      headline: headline,
      body: body,
    };
  }

  function fmtToolResult(rec) {
    const msg = rec.message || {};
    const content = msg.content || [];
    const tr = Array.isArray(content)
      ? content.find((c) => c && c.type === 'tool_result')
      : null;
    if (!tr) return { cls: 'log-tool-result', label: 'RESULT', headline: '', body: '' };
    let body = tr.content;
    if (Array.isArray(body)) {
      body = body
        .map((c) => (c && typeof c === 'object' ? (c.text || '') : String(c)))
        .join('');
    }
    body = String(body || '');
    const isErr = tr.is_error === true;
    // Tool results can be huge — short results render inline, longer
    // ones collapse to a click-to-expand <details> with a max-height
    // scrollable pre. Threshold matches the chars-per-screenful budget
    // we'd want before stealing modal real estate.
    const lines = body.split(/\r?\n/);
    let html;
    if (body.length <= 240 && lines.length <= 4) {
      html = '<pre class="log-inline-pre">' + esc(body) + '</pre>';
    } else {
      const head = lines.slice(0, 4).join('\n');
      const teaseTail = lines.length > 4 ? '\n…' : '';
      const summary = '[output ' + body.length + ' chars, ' + lines.length + ' lines] click to expand';
      html =
        '<pre class="log-inline-pre">' + esc(head + teaseTail) + '</pre>' +
        expandable(summary, body);
    }
    // Surface the matching tool_use id so the row can be correlated
    // with its TOOL call line above.
    if (tr.tool_use_id) {
      html += ' <span class="log-tool-id">[' + esc(tr.tool_use_id) + ']</span>';
    }
    // Side-channel `toolUseResult` (top-level on the user record) —
    // claude-code stashes a verbatim summary here that sometimes
    // diverges from `content` (e.g. the obligation-gate denial message
    // appears here as a clean `Error: ...` while content has the full
    // <tool_use_error> wrapper). Surface it behind a disclosure so
    // it's never the headline but always reachable.
    if (rec.toolUseResult && typeof rec.toolUseResult === 'string' &&
        rec.toolUseResult !== body) {
      html += ' ' + expandable(
        'toolUseResult (' + rec.toolUseResult.length + ' chars)',
        rec.toolUseResult
      );
    } else if (rec.toolUseResult && typeof rec.toolUseResult === 'object') {
      const j = JSON.stringify(rec.toolUseResult, null, 2);
      html += ' ' + expandable('toolUseResult (' + j.length + ' chars)', j);
    }
    html += metaDisclosure(rec);
    // Headline: `[tool_use_id_short] first-line-of-body`. Matches
    // agent-tail's `TOOL_RESULT [toolu_X]  <preview>` line. Short id is
    // the last 6 chars of the toolu_… id (the full id lives in the
    // expanded body via .log-tool-id).
    const idShort = tr.tool_use_id
      ? String(tr.tool_use_id).slice(-6)
      : '';
    const idPart = idShort ? '[' + idShort + '] ' : '';
    const firstLine = body.split(/\r?\n/)[0] || '';
    const headline = idPart + headlinePreview(firstLine, 80);
    return {
      cls: 'log-tool-result' + (isErr ? ' log-tool-error' : ''),
      label: isErr ? 'RESULT (err)' : 'RESULT',
      headline: headline,
      body: html,
    };
  }

  // User image content blocks (`{type: image, source: {type: base64,
  // media_type, data}}`). The base64 payload can be hundreds of KB of
  // useless-in-text bytes — render a tiny inline thumbnail when the
  // source is base64 + image/*, and put the byte count + media type
  // inline. Multiple image blocks per record are rendered side by side.
  function fmtUserImage(rec) {
    const msg = rec.message || {};
    const content = msg.content;
    const blocks = Array.isArray(content)
      ? content.filter((c) => c && c.type === 'image')
      : [];
    if (!blocks.length) {
      return { cls: 'log-user-image', label: 'USER IMAGE', headline: '(no image blocks)', body: '(no image blocks)' };
    }
    const parts = blocks.map((b) => {
      const src = b.source || {};
      const mt = src.media_type || 'image/?';
      const data = src.data;
      const stype = src.type || 'unknown';
      let inline = '<span>[' + esc(mt) + ' / ' + esc(stype) + ']</span>';
      // Render an inline preview only for reasonably sized payloads —
      // arbitrary 2MB ceiling so the modal doesn't choke on a giant
      // PDF page embedded as base64.
      if (stype === 'base64' && typeof data === 'string' && data.length < 2_000_000 && mt.startsWith('image/')) {
        const dataUrl = 'data:' + mt + ';base64,' + data;
        inline += ' <img class="log-image-thumb" src="' + dataUrl + '" alt="user image">';
      }
      if (typeof data === 'string') {
        inline += ' <span class="log-meta">(' + data.length + ' base64 chars)</span>';
      }
      return inline;
    });
    // Headline: count + media types so the operator sees image count
    // without expanding (e.g. "1 image (image/png)" or "3 images").
    const mediaTypes = blocks
      .map((b) => (b.source && b.source.media_type) || 'image/?')
      .filter((v, i, a) => a.indexOf(v) === i);
    const headline = blocks.length === 1
      ? '1 image (' + esc(mediaTypes[0]) + ')'
      : blocks.length + ' images (' + esc(mediaTypes.join(', ')) + ')';
    return {
      cls: 'log-user-image',
      label: 'USER IMAGE',
      headline: headline,
      body: parts.join(' ') + metaDisclosure(rec),
    };
  }

  function fmtAttachment(rec) {
    const a = rec.attachment || {};
    const t = a.type || 'attachment';
    // Surface the full file path inline (no truncation — paths are
    // useful in their entirety). The full attachment record drops into
    // an expandable for any extra metadata fields.
    const path = a.path || a.file_path || a.filename || '';
    const inline = path
      ? '<code>' + esc(path) + '</code>'
      : '';
    const fullJson = JSON.stringify(a, null, 2);
    let body = inline;
    // Only attach the JSON disclosure if there's more than just the
    // path — keeps single-field attachments clean.
    const extraKeys = Object.keys(a).filter((k) => k !== 'path' && k !== 'file_path' && k !== 'filename' && k !== 'type');
    if (extraKeys.length > 0 || !path) {
      const summary = path
        ? 'metadata (' + extraKeys.length + ' extra fields)'
        : '[attachment ' + fullJson.length + ' chars]';
      body = (body ? body + ' ' : '') + expandable(summary, fullJson);
    }
    // Headline: type + path. Path is the most-useful field, so we keep
    // the FULL path in the headline (no truncation) — it's typically
    // short enough to fit, and truncating filenames defeats the point.
    const headline = headlinePreview(path || JSON.stringify(a), 100);
    return {
      cls: 'log-attachment',
      label: 'ATTACH ' + esc(t),
      headline: headline,
      body: body || esc('(empty)'),
    };
  }

  function fmtSystem(rec) {
    // Surface known system subtypes as compact one-liners; fall back
    // to a JSON dump for unknown shapes. `turn_duration` is the common
    // case in subagent transcripts — claude-code stamps each completed
    // assistant turn with its wall-clock cost.
    const subtype = rec.subtype || '';
    const dur = rec.durationMs;
    const content = rec.content;
    const inlineParts = [];
    if (subtype) inlineParts.push('subtype=' + subtype);
    if (dur != null) {
      const sec = (dur / 1000).toFixed(2);
      inlineParts.push('duration=' + sec + 's');
    }
    if (rec.level) inlineParts.push('level=' + rec.level);
    if (rec.hookCount != null) inlineParts.push('hookCount=' + rec.hookCount);
    if (rec.preventedContinuation != null) inlineParts.push('preventedContinuation=' + rec.preventedContinuation);
    if (rec.toolUseID) inlineParts.push('toolUseID=' + rec.toolUseID);
    let inline = inlineParts.length ? esc(inlineParts.join(' ')) : '';
    if (typeof content === 'string' && content) {
      inline += (inline ? ' ' : '') + bodyOrExpandable(content, 200, 100, 'content');
    } else if (content && typeof content === 'object') {
      const j = JSON.stringify(content, null, 2);
      inline += (inline ? ' ' : '') + expandable('content (' + j.length + ' chars)', j);
    }
    const fullJson = JSON.stringify(rec, null, 2);
    const body = (inline || esc('(no content)')) +
      ' ' + expandable('full record (' + fullJson.length + ' chars)', fullJson) +
      metaDisclosure(rec);
    // Headline: inline parts (subtype + duration + level + etc.) plus a
    // short preview of content if it's a string.
    const headlineParts = inlineParts.slice();
    if (typeof content === 'string' && content) {
      headlineParts.push(content.replace(/\s+/g, ' ').slice(0, 60));
    }
    return {
      cls: 'log-system',
      label: 'SYSTEM' + (subtype ? ' ' + esc(subtype) : ''),
      headline: headlinePreview(headlineParts.join(' '), 100),
      body: body,
    };
  }

  // Progress events fire while a tool is running (e.g. hook callbacks
  // mid-flight). The shape is `{type: progress, data: {...},
  // toolUseID, parentToolUseID, ...}`. Render the data subtype + the
  // tool it's reporting against, with a full-record disclosure.
  function fmtProgress(rec) {
    const data = rec.data || {};
    const dataType = data.type || '?';
    const inlineParts = [dataType];
    if (data.hookEvent) inlineParts.push('hookEvent=' + data.hookEvent);
    if (data.hookName) inlineParts.push('hookName=' + data.hookName);
    if (data.command) inlineParts.push('command=' + data.command);
    if (rec.toolUseID) inlineParts.push('toolUseID=' + rec.toolUseID);
    const inline = esc(inlineParts.join(' '));
    const fullJson = JSON.stringify(rec, null, 2);
    return {
      cls: 'log-progress',
      label: 'PROGRESS',
      headline: headlinePreview(inlineParts.join(' '), 100),
      body: inline +
        ' ' + expandable('full record (' + fullJson.length + ' chars)', fullJson) +
        metaDisclosure(rec),
    };
  }

  function fmtUnknown(rec) {
    const json = JSON.stringify(rec, null, 2);
    const jsonOneLine = JSON.stringify(rec || {});
    return {
      cls: 'log-unknown',
      label: rec && rec.type ? esc(rec.type).toUpperCase() : 'EVENT',
      headline: headlinePreview(jsonOneLine, 100),
      body: bodyOrExpandable(json, 200, 100, 'json'),
    };
  }

  // Workload tail emits `{type: 'event', kind: 'workload_line', text: '...'}`
  // — plain stdout/stderr line, no JSONL structure. Render as a compact
  // row with no per-line label so the output looks like a terminal tail
  // rather than the rich claude-code transcript.
  function fmtWorkloadLine(rec, payload) {
    const text = (payload && typeof payload.text === 'string') ? payload.text : '';
    // Workload lines are already terminal-style one-liners — the
    // headline IS the line, no need to wrap in a disclosure. Pass an
    // empty headline + flag so renderEvent skips the <details> chrome
    // and renders the line inline (terminal-tail feel preserved).
    return {
      cls: 'log-workload-line',
      label: '',
      headline: headlinePreview(text, 200),
      body: '<pre class="log-inline-pre">' + esc(text) + '</pre>',
      // Workload lines render inline-only — the body is identical to the
      // headline (just wrapped in <pre>), so collapsing it under a
      // disclosure adds chrome for no value. renderEvent honors this flag.
      flat: true,
    };
  }

  const FORMATTERS = {
    user: fmtUser,
    user_image: fmtUserImage,
    assistant: fmtAssistantText,
    assistant_text: fmtAssistantText,
    thinking: fmtThinking,
    tool_use: fmtToolUse,
    tool_result: fmtToolResult,
    attachment: fmtAttachment,
    system: fmtSystem,
    progress: fmtProgress,
    workload_line: fmtWorkloadLine,
  };

  function renderEvent(payload) {
    if (!payload || typeof payload !== 'object') return;
    if (payload.type === 'meta') {
      const k = payload.kind || 'meta';
      let msg;
      if (k === 'stream-start') {
        // First real event from the agent — if we were polling for a
        // starting item, drop out of polling state and let the rest of
        // the modal flow take over.
        if (pollingQid) {
          pollingQid = null;
          if (pollTimer) {
            clearTimeout(pollTimer);
            pollTimer = null;
          }
          appendLine(
            '<span class="log-meta">[meta] agent started — switching to live tail</span>',
            'log-meta-line',
          );
        }
        msg = 'connected: ' + esc(payload.path || '');
        if (mode === 'archive') {
          setStatus('replaying…', 'ok');
        } else if (mode === 'workload') {
          setStatus('tailing workload', 'ok');
        } else if (mode === 'hostjob') {
          setStatus('tailing hostjob', 'ok');
        } else {
          setStatus('streaming', 'ok');
        }
      } else if (k === 'backfill-begin') {
        msg = 'backfilling ' + (payload.lines || 0) + ' recent lines…';
      } else if (k === 'backfill-end') {
        if (mode === 'workload') msg = '— live tail (workload) —';
        else if (mode === 'hostjob') msg = '— live tail (hostjob) —';
        else msg = '— live tail —';
      } else if (k === 'archive-end') {
        msg = '— end of archive (' + (payload.lines || 0) + ' lines) —';
        setStatus('archived', 'ok');
      } else if (k === 'workload-end') {
        // Shared terminal frame for both workload and hostjob tails (the
        // hostjob backend reuses the workload-end kind). Hostjob has no
        // .exit sidecar so its exit_code is always null — render a
        // job-flavored message in that case rather than a bogus
        // "exit_code=?".
        if (mode === 'hostjob') {
          msg = '— hostjob finished (queue item terminal) —';
        } else {
          const ec = (payload.exit_code === null || payload.exit_code === undefined) ? '?' : payload.exit_code;
          msg = '— workload exited (exit_code=' + esc(ec) + ') —';
        }
        setStatus('exited', 'ok');
      } else if (k === 'idle-timeout') {
        msg = 'idle timeout (' + (payload.idle_seconds || '?') + 's)';
        setStatus('idle', 'warn');
      } else if (k === 'lifetime-timeout') {
        msg = 'lifetime cap reached (' + (payload.seconds || '?') + 's). reconnect to continue.';
        setStatus('closed', 'warn');
      } else {
        msg = k;
      }
      appendLine('<span class="log-meta">[meta] ' + msg + '</span>', 'log-meta-line');
      // A meta frame breaks the transient-replace chain so subsequent
      // workload frames don't accidentally overwrite an unrelated row.
      lastTransientRow = null;
      // Workload / hostjob mode: server closes after the workload-end meta
      // (hostjob reuses the same terminal kind). Close the source
      // proactively so the browser doesn't reconnect.
      if ((mode === 'workload' || mode === 'hostjob') && k === 'workload-end' && evtSource) {
        try { evtSource.close(); } catch (_) {}
      }
      return;
    }
    if (payload.type === 'error') {
      // Polling mode: a `no-agent` / `no-jsonl` error from /stream means
      // the agent hasn't emitted its first event yet. Don't surface as
      // a hard error — close the SSE source (server already closed it
      // after the one-shot event, but be explicit) and schedule a
      // retry. Other error kinds (or non-polling rows) fall through to
      // the normal error-render path.
      const k = payload.kind || 'error';
      if (pollingQid && (k === 'no-agent' || k === 'no-jsonl')) {
        if (evtSource) {
          try { evtSource.close(); } catch (_) {}
          evtSource = null;
        }
        setStatus('waiting for agent…', 'pending');
        schedulePollRetry();
        return;
      }
      appendLine(
        '<span class="log-error">[error] ' +
          esc(k) +
          ': ' +
          esc(payload.error || '') +
          '</span>',
        'log-error-line'
      );
      setStatus('error', 'err');
      lastTransientRow = null;
      return;
    }
    if (payload.type === 'raw') {
      appendLine('<span class="log-raw">[raw] ' + esc(payload.line || '') + '</span>', 'log-raw-line');
      lastTransientRow = null;
      return;
    }
    if (payload.type !== 'event') return;
    const kind = payload.kind || 'unknown';
    const rec = payload.rec || {};
    const fmt = FORMATTERS[kind] || fmtUnknown;
    // Pass the full payload as a 2nd arg so formatters (e.g.
    // fmtWorkloadLine) can access payload-level fields like `text` that
    // don't live under `rec`. Existing formatters ignore the 2nd arg.
    const out = fmt(rec, payload);
    const ts = fmtTs(rec);
    const tsHtml = ts ? '<span class="log-ts">' + esc(ts) + '</span> ' : '';
    const labelHtml = out.label ? '<span class="log-label">' + out.label + '</span> ' : '';
    const headlineText = (out.headline !== undefined && out.headline !== null)
      ? out.headline
      : '';
    // Per-event one-line summary headline (timestamp + label chip +
    // concise content preview) — matches task-watch's per-line stream
    // shape so an operator can scan the log without expanding every
    // record. Click the summary row to disclose the full expanded body
    // (text + metadata + tool-result preview + meta disclosure). The
    // <details> element is closed by default so the modal opens as a
    // stream of one-liners; the operator opts in to detail per line.
    //
    // `flat:true` formatters (currently fmtWorkloadLine) bypass the
    // disclosure chrome — workload lines are already terminal-style
    // one-liners and don't have a separate "expanded" body worth
    // collapsing.
    let html;
    if (out.flat) {
      html =
        tsHtml +
        labelHtml +
        '<span class="log-body">' + out.body + '</span>';
    } else {
      const headlineHtml =
        tsHtml +
        labelHtml +
        '<span class="log-headline-text">' + headlineText + '</span>';
      html =
        '<details class="log-event">' +
        '<summary class="log-headline">' + headlineHtml + '</summary>' +
        '<div class="log-event-body">' + out.body + '</div>' +
        '</details>';
    }
    // Transient-replace path: workload lines whose source segment was
    // \r-terminated (rsync-style progress frames) update the previous
    // workload row in place rather than appending a new one. The flag
    // arrives on the payload as `transient: true`; we only honor it for
    // workload_line kinds (anything else gets the normal append path).
    //
    // State machine:
    //   prev=transient, new=transient  → REPLACE prior row, keep ref.
    //   prev=transient, new=permanent  → REPLACE prior row, drop ref
    //                                     (the row "graduates").
    //   prev=permanent / unset, new=transient → append + start tracking.
    //   prev=permanent / unset, new=permanent → append (current behavior).
    //
    // Any non-workload event (meta, error, raw, agent JSONL kinds) breaks
    // the chain — clears lastTransientRow so a subsequent transient
    // segment starts a fresh row instead of overwriting a wholly
    // unrelated line.
    const isWorkload = kind === 'workload_line';
    const isTransient = isWorkload && payload.transient === true;
    // Defensive: a tracked row can be evicted under us by the MAX_LINES
    // head-trim or by a wholesale streamEl.innerHTML='' reset on modal
    // re-open / mode change. Either case detaches the node from the
    // stream, at which point we MUST fall through to the append path
    // so the new segment shows up. `parentNode === streamEl` is cheap
    // and robust; `isConnected` would also work but reads less well.
    if (isWorkload && lastTransientRow && lastTransientRow.parentNode === streamEl) {
      replaceLineContent(lastTransientRow, html, out.cls);
      lastTransientRow = isTransient ? lastTransientRow : null;
      return;
    }
    const appended = appendLine(html, out.cls);
    if (isTransient) {
      lastTransientRow = appended;
    } else {
      lastTransientRow = null;
    }
  }

  function open(row) {
    triggerEl = row;
    const id = row.getAttribute('data-queue-id') || '';
    const summary = row.getAttribute('data-queue-summary') || '';
    const description = row.getAttribute('data-queue-description') || '';
    // 'live' (default) uses /stream — SSE tail of the active transcript.
    // 'archive' uses /archive — one-shot replay of the saved JSONL,
    // server closes on EOF.
    // 'workload' uses /stream — server-side dispatch tails
    //   /tmp/claude-workloads/<label>.output (line-oriented plain text)
    //   instead of an agent JSONL. Same wire envelope, simpler payloads.
    // 'hostjob' uses /stream — server-side dispatch tails
    //   <HOSTJOB_LOG_DIR>/<label>/log (line-oriented plain text), same wire
    //   envelope as 'workload' (kind workload_line + workload-end terminal),
    //   so the renderer is shared
    //   Used for BOTH running hostjob rows (live tail) AND terminal
    //   (done/abandoned) hostjob rows: the per-label log file persists
    //   on disk after the job exits, so the same /stream tail backfills
    //   the full file and ends immediately on the terminal queue status.
    //   Only the status / label strings differ from live mode.
    // 'subagent' uses /api/subagent/<subagent-id>/stream — tails a child
    //   subagent's JSONL directly (nested tree under a running card). Same
    //   wire envelope as 'live', just a different endpoint + meta source.
    mode = (row.getAttribute('data-log-mode') || 'live').toLowerCase();
    if (mode !== 'archive' && mode !== 'workload' && mode !== 'hostjob' && mode !== 'subagent') mode = 'live';
    // Subagent rows carry their own id distinct from the queue id; the
    // header + stream + meta key off it in subagent mode.
    subagentId = (mode === 'subagent')
      ? (row.getAttribute('data-subagent-id') || '')
      : null;
    if (mode === 'subagent' && !subagentId) return;
    if (mode !== 'subagent' && !id) return;

    modal.setAttribute('data-mode', mode);
    titleIdEl.textContent = (mode === 'subagent') ? subagentId : id;
    if (modeLabelEl) {
      if (mode === 'archive') modeLabelEl.textContent = 'Archived log';
      else if (mode === 'workload') modeLabelEl.textContent = 'Workload output';
      else if (mode === 'hostjob') modeLabelEl.textContent = 'Hostjob output';
      else if (mode === 'subagent') modeLabelEl.textContent = 'Subagent log';
      else modeLabelEl.textContent = 'Live log';
    }
    summaryEl.textContent = summary;
    streamEl.innerHTML = '';
    // Fresh modal — no prior workload row to replace.
    lastTransientRow = null;
    // Cancel any prior polling state (modal can be re-opened on a
    // different row without an intervening close — be defensive).
    if (pollTimer) {
      clearTimeout(pollTimer);
      pollTimer = null;
    }
    pollingQid = null;
    // Detect starting-state rows. The template stamps
    // data-queue-starting="1" on rows where the queue item is
    // registered but the owning agent hasn't emitted its first JSONL
    // event yet (and the row isn't workload-bound). For those we open
    // the modal in a polling state — the SSE /stream endpoint will
    // emit a one-shot no-agent/no-jsonl error, we'll retry every
    // POLL_INTERVAL_MS until a real stream-start lands.
    const startingFlag = (row.getAttribute('data-queue-starting') || '0') === '1';
    if (startingFlag && mode === 'live') {
      pollingQid = id;
      appendLine(
        '<span class="log-meta">[meta] <span class="spinner" aria-hidden="true"></span>waiting for agent — polling for first event every ' +
          (POLL_INTERVAL_MS / 1000) + 's…</span>',
        'log-meta-line',
      );
    }
    setStatus(pollingQid ? 'waiting for agent…' : 'connecting…', 'pending');
    // In live + workload modes auto-scroll defaults on (we want to see
    // new events as they arrive). In archive mode the file is finite —
    // start at the top and let the reader scroll naturally; auto-scroll
    // off so the initial view isn't yanked to the bottom.
    autoscroll = mode !== 'archive';
    if (autoscrollBtn) {
      autoscrollBtn.setAttribute('aria-pressed', autoscroll ? 'true' : 'false');
      // Auto-scroll toggle is meaningless once a finite archive replay
      // completes, but we keep it interactive so the reader can re-arm
      // bottom-tracking after manually scrolling around.
      autoscrollBtn.hidden = false;
    }

    // Populate the Prompt section. Hidden when there's nothing to show.
    if (promptDetailsEl && promptBodyEl) {
      if (description) {
        promptBodyEl.textContent = description;
        if (promptLabelEl) {
          promptLabelEl.textContent = 'Prompt (' + description.length + ' chars)';
        }
        promptDetailsEl.hidden = false;
        promptDetailsEl.open = false;
      } else {
        promptBodyEl.textContent = '';
        promptDetailsEl.hidden = true;
      }
    }

    // Reset the per-modal Summary header block, then fire a background
    // fetch against /api/queue/<id>/meta. The fetch is fire-and-forget
    // — applyMetaSummary() will unhide rows as data lands. Doing this
    // BEFORE the EventSource open avoids any "first event scrolls past
    // the meta block" race.
    resetMetaSummary();
    // Subagent mode has its own cheap meta endpoint (/api/subagent/<id>/meta);
    // everything else uses the queue meta. fetchMetaSummary handles the
    // dispatch internally based on `mode`.
    fetchMetaSummary(mode === 'subagent' ? subagentId : id);

    modal.hidden = false;
    document.body.classList.add('modal-open');

    connectEventSource(mode === 'subagent' ? subagentId : id);

    setTimeout(() => closeBtn && closeBtn.focus(), 0);
  }

  // Open (or re-open) the EventSource against the SSE endpoint chosen by
  // `mode`. Factored out of `open()` so the polling-retry path can
  // re-connect without re-running the modal-setup chrome (DOM reset,
  // meta fetch, focus). Idempotent: closes any existing source first.
  function connectEventSource(id) {
    if (evtSource) {
      try { evtSource.close(); } catch (_) {}
      evtSource = null;
    }
    // 'archive' hits the dedicated replay endpoint; 'live', 'workload' AND
    // 'hostjob' all hit /stream — the server dispatches on the queue item's
    // scope (workload-bound items tail the workload output file; hostjob-bound
    // items tail <HOSTJOB_LOG_DIR>/<label>/log; everything else falls through
    // to the agent JSONL tail).
    let endpoint;
    if (mode === 'archive') {
      endpoint = '/api/queue/' + encodeURIComponent(id) + '/archive';
    } else if (mode === 'subagent') {
      // Subagent mode tails the child subagent transcript directly. Same
      // SSE wire format as the agent stream, so the renderer is unchanged.
      endpoint = '/api/subagent/' + encodeURIComponent(id) + '/stream';
    } else {
      endpoint = '/api/queue/' + encodeURIComponent(id) + '/stream';
    }

    // Open the EventSource. In live mode browsers auto-reconnect if
    // the server closes; in archive mode the server always closes on
    // EOF, so we explicitly close the source on archive-end to avoid
    // a noisy reconnect attempt.
    try {
      evtSource = new EventSource(endpoint);
    } catch (e) {
      setStatus('failed', 'err');
      appendLine('<span class="log-error">[error] EventSource failed: ' + esc(e && e.message ? e.message : e) + '</span>', 'log-error-line');
      return;
    }

    evtSource.onmessage = (ev) => {
      let payload = null;
      try {
        payload = JSON.parse(ev.data);
      } catch (_) {
        payload = { type: 'raw', line: ev.data };
      }
      renderEvent(payload);
      // Archive mode: server closes after the archive-end meta. Close
      // proactively so the browser doesn't try to reconnect.
      if (mode === 'archive' && payload && payload.type === 'meta' && payload.kind === 'archive-end') {
        if (evtSource) {
          try { evtSource.close(); } catch (_) {}
        }
      }
    };

    evtSource.onerror = () => {
      if (!evtSource) return;
      // Polling mode: the server closes the SSE after emitting the
      // one-shot no-agent/no-jsonl error event. The renderEvent path
      // already scheduled a retry — onerror just swallows the close
      // without changing status (the polling status was set by the
      // error branch).
      if (pollingQid && evtSource.readyState === EventSource.CLOSED) {
        return;
      }
      if (mode === 'archive') {
        // Archive mode: the connection close on EOF is expected. Only
        // surface if we never got a stream-start (real error).
        if (evtSource.readyState === EventSource.CLOSED) {
          // If we already saw the archive-end frame, status is
          // 'archived' — leave it alone. Otherwise mark closed.
          if (statusEl && statusEl.textContent !== 'archived') {
            setStatus('closed', 'warn');
          }
        }
        return;
      }
      // Live mode: the browser auto-reconnects unless we close —
      // surface the status change but keep the stream open so a
      // transient blip doesn't drop the viewer.
      if (evtSource.readyState === EventSource.CLOSED) {
        setStatus('closed', 'warn');
      } else {
        setStatus('reconnecting…', 'warn');
      }
    };
  }

  // Schedule the next polling retry. Called from renderEvent() when a
  // no-agent / no-jsonl event lands while we're polling. Single pending
  // timer at a time; close() clears it.
  function schedulePollRetry() {
    if (!pollingQid) return;
    if (pollTimer) {
      clearTimeout(pollTimer);
    }
    pollTimer = setTimeout(() => {
      pollTimer = null;
      if (!pollingQid) return;
      if (modal.hidden) return;
      connectEventSource(pollingQid);
    }, POLL_INTERVAL_MS);
  }

  function close() {
    if (modal.hidden) return;
    if (evtSource) {
      try { evtSource.close(); } catch (_) {}
      evtSource = null;
    }
    // Cancel any pending polling retry — modal close is the universal
    // teardown.
    if (pollTimer) {
      clearTimeout(pollTimer);
      pollTimer = null;
    }
    pollingQid = null;
    // Stop the 1Hz runtime ticker (running-item RUNTIME field). Leaves
    // the row's data-started-at attribute removed so the next open
    // starts from a clean slate.
    stopRuntimeTicker();
    modal.hidden = true;
    document.body.classList.remove('modal-open');
    lastTransientRow = null;
    if (triggerEl && typeof triggerEl.focus === 'function') {
      triggerEl.focus();
    }
    triggerEl = null;
  }

  // Event delegation for clicks on running rows. We exclude any click
  // that originated inside an action button (stop) or drag handle so
  // existing handlers stay in charge. Also exclude clicks on a
  // <summary> element (or its children) — the per-card prompt-toggle
  // uses the same .log-clickable card, and clicking the disclosure
  // caret should toggle the <details>, not open the live-log modal.
  document.addEventListener('click', (ev) => {
    if (ev.target.closest('.action-btn')) return;
    if (ev.target.closest('.drag-handle')) return;
    if (ev.target.closest('#log-modal')) return;
    if (ev.target.closest('summary')) return;
    // A subagent node lives INSIDE a .log-clickable running card, so it must
    // be checked FIRST — otherwise the queue-card handler would swallow the
    // click and open the wrong (parent agent) stream. The subagent node
    // carries data-log-mode="subagent" + data-subagent-id; open() dispatches.
    const subRow = ev.target.closest('.subagent-log-clickable');
    if (subRow) {
      ev.preventDefault();
      ev.stopPropagation();
      open(subRow);
      return;
    }
    const row = ev.target.closest('.log-clickable');
    if (!row) return;
    ev.preventDefault();
    open(row);
  });

  // Vim-style keyboard nav inside the open modal.
  //
  //   Esc      — close modal (restores focus to the row that opened it).
  //   j / k    — scroll the log stream down / up by one "line"
  //              (SCROLL_STEP_PX, roughly one rendered log row).
  //   g  | gg  — jump to top of log. We accept single-g for the historical
  //              shortcut (q-f806 wired this before the chord) AND the
  //              true-vim two-key `gg` chord. Either is fine; both land at
  //              top. The chord is detected by stashing a pending-g
  //              timestamp and treating the second g within
  //              CHORD_WINDOW_MS as the trigger.
  //   G        — jump to bottom of log + re-arm auto-scroll.
  //   /        — placeholder. No log-search surface today; reserved so a
  //              future filter input can wire up without further keybind
  //              plumbing. See task notes 2026-05-13.
  //
  // Typing-target guard: any keydown originating inside an <input>,
  // <textarea>, <select>, or contenteditable element is left to bubble
  // normally — so the user can type the literal characters into the
  // action-modal reason box, the metadata search field, etc. The lone
  // exception is Esc, which always closes the topmost modal (parity with
  // the action.js modal handler — Esc out of an input is "get me out").
  //
  // Why this handler doesn't fight keyboard.js: keyboard.js's main-list
  // shortcuts (j/k row nav, /, Enter to open) early-return when
  // isAnyModalOpen() is true, so when the log modal is showing only the
  // handler below is active for j/k/g/G.
  const SCROLL_STEP_PX = 40;           // ~one log row in the rendered stream
  const CHORD_WINDOW_MS = 700;         // gg chord acceptance window
  let pendingGAt = 0;                  // monotonic ms of the last solo `g`

  function scrollStream(delta) {
    streamEl.scrollTop = streamEl.scrollTop + delta;
  }

  function isModalTypingTarget(el) {
    if (!el) return false;
    const tag = el.tagName;
    if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return true;
    if (el.isContentEditable) return true;
    return false;
  }

  // Keyboard activation on focused row (Enter / Space).
  document.addEventListener('keydown', (ev) => {
    // First handle modal-open state.
    if (!modal.hidden) {
      // Esc always closes the modal — even from inside an input — so the
      // user can bail without first tabbing out of a focused field.
      if (ev.key === 'Escape') {
        ev.preventDefault();
        close();
        return;
      }
      // Don't hijack literal-character keys when the user is typing into
      // a real input field inside the modal (e.g. action-modal reason
      // textarea — that modal sits at the same level so it can capture
      // the same keydown). The action-modal's own focus-trap + Enter
      // handler still wins.
      if (isModalTypingTarget(document.activeElement)) return;
      if (ev.ctrlKey || ev.metaKey || ev.altKey) return;

      if (ev.key === 'j') {
        ev.preventDefault();
        scrollStream(SCROLL_STEP_PX);
      } else if (ev.key === 'k') {
        ev.preventDefault();
        scrollStream(-SCROLL_STEP_PX);
      } else if (ev.key === 'g') {
        // Both single-g (historical) and gg-chord (true vim) jump to top.
        // Single-g jumps immediately; a second g inside the chord window
        // is a harmless re-jump (already at top). Either way the chord
        // state resets.
        ev.preventDefault();
        jumpToTop();
        pendingGAt = Date.now();
      } else if (ev.key === 'G') {
        ev.preventDefault();
        jumpToBottom();
        pendingGAt = 0;
      } else if (ev.key === '/') {
        // Reserved for in-modal log search. No search surface today —
        // preventDefault would only swallow Firefox's quick-find without
        // offering a replacement, so we let it through.
      }
      return;
    }
    // Activate row when focused. Subagent nodes (nested inside a
    // .log-clickable card) are checked first so keyboard activation opens
    // the subagent stream, not the parent queue stream.
    const active = document.activeElement;
    const subRow = active && active.closest && active.closest('.subagent-log-clickable');
    if (subRow) {
      if (ev.key === 'Enter' || ev.key === ' ') {
        ev.preventDefault();
        open(subRow);
      }
      return;
    }
    const row = active && active.closest && active.closest('.log-clickable');
    if (!row) return;
    if (ev.key === 'Enter' || ev.key === ' ') {
      ev.preventDefault();
      open(row);
    }
  });

  // Modal dismiss.
  modal.addEventListener('click', (ev) => {
    if (ev.target.closest('[data-modal-dismiss]')) {
      ev.preventDefault();
      close();
    }
  });

  // Auto-scroll toggle. Pressed -> on; released -> off (user wants to
  // review historical events without the stream yanking them away).
  if (autoscrollBtn) {
    autoscrollBtn.addEventListener('click', () => {
      autoscroll = !autoscroll;
      autoscrollBtn.setAttribute('aria-pressed', autoscroll ? 'true' : 'false');
      if (autoscroll) {
        streamEl.scrollTop = streamEl.scrollHeight;
      }
    });
  }

  // Jump-to-top: momentary navigation. Doesn't disable auto-scroll on its
  // own — but the on-scroll-away handler below will flip auto-scroll off
  // because we're no longer within ~40px of the bottom.
  function jumpToTop() {
    streamEl.scrollTop = 0;
  }
  // Jump-to-bottom: re-arm auto-scroll and snap to the latest line.
  function jumpToBottom() {
    streamEl.scrollTop = streamEl.scrollHeight;
    if (!autoscroll) {
      autoscroll = true;
      if (autoscrollBtn) autoscrollBtn.setAttribute('aria-pressed', 'true');
    }
  }
  if (jumpTopBtn) jumpTopBtn.addEventListener('click', jumpToTop);
  if (jumpBottomBtn) jumpBottomBtn.addEventListener('click', jumpToBottom);

  // If the user manually scrolls up, disable auto-scroll so we don't
  // fight them. Re-enable when they scroll back to within ~40px of the
  // bottom — but only in live mode. In archive mode the file is finite,
  // so silently re-arming auto-scroll just because the reader scrolled
  // to read the last line would be surprising; the explicit jump-to-
  // bottom button stays as the way to opt back in.
  streamEl.addEventListener('scroll', () => {
    if (mode === 'archive') return;
    const nearBottom =
      streamEl.scrollHeight - streamEl.scrollTop - streamEl.clientHeight < 40;
    if (!nearBottom && autoscroll) {
      autoscroll = false;
      if (autoscrollBtn) autoscrollBtn.setAttribute('aria-pressed', 'false');
    } else if (nearBottom && !autoscroll) {
      autoscroll = true;
      if (autoscrollBtn) autoscrollBtn.setAttribute('aria-pressed', 'true');
    }
  });

  // --- Test hooks (parity with refresh.js#__queueRefresh) ---------------
  // Expose the per-event formatters + the renderer plumbing so an
  // automated test can drive synthetic JSONL records through the same
  // code paths the live stream uses. NOT for app code consumption.
  window.__liveLog = {
    formatters: FORMATTERS,
    headlinePreview,
    renderEvent,
    appendLine,
    setMetaToggleInitialState,
    readMetaToggleStored,
    writeMetaToggleStored,
    // Modal keybind hooks — exposed so a jsdom test can drive the
    // scroll/jump primitives without dispatching synthetic keydowns
    // through the global listener.
    jumpToTop,
    jumpToBottom,
    scrollStream,
    SCROLL_STEP_PX,
    // Starting-state polling hooks. Tests drive the polling flow by
    // setting pollingQid via setPollingQid(), then firing a synthetic
    // error event through renderEvent() and asserting the timer state
    // / status. POLL_INTERVAL_MS is exposed read-only.
    POLL_INTERVAL_MS,
    getPollingQid: () => pollingQid,
    setPollingQid: (q) => { pollingQid = q; },
    getPollTimer: () => pollTimer,
    clearPollTimer: () => {
      if (pollTimer) { clearTimeout(pollTimer); pollTimer = null; }
    },
    // Runtime ticker hooks — exposed so a jsdom test can drive
    // applyMetaSummary() with a running-item payload, advance the
    // jsdom clock, and assert the rendered runtime string updates.
    applyMetaSummary,
    resetMetaSummary,
    fmtRuntime,
    RUNTIME_TICK_MS,
    getRuntimeTickerActive: () => runtimeTickerTimer !== null,
    getRuntimeStartedMs: () => runtimeStartedMs,
    stopRuntimeTicker,
    // Script-capture render hook — exposed so a jsdom test can drive
    // applyScriptCapture() with a capture payload and assert the
    // <details> section toggles + body renders correctly.
    applyScriptCapture,
  };
})();
