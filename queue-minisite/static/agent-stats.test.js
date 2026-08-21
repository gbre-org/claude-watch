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
//   B. buildTopbarMetaDOM emits the .count-agent-stats pill (with the stale
//      variant) when state.agent_stats is set, and NOTHING when null.
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

console.log('refresh.js: buildTopbarMetaDOM renders the agent-stats pill (fresh / stale / absent)');
{
  const dom = boot();
  const R = dom.window.__queueRefresh;
  const fresh = R.buildTopbarMetaDOM(emptyState({
    agent_stats: {
      stale: false, label: '1 agents · 11 calls · 82K tok', main_label: 'main 546K',
      title: '1 live agents · 11 tool calls',
    },
  }));
  const pill = fresh.querySelector('.count-agent-stats');
  assert('fresh pill rendered', !!pill);
  assert('fresh pill not stale', pill && !pill.classList.contains('stale'));
  assert('fresh pill label', pill && /1 agents · 11 calls · 82K tok/.test(pill.textContent), pill && pill.textContent);
  assert('fresh pill main label', pill && pill.querySelector('.agent-stats-main') &&
    pill.querySelector('.agent-stats-main').textContent === '· main 546K');

  const stale = R.buildTopbarMetaDOM(emptyState({
    agent_stats: { stale: true, label: 'agents n/a', main_label: '', title: 'stale' },
  }));
  const spill = stale.querySelector('.count-agent-stats');
  assert('stale pill rendered', !!spill);
  assert('stale pill has stale class', spill && spill.classList.contains('stale'));
  assert('stale pill says n/a', spill && spill.textContent.trim() === 'agents n/a', spill && spill.textContent);
  assert('stale pill has no main label', spill && !spill.querySelector('.agent-stats-main'));

  const absent = R.buildTopbarMetaDOM(emptyState({ agent_stats: null }));
  assert('no pill when agent_stats is null', !absent.querySelector('.count-agent-stats'));
  const missing = R.buildTopbarMetaDOM(emptyState());
  assert('no pill when agent_stats key is missing', !missing.querySelector('.count-agent-stats'));
}

if (failures) {
  console.error(`\n${failures} failure(s)`);
  process.exit(1);
}
console.log('\nall agent-stats tests passed');
