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

assert('footer starts hidden', footer() && footer().hidden);
es.emit({ type: 'meta', kind: 'stream-start', mode: 'ci', label: 'q-ci' });
es.emit({ type: 'event', kind: 'workload_line', text: 'job A log', source: 'workload' });
es.emit({ type: 'meta', kind: 'ci-status', ts: Date.UTC(2026, 0, 1, 12, 0, 0) / 1000 + 5,
  lines: ["job 'b' in_progress", '  [  >>] 2. Test'], final: false });
assert('finished-job line in stream', lines().some((t) => t.indexOf('job A log') !== -1), lines().join('|'));
assert('footer visible with in-progress lines',
  !footer().hidden && footer().textContent.indexOf('2. Test') !== -1, footer().textContent);
assert('footer has last-updated stamp',
  /last updated \d\d:\d\d:\d\d/.test(footer().textContent), footer().textContent);
assert('footer is outside the scrolling stream', footer().parentNode === pane && !stream().contains(footer()));
const first = footer().textContent.match(/last updated (\S+)/)[1];

es.emit({ type: 'event', kind: 'workload_line', text: 'job B log', source: 'workload' });
es.emit({ type: 'meta', kind: 'ci-status', ts: Date.UTC(2026, 0, 1, 12, 0, 0) / 1000 + 10,
  lines: [], final: false });
const l = lines();
assert('new job log appended at the bottom', l[l.length - 1].indexOf('job B log') !== -1, l.join('|'));
const second = footer().textContent.match(/last updated (\S+)/)[1];
assert('stamp advances on each poll', first !== second, first + ' vs ' + second);
assert('header status has no clock (the footer carries the one stamp)',
  /^live$/.test(pane.querySelector('.mt-pane-status').textContent),
pane.querySelector('.mt-pane-status').textContent);

es.emit({ type: 'meta', kind: 'log-reset' });
assert('log-reset clears the stream', lines().length === 0, lines().join('|'));
es.emit({ type: 'event', kind: 'workload_line', text: 'fresh', source: 'workload' });
assert('lines after reset render', lines().length === 1 && lines()[0].indexOf('fresh') !== -1);

es.emit({ type: 'meta', kind: 'ci-status', ts: Date.UTC(2026, 0, 1, 12, 1, 0) / 1000,
  lines: [], final: true });
assert('final frame says run complete', footer().textContent.indexOf('run complete') !== -1,
  footer().textContent);

if (failures) { console.error(failures + ' failure(s)'); process.exit(1); }
console.log('all multitail-ci checks passed');
process.exit(0);  // the module's refresh timers would otherwise keep node alive
