/* Copyright 2026 The Ontele Authors — SPDX-License-Identifier: Apache-2.0 */
/* Music views: library (albums / artists / tracks), artist page, album page. */

import { el, api, icon, img, artUrl, fmtTime, fmtMins, pref, view, debounce, skeletons, emptyState, toast, ambientFrom, initials } from '../core.js';
import { albumCard, artistCard, strip } from '../cards.js';

const TABS = [['albums', 'Albums'], ['artists', 'Artists'], ['tracks', 'Tracks']];
const SORTS = [['title', 'Title'], ['artist', 'Artist'], ['year', 'Year'], ['added', 'Added']];

const seg = (opts, cur, onPick) => el('div', { class: 'seg', role: 'tablist' },
  opts.map(([k, label]) => el('button', { type: 'button', role: 'tab', class: k === cur ? 'on' : '', 'aria-selected': String(k === cur), onclick: () => onPick(k) }, label)));

function sortAlbums(list, by) {
  const c = (a, b) => String(a || '').localeCompare(String(b || ''), undefined, { sensitivity: 'base', numeric: true });
  const arr = list.slice();
  if (by === 'artist') arr.sort((a, b) => c(a.artist, b.artist) || (a.year || 0) - (b.year || 0) || c(a.title, b.title));
  else if (by === 'year') arr.sort((a, b) => (b.year || 0) - (a.year || 0) || c(a.title, b.title));
  else if (by === 'added') arr.sort((a, b) => String(b.added || '').localeCompare(String(a.added || '')));
  else arr.sort((a, b) => c(a.title, b.title));
  return arr;
}

// ---------- track rows (shared by album / artist / tracks tab) ----------
/// Build a .tracks list. `opts.numbers` shows trackNo instead of list position; `opts.albumArtist` hides matching artist.
function trackList(tracks, opts = {}) {
  const rows = tracks.map((t, i) => {
    const n = opts.numbers ? (t.trackNo || i + 1) : i + 1;
    const sub = [];
    if (t.artist && t.artist !== opts.albumArtist) sub.push(t.artist);
    if (opts.showAlbum && t.album) sub.push(t.album);
    const play = () => window.Music?.playTrack(t, { queue: tracks, index: i });
    return el('div', { class: 'track', role: 'button', tabindex: '0', dataset: { id: t.id }, title: 'Play',
      onclick: play, onkeydown: (e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); play(); } } },
      el('span', { class: 'n' }, el('span', { class: 'num' }, String(n)), el('span', { class: 'pl' }, icon('play')), el('span', { class: 'eq' }, el('i'), el('i'), el('i'))),
      el('div', { class: 't' }, el('b', {}, t.title), sub.length ? el('small', {}, sub.join(' · ')) : null),
      el('span', { class: 'd' }, fmtTime(t.duration || 0)),
      el('button', { class: 'icon-btn more', type: 'button', title: 'Add to queue', 'aria-label': 'Add to queue', onclick: (e) => { e.stopPropagation(); window.Music?.enqueue([t]); } }, icon('plus')));
  });
  const list = el('div', { class: 'tracks' }, rows);
  markPlaying(list);
  return list;
}
/// Highlight the playing track in any .tracks list on the page.
function markPlaying(list, detail) {
  const d = detail || lastMusic;
  for (const row of list.querySelectorAll('.track')) {
    const on = !!d && d.id === row.dataset.id;
    row.classList.toggle('playing', on);
    row.classList.toggle('paused', on && !d.playing);
  }
}
let lastMusic = null;
document.addEventListener('ontele:music', (e) => {
  lastMusic = e.detail;
  for (const l of view.querySelectorAll('.tracks')) markPlaying(l, e.detail);
});
const headActions = (onPlay, onShuffle, extra) => el('div', { class: 'actions' },
  el('button', { class: 'btn primary', type: 'button', onclick: onPlay }, icon('play'), 'Play'),
  el('button', { class: 'btn', type: 'button', onclick: onShuffle }, icon('shuffle'), 'Shuffle'),
  extra);

