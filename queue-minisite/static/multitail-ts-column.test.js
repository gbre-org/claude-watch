#!/usr/bin/env node
// Exactly ONE timestamp per line, in the right-hand column:
//   * a workload runner prefix (`2026-11-25T13:13:44-05:00 `) and a CI
//     prefix (`...T17:41:36.4583039Z `) are parsed off the text and shown once
//   * stacked prefixes collapse to ONE column (outermost wins), none left in the body
//   * unprefixed lines (continuations) show NO timestamp, never an arrival time
//   * a cached CI run re-hydrated from localStorage keeps the same single column
//   * the CI header status carries no clock of its own (footer has it)
// Usage: node multitail-ts-column.test.js

'use strict';

const path = require('path');
const fs = require('fs');

const NODE_MODULES = process.env.QM_NODE_MODULES || '/tmp/queue-minisite-test/node_modules';
const { JSDOM } = require(path.join(NODE_MODULES, 'jsdom'));
const STATIC_DIR = path.dirname(path.resolve(__filename));
const src = fs.readFileSync(path.join(STATIC_DIR, 'multitail.js'), 'utf8');
const ansiSrc = fs.readFileSync(path.join(STATIC_DIR, 'ansi.js'), 'utf8');

let failures = 0;
function assert(label, cond, detail) {
  if (cond) console.log('  ok  ' + label);
  else { failures += 1; console.error('  FAIL ' + label + (detail ? '\n       ' + detail : '')); }
}

function memStore(init) {
  const m = new Map(Object.entries(init || {}));
  return {
    getItem: (k) => (m.has(k) ? m.get(k) : null),
    setItem: (k, v) => { m.set(k, String(v)); },
    removeItem: (k) => { m.delete(k); },
  };
}

function boot(store) {
  const html = `<!doctype html><html><body>
  <header class="topbar"><div class="meta" id="topbar-meta">
    <span class="count multitail-control"><button type="button" id="multitail-toggle"
      aria-pressed="false" aria-controls="multitail">multitail</button></span></div></header>
  <main id="queue-root">
    <article class="item state-running log-clickable" data-queue-id="q-ci"
      data-queue-status="running" data-queue-summary="CI run" data-log-mode="live"
      data-live-log-mode="ci" data-ci-run="9"></article>
  </main>
  <section id="multitail" class="multitail" data-no-morph hidden>
    <header class="multitail-head"><h2 id="multitail-title">multitail</h2>
      <span id="multitail-count"></span><button type="button" id="multitail-exit">exit</button></header>
    <div id="multitail-panes"></div><p id="multitail-empty" hidden>Nothing</p>
  </section></body></html>`;
  const dom = new JSDOM(html, { runScripts: 'outside-only' });
  const { window } = dom;
  const streams = [];
  window.EventSource = class {
    constructor(url) { this.url = url; this.closed = false; streams.push(this); }
    close() { this.closed = true; }
    emit(o) { if (this.onmessage) this.onmessage({ data: JSON.stringify(o) }); }
  };
  Object.defineProperty(window, 'localStorage', { value: store });
  window.eval(ansiSrc);
  window.eval(src);
  window.document.getElementById('multitail-toggle').click();
  const pane = window.document.querySelector('#multitail-panes .mt-pane[data-queue-id="q-ci"]');
  return { window, pane, es: () => streams.find((s) => !s.closed) };
}

// Each row -> { ts: [cell texts], body }
const rows = (pane) => Array.from(pane.querySelector('.mt-pane-stream').children).map((r) => ({
  ts: Array.from(r.querySelectorAll('.mt-ts')).map((e) => e.textContent),
  body: r.querySelector('.mt-body').textContent,
}));
const HHMMSS = /\d\d:\d\d:\d\d/g;

const store = memStore({ qsite_mt_ts: '1' });
const x = boot(store);
const es = x.es();
es.emit({ type: 'meta', kind: 'stream-start', mode: 'ci', label: 'q-ci', run: '9' });
// workload-prefixed (server already split it into source_ts)
es.emit({ type: 'event', kind: 'workload_line', source: 'workload',
  source_ts: '2026-11-25T13:13:44-05:00', text: 'make deploy' });
// CI Z-prefixed, NOT split by the server (older server / client path)
es.emit({ type: 'event', kind: 'workload_line', source: 'workload',
  text: '2026-11-25T17:41:36.4583039Z ##[group]Run tests' });
// stacked: workload stamp in front of a CI stamp
es.emit({ type: 'event', kind: 'workload_line', source: 'workload',
  text: '2026-11-25T13:13:45-05:00 2026-11-25T18:13:45.1Z step ok' });
// unprefixed continuation / wrapped line
es.emit({ type: 'event', kind: 'workload_line', source: 'workload',
  text: '    continuation of the previous line' });
// a date that is NOT a stamp stays prose
es.emit({ type: 'event', kind: 'workload_line', source: 'workload',
  text: '2026-05-01 promoted 3 shows' });
