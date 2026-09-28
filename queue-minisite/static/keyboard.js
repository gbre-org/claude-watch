// Vim-style keyboard shortcuts for the queue minisite.
//
// j / k        — move selection down / up through queue items (.item rows).
// Enter        — open the live-log / archive modal for the selected item
//                (only fires for items that already have .log-clickable).
// Esc          — close any open modal; second Esc clears selection.
// m            — toggle MULTITAIL mode (whole-window stack of live tails for
//                every running item that has a log). Owned by multitail.js,
//                listed here so this file stays the map of what the page
//                binds; while that overlay is up it counts as an open modal
//                below, so j/k stop moving the row selection behind it.
// /            — focus a search/filter input if one exists (#search-input).
//                No-op otherwise — the queue minisite has no search box
//                today, but we leave the binding in place so adding one
//                later is zero-config.
//
// Selection state lives in a single `selectedQueueId` variable. We re-find
// the row by data-queue-id on every action so the 5s morphdom refresh
// can replace nodes without dropping the selection. Wrap-around: j on
// the last row goes to the first; k on the first goes to the last.
//
// Typing-into-input guard: shortcuts are skipped while the focused
// element is an INPUT or TEXTAREA, EXCEPT Esc — Esc always defocuses
// (so the user can quickly hop out of the search box).
//
// Mobile / touch: this file only attaches keydown listeners; no DOM
// mutations beyond toggling a CSS class on the selected row. Touch
// behavior is unaffected.

