#!/usr/bin/env node
// Integration test: static/refresh.js AND static/multitail.js in one page.
//
// The two modules have a suite each, and between them sat the gap this file
// exists to close. multitail.test.js drives the panes against HAND-WRITTEN
// cards; refresh.test.js drives the 5s morph without the overlay. Neither one
// answers the question an operator actually asks:
//
//   **When a running item finishes, does its pane notice and go away?**
//
// That path crosses both modules. multitail marks a pane ENDED by observing
// that its row is no longer `.item[data-live-log-mode]`, and the thing that
// removes the attribute is refresh.js rebuilding the section from a payload
// where the item has moved to `done`. A renderer that kept the attribute on a
// finished card, or a morph that left the stale card in place, would leave
// panes for long-dead agents on screen forever — and BOTH suites would still
// pass, because each one owns only its half.
//
// Also pinned here, for the same reason: the STALE-BUILD banner (botchat
// #4947). A page that polls JSON forever never reloads, so a deployed
// front-end fix can be entirely absent from an open tab, which is how the
// ended-pane retention shipped in one build and was reported as broken from a
// tab still running the previous one. The banner is the only signal that
// situation produces, so the comparison it rests on is worth a test.
//
// Usage:   node multitail-refresh.test.js
// Exit 0 on success, 1 on first failure.

'use strict';

const path = require('path');
const fs = require('fs');

const NODE_MODULES = process.env.QM_NODE_MODULES ||
  '/tmp/queue-minisite-test/node_modules';
const { JSDOM } = require(path.join(NODE_MODULES, 'jsdom'));

const STATIC_DIR = path.dirname(path.resolve(__filename));
const read = (f) => fs.readFileSync(path.join(STATIC_DIR, f), 'utf8');
const morphdomSrc = read(path.join('vendor', 'morphdom-2.7.4.min.js'));
const refreshSrc = read('refresh.js');
const multitailSrc = read('multitail.js');

// The page, as the server paints it: a topbar meta block, the queue root with a
// running section, the multitail overlay frame, and the stale-build banner.
const initialHTML = `<!doctype html>
<html><head></head><body data-asset-version="build-aaa">
  <div id="stale-build" class="stale-build" role="status" hidden>
    <span class="stale-build-text">This page is running an older build than the server.</span>
    <button type="button" id="stale-build-reload" class="stale-build-btn">Reload</button>
  </div>
  <header class="topbar">
    <div class="meta" id="topbar-meta">
      <span class="count count-running">1 running</span>
      <span class="count multitail-control">
        <button type="button" id="multitail-toggle" aria-pressed="false"
                aria-controls="multitail">multitail</button>
      </span>
      <div class="info-wrap"><button id="info-toggle" class="info-btn">i</button>
        <div id="info-dropdown" class="info-dropdown" hidden>
          <div class="info-row"><span class="info-value ts" data-local-time-iso="2026-09-29T03:00:00Z">03:00:00Z</span></div>
          <div class="info-row"><span class="info-value"><span class="cache-age">1</span>s</span></div>
        </div>
      </div>
    </div>
  </header>
  <main id="queue-root">
    <details id="section-running" class="queue-section" data-section-key="running" open>
      <summary class="section-summary"><h2 class="section-title">Running <span class="section-count">1</span></h2></summary>
      <article id="queue-q-wl" class="item state-running drop-zone log-clickable"
               data-queue-id="q-wl" data-queue-status="running" data-queue-starting="0"
               data-queue-summary="promote something" data-queue-description=""
               data-agent-id="" data-workload-label="promote-thing"
               data-log-mode="workload" data-live-log-mode="workload"
               tabindex="0" role="button">
        <header class="item-head"><span class="badge state-running">running</span><span class="id">q-wl</span><span class="prio" title="priority">p3</span></header>
        <p class="summary">promote something</p>
        <div class="age"><span>running 5m ago</span></div>
      </article>
    </details>
    <details id="section-done" class="queue-section" data-section-key="done">
      <summary class="section-summary"><h2 class="section-title">Done <span class="section-count">0 / 0</span></h2></summary>
      <div class="empty-mini">No completed items.</div>
    </details>
  </main>
  <section id="multitail" class="multitail" data-no-morph hidden>
    <header class="multitail-head">
      <h2 id="multitail-title">multitail</h2>
      <span id="multitail-count"></span>
      <button type="button" id="multitail-wrap" class="multitail-display" aria-pressed="false">wrap</button>
      <button type="button" id="multitail-ts" class="multitail-display" aria-pressed="false">time</button>
      <button type="button" id="multitail-retain" class="multitail-display multitail-retain" data-retention="1m">clear 1m</button>
      <button type="button" id="multitail-exit">exit</button>
    </header>
    <div id="multitail-panes"></div>
    <p id="multitail-empty" hidden>Nothing to tail</p>
  </section>
</body></html>`;

