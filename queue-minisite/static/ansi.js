/* static/ansi.js — terminal escape sequences → themed, ESCAPED markup.
 *
 * Most of what this site tails is colourised CLI output: docker compose,
 * cargo, pytest, ffmpeg. Those producers write SGR escape sequences
 * (`ESC[32m` …) into their stdout, the workload wrapper stamps and appends
 * the bytes verbatim, and the log views used to render the sequences as
 * literal text — `[32m✔[0m Image … [34m0.9s[0m` instead of a green tick.
 * This module is the ONE converter both log views use, so a colourised line
 * looks the same in a multitail pane and in the single-item log modal.
 *
 * ESCAPING IS THE INVARIANT, AND IT COMES FIRST.
 * A log line is untrusted text. The tokenizer splits the RAW string into
 * plain-text runs plus a style state; the text of every run is HTML-escaped
 * (or written with textContent / createTextNode) and only then wrapped in a
 * span whose class list and style come from OUR OWN tables — never from the
 * line. There is no path by which line content reaches markup unescaped, so
 * a line containing `<script>` stays inert and visible. `toHtml` never
 * interpolates line text into an attribute, and the only attribute values it
 * emits are hex / rgba strings built from clamped integers.
 *
 * WHAT IS RENDERED, AND WHAT IS DROPPED
 *   - SGR (`ESC[…m`) is the payload: the 8 base colours and their bright
 *     variants (30-37 / 90-97, 40-47 / 100-107), 256-colour and 24-bit
 *     truecolor (`38;5;N`, `38;2;R;G;B`, and the colon-delimited spellings),
 *     bold / dim / italic / underline / strike / inverse, and every reset.
 *   - Every OTHER escape sequence is DROPPED, not rendered: cursor movement
 *     and cursor show/hide (`ESC[1A`, `ESC[0G`, `ESC[?25l`), erase-line,
 *     OSC strings (including OSC-8 hyperlinks — the link TEXT survives, the
 *     URL wrapper does not), charset designators, DCS/APC/PM strings. They
 *     address a terminal grid this viewer does not have; the honest
 *     rendering of "move the cursor up one row" in a log transcript is
 *     nothing at all.
 *   - A carriage return means "redraw this line from column 0", so text
 *     after a `\r` REPLACES what came before it on that line rather than
 *     appending. A progress bar therefore collapses to its final frame,
 *     which is what the terminal would have left on screen. (Most `\r`s
 *     never get here: the server already splits plain-text tails on `\r`
 *     and ships each frame with `transient: true` so the front end can
 *     replace the row in place. This handles the ones that survive inside a
 *     single frame.)
 *   - Other C0 controls (backspace, BEL, form feed, NUL, DEL) are dropped.
 *     TAB and NEWLINE are kept: they are content in a `<pre>`.
 *
 * COLOUR STATE DOES NOT CARRY ACROSS LINES. Each call renders one frame,
 * starting from a clean state. Both views trim their head (400 lines/pane,
 * MAX_LINES in the modal) and a multitail pane joins a stream mid-flight, so
 * there is no reliable "previous line" to inherit from — and a sticky
 * unterminated colour would paint the rest of the pane. A producer that
 * opens a colour on one line and closes it on the next (docker compose does
 * exactly this) loses the colour on the continuation line. That is the
 * deliberate trade: a wrong-but-bounded line beats a wrong-forever pane.
 *
 * THEMING. The site is Solarized light/dark via prefers-color-scheme, and
 * terminal colours chosen against a dark background are frequently
 * unreadable on a light one. So nothing here emits a raw ANSI RGB value for
 * the 16 base colours: each maps to a CSS custom property that style.css
 * defines TWICE, once per theme, at a contrast ratio that reads on that
 * theme's background. Indexed-cube / truecolor values have no such table, so
 * the module computes two variants per colour — one lightened enough to read
 * on the dark background, one darkened enough to read on the light one — and
 * ships both as custom properties on the span; the stylesheet picks. Results
 * are memoised, so a stream that reuses a handful of colours pays for each
 * one once.
 *
 * PERFORMANCE. One left-to-right pass per line, no backtracking, no
 * re-parse of the buffer. A line with no escapes and no stray controls takes
 * a single regex test and returns the escaped string / one text node, which
 * is the overwhelmingly common case. Adjacent runs sharing a style collapse
 * into one span, so a fully-coloured line is a handful of nodes rather than
 * one per character.
 */
