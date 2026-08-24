/* Copyright 2026 The Ontele Authors — SPDX-License-Identifier: Apache-2.0 */
/* DVR: schedule, series passes, recordings with commercial tooling. */

import { el, api, icon, confirm, fmtClock, fmtDate, fmtDateLong, fmtTime, busy, emptyState, view, ambientFrom } from '../core.js';
import { card, openLiveChannel } from '../cards.js';

const FILTERS = [['all', 'All'], ['cut', 'Ads cut'], ['ready', 'Skip ready'], ['pending', 'Pending']];

function relTime(iso) {
  const d = (+new Date(iso) - Date.now()) / 1000;
  const abs = Math.abs(d);
  const unit = abs < 3600 ? `${Math.max(1, Math.round(abs / 60))}m` : abs < 86400 ? `${Math.round(abs / 3600)}h` : `${Math.round(abs / 86400)}d`;
  return d >= 0 ? `in ${unit}` : `${unit} ago`;
}
const whenLabel = (r) => {
  const d = new Date(r.start);
  const today = new Date(); const tmr = new Date(today.getTime() + 86400000);
  const day = d.toDateString() === today.toDateString() ? 'Today' : d.toDateString() === tmr.toDateString() ? 'Tomorrow' : fmtDateLong(d);
  return `${day} · ${fmtClock(r.start)} – ${fmtClock(r.end)}`;
};

