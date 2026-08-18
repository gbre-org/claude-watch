// Stop / Abandon / Force-start button + confirmation modal for the
// queue minisite.
//
// One modal handles three actions, distinguished by ``data-action``:
//
//   stop         — running rows; POSTs to /api/queue/stop;    danger styling.
//   abandon      — pending rows; POSTs to /api/queue/abandon; cleanup styling.
//   force-start  — pending rows; POSTs to /api/queue/<id>/force-start;
//                  warn styling. Reason is REQUIRED (auditable override).
//
// Click handler on every .action-btn opens the modal pre-filled with
// the row's queue id + summary and the action-specific copy. Confirm
// POSTs to the matching endpoint; on 200 we reload the page so the
// transition is reflected. On failure we surface the error inline and
// keep the modal open.
//
// Accessibility:
//   - role=dialog + aria-modal=true on the wrapper
//   - focus trap inside the panel while open
//   - Esc closes; clicking the backdrop closes; Cancel closes
//   - focus restoration to the originating button on close

(function () {
  'use strict';

  const modal = document.getElementById('action-modal');
  if (!modal) return;

  const idEl = document.getElementById('action-modal-id');
  const summaryEl = document.getElementById('action-modal-summary');
  const reasonEl = document.getElementById('action-modal-reason');
  const reasonLabelEl = document.getElementById('action-modal-reason-label');
  const errorEl = document.getElementById('action-modal-error');
  const titleEl = document.getElementById('action-modal-title');
  const confirmBtn = document.getElementById('action-modal-confirm');
  const cancelBtn = document.getElementById('action-modal-cancel');
  // Force-start-only checkbox: re-scope the item to a fresh distinct scope
  // instead of autostopping the overlapping running peer (Andrew #4713).
  const newScopeEl = document.getElementById('action-modal-new-scope');
  const warnings = modal.querySelectorAll('[data-show-for]');

  // Per-action presentation. Endpoints differ; copy differs; confirm
  // button text + danger class differs (abandon is less alarming).
  const ACTIONS = {
    stop: {
      // path-style; id goes in body
      endpoint: '/api/queue/stop',
      idInPath: false,
      title: 'Stop running item?',
      confirm: 'Stop',
      busy: 'Stopping…',
      danger: true,
      reasonRequired: false,
      placeholder: 'why are you stopping it?',
    },
    abandon: {
      endpoint: '/api/queue/abandon',
      idInPath: false,
      title: 'Abandon pending item?',
      confirm: 'Abandon',
      busy: 'Abandoning…',
      danger: false,
      reasonRequired: false,
      placeholder: 'why are you abandoning it?',
    },
    'force-start': {
      // /api/queue/<id>/force-start — id goes in URL, only reason in body.
      endpoint: '/api/queue/{id}/force-start',
      idInPath: true,
      title: 'Force-start pending item?',
      confirm: 'Force start',
      busy: 'Force-starting…',
      // Warning (orange) — destructive in the sense that it bypasses
      // the spawn-gate's serialization, but not as severe as Stop
      // (which abandons a running agent's work).
      danger: false,
      warn: true,
      reasonRequired: true,
      placeholder: 'why are you overriding scope serialization?',
    },
  };

  let triggerEl = null;       // the button that opened the modal
  let inFlight = false;       // guard against double-submit
  let currentAction = 'stop';

  const FOCUSABLE_SELECTOR = [
    'a[href]',
    'button:not([disabled])',
    'input:not([disabled]):not([type="hidden"])',
    'textarea:not([disabled])',
    'select:not([disabled])',
    '[tabindex]:not([tabindex="-1"])',
  ].join(',');

  function focusableNodes() {
    return Array.from(modal.querySelectorAll(FOCUSABLE_SELECTOR))
      .filter((el) => !el.hasAttribute('hidden') && el.offsetParent !== null);
  }

  function setError(msg) {
    if (!msg) {
      errorEl.hidden = true;
      errorEl.textContent = '';
      return;
    }
    errorEl.textContent = msg;
    errorEl.hidden = false;
  }

  function setBusy(busy) {
    inFlight = busy;
    confirmBtn.disabled = busy;
    cancelBtn.disabled = busy;
    reasonEl.disabled = busy;
    const cfg = ACTIONS[currentAction] || ACTIONS.stop;
    confirmBtn.textContent = busy ? cfg.busy : cfg.confirm;
  }

  function applyAction(action) {
    const cfg = ACTIONS[action] || ACTIONS.stop;
    currentAction = action in ACTIONS ? action : 'stop';
    titleEl.textContent = cfg.title;
    confirmBtn.textContent = cfg.confirm;
    reasonEl.placeholder = cfg.placeholder;
    if (cfg.danger) {
      confirmBtn.classList.add('modal-btn-danger');
      confirmBtn.classList.remove('modal-btn-warn');
    } else {
      confirmBtn.classList.remove('modal-btn-danger');
      confirmBtn.classList.add('modal-btn-warn');
    }
    modal.dataset.action = currentAction;
    // Show only the warning paragraph(s) tagged for this action.
    warnings.forEach((el) => {
      const target = el.getAttribute('data-show-for');
      el.hidden = target !== currentAction;
    });
    // Visually mark the reason field as required when the action demands
    // one. CSS uses the .required class to render an asterisk + invalid
    // styling; the JS submit path enforces the same rule.
    if (cfg.reasonRequired) {
      reasonEl.classList.add('required');
      reasonEl.required = true;
      if (reasonLabelEl) reasonLabelEl.textContent = 'Reason';
    } else {
      reasonEl.classList.remove('required');
      reasonEl.required = false;
      if (reasonLabelEl) reasonLabelEl.textContent = 'Reason (optional)';
    }
  }

  function open(btn) {
    triggerEl = btn;
    const id = btn.getAttribute('data-id') || '';
    const summary = btn.getAttribute('data-summary') || '';
    const action = btn.getAttribute('data-action') || 'stop';
    applyAction(action);
    idEl.textContent = id;
    summaryEl.textContent = summary;
    reasonEl.value = '';
    if (newScopeEl) newScopeEl.checked = false;
    setError(null);
    setBusy(false);
    modal.hidden = false;
    document.body.classList.add('modal-open');
    // Focus the reason input first — most natural target for the user.
    setTimeout(() => {
      const target = reasonEl.disabled ? cancelBtn : reasonEl;
      target.focus();
    }, 0);
  }

  function close() {
    if (modal.hidden) return;
    modal.hidden = true;
    document.body.classList.remove('modal-open');
    setError(null);
    setBusy(false);
    if (triggerEl && typeof triggerEl.focus === 'function') {
      triggerEl.focus();
    }
    triggerEl = null;
  }

  async function submit() {
    if (inFlight) return;
    const id = idEl.textContent.trim();
    if (!id) {
      setError('No queue id selected.');
      return;
    }
    const cfg = ACTIONS[currentAction] || ACTIONS.stop;
    const reasonVal = (reasonEl.value || '').trim();
    if (cfg.reasonRequired && !reasonVal) {
      setError('Reason is required for this action.');
      reasonEl.focus();
      return;
    }
    // Endpoints that take id in the URL path (e.g. force-start) get
    // a fresh URL; endpoints that take id in the body keep the
    // legacy {id, reason} shape.
    const url = cfg.idInPath
      ? cfg.endpoint.replace('{id}', encodeURIComponent(id))
      : cfg.endpoint;
    const reqBody = cfg.idInPath
      ? { reason: reasonVal }
      : { id: id, reason: reasonVal };
    // Force-start with a new scope: signal the backend to re-scope this item
    // to a fresh distinct scope so it co-runs with the peer (no autostop).
    if (currentAction === 'force-start' && newScopeEl && newScopeEl.checked) {
      reqBody.new_scope = true;
    }
    setBusy(true);
    setError(null);
    try {
      const r = await fetch(url, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        cache: 'no-store',
        body: JSON.stringify(reqBody),
      });
      let body = null;
      try {
        body = await r.json();
      } catch (_) {
        body = null;
      }
      if (r.ok && body && body.ok) {
        // Success — reload so the running/pending -> abandoned
        // transition is visible immediately. The backend already busts
        // the read cache.
        location.reload();
        return;
      }
      const detail =
        (body && (body.error || body.stderr || body.stdout)) ||
        `HTTP ${r.status}`;
      setError(`${currentAction} failed: ${detail}`);
      setBusy(false);
    } catch (e) {
      setError(`network error: ${e && e.message ? e.message : e}`);
      setBusy(false);
    }
  }

  // Wire up the action buttons (event delegation handles re-rendering
  // on refresh.js' location.reload).
  document.addEventListener('click', (ev) => {
    const btn = ev.target.closest('.action-btn');
    if (!btn) return;
    ev.preventDefault();
    open(btn);
  });

  // Modal dismiss: backdrop click, Cancel button, anything tagged
  // [data-modal-dismiss].
  modal.addEventListener('click', (ev) => {
    if (inFlight) return;
    if (ev.target.closest('[data-modal-dismiss]')) {
      close();
    }
  });

  confirmBtn.addEventListener('click', (ev) => {
    ev.preventDefault();
    submit();
  });

  // Esc to dismiss; Enter inside reason input submits.
  document.addEventListener('keydown', (ev) => {
    if (modal.hidden) return;
    if (ev.key === 'Escape') {
      ev.preventDefault();
      if (!inFlight) close();
      return;
    }
    if (ev.key === 'Enter' && document.activeElement === reasonEl) {
      ev.preventDefault();
      submit();
      return;
    }
    // Focus trap.
    if (ev.key === 'Tab') {
      const nodes = focusableNodes();
      if (!nodes.length) return;
      const first = nodes[0];
      const last = nodes[nodes.length - 1];
      if (ev.shiftKey && document.activeElement === first) {
        ev.preventDefault();
        last.focus();
      } else if (!ev.shiftKey && document.activeElement === last) {
        ev.preventDefault();
        first.focus();
      }
    }
  });
})();
