#!/usr/bin/env node
// Tests for static/multitail.js — the whole-window multitail mode.
//
// Boots jsdom with the overlay frame + a synthetic queue list, stubs
// EventSource (jsdom has none), loads multitail.js, then drives the module
// through its test hook (window.__multitail) to assert:
//
//   1. Panes are built from `data-live-log-mode` rows ONLY — a running row
//      with no log (a `starting` item) gets no pane.
//   2. At most MAX_LIVE_STREAMS panes hold a connection; the rest are visible
//      and labelled as waiting for a slot.
//   3. Closing one pane by hand removes it, does NOT leave the mode, keeps it
//      from coming back on the next reconcile, and promotes a waiting pane
//      into the freed slot.
//   4. Toggling works from BOTH the button and the `m` key; Esc exits.
//   5. A pane whose job ends keeps its output (marked `ended`) and gives its
//      slot back — it is never yanked out from under the reader.
//   6. The compact one-line formatter handles agent JSONL records and
//      plain-text workload lines, and never uses innerHTML.
//   7. The per-pane retained-line cap actually trims.
//   8. LINE WRAP (`w`): off by default, retroactive (already-rendered lines
//      change too, which is only possible because panes keep records), and it
//      does NOT shrink the retained-line budget.
//   9. TIMESTAMPS (`t`): SOURCE timestamps only. An agent record's own
//      `timestamp` is shown; a plain-text workload/hostjob line gets NO cell
//      and its pane says `no ts` instead of borrowing arrival time.
//  10. ENDED-PANE RETENTION (`c`): an ended pane is cleared once the chosen
//      delay has elapsed (default 1m), the timing boundary is exact, `keep`
//      genuinely keeps forever, the choice persists per viewer through
//      localStorage (and the module still works when storage throws), and a
//      clear is NOT a manual dismissal — a requeued qid gets a fresh pane.
//  11. STREAM RETRY: a pane whose log does not exist YET (`open-failed` /
//      `no-jsonl` / `no-agent` / `read-failed`) backs off and reconnects
//      instead of dying until the mode is toggled — the bug that made a
//      just-started job's pane permanently blank. Plus: it must not
//      reintroduce the replayed-backfill suppression bug, must not fight the
//      slot cap, and must lose to `ended`.
//
// Usage:   node multitail.test.js
// Exit 0 on success, 1 on first failure.

'use strict';

const path = require('path');
const fs = require('fs');

const NODE_MODULES = process.env.QM_NODE_MODULES ||
  '/tmp/queue-minisite-test/node_modules';
const { JSDOM } = require(path.join(NODE_MODULES, 'jsdom'));

const STATIC_DIR = path.dirname(path.resolve(__filename));
const src = fs.readFileSync(path.join(STATIC_DIR, 'multitail.js'), 'utf8');

// One running card per interesting shape. `q-start` is the "no log yet" case:
// clickable like every running card, but with no data-live-log-mode.
function card(qid, mode, summary) {
  const attr = mode ? ` data-live-log-mode="${mode}"` : '';
  return `<article class="item state-running log-clickable" ` +
    `data-queue-id="${qid}" data-queue-status="running" ` +
    `data-queue-summary="${summary}" data-log-mode="live"${attr}></article>`;
}

const initialHTML = `<!doctype html>
<html><head></head><body>
  <header class="topbar">
    <div class="meta" id="topbar-meta">
      <span class="count multitail-control">
        <button type="button" id="multitail-toggle" aria-pressed="false"
                aria-controls="multitail">multitail</button>
      </span>
    </div>
  </header>
  <main id="queue-root">
    ${card('q-a', 'live', 'agent one')}
    ${card('q-b', 'workload', 'workload one')}
    ${card('q-c', 'hostjob', 'hostjob one')}
    ${card('q-start', '', 'no log yet')}
    ${card('q-d', 'live', 'agent two')}
    ${card('q-e', 'live', 'agent three')}
  </main>
  <section id="multitail" class="multitail" data-no-morph hidden>
    <header class="multitail-head">
      <h2 id="multitail-title">multitail</h2>
      <span id="multitail-count"></span>
      <button type="button" id="multitail-wrap" class="multitail-display"
              aria-pressed="false">wrap</button>
      <button type="button" id="multitail-ts" class="multitail-display"
              aria-pressed="false">time</button>
      <button type="button" id="multitail-retain"
              class="multitail-display multitail-retain"
              data-retention="1m">clear 1m</button>
      <button type="button" id="multitail-exit">exit</button>
    </header>
    <div id="multitail-panes"></div>
    <p id="multitail-empty" hidden>Nothing to tail</p>
  </section>
</body></html>`;

const dom = new JSDOM(initialHTML, { runScripts: 'outside-only' });
const { window } = dom;
const { document } = window;

// --- EventSource stub ------------------------------------------------------
// Records every constructed stream so the connection cap can be asserted, and
// lets a test push a payload into a specific pane's handler.
const streams = [];
window.EventSource = class FakeEventSource {
  constructor(url) {
    this.url = url;
    this.closed = false;
    this.onmessage = null;
    this.onerror = null;
    streams.push(this);
  }
  close() { this.closed = true; }
  emit(obj) {
    if (this.onmessage) this.onmessage({ data: JSON.stringify(obj) });
  }
};
function openStreams() {
  return streams.filter((s) => !s.closed);
}
function streamFor(qid) {
  return openStreams().find((s) => s.url.indexOf(qid) !== -1);
}
// The MOST RECENT stream for a qid, open or not — the retry tests need the
// connection made by the latest attempt, not the first one ever made.
function latestStreamFor(qid) {
  for (let i = streams.length - 1; i >= 0; i--) {
    if (streams[i].url.indexOf(qid) !== -1) return streams[i];
  }
  return undefined;
}
function streamCountFor(qid) {
  return streams.filter((s) => s.url.indexOf(qid) !== -1).length;
}

window.eval(src);
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
function paneEls() {
  return Array.from(document.querySelectorAll('#multitail-panes .mt-pane'));
}
function paneFor(qid) {
  return document.querySelector(`#multitail-panes .mt-pane[data-queue-id="${qid}"]`);
}
function statusOf(qid) {
  const p = paneFor(qid);
  return p ? p.querySelector('.mt-pane-status').textContent : null;
}
function key(k, init) {
  const ev = new window.KeyboardEvent('keydown', Object.assign({
    key: k, bubbles: true, cancelable: true,
  }, init || {}));
  document.dispatchEvent(ev);
  return ev;
}
function paneRecord(qid) {
  return mt.panes.get(qid);
}
function bodyOf(qid, idx) {
  const rows = paneFor(qid).querySelectorAll('.mt-line .mt-body');
  return rows[idx === undefined ? rows.length - 1 : idx].textContent;
}

// ==========================================================================
console.log('\n-- eligibility: a pane per row WITH a log, and no others');
// ==========================================================================
mt.openMode();

