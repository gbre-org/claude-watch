#!/usr/bin/env node
// Smoke test for live-log.js per-event one-line headline rendering.
//
// Boots a jsdom environment with the minimal log-modal scaffolding,
// loads live-log.js, and drives renderEvent() with synthetic JSONL
// payloads to assert each formatter produces a <details class="log-event">
// with a <summary class="log-headline"> top row (timestamp + label chip
// + concise content preview) and a <div class="log-event-body"> below.
//
// Mirrors agent-tail's per-line format:
//   - assistant.text → `ASSISTANT <first chars>`
//   - tool_use       → `TOOL <Name> <Name>(<short args>)`
//   - tool_result    → `RESULT [<short id>] <first line>`
//   - thinking       → `THINKING <first chars>`
//
// Usage:   node live-log.test.js
// Exit 0 on success, 1 on first failure.

'use strict';

const path = require('path');
const fs = require('fs');

const NODE_MODULES = process.env.QM_NODE_MODULES ||
  '/tmp/queue-minisite-test/node_modules';
const { JSDOM } = require(path.join(NODE_MODULES, 'jsdom'));

const STATIC_DIR = path.dirname(path.resolve(__filename));
const liveLogSrc = fs.readFileSync(
  path.join(STATIC_DIR, 'live-log.js'),
  'utf8',
);

// Minimal DOM scaffolding — live-log.js looks up these IDs on load and
// bails early if #log-modal is absent. We provide just enough chrome so
// the IIFE registers handlers + the test hook exposure on window.
const initialHTML = `<!doctype html>
<html><head></head><body>
  <div id="log-modal" hidden>
    <span id="log-modal-id"></span>
    <span id="log-modal-mode-label"></span>
    <span id="log-modal-summary"></span>
    <span id="log-modal-status"></span>
    <pre id="log-modal-stream"></pre>
    <button id="log-modal-close"></button>
    <button id="log-modal-autoscroll"></button>
    <button id="log-modal-jump-top"></button>
    <button id="log-modal-jump-bottom"></button>
    <details id="log-modal-prompt" hidden>
      <summary><span id="log-modal-prompt-label"></span></summary>
      <pre id="log-modal-prompt-body"></pre>
    </details>
    <div id="log-modal-meta-summary" hidden>
      <details id="log-meta-toggle">
        <summary>Metadata</summary>
        <div id="log-meta-rows">
          <div id="log-meta-row-status"></div>
          <div id="log-meta-row-runtime"></div>
          <div id="log-meta-row-times"></div>
          <div id="log-meta-row-scope"></div>
          <div id="log-meta-row-deps"></div>
          <div id="log-meta-row-dependents"></div>
          <div id="log-meta-row-by"></div>
          <div id="log-meta-row-group"></div>
          <div id="log-meta-row-usage"></div>
          <div id="log-meta-row-abandon"></div>
        </div>
      </details>
    </div>
    <span id="log-meta-status"></span>
    <span id="log-meta-runtime"></span>
    <span id="log-meta-times"></span>
    <span id="log-meta-scope"></span>
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
  </div>
</body></html>`;

const dom = new JSDOM(initialHTML, { runScripts: 'outside-only' });
const { window } = dom;
// Inject + execute live-log.js inside the jsdom window so its IIFE
// runs and registers __liveLog on window.
window.eval(liveLogSrc);

const { renderEvent, formatters, headlinePreview } = window.__liveLog;

let failures = 0;
function assert(label, cond, detail) {
  if (cond) {
    console.log('  ok  ' + label);
  } else {
    failures += 1;
    console.error('  FAIL ' + label + (detail ? '\n       ' + detail : ''));
  }
}

const stream = window.document.getElementById('log-modal-stream');

// Helper: clear stream + render one payload + return the rendered
// .log-line element.
function render(payload) {
  stream.innerHTML = '';
  renderEvent(payload);
  return stream.firstElementChild;
}

// Helper: assert a rendered line has the expected log-event structure
// (details > summary.log-headline + .log-event-body).
function assertEventShape(prefix, line) {
  assert(prefix + ' line rendered', !!line, 'stream is empty');
  if (!line) return null;
  const details = line.querySelector('details.log-event');
  assert(prefix + ' has <details.log-event>', !!details);
  if (!details) return null;
  const headline = details.querySelector('summary.log-headline');
  assert(prefix + ' has <summary.log-headline>', !!headline);
  const body = details.querySelector('.log-event-body');
  assert(prefix + ' has <.log-event-body>', !!body);
  // Closed by default.
  assert(prefix + ' <details> closed by default', !details.hasAttribute('open'));
  return { details, headline, body };
}

