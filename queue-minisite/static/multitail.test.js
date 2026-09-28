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
  <section id="multitail" data-no-morph hidden>
    <header class="multitail-head">
      <h2 id="multitail-title">multitail</h2>
      <span id="multitail-count"></span>
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
function key(k) {
  const ev = new window.KeyboardEvent('keydown', {
    key: k, bubbles: true, cancelable: true,
  });
  document.dispatchEvent(ev);
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
assert('pane reads live after stream-start', statusOf('q-a') === 'live');

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
  // Second stream-start = the server recycled the stream (idle cap) and is
  // replaying its tail. Plain-text tails carry no resume cursor, so without
  // suppression a quiet job re-prints its whole backfill every 30s.
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

console.log(
  failures === 0
    ? '\nAll multitail tests passed.'
    : `\n${failures} multitail test(s) FAILED.`,
);
process.exit(failures === 0 ? 0 : 1);