assert('overlay is shown', document.getElementById('multitail').hidden === false);
assert(
  'one pane per eligible row (5 of 6 rows)',
  paneEls().length === 5,
  'got ' + paneEls().length + ' panes',
);
assert('no pane for the row with no log', paneFor('q-start') === null);
assert('workload row got a pane', paneFor('q-b') !== null);
assert('hostjob row got a pane', paneFor('q-c') !== null);
assert(
  'pane order follows render order',
  paneEls().map((p) => p.getAttribute('data-queue-id')).join(',') ===
    'q-a,q-b,q-c,q-d,q-e',
  paneEls().map((p) => p.getAttribute('data-queue-id')).join(','),
);
assert(
  'mode badge names the tail kind',
  paneFor('q-b').querySelector('.mt-pane-badge').textContent === 'workload',
);
assert('empty-state note is hidden while panes exist',
  document.getElementById('multitail-empty').hidden === true);

// ==========================================================================
console.log('\n-- connection cap: never more than MAX_LIVE_STREAMS at once');
// ==========================================================================
assert(
  'cap is small enough to leave the queue poll a connection',
  mt.MAX_LIVE_STREAMS <= 4,
  'MAX_LIVE_STREAMS=' + mt.MAX_LIVE_STREAMS,
);
assert(
  'exactly MAX_LIVE_STREAMS streams opened',
  openStreams().length === mt.MAX_LIVE_STREAMS,
  'open=' + openStreams().length,
);
assert(
  'streams went to the FIRST panes, in order',
  openStreams().every((s, i) =>
    s.url.indexOf(['q-a', 'q-b', 'q-c', 'q-d'][i]) !== -1),
  openStreams().map((s) => s.url).join(' '),
);
assert(
  'the pane past the cap says it is waiting for a slot',
  /waiting for a stream slot/.test(statusOf('q-e')),
  'status=' + statusOf('q-e'),
);
assert(
  'stream URL is the existing per-item SSE endpoint',
  streamFor('q-a').url === '/api/queue/q-a/stream',
  streamFor('q-a').url,
);

// ==========================================================================
console.log('\n-- streaming: stream-start, lines, and the retained-line cap');
// ==========================================================================
streamFor('q-a').emit({ type: 'meta', kind: 'stream-start', path: '/x.jsonl' });
// "connected but nothing has arrived" is its OWN state: the log exists and is
// (so far) empty, which must not read the same as a log that is producing.
assert('a connected-but-silent pane says the log is empty so far',
  statusOf('q-a') === 'live · no output yet', statusOf('q-a'));

streamFor('q-a').emit({
  type: 'event',
  kind: 'tool_use',
  rec: {
    type: 'assistant',
    message: { content: [{ type: 'tool_use', name: 'Bash', input: { command: 'ls -la /tmp' } }] },
  },
});
let lines = paneFor('q-a').querySelectorAll('.mt-line');
assert('tool_use renders one line', lines.length === 1);
assert(
  'tool_use line names the tool and its argument',
  lines[0].textContent.indexOf('Bash') !== -1 &&
    lines[0].textContent.indexOf('ls -la /tmp') !== -1,
  lines[0].textContent,
);
assert('once data arrives the pane reads plain live', statusOf('q-a') === 'live',
  statusOf('q-a'));

streamFor('q-b').emit({ type: 'meta', kind: 'stream-start', mode: 'workload' });
streamFor('q-b').emit({ type: 'event', kind: 'workload_line', text: 'rsync: 42% done' });
assert(
  'plain-text workload line passes through verbatim',
  paneFor('q-b').querySelector('.mt-line .mt-body').textContent === 'rsync: 42% done',
  paneFor('q-b').querySelector('.mt-line .mt-body').textContent,
);

// Retained-line cap.
for (let i = 0; i < mt.MAX_LINES_PER_PANE + 25; i++) {
  streamFor('q-b').emit({ type: 'event', kind: 'workload_line', text: 'line ' + i });
}
lines = paneFor('q-b').querySelectorAll('.mt-line');
assert(
  'retained lines are capped',
  lines.length === mt.MAX_LINES_PER_PANE,
  'kept ' + lines.length + ' of cap ' + mt.MAX_LINES_PER_PANE,
);
assert(
  'the cap trims the OLDEST lines',
  lines[lines.length - 1].textContent.indexOf(
    'line ' + (mt.MAX_LINES_PER_PANE + 24)) !== -1,
  lines[lines.length - 1].textContent,
);

// ==========================================================================
console.log('\n-- a reconnect must not re-print the backfill');
// ==========================================================================
{
  const before = paneFor('q-c');
  streamFor('q-c').emit({ type: 'meta', kind: 'stream-start', mode: 'hostjob' });
  streamFor('q-c').emit({ type: 'event', kind: 'workload_line', text: 'first' });
  const n1 = before.querySelectorAll('.mt-line').length;
  // Second stream-start on a pane that HAS already shown data = the server
  // recycled the stream (idle cap) and is replaying its tail. Plain-text tails
  // carry no resume cursor, so without suppression a quiet job re-prints its
  // whole backfill every 30s. (The gate is "has shown data", not "has seen a
  // stream-start" — see the retry section for why that distinction matters.)
  streamFor('q-c').emit({ type: 'meta', kind: 'stream-start', mode: 'hostjob' });
  streamFor('q-c').emit({ type: 'meta', kind: 'backfill-begin' });
  streamFor('q-c').emit({ type: 'event', kind: 'workload_line', text: 'first' });
  streamFor('q-c').emit({ type: 'meta', kind: 'backfill-end' });
  const n2 = before.querySelectorAll('.mt-line').length;
  assert('replayed backfill is suppressed', n2 === n1, `${n1} -> ${n2}`);
  streamFor('q-c').emit({ type: 'event', kind: 'workload_line', text: 'second' });
  assert(
    'post-backfill lines still render',
    before.querySelectorAll('.mt-line').length === n1 + 1,
  );
}

// ==========================================================================
console.log('\n-- a job that finishes keeps its pane and frees its slot');
// ==========================================================================
{
  const s = streamFor('q-b');
  s.emit({ type: 'meta', kind: 'workload-end', label: 'w', reason: 'exit', exit_code: 0 });
  assert('finished pane is STILL in the DOM', paneFor('q-b') !== null);
  assert('finished pane retains its output',
    paneFor('q-b').querySelectorAll('.mt-line').length > 0);
  assert('finished pane reads ended', /ended/.test(statusOf('q-b')), statusOf('q-b'));
  assert('finished pane carries the exit code', /exit 0/.test(statusOf('q-b')),
    statusOf('q-b'));
  assert('finished pane is visually marked', paneFor('q-b').classList.contains('mt-ended'));
  assert('its stream was closed', s.closed === true);
  assert(
    'the freed slot promoted the waiting pane',
    streamFor('q-e') !== undefined,
    'q-e stream: ' + String(streamFor('q-e') && streamFor('q-e').url),
  );
  assert('still at the cap, not over it',
    openStreams().length === mt.MAX_LIVE_STREAMS, 'open=' + openStreams().length);
}

