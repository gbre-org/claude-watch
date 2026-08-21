#!/usr/bin/env node
// Tests for the live AGENT ACTIVITY counters (botchat #2967) in refresh.js.
//
// The server joins the per-agent tool-call / token snapshot onto running rows
// (it.agent_stats, pre-formatted labels) and supplies the header pill
// (state.agent_stats). refresh.js rebuilds #queue-root and #topbar-meta every
// tick, so BOTH must be re-rendered here or morphdom drops them on the first
// tick. Pins:
//   A. renderRunningItem emits the .agent-stats cell (full + short labels) in
//      the item HEAD when it.agent_stats is set, and NOTHING when null.
//   B. buildTopbarMetaDOM emits the .count-agent-stats pills (one per
//      server-formatted part, with the stale variant) when state.agent_stats
//      is set, and NOTHING when null.
//   C. (botchat #2983) the count pills are TWO stacked rows inside one
//      .count-stack — status pills on top, agent pills below — with the
//      header controls outside the stack.
//   D. style.css pins: both rows half-size + hard-nowrap (flex-wrap /
//      white-space / overflow + ellipsis), nothing hides them in compact.
//
// Usage:   node agent-stats.test.js
// Exit 0 on success, 1 on first failure.

'use strict';

const path = require('path');
const fs = require('fs');

const NODE_MODULES = process.env.QM_NODE_MODULES ||
  '/tmp/queue-minisite-test/node_modules';
const { JSDOM } = require(path.join(NODE_MODULES, 'jsdom'));

const STATIC_DIR = path.dirname(path.resolve(__filename));
const refreshSrc = fs.readFileSync(path.join(STATIC_DIR, 'refresh.js'), 'utf8');
const morphdomSrc = fs.readFileSync(
  path.join(STATIC_DIR, 'vendor', 'morphdom-2.7.4.min.js'), 'utf8');

let failures = 0;
function assert(label, cond, detail) {
  if (cond) console.log('  ok  ' + label);
  else {
    failures += 1;
    console.error('  FAIL ' + label + (detail ? '\n       ' + detail : ''));
  }
}

function boot() {
  const html = `<!doctype html><html><head></head><body>
    <div class="meta" id="topbar-meta"></div>
    <main id="queue-root"></main>
    <div id="action-modal" data-no-morph hidden></div>
    <div id="log-modal" data-no-morph hidden></div>
  </body></html>`;
  const dom = new JSDOM(html, { runScripts: 'dangerously' });
  const s1 = dom.window.document.createElement('script');
  s1.textContent = morphdomSrc; dom.window.document.head.appendChild(s1);
  const s2 = dom.window.document.createElement('script');
  s2.textContent = refreshSrc; dom.window.document.head.appendChild(s2);
  return dom;
}

function runningItem(id, agentStats) {
  return {
    id, summary: 'summary ' + id, description: '', scope: [], priority: 5,
    created_by: 'main-loop', status: 'running', age: '1m ago', age_epoch: null,
    started_at_iso: '', owner: { mode: 'agent', agent_id: 'abc', alive: true },
    is_starting: false, workload_label: '', hostjob_label: '', subagents: [],
    agent_stats: agentStats,
  };
}

const STATS = {
  agent_id: 'abc', tool_calls: 11, context_tokens: 82040, output_tokens: 3209,
  last_tool: 'Bash', age_seconds: 0.3, finished: false,
  full_label: '11 calls · 82K tok', short_label: '11·82Kt',
  title: '11 tool calls · 82K context tokens · 3.2K output tokens · last tool Bash',
};

function emptyState(extra) {
  return Object.assign({
    running: [], wedged: [], quarantined: [], pending: [], blocked: [], other: [],
    done_recent: [], abandoned_recent: [], totals: {}, sources: [],
  }, extra || {});
}

// --- A. renderRunningItem cell ---

