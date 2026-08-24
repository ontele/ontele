/* Copyright 2026 The Ontele Authors — SPDX-License-Identifier: Apache-2.0 */
/* Live TV: channel list (now/next) and the EPG guide grid. */

import { $, $$, el, append, api, icon, img, toast, confirm, fmtClock, fmtDateLong, pref, busy, emptyState, session, view, ambientFrom, debounce, navigate } from '../core.js';

const HOUR = 3600000;
const FAV_KEY = 'live.favs';
const favs = () => new Set(pref.get(FAV_KEY, []));
const saveFavs = (s) => pref.set(FAV_KEY, [...s]);

// ---------- shared helpers ----------
function channelLogo(ch, cls = 'logo') {
  const box = el('div', { class: cls });
  box.append(el('span', { class: 'num-badge' }, ch.guideNumber));
  if (ch.icon) {
    const i = img(ch.icon, { alt: '' });
    i.addEventListener('load', () => box.classList.add('has-img'));
    box.append(i);
  }
  return box;
}
const airingRange = (a) => `${fmtClock(a.start)} – ${fmtClock(a.end)}`;
const airingPct = (a, now = Date.now()) => {
  const s = +new Date(a.start), e = +new Date(a.end);
  return Math.max(0, Math.min(100, ((now - s) / (e - s)) * 100));
};
const seCode = (a) => (a.season != null && a.episode != null ? `S${a.season}·E${a.episode}` : '');
const minsLeft = (a) => Math.max(0, Math.round((+new Date(a.end) - Date.now()) / 60000));

async function recordAiring(ch, a) {
  if (!a) {
    // manual one-hour capture starting now
    const start = new Date();
    const end = new Date(start.getTime() + HOUR);
    return api.post('/api/dvr/record', { channelId: ch.guideNumber, title: `${ch.guideName} · manual`, start: start.toISOString(), end: end.toISOString() });
  }
  return api.post('/api/dvr/record', {
    channelId: ch.guideNumber, title: a.title, subtitle: a.subtitle || null, description: a.description || null,
    start: a.start, end: a.end, season: a.season ?? null, episode: a.episode ?? null,
  });
}
const addSeries = (title, channelId) => api.post('/api/dvr/rules', { title, channelId: channelId || null, keep: 0 });