// ==========================================================================
console.log('\n-- manual per-pane close');
// ==========================================================================
{
  const s = streamFor('q-c');
  paneFor('q-c').querySelector('.mt-pane-close').dispatchEvent(
    new window.MouseEvent('click', { bubbles: true, cancelable: true }));
  assert('closed pane is gone', paneFor('q-c') === null);
  assert('its stream was closed', s.closed === true);
  assert('the mode is still open', mt.isOpen() === true);
  assert('other panes survive', paneFor('q-a') !== null && paneFor('q-d') !== null);
  // Reconcile must not resurrect it: "I closed that" has to stick.
  mt.reconcile();
  assert('reconcile does not bring a dismissed pane back', paneFor('q-c') === null);
}

// ==========================================================================
console.log('\n-- an item that stops running is marked ended, never removed');
// ==========================================================================
{
  const row = document.querySelector('.item[data-queue-id="q-d"]');
  row.parentNode.removeChild(row);
  mt.reconcile();
  assert('vanished item keeps its pane', paneFor('q-d') !== null);
  assert('vanished item reads ended', /ended/.test(statusOf('q-d')), statusOf('q-d'));
}

// ==========================================================================
console.log('\n-- newly-eligible rows join on the next reconcile');
// ==========================================================================
{
  document.querySelector('.item[data-queue-id="q-start"]')
    .setAttribute('data-live-log-mode', 'live');
  mt.reconcile();
  assert('an item that gained a log gets a pane', paneFor('q-start') !== null);
}

// ==========================================================================
console.log('\n-- toggling: button, `m`, Esc');
// ==========================================================================
mt.closeMode();
assert('close hides the overlay', document.getElementById('multitail').hidden === true);
assert('close tears every stream down', openStreams().length === 0,
  'still open: ' + openStreams().length);
assert('close empties the pane stack', paneEls().length === 0);
assert('toggle button reads unpressed',
  document.getElementById('multitail-toggle').getAttribute('aria-pressed') === 'false');

key('m');
assert('`m` opens the mode', mt.isOpen() === true);
assert('toggle button reads pressed',
  document.getElementById('multitail-toggle').getAttribute('aria-pressed') === 'true');
key('m');
assert('`m` closes the mode again', mt.isOpen() === false);

document.getElementById('multitail-toggle').dispatchEvent(
  new window.MouseEvent('click', { bubbles: true, cancelable: true }));
assert('the header button opens the mode', mt.isOpen() === true);
key('Escape');
assert('Esc exits the mode', mt.isOpen() === false);

// The delegated click handler must survive #topbar-meta being rebuilt by the
// 5s refresh tick — that is why it is delegated in the first place.
document.getElementById('topbar-meta').innerHTML =
  '<span class="count multitail-control">' +
  '<button type="button" id="multitail-toggle" aria-pressed="false">multitail</button>' +
  '</span>';
document.getElementById('multitail-toggle').dispatchEvent(
  new window.MouseEvent('click', { bubbles: true, cancelable: true }));
assert('toggle still works after the header is re-rendered', mt.isOpen() === true);
mt.closeMode();

// `m` typed into a text field must type an m, not hijack the window.
{
  const input = document.createElement('input');
  document.body.appendChild(input);
  input.focus();
  key('m');
  assert('`m` is inert while typing in an input', mt.isOpen() === false);
  input.blur();
  document.body.removeChild(input);
}

// Another dialog owns the keyboard.
{
  const other = document.createElement('div');
  other.setAttribute('data-no-morph', '');
  document.body.appendChild(other);
  key('m');
  assert('`m` is inert while another dialog is open', mt.isOpen() === false);
  document.body.removeChild(other);
}

// ==========================================================================
console.log('\n-- compact formatter');
// ==========================================================================
{
  const f = mt.formatPayload;
  const asst = (blocks) => ({ type: 'assistant', message: { content: blocks } });

  let r = f({ type: 'event', kind: 'assistant_text',
    rec: asst([{ type: 'text', text: 'line one\nline two' }]) });
  assert('assistant text keeps only the first line', r.text === 'line one', r.text);

  r = f({ type: 'event', kind: 'thinking',
    rec: asst([{ type: 'thinking', thinking: 'hmm' }]) });
  assert('thinking is formatted', r.text === 'hmm' && r.cls === 'mt-think', JSON.stringify(r));

  r = f({ type: 'event', kind: 'tool_result', rec: { type: 'user', message: { content: [
    { type: 'tool_result', content: [{ type: 'text', text: 'out\nmore' }] }] } } });
  assert('tool_result keeps the first line of output', r.text === 'out', r.text);

  r = f({ type: 'event', kind: 'tool_result', rec: { type: 'user', message: { content: [
    { type: 'tool_result', is_error: true, content: 'boom' }] } } });
  assert('an errored tool_result is marked', /mt-err/.test(r.cls), r.cls);

  r = f({ type: 'event', kind: 'tool_use',
    rec: asst([{ type: 'tool_use', name: 'Read', input: { file_path: '/a/b.py' } }]) });
  assert('tool arg falls back through the known keys',
    r.text === 'Read /a/b.py', r.text);

  r = f({ type: 'event', kind: 'tool_use',
    rec: asst([{ type: 'tool_use', name: 'Odd', input: { zzz: 1 } }]) });
  assert('an unknown tool input still shows something',
    r.text.indexOf('Odd') === 0 && r.text.length > 3, r.text);

  r = f({ type: 'event', kind: 'someting_new', rec: {} });
  assert('an unrecognised kind is shown, never swallowed',
    r && r.text === 'someting_new', JSON.stringify(r));

  r = f({ type: 'raw', line: 'not json' });
  assert('an unparseable transcript line still renders', r.text === 'not json', r.text);

  assert('formatPayload rejects junk', f(null) === null && f(undefined) === null);
}

// Overlong lines are clipped, not wrapped — a 4000-char tool result would
// otherwise swallow a whole short pane.
{
  mt.openMode();
  const long = 'x'.repeat(mt.MAX_LINE_CHARS + 500);
  streamFor('q-a').emit({ type: 'event', kind: 'workload_line', text: long });
  const body = paneFor('q-a').querySelector('.mt-line .mt-body').textContent;
  assert('an overlong line is clipped', body.length <= mt.MAX_LINE_CHARS + 1,
    'len=' + body.length);
  assert('the clip is marked with an ellipsis', body.endsWith('…'));
  mt.closeMode();
}