console.log('headlinePreview()');
assert('truncates long text', headlinePreview('a'.repeat(200), 50).length === 51 /* 50 chars + … */);
assert('collapses newlines', headlinePreview('a\nb\nc', 50) === 'a b c');
assert('escapes HTML', headlinePreview('<script>', 50) === '&lt;script&gt;');
assert('empty in → empty out', headlinePreview('', 50) === '');

console.log('\nfmtAssistantText — text block');
{
  const payload = {
    type: 'event',
    kind: 'assistant',
    rec: {
      timestamp: '2026-05-11T17:30:00.000Z',
      message: {
        content: [{ type: 'text', text: 'I have completed the analysis of the queue minisite. ' + 'x'.repeat(200) }],
      },
    },
  };
  const line = render(payload);
  const parts = assertEventShape('assistant', line);
  if (parts) {
    const hl = parts.headline.textContent;
    assert('headline contains "ASSISTANT"', hl.includes('ASSISTANT'), 'got: ' + hl);
    assert('headline contains preview text', hl.includes('completed the analysis'), 'got: ' + hl);
    assert('headline ends with …', hl.includes('…'), 'got: ' + hl);
  }
}

console.log('\nfmtToolUse — Bash');
{
  const payload = {
    type: 'event',
    kind: 'tool_use',
    rec: {
      timestamp: '2026-05-11T17:30:01.000Z',
      message: {
        content: [{
          type: 'tool_use',
          id: 'toolu_abc123',
          name: 'Bash',
          input: { command: 'ls -la /tmp' },
        }],
      },
    },
  };
  const line = render(payload);
  const parts = assertEventShape('tool_use Bash', line);
  if (parts) {
    const hl = parts.headline.textContent;
    assert('headline contains Bash(ls -la /tmp)',
      hl.includes('Bash(ls -la /tmp)'),
      'got: ' + hl);
  }
}

console.log('\nfmtToolUse — Bash with description (headline parity with task-watch)');
{
  // Bash callers usually pass a short imperative `description` alongside
  // `command`. The task-watch dashboard renders this as the green
  // `$ <description>` line (see src/task_filters.rs Bash branch). The
  // queue-minisite headline must match — show the description, not the
  // full command, so users see the same human-readable summary in both
  // surfaces. Regression report: green `$ <description>` lines visible
  // in the task-watch tmux dashboard never made it to the web modal.
  const payload = {
    type: 'event',
    kind: 'tool_use',
    rec: {
      timestamp: '2026-05-11T17:30:01.500Z',
      message: {
        content: [{
          type: 'tool_use',
          id: 'toolu_desc1',
          name: 'Bash',
          input: {
            command: 'mv "/srv/media/Show/Season 00/file 1.mkv" "/srv/media/Show/Season 00/Show - S00E01 - Episode One.mkv"',
            description: 'Rename episode file to canonical name',
          },
        }],
      },
    },
  };
  const line = render(payload);
  const parts = assertEventShape('tool_use Bash w/ description', line);
  if (parts) {
    const hl = parts.headline.textContent;
    assert('headline shows the description, not the command',
      hl.includes('Bash(Rename episode file to canonical name)'),
      'got: ' + hl);
    assert('headline does NOT show raw mv command in summary',
      !hl.includes('mv "/srv/media'),
      'got: ' + hl);
    // Body should still show the actual command so users can drill in.
    const bodyText = parts.body.textContent;
    assert('body still surfaces the actual command',
      bodyText.includes('mv "/srv/media'),
      'got: ' + bodyText.slice(0, 200));
  }
}

console.log('\nfmtToolUse — Bash with empty/whitespace description falls back to command');
{
  const payload = {
    type: 'event',
    kind: 'tool_use',
    rec: {
      timestamp: '2026-05-11T17:30:01.700Z',
      message: {
        content: [{
          type: 'tool_use',
          id: 'toolu_desc2',
          name: 'Bash',
          input: { command: 'ls /tmp', description: '   ' },
        }],
      },
    },
  };
  const line = render(payload);
  const parts = assertEventShape('tool_use Bash w/ blank description', line);
  if (parts) {
    const hl = parts.headline.textContent;
    assert('blank description does not override command',
      hl.includes('Bash(ls /tmp)'),
      'got: ' + hl);
  }
}

