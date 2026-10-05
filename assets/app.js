'use strict';

// Polls a versioned endpoint (cheap 304s when nothing changed) only while the tab is
// visible, and patches cards in place so thumbnails never reload or flicker.

const POLL_MS = 3000;
const BACKOFF_MS = 10000;

const $ = (s, el = document) => el.querySelector(s);
const prefs = document.documentElement.dataset;
const thumbsOn = () => prefs.thumbs !== 'off';
const tpl = $('#card-tpl');
const groups = Object.fromEntries([...document.querySelectorAll('.group')].map((g) => [g.dataset.group, g]));
const cards = new Map();
let version = 0;
let services = [];
let scannedAt = 0;
let timer = 0;
let inflight = false;
let failing = false;
let firstPaint = true;
let editing = null;
let thumbsEnabled = true;
let tailnet = null;

// Opened from another device (e.g. over Tailscale), localhost would mean that device, so
// links use the tailnet address the server worked out for each service.
// A loopback-only service has no such address; linking to localhost there would open
// whatever happens to run on the viewer's own device, so it gets no link until shared.
const remoteView = !['localhost', '127.0.0.1', '[::1]'].includes(location.hostname);
const hrefOf = (s) => (remoteView ? s.remote_url || (s.tailnet ? null : s.url) : s.url);
const sharing = new Set();

const BADGE = { live: 'live', error: 'error', stale: 'stopped', offline: 'offline', other: 'running' };