// Transcript prose is untrusted: it must never reach the DOM as markup.
{
  mt.openMode();
  streamFor('q-a').emit({
    type: 'event', kind: 'workload_line', text: '<img src=x onerror=alert(1)>',
  });
  const pane = paneFor('q-a');
  assert('markup in a log line is rendered as text',
    pane.querySelector('img') === null);
  assert('the raw text is preserved verbatim',
    pane.querySelector('.mt-line .mt-body').textContent ===
      '<img src=x onerror=alert(1)>');
  mt.closeMode();
}

// ==========================================================================
console.log('\n-- refresh.js parity (the 5s tick rebuilds both of these)');
// ==========================================================================
// #topbar-meta and #queue-root are rebuilt from refresh.js every tick, so a
// control or attribute emitted only by the Jinja template survives the first
// paint and then vanishes. Both sides are asserted against the REAL builder
// rather than by grepping the source.
{
  const morphdomSrc = fs.readFileSync(
    path.join(STATIC_DIR, 'vendor', 'morphdom-2.7.4.min.js'), 'utf8');
  const refreshSrc = fs.readFileSync(path.join(STATIC_DIR, 'refresh.js'), 'utf8');
  const d2 = new JSDOM(
    '<!doctype html><html><head></head><body>' +
    '<div class="meta" id="topbar-meta"></div><main id="queue-root"></main>' +
    '<div id="action-modal" data-no-morph hidden></div>' +
    '<div id="log-modal" data-no-morph hidden></div>' +
    '<section id="multitail" data-no-morph hidden></section>' +
    '</body></html>',
    { runScripts: 'dangerously' },
  );
  const head = d2.window.document.head;
  for (const code of [morphdomSrc, refreshSrc]) {
    const s = d2.window.document.createElement('script');
    s.textContent = code;
    head.appendChild(s);
  }
  const R = d2.window.__queueRefresh;
  assert('__queueRefresh exposed', !!R);

  const meta = R.buildTopbarMetaDOM({ totals: {}, sources: [] });
  const btn = meta.querySelector('#multitail-toggle');
  assert('refresh.js rebuilds the multitail toggle', !!btn);
  assert('rebuilt toggle reflects the CLOSED overlay',
    btn && btn.getAttribute('aria-pressed') === 'false');
  assert('rebuilt toggle sits in a .multitail-control wrapper',
    !!meta.querySelector('.multitail-control #multitail-toggle'));

  // With the overlay open, a rebuild must not flap the button back to
  // unpressed — that is exactly the flicker the density pill's mirror avoids.
  d2.window.document.getElementById('multitail').hidden = false;
  const meta2 = R.buildTopbarMetaDOM({ totals: {}, sources: [] });
  assert('rebuilt toggle reflects the OPEN overlay',
    meta2.querySelector('#multitail-toggle').getAttribute('aria-pressed') === 'true');

  const row = (id, extra) => Object.assign({
    id, summary: 's ' + id, description: '', scope: [], priority: 5,
    created_by: 'main-loop', status: 'running', owner: {}, subagents: [],
    is_starting: false, workload_label: '', hostjob_label: '',
    model: '', model_label: '', age: '1m', age_label: '', age_seconds: 60,
  }, extra || {});
  const root = R.buildQueueDOM({
    running: [
      row('q-live', { live_log_mode: 'live', owner: { agent_id: 'a1', alive: true } }),
      row('q-wl', { live_log_mode: 'workload', workload_label: 'wl' }),
      row('q-none', { live_log_mode: '', is_starting: true }),
    ],
    wedged: [], quarantined: [], blocked: [], pending: [],
    done_recent: [], abandoned_recent: [], other: [],
    totals: { running: 3, pending: 0 }, sources: [],
  });
  const get = (id) => root.querySelector(`.item[data-queue-id="${id}"]`);
  assert('refresh.js emits data-live-log-mode for an agent row',
    get('q-live') && get('q-live').getAttribute('data-live-log-mode') === 'live',
    get('q-live') && get('q-live').outerHTML.slice(0, 200));
  assert('refresh.js emits data-live-log-mode for a workload row',
    get('q-wl') && get('q-wl').getAttribute('data-live-log-mode') === 'workload');
  assert('refresh.js omits it for a row with no log',
    get('q-none') && !get('q-none').hasAttribute('data-live-log-mode'));
  assert('a row with no log is still clickable after the rebuild',
    get('q-none') && get('q-none').classList.contains('log-clickable'));
}

// ==========================================================================
// From here on the suite drives the two display toggles and the stream-retry
// path. Reset the queue list to a known shape first — the sections above
// deliberately mutate it (a row removed, a row gaining a log).
// ==========================================================================
function resetQueue(cards) {
  mt.closeMode();
  document.getElementById('queue-root').innerHTML = cards.join('\n');
}