console.log('\nfmtToolUse — Read');
{
  const payload = {
    type: 'event',
    kind: 'tool_use',
    rec: {
      timestamp: '2026-05-11T17:30:02.000Z',
      message: {
        content: [{
          type: 'tool_use',
          id: 'toolu_def456',
          name: 'Read',
          input: { file_path: '/etc/foo.conf' },
        }],
      },
    },
  };
  const line = render(payload);
  const parts = assertEventShape('tool_use Read', line);
  if (parts) {
    const hl = parts.headline.textContent;
    assert('headline contains Read(/etc/foo.conf)',
      hl.includes('Read(/etc/foo.conf)'),
      'got: ' + hl);
  }
}

console.log('\nfmtToolResult — short result with id');
{
  const payload = {
    type: 'event',
    kind: 'tool_result',
    rec: {
      timestamp: '2026-05-11T17:30:03.000Z',
      message: {
        content: [{
          type: 'tool_result',
          tool_use_id: 'toolu_abc123',
          content: 'total 0\ndrwxr-xr-x  ...\n',
        }],
      },
    },
  };
  const line = render(payload);
  const parts = assertEventShape('tool_result', line);
  if (parts) {
    const hl = parts.headline.textContent;
    assert('headline contains RESULT', hl.includes('RESULT'), 'got: ' + hl);
    assert('headline contains short id [bc123]', hl.includes('bc123') || hl.includes('c123]'),
      'got: ' + hl);
    assert('headline shows first line of body',
      hl.includes('total 0'), 'got: ' + hl);
  }
}

console.log('\nfmtThinking — pure thinking record');
{
  const payload = {
    type: 'event',
    kind: 'thinking',
    rec: {
      timestamp: '2026-05-11T17:30:04.000Z',
      message: {
        content: [{ type: 'thinking', thinking: 'Let me consider the next step carefully.' }],
      },
    },
  };
  const line = render(payload);
  const parts = assertEventShape('thinking', line);
  if (parts) {
    const hl = parts.headline.textContent;
    assert('headline contains THINKING', hl.includes('THINKING'), 'got: ' + hl);
    assert('headline contains preview',
      hl.includes('consider the next step'), 'got: ' + hl);
  }
}

console.log('\nfmtUser — user content text');
{
  const payload = {
    type: 'event',
    kind: 'user',
    rec: {
      timestamp: '2026-05-11T17:30:05.000Z',
      message: {
        content: 'Please run /clean now and report back.',
      },
    },
  };
  const line = render(payload);
  const parts = assertEventShape('user', line);
  if (parts) {
    const hl = parts.headline.textContent;
    assert('headline contains USER label', hl.includes('USER'), 'got: ' + hl);
    assert('headline contains user text',
      hl.includes('Please run /clean'), 'got: ' + hl);
  }
}

console.log('\nfmtWorkloadLine — terminal-style (flat, no <details>)');
{
  const payload = {
    type: 'event',
    kind: 'workload_line',
    rec: {},
    text: 'rsync ...some output...',
  };
  const line = render(payload);
  assert('workload line rendered', !!line);
  if (line) {
    // workload lines are flat — no <details> chrome.
    const details = line.querySelector('details.log-event');
    assert('workload line has NO <details> (flat)', !details);
    assert('workload line shows raw text',
      line.textContent.includes('rsync'));
  }
}