function ago(ts) {
  const s = Math.max(0, Math.round(Date.now() / 1000 - ts));
  if (s < 5) return 'just now';
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

function groupOf(s) {
  if (s.hidden) return 'hidden';
  if (s.pinned) return 'pinned';
  if (s.state === 'other') return 'other';
  if (s.state === 'stale' || s.state === 'offline') return 'stale';
  return 'live';
}

function setText(el, text) {
  const t = text ?? '';
  if (el.textContent !== t) el.textContent = t;
}

function buildCard(s, index) {
  const el = tpl.content.firstElementChild.cloneNode(true);
  el.dataset.port = s.port;
  el.classList.add('enter');
  if (firstPaint) el.style.setProperty('--i', index);
  el.addEventListener('animationend', () => el.classList.remove('enter'), { once: true });
  const img = $('img', el);
  const thumb = $('.thumb', el);
  img.addEventListener('load', () => thumb.classList.add('has-img'));
  img.addEventListener('error', () => thumb.classList.remove('has-img'));
  $('.actions', el).addEventListener('click', onAction);
  return el;
}

function patchCard(el, s) {
  const gone = s.state === 'stale' || s.state === 'offline';
  el.classList.toggle('gone', gone);
  el.classList.toggle('pinned', s.pinned);
  const hit = $('.hit', el);
  const href = hrefOf(s);
  if (!href) hit.removeAttribute('href');
  else if (hit.getAttribute('href') !== href) hit.href = href;
  hit.setAttribute('aria-label', `Open ${s.name} on port ${s.port}`);
  hit.title = remoteView && s.tailnet === 'local' ? 'Only reachable on the host machine. Share it on the tailnet first.' : '';

  if (editing !== s.port) setText($('.name', el), s.name);
  const subtitle = s.title && s.title !== s.name ? s.title : (s.project && s.project !== s.name ? s.project : '');
  setText($('.title', el), subtitle);
  setText($('.port', el), `:${s.port}`);
  const status = s.status && s.status >= 400 && s.state !== 'error' ? ` ${s.status}` : '';
  setText($('.rt', el), (s.runtime || '') + status);
  setText($('.path', el), gone && s.last_seen ? `seen ${ago(s.last_seen)}` : (s.cwd ? `\u200E${s.cwd}\u200E` : ''));

  const badge = $('.badge', el);
  const label = s.state === 'error' ? `${s.status}` : BADGE[s.state] || s.state;
  setText(badge, label);
  badge.className = `badge ${s.state}`;

  setText($('.ph-port', el), String(s.port));
  const img = $('img', el);
  if (s.thumb && thumbsOn()) {
    if (img.getAttribute('src') !== s.thumb) img.src = s.thumb;
  } else if (!s.thumb && img.hasAttribute('src')) {
    img.removeAttribute('src');
    $('.thumb', el).classList.remove('has-img');
  }

  // Pinned cards drag as a whole; the link overlay must not start its own URL drag.
  el.draggable = s.pinned && !s.hidden;
  hit.draggable = !el.draggable;

  const pin = $('[data-op=pin]', el);
  pin.classList.toggle('on', s.pinned);
  pin.title = s.pinned ? 'Unpin' : 'Pin';
  $('[data-op=recapture]', el).hidden = gone || !thumbsEnabled;
  $('[data-op=forget]', el).hidden = !gone;
  $('[data-op=hide]', el).hidden = gone;

  const share = $('[data-op=share]', el);
  const shared = s.tailnet === 'shared';
  share.hidden = !tailnet || !(shared || (s.tailnet === 'local' && !gone));
  share.classList.toggle('on', shared);
  share.disabled = sharing.has(s.port);
  share.title = shared ? `Stop sharing on tailnet (${s.remote_url})` : `Share on tailnet (${tailnet})`;
}

function rowFor(s, kind) {
  const li = document.createElement('li');
  const p = document.createElement('span');
  p.className = 'p';
  p.textContent = `:${s.port}`;
  const n = document.createElement('span');
  n.className = 'n';
  n.textContent = [s.name !== `:${s.port}` ? s.name : '', s.runtime, s.cwd].filter(Boolean).join('  ·  ');
  li.append(p, n);
  const button = (label, fn) => {
    const b = document.createElement('button');
    b.textContent = label;
    b.onclick = fn;
    return b;
  };
  if (kind === 'hidden') {
    li.append(button('Unhide', () => act('hide', s.port, false)));
  } else {
    if (s.status && hrefOf(s)) {
      const a = document.createElement('a');
      a.href = hrefOf(s);
      a.target = '_blank';
      a.rel = 'noopener';
      a.textContent = 'Open';
      li.append(a);
    }
    li.append(button('Pin', () => act('pin', s.port, true)), button('Hide', () => act('hide', s.port, true)));
  }
  return li;
}

function render() {
  // Never reshuffle cards under the pointer; the drop handler re-renders.
  if (dragEl) {
    pendingRender = true;
    return;
  }
  pendingRender = false;
  const buckets = { pinned: [], live: [], stale: [], other: [], hidden: [] };
  for (const s of services) buckets[groupOf(s)].push(s);
  buckets.pinned.sort((a, b) => a.pin_order - b.pin_order || a.port - b.port);
  buckets.stale.sort((a, b) => (b.last_seen || 0) - (a.last_seen || 0));

  const keep = new Set();
  let i = 0;
  for (const key of ['pinned', 'live', 'stale']) {
    const grid = $('.grid', groups[key]);
    let prev = null;
    for (const s of buckets[key]) {
      keep.add(s.port);
      let el = cards.get(s.port);
      if (!el) {
        el = buildCard(s, i);
        cards.set(s.port, el);
      }
      i++;
      patchCard(el, s);
      const want = prev ? prev.nextSibling : grid.firstChild;
      if (el.parentNode !== grid || want !== el) grid.insertBefore(el, want);
      prev = el;
    }
  }
  for (const [port, el] of cards) {
    if (!keep.has(port)) {
      el.remove();
      cards.delete(port);
    }
  }
  for (const key of ['other', 'hidden']) {
    const ul = $('.rows', groups[key]);
    ul.replaceChildren(...buckets[key].map((s) => rowFor(s, key)));
    setText($('.count', groups[key]), String(buckets[key].length));
  }
  firstPaint = false;
  applyFilter();
}

function applyFilter() {
  const q = $('#q').value.trim().toLowerCase();
  const byPort = new Map(services.map((s) => [s.port, s]));
  for (const [port, el] of cards) {
    const s = byPort.get(port);
    const hay = s ? `${port} :${port} ${s.name} ${s.title || ''} ${s.project || ''} ${s.runtime || ''} ${s.cwd || ''}`.toLowerCase() : '';
    el.hidden = q !== '' && !hay.includes(q);
  }
  for (const [key, g] of Object.entries(groups)) {
    let has;
    if (key === 'other' || key === 'hidden') has = $('.rows', g).children.length > 0 && q === '';
    else has = [...$('.grid', g).children].some((c) => !c.hidden);
    g.classList.toggle('has', has);
  }
  const anyCard = [...cards.values()].some((c) => !c.hidden);
  $('#empty').hidden = anyCard || q !== '';
}

function setStatus() {
  const st = $('#status');
  st.classList.toggle('ok', !failing);
  st.classList.toggle('down', failing);
  const live = services.filter((s) => (s.state === 'live' || s.state === 'error') && !s.hidden).length;
  setText($('#status-text'), failing ? 'server unreachable' : `${live} running · ${scannedAt ? ago(scannedAt) : 'scanning'}`);
}

function apply(state) {
  version = state.version;
  services = state.services;
  scannedAt = state.scanned_at || scannedAt;
  thumbsEnabled = state.thumbs;
  tailnet = state.tailnet;
  setText($('#foot-note'),
    `Updates every 3s while this tab is visible. Stopped services clear after ${state.stale_minutes} min unless pinned.` +
    (state.thumbs ? '' : ' Thumbnails are off (no Chromium-family browser found).'));
  render();
}

async function poll() {
  if (inflight) return;
  inflight = true;
  clearTimeout(timer);
  try {
    const r = await fetch(`/api/state?v=${version}&thumbs=${thumbsOn() ? 1 : 0}`, { cache: 'no-store' });
    scannedAt = Number(r.headers.get('x-scanned-at')) || scannedAt;
    if (r.status === 200) apply(await r.json());
    else if (r.status !== 304) throw new Error(r.status);
    failing = false;
  } catch {
    failing = true;
  } finally {
    inflight = false;
    // Refresh relative times on stopped cards without a full re-render.
    for (const s of services) {
      const el = cards.get(s.port);
      if (el && s.last_seen && (s.state === 'stale' || s.state === 'offline')) setText($('.path', el), `seen ${ago(s.last_seen)}`);
    }
    setStatus();
    schedule();
  }
}

function schedule() {
  clearTimeout(timer);
  if (!document.hidden) timer = setTimeout(poll, failing ? BACKOFF_MS : POLL_MS);
}

async function post(path, body) {
  const r = await fetch(path, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body || {}) });
  if (!r.ok) throw new Error(await r.text());
  apply(await r.json());
  setStatus();
}

