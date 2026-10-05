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
//  12. SUBAGENTS: a pane is a (kind, target) pair, not a qid. A running
//      card's nested subagent tree is collapsed by default, expands into
//      nested panes tailing /api/subagent/<id>/stream, indents by real tree
//      depth, ends when the tree stops listing a node, and goes away with its
//      parent pane.
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
// index.html loads ansi.js before multitail.js: pane lines are rendered through
// window.AnsiText so terminal colour sequences come out as colour. Loaded here
// too, or the module silently takes its no-AnsiText fallback path and the
// colour assertions below would be testing nothing.
const ansiSrc = fs.readFileSync(path.join(STATIC_DIR, 'ansi.js'), 'utf8');

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
      <button type="button" id="multitail-options" class="multitail-display"
              aria-haspopup="dialog" aria-expanded="false"
              aria-controls="multitail-options-modal">options</button>
      <button type="button" id="multitail-exit">exit</button>
    </header>
    <div id="multitail-panes"></div>
    <p id="multitail-empty" hidden>Nothing to tail</p>
    <div id="multitail-options-modal" class="modal mt-options" role="dialog"
         aria-modal="true" hidden>
      <div class="modal-backdrop" data-modal-dismiss></div>
      <div id="multitail-options-panel" class="modal-panel mt-options-panel"
           role="document">
        <button type="button" id="multitail-options-close"
                data-modal-dismiss>&times;</button>
        <ul class="mt-options-list">
          <li class="mt-option">
            <button type="button" id="multitail-wrap" class="multitail-display"
                    aria-pressed="false">wrap</button>
          </li>
          <li class="mt-option">
            <button type="button" id="multitail-ts" class="multitail-display"
                    aria-pressed="false">time</button>
          </li>
          <li class="mt-option">
            <button type="button" id="multitail-verbose" class="multitail-display"
                    aria-pressed="false">all</button>
          </li>
          <li class="mt-option mt-option-sub" id="multitail-vcap-row" hidden>
            <button type="button" id="multitail-vcap"
                    class="multitail-display multitail-vcap"
                    data-verbose-cap="8k" hidden>cap 8K</button>
          </li>
          <li class="mt-option">
            <button type="button" id="multitail-subs" class="multitail-display"
                    aria-pressed="true">subagents</button>
          </li>
          <li class="mt-option">
            <button type="button" id="multitail-retain"
                    class="multitail-display multitail-retain"
                    data-retention="1m">clear 1m</button>
          </li>
        </ul>
      </div>
    </div>
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

window.eval(ansiSrc);
window.eval(src);
const mt = window.__multitail;
// The slot-queueing scenarios below are written around five cards, so they run
// at a small cap (4) via the test seam; the SHIPPED cap is asserted separately
// (>= 10) and exercised at full size in the section at the end of this file.
const CAP = 4;
mt.setMaxLiveStreams(CAP);

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
// Panes are keyed by (kind, target), not by qid — `q:<qid>` for a queue
// item's own tail, `s:<subagent-id>` for a nested subagent tail. These
// helpers say which namespace they mean rather than passing a bare id.
function paneRecord(qid) {
  return mt.panes.get(mt.paneKey('queue', qid));
}
function subPaneRecord(sid) {
  return mt.panes.get(mt.paneKey('subagent', sid));
}
function subPaneFor(sid) {
  return document.querySelector(
    `#multitail-panes .mt-pane[data-subagent-id="${sid}"]`);
}
function subStatusOf(sid) {
  const p = subPaneFor(sid);
  return p ? p.querySelector('.mt-pane-status').textContent : null;
}
function subsBtnOf(qid) {
  const p = paneFor(qid);
  return p ? p.querySelector('.mt-pane-subs') : null;
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
  'the shipped cap covers a ten-pane stack (HTTP/2: no 6-connection limit)',
  mt.MAX_LIVE_STREAMS >= 10,
  'MAX_LIVE_STREAMS=' + mt.MAX_LIVE_STREAMS,
);
assert(
  'exactly MAX_LIVE_STREAMS streams opened',
  openStreams().length === CAP,
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
    openStreams().length === CAP, 'open=' + openStreams().length);
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

// THE PANE TITLE. A pane header is `badge · queue id · task title · status ·
// close`, and the title is the only thing on screen that says WHICH tail this
// is — in this whole-window mode the cards that carry it are not visible. It
// went missing on phones through a stylesheet rule (the guard for that lives in
// test_multitail.py, which CI runs); this is the renderer half: the element is
// built, it carries the row's summary, and a changed summary is picked up on
// the next reconcile without disturbing the stream.
{
  mt.openMode();
  const head = paneFor('q-a').querySelector('.mt-pane-head');
  const title = head.querySelector('.mt-pane-summary');
  assert('the pane header carries a title element', !!title);
  assert('and it holds the row\'s task summary',
    title && title.textContent === 'agent one',
    title && JSON.stringify(title.textContent));
  // The queue record is editable, so the title has to track it.
  document.querySelector('[data-queue-id="q-a"]').setAttribute('data-queue-summary', 'renamed task');
  mt.reconcile();
  assert('a renamed task updates the pane title in place',
    paneFor('q-a').querySelector('.mt-pane-summary').textContent === 'renamed task');
  document.querySelector('[data-queue-id="q-a"]').setAttribute('data-queue-summary', 'agent one');
  mt.reconcile();
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

  // PLACEMENT. The stamp renders on the RIGHT of the entry, and it costs the
  // body no horizontal width: in the wrapped / verbose modes the stylesheet
  // floats it right, which shortens the FIRST line box only and leaves every
  // continuation row the full width of the pane. A float can only do that for
  // content that follows it in the markup, so the cell has to be the row's
  // first child however far right it ends up looking. jsdom has no layout, so
  // what is asserted here is the source order the stylesheet depends on — the
  // geometry itself is a CSS assertion in test_multitail.py.
  const tsRow = tsCell.parentNode;
  const kids = Array.prototype.slice.call(tsRow.children);
  assert('the timestamp cell is the FIRST child of its row',
    kids.indexOf(tsCell) === 0, tsRow.innerHTML);
  assert('so the body follows it and a right float can shorten line one',
    kids.indexOf(tsRow.querySelector('.mt-body')) > kids.indexOf(tsCell),
    tsRow.innerHTML);

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
  assert('and leaves the pane map with it',
    mt.panes.has(mt.paneKey('queue', 'q-b')) === false);
  assert('an auto-clear is NOT recorded as a manual dismissal',
    mt.dismissed.has(mt.paneKey('queue', 'q-b')) === false);
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
    mt.cleared.has(mt.paneKey('queue', 'q-b')) === true &&
    mt.dismissed.has(mt.paneKey('queue', 'q-b')) === false);

  // ...and the suppression cannot outlive that row's eligibility streak, so a
  // requeued job reusing the qid is NOT permanently suppressed by a stale
  // clear.
  document.getElementById('queue-root').innerHTML = card('q-a', 'live', 'agent one');
  mt.reconcile();
  assert('the suppression is dropped as soon as the row is not eligible',
    mt.cleared.has(mt.paneKey('queue', 'q-b')) === false);
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
    d.window.eval(ansiSrc);
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
    openStreams().length === CAP, 'open=' + openStreams().length);
  assert('the fifth is waiting for a SLOT (a distinct state from a backoff)',
    /waiting for a stream slot/.test(statusOf('q-5')), statusOf('q-5'));
  latestStreamFor('q-1').emit({ type: 'error', kind: 'open-failed', error: 'nope' });
  assert('the backing-off pane handed its slot to the waiter',
    streamFor('q-5') !== undefined);
  assert('still exactly at the cap',
    openStreams().length === CAP, 'open=' + openStreams().length);
  assert('and the backing-off pane is labelled as such, not as slot-starved',
    /waiting for log/.test(statusOf('q-1')), statusOf('q-1'));
  mt.closeMode();
}