const dom = new JSDOM(initialHTML, {
  runScripts: 'outside-only',
  url: 'https://queue.test/',
});
const { window } = dom;
const { document } = window;

// jsdom has no EventSource; the panes only need a constructible stub here.
window.EventSource = class FakeEventSource {
  constructor(url) { this.url = url; this.closed = false; this.onmessage = null; this.onerror = null; }
  close() { this.closed = true; }
};

window.eval(morphdomSrc);
window.eval(refreshSrc);
window.eval(multitailSrc);

const refresh = window.__queueRefresh;
const mt = window.__multitail;

let failures = 0;
function assert(label, cond, detail) {
  if (cond) {
    console.log('  ok  ' + label);
  } else {
    failures += 1;
    console.error('  FAIL ' + label + (detail ? '\n       ' + detail : ''));
  }
}
const paneFor = (qid) => document.querySelector(`.mt-pane[data-queue-id="${qid}"]`);
const statusOf = (qid) => {
  const p = paneFor(qid);
  const s = p && p.querySelector('.mt-pane-status');
  return s ? s.textContent : '(no pane)';
};

// One running workload item — the same shape /api/queue ships.
const runningItem = {
  id: 'q-wl',
  summary: 'promote something',
  description: '',
  scope: [],
  group_head: false,
  status: 'running',
  priority: 3,
  created_by: '',
  depends_on: [],
  started_at_iso: '2026-09-29T02:55:00Z',
  age: '5m ago',
  is_starting: false,
  workload_label: 'promote-thing',
  live_log_mode: 'workload',
  owner: { mode: 'workload', alive: true, agent_id: '' },
};

const stateRunning = {
  totals: { running: 1, pending: 0, done: 0, abandoned: 0 },
  starting_count: 0,
  orphan_count: 0,
  fetched_at: '2026-09-29T03:00:00Z',
  cache_age_seconds: 1,
  error: null,
  asset_version: 'build-aaa',
  running: [runningItem],
  pending: [],
  done_recent: [],
  abandoned_recent: [],
};

const stateDone = {
  ...stateRunning,
  totals: { running: 0, pending: 0, done: 1, abandoned: 0 },
  fetched_at: '2026-09-29T03:00:05Z',
  running: [],
  done_recent: [{
    ...runningItem,
    status: 'done',
    live_log_mode: '',
    completed_at_iso: '2026-09-29T03:00:04Z',
    age: 'just now',
    exit_code: 0,
  }],
};