// =====================================================================
// Live TV: channel list
// =====================================================================
export async function renderLive() {
  ambientFrom(null);
  const isAdmin = !!session.user?.isAdmin;
  const page = el('div', { class: 'page live-page' });
  const head = el('div', { class: 'page-head' }, el('h1', {}, 'Live TV'), el('span', { class: 'count' }, 'Loading channels…'));
  page.append(head, el('div', { class: 'chan-skel' }, Array.from({ length: 6 }, () => el('div', { class: 'chan skel-chan' }, el('div', { class: 'skel-line', style: { width: '40%' } }), el('div', { class: 'skel-line', style: { width: '70%' } })))));
  view.replaceChildren(page);

  let data;
  try { data = await api.get('/api/livetv/channels'); } catch (e) {
    view.replaceChildren(el('div', { class: 'page' }, el('div', { class: 'page-head' }, el('h1', {}, 'Live TV')), emptyState('Could not load channels', e.message, 'live')));
    return;
  }
  const channels = data.channels || [];
  const dev = data.device;
  const favSet = favs();
  const query = { q: '' };

  // ---- header ----
  const devChip = dev ? el('span', { class: 'chip dev-chip', title: dev.BaseURL || '' }, icon('live'), `${dev.ModelNumber || dev.FriendlyName || 'HDHomeRun'} · ${channels.length} channel${channels.length === 1 ? '' : 's'}${dev.TunerCount ? ` · ${dev.TunerCount} tuners` : ''}`) : null;
  const search = el('input', { type: 'search', class: 'chan-search', placeholder: 'Filter channels…', 'aria-label': 'Filter channels', autocomplete: 'off' });
  const refreshBtn = isAdmin ? el('button', { class: 'btn small' }, icon('refresh'), 'Refresh lineup') : null;
  if (refreshBtn) refreshBtn.onclick = () => busy(refreshBtn, () => api.post('/api/livetv/refresh'), 'Tuner & guide refresh started').then(() => setTimeout(() => { if (location.hash.startsWith('#/live')) navigate(); }, 1200));
  const h = el('div', { class: 'page-head' },
    el('h1', {}, 'Live TV'), devChip, el('span', { class: 'spacer' }),
    el('div', { class: 'live-tools' },
      el('label', { class: 'chan-search-wrap' }, icon('search'), search),
      el('a', { class: 'btn small', href: '#/guide' }, icon('dvr'), 'Guide'),
      refreshBtn));

  if (!channels.length) {
    view.replaceChildren(el('div', { class: 'page' }, h,
      el('div', { class: 'empty' }, icon('live'),
        el('div', {},
          el('b', {}, 'No tuner found yet'),
          'Ontele discovers HDHomeRun tuners on your LAN automatically. If discovery is blocked (Docker, VLANs), set the tuner IP under ',
          el('a', { href: '#/settings', class: 'link' }, 'Settings → Live TV & DVR'), ' (hdhrIp) and refresh the lineup.'))));
    return;
  }

  // ---- rows ----
  const list = el('div', { class: 'chan-list', role: 'list' });
  const rows = new Map(); // guideNumber -> { row, ch, refs }

  function buildRow(ch) {
    const refs = {};
    const isFav = favSet.has(ch.guideNumber);
    const star = el('button', { class: `star ${isFav ? 'on' : ''}`, type: 'button', title: isFav ? 'Unfavorite' : 'Favorite', 'aria-pressed': String(isFav), 'aria-label': `Favorite ${ch.guideName}` }, icon('star'));
    star.onclick = (e) => {
      e.stopPropagation();
      const s = favs();
      if (s.has(ch.guideNumber)) s.delete(ch.guideNumber); else s.add(ch.guideNumber);
      saveFavs(s);
      const on = s.has(ch.guideNumber);
      star.classList.toggle('on', on);
      star.setAttribute('aria-pressed', String(on));
      star.title = on ? 'Unfavorite' : 'Favorite';
      layout();
      star.focus({ preventScroll: true });
    };
    refs.nowTitle = el('b', {});
    refs.nowTime = el('small', {});
    refs.bar = el('i', {});
    refs.airbar = el('div', { class: 'airbar' }, refs.bar);
    refs.next = el('small', { class: 'next' });
    const now = el('div', { class: 'now' }, refs.nowTitle, refs.nowTime, refs.airbar, refs.next);

    const watch = el('button', { class: 'btn small primary', type: 'button' }, icon('play'), 'Watch');
    watch.onclick = (e) => { e.stopPropagation(); window.Player?.openLive(ch, ch.now); };
    const rec = el('button', { class: 'btn small', type: 'button', title: 'Record this airing (or a 1-hour manual capture)' }, icon('rec', 'rec-dot'), 'Rec');
    rec.onclick = (e) => { e.stopPropagation(); busy(rec, () => recordAiring(ch, ch.now), ch.now ? `Recording “${ch.now.title}”` : 'Recording 1 hour'); };
    const series = el('button', { class: 'btn small', type: 'button', title: 'Series pass for the current programme' }, 'Series');
    series.onclick = (e) => { e.stopPropagation(); if (!ch.now) return toast('No guide data for this channel', 'err'); busy(series, () => addSeries(ch.now.title, ch.guideNumber), `Series pass: ${ch.now.title}`); };
    refs.series = series;

    const row = el('div', { class: `chan ${ch.now ? '' : 'noguide'}`, role: 'listitem', tabindex: '0', dataset: { ch: ch.guideNumber } },
      star, channelLogo(ch),
      el('div', { class: 'num' }, ch.guideNumber, el('small', {}, ch.guideName + (ch.hd ? ' · HD' : ''))),
      now,
      el('div', { class: 'acts' }, watch, rec, series));
    row.addEventListener('dblclick', () => window.Player?.openLive(ch, ch.now));
    row.addEventListener('keydown', (e) => { if (e.key === 'Enter' && e.target === row) window.Player?.openLive(ch, ch.now); });
    rows.set(ch.guideNumber, { row, ch, refs });
    updateRow(ch.guideNumber);
    return row;
  }

  function updateRow(num) {
    const r = rows.get(num);
    if (!r) return;
    const { ch, refs, row } = r;
    const a = ch.now;
    if (!a) {
      refs.nowTitle.textContent = 'No guide data';
      refs.nowTime.textContent = 'Add an XMLTV source in Settings to see what is on.';
      refs.airbar.hidden = true;
      refs.next.textContent = '';
      refs.series.disabled = true;
      row.classList.add('noguide');
      return;
    }
    row.classList.remove('noguide');
    refs.series.disabled = false;
    refs.airbar.hidden = false;
    const nt = `${a.title}${a.new ? '  NEW' : ''}`;
    if (refs.nowTitle.dataset.t !== nt) { refs.nowTitle.dataset.t = nt; refs.nowTitle.replaceChildren(); append(refs.nowTitle, [a.title, a.new ? el('span', { class: 'new' }, 'NEW') : null]); }
    const sub = [airingRange(a), seCode(a), a.subtitle].filter(Boolean).join(' · ');
    const left = minsLeft(a);
    const t = `${sub}${left ? ` · ${left} min left` : ''}`;
    if (refs.nowTime.textContent !== t) refs.nowTime.textContent = t;
    refs.bar.style.width = `${airingPct(a)}%`;
    const nx = ch.next ? `Next: ${ch.next.title} · ${fmtClock(ch.next.start)}` : '';
    if (refs.next.textContent !== nx) refs.next.textContent = nx;
  }

  for (const ch of channels) list.append(buildRow(ch));
  const favHead = el('div', { class: 'chan-group', hidden: true }, icon('star'), 'Favorites');
  const allHead = el('div', { class: 'chan-group' }, 'All channels');
  const noMatch = el('div', { class: 'muted chan-nomatch', hidden: true }, 'No channels match.');

  // Re-order / filter in place (cheap: moves existing nodes, never rebuilds).
  function layout() {
    const s = favs();
    const q = query.q.trim().toLowerCase();
    const match = (ch) => !q || ch.guideNumber.includes(q) || ch.guideName.toLowerCase().includes(q) || (ch.now?.title || '').toLowerCase().includes(q) || (ch.next?.title || '').toLowerCase().includes(q);
    const favRows = [], rest = [];
    let shown = 0;
    for (const ch of channels) {
      const r = rows.get(ch.guideNumber);
      const ok = match(ch);
      r.row.hidden = !ok;
      if (!ok) continue;
      shown++;
      (s.has(ch.guideNumber) ? favRows : rest).push(r.row);
    }
    favHead.hidden = !favRows.length;
    allHead.hidden = !favRows.length || !rest.length;
    noMatch.hidden = shown > 0;
    list.replaceChildren(favHead, ...favRows, allHead, ...rest, noMatch);
  }
  search.addEventListener('input', debounce(() => { query.q = search.value; layout(); }, 80));
  layout();

  const updated = data.guideUpdated ? el('p', { class: 'faint live-foot' }, `Guide updated ${fmtDateLong(data.guideUpdated)} ${fmtClock(data.guideUpdated)}`) : null;
  view.replaceChildren(el('div', { class: 'page live-page' }, h, list, updated));

  // ---- live ticking: progress bars each 30 s, now/next refetch each 60 s ----
  const tick = setInterval(() => { for (const num of rows.keys()) updateRow(num); }, 30000);
  const refresh = setInterval(async () => {
    try {
      const d = await api.get('/api/livetv/channels');
      for (const c of d.channels || []) {
        const r = rows.get(c.guideNumber);
        if (!r) continue;
        r.ch.now = c.now; r.ch.next = c.next;
        updateRow(c.guideNumber);
      }
    } catch { /* transient; keep the last state */ }
  }, 60000);
  return () => { clearInterval(tick); clearInterval(refresh); };
}