console.log('\nfmtWorkloadLine — transient-replace (\\r progress frames)');
{
  // The CR/LF-aware tail emits each \r-terminated rsync-style progress
  // segment as a workload_line with `transient: true`. The renderer
  // must REPLACE the previous workload row in place rather than stack
  // a new one. \n-terminated segments graduate to permanent.
  //
  // State machine under test:
  //   transient -> transient -> transient  (one replacing row)
  //   transient -> permanent               (final replace + graduate)
  //   permanent -> transient                (start a new tracked row)
  //   permanent -> permanent                (regression — current behavior)
  stream.innerHTML = '';

  // Three back-to-back transient frames — should leave ONE row whose
  // text is the final transient value.
  renderEvent({ type: 'event', kind: 'workload_line', rec: {}, text: '20%', transient: true });
  renderEvent({ type: 'event', kind: 'workload_line', rec: {}, text: '40%', transient: true });
  renderEvent({ type: 'event', kind: 'workload_line', rec: {}, text: '60%', transient: true });
  assert('three transient frames → one row in stream',
    stream.children.length === 1,
    'got rows=' + stream.children.length);
  assert('only row shows the latest transient value (60%)',
    stream.firstElementChild && stream.firstElementChild.textContent.includes('60%'),
    'got: ' + (stream.firstElementChild && stream.firstElementChild.textContent));
  assert('only row no longer shows the earlier transient value (20%)',
    !(stream.firstElementChild && stream.firstElementChild.textContent.includes('20%')),
    'got: ' + (stream.firstElementChild && stream.firstElementChild.textContent));

  // Final \n-terminated frame — REPLACE the prior row and graduate it
  // so the NEXT segment (if any) appends fresh.
  renderEvent({ type: 'event', kind: 'workload_line', rec: {}, text: '100%', transient: false });
  assert('finalize (\\n) still keeps a single row in place',
    stream.children.length === 1,
    'got rows=' + stream.children.length);
  assert('finalized row text is the permanent 100% value',
    stream.firstElementChild && stream.firstElementChild.textContent.includes('100%'));

  // A subsequent permanent line should APPEND, not replace — the prior
  // row has graduated, transient-tracking is cleared.
  renderEvent({ type: 'event', kind: 'workload_line', rec: {}, text: 'done', transient: false });
  assert('post-finalize permanent line APPENDS (now 2 rows)',
    stream.children.length === 2,
    'got rows=' + stream.children.length);

  // permanent → transient: the new transient line opens a fresh tracked
  // row instead of overwriting the prior permanent row.
  renderEvent({ type: 'event', kind: 'workload_line', rec: {}, text: '10s elapsed', transient: true });
  assert('permanent → transient APPENDS (3 rows now)',
    stream.children.length === 3,
    'got rows=' + stream.children.length);
  assert('last row reflects the new transient text',
    stream.lastElementChild && stream.lastElementChild.textContent.includes('10s elapsed'));

  // Another transient — REPLACE the just-added transient row.
  renderEvent({ type: 'event', kind: 'workload_line', rec: {}, text: '20s elapsed', transient: true });
  assert('back-to-back transient still 3 rows (replaced)',
    stream.children.length === 3);
  assert('last row updated to 20s elapsed',
    stream.lastElementChild && stream.lastElementChild.textContent.includes('20s elapsed'));
}

console.log('\nfmtWorkloadLine — meta frame breaks the transient chain');
{
  // A meta frame (or error, or raw) MUST clear the transient-tracking
  // anchor so a subsequent transient workload line opens a fresh row
  // rather than overwriting the meta line.
  stream.innerHTML = '';
  renderEvent({ type: 'event', kind: 'workload_line', rec: {}, text: '20%', transient: true });
  // Inject a meta frame (workload-end / backfill-end / etc all flow
  // through the same appendLine path).
  renderEvent({ type: 'meta', kind: 'backfill-end' });
  // Transient again — should NOT replace the meta row; should append.
  renderEvent({ type: 'event', kind: 'workload_line', rec: {}, text: '50%', transient: true });
  assert('after meta, transient frame appends (3 rows: 20%, meta, 50%)',
    stream.children.length === 3,
    'got rows=' + stream.children.length);
  assert('meta row is intact between the two transient frames',
    stream.children[1].textContent.includes('[meta]'));
  assert('last row is the new transient value (50%)',
    stream.lastElementChild.textContent.includes('50%'));
}

console.log('\nfmtAttachment');
{
  const payload = {
    type: 'event',
    kind: 'attachment',
    rec: {
      timestamp: '2026-05-11T17:30:06.000Z',
      attachment: { type: 'image', path: '/tmp/screenshot.png' },
    },
  };
  const line = render(payload);
  const parts = assertEventShape('attachment', line);
  if (parts) {
    const hl = parts.headline.textContent;
    assert('headline contains ATTACH', hl.includes('ATTACH'), 'got: ' + hl);
    assert('headline contains file path',
      hl.includes('screenshot.png'), 'got: ' + hl);
  }
}