console.log('\n-- a finished item\'s pane ends, then clears (refresh.js + multitail.js)');
{
  // The server's first paint is already in the DOM; merging the matching state
  // must leave the row eligible (a no-op merge that dropped the attribute would
  // end every pane the moment it opened).
  refresh.mergeQueueRoot(stateRunning);
  assert('the running row is multitail-eligible after a no-op merge',
    mt.eligibleRows().length === 1, String(mt.eligibleRows().length));

  mt.openMode();
  assert('the pane exists', paneFor('q-wl') !== null);
  assert('and it is not ended', mt.panes.get('q-wl').ended === false);

  // THE TRANSITION. This is what the two suites could not see between them:
  // refresh.js moves the item into the done section on a real morph, and
  // multitail's next reconcile has to notice the row is no longer eligible.
  refresh.mergeQueueRoot(stateDone);
  assert('the merge moved the card out of the running section',
    document.querySelector('#section-running .item') === null);
  assert('and the finished card carries NO data-live-log-mode',
    document.querySelectorAll('.item[data-live-log-mode]').length === 0,
    document.querySelector('.item') ? document.querySelector('.item').outerHTML.slice(0, 220) : '(no card)');

  mt.reconcile();
  const pane = mt.panes.get('q-wl');
  assert('the pane is marked ended by the reconcile that followed the merge',
    !!pane && pane.ended === true);
  assert('it says so, with a countdown to its clear',
    /ended · no longer running · clears in \d+s/.test(statusOf('q-wl')), statusOf('q-wl'));
  assert('an ended pane holds no stream slot',
    !!pane && pane.streaming === false && pane.es === null);

  // Retention elapses. (Rewinding endedAt is how multitail.test.js drives the
  // same boundary — the point here is that the ENDED state was reached through
  // the real renderer, not hand-set.)
  pane.endedAt = Date.now() - mt.retentionMs();
  mt.reconcile();
  assert('and the pane is cleared once its retention has elapsed',
    paneFor('q-wl') === null && mt.panes.has('q-wl') === false);
  assert('the clear was not recorded as a manual dismissal',
    mt.dismissed.has('q-wl') === false);
  assert('the mode is still open',
    mt.isOpen() === true);
  // The row is gone from the eligible set, so nothing can rebuild the pane —
  // and the suppression set must not be holding it out either.
  mt.reconcile();
  assert('it stays gone', paneFor('q-wl') === null);
  assert('with no stale suppression entry left behind',
    mt.cleared.has('q-wl') === false);
  mt.closeMode();
}

console.log('\n-- stale-build banner: the page says when it is older than the server');
{
  const banner = document.getElementById('stale-build');
  assert('hidden while the page and the server agree', banner.hidden === true);

  refresh.applyBuildVersion({ asset_version: 'build-bbb' });
  assert('shown the moment the served build differs', banner.hidden === false);
  assert('and it offers a reload rather than performing one',
    !!document.getElementById('stale-build-reload'));

  refresh.applyBuildVersion({ asset_version: 'build-aaa' });
  assert('hidden again when they match', banner.hidden === true);

  // A server that ships no version at all (older build, or the key removed)
  // must not make every page shout about a difference it cannot establish.
  refresh.applyBuildVersion({});
  assert('a payload with no version is not a mismatch', banner.hidden === true);
  refresh.applyBuildVersion({ asset_version: '' });
  assert('nor is an empty one', banner.hidden === true);

  assert('the page reads its own build off <body>',
    refresh.pageAssetVersion() === 'build-aaa', refresh.pageAssetVersion());

  // The banner must NOT be a [data-no-morph] element: multitail.js treats any
  // visible one as a dialog that owns the keyboard, which would silently kill
  // its w / t / c keys while the banner was up.
  refresh.applyBuildVersion({ asset_version: 'build-ccc' });
  assert('the banner is not data-no-morph (that would disable multitail keys)',
    !banner.hasAttribute('data-no-morph'));
  mt.openMode();
  const before = mt.isWrap();
  document.dispatchEvent(new window.KeyboardEvent('keydown', { key: 'w', bubbles: true }));
  assert('so `w` still works with the banner up', mt.isWrap() === !before);
  mt.closeMode();
}

console.log(
  failures === 0
    ? '\nAll multitail+refresh integration tests passed.'
    : `\n${failures} integration test(s) FAILED.`,
);
process.exit(failures === 0 ? 0 : 1);
