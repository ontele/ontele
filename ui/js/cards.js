/* Copyright 2026 The Ontele Authors — SPDX-License-Identifier: Apache-2.0 */
/* Cards, strips, grids and the home hero — every view composes these. */

import { el, icon, img, artUrl, itemLabel, fmtTime, fmtMins, resLabel, go, fmtDate, api, toast } from './core.js';

/// Poster/thumb/cover block with lazy image + text fallback.
export function art(key, kind = 'poster', label = '', w) {
  const wrap = el('div', { class: 'art' });
  wrap.append(el('div', { class: 'fallback' }, label));
  wrap.append(img(artUrl(key, kind, w)));
  return wrap;
}

const BADGES = {
  cut: ['ADS CUT', 'cut'], ready: ['AD SKIP', 'ads'], pending: ['AD SCAN…', 'pending'], failed: ['SCAN FAILED', 'pending'],
};

/// Tune the live player to a channel by guide number (used for recordings still being captured:
/// the server only serves finished recordings, so "watch" means the live feed).
export async function openLiveChannel(channelId) {
  let ch = null;
  try { const d = await api.get('/api/livetv/channels'); ch = (d.channels || []).find((c) => c.guideNumber === channelId) || null; } catch {}
  if (!ch) return toast('That channel is not in the tuner lineup', 'err');
  return window.Player?.openLive(ch, ch.now);
}

/// Open whatever the item is: video → player, track → music dock, in-progress recording → live channel.
export function openItem(it, opts = {}) {
  if (it.kind === 'track') return window.Music?.playTrack(it, opts);
  if (it.kind === 'recording' && it.status === 'recording' && it.channelId) return openLiveChannel(it.channelId);
  return window.Player?.open(it, opts);
}

/// A card. `shape`: poster (2:3) | wide (16:9) | square (1:1)
export function card(it, opts = {}) {
  const shape = opts.shape || (it.kind === 'episode' || it.kind === 'recording' ? 'wide' : it.kind === 'track' ? 'square' : 'poster');
  const { title, sub } = itemLabel(it);
  const artKind = shape === 'wide' ? 'thumb' : 'poster';
  const a = art(it.id, artKind, title, shape === 'wide' ? 640 : 480);
  a.append(el('div', { class: 'play-hint' }, el('i', {}, icon('play'))));
  if (it.watch && !it.watch.done && it.watch.dur > 0) {
    const p = el('div', { class: 'prog' }, el('i', { style: { width: `${Math.min(100, it.watch.pos / it.watch.dur * 100)}%` } }));
    a.append(p);
  }
  if (it.watch?.done) a.append(el('span', { class: 'badge r done' }, icon('check')));
  if (it.kind === 'recording') {
    const b = BADGES[it.breaksState];
    if (it.status === 'recording') a.append(el('span', { class: 'badge rec' }, '● REC'));
    else if (b) a.append(el('span', { class: 'badge ' + b[1] }, b[0]));
  } else if (it.height >= 2000 || it.hdr) {
    a.append(el('span', { class: 'badge' }, it.height >= 2000 ? '4K' : (it.hdr === 'dv' ? 'DV' : 'HDR')));
  }
  if (shape === 'wide' && it.duration) a.append(el('span', { class: 'dur' }, fmtTime(it.duration)));
  if (shape === 'wide' && it.kind === 'episode') a.append(el('span', { class: 'ep-num' }, `S${it.season}·E${it.episode}`));
  const onclick = opts.onclick || ((e) => {
    if (e.altKey || opts.detailOnClick) return go(`#/item/${it.id}`);
    openItem(it);
  });
  const c = el('button', { class: `card ${shape}`, type: 'button', onclick, title: opts.detailOnClick ? 'Open' : 'Play (Alt+click for details)' },
    a, el('div', { class: 'meta' }, el('b', {}, title), el('small', {}, sub)));
  c.addEventListener('contextmenu', (e) => { e.preventDefault(); go(`#/item/${it.id}`); });
  return c;
}

export function showCard(s) {
  const a = art(`show:${s.show}`, 'poster', s.show, 480);
  a.append(el('div', { class: 'play-hint' }, el('i', {}, icon('chevronR'))));
  if (s.episodes && s.watched >= s.episodes) a.append(el('span', { class: 'badge r done' }, icon('check')));
  else if (s.watched > 0) a.append(el('div', { class: 'prog' }, el('i', { style: { width: `${s.watched / s.episodes * 100}%` } })));
  return el('button', { class: 'card poster', type: 'button', onclick: () => go(`#/show/${encodeURIComponent(s.show)}`) },
    a, el('div', { class: 'meta' }, el('b', {}, s.show), el('small', {}, `${s.seasons} season${s.seasons > 1 ? 's' : ''} · ${s.episodes} ep${s.year ? ' · ' + s.year : ''}`)));
}

