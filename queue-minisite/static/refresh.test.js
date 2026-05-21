#!/usr/bin/env node
// Smoke test for refresh.js' morphdom-based merge.
//
// Boots a jsdom environment, loads vendor/morphdom-2.7.4.min.js + refresh.js,
// then drives buildQueueDOM + mergeQueueRoot with two synthetic snapshots.
// Assertions:
//   1. State A → state B with one item moved pending → running merges
//      cleanly: the moved article appears in #section-running, disappears
//      from #section-pending, totals update.
//   2. Open <details> on a card stays open across a merge.
//   3. data-no-morph subtree (mock action-modal) is not touched.
//   4. .dragging class on a row is preserved across a merge (refresh
//      shouldn't rug-pull a drag in progress).
//   5. data-local-time-rendered marker survives unchanged-iso merges.
//   6. Modifying an item's summary updates the visible text without
//      re-creating the article (preserves any DOM state we'd attach).
//   7. New STARTING-state item renders the starting badge correctly
//      (q-a828 STARTING work).
//   8. has_archive done item renders the log-clickable + archive attrs
//      (q-edcf archive modal compatibility).
//
// Usage:   node refresh.test.js
// Exit 0 on success, 1 on first failure.

'use strict';

const path = require('path');
const fs = require('fs');

// Dependency: jsdom installed in /tmp/queue-minisite-test/node_modules.
const NODE_MODULES = process.env.QM_NODE_MODULES ||
  '/tmp/queue-minisite-test/node_modules';
const { JSDOM } = require(path.join(NODE_MODULES, 'jsdom'));

const STATIC_DIR = path.dirname(path.resolve(__filename));
const morphdomSrc = fs.readFileSync(
  path.join(STATIC_DIR, 'vendor', 'morphdom-2.7.4.min.js'),
  'utf8',
);
const refreshSrc = fs.readFileSync(
  path.join(STATIC_DIR, 'refresh.js'),
  'utf8',
);

// Bootstrap a minimal page mirroring the Jinja initial render of the
// running/pending sections.
const initialHTML = `<!doctype html>
<html><head></head><body>
  <div class="meta" id="topbar-meta">
    <span class="count count-running">1 running</span>
    <span class="count count-pending">2 pending</span>
    <span class="dot dot-ok"></span>
    <span class="ts" data-local-time-iso="2026-05-01T20:00:00Z" data-local-time-fmt="time">20:00:00Z</span>
    <div class="info-wrap"><button id="info-toggle" class="info-btn">i</button>
      <div id="info-dropdown" class="info-dropdown" hidden>
        <div class="info-row"><span class="info-label">user</span><span class="info-value">test@example.com</span></div>
        <div class="info-row"><span class="info-label">cache</span><span class="info-value"><span class="cache-age">1</span>s</span></div>
      </div>
    </div>
  </div>
  <main id="queue-root">
    <section id="section-running">
      <h2 class="section-title">Running <span class="section-count">1</span></h2>
      <article class="item state-running drop-zone log-clickable" data-queue-id="q-aaaa" data-queue-status="running" data-queue-starting="0" data-queue-summary="alpha" data-queue-description="" data-agent-id="agent-aaaa" data-log-mode="live" tabindex="0" role="button">
        <header class="item-head"><span class="badge state-running">running</span><span class="id">q-aaaa</span><span class="prio" title="priority">p3</span><button type="button" class="action-btn stop-btn" data-action="stop" data-id="q-aaaa" data-summary="alpha">stop</button></header>
        <p class="summary">alpha</p>
        <div class="age"><span>running 5m ago</span></div>
        <details class="prompt-toggle"><summary class="prompt-summary">Prompt (10 chars)</summary><pre class="prompt-body">test promp</pre></details>
      </article>
    </section>
    <section id="section-pending">
      <h2 class="section-title">Pending <span class="section-count">2</span></h2>
      <article class="item state-pending drop-zone draggable ready" draggable="true" data-queue-id="q-bbbb" data-queue-status="pending" data-queue-summary="beta">
        <header class="item-head"><span class="badge state-pending">pending</span><span class="badge ghead">ready</span><span class="id">q-bbbb</span><span class="prio" title="priority">p4</span><span class="drag-handle">☰</span><button type="button" class="action-btn abandon-btn" data-action="abandon" data-id="q-bbbb" data-summary="beta">abandon</button></header>
        <p class="summary">beta</p>
        <div class="age"><span>created 2m ago</span></div>
      </article>
      <article class="item state-pending drop-zone draggable" draggable="true" data-queue-id="q-cccc" data-queue-status="pending" data-queue-summary="gamma">
        <header class="item-head"><span class="badge state-pending">pending</span><span class="id">q-cccc</span><span class="prio" title="priority">p5</span><span class="drag-handle">☰</span><button type="button" class="action-btn abandon-btn" data-action="abandon" data-id="q-cccc" data-summary="gamma">abandon</button></header>
        <p class="summary">gamma</p>
        <div class="age"><span>created 1m ago</span></div>
      </article>
    </section>
    <section id="section-done">
      <h2 class="section-title">Done <span class="section-count">0 / 0</span></h2>
      <div class="empty-mini">No completed items.</div>
    </section>
    <section id="section-abandoned">
      <h2 class="section-title">Abandoned <span class="section-count">0 / 0</span></h2>
      <div class="empty-mini">No abandoned items.</div>
    </section>
  </main>
  <div id="action-modal" data-no-morph hidden></div>
  <div id="log-modal" data-no-morph hidden></div>
</body></html>`;

