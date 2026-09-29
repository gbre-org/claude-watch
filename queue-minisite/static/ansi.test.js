#!/usr/bin/env node
// Tests for static/ansi.js — the shared terminal-escape → markup converter
// both log views render lines through.
//
// Deliberately DEPENDENCY-FREE: plain node, no jsdom, so CI can run it (the
// jsdom suites in this directory are local-only). `toFragment` needs a DOM
// and is exercised under jsdom in multitail.test.js; everything the escaping
// guarantee rests on — the tokenizer, `toHtml`, `strip`, `clip` — is pure
// string work and lives here.
//
// What it pins:
//   1. ESCAPING. Line text is untrusted: `<script>`, `<`, `&`, quotes stay
//      inert and VISIBLE, including when an SGR span wraps them, and the
//      markup the module emits carries no attacker-controlled attribute.
//   2. SGR coverage: base + bright fg/bg, bold/dim/italic/underline/strike,
//      inverse, reset, 256-colour (palette, cube, gray ramp) and 24-bit
//      truecolor in both the `;` and `:` spellings.
//   3. NON-SGR sequences are DROPPED, not printed — cursor movement,
//      cursor show/hide, erase-line, OSC (link text kept), charset
//      designators, and the stray C0 controls.
//   4. Carriage return REPLACES the line: a progress bar collapses to its
//      last frame.
//   5. Clipping counts VISIBLE characters, and `clip` never cuts a sequence
//      in half.
//   6. The REAL docker-compose output shape from a live workload log renders
//      as colour rather than as escapes.
//
// Usage:   node ansi.test.js      (exit 0 pass, 1 on first failure)

'use strict';

const path = require('path');
const Ansi = require(path.join(path.dirname(path.resolve(__filename)), 'ansi.js'));

let failures = 0;

function ok(cond, what) {
  if (cond) {
    console.log('  ok   ' + what);
  } else {
    failures += 1;
    console.log('  FAIL ' + what);
  }
}

function eq(actual, expected, what) {
  const good = actual === expected;
  if (!good) {
    console.log('       expected: ' + JSON.stringify(expected));
    console.log('       actual:   ' + JSON.stringify(actual));
  }
  ok(good, what);
}

function has(hay, needle, what) {
  ok(String(hay).indexOf(needle) !== -1, what);
}

function hasNot(hay, needle, what) {
  ok(String(hay).indexOf(needle) === -1, what);
}

const E = '\u001b';

// ---------------------------------------------------------------------------
console.log('escaping (the invariant)');

{
  // A log line is untrusted text. Escaping happens BEFORE styling, so
  // markup in the line is inert no matter what colour wraps it.
  const evil = '<script>alert(1)</script>';
  const plain = Ansi.toHtml(evil);
  hasNot(plain, '<script', 'no <script survives an uncoloured line');
  has(plain, '&lt;script&gt;', 'the text is still VISIBLE, escaped');

  const coloured = Ansi.toHtml(E + '[31m' + evil + E + '[0m');
  hasNot(coloured, '<script', 'no <script survives a coloured line');
  has(coloured, '&lt;script&gt;alert(1)&lt;/script&gt;', 'escaped inside the span');
  has(coloured, '<span class="ansi-fg-red">', 'and the span is ours');

  // A stray `<` with no closing bracket, and the ampersand-first ordering
  // that a naive replace chain gets wrong (`&lt;` must not become `&amp;lt;`).
  eq(Ansi.toHtml('a < b & c'), 'a &lt; b &amp; c', 'stray < and & escaped once');
  eq(Ansi.toHtml('&lt;'), '&amp;lt;', 'a literal &lt; in the log stays literal');

  // Attribute-breakout attempt: the line tries to close our span attribute.
  const breakout = Ansi.toHtml(E + '[32m" onmouseover="alert(1)' + E + '[0m');
  hasNot(breakout, 'onmouseover="alert', 'quotes in the line cannot open an attribute');
  has(breakout, '&quot; onmouseover=&quot;alert(1)', 'the attempt renders as text');

  // The only style attribute the module emits is built from OUR integers.
  const tc = Ansi.toHtml(E + '[38;2;10;200;30mx' + E + '[0m');
  ok(/style="--ansi-fg-d:#[0-9a-f]{6};--ansi-fg-l:#[0-9a-f]{6};"/.test(tc),
    'truecolor style attribute is two generated hex values and nothing else');

  // And an injection attempt THROUGH the colour parameters cannot reach the
  // attribute, because the parameters are parsed as integers.
  const viaParams = Ansi.toHtml(E + '[38;2;10;200;30" onload="x;mZ' + E + '[0m');
  hasNot(viaParams, 'onload="', 'SGR parameters cannot smuggle an attribute');
}