// ==========================================================================
console.log('\n-- line wrap (`w`)');
// ==========================================================================
{
  resetQueue([card('q-a', 'live', 'agent one'), card('q-b', 'workload', 'wl one')]);
  mt.openMode();

  assert('wrap is OFF by default (the pre-existing behaviour)',
    mt.isWrap() === false);
  assert('the overlay carries no wrap class while off',
    document.getElementById('multitail').classList.contains('mt-wrap') === false);
  assert('the wrap pill reads unpressed',
    document.getElementById('multitail-wrap').getAttribute('aria-pressed') === 'false');
  assert('the wrapped slack is wider than the unwrapped one',
    mt.NEAR_BOTTOM_PX_WRAPPED > mt.NEAR_BOTTOM_PX,
    mt.NEAR_BOTTOM_PX + ' -> ' + mt.NEAR_BOTTOM_PX_WRAPPED);
  assert('the wrapped store bound is larger than the clipped one',
    mt.MAX_LINE_CHARS_WRAPPED > mt.MAX_LINE_CHARS);

  // A line longer than the unwrapped clip, appended while wrap is OFF.
  const long = 'y'.repeat(mt.MAX_LINE_CHARS + 900);
  streamFor('q-a').emit({ type: 'event', kind: 'workload_line', text: long });
  assert('with wrap off a long line is clipped as before',
    bodyOf('q-a').length === mt.MAX_LINE_CHARS + 1 && bodyOf('q-a').endsWith('…'),
    'len=' + bodyOf('q-a').length);

  // THE POINT of keeping records: turning wrap on must change a line that was
  // already rendered, not just future ones. A pane whose job has ended emits
  // nothing more, so a future-lines-only toggle would do nothing at all there.
  mt.setWrap(true);
  assert('turning wrap on re-renders an ALREADY-rendered line in full',
    bodyOf('q-a').length > mt.MAX_LINE_CHARS + 1, 'len=' + bodyOf('q-a').length);
  assert('the wrapped line shows the whole stored text',
    bodyOf('q-a') === long, 'len=' + bodyOf('q-a').length);
  assert('the overlay carries the wrap class so CSS can switch',
    document.getElementById('multitail').classList.contains('mt-wrap') === true);
  assert('the wrap pill reads pressed',
    document.getElementById('multitail-wrap').getAttribute('aria-pressed') === 'true');

  mt.setWrap(false);
  assert('turning wrap off clips that same line again',
    bodyOf('q-a').length === mt.MAX_LINE_CHARS + 1, 'len=' + bodyOf('q-a').length);

  // Storage IS bounded — "show the full line" must not mean "retain an
  // unbounded line", or one 400 KB tool result lives in four panes at once.
  const huge = 'z'.repeat(mt.MAX_LINE_CHARS_WRAPPED + 5000);
  streamFor('q-b').emit({ type: 'event', kind: 'workload_line', text: huge });
  mt.setWrap(true);
  assert('a line past the wrapped bound is still clipped when wrapped',
    bodyOf('q-b').length === mt.MAX_LINE_CHARS_WRAPPED + 1 && bodyOf('q-b').endsWith('…'),
    'len=' + bodyOf('q-b').length);

  // The retained-LINE budget is not a retained-ROW budget: wrapping changes how
  // tall the history renders, never how much of it is kept.
  for (let i = 0; i < mt.MAX_LINES_PER_PANE + 40; i++) {
    streamFor('q-b').emit({ type: 'event', kind: 'workload_line', text: 'w' + i });
  }
  assert('the line budget is unchanged by wrap',
    paneFor('q-b').querySelectorAll('.mt-line').length === mt.MAX_LINES_PER_PANE,
    'kept ' + paneFor('q-b').querySelectorAll('.mt-line').length);
  mt.setWrap(false);
  assert('and unchanged again after wrapping back off',
    paneFor('q-b').querySelectorAll('.mt-line').length === mt.MAX_LINES_PER_PANE);

  // Keyboard + pill.
  key('w');
  assert('`w` turns wrap on', mt.isWrap() === true);
  key('w');
  assert('`w` turns wrap off again', mt.isWrap() === false);
  document.getElementById('multitail-wrap').dispatchEvent(
    new window.MouseEvent('click', { bubbles: true, cancelable: true }));
  assert('the wrap pill toggles too', mt.isWrap() === true);
  mt.setWrap(false);

  // Ctrl+W must still close the tab.
  const ev = key('w', { ctrlKey: true });
  assert('Ctrl+W is passed through, not swallowed',
    mt.isWrap() === false && ev.defaultPrevented === false);

  // Typing an `w` into a field types a w.
  {
    const input = document.createElement('input');
    document.body.appendChild(input);
    input.focus();
    key('w');
    assert('`w` is inert while typing in an input', mt.isWrap() === false);
    input.blur();
    document.body.removeChild(input);
  }
  // Another dialog owns the keyboard.
  {
    const other = document.createElement('div');
    other.setAttribute('data-no-morph', '');
    document.body.appendChild(other);
    key('w');
    assert('`w` is inert while another dialog is open', mt.isWrap() === false);
    document.body.removeChild(other);
  }

  // The preference outlives closing and reopening the mode (it is a display
  // preference for this page, not part of the takeover).
  mt.setWrap(true);
  mt.closeMode();
  mt.openMode();
  assert('wrap survives leaving and re-entering the mode', mt.isWrap() === true);
  mt.setWrap(false);
  mt.closeMode();
}

// ==========================================================================
console.log('\n-- timestamps (`t`): SOURCE time only, never arrival time');
// ==========================================================================
{
  resetQueue([
    card('q-a', 'live', 'agent one'),
    card('q-b', 'workload', 'wl one'),
    card('q-c', 'hostjob', 'hj one'),
  ]);
  mt.openMode();

  const ISO = '2026-09-28T15:28:44.618Z';
  const agentLine = (ts) => ({
    type: 'event',
    kind: 'assistant_text',
    rec: {
      type: 'assistant',
      timestamp: ts,
      message: { content: [{ type: 'text', text: 'hello there' }] },
    },
  });

  assert('timestamps are OFF by default', mt.isTimestamps() === false);
  assert('only the agent-transcript source has per-line timestamps',
    mt.paneHasSourceTimestamps(paneRecord('q-a')) === true &&
    mt.paneHasSourceTimestamps(paneRecord('q-b')) === false &&
    mt.paneHasSourceTimestamps(paneRecord('q-c')) === false);

  streamFor('q-a').emit(agentLine(ISO));
  streamFor('q-b').emit({ type: 'event', kind: 'workload_line', text: 'plain line' });
  assert('no timestamp cell while the column is off',
    paneFor('q-a').querySelector('.mt-ts') === null);
  assert('the no-ts marker is hidden while the column is off',
    paneFor('q-b').querySelector('.mt-pane-nots').hidden === true);

  mt.setTimestamps(true);
  // Retroactive, for the same reason wrap is.
  const tsCell = paneFor('q-a').querySelector('.mt-ts');
  assert('turning timestamps on stamps an ALREADY-rendered agent line',
    tsCell !== null);
  assert('the cell shows the RECORD\'s own time, not "now"',
    tsCell && tsCell.textContent === '15:28:44', tsCell && tsCell.textContent);
  assert('the raw source timestamp is kept as the cell tooltip',
    tsCell && tsCell.title === ISO, tsCell && tsCell.title);

  // The whole judgment call: a plain-text source gets NOTHING, not the
  // browser's arrival time dressed up as the log's own.
  assert('a plain-text workload line gets NO timestamp cell',
    paneFor('q-b').querySelector('.mt-ts') === null);
  assert('its pane explains the empty column instead',
    paneFor('q-b').querySelector('.mt-pane-nots').hidden === false);
  assert('the hostjob pane says the same',
    paneFor('q-c').querySelector('.mt-pane-nots').hidden === false);
  assert('the agent pane does NOT claim to be missing timestamps',
    paneFor('q-a').querySelector('.mt-pane-nots').hidden === true);

  // An agent record with no timestamp of its own (transcript bookkeeping
  // lines carry none) gets no cell either — same rule, applied per record.
  streamFor('q-a').emit(agentLine(undefined));
  assert('an agent record with no timestamp gets no cell',
    paneFor('q-a').querySelectorAll('.mt-line').length === 2 &&
    paneFor('q-a').querySelectorAll('.mt-ts').length === 1,
    paneFor('q-a').querySelectorAll('.mt-ts').length + ' cells');

  mt.setTimestamps(false);
  assert('turning the column off removes the cells again',
    paneFor('q-a').querySelector('.mt-ts') === null);
  assert('and hides the explanation with it',
    paneFor('q-b').querySelector('.mt-pane-nots').hidden === true);

  // Keyboard + pill + the same inertness rules as `w`.
  key('t');
  assert('`t` turns timestamps on', mt.isTimestamps() === true);
  key('t');
  assert('`t` turns them off again', mt.isTimestamps() === false);
  document.getElementById('multitail-ts').dispatchEvent(
    new window.MouseEvent('click', { bubbles: true, cancelable: true }));
  assert('the time pill toggles too', mt.isTimestamps() === true);
  mt.setTimestamps(false);
  const ev = key('t', { metaKey: true });
  assert('Cmd/Ctrl+T is passed through, not swallowed',
    mt.isTimestamps() === false && ev.defaultPrevented === false);
  {
    const input = document.createElement('input');
    document.body.appendChild(input);
    input.focus();
    key('t');
    assert('`t` is inert while typing in an input', mt.isTimestamps() === false);
    input.blur();
    document.body.removeChild(input);
  }
  mt.closeMode();
  key('t');
  assert('`t` is inert while the mode is closed (it is a mode-local control)',
    mt.isTimestamps() === false);
  key('w');
  assert('`w` is inert while the mode is closed', mt.isWrap() === false);
  assert('neither key opened the mode', mt.isOpen() === false);
}

