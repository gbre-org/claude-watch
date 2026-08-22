#!/usr/bin/env node
// Tests for the live AGENT ACTIVITY counters (botchat #2967 / #3066) in
// refresh.js + agent-bar.js.
//
// The server joins the per-agent tool-call / token snapshot onto running rows
// (it.agent_stats, pre-formatted labels) and supplies the header agent-bar
// payload (state.agent_stats: numerals, popover rows, main loop, freshness).
// refresh.js rebuilds #queue-root and #topbar-meta every tick, so BOTH must be
// re-rendered here or morphdom drops them on the first tick. Pins:
//   A. renderRunningItem emits the .agent-stats cell (full + short labels) in
//      the item HEAD when it.agent_stats is set, and NOTHING when null.
//   B. buildTopbarMetaDOM emits ONE outlined pill — button#agent-bar, "● N
//      agents · C calls · K tok", botchat's agent-bar look — in the TOP
//      row: .active (≥1 agent) / .idle (0) / .stale ("n/a" numerals), and
//      NOTHING when state.agent_stats is null. The old per-part pills are gone.
//   C. (botchat #2983, reordered #3090) the count pills are TWO stacked rows
//      inside one .count-stack — the agent-bar on TOP (right-aligned via
//      CSS), the status pills below — with the header controls outside the
//      stack; the merge keeps the info dropdown.
//   D. style.css pins: rows half-size + hard-nowrap; the agent row is pushed
//      to the stack's right edge (align-self: flex-end) while the stack stays
//      flex-start; the pill has botchat's chip geometry/colours; units
//      collapse to a/c/t under 480px; nothing hides them in compact /
//      collapsed; the popover's right inset is the --abp-right custom property.
//   E. agent-bar.js: the popover paints from the JSON seed / update(), pins on
//      click, closes on click-again / Esc / outside click, repaints live on a
//      merge while open (the rebuilt pill keeps `open` + aria-expanded), and
//      closes when the payload goes away. textContent only.
//   F. the liveness `.dot` is a `live` / `error` pill.
//   G. agent-bar.js position(): the popover's right edge is anchored to the
//      pill's right edge (--abp-right), clamped inside the topbar on both
//      sides; cleared when nothing has layout (jsdom default).
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
const agentBarSrc = fs.readFileSync(path.join(STATIC_DIR, 'agent-bar.js'), 'utf8');
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