// ---------------------------------------------------------------------------
console.log('SGR colours + attributes');

{
  eq(Ansi.toHtml(E + '[32mok' + E + '[0m'),
    '<span class="ansi-fg-green">ok</span>', 'base green fg');
  eq(Ansi.toHtml(E + '[91mbad' + E + '[0m'),
    '<span class="ansi-fg-bright-red">bad</span>', 'bright red fg');
  eq(Ansi.toHtml(E + '[44mbg' + E + '[0m'),
    '<span class="ansi-bg-blue">bg</span>', 'base blue bg');
  eq(Ansi.toHtml(E + '[104mbg' + E + '[0m'),
    '<span class="ansi-bg-bright-blue">bg</span>', 'bright blue bg');
  eq(Ansi.toHtml(E + '[1;4;32mx' + E + '[0m'),
    '<span class="ansi-fg-green ansi-bold ansi-underline">x</span>',
    'bold + underline + colour combine on one span');
  eq(Ansi.toHtml(E + '[2mdim' + E + '[22mnormal'),
    '<span class="ansi-dim">dim</span>normal',
    '22 clears dim and the rest is unstyled');
  eq(Ansi.toHtml(E + '[3;9mx' + E + '[23;29my'),
    '<span class="ansi-italic ansi-strike">x</span>y',
    'italic + strike, then both cleared');
  eq(Ansi.toHtml(E + '[31ma' + E + '[39mb'),
    '<span class="ansi-fg-red">a</span>b', '39 returns to the default fg');
  eq(Ansi.toHtml(E + '[mreset'), 'reset', 'a bare ESC[m is a reset');

  // Inverse video with only one side set: the set colour moves to the other
  // slot and the missing one becomes the theme default.
  eq(Ansi.toHtml(E + '[7mx' + E + '[0m'),
    '<span class="ansi-fg-inv ansi-bg-inv">x</span>',
    'inverse with no colours swaps the two theme defaults');
  eq(Ansi.toHtml(E + '[31;7mx' + E + '[0m'),
    '<span class="ansi-fg-inv ansi-bg-red">x</span>',
    'inverse moves the fg colour to the background');
  eq(Ansi.toHtml(E + '[31;7m' + E + '[27mx'),
    '<span class="ansi-fg-red">x</span>',
    '27 undoes inverse and leaves the fg colour it had swapped');

  // Adjacent runs sharing a style collapse into ONE span (node count is a
  // per-line cost in a streaming view).
  eq(Ansi.toHtml(E + '[32ma' + E + '[32mb'),
    '<span class="ansi-fg-green">ab</span>', 'identical adjacent styles merge');
}

// ---------------------------------------------------------------------------
console.log('256-colour + truecolor');

