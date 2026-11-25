#!/usr/bin/env node
// Tests for the completed-CI-run localStorage cache in multitail.js.
//   1. A FINAL ci-status frame stores the run's raw lines (key by row + run).
//   2. Re-opening the pane hydrates instantly from storage and asks the
//      server only for confirmation (?have=<digest>); `ci-cached` re-sends nothing.
//   3. A digest mismatch (log-reset) replaces the hydrated text.
//   4. In-progress (non-final) output is NEVER stored.
//   5. Caps: entry count and total size evict least-recently-used.
//   6. Garbage / wrong-version entries are ignored; storage that throws is fine.
// Usage: node multitail-ci-cache.test.js

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

function boot(store, opts) {
  opts = opts || {};
  const run = opts.run || '77';
  const html = `<!doctype html><html><body>
  <header class="topbar"><div class="meta" id="topbar-meta">
    <span class="count multitail-control"><button type="button" id="multitail-toggle"
      aria-pressed="false" aria-controls="multitail">multitail</button></span></div></header>
  <main id="queue-root">
    <article class="item state-running log-clickable" data-queue-id="q-ci"
      data-queue-status="running" data-queue-summary="CI run" data-log-mode="live"
      data-live-log-mode="ci" data-ci-run="${run}"></article>
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
  if (store === 'throws') {
    Object.defineProperty(window, 'localStorage', { get() { throw new Error('denied'); } });
  } else if (store) {
    Object.defineProperty(window, 'localStorage', { value: store });
  }
  window.eval(ansiSrc);
  window.eval(src);
  window.document.getElementById('multitail-toggle').click();
  const pane = window.document.querySelector('#multitail-panes .mt-pane[data-queue-id="q-ci"]');
  return { window, pane, streams, es: () => streams.find((s) => !s.closed) };
}

function memStore() {
  const m = new Map();
  return {
    getItem: (k) => (m.has(k) ? m.get(k) : null),
    setItem: (k, v) => { m.set(k, String(v)); },
    removeItem: (k) => { m.delete(k); },
    keys: () => Array.from(m.keys()),
  };
}
const lines = (pane) => Array.from(pane.querySelector('.mt-pane-stream').children).map((c) => c.textContent);
const KEY = 'qsite_ci_log:v1:q-ci:77';
const INDEX = 'qsite_ci_log:v1:index';

function finishRun(x, texts, digest, ts) {
  const es = x.es();
  es.emit({ type: 'meta', kind: 'stream-start', mode: 'ci', label: 'q-ci' });
  for (const t of texts) es.emit({ type: 'event', kind: 'workload_line', text: t, source: 'workload' });
  es.emit({ type: 'meta', kind: 'ci-status', ts: ts || 1, lines: [], final: true, digest });
}

// 1. final frame stores ------------------------------------------------------
const store = memStore();
{
  const a = boot(store);
  const es = a.es();
  assert('first open: no ?have', es.url === '/api/queue/q-ci/stream', es.url);
  es.emit({ type: 'meta', kind: 'stream-start', mode: 'ci', label: 'q-ci', run: '77' });
  es.emit({ type: 'event', kind: 'workload_line', text: 'job A', source: 'workload' });
  es.emit({ type: 'meta', kind: 'ci-status', ts: 1000, lines: ['running x'], final: false, digest: '' });
  assert('non-final frame stores nothing', store.keys().length === 0, store.keys().join());
  es.emit({ type: 'event', kind: 'workload_line', text: 'job B', source: 'workload' });
  es.emit({ type: 'meta', kind: 'ci-status', ts: 1001, lines: [], final: true, digest: 'abc123' });
  const raw = store.getItem(KEY);
  assert('final frame stores run under row+run key', !!raw, store.keys().join());
  const v = raw && JSON.parse(raw);
  assert('stored payload is versioned, digest + raw lines',
    v && v.v === 1 && v.digest === 'abc123' && v.lines.join('|') === 'job A|job B', raw);
  assert('volatile in-progress text not stored', raw.indexOf('running x') === -1);
}

// 2. hydrate + confirm -------------------------------------------------------
{
  const b = boot(store);
  assert('pane painted before any stream frame', lines(b.pane).join('|') === 'job A|job B',
    lines(b.pane).join('|'));
  assert('footer says run complete', /run complete/.test(b.pane.querySelector('.mt-ci-status').textContent));
  const es = b.es();
  assert('stream asks with ?have=digest', /\?have=abc123$/.test(es.url), es.url);
  es.emit({ type: 'meta', kind: 'stream-start', mode: 'ci', label: 'q-ci', run: '77' });
  es.emit({ type: 'meta', kind: 'ci-cached', digest: 'abc123' });
  es.emit({ type: 'meta', kind: 'ci-status', ts: 2000, lines: [], final: true, digest: 'abc123' });
  assert('confirmed: still exactly the cached lines', lines(b.pane).join('|') === 'job A|job B',
    lines(b.pane).join('|'));
}

// 3. mismatch replaces -------------------------------------------------------
{
  const c = boot(store);
  const es = c.es();
  es.emit({ type: 'meta', kind: 'stream-start', mode: 'ci', label: 'q-ci', run: '77' });
  es.emit({ type: 'meta', kind: 'log-reset' });
  es.emit({ type: 'event', kind: 'workload_line', text: 'rerun A', source: 'workload' });
  assert('mismatch: hydrated text replaced', lines(c.pane).join('|') === 'rerun A', lines(c.pane).join('|'));
  es.emit({ type: 'meta', kind: 'ci-status', ts: 3000, lines: [], final: true, digest: 'zzz999' });
  assert('new final run overwrites the entry', JSON.parse(store.getItem(KEY)).digest === 'zzz999');
}

// 3b. a different run id on the row does not pick up the old entry -----------
{
  const d = boot(store, { run: '78' });
  assert('other run id: nothing hydrated', lines(d.pane).length === 0, lines(d.pane).join('|'));
  assert('other run id: no ?have', d.es().url === '/api/queue/q-ci/stream');
}

// 5. caps --------------------------------------------------------------------
{
  const s2 = memStore();
  for (let i = 1; i <= 15; i++) finishRun(boot(s2, { run: String(100 + i) }), ['l' + i], 'd' + i, i);
  const ks = s2.keys().filter((k) => k !== INDEX && k.indexOf('qsite_ci_log') === 0);
  assert('entry count capped at 12', ks.length === 12, String(ks.length));
  assert('oldest evicted, newest kept', !s2.getItem('qsite_ci_log:v1:q-ci:101') &&
    !!s2.getItem('qsite_ci_log:v1:q-ci:115'));

  const s3 = memStore();
  const big = 'x'.repeat(100000);
  for (let i = 1; i <= 20; i++) {
    finishRun(boot(s3, { run: String(200 + i) }), [big, big, big, big], 'e' + i, i);
  }
  let total = 0;
  for (const k of s3.keys()) if (k !== INDEX) total += s3.getItem(k).length;
  assert('total stored chars <= 1.5M cap', total <= 1500000 && total > 0, String(total));
  assert('newest big run survives', !!s3.getItem('qsite_ci_log:v1:q-ci:220'));

  const s4 = memStore();
  finishRun(boot(s4, { run: '300' }), [big, big, big, big, big, big, big, big], 'huge', 1);
  assert('oversized single run (>600k) not stored', s4.keys().length === 0, s4.keys().join());

  // LRU: loading an old entry protects it from the next eviction.
  const s6 = memStore();
  for (let i = 1; i <= 12; i++) finishRun(boot(s6, { run: String(400 + i) }), ['l'], 'f' + i, i);
  boot(s6, { run: '401' });  // touch oldest -> becomes most recent
  finishRun(boot(s6, { run: '413' }), ['l'], 'f13', 13);
  assert('touched entry survives eviction', !!s6.getItem('qsite_ci_log:v1:q-ci:401'));
  assert('untouched oldest evicted instead', !s6.getItem('qsite_ci_log:v1:q-ci:402'));
}

// 6. robustness --------------------------------------------------------------
{
  const s5 = memStore();
  s5.setItem(KEY, '{not json');
  const f = boot(s5);
  assert('garbage entry ignored + removed', lines(f.pane).length === 0 && s5.getItem(KEY) === null);
  s5.setItem(KEY, JSON.stringify({ v: 99, run: '77', digest: 'd', lines: ['old'] }));
  const g = boot(s5);
  assert('wrong-version entry ignored', lines(g.pane).length === 0);
  const h = boot('throws');
  finishRun(h, ['ok'], 'q', 1);
  assert('throwing storage: pane still renders', lines(h.pane).join('|') === 'ok');
}

if (failures) { console.error(failures + ' failure(s)'); process.exit(1); }
console.log('all multitail-ci-cache checks passed');
process.exit(0);