// ==========================================================================
console.log('\n-- ended-pane retention (`c`): clear after a delay, or keep');
// ==========================================================================
{
  const retainBtn = document.getElementById('multitail-retain');
  resetQueue([card('q-a', 'live', 'agent one'), card('q-b', 'workload', 'wl one')]);
  mt.openMode();

  assert('retention defaults to one minute (what was asked for)',
    mt.retention() === '1m' && mt.retentionMs() === 60 * 1000,
    mt.retention() + ' / ' + mt.retentionMs() + 'ms');
  assert('the option set is small and cycles in order',
    mt.RETENTION_OPTIONS.map((o) => o.key).join(',') === '1m,5m,15m,keep',
    mt.RETENTION_OPTIONS.map((o) => o.key).join(','));
  assert('keep-forever is a real option, not a token entry',
    mt.RETENTION_OPTIONS[mt.RETENTION_OPTIONS.length - 1].ms === 0);
  assert('nothing below a minute is offered (a grace period has to be one)',
    mt.RETENTION_OPTIONS.every((o) => o.ms === 0 || o.ms >= 60 * 1000));
  assert('the pill shows the value rather than a pressed state',
    retainBtn.textContent === 'clear 1m' &&
    retainBtn.hasAttribute('aria-pressed') === false,
    retainBtn.outerHTML);
  assert('the pill exposes the value to CSS / assistive tech',
    retainBtn.getAttribute('data-retention') === '1m' &&
    /1 minute/.test(retainBtn.getAttribute('aria-label')),
    retainBtn.getAttribute('aria-label'));

  // An ended pane announces its own clear, so the pane about to disappear is
  // the one telling you — in time to press `c`.
  streamFor('q-b').emit({ type: 'meta', kind: 'workload-end', exit_code: 0 });
  assert('an ended pane keeps its exit code AND gains a countdown',
    /ended · exit 0 · clears in \d+s/.test(statusOf('q-b')), statusOf('q-b'));
  assert('it is not yanked the moment it ends', paneFor('q-b') !== null);
  assert('an ended pane holds no stream slot, so clearing cannot disturb the pump',
    paneRecord('q-b').streaming === false && paneRecord('q-b').es === null);

  // THE TIMING BOUNDARY. Inside the window the pane stays and counts down; at
  // the deadline exactly (>=, not >) it goes.
  const p = paneRecord('q-b');
  p.endedAt = Date.now() - mt.retentionMs() + 5000;
  mt.reconcile();
  assert('a pane still inside its retention window is kept',
    paneFor('q-b') !== null);
  assert('and its countdown tracks the time left',
    /clears in [1-5]s/.test(statusOf('q-b')), statusOf('q-b'));

  p.endedAt = Date.now() - mt.retentionMs();
  mt.reconcile();
  assert('a pane exactly at its deadline is cleared', paneFor('q-b') === null);
  assert('and leaves the pane map with it', mt.panes.has('q-b') === false);
  assert('an auto-clear is NOT recorded as a manual dismissal',
    mt.dismissed.has('q-b') === false);
  assert('the mode stays open and other panes are untouched',
    mt.isOpen() === true && paneFor('q-a') !== null);
  assert('the header count drops the cleared tail',
    /^1 tail/.test(document.getElementById('multitail-count').textContent),
    document.getElementById('multitail-count').textContent);

  // A pane cleared while its ROW is still eligible must not be rebuilt on the
  // next tick — that would be a pane flapping every 2s.
  mt.reconcile();
  assert('a cleared pane is not rebuilt while its row is still eligible',
    paneFor('q-b') === null);
  assert('the suppression is its own set, not the dismissed set',
    mt.cleared.has('q-b') === true && mt.dismissed.has('q-b') === false);

  // ...and the suppression cannot outlive that row's eligibility streak, so a
  // requeued job reusing the qid is NOT permanently suppressed by a stale
  // clear.
  document.getElementById('queue-root').innerHTML = card('q-a', 'live', 'agent one');
  mt.reconcile();
  assert('the suppression is dropped as soon as the row is not eligible',
    mt.cleared.has('q-b') === false);
  document.getElementById('queue-root').innerHTML =
    [card('q-a', 'live', 'agent one'), card('q-b', 'workload', 'wl one again')].join('\n');
  mt.reconcile();
  assert('a requeued qid gets a fresh, live pane',
    paneFor('q-b') !== null && paneRecord('q-b').ended === false &&
    paneRecord('q-b').endedAt === 0);

  // ---- keep forever ----
  mt.setRetention('keep');
  assert('keep means there is no deadline at all', mt.retentionMs() === 0);
  assert('the pill says so', retainBtn.textContent === 'keep' &&
    retainBtn.getAttribute('data-retention') === 'keep');
  streamFor('q-b').emit({ type: 'meta', kind: 'workload-end', exit_code: 2 });
  const p2 = paneRecord('q-b');
  p2.endedAt = Date.now() - 24 * 60 * 60 * 1000;
  mt.reconcile();
  assert('with keep, a pane ended a day ago is STILL there',
    paneFor('q-b') !== null);
  assert('and its status carries no countdown to contradict that',
    /ended/.test(statusOf('q-b')) && /clears in/.test(statusOf('q-b')) === false,
    statusOf('q-b'));

  // Switching to a timed value applies at once rather than on the next tick —
  // otherwise `keep` -> `1m` would look like it did nothing for two seconds.
  mt.setRetention('1m');
  assert('switching off keep clears what is already overdue, immediately',
    paneFor('q-b') === null);

  // ---- the control: cycle order, pill, key ----
  mt.setRetention('1m');
  const seenOrder = [];
  for (let i = 0; i < 5; i++) {
    seenOrder.push(mt.retention());
    mt.cycleRetention();
  }
  assert('cycling walks the options and wraps around',
    seenOrder.join(',') === '1m,5m,15m,keep,1m', seenOrder.join(','));

  mt.setRetention('1m');
  retainBtn.dispatchEvent(
    new window.MouseEvent('click', { bubbles: true, cancelable: true }));
  assert('the pill cycles on click', mt.retention() === '5m', mt.retention());
  assert('and its label follows', retainBtn.textContent === 'clear 5m');
  key('c');
  assert('`c` cycles too', mt.retention() === '15m', mt.retention());

  const ev = key('c', { ctrlKey: true });
  assert('Ctrl/Cmd+C is passed through so copying log text still works',
    mt.retention() === '15m' && ev.defaultPrevented === false);
  {
    const input = document.createElement('input');
    document.body.appendChild(input);
    input.focus();
    key('c');
    assert('`c` is inert while typing in an input', mt.retention() === '15m');
    input.blur();
    document.body.removeChild(input);
  }
  {
    const other = document.createElement('div');
    other.setAttribute('data-no-morph', '');
    document.body.appendChild(other);
    key('c');
    assert('`c` is inert while another dialog owns the keyboard',
      mt.retention() === '15m');
    document.body.removeChild(other);
  }
  // The choice outlives the mode (it is a preference, not part of the takeover).
  mt.closeMode();
  key('c');
  assert('`c` is inert while the mode is closed (a mode-local control)',
    mt.retention() === '15m' && mt.isOpen() === false);
  mt.openMode();
  assert('the retention choice survives leaving and re-entering the mode',
    mt.retention() === '15m');
  mt.setRetention('1m');
  mt.closeMode();

  // An unknown value can only come from storage or a caller mistake; either
  // way it resolves to the default rather than to a state with no delay.
  mt.setRetention('nonsense');
  assert('an unrecognised retention key falls back to the default',
    mt.retention() === mt.DEFAULT_RETENTION_KEY);
}