const dom = new JSDOM(initialHTML, { runScripts: 'dangerously' });

// Inject morphdom + refresh into the jsdom context.
const morphdomScript = dom.window.document.createElement('script');
morphdomScript.textContent = morphdomSrc;
dom.window.document.head.appendChild(morphdomScript);

const refreshScript = dom.window.document.createElement('script');
refreshScript.textContent = refreshSrc;
dom.window.document.head.appendChild(refreshScript);

const { document } = dom.window;
const $ = (sel) => document.querySelector(sel);
const $$ = (sel) => Array.from(document.querySelectorAll(sel));
const refresh = dom.window.__queueRefresh;
if (!refresh) {
  console.error('FAIL: __queueRefresh not exposed by refresh.js');
  process.exit(1);
}

let failures = 0;
function assert(label, cond, detail) {
  if (cond) {
    console.log(`PASS  ${label}`);
  } else {
    failures++;
    console.error(`FAIL  ${label}${detail ? `: ${detail}` : ''}`);
  }
}

// --- State A: matches the initialHTML snapshot ---
const stateA = {
  totals: { running: 1, pending: 2, done: 0, abandoned: 0 },
  starting_count: 0,
  orphan_count: 0,
  fetched_at: '2026-05-01T20:00:00Z',
  cache_age_seconds: 1,
  error: null,
  running: [
    {
      id: 'q-aaaa',
      summary: 'alpha',
      description: 'test promp',
      scope: [],
      group_head: false,
      status: 'running',
      priority: 3,
      created_by: '',
      depends_on: [],
      started_at_iso: '2026-05-01T19:55:00Z',
      age: '5m ago',
      is_starting: false,
      owner: { mode: 'agent', alive: true, agent_id: 'agent-aaaa', jsonl_age: '10s ago' },
    },
  ],
  pending: [
    { id: 'q-bbbb', summary: 'beta', description: '', scope: [], group_head: true, priority: 4, created_by: '', depends_on: [], created_at_iso: '2026-05-01T19:58:00Z', age: '2m ago', is_starting: false },
    { id: 'q-cccc', summary: 'gamma', description: '', scope: [], group_head: false, priority: 5, created_by: '', depends_on: [], created_at_iso: '2026-05-01T19:59:00Z', age: '1m ago', is_starting: false },
  ],
  done_recent: [],
  abandoned_recent: [],
};