async function act(op, port, value) {
  // Sharing shells out to Tailscale and can take seconds; ignore repeat clicks meanwhile.
  if (op === 'share') {
    if (sharing.has(port)) return;
    sharing.add(port);
    render();
  }
  try {
    await post('/api/action', { op, port, value });
  } catch (e) {
    console.warn(op, e);
    // Sharing shells out to Tailscale and can fail for reasons worth showing.
    if (op === 'share') setText($('#status-text'), `tailnet: ${e.message}`);
  } finally {
    if (sharing.delete(port)) render();
  }
}

async function refresh() {
  const btn = $('#refresh');
  if (btn.classList.contains('spinning')) return;
  btn.classList.add('spinning');
  try { await post('/api/refresh'); failing = false; } catch { failing = true; }
  btn.classList.remove('spinning');
  setStatus();
  schedule();
}

function onAction(ev) {
  const btn = ev.target.closest('button[data-op]');
  if (!btn) return;
  ev.preventDefault();
  const port = Number(btn.closest('.card').dataset.port);
  const s = services.find((x) => x.port === port);
  if (!s) return;
  const op = btn.dataset.op;
  if (op === 'pin') act('pin', port, !s.pinned);
  else if (op === 'hide') act('hide', port, true);
  else if (op === 'forget') act('forget', port);
  else if (op === 'recapture') act('recapture', port);
  else if (op === 'share') act('share', port, s.tailnet !== 'shared');
  else if (op === 'rename') startRename(btn.closest('.card'), s);
}

function startRename(el, s) {
  const h = $('.name', el);
  editing = s.port;
  const input = document.createElement('input');
  input.value = s.label || s.name;
  input.placeholder = s.project || s.title || `:${s.port}`;
  h.replaceChildren(input);
  input.focus();
  input.select();
  let done = false;
  const finish = (save) => {
    if (done) return;
    done = true;
    editing = null;
    h.textContent = s.name;
    if (save) act('rename', s.port, input.value);
  };
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') finish(true);
    if (e.key === 'Escape') finish(false);
  });
  input.addEventListener('blur', () => finish(true));
}

// Drag and drop reordering of pinned cards. The order lives on the server so it is
// the same in every browser.
const pinGrid = $('.grid', groups.pinned);
let dragEl = null;
let dragStartOrder = '';
let pendingRender = false;
let lastMoveAt = 0;

const pinnedOrder = () => [...pinGrid.children].map((c) => Number(c.dataset.port));

// FLIP: move the DOM node instantly, then animate siblings from their old spots.
function animateMove(mutate) {
  const before = new Map([...pinGrid.children].map((c) => [c, c.getBoundingClientRect()]));
  mutate();
  lastMoveAt = performance.now();
  if (matchMedia('(prefers-reduced-motion: reduce)').matches) return;
  for (const [c, r] of before) {
    const now = c.getBoundingClientRect();
    const dx = r.left - now.left;
    const dy = r.top - now.top;
    if (dx || dy) {
      c.animate([{ transform: `translate(${dx}px, ${dy}px)` }, { transform: 'none' }], {
        duration: 180,
        easing: 'cubic-bezier(0.16, 1, 0.3, 1)',
      });
    }
  }
}

