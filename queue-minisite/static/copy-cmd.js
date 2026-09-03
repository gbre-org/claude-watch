// copy-cmd.js — one-click copy for the exit commands rendered on WEDGED and
// QUARANTINED cards.
//
// Those cards show the CLI commands that end the state rather than wiring
// one-click mutation buttons: each exit is an assertion about whether a
// process is still alive, and that judgement is precisely what the quarantine
// state exists to stop the system from making on an inference. Copying is the
// part worth automating; deciding is not.
//
// The handler is DELEGATED on document, because refresh.js rebuilds
// #queue-root from scratch every 5s — a listener bound to the buttons
// themselves would be dropped by the first morphdom merge (the same failure
// mode the source filter and density toggle handlers are written around).
'use strict';

(function () {
  // Feedback window for the "copied" label, ms.
  const FEEDBACK_MS = 1200;

  function flash(btn, text) {
    const original = btn.getAttribute('data-copy-label') || btn.textContent;
    btn.setAttribute('data-copy-label', original);
    btn.textContent = text;
    window.setTimeout(() => {
      btn.textContent = btn.getAttribute('data-copy-label') || 'copy';
    }, FEEDBACK_MS);
  }

  // Fallback for non-secure contexts / browsers without the async clipboard
  // API. Returns true on success. The command text is also always selectable
  // in the <code> element next to the button, so a total failure here still
  // leaves the operator able to select-and-copy by hand.
  function legacyCopy(text) {
    try {
      const ta = document.createElement('textarea');
      ta.value = text;
      ta.setAttribute('readonly', '');
      ta.style.position = 'absolute';
      ta.style.left = '-9999px';
      document.body.appendChild(ta);
      ta.select();
      const ok = document.execCommand && document.execCommand('copy');
      document.body.removeChild(ta);
      return !!ok;
    } catch (_) {
      return false;
    }
  }

  document.addEventListener('click', (ev) => {
    const btn = ev.target && ev.target.closest
      ? ev.target.closest('.copy-cmd-btn')
      : null;
    if (!btn) return;
    // The card itself may be click-to-open-log elsewhere in the UI; never let
    // a copy bubble into that.
    ev.preventDefault();
    ev.stopPropagation();
    const text = btn.getAttribute('data-copy') || '';
    if (!text) return;
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(
        () => flash(btn, 'copied'),
        () => flash(btn, legacyCopy(text) ? 'copied' : 'copy failed'),
      );
    } else {
      flash(btn, legacyCopy(text) ? 'copied' : 'copy failed');
    }
  });
})();