// --- State B: q-bbbb moved from pending → running, gamma stayed pending ---
const stateB = {
  totals: { running: 2, pending: 1, done: 0, abandoned: 0 },
  starting_count: 0,
  orphan_count: 0,
  fetched_at: '2026-05-01T20:00:05Z',
  cache_age_seconds: 0,
  error: null,
  running: [
    {
      id: 'q-aaaa',
      summary: 'alpha',
      description: 'test promp',
      scope: [],
      group_head: false,
      status: 'running',
      priority: 3,
      created_by: '',
      depends_on: [],
      started_at_iso: '2026-05-01T19:55:00Z',
      age: '5m ago',
      is_starting: false,
      owner: { mode: 'agent', alive: true, agent_id: 'agent-aaaa', jsonl_age: '5s ago' },
    },
    {
      id: 'q-bbbb',
      summary: 'beta',
      description: '',
      scope: [],
      group_head: false,
      status: 'running',
      priority: 4,
      created_by: '',
      depends_on: [],
      started_at_iso: '2026-05-01T20:00:01Z',
      age: '4s ago',
      is_starting: true,
      owner: { mode: 'unknown', alive: null, agent_id: '', jsonl_age: '?', is_starting: true },
    },
  ],
  pending: [
    { id: 'q-cccc', summary: 'gamma', description: '', scope: [], group_head: true, priority: 5, created_by: '', depends_on: [], created_at_iso: '2026-05-01T19:59:00Z', age: '1m ago', is_starting: false },
  ],
  done_recent: [],
  abandoned_recent: [],
};

// === TEST 1: pending → running transition merges cleanly ===
// Mark q-aaaa's <details> as open (user-action) before merge.
const detailsAaaa = $('article[data-queue-id="q-aaaa"] details');
detailsAaaa.open = true;
// Stamp data-local-time-rendered on the .ts to simulate prior hydrate.
const tsEl = $('.ts');
tsEl.setAttribute('data-local-time-rendered', '2026-05-01T20:00:00Z');
tsEl.textContent = '16:00:00';
// Mark q-cccc with .dragging to simulate an in-flight drag.
const articleCccc = $('article[data-queue-id="q-cccc"]');
articleCccc.classList.add('dragging');

refresh.mergeQueueRoot(stateB);
refresh.mergeTopbarMeta(stateB);

const runningArticles = $$('#section-running article');
const pendingArticles = $$('#section-pending article');
assert('T1a: running section has 2 items after merge',
  runningArticles.length === 2, `got ${runningArticles.length}`);
assert('T1b: q-bbbb appears in running section',
  !!$('#section-running article[data-queue-id="q-bbbb"]'));
assert('T1c: q-bbbb absent from pending section',
  !$('#section-pending article[data-queue-id="q-bbbb"]'));
assert('T1d: q-cccc still in pending section',
  !!$('#section-pending article[data-queue-id="q-cccc"]'));
assert('T1e: pending section count badge updated',
  $('#section-pending .section-count').textContent.trim() === '1');
assert('T1f: running section count badge updated',
  $('#section-running .section-count').textContent.trim() === '2');

// === TEST 2: open <details> on q-aaaa survives merge ===
const detailsAfter = $('article[data-queue-id="q-aaaa"] details');
assert('T2: open <details> preserved across merge',
  detailsAfter.open === true);

// === TEST 3: data-no-morph subtree untouched ===
const actionModal = $('#action-modal');
assert('T3a: #action-modal still present',
  !!actionModal && actionModal.hasAttribute('data-no-morph'));
const logModal = $('#log-modal');
assert('T3b: #log-modal still present',
  !!logModal && logModal.hasAttribute('data-no-morph'));

// === TEST 4: .dragging class preserved on q-cccc ===
const cccc = $('article[data-queue-id="q-cccc"]');
assert('T4: .dragging class preserved on in-flight drag row',
  cccc && cccc.classList.contains('dragging'));

// === TEST 5: STARTING state on q-bbbb (the new running entry) ===
const bbbb = $('article[data-queue-id="q-bbbb"]');
assert('T5a: q-bbbb has state-starting class',
  bbbb && bbbb.classList.contains('state-starting'),
  bbbb ? bbbb.className : 'no element');
const startingBadge = bbbb && bbbb.querySelector('.badge.state-starting');
assert('T5b: q-bbbb has starting badge',
  !!startingBadge && startingBadge.textContent.trim() === 'starting');
// PR #131 + refresh.js sibling: starting rows ARE clickable (polling
// modal). The test originally asserted the pre-PR-131 invariant
// (starting → non-clickable); now we assert the new invariant so any
// future regression that reverts refresh.js to the gated logic gets
// caught by `node refresh.test.js`.
assert('T5c: q-bbbb IS log-clickable while starting (PR #131 polling-modal)',
  bbbb && bbbb.classList.contains('log-clickable'));