es.emit({ type: 'meta', kind: 'ci-status', ts: 1000, lines: [], final: true, digest: 'dd' });

const r = rows(x.pane);
assert('workload-prefixed: one column, body has no stamp',
  r[0].ts.length === 1 && r[0].ts[0] === '13:13:44' && r[0].body === 'make deploy', JSON.stringify(r[0]));
assert('CI Z-prefixed (7-digit fraction): one column, stamp stripped from body',
  r[1].ts.length === 1 && r[1].ts[0] === '17:41:36' && r[1].body === '##[group]Run tests', JSON.stringify(r[1]));
assert('stacked stamps: ONE column (outermost), none left in body',
  r[2].ts.length === 1 && r[2].ts[0] === '13:13:45' && r[2].body === 'step ok', JSON.stringify(r[2]));
assert('unprefixed continuation: no timestamp at all (not arrival time)',
  r[3].ts.length === 0 && !HHMMSS.test(r[3].body), JSON.stringify(r[3]));
assert('bare date prose untouched, no column',
  r[4].ts.length === 0 && r[4].body === '2026-05-01 promoted 3 shows', JSON.stringify(r[4]));
const total = r.reduce((n, row) => n + row.ts.length, 0);
assert('never more than one timestamp cell per row', r.every((row) => row.ts.length <= 1));
assert('three stamped rows -> three cells', total === 3, String(total));

const header = x.pane.querySelector('.mt-pane-status');
assert('CI header status carries no clock', header && !/\d\d:\d\d:\d\d/.test(header.textContent),
  header && header.textContent);
assert('header status line carries the one updated stamp',
  (x.pane.querySelector('.mt-ci-text').textContent.match(HHMMSS) || []).length === 1);

// Cached run re-hydrated from localStorage keeps ONE column and a clean body.
const y = boot(store);
const rr = rows(y.pane);
assert('hydrated from cache: same single column',
  rr.length === 5 && rr[0].ts[0] === '13:13:44' && rr[0].body === 'make deploy' &&
  rr[1].ts[0] === '17:41:36' && rr[3].ts.length === 0, JSON.stringify(rr));

// TTY progress redraw: exact slice of a real `docker compose` deploy log (the
// `=== containers ===` section, 2026-11-25T13:13:44-05:00). Every spinner
// frame is a stamped line starting with ESC[2K, followed by a stamped line of
// pure cursor movement (ESC[?25h ESC[?25l ESC[3A ESC[0G) and a blank line.
{
  const ESC = '\u001b';
  const spin = (g, t, name) => '2026-11-25T13:13:44-05:00 ' + ESC + '[2K ' + ESC + '[33m' + g +
    ESC + '[0m Container ' + name + '             Recreate                    ' + ESC + '[34m' + t + ESC + '[0m';
  const ctl = '2026-11-25T13:13:44-05:00 ' + ESC + '[?25h' + ESC + '[?25l' + ESC + '[3A' + ESC + '[0G';
  const w = boot(memStore({ qsite_mt_ts: '1' }));
  const e = w.es();
  e.emit({ type: 'meta', kind: 'stream-start', mode: 'ci', label: 'q-ci', run: '9' });
  const glyphs = ['\u2826', '\u2827', '\u2807', '\u280f', '\u280b', '\u2819'];
  glyphs.forEach((g, i) => {
    // server-side shape: stamp already split into source_ts
    for (const t of [spin(g, '6.' + (6 + i) + 's', 'queue-minisite'), ctl, '']) {
      const m = /^(\S+) (.*)$/.exec(t);
      e.emit({ type: 'event', kind: 'workload_line', source: 'workload',
        source_ts: m ? m[1] : undefined, text: m ? m[2] : t });
    }
    // a second, different container line in the same block
    const t2 = spin(g, '1.' + i + 's', 'other-svc');
    e.emit({ type: 'event', kind: 'workload_line', source: 'workload',
      source_ts: '2026-11-25T13:13:44-05:00', text: t2.replace(/^\S+ /, '') });
  });
  const pr = rows(w.pane);
  assert('redraw frames collapse: one row per logical line (2), not per frame',
    pr.length === 2, String(pr.length) + ' ' + JSON.stringify(pr.map((q) => q.body)));
  assert('no stamp-only / empty rows', pr.every((q) => q.body.trim().length > 0));
  assert('exactly one timestamp cell per row', pr.every((q) => q.ts.length === 1 && q.ts[0] === '13:13:44'));
  assert('latest frame wins', /queue-minisite/.test(pr[0].body) && /other-svc/.test(pr[1].body));
  assert('no escape garbage in bodies', pr.every((q) => q.body.indexOf('\u001b') === -1 && !/\[3A|\[0G/.test(q.body)));
}

if (failures) { console.error(failures + ' failure(s)'); process.exit(1); }
console.log('all multitail-ts-column checks passed');
process.exit(0);