export async function render() {
  ambientFrom(null);
  const page = el('div', { class: 'page dvr-page' });
  page.append(el('div', { class: 'page-head' }, el('h1', {}, 'DVR')),
    el('div', { class: 'stat-tiles' }, Array.from({ length: 4 }, () => el('div', { class: 'tile' }, el('div', { class: 'skel-line', style: { width: '40%', height: '26px' } }), el('div', { class: 'skel-line', style: { width: '60%' } })))));
  view.replaceChildren(page);

  let recs = [], rules = [], channels = [];
  let filter = 'all';
  const load = async () => {
    const [r, ru, ch] = await Promise.all([
      api.get('/api/dvr/recordings'),
      api.get('/api/dvr/rules').catch(() => []),
      api.get('/api/livetv/channels').then((d) => d.channels || []).catch(() => []),
    ]);
    recs = r; rules = ru; channels = ch;
  };
  try { await load(); } catch (e) {
    view.replaceChildren(el('div', { class: 'page' }, el('div', { class: 'page-head' }, el('h1', {}, 'DVR')), emptyState('DVR unavailable', e.message, 'dvr')));
    return;
  }

  // ---- section containers (re-rendered independently) ----
  const tiles = el('div', { class: 'stat-tiles' });
  const upNextSec = el('section', { class: 'section' });
  const rulesSec = el('section', { class: 'section' });
  const recsSec = el('section', { class: 'section' });
  const failedSec = el('section', { class: 'section' });
  const head = el('div', { class: 'page-head' }, el('h1', {}, 'DVR'), el('span', { class: 'spacer' }),
    el('a', { class: 'btn small', href: '#/guide' }, icon('dvr'), 'Open guide'),
    el('a', { class: 'btn small', href: '#/live' }, icon('live'), 'Live TV'));
  view.replaceChildren(el('div', { class: 'page dvr-page' }, head, tiles, upNextSec, rulesSec, recsSec, failedSec));

  const chanName = (id) => channels.find((c) => c.guideNumber === id)?.guideName || '';

  function renderTiles() {
    const n = (st) => recs.filter((r) => r.status === st).length;
    const tile = (v, label, cls = '') => el('div', { class: `tile ${cls}` }, el('b', {}, String(v)), el('small', {}, label));
    tiles.replaceChildren(
      tile(n('scheduled'), 'Scheduled'),
      tile(n('recording'), 'Recording', n('recording') ? 'hot' : ''),
      tile(n('done'), 'Done'),
      tile(n('failed'), 'Failed', n('failed') ? 'bad' : ''),
      tile(recs.filter((r) => r.breaksState === 'cut' || r.breaksState === 'ready').length, 'Ad-free'));
  }

  async function cancelRec(r, btn) {
    const live = r.status === 'recording';
    if (!(await confirm(live ? 'Stop recording?' : 'Cancel recording?', `“${r.title}”${r.subtitle ? ' · ' + r.subtitle : ''} on ${r.channel || r.channelId}`, live ? 'Stop' : 'Cancel recording'))) return;
    await busy(btn, async () => { await api.del(`/api/dvr/recordings/${encodeURIComponent(r.id)}`); recs = recs.filter((x) => x.id !== r.id); renderAll(); }, live ? 'Recording stopped' : 'Recording cancelled');
  }

  const pctOf = (r) => Math.max(0, Math.min(100, ((Date.now() - +new Date(r.start)) / (+new Date(r.end) - +new Date(r.start))) * 100));
  const upRefs = new Map(); // id -> { when, bar }
  /// Cheap tick: relative times + progress bars, no DOM rebuild.
  function tickUpNext() {
    for (const r of recs) {
      const ref = upRefs.get(r.id);
      if (!ref) continue;
      if (r.status !== 'recording') { const t = relTime(r.start); if (ref.when.textContent !== t) ref.when.textContent = t; }
      if (ref.bar) ref.bar.style.width = `${pctOf(r)}%`;
    }
  }
  function renderUpNext() {
    const up = recs.filter((r) => r.status === 'scheduled' || r.status === 'recording').sort((a, b) => new Date(a.start) - new Date(b.start));
    const list = el('div', { class: 'up-list' });
    upRefs.clear();
    for (const r of up) {
      const live = r.status === 'recording';
      const stop = el('button', { class: 'btn small danger', type: 'button' }, icon('x'), live ? 'Stop' : 'Cancel');
      stop.onclick = () => cancelRec(r, stop);
      // the server refuses to stream a recording until it is finished, so "watch" tunes the live channel
      const watch = live && r.channelId ? el('button', { class: 'btn small primary', type: 'button', onclick: () => openLiveChannel(r.channelId) }, icon('play'), 'Watch live') : null;
      const when = el('b', {}, live ? 'NOW' : relTime(r.start));
      const bar = live ? el('i', { style: { width: `${pctOf(r)}%` } }) : null;
      upRefs.set(r.id, { when, bar });
      list.append(el('div', { class: `list-item rec-item ${live ? 'live' : ''}` },
        el('div', { class: 'rec-when' }, when, el('small', {}, fmtClock(r.start))),
        el('div', { class: 'grow' },
          el('b', {}, r.title), r.subtitle ? el('span', { class: 'muted' }, ` · ${r.subtitle}`) : null,
          el('small', {}, `${whenLabel(r)} · ${r.channelId || ''} ${r.channel || chanName(r.channelId)}`),
          bar ? el('div', { class: 'airbar' }, bar) : null),
        el('div', { class: 'rec-acts' }, el('span', { class: `status ${r.status}` }, live ? '● Recording' : r.status), watch, stop)));
    }
    upNextSec.replaceChildren(
      el('h2', {}, 'Up next', el('small', {}, `${up.length}`)),
      up.length ? list : el('p', { class: 'muted' }, 'Nothing scheduled. Pick something from the ', el('a', { href: '#/guide', class: 'link' }, 'guide'), ' or add a series pass below.'));
  }

  function renderRules() {
    const list = el('div', { class: 'rules-list' });
    for (const ru of rules) {
      const del = el('button', { class: 'btn small icon danger', type: 'button', title: 'Delete series pass', 'aria-label': `Delete pass ${ru.title}` }, icon('trash'));
      del.onclick = async () => {
        if (!(await confirm('Delete series pass?', `“${ru.title}” will no longer schedule new recordings. Existing recordings are kept.`))) return;
        await busy(del, async () => { await api.del(`/api/dvr/rules/${encodeURIComponent(ru.id)}`); rules = rules.filter((x) => x.id !== ru.id); renderRules(); }, 'Series pass removed');
      };
      const n = recs.filter((r) => r.ruleId === ru.id || r.title === ru.title).length;
      list.append(el('div', { class: 'list-item rule-item' },
        el('span', { class: 'rule-ic' }, icon('repeat')),
        el('div', { class: 'grow' },
          el('b', {}, ru.title),
          el('small', {}, [ru.channelId ? `${ru.channelId} ${chanName(ru.channelId)}`.trim() : 'Any channel', ru.keep ? `keep ${ru.keep}` : 'keep all', n ? `${n} recording${n === 1 ? '' : 's'}` : null, `since ${fmtDate(ru.created)}`].filter(Boolean).join(' · '))),
        del));
    }
    // inline add form
    const title = el('input', { type: 'text', placeholder: 'Show title (matches guide title)', required: true, 'aria-label': 'Title' });
    const chSel = el('select', { 'aria-label': 'Channel' }, el('option', { value: '' }, 'Any channel'), channels.map((c) => el('option', { value: c.guideNumber }, `${c.guideNumber} ${c.guideName}`)));
    const keep = el('input', { type: 'number', min: '0', max: '99', value: '0', 'aria-label': 'Keep', title: 'Keep at most N recordings (0 = all)' });
    const add = el('button', { class: 'btn small primary', type: 'submit' }, icon('plus'), 'Add pass');
    const form = el('form', { class: 'rule-form' },
      el('div', { class: 'field grow' }, el('label', {}, 'Title'), title),
      el('div', { class: 'field' }, el('label', {}, 'Channel'), chSel),
      el('div', { class: 'field keep' }, el('label', {}, 'Keep'), keep),
      add);
    form.onsubmit = (e) => {
      e.preventDefault();
      const t = title.value.trim();
      if (!t) return title.focus();
      busy(add, async () => {
        const r = await api.post('/api/dvr/rules', { title: t, channelId: chSel.value || null, keep: +keep.value || 0 });
        rules = [r, ...rules]; renderRules();
      }, `Series pass: ${t}`);
    };
    rulesSec.replaceChildren(
      el('h2', {}, 'Series passes', el('small', {}, `${rules.length}`)),
      rules.length ? list : el('p', { class: 'muted' }, 'No series passes yet. Add one to record every airing of a title.'),
      form);
  }

  function recMenu(r) {
    const rescan = el('button', { class: 'btn tiny', type: 'button', title: 'Re-detect commercials' }, icon('refresh'), 'Rescan ads');
    rescan.onclick = () => busy(rescan, async () => { const u = await api.post(`/api/dvr/recordings/${encodeURIComponent(r.id)}/adscan`); merge(u, r); renderRecs(); }, 'Ad scan queued');
    const cut = el('button', { class: 'btn tiny', type: 'button', title: 'Permanently remove detected ad breaks from the file' }, icon('scissors'), 'Cut ads');
    cut.onclick = async () => {
      if (!(await confirm('Cut commercials?', `This rewrites “${r.title}” without the ${(r.breaks || []).length || 'detected'} ad breaks. It cannot be undone.`, 'Cut ads'))) return;
      await busy(cut, async () => { const u = await api.post(`/api/dvr/recordings/${encodeURIComponent(r.id)}/adscan?cut=1`); merge(u, r); renderRecs(); }, 'Cutting commercials…');
    };
    const del = el('button', { class: 'btn tiny danger', type: 'button' }, icon('trash'), 'Delete');
    del.onclick = async () => {
      if (!(await confirm('Delete recording?', `“${r.title}” and its file will be removed.`))) return;
      await busy(del, async () => { await api.del(`/api/dvr/recordings/${encodeURIComponent(r.id)}`); recs = recs.filter((x) => x.id !== r.id); renderAll(); }, 'Recording deleted');
    };
    const info = el('a', { class: 'btn tiny', href: `#/item/${r.id}`, title: 'Details' }, icon('info'));
    return el('div', { class: 'rec-menu' }, rescan, r.breaksState === 'ready' && r.breaks?.length ? cut : null, info, del);
  }
  const merge = (u, r) => { if (u && u.id === r.id) Object.assign(r, u); };

  function renderRecs() {
    const done = recs.filter((r) => r.status === 'done');
    const shown = done.filter((r) => filter === 'all' || r.breaksState === filter).sort((a, b) => new Date(b.start) - new Date(a.start));
    const segBar = el('div', { class: 'seg', role: 'group', 'aria-label': 'Filter recordings' },
      FILTERS.map(([k, label]) => {
        const n = k === 'all' ? done.length : done.filter((r) => r.breaksState === k).length;
        const b = el('button', { type: 'button', class: filter === k ? 'on' : '' }, label, n ? el('span', { class: 'seg-n' }, String(n)) : null);
        b.onclick = () => { filter = k; renderRecs(); };
        return b;
      }));
    const grid = el('div', { class: 'grid wide rec-grid' });
    for (const r of shown) {
      const c = card(r);
      const wrap = el('div', { class: 'rec-card' }, c,
        el('div', { class: 'rec-sub' }, el('small', {}, [r.subtitle ? `${fmtDate(r.start)} · ${r.channel || r.channelId || ''}` : null, r.duration ? fmtTime(r.duration) : null, r.breaks?.length ? `${r.breaks.length} breaks` : null].filter(Boolean).join(' · '))),
        recMenu(r));
      grid.append(wrap);
    }
    recsSec.replaceChildren(
      el('div', { class: 'sec-head' }, el('h2', {}, 'Recordings', el('small', {}, `${done.length}`)), segBar),
      shown.length ? grid : el('p', { class: 'muted' }, done.length ? 'No recordings match this filter.' : 'No finished recordings yet.'));
  }

  function renderFailed() {
    const failed = recs.filter((r) => r.status === 'failed');
    if (!failed.length) { failedSec.replaceChildren(); failedSec.hidden = true; return; }
    failedSec.hidden = false;
    const clearAll = el('button', { class: 'btn small', type: 'button' }, icon('x'), 'Clear all');
    clearAll.onclick = async () => {
      if (!(await confirm('Clear failed recordings?', `${failed.length} failed entr${failed.length === 1 ? 'y' : 'ies'} will be removed.`, 'Clear'))) return;
      await busy(clearAll, async () => { await Promise.all(failed.map((r) => api.del(`/api/dvr/recordings/${encodeURIComponent(r.id)}`))); recs = recs.filter((r) => r.status !== 'failed'); renderAll(); }, 'Cleared');
    };
    const list = el('div', {});
    for (const r of failed) {
      const clear = el('button', { class: 'btn small icon', type: 'button', title: 'Clear', 'aria-label': `Clear ${r.title}` }, icon('x'));
      clear.onclick = () => busy(clear, async () => { await api.del(`/api/dvr/recordings/${encodeURIComponent(r.id)}`); recs = recs.filter((x) => x.id !== r.id); renderAll(); });
      list.append(el('div', { class: 'list-item failed-item' },
        el('span', { class: 'status failed' }, 'failed'),
        el('div', { class: 'grow' }, el('b', {}, r.title), r.subtitle ? el('span', { class: 'muted' }, ` · ${r.subtitle}`) : null,
          el('small', {}, `${fmtDateLong(r.start)} ${fmtClock(r.start)} · ${r.channel || r.channelId || ''}`),
          r.error ? el('code', { class: 'err-text' }, r.error) : null),
        clear));
    }
    failedSec.replaceChildren(el('div', { class: 'sec-head' }, el('h2', {}, 'Failed', el('small', {}, `${failed.length}`)), clearAll), list);
  }

  function renderAll() { renderTiles(); renderUpNext(); renderRules(); renderRecs(); renderFailed(); }
  renderAll();

  // ---- poll while something is in flight ----
  let timer = null;
  const active = () => recs.some((r) => r.status === 'recording' || r.status === 'pending' || r.breaksState === 'pending');
  const schedule = () => { clearTimeout(timer); if (active()) timer = setTimeout(poll, 15000); };
  async function poll() {
    try {
      const fresh = await api.get('/api/dvr/recordings');
      const before = JSON.stringify(recs.map((r) => [r.id, r.status, r.breaksState]));
      const after = JSON.stringify(fresh.map((r) => [r.id, r.status, r.breaksState]));
      if (before !== after) { recs = fresh; renderTiles(); renderUpNext(); renderRecs(); renderFailed(); }
      else tickUpNext(); // progress bars + relative times only
    } catch { /* retry next tick */ }
    schedule();
  }
  schedule();
  const ticker = setInterval(tickUpNext, 30000);
  return () => { clearTimeout(timer); clearInterval(ticker); };
}