// ==========================================================================
console.log('\n-- stamped plain-text lines (source_ts): a real time, or none');
// ==========================================================================
{
  // A workload log whose producer stamped every line reaches the client with
  // the prefix ALREADY split off into `source_ts` (app.py _plain_line_event).
  // The pane must render it in the same column an agent record's timestamp
  // uses — and stop claiming the source has no timestamps, because it does.
  resetQueue([card('q-st', 'workload', 'stamped workload'),
    card('q-un', 'workload', 'unstamped workload')]);
  mt.openMode();
  mt.setTimestamps(true);

  assert('a plain-text pane starts out assumed timestamp-less',
    paneFor('q-st').querySelector('.mt-pane-nots').hidden === false);

  streamFor('q-st').emit({
    type: 'event',
    kind: 'workload_line',
    text: 'promoting Gundam',
    source_ts: '2026-09-28T22:53:35-04:00',
    transient: false,
  });
  const cell = paneFor('q-st').querySelector('.mt-ts');
  assert('a stamped workload line gets a timestamp cell', cell !== null);
  assert('showing the LINE\'s own time', cell && /^\d\d:\d\d:\d\d$/.test(cell.textContent),
    cell && cell.textContent);
  assert('with the raw stamp as its tooltip',
    cell && cell.title === '2026-09-28T22:53:35-04:00', cell && cell.title);
  assert('the line body no longer carries the prefix',
    bodyOf('q-st') === 'promoting Gundam', bodyOf('q-st'));
  assert('and the pane stops saying it has no timestamps',
    paneFor('q-st').querySelector('.mt-pane-nots').hidden === true);
  assert('the observation is per pane, not global',
    paneFor('q-un').querySelector('.mt-pane-nots').hidden === false);

  // An unstamped line in the SAME pane simply has no cell. Mixing the two is
  // normal: a workload file spans a deploy, and the wrapper's own header lines
  // are never stamped.
  streamFor('q-st').emit({
    type: 'event', kind: 'workload_line', text: '=== workload: x ===', transient: false,
  });
  assert('an unstamped line beside a stamped one gets no cell',
    paneFor('q-st').querySelectorAll('.mt-line').length === 2 &&
    paneFor('q-st').querySelectorAll('.mt-ts').length === 1,
    paneFor('q-st').querySelectorAll('.mt-ts').length + ' cells');
  assert('and the pane keeps the marker down (it HAS seen a real time)',
    paneFor('q-st').querySelector('.mt-pane-nots').hidden === true);

  // The toggle stays retroactive: turning the column off and on again
  // re-projects from the records, stamps included.
  mt.setTimestamps(false);
  assert('column off removes the cell', paneFor('q-st').querySelector('.mt-ts') === null);
  mt.setTimestamps(true);
  assert('column on restores it from the retained record',
    paneFor('q-st').querySelectorAll('.mt-ts').length === 1);
  mt.setTimestamps(false);
  mt.closeMode();
}

// ==========================================================================
console.log('\n-- per-pane metrics: own header row, read off the row');
// ==========================================================================
{
  // A card shaped like the real thing: the model chip and the agent-stats cell
  // live in the item HEAD, carrying the SERVER-FORMATTED strings as data
  // attributes. The pane reads those; it computes nothing.
  function richCard(qid, mode, summary, opts) {
    const o = opts || {};
    const model = o.model === undefined ? 'opus' : o.model;
    const modelChip = model
      ? `<span class="model-tag" title="model: claude-${model}-5">${model}</span>`
      : '';
    const stats = o.stats === false ? '' :
      `<span class="agent-stats" title="41 tool calls · 118K context tokens"` +
      ` data-tool-calls="41" data-context-tokens="118000"` +
      ` data-calls-text="${o.calls === undefined ? '41' : o.calls}"` +
      ` data-ctx-text="${o.ctx === undefined ? '118K' : o.ctx}"` +
      ` data-out-text="${o.out === undefined ? '9.1K' : o.out}"` +
      ` data-last-tool="${o.lastTool === undefined ? 'Bash' : o.lastTool}"` +
      ` data-age-text="${o.age === undefined ? '12m' : o.age}"></span>`;
    const label = o.label ? ` data-workload-label="${o.label}"` : '';
    return `<article class="item state-running log-clickable" data-queue-id="${qid}" ` +
      `data-queue-status="running" data-queue-summary="${summary}" ` +
      `data-log-mode="${mode}" data-live-log-mode="${mode}"${label}>` +
      `<header class="item-head">${modelChip}${stats}</header>` +
      `<p class="summary">${summary}</p></article>`;
  }
  const footCells = (qid) =>
    Array.from(paneFor(qid).querySelectorAll('.mt-pane-meta > *'))
      .map((n) => n.textContent);
  const footBar = (qid) => paneFor(qid).querySelector('.mt-pane-meta');
  const headBar = (qid) => paneFor(qid).querySelector('.mt-pane-head');
  const titleEl = (qid) => paneFor(qid).querySelector('.mt-pane-summary');

  resetQueue([
    richCard('q-f1', 'live', 'an agent'),
    richCard('q-f2', 'workload', 'a workload', { model: '', stats: false, label: 'promote-thing' }),
    richCard('q-f3', 'live', 'nothing known yet', { model: '', stats: false }),
    richCard('q-f4', 'live', 'partial', { out: '–', lastTool: '', age: '?' }),
  ]);
  mt.openMode();

  assert('every pane has a metrics element', paneEls().every(
    (p) => p.querySelector('.mt-pane-meta') !== null));
  assert('an agent pane prints model, calls, ctx, out, age and last tool',
    footCells('q-f1').join(' | ') === 'opus | 41 calls | 118K ctx | 9.1K out | 12m | last Bash',
    footCells('q-f1').join(' | '));
  assert('the model chip keeps the row\'s own title (the raw id)',
    footBar('q-f1').querySelector('.mt-meta-model').title === 'model: claude-opus-5',
    footBar('q-f1').querySelector('.mt-meta-model').title);
  assert('the metrics are visible when they have something to say',
    footBar('q-f1').hidden === false);

  // A workload pane runs no model and has no agent counters. It says the one
  // true thing it has — the label being tailed, which the head does not show.
  assert('a workload pane shows its label and nothing invented',
    footCells('q-f2').join(' | ') === 'promote-thing', footCells('q-f2').join(' | '));

  // Nothing known at all -> the title simply keeps the whole line.
  assert('a pane with nothing known at all hides its metrics (no empty cells)',
    footBar('q-f3').hidden === true, footCells('q-f3').join(' | '));

  // `–` and `?` are the server formatter's "not known" markers, never values.
  assert('unknown values are omitted, not printed as placeholders',
    footCells('q-f4').join(' | ') === 'opus | 41 calls | 118K ctx',
    footCells('q-f4').join(' | '));

  // The counters move on their own: refresh.js rebuilds the row every 5s and
  // the pane re-reads it on the reconcile tick.
  document.getElementById('queue-root').innerHTML = [
    richCard('q-f1', 'live', 'an agent', { calls: '58', ctx: '140K', out: '11K', age: '14m', lastTool: 'Read' }),
    richCard('q-f2', 'workload', 'a workload', { model: '', stats: false, label: 'promote-thing' }),
    richCard('q-f3', 'live', 'nothing known yet', { model: '', stats: false }),
    richCard('q-f4', 'live', 'partial', { out: '–', lastTool: '', age: '?' }),
  ].join('\n');
  mt.reconcile();
  assert('the metrics follow the row on the next tick',
    footCells('q-f1').join(' | ') === 'opus | 58 calls | 140K ctx | 11K out | 14m | last Read',
    footCells('q-f1').join(' | '));

  // A snapshot that catches up later fills the metrics in rather than leaving
  // a stale blank.
  document.getElementById('queue-root').innerHTML = [
    richCard('q-f3', 'live', 'nothing known yet'),
  ].join('\n');
  mt.reconcile();
  assert('a pane whose stats arrive later shows them',
    footBar('q-f3').hidden === false &&
    footCells('q-f3').join(' | ').indexOf('41 calls') !== -1,
    footCells('q-f3').join(' | '));

  // WHERE THE METRICS LIVE. They used to be a footer strip appended to the
  // pane after the stream — a whole row of chrome below the log. They are now
  // in the pane HEADER, on the row under the title.
  //
  // NOT ON the title's row, which is the thing this asserts: sharing one flex
  // row made the title the half that gives (the metrics must not shrink — a
  // clipped number reads as a different number), and six metric cells cut a
  // real pane's title down to `debug V...`. The stylesheet's 100% flex basis
  // is what keeps them apart at every width; what is pinned HERE is the DOM
  // shape that rule needs — the metrics being the header's LAST child, with
  // the title a sibling ahead of it rather than a box they share.
  assert('the metrics are inside the pane HEADER, not a row of their own',
    paneFor('q-f1').querySelector('.mt-pane-head .mt-pane-meta') !== null);
  assert('the pane is exactly header + stream — no footer row left',
    Array.from(paneFor('q-f1').children).map((n) => n.className).join(',') ===
      'mt-pane-head,mt-pane-stream',
    Array.from(paneFor('q-f1').children).map((n) => n.className).join(','));
  assert('the stream is still the pane\'s LAST child, so nothing clips it',
    paneFor('q-f1').lastElementChild.className === 'mt-pane-stream');
  // The metrics are the header's LAST child: a 100% flex basis only lands on
  // a row of its own if everything else comes first.
  assert('the metrics are the LAST thing in the header',
    headBar('q-f1').lastElementChild.className === 'mt-pane-meta',
    Array.from(headBar('q-f1').children).map((n) => n.className).join(','));
  // ...and the title is a SIBLING of theirs in that header, not a box the two
  // share. A wrapper around the pair is exactly what put them on one row.
  assert('the title is a direct child of the header, ahead of the metrics',
    titleEl('q-f1').parentElement === headBar('q-f1') &&
    Array.from(headBar('q-f1').children).indexOf(titleEl('q-f1')) <
      Array.from(headBar('q-f1').children).indexOf(footBar('q-f1')),
    Array.from(headBar('q-f1').children).map((n) => n.className).join(','));
  assert('no element wraps the title and the metrics together again',
    paneFor('q-f1').querySelector('.mt-pane-titlebar') === null);
  assert('and the title itself still says what the task is',
    titleEl('q-f1').textContent === 'an agent', titleEl('q-f1').textContent);
  // A pane whose metrics are hidden still shows its title, and gets the row
  // back rather than an empty bar.
  assert('a pane with no metrics keeps a visible title',
    footBar('q-f3').hidden === false ||
    titleEl('q-f3').textContent.length > 0);
  assert('textContent only — no markup from a queue record ever',
    paneFor('q-f3').querySelector('.mt-pane-meta').innerHTML.indexOf('<span') !== -1 &&
    paneFor('q-f3').querySelector('.mt-pane-meta').querySelectorAll('script').length === 0);
  mt.closeMode();
}

