/* Umbra: small vanilla JS on top of htmx.
   - Live updates over SSE only while the tab is visible.
   - Pulses travel along map links as devices are checked.
   - Command palette (Ctrl+K).
   - Live view code is loaded on demand. */
(function () {
  'use strict';
  var d = document, b = d.body;
  if (!b) return;
  var reduce = window.matchMedia ? matchMedia('(prefers-reduced-motion: reduce)') : { matches: false };
  var SVGNS = 'http://www.w3.org/2000/svg';

  // ---- live updates -------------------------------------------------------
  var es = null, hiddenAt = 0, changeTimer = 0, retry = 0;

  function changed() {
    clearTimeout(changeTimer);
    changeTimer = setTimeout(function () {
      if (window.htmx) htmx.trigger(b, 'umbra:changed');
    }, 300);
  }

  function connect() {
    if (es || !b.dataset.sse || d.visibilityState !== 'visible' || !window.EventSource) return;
    es = new EventSource('/events');
    es.addEventListener('check', function (e) { try { onCheck(JSON.parse(e.data)); } catch (_) {} });
    es.addEventListener('changed', function (e) {
      try { var m = JSON.parse(e.data); if (m.mood) b.dataset.mood = m.mood; } catch (_) {}
      changed();
    });
    es.onopen = function () { retry = 0; };
    es.onerror = function () {
      if (es && es.readyState === 2) {
        es = null;
        retry = Math.min(retry + 1, 6);
        setTimeout(connect, 2000 * retry);
      }
    };
  }

  function disconnect() { if (es) { es.close(); es = null; } }

  d.addEventListener('visibilitychange', function () {
    if (d.visibilityState === 'visible') {
      if (hiddenAt && Date.now() - hiddenAt > 15000) changed();
      hiddenAt = 0;
      connect();
    } else {
      hiddenAt = Date.now();
      disconnect();
    }
  });
  window.addEventListener('pagehide', disconnect);
  connect();

  function fmtMs(ms) {
    if (ms == null) return '—';
    return (ms < 10 ? ms.toFixed(1) : Math.round(ms)) + ' ms';
  }

  function onCheck(c) {
    if (c.ok) {
      var els = d.querySelectorAll('[data-lat="' + (window.CSS && CSS.escape ? CSS.escape(c.t) : c.t) + '"]');
      for (var i = 0; i < els.length; i++) els[i].textContent = fmtMs(c.ms);
    }
    pulse(c.t, c.ok);
  }

  function pulse(id, ok) {
    if (reduce.matches) return;
    var path = d.getElementById('link-' + id);
    if (!path) {
      var node = d.getElementById('node-' + id);
      if (node) { node.classList.remove('ping'); void node.getBBox(); node.classList.add('ping'); }
      return;
    }
    var svg = path.ownerSVGElement;
    var layer = svg.querySelector('.pulses') || svg;
    var dot = d.createElementNS(SVGNS, 'circle');
    dot.setAttribute('r', '3');
    dot.setAttribute('class', ok ? 'pulse' : 'pulse pulse-fail');
    var am = d.createElementNS(SVGNS, 'animateMotion');
    am.setAttribute('dur', '1.2s');
    am.setAttribute('begin', 'indefinite');
    am.setAttribute('fill', 'freeze');
    am.setAttribute('calcMode', 'spline');
    am.setAttribute('keyTimes', '0;1');
    am.setAttribute('keySplines', '0.4 0 0.2 1');
    var mp = d.createElementNS(SVGNS, 'mpath');
    mp.setAttribute('href', '#link-' + id);
    am.appendChild(mp);
    dot.appendChild(am);
    layer.appendChild(dot);
    try { am.beginElement(); } catch (_) {}
    setTimeout(function () { dot.remove(); }, 1400);
  }

  // Keep the tab title in step with the header verdict.
  d.addEventListener('htmx:afterSwap', function (e) {
    var hdr = d.getElementById('hdr');
    if (hdr && hdr.dataset.title) d.title = hdr.dataset.title + ' · Umbra';
    var t = e.detail && e.detail.target;
    if (t && t.id === 'toasts') {
      var first = t.firstElementChild;
      if (first) setTimeout(function () { first.remove(); }, 7200);
    }
  });

  // ---- command palette ----------------------------------------------------
  var pal = d.getElementById('palette');
  var input = d.getElementById('palette-q');
  var list = d.getElementById('palette-list');
  var items = [];
  var shown = [];
  var sel = 0;
  try { items = JSON.parse((d.getElementById('palette-data') || {}).textContent || '[]'); } catch (_) {}
  var sprite = (function () {
    var u = d.querySelector('use[href*="/assets/icons."]');
    return u ? u.getAttribute('href').split('#')[0] : '';
  })();

  function icon(name) {
    return '<svg class="icon" aria-hidden="true"><use href="' + sprite + '#' + name + '"></use></svg>';
  }
  function esc(s) {
    return String(s).replace(/[&<>"']/g, function (c) { return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]; });
  }

  function render() {
    var q = input.value.trim().toLowerCase().split(/\s+/).filter(Boolean);
    shown = items.filter(function (it) {
      var hay = (it.label + ' ' + it.hint).toLowerCase();
      return q.every(function (w) { return hay.indexOf(w) >= 0; });
    }).slice(0, 40);
    sel = Math.min(sel, Math.max(shown.length - 1, 0));
    list.innerHTML = shown.map(function (it, i) {
      return '<li role="option" data-i="' + i + '" aria-selected="' + (i === sel) + '">' + icon(it.icon || 'chevron-right') +
        '<span>' + esc(it.label) + '</span><span class="hint">' + esc(it.hint || '') + '</span></li>';
    }).join('') || '<li aria-disabled="true"><span>No matches</span></li>';
    var cur = list.querySelector('[aria-selected="true"]');
    if (cur && cur.scrollIntoView) cur.scrollIntoView({ block: 'nearest' });
  }

  function openPalette() {
    if (!pal || pal.open) return;
    input.value = '';
    sel = 0;
    render();
    pal.showModal();
    input.focus();
  }

  function run(it) {
    if (!it) return;
    pal.close();
    if (it.href) { location.href = it.href; return; }
    if (it.post && window.htmx) {
      htmx.ajax('POST', it.post, { target: '#toasts', swap: 'afterbegin', headers: csrfHeaders() });
      return;
    }
    if (it.live) openLive(it.live, it.device, it.name);
  }

  function csrfHeaders() {
    try { return JSON.parse(b.getAttribute('hx-headers') || '{}'); } catch (_) { return {}; }
  }

  if (pal) {
    input.addEventListener('input', function () { sel = 0; render(); });
    input.addEventListener('keydown', function (e) {
      if (e.key === 'ArrowDown') { sel = Math.min(sel + 1, shown.length - 1); render(); e.preventDefault(); }
      else if (e.key === 'ArrowUp') { sel = Math.max(sel - 1, 0); render(); e.preventDefault(); }
      else if (e.key === 'Enter') { run(shown[sel]); e.preventDefault(); }
    });
    list.addEventListener('click', function (e) {
      var li = e.target.closest('li[data-i]');
      if (li) run(shown[+li.dataset.i]);
    });
    pal.addEventListener('click', function (e) { if (e.target === pal) pal.close(); });
  }

  d.addEventListener('keydown', function (e) {
    if ((e.ctrlKey || e.metaKey) && (e.key === 'k' || e.key === 'K')) { e.preventDefault(); openPalette(); }
  });
  d.addEventListener('click', function (e) {
    if (e.target.closest('[data-palette-open]')) { e.preventDefault(); openPalette(); return; }
    var btn = e.target.closest('[data-live]');
    if (btn) { e.preventDefault(); openLive(btn.dataset.live, btn.dataset.device, btn.dataset.name); }
  });

  // ---- live view (loaded on demand) -----------------------------------------
  function openLive(source, device, name) {
    import(b.dataset.liveJs).then(function (m) { m.open(source, device, name, csrfHeaders()); })
      .catch(function () { alert('Live view could not load.'); });
  }
})();
