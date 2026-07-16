#!/usr/bin/env node
// Tests for the COMPACT DENSITY toggle (botchat #1944).
//
// Exercises the two halves of the feature:
//   A. info.js — the density toggle handler: <head> pre-paint restore is
//      reflected on the button, a delegated click flips the html.density-compact
//      class + persists to localStorage, and a stored "compact" survives.
//   B. refresh.js — buildTopbarMetaDOM() renders the density pill in the
//      CURRENT html.density-compact state, so the 5s morphdom merge never drops
//      or flaps it (same durability contract as the source filter), and the
//      html-level class is untouched by a #topbar-meta merge.
//
// Usage:   node density.test.js
// Exit 0 on success, 1 on first failure.

'use strict';

const path = require('path');
const fs = require('fs');

const NODE_MODULES = process.env.QM_NODE_MODULES ||
  '/tmp/queue-minisite-test/node_modules';
const { JSDOM } = require(path.join(NODE_MODULES, 'jsdom'));

const STATIC_DIR = path.dirname(path.resolve(__filename));
const infoSrc = fs.readFileSync(path.join(STATIC_DIR, 'info.js'), 'utf8');
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

// A page fragment mirroring the topbar density pill as painted by the Jinja
// template (aria-pressed / data-density / label seeded from the server, which
// always paints "comfortable" — the <head> guard adds the class before this).
function topbarHTML(compact) {
  return `<header class="topbar"><div class="meta" id="topbar-meta">
    <span class="count count-running">0 running</span>
    <span class="count density-control">
      <span class="density-label">density</span>
      <button type="button" id="density-toggle" class="density-btn"
              aria-pressed="${compact ? 'true' : 'false'}"
              data-density="${compact ? 'compact' : 'comfortable'}">${compact ? 'compact' : 'comfortable'}</button>
    </span>
  </div></header>`;
}

// Boot a jsdom page with the density-compact class optionally pre-applied to
// <html> (simulating the <head> pre-paint restore), a seeded localStorage, and
// info.js evaluated. Returns { window, document }.
function bootInfo(storedValue, preApplyCompact) {
  const cls = preApplyCompact ? ' class="density-compact"' : '';
  const html = `<!doctype html><html${cls}><head></head><body>${topbarHTML(false)}</body></html>`;
  const dom = new JSDOM(html, { runScripts: 'outside-only', url: 'http://localhost/' });
  const { window } = dom;
  if (storedValue !== null && storedValue !== undefined) {
    window.localStorage.setItem('qsite_density', storedValue);
  } else {
    window.localStorage.removeItem('qsite_density');
  }
  window.eval(infoSrc);
  return { window, document: window.document };
}

// --- A. info.js toggle handler ---

console.log('info.js: default (no stored pref) — comfortable, button not pressed');
{
  const { window, document } = bootInfo(null, false);
  const btn = document.getElementById('density-toggle');
  assert('html has no density-compact class',
    !document.documentElement.classList.contains('density-compact'));
  assert('button aria-pressed=false', btn.getAttribute('aria-pressed') === 'false',
    'aria-pressed=' + btn.getAttribute('aria-pressed'));
  assert('button label is comfortable', btn.textContent.trim() === 'comfortable');
}

console.log('info.js: <head> restored compact — button reflects it on load');
{
  const { window, document } = bootInfo('compact', true);
  const btn = document.getElementById('density-toggle');
  assert('html has density-compact class (from head guard)',
    document.documentElement.classList.contains('density-compact'));
  assert('button aria-pressed=true after applyDensityButton',
    btn.getAttribute('aria-pressed') === 'true',
    'aria-pressed=' + btn.getAttribute('aria-pressed'));
  assert('button label is compact', btn.textContent.trim() === 'compact');
  assert('button data-density=compact', btn.getAttribute('data-density') === 'compact');
}

console.log('info.js: click toggles class ON + persists');
{
  const { window, document } = bootInfo(null, false);
  const btn = document.getElementById('density-toggle');
  btn.click();
  assert('html gains density-compact after click',
    document.documentElement.classList.contains('density-compact'));
  assert('localStorage persisted compact',
    window.localStorage.getItem('qsite_density') === 'compact',
    'stored=' + window.localStorage.getItem('qsite_density'));
  assert('button now aria-pressed=true', btn.getAttribute('aria-pressed') === 'true');
  assert('button label now compact', btn.textContent.trim() === 'compact');
}