console.log('\nmodal vim shortcuts — jumpToTop / jumpToBottom / scrollStream');
{
  // The modal keybind helpers manipulate streamEl.scrollTop directly.
  // jsdom doesn't lay elements out, so scrollHeight defaults to 0 — we
  // can stub a non-zero scrollHeight on the actual <pre> the IIFE
  // looked up to drive the bottom-jump path. Bind the same element the
  // IIFE captured (window.document.getElementById('log-modal-stream')).
  const streamEl = window.document.getElementById('log-modal-stream');
  // jsdom's HTMLElement.scrollHeight is read-only; defineProperty lets
  // us shim it without touching layout. scrollTop is read/write so we
  // can just assign + read it back.
  Object.defineProperty(streamEl, 'scrollHeight', {
    configurable: true,
    get: () => 1000,
  });
  const { jumpToTop, jumpToBottom, scrollStream, SCROLL_STEP_PX } = window.__liveLog;

  assert('SCROLL_STEP_PX is positive', typeof SCROLL_STEP_PX === 'number' && SCROLL_STEP_PX > 0);

  streamEl.scrollTop = 500;
  jumpToTop();
  assert('jumpToTop sets scrollTop to 0', streamEl.scrollTop === 0);

  streamEl.scrollTop = 0;
  jumpToBottom();
  assert('jumpToBottom sets scrollTop to scrollHeight', streamEl.scrollTop === 1000);

  streamEl.scrollTop = 100;
  scrollStream(SCROLL_STEP_PX);
  assert('scrollStream(+step) increases scrollTop by step',
    streamEl.scrollTop === 100 + SCROLL_STEP_PX,
    'got: ' + streamEl.scrollTop);
  scrollStream(-SCROLL_STEP_PX);
  assert('scrollStream(-step) decreases scrollTop by step',
    streamEl.scrollTop === 100,
    'got: ' + streamEl.scrollTop);
}

console.log('\nmodal vim shortcuts — keydown dispatch (j/k/g/G/Esc)');
{
  // Verify that the global keydown listener actually wires the modal
  // helpers when modal.hidden = false. We toggle the modal visible,
  // dispatch synthetic KeyboardEvents at the document, and assert the
  // observable side-effect on streamEl.scrollTop. A modal-hidden
  // baseline confirms the handler stays a no-op when the modal is closed
  // (so the main-list keyboard.js shortcuts continue to own j/k).
  const doc = window.document;
  const modal = doc.getElementById('log-modal');
  const streamEl = doc.getElementById('log-modal-stream');

  function fire(key) {
    const ev = new window.KeyboardEvent('keydown', {
      key, bubbles: true, cancelable: true,
    });
    doc.dispatchEvent(ev);
    return ev;
  }

  // Baseline: modal hidden → j/k must NOT scroll the stream (keyboard.js
  // owns those keys when the modal is closed).
  modal.hidden = true;
  streamEl.scrollTop = 200;
  fire('j');
  assert('modal hidden: j is not consumed by live-log handler',
    streamEl.scrollTop === 200, 'got: ' + streamEl.scrollTop);

  // Modal open: j scrolls down by step.
  modal.hidden = false;
  streamEl.scrollTop = 200;
  fire('j');
  assert('modal open: j scrolls down by SCROLL_STEP_PX',
    streamEl.scrollTop === 200 + window.__liveLog.SCROLL_STEP_PX,
    'got: ' + streamEl.scrollTop);

  // Modal open: k scrolls up by step.
  streamEl.scrollTop = 200;
  fire('k');
  assert('modal open: k scrolls up by SCROLL_STEP_PX',
    streamEl.scrollTop === 200 - window.__liveLog.SCROLL_STEP_PX,
    'got: ' + streamEl.scrollTop);

  // Modal open: g jumps to top.
  streamEl.scrollTop = 500;
  fire('g');
  assert('modal open: g jumps to top', streamEl.scrollTop === 0,
    'got: ' + streamEl.scrollTop);

  // Modal open: G jumps to bottom (scrollHeight=1000 from the previous block).
  streamEl.scrollTop = 0;
  fire('G');
  assert('modal open: G jumps to bottom', streamEl.scrollTop === 1000,
    'got: ' + streamEl.scrollTop);

  // gg chord (two-key vim sequence) also jumps to top. The second g
  // is a harmless re-jump since we're already at 0, but verifying
  // both keys are accepted within the chord window matters for
  // muscle-memory parity with vim.
  streamEl.scrollTop = 500;
  fire('g');
  fire('g');
  assert('modal open: gg chord lands at top', streamEl.scrollTop === 0,
    'got: ' + streamEl.scrollTop);

  // Typing guard: when focus is inside an <input>, j/k/g/G must pass
  // through so the user can type literal characters.
  const probe = doc.createElement('input');
  probe.id = 'modal-typing-probe';
  modal.appendChild(probe);
  probe.focus();
  streamEl.scrollTop = 300;
  fire('j');
  assert('typing target: j is ignored inside <input>',
    streamEl.scrollTop === 300, 'got: ' + streamEl.scrollTop);
  fire('g');
  assert('typing target: g is ignored inside <input>',
    streamEl.scrollTop === 300, 'got: ' + streamEl.scrollTop);
  // ...but Esc still closes the modal (the "get-me-out" override).
  modal.hidden = false;
  fire('Escape');
  assert('typing target: Esc still closes the modal even from <input>',
    modal.hidden === true);
  probe.remove();
}

