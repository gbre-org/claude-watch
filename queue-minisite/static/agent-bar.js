// Agent-activity bar popover (botchat #3066: "port botchat's agent bar").
//
// The header's bottom half-row is one outlined pill — `● N agents · C calls ·
// K tok` (#agent-bar, a <button>) — and THIS file owns the per-agent
// breakdown behind it (#agent-bar-pop): head "N live agents — C calls · K ctx
// · O out", a table with one row per live agent (description; type · queue id
// · last tool; calls / ctx / out / age since spawn) and a footer with the main
// loop's context size, the snapshot freshness and the host. Same layout as the
// botchat popover this look is ported from.
//
// Data: the server's `agent_stats` header payload (see app.py
// _agent_stats_header) — every string pre-formatted server-side (one
// formatter). First paint reads the JSON seed the template embeds
// (<script type="application/json" id="agent-bar-data">); afterwards
// refresh.js mergeTopbarMeta() calls window.__qsiteAgentBar.update(payload)
// every 5s tick, so an OPEN popover repaints live and a payload that goes
// away (feature off / snapshot gone) closes it. Painted with textContent
// only — descriptions come from agent prompts.
//
// Interaction (mirrors botchat): click/tap PINS the popover (click again,
// Esc, or an outside click closes it); hovering the pill PEEKS (closes ~250ms
// after the pointer leaves pill + popover) unless pinned. All handlers are
// DELEGATED on document because #agent-bar is rebuilt every tick by
// refresh.js — the same survival pattern as the density toggle. The popover
// itself sits OUTSIDE #topbar-meta, so the morph never touches it; the
// rebuilt pill mirrors the live open state (class `open` + aria-expanded).
(function () {
  'use strict';

  const pop = document.getElementById('agent-bar-pop');
  if (!pop) return;

  let last = null;      // latest header payload (null = no pill)
  let pinned = false;   // click toggles; hover is transient
  let hoverTimer = null;

  const seed = document.getElementById('agent-bar-data');
  if (seed) {
    try { last = JSON.parse(seed.textContent || 'null'); } catch (e) { last = null; }
  }

  function bar() { return document.getElementById('agent-bar'); }

  function el(tag, cls, text) {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
  }

  function paint() {
    while (pop.firstChild) pop.removeChild(pop.firstChild);
    const data = last;
    if (!data) { pop.appendChild(el('div', 'abp-empty', 'no agent data')); return; }
    const rows = Array.isArray(data.rows) ? data.rows : [];
    const n = data.stale ? null : (Number(data.agents) || rows.length);

    const head = el('div', 'abp-head');
    if (data.stale) {
      head.appendChild(el('span', null, 'agent activity: n/a'));
      head.appendChild(el('span', 'abp-stale', 'stale snapshot'));
    } else {
      head.appendChild(el('span', null, n + ' live agent' + (n === 1 ? '' : 's')));
      head.appendChild(el('span', null, (data.calls_text || '?') + ' calls · ' +
        (data.tok_text || '?') + ' ctx · ' + (data.out_text || '–') + ' out'));
    }
    pop.appendChild(head);

    if (rows.length) {
      const table = document.createElement('table');
      const thead = document.createElement('thead');
      const hr = document.createElement('tr');
      ['agent', 'calls', 'ctx', 'out', 'age'].forEach((h) => hr.appendChild(el('th', null, h)));
      thead.appendChild(hr); table.appendChild(thead);
      const tbody = document.createElement('tbody');
      rows.forEach((a) => {
        const tr = document.createElement('tr');
        if (a.queue_id) tr.setAttribute('data-queue-id', a.queue_id);
        if (a.agent_id) tr.setAttribute('data-agent-id', a.agent_id);
        const td = el('td', 'abp-desc');
        td.appendChild(el('div', null, a.description || a.agent_id || '?'));
        const sub = [];
        if (a.agent_type) sub.push(a.agent_type);
        if (a.queue_id) sub.push(a.queue_id);
        if (a.last_tool) sub.push('last: ' + a.last_tool);
        if (sub.length) td.appendChild(el('div', 'abp-type', sub.join(' · ')));
        tr.appendChild(td);
        tr.appendChild(el('td', null, a.calls_text || '?'));
        tr.appendChild(el('td', null, a.ctx_text || '?'));
        tr.appendChild(el('td', null, a.out_text || '–'));
        const ageTd = el('td', null, a.age_text || '?');
        if (a.last_write_text) ageTd.title = 'last transcript write ' + a.last_write_text + ' ago';
        tr.appendChild(ageTd);
        tbody.appendChild(tr);
      });
      table.appendChild(tbody);
      pop.appendChild(table);
    } else {
      pop.appendChild(el('div', 'abp-empty', data.stale ? 'counters withheld — snapshot is stale' : 'no live agents'));
    }

    const foot = el('div', 'abp-foot');
    if (data.main && data.main.text) {
      foot.appendChild(el('span', null, 'main loop ctx ' + data.main.text +
        (data.main.age_text ? ' (' + data.main.age_text + ' ago)' : '')));
    }
    if (data.stale) {
      foot.appendChild(el('span', 'abp-stale', 'STALE — producer silent ' + (data.age_text || '?')));
    } else {
      foot.appendChild(el('span', null, 'updated ' + (data.age_text || '?') + ' ago' +
        (data.host ? ' · ' + data.host : '')));
    }
    pop.appendChild(foot);
  }

  function syncBar() {
    const b = bar();
    if (!b) return;
    const open = !pop.hidden;
    b.classList.toggle('open', open);
    b.setAttribute('aria-expanded', open ? 'true' : 'false');
  }

  function open() {
    if (!bar() || !last) return;
    paint();
    pop.hidden = false;
    syncBar();
  }

  function close() {
    pinned = false;
    if (hoverTimer) { clearTimeout(hoverTimer); hoverTimer = null; }
    pop.hidden = true;
    syncBar();
  }

  function isOpen() { return !pop.hidden; }

  // Called by refresh.js after every #topbar-meta merge with the fresh
  // payload (or null). Repaints an open popover; closes it when the pill is
  // gone; re-syncs the (rebuilt) pill's open state.
  function update(data) {
    last = data || null;
    if (!last) { close(); return; }
    if (!pop.hidden) paint();
    syncBar();
  }

  function inBarOrPop(node) {
    if (!node || !node.closest) return false;
    return !!node.closest('#agent-bar, #agent-bar-pop');
  }

  function scheduleHoverClose() {
    if (pinned) return;
    if (hoverTimer) clearTimeout(hoverTimer);
    hoverTimer = setTimeout(() => { if (!pinned) close(); }, 250);
  }

  document.addEventListener('click', (ev) => {
    const t = ev.target;
    const onBar = t && t.closest ? t.closest('#agent-bar') : null;
    if (onBar) {
      ev.stopPropagation();
      if (pinned) { close(); return; }
      pinned = true;
      open();
      return;
    }
    if (pop.hidden) return;
    if (pop.contains(t)) return;
    close();
  });
  document.addEventListener('mouseover', (ev) => {
    if (!inBarOrPop(ev.target)) return;
    if (hoverTimer) { clearTimeout(hoverTimer); hoverTimer = null; }
    if (!pinned && pop.hidden && ev.target.closest('#agent-bar')) open();
  });
  document.addEventListener('mouseout', (ev) => {
    if (!inBarOrPop(ev.target)) return;
    if (inBarOrPop(ev.relatedTarget)) return;
    scheduleHoverClose();
  });
  document.addEventListener('keydown', (ev) => {
    if (ev.key === 'Escape' && !pop.hidden) {
      close();
      const b = bar();
      if (b) b.focus();
    }
  });

  syncBar();
  window.__qsiteAgentBar = { update, open, close, paint, isOpen, get last() { return last; } };
})();
