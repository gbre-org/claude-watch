// Operator-presence pill — live idle stopwatch.
//
// The header "operator present" pill reflects the operator's HID-idle time,
// derived from the presence carrier file's mtime (the SAME carrier the Rust
// daemon reads for its `claude_operator_present*` gauges — see
// src/metrics.rs). app.py stamps the pill with `data-idle-base` = the server-
// computed idle seconds at render time; this module ticks it forward once a
// second so the stopwatch advances smoothly WITHOUT hammering the backend.
//
// Behaviour (mirrors app.py `_presence_state` / `_format_idle_stopwatch`):
//   - idle  < threshold (default 10s): state "present" — plain "operator
//     present", no stopwatch.
//   - idle >= threshold, within max_age: state "idle" — a ticking stopwatch.
//   - idle  > max_age (carrier stale): state "away" — the SAME stopwatch,
//     still ticking. It is NEVER reset or hidden across the present->away
//     flip; only the colour changes.
//
// Re-seeding across the 5s /api/queue merge: refresh.js rebuilds #topbar-meta
// every tick via buildTopbarMetaDOM, which re-emits this pill (through
// buildPresencePillHTML below) with a FRESH `data-idle-base` from the server.
// morphdom patches the live element's attributes in place, dropping the
// JS-managed `data-anchor-*` attrs; the next tick sees the changed base and
// re-anchors, so the stopwatch stays synced to the server's clock every 5s
// while ticking locally in between. This makes the client immune to
// client<->server clock skew (only server-relative deltas are ever summed).
//
// Exposed on window as `Presence` (plain <script>, no module loader) so
// refresh.js can reuse the formatter + pill builder.