(function () {
  'use strict';

  const ESC = '\u001b';

  // Does this string contain anything a terminal would have interpreted
  // rather than printed? The fast-path gate: false means "escape the text
  // and you are done". TAB (0x09) and LF (0x0a) are deliberately absent —
  // they are content.
  const CONTROL_RE =
    /[\u0000-\u0008\u000b-\u001f\u007f]/;  // eslint-disable-line no-control-regex

  // C0 controls that are silently dropped when they appear in text (they
  // address a terminal, and there is no terminal here). Same set as
  // CONTROL_RE minus the ones handled structurally (ESC, CR).
  function isDroppedControl(code) {
    if (code === 9 || code === 10) return false;        // TAB, LF: content
    if (code === 13 || code === 27) return false;       // CR, ESC: structural
    return code < 32 || code === 127;
  }

  const BASE_NAMES = [
    'black', 'red', 'green', 'yellow', 'blue', 'magenta', 'cyan', 'white',
  ];

  // The 6 levels of the xterm 6x6x6 colour cube, and the 24-step gray ramp.
  const CUBE_LEVELS = [0, 95, 135, 175, 215, 255];

  // --- HTML escaping ------------------------------------------------------
  // Quotes are escaped too. Nothing here interpolates line text into an
  // attribute, but escaping them costs nothing and removes the question.
  function escapeHtml(str) {
    return String(str)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#39;');
  }

  // --- theme-aware colour derivation for cube / truecolor values ----------

  function clamp255(n) {
    n = Math.round(Number(n));
    if (!isFinite(n) || n < 0) return 0;
    return n > 255 ? 255 : n;
  }

  function hex2(n) {
    const s = clamp255(n).toString(16);
    return s.length === 1 ? '0' + s : s;
  }

  function toHex(r, g, b) {
    return '#' + hex2(r) + hex2(g) + hex2(b);
  }

  function channelLum(c) {
    const v = c / 255;
    return v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4);
  }

  function luminance(r, g, b) {
    return 0.2126 * channelLum(r) + 0.7152 * channelLum(g) + 0.0722 * channelLum(b);
  }

  // Lower / upper relative-luminance bounds a colour must satisfy to be
  // legible on the Solarized dark (#002b36, luminance 0.020) and light
  // (#fdf6e3, luminance 0.923) backgrounds.
  //
  // Both are DERIVED FROM A CONTRAST TARGET, not picked by eye: the WCAG ratio
  // is (Llighter + 0.05) / (Ldarker + 0.05), so against those two backgrounds
  // these bounds put the clamped result at 3.2:1 and 3.4:1 respectively — the
  // floor for incidental monospace text, and where this site's own accent
  // palette already sits. Loosening either one is how a "readable in both
  // themes" guarantee quietly stops being true: 0.38 on the light side, which
  // looks like a reasonable number, is 2.3:1.
  const DARK_MIN_LUM = 0.18;
  const LIGHT_MAX_LUM = 0.24;

  function mixToward(r, g, b, tr, tg, tb, t) {
    return [r + (tr - r) * t, g + (tg - g) * t, b + (tb - b) * t];
  }

  // Walk toward white (dark theme) / black (light theme) in fixed steps
  // until the luminance bound is met. Bounded at 20 steps, so this cannot
  // spin on a pathological input.
  function legible(r, g, b, wantDark) {
    let cur = [r, g, b];
    for (let i = 0; i < 20; i++) {
      const lum = luminance(cur[0], cur[1], cur[2]);
      if (wantDark ? lum >= DARK_MIN_LUM : lum <= LIGHT_MAX_LUM) break;
      cur = wantDark
        ? mixToward(cur[0], cur[1], cur[2], 255, 255, 255, 0.12)
        : mixToward(cur[0], cur[1], cur[2], 0, 0, 0, 0.12);
    }
    return toHex(cur[0], cur[1], cur[2]);
  }

  // hex key -> { d, l } so a stream that reuses colours parses each once.
  const themedCache = new Map();

  function themedPair(r, g, b) {
    const key = toHex(r, g, b);
    let hit = themedCache.get(key);
    if (hit) return hit;
    hit = { d: legible(r, g, b, true), l: legible(r, g, b, false) };
    themedCache.set(key, hit);
    return hit;
  }

  // --- SGR state ----------------------------------------------------------
  //
  // fg / bg are null (default), { name, bright } for a palette colour,
  // { rgb: [r, g, b] } for cube / truecolor, or { swap: 'fg' | 'bg' } — the
  // inverse-video sentinels meaning "whatever the theme's default fg / bg
  // is". Keeping inverse in the state (rather than as a class) is what lets
  // `ESC[7m` work when only one of the two colours was set.

  function newState() {
    return {
      fg: null, bg: null, bold: false, dim: false, italic: false,
      underline: false, strike: false, inverse: false,
    };
  }

  function resetState(st) {
    st.fg = null; st.bg = null;
    st.bold = false; st.dim = false; st.italic = false;
    st.underline = false; st.strike = false; st.inverse = false;
  }

  function indexedColour(n) {
    n = Number(n);
    if (!isFinite(n) || n < 0 || n > 255) return null;
    if (n < 8) return { name: BASE_NAMES[n], bright: false };
    if (n < 16) return { name: BASE_NAMES[n - 8], bright: true };
    if (n < 232) {
      const c = n - 16;
      return {
        rgb: [
          CUBE_LEVELS[Math.floor(c / 36) % 6],
          CUBE_LEVELS[Math.floor(c / 6) % 6],
          CUBE_LEVELS[c % 6],
        ],
      };
    }
    const v = 8 + (n - 232) * 10;
    return { rgb: [v, v, v] };
  }

  // Apply one SGR parameter list. `params` is the raw text between `ESC[`
  // and `m`; the colon-delimited spellings (`38:5:N`, `38:2::r:g:b`) are
  // normalised onto the semicolon form, and an empty list means reset.
  function applySgr(st, params) {
    const raw = String(params || '').replace(/:/g, ';');
    if (raw === '') { resetState(st); return; }
    const parts = raw.split(';');
    for (let i = 0; i < parts.length; i++) {
      const tok = parts[i];
      // An empty parameter is a default parameter, i.e. 0 — except inside
      // the `38:2::r:g:b` colour spelling, where it is a skipped field the
      // extended-colour branch consumes itself.
      const n = tok === '' ? 0 : parseInt(tok, 10);
      if (isNaN(n)) continue;
      if (n === 0) { resetState(st); continue; }
      if (n === 1) { st.bold = true; continue; }
      if (n === 2) { st.dim = true; continue; }
      if (n === 3) { st.italic = true; continue; }
      if (n === 4) { st.underline = true; continue; }
      if (n === 7) { st.inverse = true; continue; }
      if (n === 9) { st.strike = true; continue; }
      if (n === 21 || n === 22) { st.bold = false; st.dim = false; continue; }
      if (n === 23) { st.italic = false; continue; }
      if (n === 24) { st.underline = false; continue; }
      if (n === 27) { st.inverse = false; continue; }
      if (n === 29) { st.strike = false; continue; }
      if (n >= 30 && n <= 37) {
        st.fg = { name: BASE_NAMES[n - 30], bright: false };
        continue;
      }
      if (n === 39) { st.fg = null; continue; }
      if (n >= 40 && n <= 47) {
        st.bg = { name: BASE_NAMES[n - 40], bright: false };
        continue;
      }
      if (n === 49) { st.bg = null; continue; }
      if (n >= 90 && n <= 97) {
        st.fg = { name: BASE_NAMES[n - 90], bright: true };
        continue;
      }
      if (n >= 100 && n <= 107) {
        st.bg = { name: BASE_NAMES[n - 100], bright: true };
        continue;
      }
      if (n === 38 || n === 48) {
        // Extended colour. `5;N` = indexed, `2;R;G;B` = truecolor. Empty
        // fields (the `38:2::r:g:b` spelling) are skipped rather than read
        // as zeros, which would paint everything black.
        let j = i + 1;
        while (j < parts.length && parts[j] === '') j++;
        const kind = parseInt(parts[j], 10);
        let colour = null;
        if (kind === 5) {
          let k = j + 1;
          while (k < parts.length && parts[k] === '') k++;
          colour = indexedColour(parts[k]);
          i = k;
        } else if (kind === 2) {
          const vals = [];
          let k = j + 1;
          while (k < parts.length && vals.length < 3) {
            if (parts[k] !== '') vals.push(parseInt(parts[k], 10));
            k++;
          }
          if (vals.length === 3 && !vals.some(isNaN)) {
            colour = { rgb: [clamp255(vals[0]), clamp255(vals[1]), clamp255(vals[2])] };
          }
          i = k - 1;
        } else {
          // Unrecognised extended-colour kind (0/1/3/4 are
          // implementation-defined and take varying argument counts). Its
          // argument count is unknowable, so the REST of the list is
          // consumed: reading the tail as independent attributes is how
          // `38;9;1` silently turns into bold.
          i = parts.length;
        }
        if (colour) {
          if (n === 38) st.fg = colour;
          else st.bg = colour;
        }
        continue;
      }
      // Anything else (overline, framing, ideogram attributes, …) is a
      // no-op: recognised as a parameter, rendered as nothing.
    }
  }

  // --- style spec for the current state -----------------------------------

  function colourClass(prefix, colour) {
    if (!colour) return '';
    if (colour.swap) return prefix + '-inv';
    if (colour.rgb) return prefix + '-rgb';
    return prefix + '-' + (colour.bright ? 'bright-' : '') + colour.name;
  }

  // { cls, style } for the current state. `style` holds ONLY generated
  // custom properties (hex / rgba built from clamped integers); no line
  // content ever reaches it.
  function specOf(st) {
    let fg = st.fg;
    let bg = st.bg;
    if (st.inverse) {
      const oldFg = fg;
      fg = bg || { swap: 'bg' };
      bg = oldFg || { swap: 'fg' };
    }
    const cls = [];
    let style = '';
    const fgCls = colourClass('ansi-fg', fg);
    if (fgCls) cls.push(fgCls);
    const bgCls = colourClass('ansi-bg', bg);
    if (bgCls) cls.push(bgCls);
    if (fg && fg.rgb) {
      const pair = themedPair(fg.rgb[0], fg.rgb[1], fg.rgb[2]);
      style += '--ansi-fg-d:' + pair.d + ';--ansi-fg-l:' + pair.l + ';';
    }
    if (bg && bg.rgb) {
      // A background is a tint behind the text, so it is used at low alpha
      // in both themes — no lightness clamping, and the foreground stays
      // legible whatever the producer picked.
      style += '--ansi-bg-c:rgba(' + clamp255(bg.rgb[0]) + ',' +
        clamp255(bg.rgb[1]) + ',' + clamp255(bg.rgb[2]) + ',0.30);';
    }
    if (st.bold) cls.push('ansi-bold');
    if (st.dim) cls.push('ansi-dim');
    if (st.italic) cls.push('ansi-italic');
    if (st.underline) cls.push('ansi-underline');
    if (st.strike) cls.push('ansi-strike');
    return { cls: cls.join(' '), style: style };
  }

  // --- escape-sequence scanning -------------------------------------------

  // Where does the escape sequence starting at `i` end, and is it an SGR?
  // Returns { end, sgr } — `end` is the index just past the sequence,
  // `sgr` the parameter text for `ESC[…m` and null for everything else.
  // An unterminated sequence at end-of-string consumes the remainder: half
  // an escape is not printable text.
  function scanEscape(text, i) {
    const n = text.length;
    if (i + 1 >= n) return { end: n, sgr: null };
    const c = text[i + 1];
    if (c === '[') {
      let j = i + 2;
      // Parameter bytes 0x30-0x3f (digits, ';', ':', and the private-use
      // markers '?' and '<' '=' '>'), then intermediates 0x20-0x2f, then
      // one final byte 0x40-0x7e.
      const pStart = j;
      while (j < n) {
        const code = text.charCodeAt(j);
        if (code >= 0x30 && code <= 0x3f) j++;
        else break;
      }
      const pEnd = j;
      while (j < n) {
        const code = text.charCodeAt(j);
        if (code >= 0x20 && code <= 0x2f) j++;
        else break;
      }
      if (j >= n) return { end: n, sgr: null };
      const fin = text[j];
      const isSgr = fin === 'm' && pEnd === j &&
        // A private-parameter CSI (`ESC[?…m`) is not an SGR.
        !/[<=>?]/.test(text.slice(pStart, pEnd));
      return { end: j + 1, sgr: isSgr ? text.slice(pStart, pEnd) : null };
    }
    if (c === ']' || c === 'P' || c === 'X' || c === '^' || c === '_') {
      // String sequences (OSC / DCS / SOS / PM / APC): run to ST (`ESC\`)
      // or BEL. The payload is a command for the terminal, never content —
      // for OSC-8 hyperlinks the visible link text sits OUTSIDE it and is
      // kept.
      let j = i + 2;
      while (j < n) {
        if (text.charCodeAt(j) === 7) return { end: j + 1, sgr: null };
        if (text[j] === ESC && text[j + 1] === '\\') return { end: j + 2, sgr: null };
        j++;
      }
      return { end: n, sgr: null };
    }
    if (c === '(' || c === ')' || c === '*' || c === '+' || c === '-' ||
        c === '.' || c === '/' || c === '%' || c === '#' || c === ' ') {
      // Two-byte sequences with one intermediate (charset designators,
      // `ESC # 8`, …): ESC + intermediate + final.
      return { end: Math.min(n, i + 3), sgr: null };
    }
    // Everything else is a two-byte escape (RIS, NEL, IND, ST, …).
    return { end: i + 2, sgr: null };
  }

  function hasCodes(text) {
    if (text === null || text === undefined) return false;
    const s = String(text);
    return s.indexOf(ESC) !== -1 || s.indexOf('\r') !== -1 || CONTROL_RE.test(s);
  }

  // --- tokenizer ----------------------------------------------------------

  // One pass over `text`, producing [{ text, cls, style }] plus a
  // `truncated` flag. `limit`, when given, is a budget in VISIBLE
  // characters — escape sequences do not spend it, which is the whole point
  // of clipping here rather than on the raw string.
  function tokenize(text, opts) {
    const src = (text === null || text === undefined) ? '' : String(text);
    const limit = (opts && typeof opts.limit === 'number' && opts.limit > 0)
      ? opts.limit : Infinity;
    const out = [];
    const st = newState();
    let vis = 0;
    let truncated = false;
    // Index into `out` where the current display line starts, so a `\r` can
    // discard exactly that line's segments. `noMerge` stops the next run
    // from being folded into the segment before a line boundary, which
    // would make that discard impossible to bound.
    let lineStart = 0;
    let noMerge = false;

    function emit(chunk) {
      if (!chunk || truncated) return;
      if (vis + chunk.length > limit) {
        chunk = chunk.slice(0, Math.max(0, limit - vis));
        truncated = true;
        if (!chunk) return;
      }
      const spec = specOf(st);
      const last = (!noMerge && out.length) ? out[out.length - 1] : null;
      if (last && last.cls === spec.cls && last.style === spec.style) {
        last.text += chunk;
      } else {
        out.push({ text: chunk, cls: spec.cls, style: spec.style });
      }
      noMerge = false;
      vis += chunk.length;
    }

    function carriageReturn() {
      while (out.length > lineStart) {
        vis -= out[out.length - 1].text.length;
        out.pop();
      }
      // The discarded frame's characters are back in the budget, so a line
      // that overran before being overwritten is not permanently truncated.
      truncated = vis >= limit;
      noMerge = true;
    }

    // Emit a run of printable text, splitting at the line-structural
    // characters and dropping the C0 controls a terminal would have eaten.
    function emitRun(run) {
      let start = 0;
      for (let k = 0; k < run.length; k++) {
        const code = run.charCodeAt(k);
        if (code === 10) {              // LF: ends the display line
          emit(run.slice(start, k + 1));
          noMerge = true;
          lineStart = out.length;
          start = k + 1;
        } else if (code === 13) {       // CR: redraw from column 0
          emit(run.slice(start, k));
          carriageReturn();
          start = k + 1;
        } else if (isDroppedControl(code)) {
          emit(run.slice(start, k));
          start = k + 1;
        }
      }
      emit(run.slice(start));
    }

    let i = 0;
    while (i < src.length && !truncated) {
      const idx = src.indexOf(ESC, i);
      if (idx === -1) { emitRun(src.slice(i)); break; }
      if (idx > i) emitRun(src.slice(i, idx));
      if (truncated) break;
      const seq = scanEscape(src, idx);
      if (seq.sgr !== null) applySgr(st, seq.sgr);
      i = seq.end > idx ? seq.end : idx + 1;
    }
    return { segments: out, truncated: truncated };
  }

  // --- public renderers ---------------------------------------------------

  // Escaped HTML with the colour spans applied. Safe to drop into any
  // innerHTML sink that previously took `esc(text)`.
  function toHtml(text, opts) {
    const src = (text === null || text === undefined) ? '' : String(text);
    const limit = (opts && typeof opts.limit === 'number' && opts.limit > 0)
      ? opts.limit : 0;
    if (!hasCodes(src)) {
      // Fast path: nothing to interpret. Same output the old `esc()` gave.
      const clipped = (limit && src.length > limit)
        ? src.slice(0, limit) + '…' : src;
      return escapeHtml(clipped);
    }
    const res = tokenize(src, opts);
    let html = '';
    for (const seg of res.segments) {
      const body = escapeHtml(seg.text);
      if (!seg.cls && !seg.style) { html += body; continue; }
      html += '<span class="' + seg.cls + '"' +
        (seg.style ? ' style="' + seg.style + '"' : '') + '>' + body + '</span>';
    }
    if (res.truncated) html += '…';
    return html;
  }

  // The same rendering as a DocumentFragment, for the modules that write
  // text with textContent and never touch innerHTML.
  function toFragment(text, opts) {
    const src = (text === null || text === undefined) ? '' : String(text);
    const doc = (typeof document !== 'undefined') ? document : null;
    if (!doc) return null;
    const frag = doc.createDocumentFragment();
    const limit = (opts && typeof opts.limit === 'number' && opts.limit > 0)
      ? opts.limit : 0;
    if (!hasCodes(src)) {
      const clipped = (limit && src.length > limit)
        ? src.slice(0, limit) + '…' : src;
      frag.appendChild(doc.createTextNode(clipped));
      return frag;
    }
    const res = tokenize(src, opts);
    for (const seg of res.segments) {
      if (!seg.cls && !seg.style) {
        frag.appendChild(doc.createTextNode(seg.text));
        continue;
      }
      const span = doc.createElement('span');
      span.className = seg.cls;
      if (seg.style) span.setAttribute('style', seg.style);
      span.textContent = seg.text;
      frag.appendChild(span);
    }
    if (res.truncated) frag.appendChild(doc.createTextNode('…'));
    return frag;
  }

  // Plain text — every escape sequence removed, `\r` frames collapsed. What
  // a viewer copying a rendered line out of the page gets, and the fallback
  // for anything that wants no markup at all.
  function strip(text, opts) {
    const res = tokenize(text, opts);
    let out = '';
    for (const seg of res.segments) out += seg.text;
    return res.truncated ? out + '…' : out;
  }

  // Truncate to `limit` VISIBLE characters while keeping the escape
  // sequences intact, so a stored line can be re-rendered later at a wider
  // width without having been cut mid-sequence (which is how a clip turns
  // into visible `[32m` garbage).
  function clip(text, limit) {
    const src = (text === null || text === undefined) ? '' : String(text);
    const lim = (typeof limit === 'number' && limit > 0) ? limit : 0;
    if (!lim) return src;
    if (!hasCodes(src)) {
      return src.length > lim ? src.slice(0, lim) + '…' : src;
    }
    let out = '';
    let vis = 0;
    let i = 0;
    while (i < src.length) {
      if (src[i] === ESC) {
        const seq = scanEscape(src, i);
        const end = seq.end > i ? seq.end : i + 1;
        out += src.slice(i, end);
        i = end;
        continue;
      }
      if (vis >= lim) return out + '…';
      out += src[i];
      vis++;
      i++;
    }
    return out;
  }

  const api = {
    hasCodes: hasCodes,
    tokenize: tokenize,
    toHtml: toHtml,
    toFragment: toFragment,
    strip: strip,
    clip: clip,
    escapeHtml: escapeHtml,
  };

  const root = (typeof window !== 'undefined') ? window
    : (typeof globalThis !== 'undefined' ? globalThis : null);
  if (root) root.AnsiText = api;
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
})();