// ---------- /music ----------
export async function renderMusic(params) {
  if (!(params instanceof URLSearchParams)) params = new URLSearchParams();
  ambientFrom(null);
  let tab = pref.get('musicTab', 'albums');
  if (!TABS.some(([k]) => k === tab)) tab = 'albums';
  const body = el('div', { class: 'music-body' });
  const head = el('div', { class: 'page-head' }, el('h1', {}, 'Music'), el('span', { class: 'count' }), el('span', { class: 'spacer' }));
  const tabsEl = el('div', { class: 'toolbar' });
  const recent = el('div');
  view.replaceChildren(el('div', { class: 'page music' }, head, recent, tabsEl, body));
  body.append(el('div', { class: 'grid square' }, [...skeletons(12, 'square').children]));

  // Recently added strip (from /api/home albums), full-bleed inside the page.
  api.get('/api/home').then((h) => {
    const albums = (h.albums || []).slice(0, 20);
    const s = strip('Recently added', albums.map(albumCard), { shape: 'square' });
    if (s) { s.classList.add('bleed'); recent.replaceChildren(s); }
  }).catch(() => {});

  const paintTabs = () => tabsEl.replaceChildren(seg(TABS, tab, (k) => { tab = k; pref.set('musicTab', k); paintTabs(); load(); }));
  paintTabs();

  let token = 0;
  async function load() {
    const my = ++token;
    head.querySelector('.count').textContent = '';
    if (tab === 'albums') {
      body.replaceChildren(el('div', { class: 'grid square' }, [...skeletons(12, 'square').children]));
      let sort = pref.get('musicSort', 'title');
      let albums;
      try { albums = await api.get(`/api/music/albums?sort=${encodeURIComponent(sort)}`); } catch (e) { if (my === token) body.replaceChildren(emptyState('Could not load albums', e.message, 'music')); return; }
      if (my !== token) return;
      head.querySelector('.count').textContent = `${albums.length} album${albums.length === 1 ? '' : 's'}`;
      const grid = el('div', { class: 'grid square' });
      const paint = () => grid.replaceChildren(...sortAlbums(albums, sort).map(albumCard));
      const lbl = el('span', { class: 'faint lbl' }, 'Sort');
      const bar = el('div', { class: 'toolbar sub' }, lbl);
      const sortSeg = () => seg(SORTS, sort, (k) => { sort = k; pref.set('musicSort', k); bar.replaceChildren(lbl, sortSeg()); paint(); });
      bar.append(sortSeg());
      paint();
      body.replaceChildren(bar, albums.length ? grid : emptyState('No albums yet', 'Add a music folder in Settings and scan your library.', 'music'));
    } else if (tab === 'artists') {
      body.replaceChildren(el('div', { class: 'grid square' }, [...skeletons(12, 'square').children]));
      let artists;
      try { artists = await api.get('/api/music/artists'); } catch (e) { if (my === token) body.replaceChildren(emptyState('Could not load artists', e.message, 'music')); return; }
      if (my !== token) return;
      head.querySelector('.count').textContent = `${artists.length} artist${artists.length === 1 ? '' : 's'}`;
      artists.sort((a, b) => a.name.localeCompare(b.name, undefined, { sensitivity: 'base' }));
      body.replaceChildren(artists.length ? el('div', { class: 'grid square' }, artists.map(artistCard)) : emptyState('No artists yet', 'Scan a music folder to populate your library.', 'music'));
    } else {
      const input = el('input', { type: 'search', placeholder: 'Filter tracks…', 'aria-label': 'Filter tracks', autocomplete: 'off', value: params?.get('q') || '' });
      const listWrap = el('div', {}, el('div', { class: 'tracks' }, Array.from({ length: 10 }, () => el('div', { class: 'skel-line' }))));
      let tracks = [];
      const playAll = () => tracks.length && window.Music?.playQueue(tracks, 0);
      const shuffle = () => { if (!tracks.length) return; window.Music?.setShuffle(true, true); window.Music?.playQueue(tracks, Math.floor(Math.random() * tracks.length)); };
      const bar = el('div', { class: 'toolbar sub' },
        el('div', { class: 'filter glass' }, icon('search'), input),
        el('span', { class: 'spacer flex' }),
        el('button', { class: 'btn small primary', type: 'button', onclick: playAll }, icon('play'), 'Play all'),
        el('button', { class: 'btn small', type: 'button', onclick: shuffle }, icon('shuffle'), 'Shuffle'));
      body.replaceChildren(bar, listWrap);
      let tq = 0;
      const fetchTracks = async () => {
        const mine = ++tq;
        const q = input.value.trim();
        try {
          const r = await api.get(`/api/music/tracks?limit=500${q ? `&q=${encodeURIComponent(q)}` : ''}`);
          if (mine !== tq || my !== token) return;
          // The server filters on q; narrow again locally so the list is exact even when it doesn't.
          const needle = q.toLowerCase();
          tracks = needle ? r.filter((t) => [t.title, t.artist, t.album].some((x) => (x || '').toLowerCase().includes(needle))) : r;
          head.querySelector('.count').textContent = `${tracks.length} track${tracks.length === 1 ? '' : 's'}${q ? ' · filtered' : ''}`;
          listWrap.replaceChildren(tracks.length ? trackList(tracks, { showAlbum: true }) : emptyState(q ? 'No matches' : 'No tracks yet', q ? `Nothing matches “${q}”.` : 'Scan a music folder to populate your library.', 'music'));
        } catch (e) { if (mine === tq) listWrap.replaceChildren(emptyState('Could not load tracks', e.message, 'music')); }
      };
      input.addEventListener('input', debounce(fetchTracks, 250));
      input.addEventListener('keydown', (e) => { if (e.key === 'Escape') { input.value = ''; fetchTracks(); input.blur(); } });
      fetchTracks();
    }
  }
  load();
  return () => { token++; };
}