// =====================================================================
// Guide (EPG grid)
// =====================================================================
export async function renderGuide(params) {
  ambientFrom(null);
  params = params instanceof URLSearchParams ? params : new URLSearchParams();
  let hours = [3, 6, 12].includes(pref.get('guide.hours', 6)) ? pref.get('guide.hours', 6) : 6;
  const floorHalf = (t) => { const d = new Date(t); d.setMinutes(d.getMinutes() < 30 ? 0 : 30, 0, 0); return d; };
  let from = params.get('t') && !isNaN(new Date(params.get('t'))) ? floorHalf(new Date(params.get('t'))) : floorHalf(Date.now() - HOUR / 2);
  const focusCh = params.get('ch');

  const page = el('div', { class: 'page guide-page' });
  const head = el('div', { class: 'page-head' }, el('h1', {}, 'Guide'));
  const toolbar = el('div', { class: 'toolbar guide-tools' });
  const grid = el('div', { class: 'guide' });
  grid.style.setProperty('--hours', hours);
  const scroll = el('div', { class: 'guide-scroll' });
  grid.append(scroll);
  page.append(head, toolbar, grid);
  view.replaceChildren(page);

  // ---- toolbar ----
  const nowBtn = el('button', { class: 'btn small primary', type: 'button', title: 'Jump to now (n)' }, 'Now');
  const back = el('button', { class: 'btn small icon', type: 'button', title: 'Back 3 hours (←)', 'aria-label': 'Back 3 hours' }, icon('chevronL'));
  const fwd = el('button', { class: 'btn small icon', type: 'button', title: 'Forward 3 hours (→)', 'aria-label': 'Forward 3 hours' }, icon('chevronR'));
  const daySel = el('select', { class: 'guide-day', 'aria-label': 'Day' });
  const rangeLbl = el('span', { class: 'guide-range muted' });
  const seg = el('div', { class: 'seg', role: 'group', 'aria-label': 'Hours visible' });
  for (const hh of [3, 6, 12]) {
    const b = el('button', { type: 'button', class: hh === hours ? 'on' : '' }, `${hh}h`);
    b.onclick = () => { hours = hh; pref.set('guide.hours', hh); $$('button', seg).forEach((x) => x.classList.toggle('on', x === b)); grid.style.setProperty('--hours', hh); load(); };
    seg.append(b);
  }
  toolbar.append(nowBtn, el('div', { class: 'guide-nav' }, back, daySel, fwd), rangeLbl, el('span', { class: 'spacer' }), seg, el('span', { class: 'faint guide-kbd' }, el('kbd', {}, '←'), el('kbd', {}, '→'), ' pan  ', el('kbd', {}, 'n'), ' now'));

  function fillDays() {
    daySel.replaceChildren();
    const today = new Date(); today.setHours(0, 0, 0, 0);
    for (let i = 0; i < 7; i++) {
      const d = new Date(today.getTime() + i * 86400000);
      const label = i === 0 ? 'Today' : i === 1 ? 'Tomorrow' : fmtDateLong(d);
      daySel.append(el('option', { value: String(d.getTime()) }, label));
    }
    const fd = new Date(from); fd.setHours(0, 0, 0, 0);
    daySel.value = String(fd.getTime());
    if (daySel.value !== String(fd.getTime())) daySel.value = String(today.getTime());
  }
  daySel.onchange = () => {
    const day = new Date(+daySel.value);
    const cur = new Date(from);
    const isToday = day.toDateString() === new Date().toDateString();
    const d = isToday ? floorHalf(Date.now()) : new Date(day.getFullYear(), day.getMonth(), day.getDate(), cur.getHours(), cur.getMinutes());
    from = isToday ? d : (d < day ? day : d);
    load();
  };
  const pan = (h) => { from = new Date(from.getTime() + h * HOUR); load(); };
  back.onclick = () => pan(-3);
  fwd.onclick = () => pan(3);
  nowBtn.onclick = () => { from = floorHalf(Date.now() - HOUR / 2); load({ scrollNow: true }); };

  // ---- state ----
  let token = 0;
  let nowLine = null, nowTimer = null, popover = null;
  let lanes = []; // [{ch, lane, blocks:[{a, node}]}]
  let scheduled = new Map(); // `${channelId}|${startMs}` -> recording
  let hourPx = 360;

  let colPx = 180;
  const readHourPx = () => { const cs = getComputedStyle(grid); hourPx = parseFloat(cs.getPropertyValue('--hour')) || 360; colPx = parseFloat(cs.getPropertyValue('--col')) || 180; };
  const xOf = (t) => ((+new Date(t) - from.getTime()) / HOUR) * hourPx;

  async function loadScheduled() {
    try {
      const recs = await api.get('/api/dvr/recordings');
      const m = new Map();
      for (const r of recs) if (r.status === 'scheduled' || r.status === 'recording') m.set(`${r.channelId}|${+new Date(r.start)}`, r);
      scheduled = m;
    } catch { /* non-fatal */ }
  }
  const schedKey = (a) => `${a.channelId}|${+new Date(a.start)}`;
  function applySched() {
    for (const l of lanes) for (const b of l.blocks) b.node.classList.toggle('sched', scheduled.has(schedKey(b.a)));
  }

  function markLive() {
    const now = Date.now();
    for (const l of lanes) for (const b of l.blocks) {
      const s = +new Date(b.a.start), e = +new Date(b.a.end);
      b.node.classList.toggle('live', s <= now && now < e);
    }
  }
  function placeNow() {
    if (!nowLine) return;
    const x = xOf(Date.now());
    const inWin = x >= 0 && x <= hours * hourPx;
    nowLine.hidden = !inWin;
    if (inWin) nowLine.style.transform = `translateX(${colPx + x}px)`;
  }
  const scrollToNow = () => {
    const x = xOf(Date.now());
    scroll.scrollTo({ left: Math.max(0, x - (scroll.clientWidth - colPx) / 2), behavior: 'smooth' });
  };

  async function load(opts = {}) {
    const my = ++token;
    closePopover();
    fillDays();
    rangeLbl.textContent = `${fmtDateLong(from)} · ${fmtClock(from)} – ${fmtClock(from.getTime() + hours * HOUR)}`;
    scroll.classList.add('loading');
    const [g] = await Promise.all([
      api.get(`/api/guide?hours=${hours}&from=${encodeURIComponent(from.toISOString())}`).catch((e) => ({ error: e.message })),
      loadScheduled(),
    ]);
    if (my !== token) return;
    scroll.classList.remove('loading');
    if (g.error) { scroll.replaceChildren(emptyState('Guide unavailable', g.error, 'live')); return; }
    const channels = g.channels || [];
    if (!channels.length) {
      scroll.replaceChildren(el('div', { class: 'empty' }, icon('live'), el('div', {}, el('b', {}, 'No guide data'), 'No channels or no XMLTV source. Set hdhrIp / xmltvUrl in ', el('a', { href: '#/settings', class: 'link' }, 'Settings'), ' and refresh.')));
      return;
    }
    readHourPx();
    lanes = [];

    // ruler
    const ruler = el('div', { class: 'guide-ruler' }, el('div', { class: 'corner' }, el('span', { class: 'eyebrow' }, `${channels.length} ch`)));
    for (let i = 0; i < hours; i++) {
      const t = from.getTime() + i * HOUR;
      ruler.append(el('div', { class: 'hour' }, fmtClock(t), el('span', { class: 'half' }, fmtClock(t + HOUR / 2))));
    }
    const inner = el('div', { class: 'guide-inner' }, ruler);
    const winStart = from.getTime(), winEnd = winStart + hours * HOUR;

    for (const ch of channels) {
      const lane = el('div', { class: 'guide-lane' });
      const blocks = [];
      for (const a of ch.airings || []) {
        const s = +new Date(a.start), e = +new Date(a.end);
        if (e <= winStart || s >= winEnd) continue;
        const cs = Math.max(s, winStart), ce = Math.min(e, winEnd);
        const left = ((cs - winStart) / HOUR) * hourPx;
        const width = Math.max(4, ((ce - cs) / HOUR) * hourPx - 4);
        a.channelId = a.channelId || ch.guideNumber;
        const node = el('button', { class: 'prog-block', type: 'button', style: { left: `${left}px`, width: `${width}px` }, title: `${a.title} · ${airingRange(a)}` },
          el('b', {}, a.title, a.new ? el('span', { class: 'new' }, 'NEW') : null),
          el('small', {}, [fmtClock(a.start), seCode(a), a.subtitle].filter(Boolean).join(' · ')));
        node.onclick = (ev) => openPopover(ch, a, node, ev);
        blocks.push({ a, node });
        lane.append(node);
      }
      if (!blocks.length) lane.append(el('span', { class: 'lane-empty' }, 'No guide data'));
      const chCell = el('div', { class: 'guide-ch', role: 'button', tabindex: '0', title: `Watch ${ch.guideName}` },
        channelLogo(ch), el('div', { class: 'ch-txt' }, el('b', {}, ch.guideNumber), el('small', {}, ch.guideName)));
      const watchNow = () => {
        const now = Date.now();
        const cur = (ch.airings || []).find((a) => +new Date(a.start) <= now && now < +new Date(a.end)) || null;
        window.Player?.openLive(ch, cur);
      };
      chCell.onclick = watchNow;
      chCell.onkeydown = (e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); watchNow(); } };
      const row = el('div', { class: `guide-row ${focusCh === ch.guideNumber ? 'focus' : ''}`, dataset: { ch: ch.guideNumber } }, chCell, lane);
      inner.append(row);
      lanes.push({ ch, lane, blocks, row });
    }
    nowLine = el('div', { class: 'now-line', hidden: true });
    inner.append(nowLine);
    scroll.replaceChildren(inner);
    applySched();
    markLive();
    placeNow();
    if (opts.scrollNow || (!opts.keepScroll && !params.get('t') && Math.abs(Date.now() - from.getTime()) < 2 * HOUR)) requestAnimationFrame(scrollToNow);
    else scroll.scrollTo({ left: 0 });
    if (focusCh) {
      const r = lanes.find((l) => l.ch.guideNumber === focusCh)?.row;
      if (r) requestAnimationFrame(() => scroll.scrollTo({ top: Math.max(0, r.offsetTop - scroll.clientHeight / 2), behavior: 'smooth' }));
    }
  }

  // ---- popover ----
  function closePopover(refocus = false) {
    if (!popover) return;
    const hadFocus = popover.contains(document.activeElement);
    popover.remove(); popover = null;
    document.removeEventListener('pointerdown', onDocDown, true);
    for (const b of $$('.prog-block.sel', scroll)) {
      b.classList.remove('sel');
      if ((refocus || hadFocus) && b.isConnected) b.focus({ preventScroll: true });
    }
  }
  function onDocDown(e) { if (popover && !popover.contains(e.target) && !e.target.closest('.prog-block')) closePopover(); }
  function openPopover(ch, a, anchor) {
    closePopover();
    const now = Date.now();
    const onAir = +new Date(a.start) <= now && now < +new Date(a.end);
    const rec = scheduled.get(schedKey(a));
    const acts = el('div', { class: 'acts' });
    if (onAir) {
      const w = el('button', { class: 'btn small primary', type: 'button' }, icon('play'), 'Watch');
      w.onclick = () => { closePopover(); window.Player?.openLive(ch, a); };
      acts.append(w);
    }
    if (rec) {
      const c = el('button', { class: 'btn small danger', type: 'button' }, icon('x'), rec.status === 'recording' ? 'Stop recording' : 'Cancel recording');
      c.onclick = async () => {
        if (!(await confirm(rec.status === 'recording' ? 'Stop recording?' : 'Cancel recording?', `“${a.title}” on ${ch.guideName}`, rec.status === 'recording' ? 'Stop' : 'Cancel recording'))) return;
        await busy(c, async () => { await api.del(`/api/dvr/recordings/${encodeURIComponent(rec.id)}`); scheduled.delete(schedKey(a)); applySched(); closePopover(); }, 'Recording cancelled');
      };
      acts.append(c);
    } else if (+new Date(a.end) > now) {
      const r = el('button', { class: 'btn small', type: 'button' }, icon('rec', 'rec-dot'), 'Record');
      r.onclick = () => busy(r, async () => {
        const saved = await recordAiring(ch, a);
        scheduled.set(schedKey(a), { ...saved, channelId: a.channelId, start: a.start, status: saved.status || 'scheduled' });
        applySched(); closePopover();
      }, `Scheduled “${a.title}”`);
      acts.append(r);
    }
    const sp = el('button', { class: 'btn small', type: 'button', title: 'Record every airing of this title' }, icon('repeat'), 'Series pass');
    sp.onclick = () => busy(sp, () => addSeries(a.title, a.channelId), `Series pass: ${a.title}`).then(closePopover);
    acts.append(sp);

    popover = el('div', { class: 'popover glass', role: 'dialog', 'aria-label': a.title },
      el('div', { class: 'pop-head' },
        el('span', { class: 'when' }, `${ch.guideNumber} ${ch.guideName} · ${airingRange(a)}${onAir ? ' · On now' : ''}`),
        el('button', { class: 'icon-btn pop-x', type: 'button', 'aria-label': 'Close', onclick: closePopover }, icon('x'))),
      el('h3', {}, a.title, a.new ? el('span', { class: 'new' }, 'NEW') : null),
      (a.subtitle || seCode(a)) ? el('div', { class: 'muted pop-sub' }, [seCode(a), a.subtitle].filter(Boolean).join(' · ')) : null,
      a.description ? el('p', {}, a.description) : null,
      (a.categories || []).length ? el('div', { class: 'chips pop-cats' }, a.categories.slice(0, 4).map((c) => el('span', { class: 'chip' }, c))) : null,
      rec ? el('div', { class: 'pop-sched' }, el('span', { class: `status ${rec.status}` }, rec.status)) : null,
      acts);
    document.body.append(popover);
    // position near the block, clamped into the viewport
    const r = anchor.getBoundingClientRect();
    const pw = popover.offsetWidth, ph = popover.offsetHeight;
    const vw = window.innerWidth;
    const rail = $('.rail')?.getBoundingClientRect();
    // on phones the rail is a bottom bar: keep the popover above it
    const vh = rail && rail.top > 0 && rail.top < window.innerHeight && rail.width >= vw - 1 ? rail.top : window.innerHeight;
    let x = Math.max(12, Math.min(vw - pw - 12, r.left));
    let y = r.bottom + 8;
    if (y + ph > vh - 12) y = Math.max(12, r.top - ph - 8);
    if (y + ph > vh - 12) y = Math.max(12, vh - ph - 12);
    popover.style.left = `${x}px`;
    popover.style.top = `${y}px`;
    anchor.classList.add('sel');
    setTimeout(() => document.addEventListener('pointerdown', onDocDown, true), 0);
    popover.querySelector('.btn')?.focus({ preventScroll: true });
  }

  // ---- keyboard ----
  const onKey = (e) => {
    const typing = ['INPUT', 'TEXTAREA', 'SELECT'].includes(e.target.tagName);
    if (e.key === 'Escape' && popover) { closePopover(true); return; }
    if (typing || e.metaKey || e.ctrlKey || e.altKey || !$('#player').hidden || !$('#palette').hidden) return;
    if (e.key === 'ArrowLeft') { e.preventDefault(); pan(-1); }
    else if (e.key === 'ArrowRight') { e.preventDefault(); pan(1); }
    else if (e.key === 'n') { nowBtn.click(); }
  };
  document.addEventListener('keydown', onKey);
  nowTimer = setInterval(() => { placeNow(); markLive(); }, 30000);
  const onResize = debounce(() => { const was = hourPx; readHourPx(); if (hourPx !== was) load({ keepScroll: true }); }, 250);
  window.addEventListener('resize', onResize);

  await load();
  return () => {
    token++;
    closePopover();
    clearInterval(nowTimer);
    document.removeEventListener('keydown', onKey);
    window.removeEventListener('resize', onResize);
  };
}