assert('T5d: q-bbbb starting row has role=button + data-log-mode=live',
  bbbb && bbbb.getAttribute('role') === 'button' && bbbb.getAttribute('data-log-mode') === 'live');

// === TEST 6: identical state → no-op merge (idempotent) ===
const aaaaBefore = $('article[data-queue-id="q-aaaa"]');
const aaaaSummaryBefore = aaaaBefore.querySelector('.summary');
refresh.mergeQueueRoot(stateB);
const aaaaAfter = $('article[data-queue-id="q-aaaa"]');
assert('T6: same article element after no-op merge (identity preserved)',
  aaaaBefore === aaaaAfter);
assert('T6b: same .summary element after no-op merge',
  aaaaSummaryBefore === aaaaAfter.querySelector('.summary'));

// === TEST 7: summary update updates text without re-creating element ===
const stateC = JSON.parse(JSON.stringify(stateB));
stateC.running[0].summary = 'alpha (renamed)';
const aaaaPreUpdate = $('article[data-queue-id="q-aaaa"]');
const summaryEl = aaaaPreUpdate.querySelector('.summary');
refresh.mergeQueueRoot(stateC);
const aaaaPostUpdate = $('article[data-queue-id="q-aaaa"]');
assert('T7a: article element identity preserved across summary change',
  aaaaPreUpdate === aaaaPostUpdate);
assert('T7b: .summary text updated',
  aaaaPostUpdate.querySelector('.summary').textContent.trim() === 'alpha (renamed)');

// === TEST 8: data-queue-summary attr also updated (used by stop button) ===
assert('T8: data-queue-summary updated on summary change',
  aaaaPostUpdate.getAttribute('data-queue-summary') === 'alpha (renamed)');

// === TEST 9: new done item with archive renders log-clickable ===
const stateD = JSON.parse(JSON.stringify(stateC));
stateD.done_recent = [
  {
    id: 'q-dddd',
    summary: 'finished work',
    description: '',
    has_archive: true,
    completed_at_iso: '2026-05-01T19:50:00Z',
    age: '10m ago',
    created_by: 'andrew',
  },
];
stateD.totals.done = 1;
refresh.mergeQueueRoot(stateD);
const dddd = $('article[data-queue-id="q-dddd"]');
assert('T9a: done item q-dddd appears in #section-done', !!dddd);
assert('T9b: done item with archive has log-clickable',
  dddd && dddd.classList.contains('log-clickable'));
assert('T9c: done item has data-log-mode="archive"',
  dddd && dddd.getAttribute('data-log-mode') === 'archive');
assert('T9d: done item has log badge',
  dddd && !!dddd.querySelector('.badge.log-badge'));

// === TEST 10: dependency badge renders on pending item with depends_on ===
// Clear the .dragging class first — otherwise our onBeforeElUpdated
// hook (correctly!) skips the dragged element entirely. In production
// the dragend / drop handlers clear .dragging before any subsequent
// merge happens.
const ccccDragging = $('article[data-queue-id="q-cccc"]');
if (ccccDragging) ccccDragging.classList.remove('dragging');
const stateE = JSON.parse(JSON.stringify(stateD));
stateE.pending[0].depends_on = ['q-aaaa'];
refresh.mergeQueueRoot(stateE);
const ccccPending = $('article[data-queue-id="q-cccc"]');
const depBadge = ccccPending && ccccPending.querySelector('.badge.dep-badge');
assert('T10: dep-badge rendered with depends_on',
  !!depBadge && depBadge.textContent.includes('q-aaaa'));

// === TEST 11: count pills in topbar update ===
const meta = $('#topbar-meta');
const pendingCount = meta && meta.querySelector('.count-pending');
assert('T11a: topbar pending count updated to 1',
  pendingCount && pendingCount.textContent.includes('1'));
const runningCount = meta && meta.querySelector('.count-running');
assert('T11b: topbar running count updated to 2',
  runningCount && runningCount.textContent.includes('2'));

// === TEST 12: info-wrap subtree skipped (info-dropdown hidden state survives) ===
// Open the dropdown. Tick. It should still be visible.
const dropdown = $('#info-dropdown');
dropdown.hidden = false;
refresh.mergeTopbarMeta(stateE);
assert('T12: #info-dropdown stays visible after topbar merge (skipped)',
  $('#info-dropdown').hidden === false);

