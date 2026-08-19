#!/usr/bin/env node
// Exercises the BLOCKED-item detail modal (botchat #2413) inside jsdom.
//
// The Python suite (queue-minisite/test_blocked_detail_modal.py) pins the
// server side — the /api/queue/<id>/meta payload, the clickable card markup
// in both the template AND the SPA renderer, and the CSS. This file pins the
// half that only exists at runtime: what live-log.js actually DOES with that
// payload once the modal is open.
//
//   - applyBlockReason() shows / hides the disclosure and renders the reason
//     verbatim (several-hundred-word reasons must not be clamped) as TEXT
//     (reason prose containing angle brackets is not markup).
//   - applyMetaSummary() with a blocked payload fills the blocker section
//     AND the ordinary scope / group / deps / timestamps rows, because a
//     blocked item is not a second-class row.
//   - a `blocked_at` stamp lands in the timestamps row.
//
// Usage:   node blocked-modal.test.js
// Exit 0 on success, 1 on first failure.

'use strict';

const path = require('path');
const fs = require('fs');

const NODE_MODULES = process.env.QM_NODE_MODULES ||
  '/tmp/queue-minisite-test/node_modules';
const { JSDOM } = require(path.join(NODE_MODULES, 'jsdom'));

const STATIC_DIR = path.dirname(path.resolve(__filename));
const liveLogSrc = fs.readFileSync(path.join(STATIC_DIR, 'live-log.js'), 'utf8');

// Realistic length — Andrew's reasons run to a few hundred words.
const LONG_REASON = (
  'Parked pending a human greenlight on the protected-branch toggle. ' +
  'The rewrite this task needs is non-fast-forward by construction, so it ' +
  'cannot land while the protection rule stands. '
).repeat(8);

function bootDom() {
  const html = `<!doctype html><html><body>
    <div id="log-modal" data-mode="live" hidden>
      <span id="log-modal-id"></span>
      <span id="log-modal-mode-label"></span>
      <span id="log-modal-summary"></span>
      <span id="log-modal-status"></span>
      <pre id="log-modal-stream"></pre>
      <button id="log-modal-close"></button>
      <button id="log-modal-autoscroll"></button>
      <button id="log-modal-jump-top"></button>
      <button id="log-modal-jump-bottom"></button>
      <details id="log-modal-prompt" hidden>
        <summary><span id="log-modal-prompt-label"></span></summary>
        <pre id="log-modal-prompt-body"></pre>
      </details>
      <div id="log-modal-meta-summary" hidden>
        <details id="log-modal-blocker" hidden open>
          <summary><span id="log-modal-blocker-label">Block reason</span></summary>
          <pre id="log-modal-blocker-body"></pre>
        </details>
        <details id="log-meta-toggle" open>
          <summary><span class="log-meta-toggle-label">Metadata</span></summary>
          <div id="log-meta-rows">
            <div id="log-meta-row-status"></div>
            <div id="log-meta-row-runtime"></div>
            <div id="log-meta-row-times"></div>
            <div id="log-meta-row-scope"></div>
            <div id="log-meta-row-command"></div>
            <div id="log-meta-row-deps"></div>
            <div id="log-meta-row-dependents"></div>
            <div id="log-meta-row-by"></div>
            <div id="log-meta-row-group"></div>
            <div id="log-meta-row-usage"></div>
            <div id="log-meta-row-abandon"></div>
          </div>
        </details>
      </div>
      <span id="log-meta-status"></span>
      <span id="log-meta-runtime"></span>
      <span id="log-meta-times"></span>
      <span id="log-meta-scope"></span>
      <span id="log-meta-command"></span>
      <span id="log-meta-deps"></span>
      <span id="log-meta-dependents"></span>
      <span id="log-meta-by"></span>
      <span id="log-meta-group"></span>
      <span id="log-meta-usage"></span>
      <span id="log-meta-abandon"></span>
      <details id="log-modal-return" hidden>
        <summary><span id="log-modal-return-label"></span></summary>
        <pre id="log-modal-return-body"></pre>
      </details>
      <details id="log-modal-script-capture" hidden>
        <summary><span id="log-modal-script-capture-label"></span></summary>
        <div id="log-modal-script-capture-header"></div>
        <pre id="log-modal-script-capture-body"></pre>
      </details>
    </div>
  </body></html>`;
  const dom = new JSDOM(html, {
    runScripts: 'outside-only',
    pretendToBeVisual: true,
    url: 'http://localhost/',
  });
  const { window } = dom;
  window.eval(liveLogSrc);
  return { window, doc: window.document, hooks: window.__liveLog };
}

let failures = 0;
function assert(label, cond, detail) {
  if (cond) console.log('  ok  ' + label);
  else {
    failures += 1;
    console.error('  FAIL ' + label + (detail ? '\n       ' + detail : ''));
  }
}