(function () {
  'use strict';

  var PILL_ID = 'operator-presence';
  var DEFAULT_THRESHOLD = 10; // seconds; app.py OPERATOR_IDLE_STOPWATCH_THRESHOLD
  var DEFAULT_MAX_AGE = 420; // seconds; app.py CW_PRESENCE_MAX_AGE

  // VERBATIM port of app.py `_format_idle_stopwatch`: "M:SS" under an hour,
  // "H:MM:SS" at or above one hour. Zero-padded seconds/minutes so the digits
  // don't jump width as they tick.
  function formatIdleStopwatch(seconds) {
    var secs = Math.floor(seconds);
    if (!isFinite(secs) || secs < 0) {
      secs = 0;
    }
    function pad(n) {
      return n < 10 ? '0' + n : '' + n;
    }
    if (secs < 3600) {
      var m = Math.floor(secs / 60);
      var s = secs % 60;
      return m + ':' + pad(s);
    }
    var h = Math.floor(secs / 3600);
    var rem = secs % 3600;
    return h + ':' + pad(Math.floor(rem / 60)) + ':' + pad(rem % 60);
  }

  // VERBATIM port of app.py `_presence_state`.
  function presenceState(idle, threshold, maxAge) {
    if (idle < threshold) {
      return 'present';
    }
    return idle <= maxAge ? 'idle' : 'away';
  }

  // Escape for attribute/text interpolation (mirrors refresh.js `esc`).
  function esc(s) {
    return String(s == null ? '' : s)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');
  }

  function labelFor(state) {
    if (state === 'present') {
      return 'operator present';
    }
    return state === 'away' ? 'away' : 'idle';
  }

  // Build the pill's OUTER markup from the server presence object (the
  // `operator_presence` payload). Returns '' when presence is null/undefined
  // (no carrier wired in) so callers can concatenate unconditionally. Kept in
  // step with the Jinja block in templates/index.html.
  function buildPresencePillHTML(p) {
    if (!p) {
      return '';
    }
    var threshold = isFinite(p.threshold) ? p.threshold : DEFAULT_THRESHOLD;
    var maxAge = isFinite(p.max_age) ? p.max_age : DEFAULT_MAX_AGE;
    var idle = isFinite(p.idle_seconds) ? p.idle_seconds : 0;
    var state = presenceState(idle, threshold, maxAge);
    var swHidden = state === 'present' ? ' hidden' : '';
    return (
      '<span class="operator-presence state-' + esc(state) + '" ' +
      'id="' + PILL_ID + '" ' +
      'data-idle-base="' + esc(idle.toFixed(3)) + '" ' +
      'data-threshold="' + esc(Math.round(threshold)) + '" ' +
      'data-max-age="' + esc(Math.round(maxAge)) + '" ' +
      'title="Operator HID-idle time (from the presence carrier the daemon ' +
      'reads for claude_operator_present). Under ' + esc(Math.round(threshold)) +
      's: present. Above: a live idle stopwatch that keeps running across the ' +
      'present-to-away transition.">' +
      '<span class="presence-dot" aria-hidden="true"></span>' +
      '<span class="presence-label">' + esc(labelFor(state)) + '</span>' +
      '<span class="presence-stopwatch"' + swHidden + '>' +
      esc(formatIdleStopwatch(idle)) + '</span>' +
      '</span>'
    );
  }

  // Apply the computed idle to a live pill element: state class, label text,
  // and the stopwatch's text + visibility.
  function applyPill(el, idle) {
    var threshold = parseFloat(el.getAttribute('data-threshold'));
    var maxAge = parseFloat(el.getAttribute('data-max-age'));
    if (!isFinite(threshold)) {
      threshold = DEFAULT_THRESHOLD;
    }
    if (!isFinite(maxAge)) {
      maxAge = DEFAULT_MAX_AGE;
    }
    var state = presenceState(idle, threshold, maxAge);
    el.className = 'operator-presence state-' + state;
    var label = el.querySelector('.presence-label');
    if (label) {
      label.textContent = labelFor(state);
    }
    var sw = el.querySelector('.presence-stopwatch');
    if (sw) {
      if (state === 'present') {
        sw.hidden = true;
      } else {
        sw.hidden = false;
        sw.textContent = formatIdleStopwatch(idle);
      }
    }
  }

  // One tick: locate the pill, (re-)anchor to the freshest server value, then
  // render idle = anchorBase + (wall-clock elapsed since anchor).
  function tick(root) {
    var scope = root || document;
    var el = scope.getElementById
      ? scope.getElementById(PILL_ID)
      : document.getElementById(PILL_ID);
    if (!el) {
      return;
    }
    var serverBase = parseFloat(el.getAttribute('data-idle-base'));
    if (!isFinite(serverBase) || serverBase < 0) {
      return;
    }
    // Re-anchor whenever the server base changed (a fresh /api/queue merge, or
    // first sight of the element). `data-anchor-*` are JS-managed and get
    // stripped by the morphdom merge, so a merge naturally forces a re-anchor.
    var anchoredBase = el.getAttribute('data-anchor-base');
    if (anchoredBase === null || parseFloat(anchoredBase) !== serverBase) {
      el.setAttribute('data-anchor-base', String(serverBase));
      el.setAttribute('data-anchor-ms', String(Date.now()));
    }
    var anchorBase = parseFloat(el.getAttribute('data-anchor-base'));
    var anchorMs = parseFloat(el.getAttribute('data-anchor-ms'));
    var idle = anchorBase + (Date.now() - anchorMs) / 1000;
    if (!isFinite(idle) || idle < 0) {
      idle = 0;
    }
    applyPill(el, idle);
  }

  window.Presence = {
    formatIdleStopwatch: formatIdleStopwatch,
    presenceState: presenceState,
    buildPresencePillHTML: buildPresencePillHTML,
    applyPill: applyPill,
    tick: tick,
  };

  // Correct any drift between server render and paint on load, then tick every
  // second so the stopwatch advances visibly. Safe to also call after a
  // refresh.js merge introduces a fresh pill (idempotent).
  function start() {
    tick();
    setInterval(tick, 1000);
  }
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', start);
  } else {
    start();
  }
})();