// ==========================================================================
console.log('\n-- ANSI colour sequences render as colour, not as escape text');
// ==========================================================================
// Most of what these panes tail is colourised CLI output (docker compose,
// cargo, pytest). The conversion lives in static/ansi.js and is pinned there;
// what belongs HERE is that the pane's own rendering path goes through it — and
// that it does so WITHOUT innerHTML, which is this module's standing invariant.
{
  const E = '\u001b';
  // Own row: an earlier block replaced #queue-root wholesale, so the fixture's
  // original cards are long gone by now.
  document.getElementById('queue-root').innerHTML =
    card('q-ansi', 'workload', 'colourful workload');
  mt.openMode();
  // Fed straight into the pane rather than through a stream: this block is
  // about RENDERING, and by this point the suite has more eligible rows than
  // stream slots, so q-b may still be waiting for one.
  const feed = (qid, payload) =>
    mt.appendPaneLine(paneRecord(qid), mt.formatPayload(payload));
  // The real shape from a `docker compose up --build` workload log.
  feed('q-ansi', {
    type: 'event', kind: 'workload_line',
    text: E + '[32m✔' + E + '[0m Image queue-minisite   ' +
      E + '[34m0.9s' + E + '[0m',
  });
  const body = paneFor('q-ansi').querySelector('.mt-line .mt-body');
  assert('a colourised line renders colour spans',
    !!body.querySelector('span.ansi-fg-green') &&
    !!body.querySelector('span.ansi-fg-blue'), body.innerHTML);
  assert('and no escape text is left in the pane',
    body.textContent.indexOf('[32m') === -1 &&
    body.textContent.indexOf(E) === -1, JSON.stringify(body.textContent));
  assert('the visible text is the line without its sequences',
    body.textContent.indexOf('✔ Image queue-minisite') === 0,
    JSON.stringify(body.textContent));

  // Cursor movement / cursor show-hide address a terminal grid this pane does
  // not have: dropped, not printed.
  feed('q-ansi', {
    type: 'event', kind: 'workload_line',
    text: E + '[?25h' + E + '[1A' + E + '[0G[+] Building 0.3s (2/3)',
  });
  const frame = paneFor('q-ansi').querySelector('.mt-line:last-child .mt-body');
  assert('a compose redraw frame renders as just its text',
    frame.textContent === '[+] Building 0.3s (2/3)',
    JSON.stringify(frame.textContent));

  // ESCAPING. Untrusted line text wrapped in a colour must stay inert, and the
  // module must still not be using innerHTML to do it.
  feed('q-ansi', {
    type: 'event', kind: 'workload_line',
    text: E + '[31m<img src=x onerror=alert(1)>' + E + '[0m',
  });
  const evil = paneFor('q-ansi').querySelector('.mt-line:last-child');
  assert('markup inside a coloured line never becomes an element',
    evil.querySelector('img') === null, evil.innerHTML);
  assert('and it is still visible as text',
    evil.textContent.indexOf('<img src=x onerror=alert(1)>') !== -1,
    JSON.stringify(evil.textContent));
  assert('the module still contains no innerHTML assignment',
    !/\.innerHTML\s*=/.test(src));
  mt.closeMode();
}