console.log('info.js: second click toggles class OFF + persists comfortable');
{
  const { window, document } = bootInfo('compact', true);
  const btn = document.getElementById('density-toggle');
  btn.click();
  assert('html loses density-compact after second click',
    !document.documentElement.classList.contains('density-compact'));
  assert('localStorage persisted comfortable',
    window.localStorage.getItem('qsite_density') === 'comfortable',
    'stored=' + window.localStorage.getItem('qsite_density'));
  assert('button now aria-pressed=false', btn.getAttribute('aria-pressed') === 'false');
}

console.log('info.js: click is DELEGATED — survives button replacement');
{
  const { window, document } = bootInfo(null, false);
  // Simulate refresh.js rebuilding #topbar-meta: replace the button node.
  const meta = document.getElementById('topbar-meta');
  meta.innerHTML = topbarHTML(false).replace(/^<header[^>]*><div[^>]*>/, '').replace(/<\/div><\/header>$/, '');
  const btn2 = document.getElementById('density-toggle');
  assert('new button present after rebuild', !!btn2);
  btn2.click();
  assert('delegated click still fires on the rebuilt button',
    document.documentElement.classList.contains('density-compact'));
}

// --- B. refresh.js buildTopbarMetaDOM density pill ---

console.log('refresh.js: buildTopbarMetaDOM renders density pill reflecting html state');
{
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
  const R = dom.window.__queueRefresh;
  assert('__queueRefresh exposed', !!R);

  // Comfortable (no class on <html>): pill renders as comfortable.
  const metaComfortable = R.buildTopbarMetaDOM({ totals: {}, sources: [] });
  const btnC = metaComfortable.querySelector('#density-toggle');
  assert('density pill rendered by buildTopbarMetaDOM', !!btnC);
  assert('pill comfortable when html lacks class',
    btnC && btnC.getAttribute('aria-pressed') === 'false' &&
    btnC.getAttribute('data-density') === 'comfortable');

  // Now apply the class to <html> and rebuild — pill must reflect compact.
  dom.window.document.documentElement.classList.add('density-compact');
  const metaCompact = R.buildTopbarMetaDOM({ totals: {}, sources: [] });
  const btnK = metaCompact.querySelector('#density-toggle');
  assert('pill compact when html has density-compact',
    btnK && btnK.getAttribute('aria-pressed') === 'true' &&
    btnK.getAttribute('data-density') === 'compact' &&
    btnK.textContent.trim() === 'compact');
}

console.log('refresh.js: mergeTopbarMeta keeps the html.density-compact class intact');
{
  const html = `<!doctype html><html class="density-compact"><head></head><body>
    <div class="meta" id="topbar-meta"><span class="count count-running">0 running</span></div>
    <main id="queue-root"></main>
    <div id="action-modal" data-no-morph hidden></div>
    <div id="log-modal" data-no-morph hidden></div>
  </body></html>`;
  const dom = new JSDOM(html, { runScripts: 'dangerously' });
  const s1 = dom.window.document.createElement('script');
  s1.textContent = morphdomSrc; dom.window.document.head.appendChild(s1);
  const s2 = dom.window.document.createElement('script');
  s2.textContent = refreshSrc; dom.window.document.head.appendChild(s2);
  const R = dom.window.__queueRefresh;
  R.mergeTopbarMeta({ totals: { running: 3 }, sources: [] });
  assert('html.density-compact survives a #topbar-meta merge (class lives on <html>)',
    dom.window.document.documentElement.classList.contains('density-compact'));
  const btn = dom.window.document.querySelector('#density-toggle');
  assert('density pill still present after merge', !!btn);
  assert('merged pill reflects compact state',
    btn && btn.getAttribute('aria-pressed') === 'true');
}

if (failures) {
  console.error(`\n${failures} density assertion(s) FAILED.`);
  process.exit(1);
}
console.log('\nAll density assertions passed.');
// refresh.js schedules a setInterval that keeps the jsdom event loop alive;
// exit explicitly like refresh.test.js so the run terminates.
process.exit(0);