// Starting-state polling — clicking on a row whose queue item is
// "starting" (registered but no agent JSONL yet) opens the modal in a
// polling state. The /stream endpoint emits a one-shot error event
// (kind=no-agent or kind=no-jsonl) and closes; the frontend swallows
// the error, schedules a retry, and only transitions to live-tail
// when a real `meta:stream-start` lands.
console.log('\nstarting-state polling — no-agent / no-jsonl retry');
{
  const hooks = window.__liveLog;
  const statusEl = window.document.getElementById('log-modal-status');

  // Helper: reset polling state between assertions.
  function resetPolling(qid) {
    hooks.clearPollTimer();
    hooks.setPollingQid(qid);
    if (statusEl) statusEl.textContent = '';
  }

  // 1. no-agent error while polling → no error line, schedules a retry.
  resetPolling('q-test-starting');
  stream.innerHTML = '';
  renderEvent({
    type: 'error',
    kind: 'no-agent',
    queue_id: 'q-test-starting',
    error: 'No active agent record found for this queue id.',
  });
  assert('no-agent while polling: pollingQid retained',
    hooks.getPollingQid() === 'q-test-starting',
    'got: ' + hooks.getPollingQid());
  assert('no-agent while polling: retry timer scheduled',
    hooks.getPollTimer() !== null);
  assert('no-agent while polling: status = waiting for agent…',
    statusEl.textContent === 'waiting for agent…',
    'got: ' + statusEl.textContent);
  assert('no-agent while polling: no error line rendered in stream',
    stream.querySelector('.log-error-line') === null);

  // 2. no-jsonl error while polling → same retry behavior.
  resetPolling('q-test-starting');
  stream.innerHTML = '';
  renderEvent({
    type: 'error',
    kind: 'no-jsonl',
    queue_id: 'q-test-starting',
    error: 'Agent transcript not found.',
  });
  assert('no-jsonl while polling: retry timer scheduled',
    hooks.getPollTimer() !== null);
  assert('no-jsonl while polling: no error line rendered in stream',
    stream.querySelector('.log-error-line') === null);

  // 3. stream-start arrives while polling → polling cleared, transition.
  resetPolling('q-test-starting');
  stream.innerHTML = '';
  renderEvent({
    type: 'meta',
    kind: 'stream-start',
    path: '/agents-jsonl/agent-abc.jsonl',
  });
  assert('stream-start while polling: pollingQid cleared',
    hooks.getPollingQid() === null,
    'got: ' + hooks.getPollingQid());
  assert('stream-start while polling: status = streaming',
    statusEl.textContent === 'streaming',
    'got: ' + statusEl.textContent);

  // 4. Other error kinds while polling → still rendered as a real
  // error (e.g. server crash, malformed payload).
  resetPolling('q-test-starting');
  stream.innerHTML = '';
  renderEvent({
    type: 'error',
    kind: 'jsonl-read-failed',
    queue_id: 'q-test-starting',
    error: 'EIO',
  });
  assert('non-poll error while polling: error line rendered',
    stream.querySelector('.log-error-line') !== null);
  assert('non-poll error while polling: status = error',
    statusEl.textContent === 'error',
    'got: ' + statusEl.textContent);

  // 5. no-agent error when NOT polling (regular running row) →
  // surfaced as a real error.
  resetPolling(null);
  stream.innerHTML = '';
  renderEvent({
    type: 'error',
    kind: 'no-agent',
    queue_id: 'q-test-running',
    error: 'No active agent record found for this queue id.',
  });
  assert('no-agent without polling: error line rendered',
    stream.querySelector('.log-error-line') !== null);
  assert('no-agent without polling: status = error',
    statusEl.textContent === 'error',
    'got: ' + statusEl.textContent);

  // Clean up.
  hooks.clearPollTimer();
  hooks.setPollingQid(null);
}