// ---------- /artist/<name> ----------
export async function renderArtist(name) {
  view.replaceChildren(el('div', { class: 'page music' },
    el('div', { class: 'music-head' }, el('div', { class: 'detail-poster square skel' }), el('div', { class: 'detail-main' }, el('div', { class: 'skel-line', style: { width: '40%', height: '40px' } }), el('div', { class: 'skel-line', style: { width: '25%' } }))),
    el('div', { class: 'grid square' }, [...skeletons(6, 'square').children])));
  const here = location.hash;
  let ar;
  try { ar = await api.get(`/api/music/artists/${encodeURIComponent(name)}`); }
  catch (e) { if (location.hash === here) view.replaceChildren(el('div', { class: 'page' }, emptyState('Artist not found', e.message, 'music'))); return; }
  if (location.hash !== here) return; // navigated away while loading
  const albums = (ar.albums || []).slice().sort((a, b) => (a.year || 0) - (b.year || 0) || a.title.localeCompare(b.title));
  const nTracks = albums.reduce((s, a) => s + (a.tracks || 0), 0);
  const total = albums.reduce((s, a) => s + (a.duration || 0), 0);
  ambientFrom(artUrl(ar.artId, 'backdrop', 640));

  const allTracks = async () => {
    const out = [];
    for (const a of albums) { try { const r = await api.get(`/api/music/albums/${encodeURIComponent(a.id)}`); out.push(...(r.tracks || [])); } catch {} }
    return out;
  };
  const playAll = async () => { const t = await allTracks(); if (!t.length) return toast('No tracks', 'err'); window.Music?.setShuffle(false, true); window.Music?.playQueue(t, 0); };
  const shuffle = async () => { const t = await allTracks(); if (!t.length) return toast('No tracks', 'err'); window.Music?.setShuffle(true, true); window.Music?.playQueue(t, Math.floor(Math.random() * t.length)); };

  const poster = el('div', { class: 'detail-poster square' }, el('div', { class: 'fallback' }, initials(ar.name)), img(artUrl(ar.artId, 'poster', 600), { alt: ar.name }));
  view.replaceChildren(el('div', { class: 'page music detail' },
    el('div', { class: 'backdrop-bleed', style: { backgroundImage: `url("${artUrl(ar.artId, 'backdrop', 640)}")` } }),
    el('div', { class: 'music-head' },
      poster,
      el('div', { class: 'detail-main' },
        el('span', { class: 'eyebrow' }, 'Artist'),
        el('h1', {}, ar.name),
        el('div', { class: 'hero-meta' },
          el('span', { class: 'chip' }, `${albums.length} album${albums.length === 1 ? '' : 's'}`),
          el('span', { class: 'chip' }, `${nTracks} track${nTracks === 1 ? '' : 's'}`),
          total ? el('span', { class: 'chip' }, fmtMins(total)) : null,
          ...[...new Set(albums.flatMap((a) => a.meta?.genres || []))].slice(0, 3).map((g) => el('span', { class: 'chip' }, g))),
        headActions(playAll, shuffle))),
    el('section', { class: 'section' },
      el('h2', {}, 'Albums', el('small', {}, 'by year')),
      albums.length ? el('div', { class: 'grid square' }, albums.map(albumCard)) : emptyState('No albums', 'Nothing found for this artist.', 'music'))));
  return () => ambientFrom(null);
}