// Boot a page with the header shell the template renders: #topbar-meta (the
// morph target), the popover shell + JSON seed OUTSIDE it, then morphdom,
// refresh.js and agent-bar.js (same order as the template).
function boot(seed) {
  const seedJson = seed === undefined ? 'null' : JSON.stringify(seed);
  const html = `<!doctype html><html><head></head><body>
    <header class="topbar">
      <div class="meta" id="topbar-meta"></div>
      <div id="agent-bar-pop" class="agent-bar-pop" role="dialog" aria-label="Live agent activity" hidden></div>
      <script type="application/json" id="agent-bar-data">${seedJson}</script>
    </header>
    <main id="queue-root"></main>
    <div id="action-modal" data-no-morph hidden></div>
    <div id="log-modal" data-no-morph hidden></div>
  </body></html>`;
  const dom = new JSDOM(html, { runScripts: 'dangerously' });
  for (const src of [morphdomSrc, refreshSrc, agentBarSrc]) {
    const s = dom.window.document.createElement('script');
    s.textContent = src;
    dom.window.document.head.appendChild(s);
  }
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

function row(over) {
  return Object.assign({
    agent_id: 'aa64be2138dbef3d8', queue_id: 'q-2026-08-22-5fc7',
    description: 'q-site: port botchat agent-bar styling', agent_type: 'general-purpose',
    last_tool: 'Bash', tool_calls: 11, context_tokens: 102300, output_tokens: 8100,
    running_seconds: 413, finished: false,
    calls_text: '11', ctx_text: '102K', out_text: '8.1K', age_text: '6m53s', last_write_text: '6s',
  }, over || {});
}

// The server's header payload (app.py _agent_stats_header), fresh.
const FRESH = {
  stale: false, agents: 3, tool_calls: 48, context_tokens: 272000, output_tokens: 14600,
  agents_text: '3', calls_text: '48', tok_text: '272K', out_text: '14K',
  pill_calls_text: '48', pill_tok_text: '272K', pill_tok_pre: '',
  window: { minutes: 15, agents: 5, agents_text: '5', calls_text: '96', ctx_text: '410K', out_text: '31K' },
  main: { context_tokens: 195432, text: '195K', age_seconds: 2, age_text: '2s' },
  rows: [
    row(),
    row({ agent_id: 'bb11', queue_id: 'q-2026-08-22-aaaa', description: 'torrent-process: flac batch', agent_type: 'torrent-process', last_tool: 'Read', calls_text: '25', ctx_text: '88K', out_text: '4K', age_text: '1h5m', last_write_text: '40s' }),
    row({ agent_id: 'cc22', queue_id: '', description: '', agent_type: '', last_tool: '', calls_text: '12', ctx_text: '81K', out_text: '2.5K', age_text: '1m16s', last_write_text: '' }),
  ],
  host: 'gomorrah', age_seconds: 2.1, age_text: '2s',
  label: '3 agents · 48 calls · 272K tok', main_label: 'main 195K',
  title: '3 live agents (last 15m window) · 48 tool calls · 272K context tokens',
};
// Nothing running: the LIVE sums are all 0, so the server hands the pill the
// window's calls and the MAIN loop's context instead (claude-watch #676).
const IDLE = Object.assign({}, FRESH, {
  agents: 0, tool_calls: 0, context_tokens: 0, output_tokens: 0,
  agents_text: '0', calls_text: '0', tok_text: '0', out_text: '0', rows: [],
  pill_calls_text: '96', pill_tok_text: '195K', pill_tok_pre: 'main',
  label: '0 agents · 0 calls · 0 tok', title: '0 live agents',
});
const STALE = {
  stale: true, agents: null, tool_calls: null, context_tokens: null, output_tokens: null,
  agents_text: 'n/a', calls_text: 'n/a', tok_text: 'n/a', out_text: 'n/a',
  pill_calls_text: 'n/a', pill_tok_text: 'n/a', pill_tok_pre: '', window: null,
  main: null, rows: [], host: '', age_seconds: 300.4, age_text: '5m0s',
  label: 'agents n/a', main_label: '',
  title: 'agent-stats snapshot is stale (written 5m ago) — counters withheld rather than frozen',
};

function emptyState(extra) {
  return Object.assign({
    running: [], wedged: [], quarantined: [], pending: [], blocked: [], other: [],
    done_recent: [], abandoned_recent: [], totals: {}, sources: [],
  }, extra || {});
}

function click(dom, el) {
  el.dispatchEvent(new dom.window.MouseEvent('click', { bubbles: true, cancelable: true }));
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

// --- B. buildTopbarMetaDOM: the agent-bar pill ---

console.log('refresh.js: buildTopbarMetaDOM renders the agent-bar pill (active / idle / stale / absent)');
{
  const dom = boot();
  const R = dom.window.__queueRefresh;
  const fresh = R.buildTopbarMetaDOM(emptyState({
    totals: { running: 3, blocked: 84, pending: 0 },
    agent_stats: FRESH,
  }));
  const bars = fresh.querySelectorAll('#agent-bar');
  assert('exactly one agent-bar pill', bars.length === 1, String(bars.length));
  const bar = bars[0];
  assert('pill is a <button>', bar && bar.tagName === 'BUTTON' && bar.getAttribute('type') === 'button');
  assert('pill classes: agent-bar active (≥1 live agent)', bar && bar.className === 'agent-bar active', bar && bar.className);
  assert('pill aria: haspopup=dialog, expanded=false, controls the popover',
    bar && bar.getAttribute('aria-haspopup') === 'dialog' && bar.getAttribute('aria-expanded') === 'false' &&
    bar.getAttribute('aria-controls') === 'agent-bar-pop');
  assert('pill title = server title + click hint', bar && bar.getAttribute('title') === FRESH.title + ' — click for the per-agent breakdown', bar && bar.getAttribute('title'));
  assert('live dot first', bar && bar.firstElementChild && bar.firstElementChild.className === 'agent-bar-dot');
  const nums = bar ? Array.from(bar.querySelectorAll('.agent-bar-num')).map((e) => e.textContent) : [];
  assert('numerals 3 / 48 / 272K (server-formatted)', JSON.stringify(nums) === JSON.stringify(['3', '48', '272K']), JSON.stringify(nums));
  assert('numeral part classes', bar && !!bar.querySelector('.agent-bar-num.agent-bar-agents') && !!bar.querySelector('.agent-bar-num.agent-bar-calls') && !!bar.querySelector('.agent-bar-num.agent-bar-tok'));
  const longs = bar ? Array.from(bar.querySelectorAll('.agent-bar-unit-long')).map((e) => e.textContent) : [];
  const shorts = bar ? Array.from(bar.querySelectorAll('.agent-bar-unit-short')).map((e) => e.textContent) : [];
  assert('long units " agents" / " calls" / " tok"', JSON.stringify(longs) === JSON.stringify([' agents', ' calls', ' tok']), JSON.stringify(longs));
  assert('short units a / c / t (phone collapse)', JSON.stringify(shorts) === JSON.stringify(['a', 'c', 't']), JSON.stringify(shorts));
  assert('two · separators', bar && bar.querySelectorAll('.agent-bar-sep').length === 2);
  assert('pill reads "3 agents · 48 calls · 272K tok" (+ the hidden short units)',
    bar && bar.textContent.replace(/\s+/g, '') === '3agentsa·48callsc·272Ktokt', bar && bar.textContent.replace(/\s+/g, ''));
  assert('agents row wraps the pill', bar && bar.parentElement.className === 'count-row count-row-agents');
  assert('old per-part pills are NOT rendered', !fresh.querySelector('.count-agent-stats') && !/ agt\b/.test(fresh.textContent) && !/main 195K/.test(fresh.textContent));
  assert('long label is NOT rendered', !/3 agents · 48 calls/.test(fresh.textContent));

  const idle = R.buildTopbarMetaDOM(emptyState({ agent_stats: IDLE }));
  const ibar = idle.querySelector('#agent-bar');
  assert('idle (0 agents): class agent-bar idle', ibar && ibar.className === 'agent-bar idle', ibar && ibar.className);
  // #676: never "0 agents · 0 calls · 0 tok" — the live sums are structurally
  // 0 with nothing running, so the pill shows the window's calls + main ctx.
  assert('idle numerals 0 / window calls / main ctx', ibar && JSON.stringify(Array.from(ibar.querySelectorAll('.agent-bar-num')).map((e) => e.textContent)) === JSON.stringify(['0', '96', '195K']),
    ibar && JSON.stringify(Array.from(ibar.querySelectorAll('.agent-bar-num')).map((e) => e.textContent)));
  assert('idle: token numeral is tagged "main"', ibar && ibar.querySelector('.agent-bar-pre') && ibar.querySelector('.agent-bar-pre').textContent === 'main ');
  assert('idle pill reads "0 agents · 96 calls · main 195K tok"',
    ibar && ibar.textContent.replace(/\s+/g, ' ').trim() === '0 agentsa·96 callsc·main 195K tokt',
    ibar && ibar.textContent.replace(/\s+/g, ' ').trim());

  const stale = R.buildTopbarMetaDOM(emptyState({ agent_stats: STALE }));
  const sbar = stale.querySelector('#agent-bar');
  assert('stale: class agent-bar stale', sbar && sbar.className === 'agent-bar stale', sbar && sbar.className);
  assert('stale: numerals n/a ×3 (withheld, never frozen)', sbar && JSON.stringify(Array.from(sbar.querySelectorAll('.agent-bar-num')).map((e) => e.textContent)) === JSON.stringify(['n/a', 'n/a', 'n/a']));
  assert('stale: no "main" fallback qualifier', sbar && !sbar.querySelector('.agent-bar-pre'));
  assert('stale: row carries stale', !!stale.querySelector('.count-row-agents.stale'));
  assert('stale: still two rows', stale.querySelectorAll('.count-stack > .count-row').length === 2);
  assert('stale: agents row still first', stale.querySelector('.count-stack').firstElementChild.classList.contains('count-row-agents'));

  const absent = R.buildTopbarMetaDOM(emptyState({ agent_stats: null }));
  assert('no pill when agent_stats is null', !absent.querySelector('#agent-bar'));
  assert('no agents row when agent_stats is null', !absent.querySelector('.count-row-agents'));
  assert('status row still rendered when agent_stats is null', !!absent.querySelector('.count-stack > .count-row-status .count-running'));
  assert('status row is the only row when agent_stats is null', absent.querySelectorAll('.count-stack > .count-row').length === 1);
  const missing = R.buildTopbarMetaDOM(emptyState());
  assert('no pill when agent_stats key is missing', !missing.querySelector('#agent-bar'));

  // --- C. stacked two-row structure (botchat #2983, agent row on top #3090) ---
  const stack = fresh.querySelector('.count-stack');
  assert('one .count-stack wraps the pills', !!stack && fresh.querySelectorAll('.count-stack').length === 1);
  const rows = stack ? Array.from(stack.children) : [];
  assert('stack has exactly two rows', rows.length === 2, String(rows.length));
  assert('top row is the agents row', rows[0] && rows[0].className === 'count-row count-row-agents', rows[0] && rows[0].className);
  assert('bottom row is the status row', rows[1] && rows[1].className === 'count-row count-row-status', rows[1] && rows[1].className);
  assert('top row holds exactly the agent-bar', rows[0] && rows[0].children.length === 1 && rows[0].firstElementChild.id === 'agent-bar');
  const statusTexts = rows[1] ? Array.from(rows[1].children).map((e) => e.textContent) : [];
  assert('bottom row holds running / blocked / pending in order',
    JSON.stringify(statusTexts) === JSON.stringify(['3 running', '84 blocked', '0 pending']), JSON.stringify(statusTexts));
  assert('bottom row children are all .count pills', rows[1] && Array.from(rows[1].children).every((e) => e.classList.contains('count')));
  assert('stack is the first child of #topbar-meta', fresh.firstElementChild === stack);
  assert('stack is keyed by id (morphdom getNodeKey)', stack && stack.id === 'count-stack');
  assert('density control is outside the stack', !stack.querySelector('.density-control') && !!fresh.querySelector('.density-control'));
  assert('source filter is outside the stack', !stack.querySelector('#source-filter') && !!fresh.querySelector('#source-filter'));
  assert('no status pill outside the stack',
    fresh.querySelectorAll('.count-running, .count-blocked, .count-pending').length ===
    stack.querySelectorAll('.count-running, .count-blocked, .count-pending').length);

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
    R.mergeTopbarMeta(emptyState({ totals: { running: 2, pending: 1 }, agent_stats: FRESH }));
    const live = d.getElementById('topbar-meta');
    const ls = live.querySelector('#count-stack');
    assert('merge onto flat pills: stack present', !!ls);
    assert('merge onto flat pills: stack is first child', live.firstElementChild === ls);
    assert('merge onto flat pills: no flat pill left outside the stack',
      Array.from(live.children).filter((e) => e.classList.contains('count') && !e.classList.contains('density-control')).length === 0,
      Array.from(live.children).map((e) => e.className).join('|'));
    assert('merge onto flat pills: counts updated',
      ls && ls.querySelector('.count-running').textContent === '2 running' && ls.querySelector('.count-pending').textContent === '1 pending');
    assert('merge onto flat pills: agent-bar present + active', ls && ls.querySelector('#agent-bar') && ls.querySelector('#agent-bar').className === 'agent-bar active');
    assert('merge onto flat pills: exactly one info-wrap', live.querySelectorAll('.info-wrap').length === 1);
    assert('merge onto flat pills: info dropdown open state preserved',
      d.getElementById('info-dropdown') && d.getElementById('info-dropdown').hidden === false);
    // Second merge (steady state): still one stack, still two rows.
    R.mergeTopbarMeta(emptyState({ totals: { running: 3, pending: 0 }, agent_stats: IDLE }));
    assert('second merge: one stack, two rows, updated count, pill flipped to idle',
      live.querySelectorAll('#count-stack').length === 1 &&
      live.querySelectorAll('#count-stack > .count-row').length === 2 &&
      live.querySelector('.count-running').textContent === '3 running' &&
      live.querySelector('#agent-bar').className === 'agent-bar idle');
    // Third merge: payload gone -> row gone.
    R.mergeTopbarMeta(emptyState({ totals: { running: 3, pending: 0 }, agent_stats: null }));
    assert('merge with null payload removes the agent row', !live.querySelector('#agent-bar') && !live.querySelector('.count-row-agents'));
  }
}

// --- D. CSS pins ---

console.log('style.css: stacked rows half-size + nowrap; agent-bar pill has the botchat chip look');
{
  const css = fs.readFileSync(path.join(STATIC_DIR, 'style.css'), 'utf8');
  const block = (sel) => {
    const m = css.match(new RegExp(sel.replace(/[.*+?^${}()|[\]\\]/g, '\\$&') + '\\s*\\{([^}]*)\\}'));
    return m ? m[1] : '';
  };
  const stackCss = block('.count-stack');
  assert('.count-stack is a column flex', /flex-direction:\s*column/.test(stackCss), stackCss);
  assert('.count-stack can shrink (min-width: 0)', /min-width:\s*0/.test(stackCss), stackCss);
  assert('.count-stack stays flex-start (status row keeps its left edge)', /align-items:\s*flex-start/.test(stackCss), stackCss);
  const agentsRowCss = block('.count-row-agents');
  assert('.count-row-agents hugs the stack\'s right edge (align-self: flex-end)', /align-self:\s*flex-end/.test(agentsRowCss), agentsRowCss);
  assert('.count-row-agents content right-aligned (justify-content: flex-end)', /justify-content:\s*flex-end/.test(agentsRowCss), agentsRowCss);
  assert('no align-self on the shared .count-row rule', !/align-self/.test(block('.count-row')));
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
  // The agent-bar pill (botchat chip look, half-row sized).
  const barCss = block('.agent-bar');
  assert('.agent-bar is a 999px-radius outlined inline-flex pill',
    /border-radius:\s*999px/.test(barCss) && /border:\s*1px solid var\(--line\)/.test(barCss) && /display:\s*inline-flex/.test(barCss), barCss);
  assert('.agent-bar tabular numerals + nowrap + pointer',
    /font-variant-numeric:\s*tabular-nums/.test(barCss) && /white-space:\s*nowrap/.test(barCss) && /cursor:\s*pointer/.test(barCss), barCss);
  const bfs = barCss.match(/font-size:\s*([0-9.]+)rem/);
  assert('.agent-bar sized to the half-row (< 0.75rem)', bfs && parseFloat(bfs[1]) < 0.75, barCss);
  assert('--info token defined for light + dark', (css.match(/--info:\s*#268bd2/g) || []).length === 2);
  assert('.agent-bar.active is info-blue (text + border)', /\.agent-bar\.active \{ color: var\(--info\); border-color: var\(--info\); \}/.test(css));
  assert('.agent-bar.active dot is blue and pulses', /\.agent-bar\.active \.agent-bar-dot \{ background: var\(--info\); animation: agent-bar-pulse/.test(css));
  assert('@keyframes agent-bar-pulse present', /@keyframes agent-bar-pulse/.test(css));
  assert('reduced motion stops the pulse', /\.agent-bar\.active \.agent-bar-dot \{ animation: none; \}/.test(css));
  assert('.agent-bar.stale is dashed with an amber dot', /\.agent-bar\.stale \{ border-style: dashed;/.test(css) && /\.agent-bar\.stale \.agent-bar-dot \{ background: var\(--pending\); \}/.test(css));
  assert('.agent-bar.open / hover tint', /\.agent-bar:hover, \.agent-bar\.open \{ background: var\(--bg-alt\); \}/.test(css));
  assert('short units hidden by default', /\.agent-bar \.agent-bar-unit-short \{ display: none; \}/.test(css));
  assert('≤480px: long units collapse to a/c/t', /  \.agent-bar \.agent-bar-unit-long \{ display: none; \}\n  \.agent-bar \.agent-bar-unit-short \{ display: inline;/.test(css));
  assert('compact density shrinks the pill a notch', /html\.density-compact \.agent-bar \{ font-size: 0\.64rem;/.test(css));
  // Popover.
  const popCss = block('.agent-bar-pop');
  assert('.agent-bar-pop is absolute under the topbar, right inset = --abp-right (12px fallback)',
    /position:\s*absolute/.test(popCss) && /top:\s*100%/.test(popCss) && /right:\s*var\(--abp-right, 12px\)/.test(popCss) && !/left:/.test(popCss), popCss);
  assert('.agent-bar-pop[hidden] hides', /\.agent-bar-pop\[hidden\] \{ display: none; \}/.test(css));
  assert('≤480px: popover edge-to-edge', /  \.agent-bar-pop \{ right: 6px; left: 6px; min-width: 0; max-width: none; \}/.test(css));
  assert('popover table: numeric columns right-aligned, description column left + wrapping',
    /\.agent-bar-pop th, \.agent-bar-pop td \{[^}]*text-align: right/.test(css) && /\.agent-bar-pop th:first-child, \.agent-bar-pop td:first-child \{[^}]*text-align: left/.test(css));
  assert('nothing hides the stack/rows/pill in compact or collapsed',
    !/html\.(?:density-compact|header-collapsed)[^{]*\.(?:count-(?:stack|row)|agent-bar)[^{]*\{[^}]*display:\s*none/.test(css));
}

// --- E. agent-bar.js: the popover ---

console.log('agent-bar.js: popover paints from the seed, pins on click, repaints on merge, closes on Esc / outside / payload gone');
{
  const dom = boot(FRESH);
  const d = dom.window.document;
  const R = dom.window.__queueRefresh;
  const AB = dom.window.__qsiteAgentBar;
  assert('__qsiteAgentBar exposed', !!AB && typeof AB.update === 'function' && typeof AB.open === 'function');
  assert('seed parsed on boot', AB && AB.last && AB.last.agents === 3);
  const pop = d.getElementById('agent-bar-pop');
  assert('popover hidden on boot', pop && pop.hidden === true);

  // Paint the header (first tick) then click the pill.
  R.mergeTopbarMeta(emptyState({ totals: { running: 3 }, agent_stats: FRESH }));
  let bar = d.getElementById('agent-bar');
  assert('pill rendered by the merge', !!bar);
  click(dom, bar.querySelector('.agent-bar-num'));   // click lands on a child span
  assert('click opens the popover', pop.hidden === false);
  bar = d.getElementById('agent-bar');
  assert('pill gets class open + aria-expanded=true', bar.classList.contains('open') && bar.getAttribute('aria-expanded') === 'true', bar.className);
  const head = pop.querySelector('.abp-head');
  assert('head: "3 live agents" — "48 calls · 272K ctx · 14K out"',
    head && head.children[0].textContent === '3 live agents' && head.children[1].textContent === '48 calls · 272K ctx · 14K out',
    head && head.textContent);
  const winLine = pop.querySelector('.abp-window');
  assert('window line: "last 15m" — "5 agents · 96 calls · 31K out" (finished agents included)',
    winLine && winLine.children[0].textContent === 'last 15m' &&
    winLine.children[1].textContent === '5 agents · 96 calls · 31K out',
    winLine && winLine.textContent);
  const ths = Array.from(pop.querySelectorAll('thead th')).map((e) => e.textContent);
  assert('table columns agent / calls / ctx / out / age', JSON.stringify(ths) === JSON.stringify(['agent', 'calls', 'ctx', 'out', 'age']), JSON.stringify(ths));
  const trs = Array.from(pop.querySelectorAll('tbody tr'));
  assert('one row per agent (3)', trs.length === 3, String(trs.length));
  const r0 = trs[0];
  assert('row 0: description + "type · qid · last: tool" subtitle',
    r0 && r0.querySelector('td.abp-desc > div').textContent === 'q-site: port botchat agent-bar styling' &&
    r0.querySelector('td.abp-desc .abp-type').textContent === 'general-purpose · q-2026-08-22-5fc7 · last: Bash',
    r0 && r0.querySelector('td.abp-desc').textContent);
  assert('row 0: calls / ctx / out / age cells', r0 && JSON.stringify(Array.from(r0.children).slice(1).map((e) => e.textContent)) === JSON.stringify(['11', '102K', '8.1K', '6m53s']),
    r0 && JSON.stringify(Array.from(r0.children).slice(1).map((e) => e.textContent)));
  assert('row 0: age cell titled with the last-write age', r0 && r0.children[4].getAttribute('title') === 'last transcript write 6s ago');
  assert('row 0: data-queue-id / data-agent-id stamped', r0 && r0.getAttribute('data-queue-id') === 'q-2026-08-22-5fc7' && r0.getAttribute('data-agent-id') === 'aa64be2138dbef3d8');
  const r2 = trs[2];
  assert('row 2 (no description / type / qid / tool): agent id as the label, no subtitle, no age title',
    r2 && r2.querySelector('td.abp-desc > div').textContent === 'cc22' && !r2.querySelector('.abp-type') && !r2.children[4].getAttribute('title'),
    r2 && r2.querySelector('td.abp-desc').textContent);
  const foot = pop.querySelector('.abp-foot');
  assert('foot: main loop ctx + updated · host',
    foot && foot.children[0].textContent === 'main loop ctx 195K (2s ago)' && foot.children[1].textContent === 'updated 2s ago · gomorrah',
    foot && foot.textContent);
  assert('no stale marker when fresh', !pop.querySelector('.abp-stale'));

  // A tick while open: the rebuilt pill keeps the open state and the popover repaints.
  const FRESH2 = Object.assign({}, FRESH, { agents: 4, agents_text: '4', calls_text: '60', pill_calls_text: '60', rows: FRESH.rows.concat([row({ agent_id: 'dd33', description: 'fourth' })]), age_text: '0s' });
  R.mergeTopbarMeta(emptyState({ totals: { running: 4 }, agent_stats: FRESH2 }));
  bar = d.getElementById('agent-bar');
  assert('after merge while open: popover still open', pop.hidden === false);
  assert('after merge while open: rebuilt pill keeps open + aria-expanded', bar.classList.contains('open') && bar.getAttribute('aria-expanded') === 'true', bar.className);
  assert('after merge while open: pill numerals updated', bar.querySelector('.agent-bar-agents').textContent === '4' && bar.querySelector('.agent-bar-calls').textContent === '60');
  assert('after merge while open: popover repainted (4 rows, head 4 live agents)',
    pop.querySelectorAll('tbody tr').length === 4 && pop.querySelector('.abp-head').children[0].textContent === '4 live agents');

  // Click again: unpins + closes; pill loses open.
  click(dom, d.getElementById('agent-bar'));
  assert('second click closes', pop.hidden === true);
  assert('pill loses open + aria-expanded=false', !d.getElementById('agent-bar').classList.contains('open') && d.getElementById('agent-bar').getAttribute('aria-expanded') === 'false');

  // Esc closes.
  click(dom, d.getElementById('agent-bar'));
  assert('re-open by click', pop.hidden === false);
  d.dispatchEvent(new dom.window.KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
  assert('Escape closes', pop.hidden === true);

  // Outside click closes; a click inside the popover does not.
  click(dom, d.getElementById('agent-bar'));
  click(dom, pop.querySelector('.abp-head'));
  assert('click inside the popover keeps it open', pop.hidden === false);
  click(dom, d.getElementById('queue-root'));
  assert('outside click closes', pop.hidden === true);

  // A merge that brings the pill to a "stale" payload, opened: n/a head + STALE foot, no table.
  R.mergeTopbarMeta(emptyState({ totals: { running: 0 }, agent_stats: STALE }));
  click(dom, d.getElementById('agent-bar'));
  assert('stale popover: head reads n/a + stale', pop.querySelector('.abp-head').textContent.indexOf('agent activity: n/a') === 0 && !!pop.querySelector('.abp-head .abp-stale'));
  assert('stale popover: no table, withheld note', !pop.querySelector('table') && /withheld/.test(pop.querySelector('.abp-empty').textContent));
  assert('stale popover: no window line either (nothing frozen)', !pop.querySelector('.abp-window'));
  assert('stale popover: STALE footer with the silence age', /STALE — producer silent 5m0s/.test(pop.querySelector('.abp-foot .abp-stale').textContent));
  assert('stale popover: no main loop line (main is null)', !/main loop ctx/.test(pop.querySelector('.abp-foot').textContent));

  // A merge whose payload went away closes the popover and drops the pill.
  R.mergeTopbarMeta(emptyState({ totals: { running: 0 }, agent_stats: null }));
  assert('payload gone: popover closed', pop.hidden === true);
  assert('payload gone: pill removed', !d.getElementById('agent-bar'));
  assert('payload gone: open() is a no-op without a pill', (AB.open(), pop.hidden === true));

  // Idle payload: "0 live agents" + "no live agents" body, main loop still in the footer.
  R.mergeTopbarMeta(emptyState({ totals: { running: 0 }, agent_stats: IDLE }));
  click(dom, d.getElementById('agent-bar'));
  assert('idle popover: 0 live agents / no live agents / main loop footer',
    pop.querySelector('.abp-head').children[0].textContent === '0 live agents' &&
    pop.querySelector('.abp-empty').textContent === 'no live agents' &&
    /main loop ctx 195K/.test(pop.querySelector('.abp-foot').textContent));
  assert('idle popover: the recent-window line survives the agents returning',
    /last 15m/.test(pop.querySelector('.abp-window').textContent) &&
    /96 calls/.test(pop.querySelector('.abp-window').textContent));
  click(dom, d.getElementById('agent-bar'));

  // Descriptions are prompt text: rendered as text, never as markup.
  const EVIL = Object.assign({}, FRESH, { rows: [row({ description: '<img src=x onerror="window.__pwned=1"><b>bold</b>' })] });
  R.mergeTopbarMeta(emptyState({ totals: { running: 1 }, agent_stats: EVIL }));
  click(dom, d.getElementById('agent-bar'));
  assert('description rendered as text (no elements injected)',
    !pop.querySelector('img') && !pop.querySelector('b') && pop.querySelector('td.abp-desc > div').textContent === '<img src=x onerror="window.__pwned=1"><b>bold</b>' && !dom.window.__pwned);
  click(dom, d.getElementById('agent-bar'));
}

console.log('agent-bar.js: a null seed (feature off on first paint) boots quietly and lights up on a later tick');
{
  const dom = boot();   // seed = null
  const d = dom.window.document;
  const R = dom.window.__queueRefresh;
  const AB = dom.window.__qsiteAgentBar;
  assert('null seed -> last is null', AB && AB.last === null);
  assert('open() without data is a no-op', (AB.open(), d.getElementById('agent-bar-pop').hidden === true));
  R.mergeTopbarMeta(emptyState({ totals: { running: 1 }, agent_stats: FRESH }));
  click(dom, d.getElementById('agent-bar'));
  assert('later tick: pill appears and the popover opens with rows', d.getElementById('agent-bar-pop').hidden === false && d.querySelectorAll('#agent-bar-pop tbody tr').length === 3);
}

// --- F. liveness badge ---

console.log('agent-bar.js: position() anchors the popover right edge to the pill, clamped inside the topbar');
{
  const dom = boot(FRESH);
  const d = dom.window.document;
  const R = dom.window.__queueRefresh;
  const AB = dom.window.__qsiteAgentBar;
  const pop = d.getElementById('agent-bar-pop');
  const host = d.querySelector('header.topbar');
  assert('position exposed', AB && typeof AB.position === 'function');
  R.mergeTopbarMeta(emptyState({ totals: { running: 3 }, agent_stats: FRESH }));

  // jsdom has no layout: every box is 0×0 → no override, CSS fallback applies.
  AB.open();
  assert('no layout: --abp-right not set (CSS 12px fallback)', pop.hidden === false && pop.style.getPropertyValue('--abp-right') === '', pop.style.cssText);
  AB.close();

  // Fake the boxes: a 1440px-wide header, the pill ending 340px from its
  // right edge (left of the density / source / live / info controls), a
  // 320px-wide popover.
  function rect(left, right, top, bottom) {
    return { left, right, top, bottom, width: right - left, height: bottom - top, x: left, y: top };
  }
  let hostRect = rect(0, 1440, 0, 60);
  let barRect = rect(900, 1100, 10, 26);
  host.getBoundingClientRect = () => hostRect;
  Object.defineProperty(pop, 'offsetWidth', { configurable: true, get: () => 320 });
  function stubBar() { const b = d.getElementById('agent-bar'); b.getBoundingClientRect = () => barRect; return b; }
  stubBar();
  AB.open();
  assert('anchored: right = host.right - pill.right (1440 - 1100 = 340px)', pop.style.getPropertyValue('--abp-right') === '340px', pop.style.getPropertyValue('--abp-right'));

  // The pill is rebuilt by every merge; a resize re-measures the new node
  // (moved 100px right here).
  barRect = rect(1000, 1200, 10, 26);
  R.mergeTopbarMeta(emptyState({ totals: { running: 3 }, agent_stats: FRESH }));
  stubBar();
  dom.window.dispatchEvent(new dom.window.Event('resize'));
  assert('resize re-measures: 1440 - 1200 = 240px', pop.style.getPropertyValue('--abp-right') === '240px', pop.style.getPropertyValue('--abp-right'));

  // Right clamp: a pill flush with the header edge never pulls the popover
  // closer than 12px to it.
  barRect = rect(1300, 1436, 10, 26);
  stubBar();
  AB.position();
  assert('right clamp: min 12px inset', pop.style.getPropertyValue('--abp-right') === '12px', pop.style.getPropertyValue('--abp-right'));

  // Left clamp: a narrow header (400px) with the pill far left — the popover
  // (320px) may not leave the header on the left: right ≤ 400 - 320 - 12 = 68.
  hostRect = rect(0, 400, 0, 60);
  barRect = rect(20, 120, 10, 26);
  stubBar();
  AB.position();
  assert('left clamp: right ≤ host.width - pop.width - 12 (68px)', pop.style.getPropertyValue('--abp-right') === '68px', pop.style.getPropertyValue('--abp-right'));

  // Hidden popover: position() is a no-op (keeps whatever was last set).
  AB.close();
  barRect = rect(900, 1100, 10, 26); hostRect = rect(0, 1440, 0, 60);
  stubBar();
  AB.position();
  assert('hidden: position() no-op', pop.hidden === true && pop.style.getPropertyValue('--abp-right') === '68px', pop.style.getPropertyValue('--abp-right'));
  // …and re-opening measures afresh.
  AB.open();
  assert('re-open measures afresh (340px)', pop.style.getPropertyValue('--abp-right') === '340px', pop.style.getPropertyValue('--abp-right'));

  // Boxes collapse (e.g. header display:none): override cleared.
  hostRect = rect(0, 0, 0, 0);
  AB.position();
  assert('no layout while open: override cleared', pop.style.getPropertyValue('--abp-right') === '', pop.style.cssText);
}

console.log('refresh.js: the liveness .dot is a `live` / `error` pill');
{
  const dom = boot();
  const R = dom.window.__queueRefresh;
  const ok = R.buildTopbarMetaDOM(emptyState({ totals: { running: 0 } }));
  const dot = ok.querySelector('.dot');
  assert('healthy: .dot.dot-ok reads "live"', dot && dot.classList.contains('dot-ok') && dot.textContent === 'live' && /live/.test(dot.getAttribute('title')), dot && dot.outerHTML);
  const err = R.buildTopbarMetaDOM(emptyState({ totals: { running: 0 }, error: 'boom' }));
  const edot = err.querySelector('.dot');
  assert('error: .dot.dot-err reads "error"', edot && edot.classList.contains('dot-err') && edot.textContent === 'error' && /error/.test(edot.getAttribute('title')), edot && edot.outerHTML);
}

if (failures) {
  console.error(`\n${failures} failure(s)`);
  process.exit(1);
}
console.log('\nall agent-stats tests passed');
// refresh.js arms a setInterval tick inside the jsdom window; exit explicitly
// (like refresh.test.js) or the process never returns on success.
process.exit(0);