function saveOrder() {
  const order = pinnedOrder();
  order.forEach((port, i) => {
    const s = services.find((x) => x.port === port);
    if (s) s.pin_order = i;
  });
  act('reorder', null, order);
}

pinGrid.addEventListener('dragstart', (e) => {
  const el = e.target.closest && e.target.closest('.card');
  if (!el || !el.draggable) return;
  dragEl = el;
  dragStartOrder = pinnedOrder().join();
  e.dataTransfer.effectAllowed = 'move';
  e.dataTransfer.setData('text/plain', el.dataset.port);
  requestAnimationFrame(() => el.classList.add('dragging'));
});

pinGrid.addEventListener('dragover', (e) => {
  if (!dragEl) return;
  e.preventDefault();
  e.dataTransfer.dropEffect = 'move';
  // Let the previous move settle so animating cards don't make the target flicker.
  if (performance.now() - lastMoveAt < 180) return;
  const over = e.target.closest('.card');
  if (!over || over === dragEl || over.parentNode !== pinGrid) return;
  const r = over.getBoundingClientRect();
  const after = prefs.view === 'list' ? e.clientY > r.top + r.height / 2 : e.clientX > r.left + r.width / 2;
  const ref = after ? over.nextSibling : over;
  if (ref !== dragEl && dragEl.nextSibling !== ref) animateMove(() => pinGrid.insertBefore(dragEl, ref));
});

pinGrid.addEventListener('drop', (e) => {
  if (dragEl) e.preventDefault();
});

pinGrid.addEventListener('dragend', () => {
  if (!dragEl) return;
  dragEl.classList.remove('dragging');
  dragEl = null;
  if (pinnedOrder().join() !== dragStartOrder) saveOrder();
  else if (pendingRender) render();
});

// Keyboard reordering: Alt + arrow keys on a focused pinned card.
document.addEventListener('keydown', (e) => {
  if (!e.altKey || !['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown'].includes(e.key)) return;
  const focused = document.activeElement;
  const el = focused && focused.closest('.card');
  if (!el || el.parentNode !== pinGrid) return;
  e.preventDefault();
  const back = e.key === 'ArrowLeft' || e.key === 'ArrowUp';
  const sib = back ? el.previousElementSibling : el.nextElementSibling;
  if (!sib) return;
  animateMove(() => pinGrid.insertBefore(el, back ? sib : sib.nextSibling));
  focused.focus();
  saveOrder();
});

function setPref(key, value) {
  prefs[key] = value;
  localStorage.setItem(`portmap.${key}`, value);
}

function setView(view) {
  if (prefs.view !== view) setPref('view', view);
}

function toggleThumbs() {
  setPref('thumbs', thumbsOn() ? 'off' : 'on');
  $('#thumbs').title = thumbsOn() ? 'Hide thumbnails (t)' : 'Show thumbnails (t)';
  render();
  // Tell the server right away so it starts or stops capturing.
  version = 0;
  poll();
}

$('#thumbs').title = thumbsOn() ? 'Hide thumbnails (t)' : 'Show thumbnails (t)';
$('#view-grid').addEventListener('click', () => setView('grid'));
$('#view-list').addEventListener('click', () => setView('list'));
$('#thumbs').addEventListener('click', toggleThumbs);
$('#refresh').addEventListener('click', refresh);
$('#clear-stale').addEventListener('click', () => act('clear_stale'));
$('#theme').addEventListener('click', () => {
  const next = document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark';
  document.documentElement.dataset.theme = next;
  localStorage.setItem('portmap.theme', next);
});
$('#q').addEventListener('input', applyFilter);
$('#q').addEventListener('keydown', (e) => {
  if (e.key === 'Escape') { e.target.value = ''; applyFilter(); e.target.blur(); }
  if (e.key === 'Enter') {
    const first = [...document.querySelectorAll('.grid .card')].find((c) => !c.hidden);
    const href = first && $('.hit', first).getAttribute('href');
    if (href) window.open(href, '_blank', 'noopener');
  }
});
document.addEventListener('keydown', (e) => {
  const typing = e.target.closest('input, textarea');
  if (typing || e.metaKey || e.ctrlKey || e.altKey) return;
  if (e.key === '/') { e.preventDefault(); $('#q').focus(); }
  if (e.key === 'r') refresh();
  if (e.key === 'g') setView('grid');
  if (e.key === 'l') setView('list');
  if (e.key === 't') toggleThumbs();
});
document.addEventListener('visibilitychange', () => {
  if (document.hidden) clearTimeout(timer);
  else poll();
});

poll();
