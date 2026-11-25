#!/usr/bin/env node
// Tests for the CI-mode multitail pane (live CI row).
//
//   1. `ci-status` frames paint a footer BELOW the scrolling stream with the
//      volatile in-progress lines and a "last updated HH:MM:SS" stamp, and the
//      stamp advances on every frame (a quiet pane is visibly polling).
//   2. A `final` ci-status frame switches the footer to "run complete".
//   3. `log-reset` clears the stream (previously ignored, which re-appended the
//      whole log and left the 400-line window showing a stale tail).
//   4. Appended lines (newly finished jobs) land at the bottom of the stream.
//
// Usage: node multitail-ci.test.js   (exit 0 on success)

'use strict';

const path = require('path');
const fs = require('fs');

const NODE_MODULES = process.env.QM_NODE_MODULES ||
  '/tmp/queue-minisite-test/node_modules';
const { JSDOM } = require(path.join(NODE_MODULES, 'jsdom'));
const STATIC_DIR = path.dirname(path.resolve(__filename));
const src = fs.readFileSync(path.join(STATIC_DIR, 'multitail.js'), 'utf8');
const ansiSrc = fs.readFileSync(path.join(STATIC_DIR, 'ansi.js'), 'utf8');

const html = `<!doctype html><html><body>
  <header class="topbar"><div class="meta" id="topbar-meta">
    <span class="count multitail-control"><button type="button" id="multitail-toggle"
      aria-pressed="false" aria-controls="multitail">multitail</button></span></div></header>
  <main id="queue-root">
    <article class="item state-running log-clickable" data-queue-id="q-ci"
      data-queue-status="running" data-queue-summary="CI run" data-log-mode="live"
      data-live-log-mode="ci"></article>
  </main>
  <section id="multitail" class="multitail" data-no-morph hidden>
    <header class="multitail-head"><h2 id="multitail-title">multitail</h2>
      <span id="multitail-count"></span><button type="button" id="multitail-exit">exit</button></header>
    <div id="multitail-panes"></div><p id="multitail-empty" hidden>Nothing</p>
  </section></body></html>`;

const dom = new JSDOM(html, { runScripts: 'outside-only' });
const { window } = dom;
const { document } = window;
const streams = [];
window.EventSource = class {
  constructor(url) { this.url = url; this.closed = false; streams.push(this); }
  close() { this.closed = true; }
  emit(o) { if (this.onmessage) this.onmessage({ data: JSON.stringify(o) }); }
};
window.eval(ansiSrc);
window.eval(src);

let failures = 0;
function assert(label, cond, detail) {
  if (cond) console.log('  ok  ' + label);
  else { failures += 1; console.error('  FAIL ' + label + (detail ? '\n       ' + detail : '')); }
}

document.getElementById('multitail-toggle').click();
const pane = document.querySelector('#multitail-panes .mt-pane[data-queue-id="q-ci"]');
assert('ci pane built', !!pane);
const es = streams.find((s) => !s.closed);
assert('stream opened', !!es);
const stream = () => pane.querySelector('.mt-pane-stream');
const footer = () => pane.querySelector('.mt-ci-status');
const lines = () => Array.from(stream().children).map((c) => c.textContent);

const sum = () => pane.querySelector('.mt-ci-text');
const dets = () => pane.querySelector('.mt-ci-details');
const T0 = Date.UTC(2026, 0, 1, 12, 0, 0) / 1000;

assert('no footer element at all', !pane.querySelector('.mt-ci-status'));
assert('header status line starts hidden', dets() && dets().hidden);
assert('status line lives in the header', dets().parentNode === pane.querySelector('.mt-pane-head'));
es.emit({ type: 'meta', kind: 'stream-start', mode: 'ci', label: 'q-ci' });

// running / partial: 1 finished job in the stream, E2E running at step 2/3, 1 queued
es.emit({ type: 'event', kind: 'workload_line', text: "===== job 'unit' (id 1) success =====", source: 'workload' });
es.emit({ type: 'event', kind: 'workload_line', text: 'job A log', source: 'workload' });
es.emit({ type: 'meta', kind: 'ci-status', ts: T0 + 5, final: false, lines: [
  "job 'E2E' in_progress", '  started 2026-01-01', '  [  ok] 1. Checkout (success)',
  '  [  >>] 2. Run suites (in_progress) since x', '  [pending] 3. Post (pending)',
  "job 'deploy' queued"] });
