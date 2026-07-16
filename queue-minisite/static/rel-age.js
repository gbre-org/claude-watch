// Live-ticking relative-age tokens ("relative timestamps dynamically
// rendered on clock ticks, via UI by ui").
//
// Every relative-age token on the page ("running 49m ago", "created 1h 43m
// ago", "blocked 10s ago", the owner-agent transcript age, subagent-tree
// ages) is rendered ONCE server-side (or by refresh.js on each 5s /api/queue
// merge) and then sits FROZEN between renders. This module makes those tokens
// TICK on their own: each is an inner
//   <span class="rel-age" data-rel-epoch="<unix seconds>">TEXT</span>
// and a 1s interval recomputes the human string client-side from
// (now - data-rel-epoch) and rewrites the span's text in place — so a
// "10s ago" becomes "11s ago", "12s ago"… WITHOUT a page reload or a server
// round-trip. Pure UI concern.
//
// Degrades gracefully: with JS disabled the server-rendered initial text
// stays put (the span is just a plain wrapper). Spans WITHOUT a valid
// data-rel-epoch (unknown "?" ages) are left untouched.
//
// formatRelativeAge() is a VERBATIM port of app.py's `_humanize_age` so the
// client and server render byte-identical strings (a morphdom merge on an
// unchanged item stays a no-op).
//
// Exposed on window as `RelAge` (plain <script>, no module loader).

(function () {
  'use strict';

  // VERBATIM port of app.py _humanize_age (value + "ago"/"from now" suffix).
  // Takes an absolute unix-seconds epoch and returns the humanized age vs now.
  function formatRelativeAge(epochSeconds) {
    var secs = Math.floor(Date.now() / 1000 - epochSeconds);
    var suffix = 'ago';
    if (secs < 0) {
      secs = Math.abs(secs);
      suffix = 'from now';
    }
    if (secs < 60) {
      return secs + 's ' + suffix;
    }
    if (secs < 3600) {
      return Math.floor(secs / 60) + 'm ' + suffix;
    }
    if (secs < 86400) {
      var h = Math.floor(secs / 3600);
      var m = Math.floor((secs % 3600) / 60);
      return h + 'h ' + m + 'm ' + suffix;
    }
    var d = Math.floor(secs / 86400);
    var rh = Math.floor((secs % 86400) / 3600);
    return d + 'd ' + rh + 'h ' + suffix;
  }

  // Walk the DOM and re-render every [data-rel-epoch] token. Idempotent and
  // cheap — a handful of nodes, a text rewrite each. Safe to call after a
  // refresh.js merge introduces fresh cards.
  function tick(root) {
    var scope = root || document;
    var els = scope.querySelectorAll('.rel-age[data-rel-epoch]');
    for (var i = 0; i < els.length; i++) {
      var epoch = parseFloat(els[i].getAttribute('data-rel-epoch'));
      if (!isFinite(epoch) || epoch <= 0) {
        continue;
      }
      els[i].textContent = formatRelativeAge(epoch);
    }
  }

  window.RelAge = {
    formatRelativeAge: formatRelativeAge,
    tick: tick,
  };

  // Run once on load to correct any drift between server render and paint,
  // then every second so second-granularity ages advance visibly.
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