export function albumCard(al) {
  const a = art(`album:${al.id}`, 'poster', al.title, 480);
  a.append(el('div', { class: 'play-hint' }, el('i', {}, icon('play'))));
  const c = el('button', { class: 'card square', type: 'button', onclick: () => go(`#/album/${al.id}`) },
    a, el('div', { class: 'meta' }, el('b', {}, al.title), el('small', {}, `${al.artist}${al.year ? ' · ' + al.year : ''}`)));
  a.querySelector('.play-hint').addEventListener('click', (e) => { e.stopPropagation(); window.Music?.playAlbum(al.id); });
  return c;
}

export function artistCard(ar) {
  const a = art(ar.artId, 'poster', ar.name, 480);
  return el('button', { class: 'card square', type: 'button', onclick: () => go(`#/artist/${encodeURIComponent(ar.name)}`) },
    a, el('div', { class: 'meta' }, el('b', {}, ar.name), el('small', {}, `${ar.albums} album${ar.albums === 1 ? '' : 's'} · ${ar.tracks} tracks`)));
}

/// Horizontal strip with hover arrows. `kids` are already-built cards.
export function strip(title, kids, opts = {}) {
  if (!kids || !kids.length) return null;
  const s = el('div', { class: `row-strip ${opts.shape || ''}` }, kids);
  const scrollBy = (dir) => s.scrollBy({ left: dir * s.clientWidth * 0.8, behavior: 'smooth' });
  return el('section', { class: 'row' },
    el('div', { class: 'row-head' }, el('h2', {}, title), opts.more ? el('a', { href: opts.more }, 'See all →') : null),
    el('button', { class: 'row-arrow l', 'aria-label': 'Scroll left', onclick: () => scrollBy(-1) }, icon('chevronL')),
    el('button', { class: 'row-arrow r', 'aria-label': 'Scroll right', onclick: () => scrollBy(1) }, icon('chevronR')),
    s);
}

export function metaChips(it) {
  const chips = [];
  const m = it.meta || {};
  if (it.year || m.releaseDate) chips.push(el('span', { class: 'chip' }, String(it.year || m.releaseDate.slice(0, 4))));
  if (m.contentRating) chips.push(el('span', { class: 'chip' }, m.contentRating));
  if (it.duration) chips.push(el('span', { class: 'chip' }, fmtMins(it.duration)));
  if (m.rating) chips.push(el('span', { class: 'chip rating' }, icon('star'), ` ${m.rating.toFixed(1)}`));
  if (it.height) chips.push(el('span', { class: 'chip' }, resLabel(it.height)));
  if (it.hdr) chips.push(el('span', { class: 'chip' }, it.hdr === 'dv' ? 'Dolby Vision' : it.hdr === 'hlg' ? 'HLG' : it.hdr === 'hdr10plus' ? 'HDR10+' : 'HDR10'));
  if (it.breaksState === 'cut') chips.push(el('span', { class: 'chip hot' }, 'Ads removed'));
  if (it.breaksState === 'ready') chips.push(el('span', { class: 'chip hot' }, 'Ad-skip ready'));
  for (const g of (m.genres || []).slice(0, 3)) chips.push(el('span', { class: 'chip' }, g));
  return chips;
}

/// Home hero for an item.
export function hero(it, opts = {}) {
  const { title, sub } = itemLabel(it);
  const bgKey = it.kind === 'episode' && it.show ? `show:${it.show}` : it.id;
  const bg = artUrl(bgKey, 'backdrop', 1280);
  const resume = it.watch && !it.watch.done && it.watch.pos > 60;
  const m = it.meta || {};
  return el('div', { class: 'hero' },
    el('div', { class: 'bg-bleed', style: { backgroundImage: `url('${bg}')` } }),
    el('div', { class: 'bg', style: { backgroundImage: `url('${bg}')` } }),
    el('div', { class: 'scrim' }),
    el('div', { class: 'hero-body' },
      el('span', { class: 'eyebrow' }, opts.eyebrow || (resume ? 'Continue watching' : it.kind === 'recording' ? 'Recorded off-air' : 'In your library')),
      el('h1', {}, title),
      el('div', { class: 'hero-meta' }, sub && it.kind !== 'movie' ? el('span', { class: 'chip' }, sub) : null, metaChips(it)),
      m.overview ? el('p', { class: 'hero-over' }, m.overview) : null,
      el('div', { class: 'hero-actions' },
        el('button', { class: 'btn primary', onclick: () => openItem(it) }, icon('play'), resume ? `Resume · ${fmtTime(it.watch.pos)}` : 'Play'),
        resume ? el('button', { class: 'btn', onclick: () => openItem(it, { from: 0 }) }, 'Start over') : null,
        el('button', { class: 'btn', onclick: () => go(`#/item/${it.id}`) }, icon('info'), 'Details'))));
}

export const recLabel = (r) => `${fmtDate(r.start)} ${r.start ? new Date(r.start).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' }) : ''}`;