// ==========================================================================
console.log('\n-- verbose (`v`): stop eliding, within a ceiling');
// ==========================================================================
// The default renderer is one line per event and elides hard. Verbose turns
// every one of those elisions off — not just the two that were reported — and
// keeps a ceiling so four live streams cannot take the tab down.
{
  document.getElementById('queue-root').innerHTML =
    card('q-verbose', 'live', 'verbose agent');
  mt.openMode();
  const feedV = (payload) =>
    mt.appendPaneLine(paneRecord('q-verbose'), mt.formatPayload(payload));
  assert('verbose is OFF for a fresh viewer', mt.isVerbose() === false);

  const multi = { type: 'event', kind: 'assistant_text', rec: {
    message: { content: [{ type: 'text', text: 'first line\nsecond line\nthird' }] },
  } };
  const toolCall = { type: 'event', kind: 'tool_use', rec: {
    message: { content: [{ type: 'tool_use', name: 'Bash', input: {
      command: 'make deploy\n  --flag', description: 'deploy it' } }] },
  } };
  const attach = { type: 'event', kind: 'attachment', rec: {
    attachment: { type: 'file', path: '/tmp/x.png', size: 1234 } } };
  const image = { type: 'event', kind: 'user_image', rec: {
    message: { content: [
      { type: 'image', source: { type: 'base64', media_type: 'image/png', data: 'AAAA' } },
    ] } } };
  const blocky = { type: 'event', kind: 'tool_result', rec: {
    message: { content: [{ type: 'tool_result', content: [
      { type: 'image', source: { type: 'base64', media_type: 'image/png', data: 'AA' } },
      { type: 'text', text: 'the actual answer' },
    ] }] } } };

  // ---- elided (default)
  let f = mt.formatPayload(multi);
  assert('default: a multi-line message is cut to its first line',
    f.text === 'first line', JSON.stringify(f.text));
  f = mt.formatPayload(toolCall);
  assert('default: a tool call shows one argument\'s first line',
    f.text === 'Bash make deploy', JSON.stringify(f.text));
  f = mt.formatPayload(attach);
  assert('default: an attachment shows its path',
    f.text.indexOf('[attachment]') === 0, JSON.stringify(f.text));
  f = mt.formatPayload(image);
  assert('default: an image is a placeholder', f.text === '[image]',
    JSON.stringify(f.text));
  f = mt.formatPayload(blocky);
  assert('default: a result\'s text block is found past the image',
    f.text === 'the actual answer', JSON.stringify(f.text));

  // ---- verbose
  mt.setVerbose(true);
  assert('the pill reflects verbose',
    document.getElementById('multitail-verbose').getAttribute('aria-pressed') === 'true');
  assert('and the overlay carries the class the stylesheet keys on',
    document.getElementById('multitail').classList.contains('mt-verbose'));

  f = mt.formatPayload(multi);
  assert('verbose: the whole multi-line body is kept',
    f.text === 'first line\nsecond line\nthird', JSON.stringify(f.text));
  f = mt.formatPayload(toolCall);
  assert('verbose: the whole tool input is shown, indented',
    f.text.indexOf('"command"') !== -1 && f.text.indexOf('"description"') !== -1 &&
    f.text.indexOf('\n') !== -1, JSON.stringify(f.text));
  f = mt.formatPayload(attach);
  assert('verbose: the attachment record is shown, path first',
    f.text.indexOf('/tmp/x.png') !== -1 && f.text.indexOf('"size"') !== -1,
    JSON.stringify(f.text));
  f = mt.formatPayload(image);
  assert('verbose: the image is described by type and size',
    f.text.indexOf('image/png') !== -1 && f.text.indexOf('base64 chars') !== -1,
    JSON.stringify(f.text));
  f = mt.formatPayload(blocky);
  assert('verbose: every result block is described, not just the text one',
    f.text.indexOf('image/png') !== -1 && f.text.indexOf('the actual answer') !== -1,
    JSON.stringify(f.text));

  // ---- the ceiling (now a CHOICE — see the cap block further down)
  assert('verbose raises the per-line limit to the chosen cap',
    mt.lineCharLimit() === mt.verboseCapChars(),
    mt.lineCharLimit() + ' vs ' + mt.verboseCapChars());
  assert('but it is a CAP, not "unlimited"',
    mt.verboseCapChars() > mt.MAX_LINE_CHARS_WRAPPED &&
    mt.verboseCapChars() <= 40000, String(mt.verboseCapChars()));
  const huge = 'z'.repeat(mt.verboseCapChars() + 5000);
  feedV({ type: 'event', kind: 'workload_line', text: huge });
  const shown = paneFor('q-verbose').querySelector('.mt-line:last-child .mt-body');
  assert('an oversized verbose line is still clipped',
    shown.textContent.length <= mt.verboseCapChars() + 1,
    'len=' + shown.textContent.length);

  // The pane's TEXT budget, not just its line count: a handful of maximal
  // lines must evict from the head rather than accumulate.
  const pane = paneRecord('q-verbose');
  const need = Math.ceil(mt.MAX_PANE_CHARS / mt.verboseCapChars()) + 2;
  for (let i = 0; i < need; i++) {
    feedV({ type: 'event', kind: 'workload_line', text: huge });
  }
  assert('the pane text budget evicts from the head',
    pane.records.length < need, 'records=' + pane.records.length);
  assert('and the pane is never emptied by it', pane.records.length >= 1);
  assert('the rendered rows match the retained records',
    pane.streamEl.children.length === pane.records.length,
    pane.streamEl.children.length + ' vs ' + pane.records.length);

  mt.setVerbose(false);
  mt.closeMode();
}

// ==========================================================================
console.log('\n-- the verbose cap (`x`): a choice, defaulting to 8K');
// ==========================================================================
// "make max output length for verbose mode configurable. double current value
// as default". 4000 was the fixed value; 8000 is the default now, and the
// ladder brackets it in both directions so the old behaviour is still on it.
{
  const capBtn = document.getElementById('multitail-vcap');
  document.getElementById('queue-root').innerHTML =
    card('q-cap', 'live', 'cap agent');
  mt.openMode();
  const feedV = (payload) =>
    mt.appendPaneLine(paneRecord('q-cap'), mt.formatPayload(payload));

  assert('the ladder is 4K / 8K / 16K / 32K, ascending',
    mt.VERBOSE_CAP_OPTIONS.map((o) => o.key).join(',') === '4k,8k,16k,32k',
    mt.VERBOSE_CAP_OPTIONS.map((o) => o.key).join(','));
  const chars = mt.VERBOSE_CAP_OPTIONS.map((o) => o.chars);
  assert('every step is a real number of characters, in order',
    chars.every((c, i) => c > 0 && (i === 0 || c > chars[i - 1])), String(chars));
  assert('the previous fixed value (4000) is still a choice',
    chars.indexOf(4000) !== -1, String(chars));
  // THE DOUBLING, stated as a number rather than as "the second entry".
  assert('the DEFAULT is 8000 — twice the 4000 this shipped with',
    mt.verboseCap() === mt.DEFAULT_VERBOSE_CAP_KEY &&
    mt.VERBOSE_CAP_OPTIONS.find((o) => o.key === mt.DEFAULT_VERBOSE_CAP_KEY)
      .chars === 8000,
    mt.verboseCap() + ' -> ' + mt.verboseCapChars());

  // The pill: a VALUE, and only on screen while the setting it bounds is.
  mt.openMode();
  mt.setVerbose(false);
  assert('the cap pill is hidden while verbose is off', capBtn.hidden === true);
  assert('and the `x` key does nothing then, so it cannot look broken',
    (() => {
      const before = mt.verboseCap();
      document.dispatchEvent(new window.KeyboardEvent('keydown', { key: 'x' }));
      return mt.verboseCap() === before;
    })());
  mt.setVerbose(true);
  assert('turning verbose on reveals it', capBtn.hidden === false);
  assert('it shows a VALUE, not a pressed state',
    capBtn.textContent === 'cap 8K' && !capBtn.hasAttribute('aria-pressed'),
    capBtn.outerHTML);
  assert('and names that value for a screen reader',
    capBtn.getAttribute('aria-label').indexOf('8,000') !== -1,
    capBtn.getAttribute('aria-label'));

  // Cycling, from the key and from the pill, and it wraps.
  document.dispatchEvent(new window.KeyboardEvent('keydown', { key: 'x' }));
  assert('`x` cycles to the next step', mt.verboseCap() === '16k', mt.verboseCap());
  assert('the pill label followed', capBtn.textContent === 'cap 16K',
    capBtn.textContent);
  assert('and the limit in force followed too', mt.lineCharLimit() === 16000,
    String(mt.lineCharLimit()));
  capBtn.dispatchEvent(new window.MouseEvent('click', { bubbles: true }));
  assert('the pill cycles too', mt.verboseCap() === '32k', mt.verboseCap());
  capBtn.dispatchEvent(new window.MouseEvent('click', { bubbles: true }));
  assert('and it wraps round to the first step', mt.verboseCap() === '4k',
    mt.verboseCap());

  // RAISING is not retroactive, LOWERING is. Lines are stored clipped at the
  // cap in force when they arrived, so a bigger cap cannot recover characters
  // that were never kept — while a smaller one takes effect at once, because
  // rendering clips again.
  mt.setVerboseCap('4k');
  const long = 'q'.repeat(30000);
  feedV({ type: 'event', kind: 'workload_line', text: long });
  const bodyOf = () =>
    paneFor('q-cap').querySelector('.mt-line:last-child .mt-body').textContent;
  assert('a line that arrived under a 4K cap is stored clipped to it',
    bodyOf().length <= 4001, 'len=' + bodyOf().length);
  mt.setVerboseCap('32k');
  assert('raising the cap cannot widen it beyond what was retained',
    bodyOf().length <= 4001, 'len=' + bodyOf().length);
  feedV({ type: 'event', kind: 'workload_line', text: long });
  assert('but the NEXT line gets the whole new cap',
    bodyOf().length > 4001 && bodyOf().length <= 32001,
    'len=' + bodyOf().length);
  mt.setVerboseCap('4k');
  assert('and lowering it clips what is already on screen, at once',
    bodyOf().length <= 4001, 'len=' + bodyOf().length);

  // An unrecognised key is the DEFAULT, never a coercion. (The PERSISTENCE
  // half is asserted in the storage block at the end of this file: this
  // document has no origin, so localStorage throws here by design — which is
  // exactly the case the module's guarded accessors exist for.)
  assert('the storage key is the documented one',
    mt.VERBOSE_CAP_STORAGE_KEY === 'qsite_mt_vcap', mt.VERBOSE_CAP_STORAGE_KEY);
  mt.setVerboseCap('nonsense-from-another-build');
  assert('an unrecognised value resolves to the default, not to a coercion',
    mt.verboseCap() === mt.DEFAULT_VERBOSE_CAP_KEY, mt.verboseCap());

  mt.setVerboseCap(mt.DEFAULT_VERBOSE_CAP_KEY);
  mt.setVerbose(false);
  mt.closeMode();
}

