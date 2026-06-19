// Live refresh tick for the queue minisite.
//
// Strategy: every REFRESH_MS we GET /api/queue, build a fresh DOM tree
// from the JSON in memory, then morphdom-merge it into the live page.
// Morphdom only touches changed nodes — open modals, in-progress drags,
// <details> expansion, scroll position, and focus all survive a tick.
//
// The Jinja template renders the canonical first paint server-side. This
// script's buildXxx() functions mirror that markup exactly so the merge
// is a no-op on unchanged items. Anything that affects the visible UI
// (data-* attrs, classes, button text, badges) MUST be reflected in
// both places — see templates/index.html § sections RUNNING/PENDING/
// DONE/ABANDONED for the source of truth.
//
// Escape hatches:
//   - data-no-morph attribute on an element causes morphdom to skip it
//     entirely (used on the action-modal and log-modal so they survive
//     refreshes; both live OUTSIDE #queue-root, so this is belt + braces).
//   - Elements with an active drag (.dragging) or active drop-target
//     highlight (.drop-target) are skipped during the merge so a
//     drag-in-progress isn't rug-pulled by a refresh that lands mid-
//     gesture.
//   - <details> open state is explicitly preserved: if the live element
//     is open, we set the new element open before morphdom diffs them.
//   - Scroll position is window-level on this page (the .queue-list
//     containers grow with content rather than scrolling internally), so
//     no per-section scroll capture is needed — morphdom touches the
//     subtree without re-laying out the document.
//
// On totals/error change we still run the same merge — no full
// `location.reload()` anymore. The previous behavior (reload on count
// change) was the disruption Andrew flagged: any open modal would close,
// scroll position would jump, drag would abort. This rewrite eliminates
// all of that.