// Runtime ticker — applyMetaSummary() with a running-item payload
// should:
//   1. render the initial runtime string,
//   2. stamp data-started-at on the runtime row + value,
//   3. mark the runtime ticker as active,
//   4. re-render the runtime string when the simulated clock advances,
//   5. stop ticking once a non-running status comes in,
//   6. stop ticking on resetMetaSummary() (modal close / re-open).
//
// We drive the clock by stubbing Date.now() — the production code
// calls Date.now() directly inside renderRuntimeFromAnchor(), so a
// targeted stub is enough without faking setInterval too.
console.log('\nruntime ticker — running item RUNTIME field updates live');
{
  const hooks = window.__liveLog;
  const runtimeRow = window.document.getElementById('log-meta-row-runtime');
  const runtimeVal = window.document.getElementById('log-meta-runtime');

  // live-log.js runs inside the jsdom window context, so it calls
  // window.Date.now() — NOT Node's Date.now (which is a different
  // intrinsic on a different global). Stub both to keep the test
  // deterministic regardless of where the production code resolves
  // Date from.
  const realNodeDateNow = Date.now;
  const realWinDateNow = window.Date.now;
  function withClock(nowMs, fn) {
    Date.now = () => nowMs;
    window.Date.now = () => nowMs;
    try { fn(); } finally {
      Date.now = realNodeDateNow;
      window.Date.now = realWinDateNow;
    }
  }

  // Anchor "now" at a fixed instant so the test is deterministic.
  // started_at is 30s before now → initial render is "30s".
  const NOW_MS = 1778598000000;            // arbitrary fixed timestamp
  const STARTED_MS = NOW_MS - 30 * 1000;   // 30s ago
  const startedIso = new Date(STARTED_MS).toISOString();

  // Reset modal state first — applyMetaSummary() assumes a fresh
  // open (resetMetaSummary clears prior rows + stops any ticker).
  hooks.resetMetaSummary();

  withClock(NOW_MS, () => {
    hooks.applyMetaSummary({
      ok: true,
      status: 'running',
      started_at: startedIso,
      runtime_seconds: 30,
    });
  });

  assert('running: runtime row visible',
    runtimeRow && runtimeRow.hidden === false,
    'hidden=' + (runtimeRow && runtimeRow.hidden));
  assert('running: data-started-at stamped on row',
    runtimeRow && runtimeRow.getAttribute('data-started-at') === startedIso,
    'got: ' + (runtimeRow && runtimeRow.getAttribute('data-started-at')));
  assert('running: data-started-at stamped on value el',
    runtimeVal && runtimeVal.getAttribute('data-started-at') === startedIso,
    'got: ' + (runtimeVal && runtimeVal.getAttribute('data-started-at')));
  assert('running: initial runtime text = "30s"',
    runtimeVal && runtimeVal.textContent === '30s',
    'got: ' + (runtimeVal && runtimeVal.textContent));
  assert('running: runtime ticker is active',
    hooks.getRuntimeTickerActive() === true);
  assert('running: getRuntimeStartedMs matches anchor',
    hooks.getRuntimeStartedMs() === STARTED_MS,
    'got: ' + hooks.getRuntimeStartedMs());

  // Simulate +5s elapsed and call the tick path manually. We can't
  // easily fast-forward jsdom's setInterval, but we expose the same
  // logic through Date.now() — re-invoking applyMetaSummary() would
  // restart the ticker; instead we test the per-frame render by
  // calling fmtRuntime via the anchor. The cleanest assertion: stub
  // Date.now, kick the interval callback manually by calling
  // applyMetaSummary() again with the same anchor (production code
  // path is identical — startRuntimeTicker() renders once
  // immediately before the interval).
  withClock(NOW_MS + 5000, () => {
    hooks.applyMetaSummary({
      ok: true,
      status: 'running',
      started_at: startedIso,
      runtime_seconds: 35,
    });
  });
  assert('running +5s: runtime text = "35s"',
    runtimeVal && runtimeVal.textContent === '35s',
    'got: ' + (runtimeVal && runtimeVal.textContent));

  // Simulate +10s elapsed.
  withClock(NOW_MS + 10000, () => {
    hooks.applyMetaSummary({
      ok: true,
      status: 'running',
      started_at: startedIso,
      runtime_seconds: 40,
    });
  });
  assert('running +10s: runtime text = "40s"',
    runtimeVal && runtimeVal.textContent === '40s',
    'got: ' + (runtimeVal && runtimeVal.textContent));

  // Status flips to done → ticker should stop, data-started-at removed.
  hooks.applyMetaSummary({
    ok: true,
    status: 'done',
    started_at: startedIso,
    runtime_seconds: 42,
  });
  assert('done: runtime ticker stopped',
    hooks.getRuntimeTickerActive() === false);
  assert('done: data-started-at removed from row',
    runtimeRow && !runtimeRow.hasAttribute('data-started-at'));
  assert('done: data-started-at removed from value el',
    runtimeVal && !runtimeVal.hasAttribute('data-started-at'));
  assert('done: runtime text = server value "42s"',
    runtimeVal && runtimeVal.textContent === '42s',
    'got: ' + (runtimeVal && runtimeVal.textContent));

  // Re-arm: running again → ticker restarts cleanly.
  withClock(NOW_MS + 60000, () => {
    hooks.applyMetaSummary({
      ok: true,
      status: 'running',
      started_at: startedIso,
      runtime_seconds: 90,
    });
  });
  assert('re-arm: ticker active again',
    hooks.getRuntimeTickerActive() === true);
  assert('re-arm: runtime text = "1m 30s"',
    runtimeVal && runtimeVal.textContent === '1m 30s',
    'got: ' + (runtimeVal && runtimeVal.textContent));

  // resetMetaSummary() (modal close / re-open) → ticker stops.
  hooks.resetMetaSummary();
  assert('reset: runtime ticker stopped',
    hooks.getRuntimeTickerActive() === false);
  assert('reset: data-started-at cleared',
    runtimeRow && !runtimeRow.hasAttribute('data-started-at'));

  // Non-running statuses with no started_at (e.g. pending) → no
  // ticker, no data-started-at stamp, no runtime row.
  hooks.applyMetaSummary({
    ok: true,
    status: 'pending',
    started_at: null,
    runtime_seconds: null,
  });
  assert('pending: ticker stays stopped',
    hooks.getRuntimeTickerActive() === false);
  assert('pending: no data-started-at',
    runtimeRow && !runtimeRow.hasAttribute('data-started-at'));

  // Cleanup belt + braces.
  hooks.stopRuntimeTicker();
  hooks.resetMetaSummary();
}