console.log('refresh.js: running row with agent_stats renders the cell in the head');
{
  const dom = boot();
  const R = dom.window.__queueRefresh;
  assert('__queueRefresh exposed', !!R);
  const root = R.buildQueueDOM(emptyState({
    running: [runningItem('q-1', STATS), runningItem('q-2', null)],
    totals: { running: 2 },
  }));
  const a = root.querySelector('#queue-q-1');
  assert('q-1 article rendered', !!a);
  const cell = a && a.querySelector('header.item-head .agent-stats');
  assert('cell is inside the item head', !!cell);
  assert('full label', cell && cell.querySelector('.agent-stats-full').textContent === '11 calls · 82K tok',
    cell && cell.querySelector('.agent-stats-full').textContent);
  assert('short label', cell && cell.querySelector('.agent-stats-short').textContent === '11·82Kt',
    cell && cell.querySelector('.agent-stats-short').textContent);
  assert('title carries the hover detail', cell && /last tool Bash/.test(cell.getAttribute('title')));
  assert('data-tool-calls', cell && cell.getAttribute('data-tool-calls') === '11');
  assert('data-context-tokens', cell && cell.getAttribute('data-context-tokens') === '82040');
  // Cell precedes the stop button (same order as the Jinja template).
  const head = a && a.querySelector('header.item-head');
  const kids = head ? Array.from(head.children).map((e) => e.className) : [];
  const ci = kids.findIndex((c) => c === 'agent-stats');
  const si = kids.findIndex((c) => /stop-btn/.test(c));
  assert('cell before stop button', ci >= 0 && si > ci, JSON.stringify(kids));

  const b = root.querySelector('#queue-q-2');
  assert('q-2 article rendered', !!b);
  assert('no cell when agent_stats is null', b && !b.querySelector('.agent-stats'));
}

// --- B. buildTopbarMetaDOM pill ---