// ---------- /album/<id> ----------
export async function renderAlbum(id) {
  view.replaceChildren(el('div', { class: 'page music' },
    el('div', { class: 'music-head' }, el('div', { class: 'detail-poster square skel' }), el('div', { class: 'detail-main' }, el('div', { class: 'skel-line', style: { width: '50%', height: '40px' } }), el('div', { class: 'skel-line', style: { width: '30%' } }))),
    el('div', { class: 'tracks' }, Array.from({ length: 8 }, () => el('div', { class: 'skel-line' })))));
  const here = location.hash;
  let r;
  try { r = await api.get(`/api/music/albums/${encodeURIComponent(id)}`); }
  catch (e) { if (location.hash === here) view.replaceChildren(el('div', { class: 'page' }, emptyState('Album not found', e.message, 'music'))); return; }
  if (location.hash !== here) return; // navigated away while loading
  const al = r.album, tracks = r.tracks || [];
  const total = tracks.reduce((s, t) => s + (t.duration || 0), 0) || al.duration || 0;
  const m = al.meta || {};
  const key = `album:${al.id}`;
  ambientFrom(artUrl(key, 'backdrop', 640));

  const play = () => { window.Music?.setShuffle(false, true); window.Music?.playQueue(tracks, 0); };
  const shuffle = () => { window.Music?.setShuffle(true, true); window.Music?.playQueue(tracks, Math.floor(Math.random() * tracks.length)); };
  const queue = () => window.Music?.enqueue(tracks);

  view.replaceChildren(el('div', { class: 'page music detail' },
    el('div', { class: 'backdrop-bleed', style: { backgroundImage: `url("${artUrl(key, 'backdrop', 640)}")` } }),
    el('div', { class: 'music-head' },
      el('div', { class: 'detail-poster square' }, el('div', { class: 'fallback' }, initials(al.title)), img(artUrl(key, 'poster', 600), { alt: al.title })),
      el('div', { class: 'detail-main' },
        el('a', { class: 'eyebrow', href: `#/artist/${encodeURIComponent(al.artist)}` }, al.artist),
        el('h1', {}, al.title),
        el('div', { class: 'hero-meta' },
          al.year ? el('span', { class: 'chip' }, String(al.year)) : null,
          el('span', { class: 'chip' }, `${tracks.length || al.tracks || 0} track${(tracks.length || al.tracks) === 1 ? '' : 's'}`),
          total ? el('span', { class: 'chip' }, fmtMins(total)) : null,
          ...(m.genres || []).slice(0, 3).map((g) => el('span', { class: 'chip' }, g))),
        headActions(play, shuffle, el('button', { class: 'btn', type: 'button', onclick: queue }, icon('queue'), 'Add to queue')))),
    el('section', { class: 'section' },
      tracks.length ? trackList(tracks, { numbers: true, albumArtist: al.artist }) : emptyState('No tracks', 'This album has no playable tracks.', 'music')),
    m.overview ? el('section', { class: 'section' }, el('h2', {}, 'About'), el('p', { class: 'overview' }, m.overview)) : null));
  return () => ambientFrom(null);
}