// === TEST 13a: data-local-time-rendered preserved when iso unchanged ===
// Stamp a localized hydration on the q-aaaa age span (mimicking what
// LocalTime.hydrate() does), then run a merge with the same iso.
const ageSpan = $('article[data-queue-id="q-aaaa"] .age span');
if (ageSpan) {
  ageSpan.setAttribute('data-local-time-iso', '2026-05-01T19:55:00Z');
  ageSpan.setAttribute('data-local-time-rendered', '2026-05-01T19:55:00Z');
  ageSpan.textContent = 'running 15:55:00';
  refresh.mergeQueueRoot(stateE);
  const ageAfter = $('article[data-queue-id="q-aaaa"] .age span');
  assert('T13a: data-local-time-rendered marker preserved unchanged-iso merge',
    ageAfter && ageAfter.getAttribute('data-local-time-rendered') === '2026-05-01T19:55:00Z',
    ageAfter ? `attr=${ageAfter.getAttribute('data-local-time-rendered')}` : 'no element');
  assert('T13b: hydrated textContent preserved unchanged-iso merge',
    ageAfter && ageAfter.textContent === 'running 15:55:00',
    ageAfter ? `text=${ageAfter.textContent}` : 'no element');
} else {
  console.error('SKIP  T13a/b: .age span not found (page structure changed?)');
}

// === TEST 14: action-modal user-input value preserved across merge ===
// Inject an input + focus it, simulate user typing, run merge. data-no-morph
// should keep value untouched.
const fakeInput = document.createElement('input');
fakeInput.id = 'fake-modal-input';
fakeInput.value = 'user typed text';
$('#action-modal').appendChild(fakeInput);
refresh.mergeQueueRoot(stateE);
assert('T14: input inside data-no-morph subtree retains value',
  $('#fake-modal-input') && $('#fake-modal-input').value === 'user typed text');

// === TEST 15: BLOCKED section regression (q-2026-05-20-db66) ===
// Bug: refresh.js' buildQueueDOM() only emitted running/pending/done/
// abandoned sections — there was no renderBlockedSection. The Jinja
// template renders BLOCKED server-side, but the first SPA tick (5s
// after page load) would morphdom-merge a new queue-root that omitted
// #section-blocked entirely, and the server's blocked items would
// disappear.
//
// Critical sub-case: blocked items can have a free-text block_reason
// that references other queue ids ("Waiting on q-13b9 to complete").
// Those references are FYI for the operator only — they are NOT
// depends_on edges. The blocked item must remain visible regardless
// of whether the referenced ids are done. The renderer doesn't even
// parse block_reason; it just shows the row whenever status=blocked.
const stateF = JSON.parse(JSON.stringify(stateE));
stateF.blocked = [
  {
    id: 'q-99ad',
    summary: 'Reseed 3 shows from Raiden after promotes',
    description: '',
    scope: ['repo:media-tools'],
    group_head: true,
    status: 'blocked',
    priority: 3,
    created_by: 'andrew',
    block_reason: 'Waiting on q-13b9 promote-3-shows workload to complete. depends_on relation is what should normally hold this back, but the obligation predicate ignores depends_on. Re-enter running via unblock when q-13b9 done.',
    depends_on: [],
    blocked_at_iso: '2026-05-20T18:30:00Z',
    age: '15m ago',
    is_starting: false,
  },
];
stateF.totals.blocked = 1;
refresh.mergeQueueRoot(stateF);
const blockedSection = $('#section-blocked');
assert('T15a: #section-blocked rendered when blocked items present',
  !!blockedSection);
const q99ad = $('article[data-queue-id="q-99ad"]');
assert('T15b: blocked item q-99ad article rendered',
  !!q99ad);
assert('T15c: blocked item has state-blocked class',
  q99ad && q99ad.classList.contains('state-blocked'));
assert('T15d: blocked item has data-queue-status="blocked"',
  q99ad && q99ad.getAttribute('data-queue-status') === 'blocked');
const blockedBadge = q99ad && q99ad.querySelector('.badge.state-blocked');
assert('T15e: blocked item has blocked badge',
  !!blockedBadge && blockedBadge.textContent.trim() === 'blocked');