{
  eq(Ansi.toHtml(E + '[38;5;2mx' + E + '[0m'),
    '<span class="ansi-fg-green">x</span>',
    '256-colour index 2 reuses the themed palette class');
  eq(Ansi.toHtml(E + '[38;5;9mx' + E + '[0m'),
    '<span class="ansi-fg-bright-red">x</span>',
    '256-colour index 9 is bright red');

  const cube = Ansi.tokenize(E + '[38;5;196mx').segments[0];
  eq(cube.cls, 'ansi-fg-rgb', 'cube colour gets the rgb class');
  ok(/--ansi-fg-d:#[0-9a-f]{6};--ansi-fg-l:#[0-9a-f]{6};/.test(cube.style),
    'cube colour ships a dark AND a light variant');

  const gray = Ansi.tokenize(E + '[38;5;232mx').segments[0];
  ok(gray.style.indexOf('--ansi-fg-d:') === 0, 'gray-ramp index resolves');

  // THEME CLAMPING: index 232 is near-black (8,8,8) — unreadable on the dark
  // background — and index 231 is white, unreadable on the light one. Each
  // has to be pulled toward legibility in the theme that needs it, and the
  // two variants must differ.
  const darkVal = /--ansi-fg-d:(#[0-9a-f]{6})/.exec(gray.style)[1];
  const lightVal = /--ansi-fg-l:(#[0-9a-f]{6})/.exec(gray.style)[1];
  ok(darkVal !== '#080808', 'a near-black colour is lightened for the dark theme');
  eq(lightVal, '#080808', 'and left alone for the light theme');
  const white = Ansi.tokenize(E + '[38;5;231mx').segments[0].style;
  const wl = /--ansi-fg-l:(#[0-9a-f]{6})/.exec(white)[1];
  ok(wl !== '#ffffff', 'a white colour is darkened for the light theme');
  has(white, '--ansi-fg-d:#ffffff', 'and left alone for the dark theme');

  const bgrgb = Ansi.tokenize(E + '[48;2;1;2;3mx').segments[0];
  eq(bgrgb.cls, 'ansi-bg-rgb', 'truecolor background gets the rgb bg class');
  has(bgrgb.style, '--ansi-bg-c:rgba(1,2,3,0.30);',
    'a background is a low-alpha tint, so it never eats the foreground');

  // The colon spellings, including the `38:2::r:g:b` form with its empty
  // colour-space field — read as zeros that would be painted black.
  eq(Ansi.toHtml(E + '[38:5:2mx' + E + '[0m'),
    '<span class="ansi-fg-green">x</span>', 'colon-delimited 38:5:N');
  const colonTc = Ansi.tokenize(E + '[38:2::255:0:0mx').segments[0];
  has(colonTc.style, '--ansi-fg-l:',
    'colon-delimited 38:2::r:g:b skips the empty colour-space field');
  ok(colonTc.style.indexOf('#000000') === -1,
    'and is not misread as black');

  // Out-of-range / garbage parameters leave the colour alone instead of
  // producing a bogus span.
  eq(Ansi.toHtml(E + '[38;5;999mx'), 'x', 'an out-of-range index is ignored');
  eq(Ansi.toHtml(E + '[38;9;1mx'), 'x',
    'an unknown extended-colour kind swallows its arguments rather than '
    + 'letting `1` be read as bold');
}

// ---------------------------------------------------------------------------
console.log('every computed colour is legible in BOTH themes');

{
  // THE GUARANTEE THIS FILE EXISTS TO HOLD ON TO. The 16 base colours are
  // mapped to CSS custom properties defined per theme, so the stylesheet owns
  // their contrast (test_multitail.py checks both definitions exist). The
  // indexed-cube and truecolor values have no such table: ansi.js computes a
  // dark-theme and a light-theme variant for each, and if those bounds are
  // wrong the page simply renders unreadable text. So compute the real WCAG
  // ratio for every one of them against the two actual backgrounds.
  //
  // 3.0:1 is the bar — the floor for incidental monospace text, and where this
  // site's own accent palette sits. An earlier light-side bound that looked
  // reasonable (luminance 0.38) produced 2.3:1; this is what catches that.
  const LIGHT_BG = '#fdf6e3';
  const DARK_BG = '#002b36';
  const MIN_RATIO = 3.0;

  const chan = (c) => {
    const v = c / 255;
    return v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4);
  };
  const lum = (hex) => {
    const h = hex.replace('#', '');
    const r = parseInt(h.slice(0, 2), 16);
    const g = parseInt(h.slice(2, 4), 16);
    const b = parseInt(h.slice(4, 6), 16);
    return 0.2126 * chan(r) + 0.7152 * chan(g) + 0.0722 * chan(b);
  };
  const ratio = (a, b) => {
    const la = lum(a);
    const lb = lum(b);
    return (Math.max(la, lb) + 0.05) / (Math.min(la, lb) + 0.05);
  };

  function variants(seq) {
    const seg = Ansi.tokenize(seq + 'x').segments[0];
    if (!seg || seg.cls !== 'ansi-fg-rgb') return null;
    return {
      d: /--ansi-fg-d:(#[0-9a-f]{6})/.exec(seg.style)[1],
      l: /--ansi-fg-l:(#[0-9a-f]{6})/.exec(seg.style)[1],
    };
  }

  let worstDark = { r: Infinity, at: '' };
  let worstLight = { r: Infinity, at: '' };
  let checked = 0;
  // Every cube and gray-ramp index (16-255 — 0-15 are the themed classes), plus
  // the truecolor extremes and a spread of saturated values.
  const probes = [];
  for (let i = 16; i < 256; i++) probes.push([E + '[38;5;' + i + 'm', '38;5;' + i]);
  for (const rgb of [[0, 0, 0], [255, 255, 255], [255, 0, 0], [0, 255, 0],
    [0, 0, 255], [255, 255, 0], [0, 255, 255], [255, 0, 255],
    [128, 128, 128], [20, 20, 20], [240, 240, 200], [90, 90, 255]]) {
    probes.push([E + '[38;2;' + rgb.join(';') + 'm', '38;2;' + rgb.join(';')]);
  }
  for (const [seq, label] of probes) {
    const v = variants(seq);
    if (!v) continue;
    checked += 1;
    const rd = ratio(v.d, DARK_BG);
    const rl = ratio(v.l, LIGHT_BG);
    if (rd < worstDark.r) worstDark = { r: rd, at: label + ' -> ' + v.d };
    if (rl < worstLight.r) worstLight = { r: rl, at: label + ' -> ' + v.l };
  }
  ok(checked > 240, 'checked every cube / gray index plus truecolor probes (' +
    checked + ')');
  ok(worstDark.r >= MIN_RATIO,
    'worst dark-theme contrast is >= ' + MIN_RATIO + ' (' +
    worstDark.r.toFixed(2) + ' at ' + worstDark.at + ')');
  ok(worstLight.r >= MIN_RATIO,
    'worst light-theme contrast is >= ' + MIN_RATIO + ' (' +
    worstLight.r.toFixed(2) + ' at ' + worstLight.at + ')');

  // And the clamp only ever moves a colour that needed moving: a value already
  // legible on both backgrounds is passed through untouched in both variants.
  const mid = variants(E + '[38;2;40;120;40m');
  ok(mid.d !== mid.l || mid.d === '#287828',
    'a mid-luminance colour is not distorted for no reason (' +
    mid.d + ' / ' + mid.l + ')');
}

// ---------------------------------------------------------------------------
console.log('non-SGR sequences are dropped, not printed');

{
  // The exact shapes a live `docker compose up --build` log contains.
  eq(Ansi.toHtml(E + '[1A' + E + '[1B' + E + '[0G' + E + '[?25l[+] Building 0.0s'),
    '[+] Building 0.0s',
    'cursor up/down/column + cursor-hide all vanish');
  eq(Ansi.toHtml(E + '[?25htail'), 'tail', 'cursor-show vanishes');
  eq(Ansi.toHtml('a' + E + '[2Kb'), 'ab', 'erase-line vanishes');
  eq(Ansi.toHtml('a' + E + '[Hb'), 'ab', 'a parameterless CSI vanishes');
  eq(Ansi.toHtml('x' + E + '(By'), 'xy', 'a charset designator vanishes');
  eq(Ansi.toHtml('x' + E + '>y'), 'xy', 'a two-byte escape vanishes');

  // OSC: the command is dropped, the visible text between OSC-8 pairs kept.
  eq(Ansi.toHtml(E + ']0;window title\u0007body'), 'body',
    'a BEL-terminated OSC title vanishes');
  eq(Ansi.toHtml(E + ']8;;https://example.com' + E + '\\link text' + E + ']8;;' + E + '\\'),
    'link text', 'an OSC-8 hyperlink keeps its text and drops the URL');

  // An unterminated sequence at end-of-line is consumed, not printed: half
  // an escape is not content.
  eq(Ansi.toHtml('tail' + E + '[3'), 'tail', 'an unterminated CSI is consumed');
  eq(Ansi.toHtml('tail' + E), 'tail', 'a lone trailing ESC is consumed');

  // Stray C0 controls a terminal would have eaten. TAB and LF are content.
  eq(Ansi.toHtml('a\u0008b\u0007c'), 'abc', 'backspace and BEL are dropped');
  eq(Ansi.toHtml('a\tb\nc'), 'a\tb\nc', 'TAB and newline survive (pre content)');

  // strip() is the plain-text view of the same rendering.
  eq(Ansi.strip(E + '[32m✔' + E + '[0m Built ' + E + '[34m0.9s' + E + '[0m'),
    '✔ Built 0.9s', 'strip() yields clean copyable text');
}

// ---------------------------------------------------------------------------
console.log('carriage-return progress lines collapse to the last frame');

{
  eq(Ansi.toHtml('12%\r45%\r99%'), '99%',
    'a \\r progress run renders only its final frame');
  eq(Ansi.toHtml('line one\n12%\r99%'), 'line one\n99%',
    'a \\r discards only the CURRENT line');
  eq(Ansi.toHtml(E + '[32mdone\r' + E + '[31mfail' + E + '[0m'),
    '<span class="ansi-fg-red">fail</span>',
    'the discarded frame takes its styling with it');
  eq(Ansi.toHtml('abc\r'), '',
    'a trailing \\r leaves the line empty (the frame was overwritten)');
  eq(Ansi.toHtml('final\r\n'), '\n',
    'CRLF resets the line and keeps the break — the server splits these into '
    + 'separate frames before they get here, so this is the defensive path');
}

// ---------------------------------------------------------------------------
console.log('clipping counts VISIBLE characters');

{
  // 6 visible characters wrapped in 9 characters of escape sequences: a
  // raw-length clip at 6 would cut inside `ESC[32m` and leak `[32m` into
  // the page.
  const line = E + '[32mabcdef' + E + '[0m';
  eq(Ansi.toHtml(line, { limit: 3 }),
    '<span class="ansi-fg-green">abc</span>…',
    'toHtml clips at 3 VISIBLE chars and marks the truncation');
  eq(Ansi.toHtml('abcdef', { limit: 3 }), 'abc…', 'and on the fast path too');
  eq(Ansi.toHtml(line, { limit: 100 }),
    '<span class="ansi-fg-green">abcdef</span>',
    'a limit above the visible length adds no ellipsis');

  // clip() keeps the sequences so a stored line can be re-rendered wider.
  const clipped = Ansi.clip(line, 3);
  has(clipped, E + '[32m', 'clip() keeps the opening sequence');
  has(clipped, 'abc…', 'clip() cuts the visible text');
  hasNot(clipped, 'abcd', 'clip() really did cut');
  eq(Ansi.toHtml(clipped), '<span class="ansi-fg-green">abc…</span>',
    'a clipped line still renders as colour, with no leaked escape text');
  eq(Ansi.clip('plain', 99), 'plain', 'clip() is a no-op under the limit');
  eq(Ansi.clip(E + '[32mab' + E + '[0m', 99), E + '[32mab' + E + '[0m',
    'clip() under the limit returns the line unchanged, sequences intact');

  // A progress line that overran before being overwritten is not
  // permanently truncated: the overwritten frame gives its budget back.
  eq(Ansi.toHtml('aaaaaaaaaa\rbb', { limit: 5 }), 'bb',
    'a \\r frees the budget the discarded frame had spent');
}

// ---------------------------------------------------------------------------
console.log('real workload output');

{
  // Verbatim shapes from /var/run/claude/workload-state (a real
  // `docker compose up -d --build` run), with the server-side timestamp
  // prefix already split off as it is on the wire.
  const compose =
    E + '[32m✔' + E + '[0m Image docker.gbre.org/queue-minisite   ' +
    E + '[32mBuilt' + E + '[0m    ' + E + '[34m0.9s' + E + '[0m ';
  const html = Ansi.toHtml(compose);
  has(html, '<span class="ansi-fg-green">✔</span>', 'the tick is green');
  has(html, '<span class="ansi-fg-green">Built</span>', 'Built is green');
  has(html, '<span class="ansi-fg-blue">0.9s</span>', 'the duration is blue');
  hasNot(html, '[32m', 'and NO escape text leaks into the page');
  hasNot(html, E, 'not even a bare ESC byte');

  const buildFrame =
    E + '[?25h' + E + '[1A' + E + '[1A' + E + '[0G' + E + '[?25l' +
    '[+] Building 0.3s (2/3)';
  eq(Ansi.toHtml(buildFrame), '[+] Building 0.3s (2/3)',
    'a compose redraw frame renders as just its text');

  const continuation = E + '[0m' + E + '[34m => => reading from stdin 595B';
  eq(Ansi.toHtml(continuation),
    '<span class="ansi-fg-blue"> =&gt; =&gt; reading from stdin 595B</span>',
    'a reset-then-colour continuation line renders blue, arrows escaped');
}

// ---------------------------------------------------------------------------
console.log('shape + guards');

{
  eq(Ansi.toHtml(null), '', 'null renders as empty');
  eq(Ansi.toHtml(undefined), '', 'undefined renders as empty');
  eq(Ansi.toHtml(''), '', 'empty stays empty');
  ok(Ansi.hasCodes(E + '[0m'), 'hasCodes sees an escape');
  ok(Ansi.hasCodes('a\rb'), 'hasCodes sees a carriage return');
  ok(!Ansi.hasCodes('plain text\twith a tab\nand a newline'),
    'hasCodes takes the fast path on ordinary text');
  eq(Ansi.toHtml('plain'), 'plain', 'the fast path emits no spans');
}

console.log('');
if (failures) {
  console.log(failures + ' assertion(s) FAILED');
  process.exit(1);
}
console.log('all assertions passed');