console.log('applyBlockReason — a long reason renders in full');
{
  const { doc, hooks } = bootDom();
  hooks.applyBlockReason(LONG_REASON);
  const details = doc.getElementById('log-modal-blocker');
  const body = doc.getElementById('log-modal-blocker-body');
  assert('disclosure is shown', details.hidden === false);
  assert('disclosure defaults open', details.open === true);
  assert('reason renders verbatim', body.textContent === LONG_REASON,
    'len=' + body.textContent.length + ' want=' + LONG_REASON.length);
  assert('no ellipsis / truncation', body.textContent.indexOf('…') === -1);
  assert('label carries the char count',
    /\d+ chars/.test(doc.getElementById('log-modal-blocker-label').textContent));
}

console.log('applyBlockReason — reason prose is text, never markup');
{
  const { doc, hooks } = bootDom();
  hooks.applyBlockReason('waiting on <script>alert(1)</script> & friends');
  const body = doc.getElementById('log-modal-blocker-body');
  assert('no element injected', body.querySelector('script') === null);
  assert('raw text preserved',
    body.textContent === 'waiting on <script>alert(1)</script> & friends');
}

console.log('applyBlockReason — empty / whitespace hides the section');
{
  const { doc, hooks } = bootDom();
  hooks.applyBlockReason(LONG_REASON);
  hooks.applyBlockReason('');
  const details = doc.getElementById('log-modal-blocker');
  assert('empty hides it', details.hidden === true);
  assert('stale text cleared',
    doc.getElementById('log-modal-blocker-body').textContent === '');

  hooks.applyBlockReason('   \n  ');
  assert('whitespace-only hides it too', details.hidden === true);

  hooks.applyBlockReason(undefined);
  assert('undefined hides it too', details.hidden === true);
}

console.log('resetMetaSummary — the previous item\'s reason never leaks');
{
  const { doc, hooks } = bootDom();
  hooks.applyBlockReason(LONG_REASON);
  hooks.resetMetaSummary();
  const details = doc.getElementById('log-modal-blocker');
  assert('hidden after reset', details.hidden === true);
  assert('body emptied after reset',
    doc.getElementById('log-modal-blocker-body').textContent === '');
  assert('label reset',
    doc.getElementById('log-modal-blocker-label').textContent === 'Block reason');
}

console.log('applyMetaSummary — a blocked item is not second-class');
{
  const { doc, hooks } = bootDom();
  hooks.applyMetaSummary({
    ok: true,
    id: 'q-blk1',
    status: 'blocked',
    summary: 'summary q-blk1',
    scope: ['repo:claude-watch'],
    created_by: 'main-loop',
    group_id: 'g-alpha',
    group_head: false,
    created_at: '2026-06-01T00:00:00+00:00',
    blocked_at: '2026-06-02T09:30:00+00:00',
    depends_on: ['q-dep1'],
    depends_on_status: [{ id: 'q-dep1', status: 'pending' }],
    dependents: [{ id: 'q-dep2', status: 'pending' }],
    block_reason: LONG_REASON,
    abandon_reason: '',
    runtime_seconds: null,
  });

  assert('meta block is visible',
    doc.getElementById('log-modal-meta-summary').hidden === false);
  assert('blocker section filled from meta.block_reason',
    doc.getElementById('log-modal-blocker-body').textContent === LONG_REASON);
  assert('status pill rendered',
    /state-blocked/.test(doc.getElementById('log-meta-status').innerHTML));
  assert('scope chips rendered',
    /repo:claude-watch/.test(doc.getElementById('log-meta-scope').innerHTML));
  assert('group rendered',
    doc.getElementById('log-meta-group').textContent.indexOf('g-alpha') !== -1);
  assert('created-by rendered',
    doc.getElementById('log-meta-by').textContent === 'main-loop');
  assert('depends-on rendered',
    /q-dep1/.test(doc.getElementById('log-meta-deps').innerHTML));
  assert('dependents rendered',
    /q-dep2/.test(doc.getElementById('log-meta-dependents').innerHTML));
  assert('blocked_at lands in the timestamps row',
    /blocked/.test(doc.getElementById('log-meta-times').textContent),
    doc.getElementById('log-meta-times').textContent);
  assert('created stamp still there too',
    /created/.test(doc.getElementById('log-meta-times').textContent));
  assert('rows are unhidden',
    doc.getElementById('log-meta-row-scope').hidden === false &&
    doc.getElementById('log-meta-row-deps').hidden === false);
}

console.log('applyMetaSummary — non-blocked payload leaves the section hidden');
{
  const { doc, hooks } = bootDom();
  hooks.applyMetaSummary({
    ok: true,
    id: 'q-run1',
    status: 'running',
    summary: 'summary q-run1',
    scope: [],
    block_reason: '',
    abandon_reason: '',
    runtime_seconds: 12,
  });
  assert('no blocker section for a running item',
    doc.getElementById('log-modal-blocker').hidden === true);
}

console.log('');
if (failures) {
  console.error(failures + ' assertion(s) FAILED.');
  process.exit(1);
}
console.log('All blocked-modal assertions passed.');