const run1 = sum().textContent;
assert('running header', /^1\/3 jobs ok · running: E2E \(step 2\/3\) · 1 pending · updated \d\d:\d\d:\d\d$/.test(run1), run1);
assert('no badge while running', pane.querySelector('.mt-ci-badge').hidden);
assert('finished-job line in stream', lines().some((t) => t.indexOf('job A log') !== -1), lines().join('|'));
assert('detail collapsed by default', !dets().open);
dets().open = true;
const dt = pane.querySelector('.mt-ci-detail').textContent;
assert('expanded detail lists jobs and steps',
  dt.indexOf('unit') !== -1 && dt.indexOf('2. Run suites') !== -1 && dt.indexOf("job 'deploy'") !== -1, dt);
dets().open = false;
assert('collapse works', !dets().open);
assert('header status has no clock', /^live$/.test(pane.querySelector('.mt-pane-status').textContent));

es.emit({ type: 'event', kind: 'workload_line', text: 'job B log', source: 'workload' });
es.emit({ type: 'meta', kind: 'ci-status', ts: T0 + 10, final: false, lines: ["job 'E2E' in_progress"] });
const l = lines();
assert('new job log appended at the bottom', l[l.length - 1].indexOf('job B log') !== -1, l.join('|'));
assert('stamp advances on each poll', sum().textContent.match(/updated (\S+)/)[1] !== run1.match(/updated (\S+)/)[1]);
assert('exactly one clock in the header line', (sum().textContent.match(/\d\d:\d\d:\d\d/g) || []).length === 1);

es.emit({ type: 'meta', kind: 'log-reset' });
assert('log-reset clears the stream', lines().length === 0, lines().join('|'));
es.emit({ type: 'event', kind: 'workload_line', text: 'fresh', source: 'workload' });
assert('lines after reset render', lines().length === 1 && lines()[0].indexOf('fresh') !== -1);

// failing job while others still run: named in the header
es.emit({ type: 'event', kind: 'workload_line', text: "===== job 'lint' (id 2) failure =====", source: 'workload' });
es.emit({ type: 'meta', kind: 'ci-status', ts: T0 + 20, final: false, lines: ["job 'E2E' in_progress"] });
assert('failed job named while running', /failed: lint/.test(sum().textContent), sum().textContent);

// final success
es.emit({ type: 'meta', kind: 'log-reset' });
es.emit({ type: 'event', kind: 'workload_line', text: "===== job 'a' (id 1) success =====", source: 'workload' });
es.emit({ type: 'event', kind: 'workload_line', text: "===== job 'b' (id 2) success =====", source: 'workload' });
es.emit({ type: 'meta', kind: 'ci-status', ts: T0 + 60, lines: [], final: true });
const badge = pane.querySelector('.mt-ci-badge');
assert('final success: green passed badge', !badge.hidden && badge.textContent === 'passed' &&
  badge.classList.contains('mt-ci-good'), badge.className);
assert('final success text', /^2\/2 jobs ok · updated \d\d:\d\d:\d\d$/.test(sum().textContent), sum().textContent);
assert('header status complete', pane.querySelector('.mt-pane-status').textContent === 'complete');

// final failure
es.emit({ type: 'meta', kind: 'log-reset' });
es.emit({ type: 'event', kind: 'workload_line', text: "===== job 'a' (id 1) success =====", source: 'workload' });
es.emit({ type: 'event', kind: 'workload_line', text: "===== job 'E2E' (id 2) failure =====", source: 'workload' });
es.emit({ type: 'meta', kind: 'ci-status', ts: T0 + 70, lines: [], final: true });
assert('final failure: red failed badge', badge.textContent === 'failed' && badge.classList.contains('mt-ci-bad'), badge.className);
assert('final failure names job', /^1\/2 jobs ok · failed: E2E · updated /.test(sum().textContent), sum().textContent);

if (failures) { console.error(failures + ' failure(s)'); process.exit(1); }
console.log('all multitail-ci checks passed');
process.exit(0);  // the module's refresh timers would otherwise keep node alive