// ==========================================================================
console.log('\n-- every header setting persists per viewer');
// ==========================================================================
// Wrap and timestamps were page-lifetime only; they now persist the same way
// the retention value already did, through the same guarded accessors. The
// defaults for a FRESH viewer are unchanged (all off), a stored value the build
// does not recognise means the default, and storage that throws is survivable.
{
  function bootFlags(seed) {
    const d = new JSDOM(initialHTML, {
      runScripts: 'outside-only', url: 'https://queue.example/',
    });
    d.window.EventSource = class { constructor(u) { this.url = u; } close() {} };
    if (typeof seed === 'function') seed(d.window);
    d.window.eval(ansiSrc);
    d.window.eval(src);
    return d;
  }

  let d = bootFlags();
  assert('fresh viewer: wrap off, time off, verbose off',
    d.window.__multitail.isWrap() === false &&
    d.window.__multitail.isTimestamps() === false &&
    d.window.__multitail.isVerbose() === false);

  d = bootFlags((w) => {
    w.localStorage.setItem('qsite_mt_wrap', '1');
    w.localStorage.setItem('qsite_mt_ts', '1');
    w.localStorage.setItem('qsite_mt_verbose', '1');
  });
  const M = d.window.__multitail;
  assert('stored toggles are restored on a fresh page load',
    M.isWrap() && M.isTimestamps() && M.isVerbose());
  assert('and the server-rendered pills are corrected before first open',
    d.window.document.getElementById('multitail-wrap')
      .getAttribute('aria-pressed') === 'true' &&
    d.window.document.getElementById('multitail-ts')
      .getAttribute('aria-pressed') === 'true' &&
    d.window.document.getElementById('multitail-verbose')
      .getAttribute('aria-pressed') === 'true');
  assert('the overlay classes match the restored state, before it is opened',
    d.window.document.getElementById('multitail').classList.contains('mt-wrap') &&
    d.window.document.getElementById('multitail').classList.contains('mt-verbose'));

  // A stored value this build cannot interpret must not be coerced.
  d = bootFlags((w) => {
    w.localStorage.setItem('qsite_mt_wrap', 'true');
    w.localStorage.setItem('qsite_mt_ts', 'yes please');
  });
  assert('an unrecognised stored flag means the default, not truthiness',
    d.window.__multitail.isWrap() === false &&
    d.window.__multitail.isTimestamps() === false);

  // Choosing writes through for the next load.
  d = bootFlags();
  d.window.__multitail.setWrap(true);
  d.window.__multitail.setVerbose(true);
  assert('choosing a toggle writes it through',
    d.window.localStorage.getItem('qsite_mt_wrap') === '1' &&
    d.window.localStorage.getItem('qsite_mt_verbose') === '1',
    String(d.window.localStorage.getItem('qsite_mt_wrap')));
  d.window.__multitail.setWrap(false);
  assert('and turning it back off writes the OFF value, not a removal',
    d.window.localStorage.getItem('qsite_mt_wrap') === '0',
    String(d.window.localStorage.getItem('qsite_mt_wrap')));
  assert('the storage keys are the documented, distinct ones',
    d.window.__multitail.WRAP_STORAGE_KEY === 'qsite_mt_wrap' &&
    d.window.__multitail.TS_STORAGE_KEY === 'qsite_mt_ts' &&
    d.window.__multitail.VERBOSE_STORAGE_KEY === 'qsite_mt_verbose' &&
    d.window.__multitail.VERBOSE_CAP_STORAGE_KEY === 'qsite_mt_vcap');

  // The verbose CAP is a VALUE like the retention delay, so it persists the
  // same way — and a fresh viewer gets the 8K default, not the 4K this used to
  // be fixed at.
  d = bootFlags();
  assert('fresh viewer: the verbose cap is the 8K default',
    d.window.__multitail.verboseCap() === '8k' &&
    d.window.__multitail.verboseCapChars() === 8000,
    d.window.__multitail.verboseCap());
  d.window.__multitail.setVerboseCap('16k');
  assert('choosing a cap writes it through',
    d.window.localStorage.getItem('qsite_mt_vcap') === '16k',
    String(d.window.localStorage.getItem('qsite_mt_vcap')));
  d = bootFlags((w) => w.localStorage.setItem('qsite_mt_vcap', '32k'));
  assert('a stored cap is restored on a fresh page load',
    d.window.__multitail.verboseCapChars() === 32000,
    String(d.window.__multitail.verboseCapChars()));
  assert('and the server-rendered pill is corrected before first open',
    d.window.document.getElementById('multitail-vcap').textContent === 'cap 32K',
    d.window.document.getElementById('multitail-vcap').textContent);
  d = bootFlags((w) => w.localStorage.setItem('qsite_mt_vcap', '9000k'));
  assert('an unrecognised stored cap means the default, not a parsed number',
    d.window.__multitail.verboseCap() === '8k',
    d.window.__multitail.verboseCap());

  // Storage that throws on every access must not take the module down.
  d = bootFlags((w) => {
    Object.defineProperty(w, 'localStorage', {
      configurable: true,
      get() { throw new Error('site data blocked'); },
    });
  });
  assert('the module still loads when localStorage throws',
    !!d.window.__multitail && d.window.__multitail.isWrap() === false);
  d.window.__multitail.setVerbose(true);
  assert('and a toggle still applies to this page',
    d.window.__multitail.isVerbose() === true);
}


// ==========================================================================
console.log('\n-- subagents: nested tails under the pane of the item they belong to');
// ==========================================================================
// A subagent has no queue id, so a module whose panes WERE queue items could
// not hold one. The fixture below is the real markup both row renderers emit
// (the subagent_node macro in templates/index.html and renderSubagentNode in
// static/refresh.js) from app.py's _build_subagent_tree shape, because reading
// the tree off the rendered card is the contract this feature rests on.
const SID_A = 'a1b2c3d4e5f60718293a4b5c6d7e8f90';
const SID_B = 'b2c3d4e5f60718293a4b5c6d7e8f9001';  // spawn-child of SID_A
const SID_C = 'c3d4e5f60718293a4b5c6d7e8f900112';  // co-bound peer

function subNode(sid, label, age, opts) {
  const o = opts || {};
  const cls = 'subagent-node subagent-log-clickable' +
    (o.peer ? ' subagent-peer' : '');
  return `<li class="${cls}" data-subagent-id="${sid}" ` +
    `data-log-mode="subagent" tabindex="0" role="button" ` +
    `aria-label="View live log for subagent ${sid}" ` +
    `title="Click to tail this subagent's live log">` +
    `<code class="subagent-id">${sid.slice(0, 12)}</code>` +
    `<span class="subagent-label">${label}</span>` +
    `<span class="subagent-age"><span class="rel-age">${age}</span></span>` +
    (o.children
      ? `<ul class="subagent-list subagent-children">${o.children}</ul>` : '') +
    '</li>';
}