console.log('refresh.js: buildTopbarMetaDOM renders the agent-stats pills (fresh / stale / absent)');
{
  const dom = boot();
  const R = dom.window.__queueRefresh;
  const FRESH_PILLS = [
    { key: 'agents', text: '1 agt' }, { key: 'calls', text: '11 calls' },
    { key: 'tok', text: '82K tok' }, { key: 'main', text: 'main 546K' },
  ];
  const fresh = R.buildTopbarMetaDOM(emptyState({
    totals: { running: 2, blocked: 82, pending: 0 },
    agent_stats: {
      stale: false, label: '1 agents · 11 calls · 82K tok', main_label: 'main 546K',
      pills: FRESH_PILLS, title: '1 live agents · 11 tool calls',
    },
  }));
  const pills = Array.from(fresh.querySelectorAll('.count-agent-stats'));
  assert('four fresh pills rendered (one per part)', pills.length === 4, String(pills.length));
  assert('fresh pills not stale', pills.every((p) => !p.classList.contains('stale')));
  assert('fresh pill texts are the abbreviated parts',
    JSON.stringify(pills.map((p) => p.textContent)) === JSON.stringify(['1 agt', '11 calls', '82K tok', 'main 546K']),
    JSON.stringify(pills.map((p) => p.textContent)));
  assert('per-part classes', pills.map((p) => p.className).join('|') ===
    'count count-agent-stats agent-stats-agents|count count-agent-stats agent-stats-calls|count count-agent-stats agent-stats-tok|count count-agent-stats agent-stats-main',
    pills.map((p) => p.className).join('|'));
  assert('every pill carries the hover title', pills.every((p) => p.getAttribute('title') === '1 live agents · 11 tool calls'));
  assert('long label is NOT rendered (would not fit the half-row)', !/1 agents · 11 calls/.test(fresh.textContent));

  // --- C. stacked two-row structure (botchat #2983) ---
  const stack = fresh.querySelector('.count-stack');
  assert('one .count-stack wraps the pills', !!stack && fresh.querySelectorAll('.count-stack').length === 1);
  const rows = stack ? Array.from(stack.children) : [];
  assert('stack has exactly two rows', rows.length === 2, String(rows.length));
  assert('top row is the status row', rows[0] && rows[0].className === 'count-row count-row-status', rows[0] && rows[0].className);
  assert('bottom row is the agents row', rows[1] && rows[1].className === 'count-row count-row-agents', rows[1] && rows[1].className);
  const topTexts = rows[0] ? Array.from(rows[0].children).map((e) => e.textContent) : [];
  assert('top row holds running / blocked / pending in order',
    JSON.stringify(topTexts) === JSON.stringify(['2 running', '82 blocked', '0 pending']), JSON.stringify(topTexts));
  assert('top row children are all .count pills', rows[0] && Array.from(rows[0].children).every((e) => e.classList.contains('count')));
  assert('bottom row holds exactly the four agent pills', rows[1] && rows[1].children.length === 4 &&
    Array.from(rows[1].children).every((e) => e.classList.contains('count-agent-stats')));
  assert('bottom row carries the hover title', rows[1] && rows[1].getAttribute('title') === '1 live agents · 11 tool calls');
  // The controls (density / source filter / dot / info) stay OUTSIDE the stack,
  // after it, so the stack is the one leading flex item of #topbar-meta.
  assert('stack is the first child of #topbar-meta', fresh.firstElementChild === stack);
  assert('stack is keyed by id (morphdom getNodeKey)', stack && stack.id === 'count-stack');

  // Morph onto a page whose #topbar-meta still holds the FLAT pills (the
  // pre-stack layout): the keyed stack must appear with live numbers, the
  // flat pills must be gone, and the open info dropdown must survive.
  {
    const d = dom.window.document;
    const meta = d.getElementById('topbar-meta');
    meta.innerHTML = '<span class="count count-running">1 running</span>' +
      '<span class="count count-pending">2 pending</span><span class="dot dot-ok"></span>' +
      '<div class="info-wrap"><button id="info-toggle" class="info-btn">i</button>' +
      '<div id="info-dropdown" class="info-dropdown"></div></div>';
    R.mergeTopbarMeta(emptyState({
      totals: { running: 2, pending: 1 },
      agent_stats: { stale: false, label: '', main_label: '', pills: FRESH_PILLS, title: 't' },
    }));
    const live = d.getElementById('topbar-meta');
    const ls = live.querySelector('#count-stack');
    assert('merge onto flat pills: stack present', !!ls);
    assert('merge onto flat pills: stack is first child', live.firstElementChild === ls);
    assert('merge onto flat pills: no flat pill left outside the stack',
      Array.from(live.children).filter((e) => e.classList.contains('count') && !e.classList.contains('density-control')).length === 0,
      Array.from(live.children).map((e) => e.className).join('|'));
    assert('merge onto flat pills: counts updated',
      ls && ls.querySelector('.count-running').textContent === '2 running' && ls.querySelector('.count-pending').textContent === '1 pending');
    assert('merge onto flat pills: four agent pills', ls && ls.querySelectorAll('.count-agent-stats').length === 4);
    assert('merge onto flat pills: exactly one info-wrap', live.querySelectorAll('.info-wrap').length === 1);
    assert('merge onto flat pills: info dropdown open state preserved',
      d.getElementById('info-dropdown') && d.getElementById('info-dropdown').hidden === false);
    // Second merge (steady state): still one stack, still two rows.
    R.mergeTopbarMeta(emptyState({
      totals: { running: 3, pending: 0 },
      agent_stats: { stale: false, label: '', main_label: '', pills: FRESH_PILLS, title: 't' },
    }));
    assert('second merge: one stack, two rows, updated count',
      live.querySelectorAll('#count-stack').length === 1 &&
      live.querySelectorAll('#count-stack > .count-row').length === 2 &&
      live.querySelector('.count-running').textContent === '3 running');
  }
  assert('density control is outside the stack', !stack.querySelector('.density-control') && !!fresh.querySelector('.density-control'));
  assert('source filter is outside the stack', !stack.querySelector('#source-filter') && !!fresh.querySelector('#source-filter'));
  assert('no status pill outside the stack',
    fresh.querySelectorAll('.count-running, .count-blocked, .count-pending').length ===
    stack.querySelectorAll('.count-running, .count-blocked, .count-pending').length);

  const stale = R.buildTopbarMetaDOM(emptyState({
    agent_stats: { stale: true, label: 'agents n/a', main_label: '', pills: [{ key: 'na', text: 'agents n/a' }], title: 'stale' },
  }));
  const spills = Array.from(stale.querySelectorAll('.count-agent-stats'));
  assert('stale renders exactly one pill', spills.length === 1, String(spills.length));
  const spill = spills[0];
  assert('stale pill has stale class', spill && spill.classList.contains('stale'));
  assert('stale row has stale class', stale.querySelector('.count-row-agents.stale') !== null);
  assert('stale pill says n/a', spill && spill.textContent.trim() === 'agents n/a', spill && spill.textContent);
  assert('stale pill has no main pill', !stale.querySelector('.agent-stats-main'));
  assert('stale still renders two rows', stale.querySelectorAll('.count-stack > .count-row').length === 2);

  // Fallback: a payload without `pills` still renders (long label + main).
  const legacy = R.buildTopbarMetaDOM(emptyState({
    agent_stats: { stale: false, label: '1 agents · 11 calls · 82K tok', main_label: 'main 546K', title: 't' },
  }));
  const lp = Array.from(legacy.querySelectorAll('.count-agent-stats')).map((p) => p.textContent);
  assert('no-pills payload falls back to label + main', JSON.stringify(lp) === JSON.stringify(['1 agents · 11 calls · 82K tok', 'main 546K']), JSON.stringify(lp));

  const absent = R.buildTopbarMetaDOM(emptyState({ agent_stats: null }));
  assert('no pill when agent_stats is null', !absent.querySelector('.count-agent-stats'));
  assert('no agents row when agent_stats is null', !absent.querySelector('.count-row-agents'));
  assert('status row still rendered when agent_stats is null', !!absent.querySelector('.count-stack > .count-row-status .count-running'));
  const missing = R.buildTopbarMetaDOM(emptyState());
  assert('no pill when agent_stats key is missing', !missing.querySelector('.count-agent-stats'));
}

