// Info dropdown toggle in the topbar.
//
// Shows session info (auth user, cache age, /api/queue link) on demand.
// Replaces the old fixed bottombar so the visual real-estate stays focused
// on queue state. Toggled by the ⓘ button in the topbar.
//
// Accessibility:
//   - aria-expanded reflects state on the toggle
//   - role=menu on the panel; closes on Esc and on outside click
//   - focus returns to the toggle on close
(function () {
  'use strict';

  const toggle = document.getElementById('info-toggle');
  const dropdown = document.getElementById('info-dropdown');
  if (!toggle || !dropdown) return;

  function open() {
    dropdown.hidden = false;
    toggle.setAttribute('aria-expanded', 'true');
    document.addEventListener('click', onDocClick, true);
    document.addEventListener('keydown', onKeyDown, true);
  }

  function close() {
    dropdown.hidden = true;
    toggle.setAttribute('aria-expanded', 'false');
    document.removeEventListener('click', onDocClick, true);
    document.removeEventListener('keydown', onKeyDown, true);
  }

  function onDocClick(ev) {
    if (dropdown.contains(ev.target) || toggle.contains(ev.target)) return;
    close();
  }

  function onKeyDown(ev) {
    if (ev.key === 'Escape') {
      close();
      toggle.focus();
    }
  }

  toggle.addEventListener('click', (ev) => {
    ev.stopPropagation();
    if (dropdown.hidden) open();
    else close();
  });
})();

// Collapsible header (botchat #1762).
//
// A disclosure caret before the title folds the header controls (source
// filter, liveness dot, info popup) away, leaving only the title (left) and
// the count pills (right). Purely client-side; the collapsed state is
// persisted in localStorage (key `qsite_header_collapsed`) and restored
// flash-free by the <head> pre-paint guard in index.html (which adds the
// `header-collapsed` class to <html> before paint). Mirrors the pr-watch
// minisite's `header-toggle` pattern.
//
// The class lives on <html> (not #topbar-meta), so the 5s refresh.js SPA
// tick — which rebuilds #topbar-meta via morphdom — never disturbs the
// collapsed state.
(function () {
  'use strict';

  const KEY = 'qsite_header_collapsed';
  const root = document.documentElement;
  const btn = document.getElementById('header-toggle');
  if (!btn) return;

  function sync() {
    const collapsed = root.classList.contains('header-collapsed');
    btn.setAttribute('aria-expanded', collapsed ? 'false' : 'true');
    btn.setAttribute(
      'aria-label',
      collapsed ? 'Expand header controls' : 'Collapse header controls'
    );
  }

  sync(); // reflect the initial state applied by the <head> restore

  btn.addEventListener('click', () => {
    const collapsed = root.classList.toggle('header-collapsed');
    try {
      localStorage.setItem(KEY, collapsed ? '1' : '0');
    } catch (e) {
      /* localStorage unavailable — collapse still works for this session */
    }
    sync();
  });
})();

// ---------------------------------------------------------------------
// DENSITY TOGGLE (botchat #1944)
// ---------------------------------------------------------------------
// A "density" pill in the topbar flips the whole view between the comfortable
// default and a vertically-DENSE (compact) layout, condensing each queue row
// so q-site aligns row-by-row with the pr-watch minisite in a side-by-side
// split. Purely client-side + CSS-driven: the `density-compact` class lives on
// <html> (NOT on #topbar-meta or #queue-root), so the 5s refresh.js morphdom
// merge never disturbs it — the same durability trick as header-collapsed.
//
// The <head> pre-paint guard in index.html already restored the class from
// localStorage flash-free; here we (a) sync the button's label / aria-pressed
// to the restored state, and (b) bind a DELEGATED click handler (the button is
// rebuilt every tick by refresh.js buildTopbarMetaDOM, so we bind on document,
// mirroring the source-filter's delegated change handler). Mirrors the
// pr-watch minisite's density toggle affordance + localStorage persistence.
(function () {
  'use strict';

  const KEY = 'qsite_density';
  const root = document.documentElement;

  // Reflect the current html.density-compact state onto the (rebuildable)
  // toggle button. Called on load AND after each refresh tick so the button
  // never drifts from the class.
  function applyDensityButton() {
    const btn = document.getElementById('density-toggle');
    if (!btn) return;
    const compact = root.classList.contains('density-compact');
    btn.setAttribute('aria-pressed', compact ? 'true' : 'false');
    btn.setAttribute('data-density', compact ? 'compact' : 'comfortable');
    btn.textContent = compact ? 'compact' : 'comfortable';
  }

  applyDensityButton(); // reflect the initial state applied by the <head> restore

  // Delegated click — the button is replaced every 5s tick, so bind on
  // document rather than the (replaceable) element itself.
  document.addEventListener('click', (ev) => {
    const t = ev.target;
    if (!t || t.id !== 'density-toggle') return;
    const compact = root.classList.toggle('density-compact');
    try {
      localStorage.setItem(KEY, compact ? 'compact' : 'comfortable');
    } catch (e) {
      /* localStorage unavailable — toggle still works for this session */
    }
    applyDensityButton();
  });

  // Expose the sync helper so refresh.js (or a test) can re-apply the button
  // state after a merge. refresh.js buildTopbarMetaDOM already builds the
  // button in the correct state, but calling this after a tick is belt +
  // braces against any race.
  window.__qsiteDensity = { applyDensityButton };
})();
