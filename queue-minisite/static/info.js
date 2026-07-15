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