// ==========================================================================
console.log('\n-- retention persists per viewer, and works without storage');
// ==========================================================================
// The mode itself is deliberately NOT persisted; the retention choice is,
// because it is a policy about how much finished output survives rather than a
// projection of the current page. localStorage can throw (private mode, blocked
// site data) or come back empty, so a fresh boot is driven three ways.
{
  function boot(seed) {
    const d = new JSDOM(initialHTML, {
      runScripts: 'outside-only',
      url: 'https://queue.example/',
    });
    d.window.EventSource = class { constructor(u) { this.url = u; }
      close() {} };
    if (typeof seed === 'function') seed(d.window);
    d.window.eval(src);
    return d;
  }

  let d = boot((w) => w.localStorage.setItem('qsite_mt_retain', 'keep'));
  assert('a stored choice is restored on a fresh page load',
    d.window.__multitail.retention() === 'keep',
    d.window.__multitail.retention());
  assert('and the server-rendered pill label is corrected before first open',
    d.window.document.getElementById('multitail-retain').textContent === 'keep',
    d.window.document.getElementById('multitail-retain').textContent);

  d = boot((w) => w.localStorage.setItem('qsite_mt_retain', 'eventually'));
  assert('a stored value the build does not recognise means the default',
    d.window.__multitail.retention() === '1m', d.window.__multitail.retention());

  d = boot();
  assert('an empty store means the default',
    d.window.__multitail.retention() === '1m');
  d.window.__multitail.setRetention('5m');
  assert('choosing a value writes it through for the next load',
    d.window.localStorage.getItem('qsite_mt_retain') === '5m',
    String(d.window.localStorage.getItem('qsite_mt_retain')));
  assert('the storage key is the documented one',
    d.window.__multitail.RETENTION_STORAGE_KEY === 'qsite_mt_retain');

  // Storage that THROWS on every access must not take the module down with it.
  d = boot((w) => {
    Object.defineProperty(w, 'localStorage', {
      configurable: true,
      get() { throw new Error('site data blocked'); },
    });
  });
  assert('the module still loads when localStorage throws',
    !!d.window.__multitail && d.window.__multitail.retention() === '1m');
  d.window.__multitail.setRetention('keep');
  assert('and the choice still applies to this page',
    d.window.__multitail.retention() === 'keep' &&
    d.window.__multitail.retentionMs() === 0);
}

// ==========================================================================
console.log('\n-- a log that does not exist YET is retried, not written off');
// ==========================================================================
// The reported bug: a just-started job's row becomes eligible BEFORE its log
// file exists (a workload row is eligible as soon as its scope is on the queue
// record; an agent row as soon as an owner record names an agent_id). The
// server answers with a one-shot error frame and closes. Treating that as
// terminal left the pane permanently blank — only toggling the whole mode off
// and on rebuilt it.
{
  assert('the retryable set names exactly the "not there yet" errors',
    ['no-agent', 'no-jsonl', 'open-failed', 'read-failed']
      .every((k) => mt.RETRYABLE_ERROR_KINDS[k] === true) &&
    Object.keys(mt.RETRYABLE_ERROR_KINDS).length === 4,
    Object.keys(mt.RETRYABLE_ERROR_KINDS).join(','));

  assert('the backoff grows and then plateaus',
    mt.retryDelayMs(1) === mt.STREAM_RETRY_BASE_MS &&
    mt.retryDelayMs(2) === mt.STREAM_RETRY_BASE_MS * 2 &&
    mt.retryDelayMs(3) === mt.STREAM_RETRY_BASE_MS * 4 &&
    mt.retryDelayMs(9) === mt.STREAM_RETRY_MAX_MS &&
    mt.retryDelayMs(9999) === mt.STREAM_RETRY_MAX_MS,
    [1, 2, 3, 9, 9999].map(mt.retryDelayMs).join(','));

  resetQueue([card('q-new', 'workload', 'just started')]);
  mt.openMode();
  const first = latestStreamFor('q-new');
  assert('the new row got a pane and a stream', paneFor('q-new') !== null && !!first);

  // The workload/hostjob tails emit stream-start BEFORE they try to open the
  // log file, so the failure arrives AFTER a stream-start. That ordering is
  // exactly what made the naive "suppress backfill on the 2nd stream-start"
  // rule dangerous here.
  first.emit({ type: 'meta', kind: 'stream-start', mode: 'workload' });
  first.emit({
    type: 'error',
    kind: 'open-failed',
    error: "[Errno 2] No such file or directory: '/w/just-started.output'",
  });

  const p = paneRecord('q-new');
  assert('a missing log does NOT make the pane terminal', p.terminal === false);
  assert('the pane says it is waiting for the log, with a countdown',
    /waiting for log · retry \d+s/.test(statusOf('q-new')), statusOf('q-new'));
  assert('the failed connection was closed', first.closed === true);
  assert('the pane is NOT holding a stream slot while it backs off',
    p.streaming === false && p.es === null);
  assert('a pane in backoff does not want a slot yet', mt.wantsSlot(p) === false);
  assert('the overlay count surfaces the wait',
    /waiting for a log/.test(document.getElementById('multitail-count').textContent),
    document.getElementById('multitail-count').textContent);
  const notes = paneFor('q-new').querySelectorAll('.mt-line.mt-note').length;
  assert('the reason is noted in the pane once', notes === 1, notes + ' notes');

  // Fire the backoff. (pumpSlots is the retry clock, driven by the existing
  // 2s reconcile tick in the real page; here we just move the deadline.)
  const before = streamCountFor('q-new');
  p.retryAt = Date.now() - 1;
  assert('once the backoff expires the pane wants a slot again',
    mt.wantsSlot(p) === true);
  mt.pumpSlots();
  assert('the pane reconnected on its own',
    streamCountFor('q-new') === before + 1,
    before + ' -> ' + streamCountFor('q-new'));

  // A SECOND failure must back off further, and must not stack notes.
  const second = latestStreamFor('q-new');
  second.emit({ type: 'meta', kind: 'stream-start', mode: 'workload' });
  second.emit({ type: 'error', kind: 'open-failed', error: 'still missing' });
  assert('the second failure waits longer than the first',
    p.retryAt - Date.now() > mt.STREAM_RETRY_BASE_MS,
    'in ' + (p.retryAt - Date.now()) + 'ms');
  assert('the note is not repeated per attempt',
    paneFor('q-new').querySelectorAll('.mt-line.mt-note').length === notes);

  // THE REGRESSION GUARD. The retry has now seen three stream-starts. If
  // backfill suppression keyed on "seen a stream-start before" the pane would
  // silently drop the first content it ever receives.
  p.retryAt = Date.now() - 1;
  mt.pumpSlots();
  const third = latestStreamFor('q-new');
  third.emit({ type: 'meta', kind: 'stream-start', mode: 'workload' });
  third.emit({ type: 'meta', kind: 'backfill-begin', lines: 2 });
  third.emit({ type: 'event', kind: 'workload_line', text: 'stv-promote: starting' });
  third.emit({ type: 'event', kind: 'workload_line', text: 'stv-promote: 1 of 3' });
  third.emit({ type: 'meta', kind: 'backfill-end' });
  const texts = Array.from(paneFor('q-new').querySelectorAll('.mt-line .mt-body'))
    .map((n) => n.textContent);
  assert('the backfill the retry finally got is RENDERED, not suppressed',
    texts.indexOf('stv-promote: starting') !== -1 &&
    texts.indexOf('stv-promote: 1 of 3') !== -1,
    texts.join(' | '));
  assert('the pane reads live once real output lands', statusOf('q-new') === 'live',
    statusOf('q-new'));
  assert('a successful attempt clears the backoff', p.retryAt === 0);

  // ...and the already-fixed bug stays fixed: NOW that data has been shown, a
  // recycled stream's replayed backfill IS suppressed.
  const n1 = paneFor('q-new').querySelectorAll('.mt-line').length;
  third.emit({ type: 'meta', kind: 'stream-start', mode: 'workload' });
  third.emit({ type: 'meta', kind: 'backfill-begin', lines: 2 });
  third.emit({ type: 'event', kind: 'workload_line', text: 'stv-promote: starting' });
  third.emit({ type: 'meta', kind: 'backfill-end' });
  assert('a replayed backfill is still suppressed once data has been shown',
    paneFor('q-new').querySelectorAll('.mt-line').length === n1);
  mt.closeMode();
}