// --- D. CSS pins: reduced size + nowrap on both rows (botchat #2983) ---

console.log('style.css: stacked count rows are half-size and hard-nowrap');
{
  const css = fs.readFileSync(path.join(STATIC_DIR, 'style.css'), 'utf8');
  const block = (sel) => {
    const m = css.match(new RegExp(sel.replace(/[.*+?^${}()|[\]\\]/g, '\\$&') + '\\s*\\{([^}]*)\\}'));
    return m ? m[1] : '';
  };
  const stackCss = block('.count-stack');
  assert('.count-stack is a column flex', /flex-direction:\s*column/.test(stackCss), stackCss);
  assert('.count-stack can shrink (min-width: 0)', /min-width:\s*0/.test(stackCss), stackCss);
  const rowCss = block('.count-row');
  assert('.count-row flex-wrap: nowrap', /flex-wrap:\s*nowrap/.test(rowCss), rowCss);
  assert('.count-row white-space: nowrap', /white-space:\s*nowrap/.test(rowCss), rowCss);
  assert('.count-row overflow: hidden', /overflow:\s*hidden/.test(rowCss), rowCss);
  const pillCss = block('.count-row .count');
  const fs_ = pillCss.match(/font-size:\s*([0-9.]+)rem/);
  assert('.count-row .count font-size is reduced (< 0.75rem)', fs_ && parseFloat(fs_[1]) < 0.75, pillCss);
  assert('.count-row .count nowrap + ellipsis', /white-space:\s*nowrap/.test(pillCss) && /text-overflow:\s*ellipsis/.test(pillCss) && /overflow:\s*hidden/.test(pillCss), pillCss);
  assert('mobile override keeps it small (no wrap rule added)', /\.count-row \.count \{ font-size: 0\.62rem; padding: 1px 5px; \}/.test(css));
  assert('compact density override present', /html\.density-compact \.count-row \.count \{ font-size: 0\.64rem;/.test(css));
  assert('nothing hides the stack/rows in compact or collapsed',
    !/html\.(?:density-compact|header-collapsed)[^{]*\.count-(?:stack|row)[^{]*\{[^}]*display:\s*none/.test(css));
}

if (failures) {
  console.error(`\n${failures} failure(s)`);
  process.exit(1);
}
console.log('\nall agent-stats tests passed');
// refresh.js arms a setInterval tick inside the jsdom window; exit explicitly
// (like refresh.test.js) or the process never returns on success.
process.exit(0);
