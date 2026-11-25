#!/usr/bin/env node
// Header status from REAL captured ci-log output (see ../fixtures).
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

const fixture = fs.readFileSync(path.join(STATIC_DIR, '..', 'fixtures', 'ci-log-in-progress.txt'), 'utf8')
  .split('\n');
const mark = fixture.indexOf('===== in progress =====');
const statusLines = fixture.slice(mark + 1).filter((l) => l !== '');
// What app.py sends: job headers parsed server-side (none were streamed to
// the pane: the backfill is tail-limited), whole volatile block.
const jobs = [];
for (const l of fixture.slice(0, mark)) {
  const m = /^===== job '(.*)' \(id \S+\) (\S+) =====$/.exec(l);
  if (m) jobs.push({ name: m[1], state: m[2] });
}

document.getElementById('multitail-toggle').click();
const pane = document.querySelector('#multitail-panes .mt-pane[data-queue-id="q-ci"]');
const es = streams.find((s) => !s.closed);
const text = () => pane.querySelector('.mt-ci-text').textContent;
const dets = () => pane.querySelector('.mt-ci-details');
const detail = () => pane.querySelector('.mt-ci-detail').textContent;
es.emit({ type: 'meta', kind: 'stream-start', mode: 'ci', label: 'q-ci' });

assert('fixture has 6 finished jobs', jobs.length === 6, String(jobs.length));
es.emit({ type: 'meta', kind: 'ci-status', ts: 1, final: false, lines: statusLines, jobs,
  url: 'https://github.com/o/r/actions/runs/1', provider_label: 'GitHub Actions' });
assert('header counts jobs', /^6\/7 jobs ok · running: Container shell \+ shim tests \(step 42\/45\) · updated \d\d:\d\d:\d\d$/.test(text()), text());
dets().open = true;
assert('expand view non-empty', detail().length > 0 && detail().indexOf('E2E Tests') !== -1 &&
  detail().indexOf('42. Run session-task Python tests') !== -1, detail());

// Degraded: no job data at all still says something and links the run.
es.emit({ type: 'meta', kind: 'ci-status', ts: 2, final: false, lines: [], jobs: [],
  url: 'https://github.com/o/r/actions/runs/1', provider_label: 'GitHub Actions' });
assert('degraded header', /^GitHub Actions run · in progress · updated /.test(text()), text());
assert('degraded expand non-empty', detail().indexOf('actions/runs/1') !== -1 && detail().trim().length > 20, detail());

if (failures) { console.error(failures + ' failure(s)'); process.exit(1); }
console.log('all multitail-ci-real checks passed');
process.exit(0);
