#!/usr/bin/env node
// Tests for the EXPANDED-LOG-MODE toggle (botchat #3343).
//
// Andrew asked for an "expanded mode (on by default)" on the queue-site
// live-log so much more detail flies by per event instead of the
// one-line ellipsis-truncated headline. The feature lives in
// live-log.js:
//   - `logExpanded` flag, default ON, persisted in localStorage under
//     `queue-minisite.logExpanded` (guarded against disabled storage).
//   - renderEvent() stamps `open` on every new <details class="log-event">
//     when the flag is on, so full bodies render without a click.
//   - the toggle button (#log-modal-expand) flips the flag, persists it,
//     sets aria-pressed + the modal's data-log-expanded attribute (drives
//     the headline-wrap CSS), and re-opens/closes the existing backlog.
//
// This boots jsdom with the real live-log.js and drives those code paths.
//
// Usage:   node log-expanded.test.js
// Exit 0 on success, 1 on first failure.

'use strict';

const path = require('path');
const fs = require('fs');

const NODE_MODULES = process.env.QM_NODE_MODULES ||
  '/tmp/queue-minisite-test/node_modules';
const { JSDOM } = require(path.join(NODE_MODULES, 'jsdom'));

const STATIC_DIR = path.dirname(path.resolve(__filename));
const liveLogSrc = fs.readFileSync(path.join(STATIC_DIR, 'live-log.js'), 'utf8');

function bootDom(storedValue) {
  const html = `<!doctype html><html><body>
    <div id="log-modal" class="log-modal" hidden>
      <span id="log-modal-id"></span>
      <span id="log-modal-mode-label"></span>
      <span id="log-modal-summary"></span>
      <span id="log-modal-status"></span>
      <pre id="log-modal-stream"></pre>
      <button id="log-modal-close"></button>
      <button id="log-modal-autoscroll"></button>
      <button id="log-modal-expand" aria-pressed="true"></button>
      <button id="log-modal-jump-top"></button>
      <button id="log-modal-jump-bottom"></button>
      <details id="log-modal-prompt" hidden>
        <summary><span id="log-modal-prompt-label"></span></summary>
        <pre id="log-modal-prompt-body"></pre>
      </details>
      <div id="log-modal-meta-summary" hidden>
        <details id="log-meta-toggle">
          <summary><span class="log-meta-toggle-label">Metadata</span></summary>
          <div id="log-meta-rows"></div>
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
  if (storedValue !== null) {
    window.localStorage.setItem('queue-minisite.logExpanded', storedValue);
  } else {
    window.localStorage.removeItem('queue-minisite.logExpanded');
  }
  window.eval(liveLogSrc);
  return { window, hooks: window.__liveLog };
}

// Synthetic assistant-text event — renderEvent wraps it in a
// <details class="log-event">.
function assistantEvent(text) {
  return {
    type: 'event',
    kind: 'assistant',
    rec: {
      type: 'assistant',
      timestamp: '2026-08-10T12:00:00.000Z',
      message: { role: 'assistant', content: [{ type: 'text', text: text }] },
    },
  };
}

let failures = 0;
function assert(label, cond, detail) {
  if (cond) console.log('  ok  ' + label);
  else {
    failures += 1;
    console.error('  FAIL ' + label + (detail ? '\n       ' + detail : ''));
  }
}

console.log('Default state — unset localStorage → expanded ON');
{
  const { window, hooks } = bootDom(null);
  hooks.setLogExpandedInitialState();
  const modal = window.document.getElementById('log-modal');
  const btn = window.document.getElementById('log-modal-expand');
  assert('flag defaults ON', hooks.getLogExpanded() === true);
  assert('modal data-log-expanded=true', modal.getAttribute('data-log-expanded') === 'true',
    'attr=' + modal.getAttribute('data-log-expanded'));
  assert('button aria-pressed=true', btn.getAttribute('aria-pressed') === 'true');
}

console.log('Stored "0" → expanded OFF (collapsed one-liners)');
{
  const { window, hooks } = bootDom('0');
  hooks.setLogExpandedInitialState();
  const modal = window.document.getElementById('log-modal');
  assert('flag OFF from storage', hooks.getLogExpanded() === false);
  assert('modal data-log-expanded=false', modal.getAttribute('data-log-expanded') === 'false');
}

console.log('Stored "1" → expanded ON');
{
  const { hooks } = bootDom('1');
  hooks.setLogExpandedInitialState();
  assert('flag ON from storage', hooks.getLogExpanded() === true);
}

console.log('Garbage in storage → default ON');
{
  const { hooks } = bootDom('banana');
  hooks.setLogExpandedInitialState();
  assert('garbage → default ON', hooks.getLogExpanded() === true);
}

console.log('renderEvent stamps open on new rows when expanded');
{
  const { window, hooks } = bootDom(null);
  hooks.setLogExpandedInitialState();
  hooks.renderEvent(assistantEvent('hello world detail'));
  const row = window.document.querySelector('#log-modal-stream details.log-event');
  assert('row rendered', !!row);
  assert('row is open in expanded mode', row && row.open === true, 'open=' + (row && row.open));
}

console.log('renderEvent leaves rows closed when collapsed');
{
  const { window, hooks } = bootDom('0');
  hooks.setLogExpandedInitialState();
  hooks.renderEvent(assistantEvent('hello world detail'));
  const row = window.document.querySelector('#log-modal-stream details.log-event');
  assert('row rendered', !!row);
  assert('row is closed in collapsed mode', row && row.open === false, 'open=' + (row && row.open));
}

console.log('Toggle button flips flag, persists, re-opens backlog');
{
  const { window, hooks } = bootDom('0');
  hooks.setLogExpandedInitialState();
  // Render a couple of rows while collapsed.
  hooks.renderEvent(assistantEvent('one'));
  hooks.renderEvent(assistantEvent('two'));
  let rows = window.document.querySelectorAll('#log-modal-stream details.log-event');
  assert('two rows rendered closed', rows.length === 2 && !rows[0].open && !rows[1].open);
  // Click the toggle → ON.
  const btn = window.document.getElementById('log-modal-expand');
  btn.dispatchEvent(new window.Event('click'));
  assert('flag flipped ON', hooks.getLogExpanded() === true);
  assert('persisted 1', window.localStorage.getItem('queue-minisite.logExpanded') === '1');
  rows = window.document.querySelectorAll('#log-modal-stream details.log-event');
  assert('backlog rows re-opened', rows[0].open === true && rows[1].open === true);
  assert('aria-pressed true', btn.getAttribute('aria-pressed') === 'true');
  // Click again → OFF, backlog collapses.
  btn.dispatchEvent(new window.Event('click'));
  assert('flag flipped OFF', hooks.getLogExpanded() === false);
  assert('persisted 0', window.localStorage.getItem('queue-minisite.logExpanded') === '0');
  rows = window.document.querySelectorAll('#log-modal-stream details.log-event');
  assert('backlog rows re-closed', rows[0].open === false && rows[1].open === false);
}

if (failures > 0) {
  console.error('\nFAILED: ' + failures + ' assertion(s)');
  process.exit(1);
}
console.log('\nAll log-expanded assertions passed.');