(function () {
  'use strict';
  const REFRESH_MS = 5000;

  // Lookup once. The element is required — if it's absent, refresh is a
  // no-op (avoids throwing on partial DOMs / template rebuilds).
  const queueRoot = document.getElementById('queue-root');
  const topbarMeta = document.getElementById('topbar-meta');
  if (!queueRoot) return;

  // ---------------------------------------------------------------------
  // HTML escape — we build markup as strings (template-literal style)
  // because it's easier to keep in lockstep with the Jinja template than
  // a verbose createElement() ladder. All user-supplied content goes
  // through esc(); attribute values that may contain quotes go through
  // attr().
  // ---------------------------------------------------------------------
  function esc(s) {
    if (s === null || s === undefined) return '';
    return String(s)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;');
  }
  function attr(s) {
    if (s === null || s === undefined) return '';
    return String(s)
      .replace(/&/g, '&amp;')
      .replace(/"/g, '&quot;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;');
  }

  // ---------------------------------------------------------------------
  // Subagent-tree collapse persistence.
  //
  // The nested subagent tree (one .subagent-tree <details> per running card,
  // keyed by `data-tree-key` = the queue id) renders DEFAULT-EXPANDED on
  // first load. The operator can collapse/expand any card's tree; we persist
  // that per-card choice in localStorage under `subtree:<queueId>` ("1"=open,
  // "0"=closed). The DEFAULT (no stored value, e.g. first ever load) is OPEN.
  //
  // Why this can't ride the generic <details> preserve-open path in
  // onBeforeElUpdated: that path only re-applies `open` when the LIVE element
  // is open. Because our server + renderer both emit `open` by default, a
  // user-COLLAPSED tree would be slammed back open on the next 5s tick. So
  // we read localStorage explicitly for .subagent-tree nodes during the merge
  // (applySubtreeStateToEl) and re-apply it after the first server paint
  // (applySubtreeState). Everything is dependency-free + fails safe to OPEN.
  // ---------------------------------------------------------------------
  const SUBTREE_KEY_PREFIX = 'subtree:';

  function readSubtreeStored(key) {
    if (!key) return null;
    try {
      return window.localStorage.getItem(SUBTREE_KEY_PREFIX + key);
    } catch (_) {
      return null; // private mode / disabled storage -> default open
    }
  }

  function writeSubtreeStored(key, isOpen) {
    if (!key) return;
    try {
      window.localStorage.setItem(SUBTREE_KEY_PREFIX + key, isOpen ? '1' : '0');
    } catch (_) { /* storage unavailable -> just don't persist */ }
  }

  // Apply the stored collapse state to a single .subagent-tree element.
  // Default (no stored value) = OPEN (expanded). Returns nothing.
  function applySubtreeStateToEl(el) {
    if (!el || !el.getAttribute) return;
    const key = el.getAttribute('data-tree-key');
    if (!key) return;
    const stored = readSubtreeStored(key);
    if (stored === '0') {
      el.removeAttribute('open');
    } else {
      // stored === '1' OR null (first load) -> expanded
      el.setAttribute('open', '');
    }
  }

  // Re-apply stored state to every subagent tree currently in the DOM. Run
  // once after the server's first paint and again after each merge tick.
  function applySubtreeState() {
    const trees = document.querySelectorAll('.subagent-tree[data-tree-key]');
    for (let i = 0; i < trees.length; i += 1) {
      applySubtreeStateToEl(trees[i]);
    }
  }

  // Persist on toggle. One delegated capturing listener catches the native
  // <details> `toggle` event from any .subagent-tree and records its new
  // open/closed state keyed by queue id.
  document.addEventListener('toggle', (ev) => {
    const t = ev.target;
    if (!t || !t.classList || !t.classList.contains('subagent-tree')) return;
    const key = t.getAttribute && t.getAttribute('data-tree-key');
    if (!key) return;
    writeSubtreeStored(key, !!t.open);
  }, true);

  // ---------------------------------------------------------------------
  // Source filter — filters the visible queue cards by their producer
  // (the queue item's `created_by`: `main-loop`, `workload`, …). The
  // dropdown is populated from `state.sources`, the GLOBAL distinct
  // `created_by` set computed server-side over EVERY queue item — NOT a
  // per-section facet. Before this fix there was no source dropdown at
  // all (it "showed nothing"); the fix adds one fed by real queue data.
  //
  // Filtering is purely client-side (show/hide cards via the `filtered-out`
  // class) so it composes cleanly with the morphdom refresh: each tick
  // rebuilds the cards + the dropdown options, then re-applies the active
  // selection. The selected value survives the merge via onBeforeElUpdated.
  // ---------------------------------------------------------------------
  const SOURCE_FILTER_ID = 'source-filter';

  function buildSourceFilterHTML(sources) {
    const opts = ['<option value="">all sources</option>']
      .concat((sources || []).map(
        (s) => `<option value="${attr(s)}">${esc(s)}</option>`
      ))
      .join('');
    return (
      `<label class="source-filter-label" for="${SOURCE_FILTER_ID}">` +
      `<span class="sr-only">Filter by source</span>` +
      `<select id="${SOURCE_FILTER_ID}" class="source-filter" ` +
        `aria-label="Filter queue items by source (created_by)" ` +
        `title="Filter items by source (who enqueued them)">${opts}</select>` +
      `</label>`
    );
  }

  function getSourceFilterValue() {
    const sel = document.getElementById(SOURCE_FILTER_ID);
    return sel ? (sel.value || '') : '';
  }

  function applySourceFilter() {
    const want = getSourceFilterValue();
    const cards = document.querySelectorAll('#queue-root .item');
    cards.forEach((card) => {
      if (!want) {
        card.classList.remove('filtered-out');
        return;
      }
      const by = card.getAttribute('data-created-by') || '';
      card.classList.toggle('filtered-out', by !== want);
    });
  }

  // Delegated change handler — the <select> is rebuilt every tick, so we
  // bind on document rather than the (replaceable) element itself.
  document.addEventListener('change', (e) => {
    if (e.target && e.target.id === SOURCE_FILTER_ID) {
      applySourceFilter();
    }
  });

  // ---------------------------------------------------------------------
  // Section renderers — mirror templates/index.html block-for-block.
  // Returning HTML strings keeps the diff with the template easy to
  // eyeball. The outer section element gets a stable id (#section-X) so
  // morphdom keys the merge correctly.
  // ---------------------------------------------------------------------

  // Recursive subagent-node renderer. MUST mirror the subagent_node()
  // Jinja macro in templates/index.html so morphdom doesn't flap between
  // the server paint and the SPA refresh. Node shape (from app.py
  // _build_subagent_tree): { subagent_id, label, age, age_seconds,
  // queue_id, kind: "attempt"|"child", attempt: <int?>, children: [...] }.
  // `kind=="attempt"` nodes are prior dispatch attempts of the same queue
  // item (owner dropped server-side) — labeled "attempt N".
  function renderSubagentNode(sa) {
    const sid = sa.subagent_id || '';
    const isAttempt = sa.kind === 'attempt';
    const cls = 'subagent-node subagent-log-clickable' + (isAttempt ? ' subagent-attempt' : '');
    let html =
      `<li class="${cls}" data-subagent-id="${attr(sid)}" data-log-mode="subagent" tabindex="0" role="button" aria-label="View live log for subagent ${attr(sid)}" title="Click to tail this subagent's live log">`;
    if (isAttempt) {
      html += `<span class="subagent-attempt-badge" title="prior dispatch attempt of this queue item">attempt ${esc(sa.attempt)}</span>`;
    }
    html +=
      `<code class="subagent-id">${esc(sid.slice(0, 12))}</code>` +
      `<span class="subagent-label">${esc(sa.label || sid)}</span>` +
      `<span class="subagent-age">${esc(sa.age || '')}</span>`;
    const children = sa.children || [];
    if (children.length) {
      html += '<ul class="subagent-list subagent-children">' +
        children.map((c) => renderSubagentNode(c)).join('') +
        '</ul>';
    }
    html += '</li>';
    return html;
  }

  function renderRunningItem(it) {
    const owner = it.owner || {};
    const isStarting = !!it.is_starting;
    const workloadLabel = it.workload_label || '';
    const isWorkload = workloadLabel.length > 0;
    const hostjobLabel = it.hostjob_label || '';
    const isHostjob = hostjobLabel.length > 0;
    // Workload- and hostjob-bound items render as RUNNING (not starting)
    // regardless of active-agents.json — they tail a plain-text output file
    // (workload: /tmp/claude-workloads/<label>.output; hostjob:
    // <HOSTJOB_LOG_DIR>/<label>/log), not a subagent JSONL. Orphan badging is
    // also suppressed for both since active-agents tracks subagents, not
    // workload / hostjob procs.
    const ownerAlive = owner.alive;
    const orphan = ownerAlive === false && !isStarting && !isWorkload && !isHostjob;
    const stateCls = (isStarting && !isWorkload && !isHostjob) ? 'state-starting' : 'state-running';
    // PR #131: starting rows are clickable too — clicking opens the
    // log modal in a polling state, retrying SSE until the agent's
    // first event lands. Must match the server-side template
    // (templates/index.html) so the SPA refresh tick doesn't overwrite
    // the server's clickable markup with a non-clickable replacement
    // every REFRESH_MS.
    const isClickable = true;
    const cardClasses = [
      'item',
      stateCls,
      'drop-zone',
      'log-clickable',
      orphan ? 'orphan' : '',
    ].filter(Boolean).join(' ');
    const startingFlag = (isStarting && !isWorkload && !isHostjob) ? '1' : '0';
    const logMode = isHostjob ? 'hostjob' : (isWorkload ? 'workload' : 'live');
    // aria-label / title vary by state so screen readers + tooltips
    // describe the polling behaviour for starting rows. Mirrors the
    // template's logic.
    let logKindLabel;
    if (isHostjob) logKindLabel = 'hostjob';
    else if (isWorkload) logKindLabel = 'workload';
    else if (isStarting) logKindLabel = 'live (polling — waiting for first event)';
    else logKindLabel = 'live';
    let logViewNoun;
    if (isHostjob) logViewNoun = 'hostjob output';
    else if (isWorkload) logViewNoun = 'workload output';
    else logViewNoun = 'live log';
    const titleText = (isStarting && !isWorkload && !isHostjob)
      ? 'Click to open log viewer — polls for the agent\'s first event, then tails live. Drop a pending item here to set this as its dependency.'
      : `Click to view ${logViewNoun}. Drop a pending item here to set this as its dependency.`;
    const logModeAttr = `data-log-mode="${logMode}" tabindex="0" role="button" aria-label="View ${logKindLabel} log for ${attr(it.id)}" title="${attr(titleText)}"`;
    const workloadAttr = isWorkload ? ` data-workload-label="${attr(workloadLabel)}"` : '';
    const hostjobAttr = isHostjob ? ` data-hostjob-label="${attr(hostjobLabel)}"` : '';

    let head = '';
    if (isStarting && !isWorkload && !isHostjob) {
      head += '<span class="badge state-starting" title="registered, waiting for agent to emit first event">starting</span>';
    } else {
      head += '<span class="badge state-running">running</span>';
    }
    if (isWorkload) {
      head += `<span class="badge workload-badge" title="workload-bound: tails /tmp/claude-workloads/${attr(workloadLabel)}.output">workload</span>`;
    }
    if (isHostjob) {
      head += `<span class="badge hostjob-badge" title="hostjob-bound: tails the host job log for ${attr(hostjobLabel)}">hostjob</span>`;
    }
    if (orphan) head += '<span class="badge state-orphan">orphan</span>';
    if (it.group_head) head += '<span class="badge ghead" title="head of serialization group">head</span>';
    head += `<span class="id">${esc(it.id)}</span>`;
    head += `<span class="prio" title="priority">p${esc(it.priority)}</span>`;
    head += `<button type="button" class="action-btn stop-btn" data-action="stop" data-id="${attr(it.id)}" data-summary="${attr(it.summary)}" title="Stop this running item">stop</button>`;

    const startedIso = it.started_at_iso || '';
    const ageLabel = (isStarting && !isWorkload && !isHostjob) ? 'registered' : 'running';
    let ageBlock = '';
    ageBlock += `<span ${startedIso ? `data-local-time-iso="${attr(startedIso)}" data-local-time-title-only` : ''} title="${attr(startedIso)}">${esc(ageLabel)} ${esc(it.age)}</span>`;
    ageBlock += '<span class="sep">·</span>';
    if (isHostjob) {
      ageBlock += `<span title="hostjob output tail for ${attr(hostjobLabel)}">hostjob ${esc(hostjobLabel)}</span>`;
    } else if (isWorkload) {
      ageBlock += `<span title="workload output tail: /tmp/claude-workloads/${attr(workloadLabel)}.output">workload ${esc(workloadLabel)}</span>`;
    } else if (isStarting) {
      ageBlock += '<span class="agent-spawning" title="queue item registered, waiting for agent\'s first JSONL event"><span class="spinner" aria-hidden="true"></span>agent spawning…</span>';
    } else if (owner.mode === 'agent') {
      const aid = owner.agent_id || '';
      const aliveTxt = owner.alive ? 'alive' : 'STALE';
      ageBlock += `<span title="claude-watch agent transcript mtime (${attr(aid)})">agent ${esc(aid.slice(0, 12))} ${esc(aliveTxt)} (${esc(owner.jsonl_age || '?')})</span>`;
    } else {
      ageBlock += '<span title="no matching agent record in claude-watch state">owner unknown</span>';
    }
    if (it.created_by) ageBlock += `<span class="sep">·</span><span>by ${esc(it.created_by)}</span>`;

    let scope = '';
    if (it.scope && it.scope.length) {
      scope = '<div class="scope">' +
        it.scope.map((s) => `<span class="chip">${esc(s)}</span>`).join('') +
        '</div>';
    }

    let prompt = '';
    if (it.description) {
      prompt = '<details class="prompt-toggle">' +
        `<summary class="prompt-summary">Prompt (${esc(it.description.length)} chars)</summary>` +
        `<pre class="prompt-body">${esc(it.description)}</pre>` +
        '</details>';
    }

    // Nested subagent tree -- mirrors the RUNNING block in
    // templates/index.html. it.subagents is computed server-side by
    // app.py _build_subagent_tree for running items: subagents are
    // attributed to their owning queue item via the AUTHORITATIVE
    // agent_id->queue_id bindings (post-tool-agent-arm-hook), the owner
    // agent is DROPPED (it IS the item, not a child), and remaining
    // same-item agents are collapsed as "attempt N" nodes. Each node is
    // rendered by the recursive renderSubagentNode() below — MUST mirror
    // the subagent_node() Jinja macro in templates/index.html so morphdom
    // doesn't flap between the server paint and this 5s SPA re-render.
    const subagents = it.subagents || [];
    let subtree = '';
    if (subagents.length) {
      const nodes = subagents.map((sa) => renderSubagentNode(sa)).join('');
      // DEFAULT-EXPANDED (`open`) + collapsible — mirrors templates/index.html
      // so morphdom doesn't flap. `data-tree-key` (the queue id) is the
      // localStorage persistence key (see applySubtreeState + the toggle
      // listener below).
      subtree = `<details class="prompt-toggle subagent-tree" open data-tree-key="${attr(it.id)}">` +
        `<summary class="prompt-summary">Subagents (${esc(subagents.length)})</summary>` +
        `<ul class="subagent-list">${nodes}</ul>` +
        '</details>';
    }

    return (
      `<article class="${cardClasses}" data-queue-id="${attr(it.id)}" data-queue-status="running" data-created-by="${attr(it.created_by || '')}" data-queue-starting="${startingFlag}" data-queue-summary="${attr(it.summary)}" data-queue-description="${attr(it.description)}" data-agent-id="${attr(owner.agent_id || '')}"${workloadAttr}${hostjobAttr} ${logModeAttr}>` +
      `<header class="item-head">${head}</header>` +
      `<p class="summary">${esc(it.summary)}</p>` +
      `<div class="age">${ageBlock}</div>` +
      scope +
      prompt +
      subtree +
      '</article>'
    );
  }

  function renderBlockedItem(it) {
    // Markup MUST mirror the BLOCKED block in templates/index.html.
    //
    // BLOCKED items are status=blocked — the owning agent moved itself
    // there via `session-task queue block <id> --reason ...`. The reason
    // is operator-set free text; it is NOT a depends_on edge. Do NOT cull
    // these by referenced-blocker state (e.g. "blocker q-XYZ is done so
    // hide this") — the operator set the block and only the operator
    // (via `session-task queue unblock`) clears it. The server-side
    // shaping in app.py already emits exactly the items with
    // ``status == "blocked"``; this renderer just mirrors the markup.
    //
    // Prior bug (q-2026-05-20-db66): this renderer + section did not
    // exist, so the SPA's first refresh tick built a queue-root WITHOUT
    // #section-blocked and morphdom discarded the server-rendered
    // blocked section. Visible symptom: "BLOCKED 1" on first paint,
    // then disappears ~5s later.
    let head = '';
    head += '<span class="badge state-blocked" title="parked on an external blocker; no live agent expected">blocked</span>';
    if (it.group_head) head += '<span class="badge ghead" title="head of serialization group">head</span>';
    head += `<span class="id">${esc(it.id)}</span>`;
    head += `<span class="prio" title="priority">p${esc(it.priority)}</span>`;

    const blockedIso = it.blocked_at_iso || '';
    let ageBlock = `<span ${blockedIso ? `data-local-time-iso="${attr(blockedIso)}" data-local-time-title-only` : ''} title="${attr(blockedIso)}">blocked ${esc(it.age)}</span>`;
    if (it.created_by) ageBlock += `<span class="sep">·</span><span>by ${esc(it.created_by)}</span>`;

    let scope = '';
    if (it.scope && it.scope.length) {
      scope = '<div class="scope">' +
        it.scope.map((s) => `<span class="chip">${esc(s)}</span>`).join('') +
        '</div>';
    }

    let reasonHtml = '';
    if (it.block_reason) {
      reasonHtml = `<p class="description"><strong>blocker:</strong> ${esc(it.block_reason)}</p>`;
    }

    let prompt = '';
    if (it.description) {
      prompt = '<details class="prompt-toggle">' +
        `<summary class="prompt-summary">Prompt (${esc(it.description.length)} chars)</summary>` +
        `<pre class="prompt-body">${esc(it.description)}</pre>` +
        '</details>';
    }

    return (
      `<article id="queue-${attr(it.id)}" class="item state-blocked" data-queue-id="${attr(it.id)}" data-queue-status="blocked" data-created-by="${attr(it.created_by || '')}" data-queue-summary="${attr(it.summary)}" data-queue-description="${attr(it.description)}">` +
      `<header class="item-head">${head}</header>` +
      `<p class="summary">${esc(it.summary)}</p>` +
      reasonHtml +
      `<div class="age">${ageBlock}</div>` +
      scope +
      prompt +
      '</article>'
    );
  }

  function renderPendingItem(it) {
    // Markup MUST mirror the pending block in templates/index.html. Any
    // drift will cause morphdom to flap on every tick (server's first
    // paint differs from JS-rendered re-render → diff → repaint), and
    // can also drop affordances mid-session (Bug q-55b6: previous
    // version of this function omitted the force-start button + the
    // dep-link anchor / dep-remove-btn structure; first refresh after
    // server-rendered initial paint silently stripped them).
    //
    // ``ready`` class + ``ready`` badge are gated on the BACKEND's
    // ``ready_now`` field (group-head AND every depends_on resolved to
    // ``done``). The older ``group_head`` flag was FIFO-only and ignored
    // depends_on (Bug q-1b89).
    const cardClasses = [
      'item',
      'state-pending',
      'drop-zone',
      'draggable',
      it.ready_now ? 'ready' : '',
      (it.depends_on && it.depends_on.length) ? 'has-deps' : '',
    ].filter(Boolean).join(' ');

    let head = '';
    head += '<span class="badge state-pending">pending</span>';
    if (it.ready_now) head += '<span class="badge ghead" title="ready to spawn (group head, all deps done)">ready</span>';
    head += `<span class="id">${esc(it.id)}</span>`;
    head += `<span class="prio" title="priority">p${esc(it.priority)}</span>`;
    if (it.depends_on && it.depends_on.length) {
      for (const dep of it.depends_on) {
        // Each dep is a clickable anchor (jumps to the target row)
        // with an "x" affordance to remove the edge via DELETE — must
        // match the Jinja template's structure so the page-level click
        // handlers (.dep-link, .dep-remove-btn) keep working after a
        // morphdom merge.
        head += `<span class="badge dep-badge" title="depends on ${attr(dep)} — click to scroll, x to remove">` +
          `<a class="dep-link" href="#queue-${attr(dep)}" data-dep-target="${attr(dep)}">&rarr; ${esc(dep)}</a>` +
          `<button type="button" class="dep-remove-btn" data-dep-source="${attr(it.id)}" data-dep-target="${attr(dep)}" title="Remove this dependency" aria-label="Remove dependency on ${attr(dep)}">&times;</button>` +
          `</span>`;
      }
    }
    head += '<span class="drag-handle" aria-hidden="true" title="drag to set dependency">&#x2630;</span>';
    head += `<button type="button" class="action-btn force-start-btn" data-action="force-start" data-id="${attr(it.id)}" data-summary="${attr(it.summary)}" title="Override scope-conflict serialization and promote to running">force start</button>`;
    head += `<button type="button" class="action-btn abandon-btn" data-action="abandon" data-id="${attr(it.id)}" data-summary="${attr(it.summary)}" title="Remove this pending item from the queue">abandon</button>`;

    const createdIso = it.created_at_iso || '';
    let ageBlock = `<span ${createdIso ? `data-local-time-iso="${attr(createdIso)}" data-local-time-title-only` : ''} title="${attr(createdIso)}">created ${esc(it.age)}</span>`;
    if (it.created_by) ageBlock += `<span class="sep">·</span><span>by ${esc(it.created_by)}</span>`;

    let scope = '';
    if (it.scope && it.scope.length) {
      scope = '<div class="scope">' +
        it.scope.map((s) => `<span class="chip">${esc(s)}</span>`).join('') +
        '</div>';
    }

    let prompt = '';
    if (it.description) {
      prompt = '<details class="prompt-toggle">' +
        `<summary class="prompt-summary">Prompt (${esc(it.description.length)} chars)</summary>` +
        `<pre class="prompt-body">${esc(it.description)}</pre>` +
        '</details>';
    }

    return (
      `<article id="queue-${attr(it.id)}" class="${cardClasses}" draggable="true" data-queue-id="${attr(it.id)}" data-queue-status="pending" data-created-by="${attr(it.created_by || '')}" data-queue-summary="${attr(it.summary)}" title="Drag onto another item to set as dependency">` +
      `<header class="item-head">${head}</header>` +
      `<p class="summary">${esc(it.summary)}</p>` +
      `<div class="age">${ageBlock}</div>` +
      scope +
      prompt +
      '</article>'
    );
  }

  function renderTerminalItem(it, status) {
    // Shared renderer for done + abandoned. They differ only in:
    //   - badge text + class
    //   - age anchor (completed_at_iso / abandoned_at_iso)
    //   - aria-label phrasing (archived log link)
    //   - abandoned has an optional reason paragraph
    const isDone = status === 'done';
    const stateCls = isDone ? 'state-done' : 'state-abandoned';
    const badgeTxt = isDone ? 'done' : 'abandoned';
    // A terminal hostjob keeps its on-disk log (<HOSTJOB_LOG_DIR>/<label>/log)
    // after completion, so the row stays clickable in hostjob mode even though
    // it has no archived agent transcript. hostjob mode takes precedence over
    // archive so a hostjob item opens its plain-text job log (its real
    // artifact). Mirrors the templates/index.html Done/Abandoned sections.
    const isHostjob = (it.hostjob_label || '').length > 0;
    const isClickable = it.has_archive || isHostjob;
    const cardClasses = [
      'item',
      stateCls,
      isClickable ? 'log-clickable' : '',
    ].filter(Boolean).join(' ');
    const archiveAttr = isHostjob
      ? `data-hostjob-label="${attr(it.hostjob_label)}" data-log-mode="hostjob" tabindex="0" role="button" aria-label="View hostjob output for ${attr(it.id)}" title="Click to view hostjob output."`
      : it.has_archive
      ? `data-log-mode="archive" tabindex="0" role="button" aria-label="View archived log for ${attr(it.id)}" title="Click to view archived log."`
      : '';

    let head = `<span class="badge ${stateCls}">${esc(badgeTxt)}</span>` +
      `<span class="id">${esc(it.id)}</span>`;
    if (it.has_archive) {
      head += '<span class="badge log-badge" title="archived agent transcript available">log</span>';
    }

    const anchorIso = isDone ? it.completed_at_iso : it.abandoned_at_iso;
    const anchorLabel = isDone ? 'completed' : 'abandoned';
    let ageBlock = `<span ${anchorIso ? `data-local-time-iso="${attr(anchorIso)}" data-local-time-title-only` : ''} title="${attr(anchorIso || '')}">${esc(anchorLabel)} ${esc(it.age)}</span>`;
    if (it.created_by) ageBlock += `<span class="sep">·</span><span>by ${esc(it.created_by)}</span>`;

    let reasonHtml = '';
    if (!isDone && it.abandon_reason) {
      reasonHtml = `<p class="description">${esc(it.abandon_reason)}</p>`;
    }

    let prompt = '';
    if (it.description) {
      prompt = '<details class="prompt-toggle">' +
        `<summary class="prompt-summary">Prompt (${esc(it.description.length)} chars)</summary>` +
        `<pre class="prompt-body">${esc(it.description)}</pre>` +
        '</details>';
    }

    return (
      `<article class="${cardClasses}" data-queue-id="${attr(it.id)}" data-queue-status="${attr(status)}" data-created-by="${attr(it.created_by || '')}" data-queue-summary="${attr(it.summary)}" data-queue-description="${attr(it.description)}" ${archiveAttr}>` +
      `<header class="item-head">${head}</header>` +
      `<p class="summary">${esc(it.summary)}</p>` +
      reasonHtml +
      `<div class="age">${ageBlock}</div>` +
      prompt +
      '</article>'
    );
  }

  function renderRunningSection(state) {
    const totals = state.totals || {};
    const items = state.running || [];
    let body = '';
    if (!items.length) {
      body = '<div class="empty-mini">No running items.</div>';
    }
    body += items.map(renderRunningItem).join('');
    return (
      `<section id="section-running">` +
      `<h2 class="section-title">Running <span class="section-count">${esc(totals.running ?? items.length)}</span></h2>` +
      body +
      '</section>'
    );
  }

  function renderBlockedSection(state) {
    // The Jinja template wraps the entire BLOCKED section in
    // `{% if blocked %}` — i.e. when the queue has zero blocked items,
    // there is NO #section-blocked element at all. Mirror that here:
    // if the SPA renders an empty #section-blocked while the server
    // omits it, morphdom would diff the two trees and re-create the
    // wrapper on every tick (and worse, never let the server's
    // omit-when-empty state stick after a refresh). Return an empty
    // string when there's nothing to render — morphdom will then
    // simply delete the wrapper if it was present before, which is
    // what we want.
    const totals = state.totals || {};
    const items = state.blocked || [];
    if (!items.length) return '';
    return (
      `<section id="section-blocked">` +
      `<h2 class="section-title">Blocked <span class="section-count">${esc(totals.blocked ?? items.length)}</span></h2>` +
      items.map(renderBlockedItem).join('') +
      '</section>'
    );
  }

  function renderPendingSection(state) {
    const totals = state.totals || {};
    const items = state.pending || [];
    let body = '';
    if (!items.length) {
      body = '<div class="empty-mini">No pending items.</div>';
    }
    body += items.map(renderPendingItem).join('');
    return (
      `<section id="section-pending">` +
      `<h2 class="section-title">Pending <span class="section-count">${esc(totals.pending ?? items.length)}</span></h2>` +
      body +
      '</section>'
    );
  }

  function renderDoneSection(state) {
    const totals = state.totals || {};
    const items = state.done_recent || [];
    let body = '';
    if (!items.length) {
      body = '<div class="empty-mini">No completed items.</div>';
    }
    body += items.map((it) => renderTerminalItem(it, 'done')).join('');
    return (
      `<section id="section-done">` +
      `<h2 class="section-title">Done <span class="section-count">${esc(items.length)} / ${esc(totals.done ?? 0)}</span></h2>` +
      body +
      '</section>'
    );
  }

  function renderAbandonedSection(state) {
    const totals = state.totals || {};
    const items = state.abandoned_recent || [];
    let body = '';
    if (!items.length) {
      body = '<div class="empty-mini">No abandoned items.</div>';
    }
    body += items.map((it) => renderTerminalItem(it, 'abandoned')).join('');
    return (
      `<section id="section-abandoned">` +
      `<h2 class="section-title">Abandoned <span class="section-count">${esc(items.length)} / ${esc(totals.abandoned ?? 0)}</span></h2>` +
      body +
      '</section>'
    );
  }

  function buildQueueDOM(state) {
    // Build a detached <main> with the four sections in order. The merge
    // target is the live <main id="queue-root"> so we wrap in the same
    // tag + id.
    // Section order MUST match templates/index.html:
    //   RUNNING → BLOCKED → PENDING → DONE → ABANDONED.
    // The previous version (q-2026-05-20-db66) omitted BLOCKED entirely,
    // so the first refresh tick after page load discarded the server-
    // rendered #section-blocked: "BLOCKED 1" → vanishes after ~5s.
    const html =
      `<main id="queue-root">` +
      renderRunningSection(state) +
      renderBlockedSection(state) +
      renderPendingSection(state) +
      renderDoneSection(state) +
      renderAbandonedSection(state) +
      '</main>';
    const tpl = document.createElement('template');
    tpl.innerHTML = html;
    return tpl.content.firstElementChild;
  }

  function buildTopbarMetaDOM(state) {
    // Topbar count pills + dot + timestamp. The Info button (dropdown)
    // is OUTSIDE this element (it lives next to .meta as a sibling) so
    // we can rebuild .meta freely without disturbing the dropdown's
    // open state.
    //
    // Wait — re-read template: the .info-wrap IS inside .meta. We must
    // preserve it. Simplest: re-render the count/dot/ts as the first
    // children of a fresh .meta, then append a CLONE of the live
    // .info-wrap so morphdom sees a matching child (id=info-toggle / id=
    // info-dropdown are stable). Morphdom keys by id for the
    // info-dropdown so its hidden state is preserved naturally.
    const totals = state.totals || {};
    const startingCount = state.starting_count || 0;
    const orphanCount = state.orphan_count || 0;
    const fetchedAt = state.fetched_at || '';
    const cacheAge = state.cache_age_seconds;
    const errorTxt = state.error;

    let html = '';
    html += `<span class="count count-running" title="running items">${esc(totals.running ?? 0)} running</span>`;
    if (startingCount) {
      html += `<span class="count count-starting" title="registered but agent not yet emitting events">${esc(startingCount)} starting</span>`;
    }
    html += `<span class="count count-pending" title="pending items">${esc(totals.pending ?? 0)} pending</span>`;
    if (orphanCount) {
      html += `<span class="count count-orphan" title="running items with no live owner">${esc(orphanCount)} orphan</span>`;
    }
    // Source filter dropdown. MUST be rendered here too (not just in the
    // Jinja template): mergeTopbarMeta rebuilds #topbar-meta every tick,
    // so omitting it would let morphdom discard the server-rendered
    // <select> on the first tick — the same class of bug as the dropped
    // BLOCKED section (q-2026-05-20-db66). The selected value is
    // preserved across the merge by onBeforeElUpdated. Options are
    // rebuilt from state.sources (the global distinct created_by set).
    html += buildSourceFilterHTML(state.sources || []);
    html += `<span class="dot ${errorTxt ? 'dot-err' : 'dot-ok'}" title="${errorTxt ? 'fetch error' : 'live'}"></span>`;
    const tsAttrs = fetchedAt
      ? ` data-local-time-iso="${attr(fetchedAt)}" data-local-time-fmt="time" data-local-time-tooltip`
      : '';
    const tsText = fetchedAt
      ? `${esc(fetchedAt.substring(11, 19))}Z`
      : '—';
    html += `<span class="ts" title="last fetch"${tsAttrs}>${tsText}</span>`;

    // Re-attach the existing info-wrap (so Info dropdown state survives).
    // We append a string sentinel that morphdom's DOM diff will replace
    // with the live element via the onBeforeElUpdated hook. Simpler:
    // include the wrap markup with the SAME ids; morphdom matches by id
    // and preserves the hidden attr from the destination element.
    // BUT: open/closed state is driven by `hidden` attr only; if we
    // emit `hidden` here then morphdom will set `hidden` on the live
    // dropdown (closing it). We instead leave info-dropdown OFF
    // entirely from the merge by extracting it from the target before
    // merging — see onBeforeElUpdated below.
    html += `<div class="info-wrap">` +
      `<button type="button" id="info-toggle" class="info-btn" aria-haspopup="true" aria-expanded="false" aria-controls="info-dropdown" title="session info">ⓘ</button>` +
      `<div id="info-dropdown" class="info-dropdown" role="menu" aria-labelledby="info-toggle" hidden>` +
      `<div class="info-row"><span class="info-label">user</span><span class="info-value">—</span></div>` +
      `<div class="info-row"><span class="info-label">cache</span><span class="info-value"><span class="cache-age">${esc(cacheAge !== undefined && cacheAge !== null ? cacheAge : '?')}</span>s</span></div>` +
      `<div class="info-row"><span class="info-label">api</span><span class="info-value"><a href="/api/queue">/api/queue</a></span></div>` +
      `</div></div>`;

    const wrapper = document.createElement('div');
    wrapper.className = 'meta';
    wrapper.id = 'topbar-meta';
    wrapper.innerHTML = html;
    return wrapper;
  }

  // ---------------------------------------------------------------------
  // Morphdom merge — single entry point per-tree.
  //
  // Skip rules (onBeforeElUpdated):
  //   - `data-no-morph` attribute — entire subtree skipped.
  //   - `.dragging` class — drag in progress, leave it alone.
  //   - `.drop-target` class — drop highlight active, ditto.
  //   - `#log-modal`, `#action-modal` — defensive (also tagged data-no-morph).
  //   - `#info-dropdown` — preserve open/hidden state across ticks.
  //   - <details> open: preserve from-element's open state.
  //   - <input>/<textarea>/<select>: preserve user input value + focus
  //     across the merge (morphdom doesn't update form values by default
  //     for elements with a typed value, but be explicit).
  // ---------------------------------------------------------------------
  function shouldSkipElement(fromEl) {
    if (!(fromEl instanceof Element)) return false;
    if (fromEl.hasAttribute && fromEl.hasAttribute('data-no-morph')) return true;
    if (fromEl.id === 'log-modal' || fromEl.id === 'action-modal') return true;
    // Skip the entire info-wrap subtree — its contents (user email,
    // cache_age value) update via separate paths and we don't want to
    // toggle the dropdown's hidden state during a refresh tick.
    if (fromEl.classList && fromEl.classList.contains('info-wrap')) return true;
    if (fromEl.id === 'info-toggle' || fromEl.id === 'info-dropdown') return true;
    if (fromEl.classList && (fromEl.classList.contains('dragging') || fromEl.classList.contains('drop-target'))) return true;
    return false;
  }

  function onBeforeElUpdated(fromEl, toEl) {
    if (shouldSkipElement(fromEl)) return false;

    // Preserve <details> expansion across the tick.
    //
    // Subagent trees (.subagent-tree, keyed by data-tree-key) have their own
    // persistence: the canonical open/closed state lives in localStorage and
    // defaults to OPEN. Apply that to the freshly-built node so a user's
    // COLLAPSE survives — the generic "preserve if live open" rule below
    // can't express a collapse because both server + renderer emit `open` by
    // default (the new node is always open, so the tree would re-expand on
    // every tick). applySubtreeStateToEl reads localStorage and sets/clears
    // `open` accordingly (default open when unset).
    if (fromEl.classList && fromEl.classList.contains('subagent-tree')) {
      applySubtreeStateToEl(toEl);
    } else if (fromEl.tagName === 'DETAILS' && fromEl.open) {
      // Generic disclosures (Prompt blocks, etc.): if the live element is
      // open, mirror it onto the new element BEFORE morphdom diffs them.
      // Otherwise the server's `closed` state would slam the user-opened
      // panel shut on every tick.
      toEl.setAttribute('open', '');
    }

    // Preserve focused input/textarea state — value + focus.
    if ((fromEl.tagName === 'INPUT' || fromEl.tagName === 'TEXTAREA') &&
        document.activeElement === fromEl) {
      toEl.value = fromEl.value;
    }

    // Preserve the source-filter selection across the tick. The dropdown
    // is rebuilt every refresh (options can change as new producers
    // enqueue items), but the user's chosen value must persist. Copy the
    // live value onto the freshly-built <select> BEFORE morphdom diffs
    // them; if the previously-selected source is no longer an option,
    // the assignment is a no-op and the select falls back to "all".
    if (fromEl.tagName === 'SELECT' && fromEl.id === SOURCE_FILTER_ID) {
      toEl.value = fromEl.value;
    }

    // Preserve LocalTime hydration:
    //   - LocalTime.hydrate() replaces textContent with a localized
    //     time string and stamps `data-local-time-rendered=<iso>` to
    //     stay idempotent on subsequent calls.
    //   - Morphdom would by default REMOVE the `data-local-time-rendered`
    //     attr (missing from our renderer) AND replace the localized
    //     textContent with the server's UTC string, causing flicker.
    //   - When the iso hasn't changed, copy the rendered marker AND
    //     keep the live textContent.
    if (fromEl.hasAttribute && fromEl.hasAttribute('data-local-time-iso')) {
      const fromIso = fromEl.getAttribute('data-local-time-iso');
      const toIso = toEl.getAttribute('data-local-time-iso');
      const fromRendered = fromEl.getAttribute('data-local-time-rendered');
      if (fromRendered && fromRendered === fromIso && fromIso === toIso) {
        // Already-hydrated node, iso unchanged — preserve.
        toEl.setAttribute('data-local-time-rendered', fromRendered);
        toEl.textContent = fromEl.textContent;
        if (fromEl.title) toEl.title = fromEl.title;
      }
    }

    // Skip identical nodes (cheap fast-path; morphdom does this
    // internally too but skipping the descent saves a few cycles on
    // large prompt blobs).
    if (fromEl.isEqualNode && fromEl.isEqualNode(toEl)) return false;
    return true;
  }

  function onBeforeNodeDiscarded(node) {
    // Don't discard the action / log modals if morphdom encounters them
    // (it shouldn't, since they're outside #queue-root, but defensive).
    if (node && node.id === 'log-modal') return false;
    if (node && node.id === 'action-modal') return false;
    if (node && node.id === 'info-dropdown') return false;
    return true;
  }

  // Morphdom keys nodes by `el.id` by default. Our queue items use
  // `data-queue-id` (one card → one queue item) — without a custom key
  // resolver morphdom would match articles by sibling position, which
  // means moving an item between sections would discard + recreate the
  // node (losing focus, in-flight state, the .dragging escape hatch).
  // Returning data-queue-id makes morphdom track the card as a stable
  // entity across reorders.
  function getNodeKey(node) {
    if (node.nodeType !== 1) return undefined;
    if (node.id) return node.id;
    if (node.getAttribute) {
      const qid = node.getAttribute('data-queue-id');
      if (qid) return 'qid:' + qid;
    }
    return undefined;
  }

  function mergeQueueRoot(state) {
    const newRoot = buildQueueDOM(state);
    if (!window.morphdom) return false;
    window.morphdom(queueRoot, newRoot, {
      onBeforeElUpdated: onBeforeElUpdated,
      onBeforeNodeDiscarded: onBeforeNodeDiscarded,
      getNodeKey: getNodeKey,
      childrenOnly: false,
    });
    return true;
  }

  function mergeTopbarMeta(state) {
    if (!topbarMeta) return false;
    const newMeta = buildTopbarMetaDOM(state);
    if (!window.morphdom) return false;
    window.morphdom(topbarMeta, newMeta, {
      onBeforeElUpdated: onBeforeElUpdated,
      onBeforeNodeDiscarded: onBeforeNodeDiscarded,
      getNodeKey: getNodeKey,
      childrenOnly: false,
    });
    return true;
  }

  // ---------------------------------------------------------------------
  // Tick — fetch + merge. After a successful merge re-hydrate local-time
  // so any new timestamp elements are converted to the viewer's tz.
  // ---------------------------------------------------------------------
  async function tick() {
    try {
      const r = await fetch('/api/queue', { cache: 'no-store' });
      if (!r.ok) return;
      const j = await r.json();

      mergeQueueRoot(j);
      mergeTopbarMeta(j);

      // Re-apply the active source filter to the freshly-merged cards.
      // New/changed cards default to visible; this hides any that don't
      // match the current selection.
      applySourceFilter();

      // Re-apply persisted subagent-tree collapse state (default expanded).
      // onBeforeElUpdated already handles UPDATED nodes; this also covers any
      // newly-ADDED card whose tree morphdom inserted this tick.
      applySubtreeState();

      // Update the cache-age value inside the (skipped) info-dropdown so
      // it stays live without re-rendering the whole subtree.
      const cacheAgeEl = document.querySelector('.cache-age');
      if (cacheAgeEl) {
        cacheAgeEl.textContent =
          (j.cache_age_seconds !== undefined && j.cache_age_seconds !== null)
            ? j.cache_age_seconds
            : '?';
      }

      if (window.LocalTime && typeof window.LocalTime.hydrate === 'function') {
        try { window.LocalTime.hydrate(); } catch (_) { /* defensive */ }
      }
    } catch (_) {
      // Network blip — try again next tick.
    }
  }

  // Apply persisted subagent-tree collapse state to the server's first
  // paint (which always emits `open`). A previously-collapsed card is
  // restored to collapsed; everything else stays expanded (the default).
  applySubtreeState();

  // Don't fire the first tick immediately on load — the server-rendered
  // first paint is already correct. Schedule the recurring tick.
  setInterval(tick, REFRESH_MS);
  document.addEventListener('visibilitychange', () => {
    if (!document.hidden) tick();
  });

  // Expose internals for the test harness (test.html in the repo). The
  // production page never reads these but they exist so a smoke test can
  // exercise buildQueueDOM + the merge with synthetic JSON snapshots.
  window.__queueRefresh = {
    buildQueueDOM,
    buildTopbarMetaDOM,
    buildSourceFilterHTML,
    applySourceFilter,
    getSourceFilterValue,
    mergeQueueRoot,
    mergeTopbarMeta,
    onBeforeElUpdated,
    applySubtreeState,
    applySubtreeStateToEl,
  };
})();