(function () {
  'use strict';

  // Single-source-of-truth selector for queue rows. The Jinja template
  // and refresh.js both render <article class="item">; we don't need a
  // dedicated `.queue-row` class — `.item` is the canonical row.
  const ROW_SELECTOR = '.item';
  // CSS class that paints the focus ring + tinted background. Defined
  // in style.css alongside the existing .item / .state-* rules.
  const SELECTED_CLASS = 'kbd-selected';
  const SEARCH_INPUT_ID = 'search-input';

  let selectedQueueId = null;

  function isTypingTarget(el) {
    if (!el) return false;
    const tag = el.tagName;
    if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return true;
    // contenteditable elements behave like inputs for typing purposes.
    if (el.isContentEditable) return true;
    return false;
  }

  function isAnyModalOpen() {
    // Any data-no-morph modal that's not hidden counts as "open" — we
    // pause our shortcuts so live-log.js's own keydown handler (Esc, g,
    // G) takes precedence inside an open modal.
    const modals = document.querySelectorAll('[data-no-morph]');
    for (const m of modals) {
      if (!m.hidden) return true;
    }
    return false;
  }

  function rows() {
    // Live NodeList → static Array so the order is stable across the
    // arrow-iteration loop even if DOM mutates mid-tick (it shouldn't,
    // but defensive — morphdom replaces nodes async on refresh).
    return Array.prototype.slice.call(document.querySelectorAll(ROW_SELECTOR));
  }

  function findSelectedIndex(allRows) {
    if (!selectedQueueId) return -1;
    for (let i = 0; i < allRows.length; i++) {
      const qid = allRows[i].getAttribute('data-queue-id');
      if (qid === selectedQueueId) return i;
    }
    return -1;
  }

  function clearSelectionStyles() {
    const prev = document.querySelectorAll('.' + SELECTED_CLASS);
    for (const el of prev) {
      el.classList.remove(SELECTED_CLASS);
    }
  }

  function applySelection(row) {
    clearSelectionStyles();
    if (!row) return;
    row.classList.add(SELECTED_CLASS);
    selectedQueueId = row.getAttribute('data-queue-id') || null;
    // Scroll into view if off-screen. `nearest` keeps in-view rows put;
    // off-screen rows get pulled into view without yanking the page.
    if (typeof row.scrollIntoView === 'function') {
      try {
        row.scrollIntoView({ block: 'nearest', inline: 'nearest', behavior: 'smooth' });
      } catch (_) {
        // Older browsers may not accept the options object.
        row.scrollIntoView();
      }
    }
  }

  function selectByIndex(idx) {
    const all = rows();
    if (all.length === 0) return;
    let i = idx;
    if (i < 0) i = all.length - 1;
    if (i >= all.length) i = 0;
    applySelection(all[i]);
  }

  function moveDown() {
    const all = rows();
    if (all.length === 0) return;
    const cur = findSelectedIndex(all);
    if (cur < 0) {
      // Nothing selected yet → start at the top.
      applySelection(all[0]);
      return;
    }
    selectByIndex(cur + 1);
  }

  function moveUp() {
    const all = rows();
    if (all.length === 0) return;
    const cur = findSelectedIndex(all);
    if (cur < 0) {
      // Nothing selected yet → start at the bottom.
      applySelection(all[all.length - 1]);
      return;
    }
    selectByIndex(cur - 1);
  }

  function openSelected() {
    if (!selectedQueueId) return;
    const all = rows();
    const idx = findSelectedIndex(all);
    if (idx < 0) return;
    const row = all[idx];
    // Only fire for rows that are actually log-clickable (running with
    // an agent / workload, or done/abandoned with an archive). For
    // non-clickable rows (e.g. starting items without a workload binding)
    // Enter is a no-op.
    if (!row.classList.contains('log-clickable')) return;
    // Synthesize a click — live-log.js's delegated click handler will
    // pick it up and open the modal. Using dispatchEvent ensures any
    // other listeners (e.g. analytics) also fire as if the user clicked.
    row.dispatchEvent(new MouseEvent('click', {
      bubbles: true,
      cancelable: true,
      view: window,
    }));
  }

  function clearSelection() {
    clearSelectionStyles();
    selectedQueueId = null;
  }

  function focusSearch() {
    const input = document.getElementById(SEARCH_INPUT_ID);
    if (!input) return;
    try {
      input.focus();
      // Cursor at end if the input already has content.
      if (typeof input.select === 'function') input.select();
    } catch (_) { /* defensive */ }
  }

  document.addEventListener('keydown', (ev) => {
    // Esc always works — even from inside an input — so the user can
    // hop out of the search box quickly. We only defocus + clear
    // selection here; if a modal is open, live-log.js / action.js
    // already handle Esc-to-close inside the modal.
    if (ev.key === 'Escape') {
      // If we're inside an input AND a modal isn't open, defocus first.
      if (isTypingTarget(document.activeElement) && !isAnyModalOpen()) {
        try { document.activeElement.blur(); } catch (_) {}
        ev.preventDefault();
        return;
      }
      // If a modal is open the modal owns Esc — don't double-handle.
      if (isAnyModalOpen()) return;
      // Otherwise: clear selection (second-Esc behavior; harmless
      // first-Esc when nothing is selected).
      if (selectedQueueId !== null) {
        clearSelection();
        ev.preventDefault();
      }
      return;
    }

    // All other shortcuts are skipped when typing or when a modal owns
    // the keyboard.
    if (isTypingTarget(document.activeElement)) return;
    if (isAnyModalOpen()) return;
    // Modifier keys (ctrl/alt/meta) suppress — leaves browser shortcuts
    // and OS-level chords untouched.
    if (ev.ctrlKey || ev.metaKey || ev.altKey) return;

    if (ev.key === 'j') {
      ev.preventDefault();
      moveDown();
    } else if (ev.key === 'k') {
      ev.preventDefault();
      moveUp();
    } else if (ev.key === 'Enter') {
      // Only intercept Enter when we have a selection — otherwise let
      // any focused button/link handle it normally.
      if (selectedQueueId) {
        ev.preventDefault();
        openSelected();
      }
    } else if (ev.key === '/') {
      // vim-style search focus. No-op if there's no search input.
      const input = document.getElementById(SEARCH_INPUT_ID);
      if (input) {
        ev.preventDefault();
        focusSearch();
      }
    }
  });

  // Re-apply the selection ring after a morphdom refresh — refresh.js
  // replaces nodes by data-queue-id, but the .kbd-selected class is
  // ours, not in the rendered template, so we have to repaint it.
  // MutationObserver is overkill for a 5s tick; a small interval that
  // checks whether the selected row still has the class is enough and
  // costs nothing when nothing has changed.
  setInterval(() => {
    if (!selectedQueueId) return;
    const all = rows();
    const idx = findSelectedIndex(all);
    if (idx < 0) {
      // Selected row vanished (item moved to done / abandoned and got
      // pruned out of the recent tail). Drop the selection so the next
      // j/k starts fresh.
      clearSelection();
      return;
    }
    const row = all[idx];
    if (!row.classList.contains(SELECTED_CLASS)) {
      // Class was stripped by a refresh — repaint without scrolling.
      clearSelectionStyles();
      row.classList.add(SELECTED_CLASS);
    }
  }, 1000);

  // Expose for tests / debugging.
  window.__queueKeyboard = {
    moveDown,
    moveUp,
    openSelected,
    clearSelection,
    focusSearch,
    get selectedQueueId() { return selectedQueueId; },
  };
})();
