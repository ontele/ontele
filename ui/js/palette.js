/* Copyright 2026 The Ontele Authors — SPDX-License-Identifier: Apache-2.0 */
/* Command palette (⌘K / "/"): search across everything, keyboard-first. */

import { $, el, api, debounce, artUrl, img, itemLabel, epCode, fmtClock, fmtDate, go, icon } from './core.js';
import { openItem } from './cards.js';

const root = $('#palette'), input = $('#palette-q'), results = $('#palette-results');
let items = [];   // flat list of {el, run}
let sel = 0;

export function openPalette(q = '') {
  root.hidden = false;
  input.value = q;
  input.focus();
  input.select();
  render(q ? null : quickLinks());
  if (q) search(q);
}
export function closePalette() {
  root.hidden = true;
  input.blur();
}

function quickLinks() {
  const links = [
    ['Home', '#/home', 'film'], ['Movies', '#/movies', 'film'], ['Shows', '#/shows', 'tv'], ['Music', '#/music', 'music'],
    ['Live TV', '#/live', 'live'], ['Guide', '#/guide', 'live'], ['DVR', '#/dvr', 'dvr'], ['Settings', '#/settings', 'info'], ['Activity', '#/activity', 'info'],
  ];
  const frag = document.createDocumentFragment();
  frag.append(el('h4', {}, 'Jump to'));
  for (const [label, hash, ic] of links) frag.append(row({ title: label, sub: hash, icon: ic, run: () => go(hash) }));
  return frag;
}

function row({ title, sub, thumb, thumbClass = '', icon: ic, run }) {
  const th = el('div', { class: `th ${thumbClass}` }, thumb ? img(thumb) : icon(ic || 'info'));
  const r = el('button', { class: 'pr', type: 'button', role: 'option' }, th, el('div', { class: 't' }, el('b', {}, title), el('small', {}, sub || '')), el('kbd', {}, '↵'));
  r.onclick = () => { closePalette(); run(); };
  r.onmousemove = () => select(items.findIndex((i) => i.el === r));
  items.push({ el: r, run });
  return r;
}

function render(content) {
  items = [];
  sel = 0;
  results.replaceChildren();
  if (content) results.append(content);
  select(0);
}
function select(i) {
  if (!items.length) return;
  sel = Math.max(0, Math.min(items.length - 1, i));
  items.forEach((it, idx) => it.el.classList.toggle('on', idx === sel));
  items[sel].el.scrollIntoView({ block: 'nearest' });
}

const search = debounce(async (q) => {
  if (!q.trim()) return render(quickLinks());
  let r;
  try { r = await api.get(`/api/search?q=${encodeURIComponent(q)}`); } catch { return; }
  if (input.value.trim() !== q.trim()) return;
  const frag = document.createDocumentFragment();
  const group = (title, list, mk) => {
    if (!list || !list.length) return;
    frag.append(el('h4', {}, title));
    list.forEach((x) => frag.append(mk(x)));
  };
  group('Movies', r.movies, (it) => row({ title: it.title, sub: [it.year, it.meta?.genres?.slice(0, 2).join(', ')].filter(Boolean).join(' · '), thumb: artUrl(it.id, 'poster', 120), run: () => go(`#/item/${it.id}`) }));
  group('Shows', r.shows, (s) => row({ title: s.show, sub: `${s.seasons} seasons · ${s.episodes} episodes`, thumb: artUrl(`show:${s.show}`, 'poster', 120), run: () => go(`#/show/${encodeURIComponent(s.show)}`) }));
  group('Episodes', r.episodes, (it) => row({ title: `${it.show} · ${epCode(it)}`, sub: it.title, thumb: artUrl(it.id, 'thumb', 160), thumbClass: 'wide', run: () => openItem(it) }));
  group('Albums', r.albums, (a) => row({ title: a.title, sub: `${a.artist}${a.year ? ' · ' + a.year : ''}`, thumb: artUrl(`album:${a.id}`, 'poster', 120), thumbClass: 'sq', run: () => go(`#/album/${a.id}`) }));
  group('Artists', r.artists, (a) => row({ title: a.name, sub: `${a.albums} albums`, thumb: artUrl(a.artId, 'poster', 120), thumbClass: 'sq', run: () => go(`#/artist/${encodeURIComponent(a.name)}`) }));
  group('Tracks', r.tracks, (it) => row({ title: it.title, sub: itemLabel(it).sub, thumb: artUrl(it.id, 'poster', 120), thumbClass: 'sq', run: () => openItem(it) }));
  group('Recordings', r.recordings, (it) => row({ title: it.title, sub: it.subtitle || fmtDate(it.start), thumb: artUrl(it.id, 'thumb', 160), thumbClass: 'wide', run: () => openItem(it) }));
  group('Channels', r.channels, (c) => row({ title: `${c.guideNumber} · ${c.guideName}`, sub: c.now?.title || 'Live TV', icon: 'live', run: () => window.Player?.openLive(c, c.now) }));
  group('On air soon', r.airings, (a) => row({ title: a.title, sub: `${fmtDate(a.start)} ${fmtClock(a.start)} · ch ${a.channelId}${a.subtitle ? ' · ' + a.subtitle : ''}`, icon: 'dvr', run: () => go(`#/guide?t=${encodeURIComponent(a.start)}&ch=${encodeURIComponent(a.channelId)}`) }));
  if (!frag.childNodes.length) frag.append(el('div', { class: 'palette-empty' }, `Nothing matched “${q}”`));
  render(frag);
}, 180);

input.addEventListener('input', () => search(input.value));
input.addEventListener('keydown', (e) => {
  if (e.key === 'ArrowDown') { e.preventDefault(); select(sel + 1); }
  else if (e.key === 'ArrowUp') { e.preventDefault(); select(sel - 1); }
  else if (e.key === 'Enter') { e.preventDefault(); items[sel]?.el.click(); }
  else if (e.key === 'Escape') { e.preventDefault(); closePalette(); }
});
root.addEventListener('click', (e) => { if (e.target === root) closePalette(); });