// Template attribute presence — sanity-check that the inline
// templates/index.html scaffolding still wires up the meta rows the
// runtime ticker depends on. We don't load Flask here, just grep the
// raw file for the element IDs and confirm they exist alongside the
// data-started-at consumer in live-log.js. Cheap, catches the case
// where someone renames the row IDs out from under the ticker.
console.log('\nruntime ticker — template element IDs present');
{
  const templatePath = path.resolve(STATIC_DIR, '..', 'templates', 'index.html');
  const tmpl = fs.readFileSync(templatePath, 'utf8');
  assert('template has #log-meta-row-runtime',
    tmpl.includes('id="log-meta-row-runtime"'));
  assert('template has #log-meta-runtime',
    tmpl.includes('id="log-meta-runtime"'));
  assert('live-log.js sets data-started-at on runtime row',
    liveLogSrc.includes("setAttribute('data-started-at'"));
  assert('live-log.js starts runtime ticker for running items',
    liveLogSrc.includes('startRuntimeTicker'));
  assert('live-log.js exposes RUNTIME_TICK_MS',
    liveLogSrc.includes('RUNTIME_TICK_MS'));
}

console.log('\n--------------------------------------------------------------');
if (failures) {
  console.error(failures + ' assertion(s) failed.');
  process.exit(1);
} else {
  console.log('All assertions passed.');
}