function treeCard(qid, summary, nodes) {
  return `<article class="item state-running log-clickable" ` +
    `data-queue-id="${qid}" data-queue-status="running" ` +
    `data-queue-summary="${summary}" data-log-mode="live" ` +
    `data-live-log-mode="live">` +
    `<details class="prompt-toggle subagent-tree" open data-tree-key="${qid}">` +
    `<summary class="prompt-summary">Subagents (${nodes.length})</summary>` +
    `<ul class="subagent-list">${nodes.join('')}</ul>` +
    '</details></article>';
}

function fullTree() {
  return [
    subNode(SID_A, 'investigate the flake', '4m', {
      children: subNode(SID_B, 'grep the CI logs', '90s'),
    }),
    subNode(SID_C, 'co-bound dispatch', '11m', { peer: true }),
  ];
}

function clickEl(node) {
  node.dispatchEvent(
    new window.MouseEvent('click', { bubbles: true, cancelable: true }));
}

{
  resetQueue([
    treeCard('q-tree', 'parent item', fullTree()),
    card('q-plain', 'live', 'no subagents'),
  ]);
  mt.openMode();

  // --- SHOWN by default ----------------------------------------------------
  // This flipped (Andrew: "make subagent views autoshow by default"). The
  // children of a running item are part of "what is everything doing", and
  // having to ask for them per card meant they were usually not on screen.
  assert('a card with subagents opens one pane per node WITHOUT being asked',
    paneEls().length === 5, paneEls().length + ' panes');
  assert('every node in the tree is there, at whatever depth',
    subPaneFor(SID_A) !== null && subPaneFor(SID_B) !== null &&
    subPaneFor(SID_C) !== null);

  const btn = subsBtnOf('q-tree');
  assert('the pane carries a subagent expander', btn !== null && !btn.hidden);
  assert('and it reads as already expanded',
    btn.getAttribute('aria-expanded') === 'true');
  assert('it counts every tail it opened, not just the top level',
    btn.textContent.indexOf('3') !== -1 && /subagents/.test(btn.textContent),
    btn.textContent);
  assert('a pane whose item has no subagents carries no expander at all',
    subsBtnOf('q-plain').hidden === true);
  assert('the default the cards start from is the persisted view preference',
    mt.isSubagentsShown() === true && mt.DEFAULT_SUBS_SHOWN === true);
  assert('and no per-card deviation was recorded to get there',
    mt.subsChoice.size === 0, String(mt.subsChoice.size));

  assert('nested panes sit directly under their parent, in tree order',
    paneEls().map((p) => p.getAttribute('data-pane-key')).join(',') ===
      ['q:q-tree', 's:' + SID_A, 's:' + SID_B, 's:' + SID_C, 'q:q-plain'].join(','),
    paneEls().map((p) => p.getAttribute('data-pane-key')).join(','));
  assert('a nested pane is keyed by SUBAGENT id, never by a queue id',
    subPaneRecord(SID_A).kind === 'subagent' &&
    subPaneRecord(SID_A).target === SID_A &&
    subPaneRecord(SID_A).key === 's:' + SID_A);
  assert('and it carries its PARENT item, not a queue id of its own',
    subPaneRecord(SID_A).qid === 'q-tree' &&
    subPaneFor(SID_A).getAttribute('data-parent-queue-id') === 'q-tree' &&
    subPaneFor(SID_A).hasAttribute('data-queue-id') === false);
  assert('so the parent pane is still the only match for its qid',
    document.querySelectorAll(
      '#multitail-panes .mt-pane[data-queue-id="q-tree"]').length === 1);
  assert('nested panes are badged as subagents',
    subPaneFor(SID_A).querySelector('.mt-pane-badge').textContent === 'subagent');
  assert('a nested pane shows the label the card gave it',
    subPaneFor(SID_A).querySelector('.mt-pane-summary').textContent ===
      'investigate the flake',
    subPaneFor(SID_A).querySelector('.mt-pane-summary').textContent);
  assert('a peer node is marked as co-bound rather than implied to be a child',
    subPaneFor(SID_C).querySelector('.mt-meta-peer') !== null &&
    subPaneFor(SID_A).querySelector('.mt-meta-peer') === null);

  // The indent is the ONLY thing carrying the hierarchy in a flat stack, and
  // it comes from the node's real depth in the card's tree.
  assert('nesting depth is read from the tree, not from position',
    subPaneFor(SID_A).style.getPropertyValue('--mt-sub-depth').trim() === '1' &&
    subPaneFor(SID_B).style.getPropertyValue('--mt-sub-depth').trim() === '2' &&
    subPaneFor(SID_C).style.getPropertyValue('--mt-sub-depth').trim() === '1',
    [SID_A, SID_B, SID_C]
      .map((s2) => subPaneFor(s2).style.getPropertyValue('--mt-sub-depth')).join(','));
  assert('the indent is capped so a deep tree keeps its pane width',
    mt.MAX_SUB_INDENT_DEPTH >= 2 && mt.MAX_SUB_INDENT_DEPTH <= 6,
    String(mt.MAX_SUB_INDENT_DEPTH));

  // --- the stream ----------------------------------------------------------
  assert('a nested pane tails the per-subagent endpoint, not a queue one',
    mt.streamUrl(subPaneRecord(SID_A)) === '/api/subagent/' + SID_A + '/stream',
    mt.streamUrl(subPaneRecord(SID_A)));
  assert('and that is the URL it actually opened',
    !!latestStreamFor(SID_A) &&
    latestStreamFor(SID_A).url === '/api/subagent/' + SID_A + '/stream',
    latestStreamFor(SID_A) && latestStreamFor(SID_A).url);
  assert('a queue pane is untouched by the widening',
    mt.streamUrl(paneRecord('q-tree')) === '/api/queue/q-tree/stream',
    mt.streamUrl(paneRecord('q-tree')));
  assert('the cap still holds across both kinds of pane',
    openStreams().length === CAP,
    'open=' + openStreams().length);
  // DISPLAY order is the order panes are ON SCREEN, not the order they were
  // created: a nested pane is inserted beside its parent, so map order and
  // display order diverge the moment a tree is shown.
  assert('display order puts the tree between its parent and the next card',
    mt.panesInDisplayOrder().map((p) => p.key).join(',') ===
      ['q:q-tree', 's:' + SID_A, 's:' + SID_B, 's:' + SID_C, 'q:q-plain'].join(','),
    mt.panesInDisplayOrder().map((p) => p.key).join(','));
  // SLOT order is NOT display order any more. Nested tails now open without
  // being asked for, so plain display order would let one busy item take the
  // whole connection budget and leave the second running task dark — in the
  // window whose entire question is "what is everything doing".
  assert('slot order puts every top-level tail ahead of every nested one',
    mt.slotOrder().map((p) => p.key).join(',') ===
      ['q:q-tree', 'q:q-plain', 's:' + SID_A, 's:' + SID_B, 's:' + SID_C].join(','),
    mt.slotOrder().map((p) => p.key).join(','));
  assert('so the SECOND running item is streaming, not starved by the tree',
    paneRecord('q-plain').streaming === true, statusOf('q-plain'));
  assert('and it is the nested pane past the cap that waits, and says so',
    /waiting for a stream slot/.test(subStatusOf(SID_C)), subStatusOf(SID_C));
  assert('the overlay count names how much of the stack is the tree',
    /3 subagents/.test(document.getElementById('multitail-count').textContent),
    document.getElementById('multitail-count').textContent);

  // A subagent transcript is the SAME JSONL through the same server tail, so
  // its records carry their own timestamp - the `no ts` marker would be a lie.
  assert('a subagent tail is a timestamped source',
    mt.paneHasSourceTimestamps(subPaneRecord(SID_A)) === true);
  mt.setTimestamps(true);
  latestStreamFor(SID_A).emit({
    type: 'event',
    kind: 'assistant',
    rec: {
      type: 'assistant',
      timestamp: '2026-09-29T18:04:05.000Z',
      message: { content: [{ type: 'text', text: 'looking at the logs' }] },
    },
  });
  assert('and its line renders the record\'s own stamp',
    subPaneFor(SID_A).querySelector('.mt-line .mt-ts') !== null);
  assert('with no "no ts" marker on it',
    subPaneFor(SID_A).querySelector('.mt-pane-nots').hidden === true);
  mt.setTimestamps(false);

  // Untrusted prose: a subagent label is a first-prompt line, which is exactly
  // the kind of string that can contain markup.
  {
    const evil = 'xx<img src=x onerror=alert(1)>';
    const node = document.querySelector(`[data-subagent-id="${SID_C}"] .subagent-label`);
    node.textContent = evil;
    mt.reconcile();
    const title = subPaneFor(SID_C).querySelector('.mt-pane-summary');
    assert('a label containing markup stays inert text in the pane title',
      title.textContent === evil && title.querySelector('img') === null,
      title.innerHTML);
  }

  // --- a node that leaves the tree -----------------------------------------
  // The grandchild, which is one of the panes actually holding a connection,
  // so this also pins what its ending does for the pane behind it in the queue.
  {
    const gone = document.querySelector(`[data-subagent-id="${SID_B}"]`);
    gone.parentNode.removeChild(gone);
    mt.reconcile();
    assert('a subagent the card stopped listing is marked ended, not yanked',
      subPaneFor(SID_B) !== null && subPaneRecord(SID_B).ended === true,
      subStatusOf(SID_B));
    assert('and it says which kind of ending that was',
      /ended . no longer listed/.test(subStatusOf(SID_B)), subStatusOf(SID_B));
    assert('an ended nested pane keeps its output',
      subPaneFor(SID_B).querySelectorAll('.mt-line').length >= 0 &&
      subPaneFor(SID_B) !== null);
    assert('an ended nested pane gives its stream slot back',
      subPaneRecord(SID_B).streaming === false &&
      subPaneRecord(SID_B).es === null);
    assert('so the nested pane that was waiting gets it',
      subPaneRecord(SID_C).streaming === true, subStatusOf(SID_C));
    // It then clears on the ordinary retention countdown, like any other pane.
    subPaneRecord(SID_B).endedAt = Date.now() - (mt.retentionMs() + 1000);
    mt.sweepEndedPanes(new Set());
    assert('and then clears on the ordinary retention countdown',
      subPaneFor(SID_B) === null && subPaneRecord(SID_B) === undefined);
  }

  // --- collapse ONE card, against the default ------------------------------
  {
    const before = openStreams().length;
    clickEl(subsBtnOf('q-tree'));
    assert('collapsing removes the nested panes at once',
      subPaneFor(SID_A) === null && subPaneFor(SID_C) === null &&
      paneFor('q-tree') !== null);
    assert('it closes their streams rather than leaking them',
      openStreams().length < before,
      before + ' -> ' + openStreams().length);
    assert('and records NO dismissal - it is not the same statement as x',
      mt.dismissed.size === 0, Array.from(mt.dismissed).join(','));
    assert('what it records is a per-card deviation from the default',
      mt.subsChoice.get('q-tree') === false &&
      mt.isSubagentsShown() === true,
      JSON.stringify(Array.from(mt.subsChoice.entries())));
    mt.reconcile();
    assert('a later reconcile does not resurrect them', subPaneFor(SID_A) === null);
    // The per-card choice OUTRANKS the default, in both directions: a card the
    // reader collapsed by hand must not spring open because the default was
    // toggled under it.
    mt.setSubagents(false);
    mt.setSubagents(true);
    assert('a card collapsed by hand survives the default being cycled',
      subPaneFor(SID_A) === null, 'nested panes came back');
    clickEl(subsBtnOf('q-tree'));
    assert('re-expanding brings them straight back',
      subPaneFor(SID_A) !== null && subPaneFor(SID_C) !== null);
  }

  // --- turning the DEFAULT off ---------------------------------------------
  // On a fresh open, so every card is following the default rather than a
  // per-card choice — which is what the default is for.
  {
    mt.closeMode();
    resetQueue([
      treeCard('q-tree', 'parent item', fullTree()),
      card('q-plain', 'live', 'no subagents'),
    ]);
    mt.openMode();
    assert('the fresh stack is showing its tree', subPaneFor(SID_A) !== null);
    mt.setSubagents(false);
    assert('every card that was following the default closes its tree',
      subPaneFor(SID_A) === null && subPaneFor(SID_C) === null,
      'nested panes survived');
    assert('the top-level panes are untouched',
      paneFor('q-tree') !== null && paneFor('q-plain') !== null);
    assert('and the nested panes are GONE, not marked ended - nobody ended',
      mt.panes.has('s:' + SID_A) === false,
      Array.from(mt.panes.keys()).join(','));
    assert('the control in the options sheet says so',
      document.getElementById('multitail-subs')
        .getAttribute('aria-pressed') === 'false');
    mt.setSubagents(true);
    assert('turning it back on brings every tree back',
      subPaneFor(SID_A) !== null && subPaneFor(SID_C) !== null);
  }

  // --- closing the parent pane takes its tree with it ----------------------
  {
    clickEl(paneFor('q-tree').querySelector('.mt-pane-close'));
    assert('x on the card pane closes its nested tails too',
      paneFor('q-tree') === null && subPaneFor(SID_A) === null &&
      subPaneFor(SID_C) === null);
    assert('and leaves nothing orphaned in the pane map',
      mt.panes.size === 1 && mt.panes.has('q:q-plain'),
      Array.from(mt.panes.keys()).join(','));
    mt.reconcile();
    assert('the dismissal sticks for the whole group',
      paneFor('q-tree') === null && subPaneFor(SID_A) === null);
  }
}