// ==========================================================================
console.log('\n-- the other two "nothing is showing" states are NOT retries');
// ==========================================================================
{
  resetQueue([card('q-empty', 'live', 'log exists, empty')]);
  mt.openMode();
  const s = latestStreamFor('q-empty');
  s.emit({ type: 'meta', kind: 'stream-start', path: '/x.jsonl' });
  const p = paneRecord('q-empty');
  assert('an existing-but-empty log reads as connected, not as an error',
    statusOf('q-empty') === 'live · no output yet', statusOf('q-empty'));
  assert('and is not in a retry backoff', p.retryAt === 0 && p.terminal === false);
  s.emit({ type: 'meta', kind: 'idle-timeout', idle_seconds: 30 });
  assert('the server recycling an empty stream is not an error either',
    statusOf('q-empty') === 'idle · no output yet', statusOf('q-empty'));
  assert('still not terminal — EventSource reconnects on its own',
    p.terminal === false);

  // An error shape we cannot reason about is still terminal, and named.
  s.emit({ type: 'error', kind: 'schema-drift', error: 'unexpected frame' });
  assert('an unrecognised error kind stays terminal', p.terminal === true);
  assert('and is named in the status', statusOf('q-empty') === 'schema-drift',
    statusOf('q-empty'));
  mt.closeMode();
}

// ==========================================================================
console.log('\n-- retries lose to `ended`, and do not fight the slot cap');
// ==========================================================================
{
  // A job that finished before its log ever appeared must settle, not retry
  // forever.
  resetQueue([card('q-gone', 'workload', 'finished before logging')]);
  mt.openMode();
  latestStreamFor('q-gone').emit({ type: 'error', kind: 'open-failed', error: 'nope' });
  const p = paneRecord('q-gone');
  assert('the pane is retrying', p.retryAt > 0 && p.terminal === false);
  document.getElementById('queue-root').innerHTML = '';
  mt.reconcile();
  assert('an item that stopped running is marked ended', p.ended === true);
  assert('ended is terminal, so the retry stops', p.terminal === true);
  assert('and no expired backoff can revive it',
    mt.wantsSlot(p) === false && p.retryAt === 0);
  const n = streamCountFor('q-gone');
  p.retryAt = Date.now() - 1;
  mt.pumpSlots();
  assert('pumpSlots opens no further stream for an ended pane',
    streamCountFor('q-gone') === n, n + ' -> ' + streamCountFor('q-gone'));
  mt.closeMode();

  // A pane in backoff releases its slot, so the cap is spent on panes that can
  // actually use it — and the freed slot goes to the next waiter immediately.
  resetQueue([
    card('q-1', 'workload', 'one'), card('q-2', 'live', 'two'),
    card('q-3', 'live', 'three'), card('q-4', 'live', 'four'),
    card('q-5', 'live', 'five'),
  ]);
  mt.openMode();
  assert('five panes, four streams', paneEls().length === 5 &&
    openStreams().length === mt.MAX_LIVE_STREAMS, 'open=' + openStreams().length);
  assert('the fifth is waiting for a SLOT (a distinct state from a backoff)',
    /waiting for a stream slot/.test(statusOf('q-5')), statusOf('q-5'));
  latestStreamFor('q-1').emit({ type: 'error', kind: 'open-failed', error: 'nope' });
  assert('the backing-off pane handed its slot to the waiter',
    streamFor('q-5') !== undefined);
  assert('still exactly at the cap',
    openStreams().length === mt.MAX_LIVE_STREAMS, 'open=' + openStreams().length);
  assert('and the backing-off pane is labelled as such, not as slot-starved',
    /waiting for log/.test(statusOf('q-1')), statusOf('q-1'));
  mt.closeMode();
}

console.log(
  failures === 0
    ? '\nAll multitail tests passed.'
    : `\n${failures} multitail test(s) FAILED.`,
);
process.exit(failures === 0 ? 0 : 1);
