#!/usr/bin/env node
// Test for static/rel-age.js — the shared live-ticking relative-age module
// (#1913: "relative timestamps dynamically rendered on clock ticks").
//
// Boots a jsdom environment, loads rel-age.js, then asserts:
//   1. formatRelativeAge is a VERBATIM port of app.py _humanize_age (matching
//      "Ns ago" / "Nm ago" / "Nh Nm ago" / "Nd Nh ago" + "from now").
//   2. A relative-age token TICKS: a span rendered "10s ago" advances to
//      "12s ago" after 2s of simulated time — WITHOUT any DOM re-render.
//   3. Spans without a valid data-rel-epoch are left untouched (graceful
//      degrade — the server-rendered text stays).
//
// Usage:   node rel-age.test.js
// Exit 0 on success, 1 on first failure.

'use strict';

const path = require('path');
const fs = require('fs');

const NODE_MODULES = process.env.QM_NODE_MODULES ||
  '/tmp/queue-minisite-test/node_modules';
const { JSDOM } = require(path.join(NODE_MODULES, 'jsdom'));

const STATIC_DIR = path.dirname(path.resolve(__filename));
const relAgeSrc = fs.readFileSync(path.join(STATIC_DIR, 'rel-age.js'), 'utf8');

const dom = new JSDOM('<!doctype html><html><body></body></html>', {
  runScripts: 'outside-only',
});
const { document } = dom.window;

// Load rel-age.js into the jsdom window (it auto-starts a setInterval, which
// jsdom's timer runs; we drive time via a controllable Date.now stub below).
dom.window.eval(relAgeSrc);
const RelAge = dom.window.RelAge;

let failures = 0;
function assert(label, cond, detail) {
  if (cond) {
    console.log(`PASS  ${label}`);
  } else {
    failures++;
    console.error(`FAIL  ${label}${detail ? `: ${detail}` : ''}`);
  }
}

// A controllable "now" so we can advance simulated time deterministically.
let simNow = 1_000_000; // seconds
dom.window.Date.now = () => simNow * 1000;

// --- TEST 1: formatRelativeAge mirrors _humanize_age exactly ---
const fmt = (agoSecs) => RelAge.formatRelativeAge(simNow - agoSecs);
assert('T1a: <60s -> "Ns ago"', fmt(10) === '10s ago', fmt(10));
assert('T1b: minutes -> "Nm ago"', fmt(125) === '2m ago', fmt(125));
assert('T1c: hours -> "Nh Nm ago"', fmt(3600 + 43 * 60) === '1h 43m ago',
  fmt(3600 + 43 * 60));
assert('T1d: days -> "Nd Nh ago"', fmt(86400 + 5 * 3600) === '1d 5h ago',
  fmt(86400 + 5 * 3600));
assert('T1e: future epoch -> "from now"', fmt(-30) === '30s from now', fmt(-30));

// --- TEST 2: a token TICKS as time passes (no DOM re-render) ---
const epoch = simNow - 10; // rendered "10s ago"
const span = document.createElement('span');
span.className = 'rel-age';
span.setAttribute('data-rel-epoch', String(epoch));
span.textContent = '10s ago'; // initial server-rendered value
document.body.appendChild(span);

RelAge.tick();
assert('T2a: tick renders initial "10s ago"', span.textContent === '10s ago',
  span.textContent);
simNow += 2; // 2 seconds pass
RelAge.tick();
assert('T2b: same span now reads "12s ago" WITHOUT a re-render (live tick)',
  span.textContent === '12s ago', span.textContent);

// --- TEST 3: no valid epoch -> untouched (graceful degrade) ---
const stale = document.createElement('span');
stale.className = 'rel-age';
stale.textContent = '?'; // unknown age, no data-rel-epoch
document.body.appendChild(stale);
RelAge.tick();
assert('T3a: span without data-rel-epoch left untouched', stale.textContent === '?',
  stale.textContent);
const bad = document.createElement('span');
bad.className = 'rel-age';
bad.setAttribute('data-rel-epoch', '0'); // <= 0 sentinel skipped
bad.textContent = 'never';
document.body.appendChild(bad);
RelAge.tick();
assert('T3b: data-rel-epoch="0" (sentinel) left untouched',
  bad.textContent === 'never', bad.textContent);

console.log('---');
if (failures) {
  console.error(`FAILED: ${failures} assertion(s)`);
  process.exit(1);
}
console.log('ALL TESTS PASSED');
process.exit(0);