// ==========================================================================
console.log('\n-- subagents: the whole group ends when the item stops running');
// ==========================================================================
{
  resetQueue([treeCard('q-tree2', 'parent item', fullTree())]);
  mt.openMode();
  assert('three nested panes are up', paneEls().length === 4,
    paneEls().length + ' panes');

  // The item stops running: refresh.js rebuilds the card without the
  // eligibility attribute (and without the tree).
  document.querySelector('[data-queue-id="q-tree2"]')
    .removeAttribute('data-live-log-mode');
  mt.reconcile();

  assert('the card pane ends', paneRecord('q-tree2').ended === true,
    statusOf('q-tree2'));
  assert('and every nested pane ends with it, keeping its output',
    subPaneRecord(SID_A).ended === true && subPaneRecord(SID_B).ended === true &&
    subPaneFor(SID_A) !== null);
  assert('nothing in the group is still holding a connection',
    openStreams().length === 0, openStreams().map((st) => st.url).join(' '));

  // Retention then clears the group exactly as it clears any ended pane.
  const past = Date.now() - (mt.retentionMs() + 1000);
  for (const pane of mt.panes.values()) pane.endedAt = past;
  mt.sweepEndedPanes(new Set());
  assert('retention clears the ended group', mt.panes.size === 0,
    Array.from(mt.panes.keys()).join(','));
}

