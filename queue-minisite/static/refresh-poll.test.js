#!/usr/bin/env node
// Polling-discipline test for refresh.js.
//
// A dashboard tab is left open for hours, so its poll loop must be polite:
//   1. one request in flight at a time (no overlap even if a poll is slow);
//   2. the next poll is scheduled only AFTER the previous one settles, at
//      REFRESH_MS (5s) -- never faster;
//   3. nothing is scheduled while the tab is hidden; becoming visible polls
//      immediately;
//   4. the ETag of a 200 rides If-None-Match on the next poll, and a 304
//      skips the merge;
//   5. errors (network or 5xx) back off exponentially, capped, and reset on
//      the next success.
//
// Timers and fetch are faked, so the test is deterministic and instant.
//
// Usage:   QM_NODE_MODULES=<dir>/node_modules node refresh-poll.test.js

'use strict';

const path = require('path');
const fs = require('fs');

const NODE_MODULES = process.env.QM_NODE_MODULES ||
  '/tmp/queue-minisite-test/node_modules';
const { JSDOM } = require(path.join(NODE_MODULES, 'jsdom'));

const STATIC_DIR = path.dirname(path.resolve(__filename));
const morphdomSrc = fs.readFileSync(
  path.join(STATIC_DIR, 'vendor', 'morphdom-2.7.4.min.js'), 'utf8');
const refreshSrc = fs.readFileSync(path.join(STATIC_DIR, 'refresh.js'), 'utf8');

const html = `<!doctype html><html><head></head><body>
  <div class="meta" id="topbar-meta"></div>
  <main id="queue-root"></main>
</body></html>`;

const dom = new JSDOM(html, { runScripts: 'dangerously', url: 'https://queue.test/' });
const win = dom.window;

// --- fakes -------------------------------------------------------------
let timers = [];   // {id, ms, fn}
let nextTimerId = 1;
win.setTimeout = (fn, ms) => { const id = nextTimerId++; timers.push({ id, ms, fn }); return id; };
win.clearTimeout = (id) => { timers = timers.filter((t) => t.id !== id); };
win.setInterval = () => 0;  // other modules' tickers are irrelevant here

let hidden = false;
Object.defineProperty(win.document, 'hidden', { get: () => hidden, configurable: true });

const calls = [];  // {headers, resolve}
win.fetch = (url, opts) => new Promise((resolve) => {
  calls.push({ url, headers: (opts && opts.headers) || {}, resolve });
});

function respond(call, { status = 200, etag = null, body = {} } = {}) {
  call.resolve({
    status,
    ok: status >= 200 && status < 300,
    headers: { get: (k) => (k.toLowerCase() === 'etag' ? etag : null) },
    json: async () => body,
  });
}
const flush = () => new Promise((r) => setImmediate(r));

for (const src of [morphdomSrc, refreshSrc]) {
  const s = win.document.createElement('script');
  s.textContent = src;
  win.document.head.appendChild(s);
}
const R = win.__queueRefresh;

let failures = 0;
function assert(label, cond, detail) {
  if (cond) console.log(`PASS  ${label}`);
  else { failures++; console.error(`FAIL  ${label}${detail ? `: ${detail}` : ''}`); }
}
const pending = () => timers.map((t) => t.ms);
function fireTimer() {
  const t = timers.shift();
  t.fn();
}

(async () => {
  // 1. Boot: exactly one timer at 5s, no request yet.
  assert('boot schedules one 5s poll and no request', pending().join() === '5000' && calls.length === 0, String(pending()));

  // 2. Fire: request goes out, and NO next timer until it settles.
  fireTimer();
  await flush();
  assert('poll sends one request', calls.length === 1);
  assert('first request has no If-None-Match', !calls[0].headers['If-None-Match']);
  assert('no next poll scheduled while a request is in flight', timers.length === 0);

  // A visibility flip while in flight must not start a second request.
  hidden = true; win.document.dispatchEvent(new win.Event('visibilitychange'));
  hidden = false; win.document.dispatchEvent(new win.Event('visibilitychange'));
  await flush();
  assert('visibility flip while in flight does not overlap', calls.length === 1);

  // 3. Settle with 200 + ETag -> next poll at 5s.
  respond(calls[0], { etag: '"abc"', body: { running: [], pending: [], totals: {} } });
  await flush(); await flush();
  assert('after success the next poll is 5s out', pending().join() === '5000', String(pending()));
  assert('ETag remembered', R.pollState().lastEtag === '"abc"');

  // 4. Next poll is conditional; 304 -> no merge, still 5s cadence.
  fireTimer();
  await flush();
  assert('second request sends If-None-Match', calls[1].headers['If-None-Match'] === '"abc"');
  respond(calls[1], { status: 304 });
  await flush(); await flush();
  assert('304 keeps the 5s cadence', pending().join() === '5000', String(pending()));
  assert('304 does not reset failures', R.pollState().failures === 0);

  // 5. Errors back off: 10s, 20s, 40s, 60s cap; success resets.
  const expected = [10000, 20000, 40000, 60000, 60000];
  for (let i = 0; i < expected.length; i++) {
    fireTimer();
    await flush();
    respond(calls[calls.length - 1], { status: 500 });
    await flush(); await flush();
    assert(`error #${i + 1} backs off to ${expected[i]}ms`, pending().join() === String(expected[i]), String(pending()));
  }
  fireTimer();
  await flush();
  respond(calls[calls.length - 1], { etag: '"def"', body: { running: [], pending: [], totals: {} } });
  await flush(); await flush();
  assert('success after errors resets to 5s', pending().join() === '5000', String(pending()));

  // Network failure (fetch rejects) also backs off.
  const realFetch = win.fetch;
  win.fetch = () => Promise.reject(new Error('net down'));
  fireTimer();
  await flush(); await flush();
  assert('rejected fetch backs off to 10s', pending().join() === '10000', String(pending()));
  win.fetch = realFetch;

  // Hidden tab: nothing scheduled; visible again: immediate poll.
  const before = calls.length;
  hidden = true; win.document.dispatchEvent(new win.Event('visibilitychange'));
  assert('hiding the tab cancels the pending poll', timers.length === 0, String(pending()));
  hidden = false; win.document.dispatchEvent(new win.Event('visibilitychange'));
  await flush();
  assert('showing the tab polls immediately', calls.length === before + 1);
  respond(calls[calls.length - 1], { etag: '"ghi"', body: { running: [], pending: [], totals: {} } });
  await flush(); await flush();
  assert('and resumes the 5s cadence', pending().join() === '5000', String(pending()));

  console.log('---');
  if (failures) { console.error(`FAILED: ${failures} assertion(s)`); process.exit(1); }
  console.log('ALL TESTS PASSED');
  process.exit(0);
})();
