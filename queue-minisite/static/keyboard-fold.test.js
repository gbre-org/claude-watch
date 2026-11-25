#!/usr/bin/env node
// j/k skip rows inside folded sections; folding the selected row's section
// moves the selection to a visible row. Usage: node keyboard-fold.test.js
'use strict';
const path = require('path');
const fs = require('fs');
const NODE_MODULES = process.env.QM_NODE_MODULES || '/tmp/queue-minisite-test/node_modules';
const { JSDOM } = require(path.join(NODE_MODULES, 'jsdom'));
const src = fs.readFileSync(path.join(path.dirname(path.resolve(__filename)), 'keyboard.js'), 'utf8');

let failures = 0;
function assert(label, cond, detail) {
  if (cond) console.log('  ok  ' + label);
  else { failures += 1; console.error('  FAIL ' + label + (detail ? '\n       ' + detail : '')); }
}
const row = (id) => `<article class="item" data-queue-id="${id}"></article>`;
const dom = new JSDOM(`<body>
<details class="queue-section" data-section-key="running" open><summary>R</summary>${row('a')}${row('b')}</details>
<details class="queue-section" data-section-key="blocked"><summary>B</summary>${row('x')}${row('y')}</details>
<details class="queue-section" data-section-key="pending" open><summary>P</summary>${row('c')}<div hidden>${row('h')}</div>${row('d')}</details>
</body>`, { runScripts: 'outside-only', pretendToBeVisual: true });
const { window } = dom;
window.Element.prototype.scrollIntoView = function () {};
window.eval(src);
const kb = window.__queueKeyboard;
const key = (k) => window.document.dispatchEvent(new window.KeyboardEvent('keydown', { key: k, bubbles: true, cancelable: true }));
const sel = () => kb.selectedQueueId;

key('j'); assert('first j -> a', sel() === 'a', sel());
key('j'); key('j'); assert('j skips folded x,y -> c', sel() === 'c', sel());
key('j'); assert('j skips hidden h -> d', sel() === 'd', sel());
key('j'); assert('j wraps to a', sel() === 'a', sel());
key('k'); assert('k wraps back to d', sel() === 'd', sel());
key('k'); key('k'); assert('k skips folded -> b', sel() === 'b', sel());

// Fold the selected row's section: selection moves to the next visible row.
const running = window.document.querySelector('[data-section-key=running]');
running.open = false;
running.dispatchEvent(new window.Event('toggle'));
assert('folding selected section moves to c', sel() === 'c', sel());
assert('only one row painted', window.document.querySelectorAll('.kbd-selected').length === 1);

// Fold the last open section too: nothing visible remains.
const pending = window.document.querySelector('[data-section-key=pending]');
pending.open = false;
pending.dispatchEvent(new window.Event('toggle'));
assert('all folded -> selection cleared', sel() === null, sel());
key('j'); assert('j with nothing visible is a no-op', sel() === null, sel());

// Unfold blocked: its rows become navigable.
window.document.querySelector('[data-section-key=blocked]').open = true;
key('j'); assert('unfolded section reachable', sel() === 'x', sel());

window.close();
if (failures) { console.error(failures + ' failure(s)'); process.exit(1); }
console.log('all ok');