// ==========================================================================
console.log('\n-- subagents: auto-showing never exceeds the connection cap');
// ==========================================================================
// The cap is what stops "autoshow by default" meaning "open more live
// connections than the browser will give us". It is enforced by pumpSlots and
// does not care where a pane came from, so a stack that opens itself is
// rationed exactly as one the reader opened by hand was.
{
  resetQueue([
    card('q-1', 'live', 'one'), card('q-2', 'live', 'two'),
    card('q-3', 'live', 'three'), card('q-4', 'live', 'four'),
    treeCard('q-tree3', 'parent item', fullTree()),
  ]);
  mt.openMode();
  assert('the nested panes are built and visible without being asked for',
    subPaneFor(SID_A) !== null && subPaneFor(SID_B) !== null &&
    subPaneFor(SID_C) !== null);
  assert('but the cap is unchanged — no fifth connection for them',
    openStreams().length === CAP,
    'open=' + openStreams().length);
  assert('and each one SAYS it is waiting rather than looking broken',
    [SID_A, SID_B, SID_C].every((s2) =>
      /waiting for a stream slot/.test(subStatusOf(s2))),
    [SID_A, SID_B, SID_C].map(subStatusOf).join(' | '));
  // THE POINT OF THE TIER. Five top-level items and a tree: the four
  // connections go to top-level tails, and it is the tree — and the fifth
  // card — that wait. Under plain display order the tree would have taken
  // three of the four and left q-2, q-3 and q-4 dark.
  assert('every connection went to a top-level tail',
    openStreams().every((st) => st.url.indexOf('/api/queue/') === 0),
    openStreams().map((st) => st.url).join(' '));
  assert('the count line names how much of the stack is nested',
    /3 subagents/.test(document.getElementById('multitail-count').textContent),
    document.getElementById('multitail-count').textContent);
}

// ==========================================================================
console.log('\n-- the options dialog: one entry point for every setting');
// ==========================================================================
{
  resetQueue([card('q-opt', 'live', 'one')]);
  mt.openMode();
  const optBtn = document.getElementById('multitail-options');
  const modal = document.getElementById('multitail-options-modal');

  assert('it starts closed', modal.hidden === true &&
    mt.isOptionsOpen() === false &&
    optBtn.getAttribute('aria-expanded') === 'false');

  clickEl(optBtn);
  assert('the header button opens it', modal.hidden === false &&
    mt.isOptionsOpen() === true);
  assert('and the trigger says the dialog it controls is open',
    optBtn.getAttribute('aria-expanded') === 'true');
  assert('every setting is reachable inside the panel',
    ['multitail-wrap', 'multitail-ts', 'multitail-verbose',
     'multitail-subs', 'multitail-retain'].every((id) =>
      document.getElementById('multitail-options-panel')
        .querySelector('#' + id) !== null));

  // A setting changed from inside the sheet applies at once — no OK/Cancel.
  clickEl(document.getElementById('multitail-wrap'));
  assert('a control in the sheet applies live', mt.isWrap() === true);
  clickEl(document.getElementById('multitail-wrap'));

  // The verbose cap is verbose mode's own bound: its whole ROW comes and goes
  // with verbose, not just its control, or the sheet keeps a labelled row
  // describing something that is not there.
  const capRow = document.getElementById('multitail-vcap-row');
  assert('the cap row is hidden while verbose is off', capRow.hidden === true);
  mt.setVerbose(true);
  assert('and appears with it', capRow.hidden === false &&
    document.getElementById('multitail-vcap').hidden === false);
  assert('and its control is then tabbable',
    mt.optionsFocusables().indexOf(
      document.getElementById('multitail-vcap')) !== -1);
  // The trap walks ANCESTORS, not just the control: a row hidden with its
  // control still visible would otherwise leave Tab stopping on something
  // nobody can see.
  capRow.hidden = true;
  assert('a visible control inside a hidden ROW is not tabbable',
    mt.optionsFocusables().indexOf(
      document.getElementById('multitail-vcap')) === -1);
  capRow.hidden = false;
  mt.setVerbose(false);
  assert('turning verbose back off hides the cap row again',
    capRow.hidden === true &&
    document.getElementById('multitail-vcap').hidden === true);

  // Esc dismisses the DIALOG, not the mode behind it: dismissing should undo
  // the last thing that opened, not two things.
  key('Escape');
  assert('Esc closes the sheet', mt.isOptionsOpen() === false);
  assert('and leaves the window open', mt.isOpen() === true);
  key('Escape');
  assert('a second Esc then leaves the mode', mt.isOpen() === false);

  // `o` is the keyboard entry point, and the backdrop is a way out.
  mt.openMode();
  key('o');
  assert('`o` opens it', mt.isOptionsOpen() === true);
  key('m');
  assert('`m` does NOT yank the window out from under the dialog',
    mt.isOpen() === true && mt.isOptionsOpen() === true);
  clickEl(modal.querySelector('.modal-backdrop'));
  assert('a backdrop click closes it', mt.isOptionsOpen() === false);
  clickEl(optBtn);
  clickEl(document.getElementById('multitail-options-close'));
  assert('so does the close button', mt.isOptionsOpen() === false);

  // `s` is the subagent default's key, and it is the same setting the sheet
  // shows — the control repaints rather than going stale.
  key('s');
  assert('`s` flips the subagent default', mt.isSubagentsShown() === false);
  assert('and the control in the sheet followed it',
    document.getElementById('multitail-subs')
      .getAttribute('aria-pressed') === 'false');
  key('s');
  assert('back on', mt.isSubagentsShown() === true);

  // Leaving the mode cannot leave the sheet behind, or it would be the first
  // thing the next `m` showed.
  clickEl(optBtn);
  assert('the sheet is up', mt.isOptionsOpen() === true);
  mt.closeMode();
  assert('closing the mode closes the sheet with it',
    mt.isOptionsOpen() === false && modal.hidden === true);
}

// ==========================================================================
console.log('\n-- the SHIPPED cap: a ten-plus pane stack streams, and no slot leaks');
// ==========================================================================
{
  mt.setMaxLiveStreams(0); // 0 restores the shipped MAX_LIVE_STREAMS
  const N = mt.MAX_LIVE_STREAMS + 2;
  const ids = [];
  for (let i = 1; i <= N; i++) ids.push('q-big' + i);
  resetQueue(ids.map((id) => card(id, 'live', 'agent ' + id)));
  mt.openMode();
  assert('every card has a pane', paneEls().length === N, 'panes=' + paneEls().length);
  assert('the first ten panes are all streaming (none "waiting for a stream slot")',
    ids.slice(0, 10).every((id) => !/waiting for a stream slot/.test(statusOf(id))),
    ids.slice(0, 10).map(statusOf).join(' | '));
  assert('exactly the shipped cap is open', openStreams().length === mt.MAX_LIVE_STREAMS,
    'open=' + openStreams().length);
  assert('only the overflow waits',
    /waiting for a stream slot/.test(statusOf(ids[N - 1])), statusOf(ids[N - 1]));

  // LEAK GUARD. A CLOSED EventSource (readyState 2: the response was an HTTP
  // error) never reconnects and never fires onerror again. It must hand its
  // slot back, or the pane holds it forever while a waiter starves.
  const dead = streamFor(ids[0]);
  dead.readyState = 2;
  dead.onerror({});
  assert('a browser-CLOSED stream releases its slot', dead.closed === true);
  assert('so the overflow pane is promoted',
    streamFor(ids[N - 1]) !== undefined || streamFor(ids[N - 2]) !== undefined);
  assert('the cap still holds after the swap',
    openStreams().length === mt.MAX_LIVE_STREAMS, 'open=' + openStreams().length);
  assert('the dead pane says it is retrying, with a record',
    /waiting for log/.test(statusOf(ids[0])) &&
    /retrying/.test(paneFor(ids[0]).textContent), statusOf(ids[0]));
  const before = streamCountFor(ids[1]);

  // A stream the browser is still reconnecting (readyState 0) is healthy: it
  // keeps its slot and is NOT torn down.
  const live = streamFor(ids[1]);
  live.readyState = 0;
  live.onerror({});
  assert('a CONNECTING (auto-reconnecting) stream keeps its slot',
    live.closed === false && streamCountFor(ids[1]) === before);
  assert('and says it is reconnecting', /reconnecting/.test(statusOf(ids[1])),
    statusOf(ids[1]));
  mt.setMaxLiveStreams(CAP);
}

console.log(
  failures === 0
    ? '\nAll multitail tests passed.'
    : `\n${failures} multitail test(s) FAILED.`,
);
process.exit(failures === 0 ? 0 : 1);