const reasonP = q99ad && q99ad.querySelector('p.description');
assert('T15f: block_reason surfaced as description paragraph',
  !!reasonP && reasonP.textContent.includes('Waiting on q-13b9'));
assert('T15g: blocked section count badge shows 1',
  blockedSection && blockedSection.querySelector('.section-count').textContent.trim() === '1');
// Section ordering: running → blocked → pending → done → abandoned.
// Verify #section-blocked comes after #section-running and before
// #section-pending in document order.
const queueRoot = $('#queue-root');
const sectionIds = Array.from(queueRoot.children)
  .filter((el) => el.tagName === 'SECTION')
  .map((el) => el.id);
assert('T15h: section order is running → blocked → pending → done → abandoned',
  JSON.stringify(sectionIds) === JSON.stringify([
    'section-running', 'section-blocked', 'section-pending', 'section-done', 'section-abandoned',
  ]),
  `got ${JSON.stringify(sectionIds)}`);

// === TEST 16: blocked item survives ticks even when referenced
// blocker id is done (operator-set block, not depends_on-derived). ===
//
// q-13b9 (referenced in q-99ad's block_reason free text) is in
// done_recent — exactly the q-2026-05-20-db66 bug scenario. The
// renderer must NOT cull q-99ad on this basis: only status=blocked
// drives section membership.
const stateG = JSON.parse(JSON.stringify(stateF));
stateG.done_recent = [
  {
    id: 'q-13b9',
    summary: 'promote-3-shows workload',
    description: '',
    has_archive: true,
    completed_at_iso: '2026-05-20T18:25:00Z',
    age: '20m ago',
    created_by: 'andrew',
  },
];
stateG.totals.done = 1;
refresh.mergeQueueRoot(stateG);
assert('T16a: q-99ad still rendered after referenced blocker q-13b9 transitions to done',
  !!$('article[data-queue-id="q-99ad"]'));
assert('T16b: #section-blocked still present after referenced blocker done',
  !!$('#section-blocked'));
// And q-13b9 IS in done.
assert('T16c: q-13b9 appears in #section-done',
  !!$('#section-done article[data-queue-id="q-13b9"]'));

// === TEST 17: empty blocked array removes #section-blocked ===
// When the operator unblocks q-99ad (blocked → running/pending),
// the next tick has blocked=[] and the section wrapper should
// disappear (matches the Jinja `{% if blocked %}` gate).
const stateH = JSON.parse(JSON.stringify(stateG));
stateH.blocked = [];
stateH.totals.blocked = 0;
refresh.mergeQueueRoot(stateH);
assert('T17a: #section-blocked removed when blocked array empties',
  !$('#section-blocked'));
assert('T17b: q-99ad article gone when blocked empties',
  !$('article[data-queue-id="q-99ad"]'));

// === TEST 18: blocked section appears on first SPA tick when server
// rendered without it (initial-paint had zero blocked, then one
// arrived). This mirrors the natural flow: page loads with no blocked
// items, operator runs `session-task queue block`, the next 5s tick
// must materialize #section-blocked. ===
//
// Build a fresh dom that matches the initial page (no blocked
// section), then tick with one blocked item.
const stateI = JSON.parse(JSON.stringify(stateE));
stateI.blocked = [
  {
    id: 'q-zzzz',
    summary: 'newly blocked',
    description: '',
    scope: [],
    group_head: false,
    status: 'blocked',
    priority: 5,
    created_by: '',
    block_reason: 'waiting on andrew greenlight',
    depends_on: [],
    blocked_at_iso: '2026-05-20T18:35:00Z',
    age: '1m ago',
    is_starting: false,
  },
];
stateI.totals.blocked = 1;
// First, ensure #section-blocked is gone (from T17 we already removed it).
refresh.mergeQueueRoot(stateI);
assert('T18a: #section-blocked materializes on tick when blocked item appears',
  !!$('#section-blocked'));
assert('T18b: q-zzzz rendered',
  !!$('article[data-queue-id="q-zzzz"]'));

console.log('---');
if (failures) {
  console.error(`FAILED: ${failures} assertion(s)`);
  process.exit(1);
}
console.log('ALL TESTS PASSED');
process.exit(0);
