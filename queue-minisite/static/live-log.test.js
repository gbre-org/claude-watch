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

console.log('\n--------------------------------------------------------------');
if (failures) {
  console.error(failures + ' assertion(s) failed.');
  process.exit(1);
} else {
  console.log('All assertions passed.');
}
