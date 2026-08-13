/*
 * Mobile kiosk toggle for Grafana 13.x — restores the one-tap kiosk entry that
 * Grafana 11.x had in the dashboard toolbar.
 *
 * WHY THIS EXISTS
 * ---------------
 * Grafana 11.2 rendered a ToolbarButton (icon "monitor", title "Enable kiosk
 * mode") in the nav toolbar whenever the top search bar was hidden. Because
 * that hidden state persisted in localStorage (SearchBar_Hidden=true), the
 * steady-state cost of entering kiosk on a phone was ONE tap.
 *
 * Grafana 13.1.3 deleted that button. Kiosk is now only reachable from the
 * profile menu: Profile -> "Enable kiosk mode" = TWO taps, every time, with no
 * persisted state to amortise. (The orphaned `kioskToggle` emotion class still
 * exists in the 13.1.3 bundle with a `breakpoints.down("lg") { display:none }`
 * rule, but NOTHING references it — the component was removed, so there is no
 * feature flag or CSS-only way to bring the button back.)
 *
 * There is also no `GF_*` setting for this, and on mobile there is no way to
 * LEAVE kiosk mode either: exiting is bound to Esc, and phones have no Esc key.
 * This button toggles both directions, so it fixes the exit trap too.
 *
 * WHY IT IS NOT A BUNDLE PATCH
 * ----------------------------
 * Re-adding a real React ToolbarButton would mean *creating* a new JSX element
 * inside minified code — that needs the jsx factory alias, a ToolbarButton
 * module reference, and the chrome-service handle, all of which webpack
 * re-mints on essentially every release. Every other patch in the Dockerfile
 * only ever *rewrites an expression that already exists*, which is far more
 * stable. This script instead depends on two things that are public contract:
 *   1. the `?kiosk` URL parameter (stable across Grafana 6 -> 13), and
 *   2. the `d k` keybinding the Dockerfile already injects (and already guards).
 * No minified identifier appears anywhere below.
 *
 * BEHAVIOUR
 * ---------
 * - Only on dashboard routes (/d/ and /d-solo/).
 * - Only below Grafana's own `lg` breakpoint (1200px) — desktop keeps a real
 *   keyboard, where `d k` already works, so desktop chrome is left untouched.
 * - Tapping dispatches the same `d k` sequence we verified works. If that has
 *   no effect (e.g. a future release drops the binding), it falls back to
 *   rewriting the URL and reloading, so the button degrades rather than dies.
 */
(function () {
  'use strict';

  var LG_BREAKPOINT = 1200; // Grafana theme.breakpoints.values.lg
  var BTN_ID = 'cw-kiosk-toggle';

  function onDashboardRoute() {
    return /^\/d\//.test(location.pathname) || /^\/d-solo\//.test(location.pathname);
  }

  function inKiosk() {
    return new URLSearchParams(location.search).has('kiosk');
  }

  function shouldShow() {
    return onDashboardRoute() && window.innerWidth < LG_BREAKPOINT;
  }

  // Synthesize the 'd k' sequence. Mousetrap listens on document and reads
  // key/keyCode/which, so a well-formed synthetic event drives the same
  // handler a physical keyboard would.
  function pressKey(ch, code, keyCode) {
    ['keydown', 'keypress', 'keyup'].forEach(function (type) {
      document.dispatchEvent(new KeyboardEvent(type, {
        key: ch, code: code, keyCode: keyCode, which: keyCode,
        bubbles: true, cancelable: true
      }));
    });
  }

  function fallbackToggle() {
    var params = new URLSearchParams(location.search);
    if (params.has('kiosk')) {
      params.delete('kiosk');
    } else {
      params.set('kiosk', '');
    }
    var qs = params.toString().replace(/=(&|$)/g, '$1'); // keep bare `?kiosk`
    location.search = qs;
  }

  // NOTE: 'd k' only ENTERS kiosk on 13.1.3 — dispatching it again while in
  // kiosk mode is a no-op (verified at a 390px viewport). Grafana exits kiosk
  // via Esc (`bind("esc", exit)` -> exitKioskMode), so the two directions need
  // different keys. Getting this wrong is not fatal, just slow: the fallback
  // below still exits, but by reloading the page.
  function toggleKiosk() {
    var before = inKiosk();

    if (before) {
      pressKey('Escape', 'Escape', 27);
      window.setTimeout(function () {
        if (inKiosk() === before) { fallbackToggle(); }
      }, 600);
      return;
    }

    pressKey('d', 'KeyD', 68);
    window.setTimeout(function () {
      pressKey('k', 'KeyK', 75);
      // If the keybinding did not take effect, fall back to a URL toggle.
      window.setTimeout(function () {
        if (inKiosk() === before) {
          fallbackToggle();
        }
      }, 600);
    }, 80);
  }

  function icon(active) {
    // "monitor" glyph, mirroring the icon Grafana 11 used for this control.
    // In kiosk mode we show it filled to signal "tap to leave".
    return '<svg viewBox="0 0 24 24" width="22" height="22" aria-hidden="true" focusable="false">' +
      '<rect x="2.5" y="4" width="19" height="13" rx="2" fill="' +
        (active ? 'currentColor' : 'none') +
        '" stroke="currentColor" stroke-width="1.8"/>' +
      '<line x1="8" y1="20.5" x2="16" y2="20.5" stroke="currentColor" stroke-width="1.8" ' +
        'stroke-linecap="round"/></svg>';
  }

  function ensureButton() {
    var btn = document.getElementById(BTN_ID);

    if (!shouldShow()) {
      if (btn) { btn.style.display = 'none'; }
      return;
    }

    if (!btn) {
      btn = document.createElement('button');
      btn.id = BTN_ID;
      btn.type = 'button';
      btn.setAttribute(
        'style',
        'position:fixed;right:12px;bottom:12px;z-index:1100;' +
        'width:44px;height:44px;border-radius:50%;' +          // 44px = min tap target
        'display:flex;align-items:center;justify-content:center;' +
        'background:#073642;color:#93a1a1;border:1px solid #586e75;' +
        'box-shadow:0 2px 6px rgba(0,0,0,.4);opacity:.75;' +
        'cursor:pointer;padding:0;-webkit-tap-highlight-color:transparent;'
      );
      btn.addEventListener('click', function (e) {
        e.preventDefault();
        e.stopPropagation();
        toggleKiosk();
      });
      document.body.appendChild(btn);
    }

    btn.style.display = 'flex';
    var active = inKiosk();
    btn.setAttribute('aria-label', active ? 'Exit kiosk mode' : 'Enable kiosk mode');
    btn.setAttribute('title', active ? 'Exit kiosk mode' : 'Enable kiosk mode');
    if (btn.dataset.state !== String(active)) {
      btn.dataset.state = String(active);
      btn.innerHTML = icon(active);
    }
  }

  function start() {
    ensureButton();
    // The app is a SPA: route and kiosk state change without a page load, and
    // React owns the DOM, so poll rather than trying to hook the router.
    window.setInterval(ensureButton, 1000);
    window.addEventListener('resize', ensureButton);
    window.addEventListener('popstate', ensureButton);
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', start);
  } else {
    start();
  }
})();
