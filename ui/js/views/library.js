/* Copyright 2026 The Ontele Authors — SPDX-License-Identifier: Apache-2.0 */
/* Library views: movie grid, show grid, show detail (seasons + episodes), item detail. */

import {
  el, append, api, view, icon, toast, modal, confirm, fmtTime, fmtMins, fmtDate, fmtDateLong, fmtClock, fmtBytes, fmtAgo,
  epCode, resLabel, pref, ambientFrom, img, artUrl, go, navigate, debounce, emptyState, busy, initials, session,
} from '../core.js';
import { card, showCard, metaChips, openItem } from '../cards.js';

// ---------------------------------------------------------------- shared bits

/// replaceChildren that flattens arrays and skips null/false like el() does.
const set = (n, ...kids) => { n.replaceChildren(); return append(n, kids); };

const pageHead = (title, count, ...extra) => el('div', { class: 'page-head' }, el('h1', {}, title), count, el('span', { class: 'spacer' }), ...extra);

/// Segmented control. `options` = [[value, label], …]
function seg(options, value, onChange, label) {
  let cur = value;
  const s = el('div', { class: 'seg', role: 'group', 'aria-label': label });
  for (const [v, text] of options) {
    s.append(el('button', {
      type: 'button', class: v === cur ? 'on' : '', 'aria-pressed': String(v === cur), dataset: { v },
      onclick: () => {
        if (v === cur) return;
        cur = v;
        for (const b of s.children) { const on = b.dataset.v === v; b.classList.toggle('on', on); b.setAttribute('aria-pressed', String(on)); }
        onChange(v);
      },
    }, text));
  }
  return s;
}

const chipBtn = (label, on, onclick, ...kids) =>
  el('button', { type: 'button', class: `chip ${on ? 'on' : ''}`, 'aria-pressed': String(!!on), onclick }, ...kids, label);

/// Inline text filter (client side). Debounced; Esc clears.
function filterBox(initial, onChange, placeholder = 'Filter titles…') {
  const input = el('input', { type: 'search', value: initial || '', placeholder, 'aria-label': placeholder, autocomplete: 'off', spellcheck: 'false' });
  const clear = el('button', { type: 'button', class: 'clear', 'aria-label': 'Clear filter', hidden: !initial, onclick: () => { input.value = ''; clear.hidden = true; onChange(''); input.focus(); } }, icon('x'));
  const fire = debounce(() => onChange(input.value), 120);
  input.addEventListener('input', () => { clear.hidden = !input.value; fire(); });
  input.addEventListener('keydown', (e) => { if (e.key === 'Escape' && input.value) { e.stopPropagation(); input.value = ''; clear.hidden = true; onChange(''); } });
  return el('label', { class: 'filter' }, icon('search'), input, clear);
}

/// Write list state into the hash without triggering a navigation.
function syncHash(base, state, defaults = {}) {
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(state)) if (v && v !== defaults[k]) p.set(k, v === true ? '1' : v);
  const s = p.toString();
  history.replaceState(null, '', `#/${base}${s ? '?' + s : ''}`);
}
const truthy = (v) => v === '1' || v === 'true';

/// Fill a grid. Big lists render in idle-time chunks so the first paint is instant.
function fillGrid(grid, items, mk, token) {
  grid.replaceChildren();
  const gen = (token.gen = (token.gen || 0) + 1); // a newer fill supersedes any in-flight chunks
  const chunk = items.length > 400 ? 120 : Infinity;
  let i = 0;
  const step = () => {
    if (token.cancelled || token.gen !== gen) return;
    const frag = document.createDocumentFragment();
    const end = Math.min(items.length, i + chunk);
    for (; i < end; i++) frag.append(mk(items[i]));
    grid.append(frag);
    if (i < items.length) (window.requestIdleCallback || ((f) => setTimeout(f, 16)))(step, { timeout: 250 });
  };
  step();
}
const skelCards = (n, cls = '') => Array.from({ length: n }, () => el('div', { class: `card skel ${cls}` }, el('div', { class: 'art' })));

function notFound(what, backHash, backLabel) {
  return el('div', { class: 'page' },
    el('div', { class: 'empty' }, icon('search'),
      el('div', {}, el('b', {}, `That ${what} isn’t here`),
        'It may have been removed from disk, or the library is still scanning. ',
        el('div', { class: 'acts' },
          history.length > 1 ? el('button', { class: 'btn small', type: 'button', onclick: () => history.back() }, icon('chevronL'), 'Go back') : null,
          el('button', { class: 'btn small', type: 'button', onclick: () => go(backHash) }, backLabel)))));
}

const plural = (n, one, many = one + 's') => `${n.toLocaleString()} ${n === 1 ? one : many}`;

// ---------------------------------------------------------------- movies

export async function renderMovies(params = new URLSearchParams()) {
  if (!(params instanceof URLSearchParams)) params = new URLSearchParams(); // `#/movies/junk` passes a path arg first
  ambientFrom(null);
  const state = {
    sort: params.get('sort') || 'title', genre: params.get('genre') || '', tag: params.get('tag') || '',
    unwatched: truthy(params.get('unwatched')), q: params.get('q') || '',
  };
  const token = { cancelled: false };
  const count = el('span', { class: 'count' });
  const grid = el('div', { class: 'grid', role: 'list' }, skelCards(18));
  const wrap = el('div', { class: 'grid-wrap' }, grid);
  const facets = el('div', { class: 'facets' });
  const toolbar = el('div', { class: 'toolbar' });
  view.replaceChildren(el('div', { class: 'page' }, pageHead('Movies', count), toolbar, facets, wrap));

  let all = [], seq = 0;
  const sync = () => { if (!token.cancelled) syncHash('movies', state, { sort: 'title' }); };

  const paint = () => {
    if (token.cancelled) return;
    const q = state.q.trim().toLowerCase();
    const list = q ? all.filter((m) => (m.title || '').toLowerCase().includes(q) || String(m.year || '').includes(q)) : all;
    const narrowed = state.genre || state.tag || state.unwatched; // server-side filters: `all` is already a subset
    count.textContent = list.length === all.length ? `${plural(all.length, 'movie')}${narrowed ? ' match' : ''}` : `${list.length.toLocaleString()} of ${plural(all.length, 'movie')}${narrowed ? ' matching' : ''}`;
    wrap.querySelector('.empty')?.remove();
    if (!list.length) {
      grid.replaceChildren();
      const filtered = q || state.genre || state.tag || state.unwatched;
      wrap.append(el('div', { class: 'empty' }, icon('film'),
        el('div', {}, el('b', {}, filtered ? 'No movies match' : 'No movies yet'),
          filtered ? 'Try loosening the filters.' : 'Add a movies folder in Settings and Ontele will catalog it.',
          el('div', { class: 'acts' }, filtered
            ? el('button', { class: 'btn small', type: 'button', onclick: () => { Object.assign(state, { genre: '', tag: '', unwatched: false, q: '' }); sync(); rebuild(); load(); } }, 'Clear filters')
            : el('button', { class: 'btn small', type: 'button', onclick: () => go('#/settings') }, 'Open Settings')))));
      return;
    }
    fillGrid(grid, list, (m) => card(m), token);
  };

  const load = async () => {
    const my = ++seq;
    wrap.classList.add('loading');
    const p = new URLSearchParams({ sort: state.sort });
    if (state.genre) p.set('genre', state.genre);
    if (state.tag) p.set('tag', state.tag);
    if (state.unwatched) p.set('unwatched', 'true');
    try {
      const r = await api.get(`/api/movies?${p}`);
      if (my !== seq || token.cancelled) return;
      all = Array.isArray(r) ? r : [];
    } catch (e) { if (my !== seq || token.cancelled) return; toast(e.message, 'err'); all = []; }
    wrap.classList.remove('loading');
    paint();
  };

  let genres = [], tags = [];
  const rebuild = () => {
    toolbar.replaceChildren(
      seg([['title', 'Title'], ['added', 'Added'], ['year', 'Year'], ['rating', 'Rating']], state.sort, (v) => { state.sort = v; sync(); load(); }, 'Sort'),
      chipBtn('Unwatched', state.unwatched, () => { state.unwatched = !state.unwatched; sync(); rebuild(); load(); }, icon('eye')),
      filterBox(state.q, (v) => { state.q = v; sync(); paint(); }));
    facets.replaceChildren();
    if (genres.length) {
      facets.append(el('span', { class: 'lbl' }, 'Genre'));
      for (const g of genres) facets.append(chipBtn([g.name, g.count ? el('small', {}, String(g.count)) : null], state.genre === g.name, () => { state.genre = state.genre === g.name ? '' : g.name; sync(); rebuild(); load(); }));
    }
    if (tags.length) {
      if (genres.length) facets.append(el('span', { class: 'sep' }));
      facets.append(el('span', { class: 'lbl' }, 'Tags'));
      for (const t of tags) facets.append(chipBtn(t.name, state.tag === t.name, () => { state.tag = state.tag === t.name ? '' : t.name; sync(); rebuild(); load(); }, icon('tag')));
    }
    facets.hidden = !facets.childNodes.length;
  };
  rebuild();

  const loadFacets = async () => {
    const [g, t] = await Promise.all([api.get('/api/genres').catch(() => ({})), api.get('/api/tags').catch(() => [])]);
    if (token.cancelled) return;
    genres = (g.movies || []).filter((x) => x.count > 0);
    const seen = new Set();
    tags = (Array.isArray(t) ? t : []).filter((x) => x.count > 0 && !seen.has(x.name) && seen.add(x.name)).sort((a, b) => a.name.localeCompare(b.name));
    // keep a genre/tag from the URL visible even if the server doesn't list it
    if (state.genre && !genres.some((x) => x.name === state.genre)) genres.push({ name: state.genre, count: 0 });
    if (state.tag && !tags.some((x) => x.name === state.tag)) tags.push({ name: state.tag, count: 0 });
    rebuild();
  };
  await Promise.all([load(), loadFacets()]);
  return () => { token.cancelled = true; };
}

// ---------------------------------------------------------------- shows

export async function renderShows(params = new URLSearchParams()) {
  if (!(params instanceof URLSearchParams)) params = new URLSearchParams(); // `#/movies/junk` passes a path arg first
  ambientFrom(null);
  const state = { sort: params.get('sort') || 'title', progress: truthy(params.get('progress')), q: params.get('q') || '' };
  const token = { cancelled: false };
  const count = el('span', { class: 'count' });
  const grid = el('div', { class: 'grid', role: 'list' }, skelCards(14));
  const wrap = el('div', { class: 'grid-wrap' }, grid);
  const toolbar = el('div', { class: 'toolbar' });
  view.replaceChildren(el('div', { class: 'page' }, pageHead('Shows', count), toolbar, wrap));

  let all = [];
  const sync = () => { if (!token.cancelled) syncHash('shows', state, { sort: 'title' }); };
  const paint = () => {
    if (token.cancelled) return;
    const q = state.q.trim().toLowerCase();
    let list = all;
    if (state.progress) list = list.filter((s) => s.watched > 0 && s.watched < s.episodes);
    if (q) list = list.filter((s) => s.show.toLowerCase().includes(q));
    list = [...list].sort(state.sort === 'added' ? (a, b) => new Date(b.added) - new Date(a.added) : (a, b) => a.show.localeCompare(b.show, undefined, { sensitivity: 'base' }));
    count.textContent = list.length === all.length ? plural(all.length, 'show') : `${list.length} of ${plural(all.length, 'show')}`;
    wrap.querySelector('.empty')?.remove();
    if (!list.length) {
      grid.replaceChildren();
      const filtered = q || state.progress;
      wrap.append(el('div', { class: 'empty' }, icon('tv'),
        el('div', {}, el('b', {}, filtered ? 'No shows match' : 'No shows yet'),
          filtered ? 'Nothing in progress under that filter.' : 'Add a TV folder in Settings — episodes are grouped by show and season automatically.',
          el('div', { class: 'acts' }, filtered
            ? el('button', { class: 'btn small', type: 'button', onclick: () => { state.progress = false; state.q = ''; sync(); rebuild(); paint(); } }, 'Clear filters')
            : el('button', { class: 'btn small', type: 'button', onclick: () => go('#/settings') }, 'Open Settings')))));
      return;
    }
    fillGrid(grid, list, showCard, token);
  };
  const rebuild = () => toolbar.replaceChildren(
    seg([['title', 'Title'], ['added', 'Recently added']], state.sort, (v) => { state.sort = v; sync(); paint(); }, 'Sort'),
    chipBtn('In progress', state.progress, () => { state.progress = !state.progress; sync(); rebuild(); paint(); }, icon('play')),
    filterBox(state.q, (v) => { state.q = v; sync(); paint(); }, 'Filter shows…'));
  rebuild();
  try { all = await api.get('/api/shows'); } catch (e) { toast(e.message, 'err'); all = []; }
  if (token.cancelled) return;
  paint();
  return () => { token.cancelled = true; };
}

// ---------------------------------------------------------------- show detail

const seasonLabel = (n) => (n === 0 ? 'Specials' : `Season ${n}`);

/// First unwatched episode (specials last); falls back to the very first episode.
function pickNext(seasons) {
  const ordered = [...seasons].sort((a, b) => (a.season === 0) - (b.season === 0) || a.season - b.season);
  const eps = ordered.flatMap((s) => s.episodes);
  const partial = eps.find((e) => e.watch && !e.watch.done && e.watch.pos > 60);
  return partial || eps.find((e) => !e.watch?.done) || eps[0] || null;
}

function detailSkeleton(wide) {
  return el('div', { class: 'page detail-page' },
    el('div', { class: 'detail detail-skel' },
      el('div', { class: `detail-body ${wide ? 'wide-art' : ''}` },
        el('div', { class: `detail-poster ${wide ? 'wide' : ''}` }),
        el('div', { class: 'detail-main' },
          el('div', { class: 'skel-line w1' }), el('div', { class: 'skel-line w2' }), el('div', { class: 'skel-line' }), el('div', { class: 'skel-line' })))));
}

function refreshMetaBtn(id, after) {
  const b = el('button', { class: 'btn', type: 'button', title: 'Refresh metadata' }, icon('refresh'), 'Refresh metadata');
  b.onclick = () => busy(b, async () => { await api.post(`/api/items/${encodeURIComponent(id)}/refresh-metadata`); after && after(); }, 'Metadata refreshed');
  return b;
}

export async function renderShow(name) {
  view.replaceChildren(detailSkeleton(false));
  let d;
  try { d = await api.get(`/api/shows/${encodeURIComponent(name)}`); } catch (e) {
    view.replaceChildren(e.status === 404 ? notFound('show', '#/shows', 'All shows') : el('div', { class: 'page' }, emptyState('Couldn’t load this show', e.message || 'The server did not answer.', 'info')));
    return;
  }
  const m = d.meta || {};
  const key = `show:${d.show}`;
  const backdrop = artUrl(key, 'backdrop', 1280);
  ambientFrom(backdrop);
  const seasons = (d.seasons || []).map((s) => ({ season: s.season ?? 0, episodes: s.episodes || [] }));
  const allEps = () => seasons.flatMap((s) => s.episodes);
  const year = m.releaseDate ? m.releaseDate.slice(0, 4) : (allEps().find((e) => e.year)?.year || '');

  // --- header chips + progress
  const chips = el('div', { class: 'hero-meta' });
  const chipsPaint = () => set(chips,
    year ? el('span', { class: 'chip' }, String(year)) : null,
    m.contentRating ? el('span', { class: 'chip' }, m.contentRating) : null,
    el('span', { class: 'chip' }, plural(seasons.length, 'season')),
    el('span', { class: 'chip' }, plural(allEps().length, 'episode')),
    m.rating ? el('span', { class: 'chip rating' }, icon('star'), ` ${m.rating.toFixed(1)}`) : null,
    ...(m.genres || []).slice(0, 4).map((g) => el('span', { class: 'chip' }, g)));
  chipsPaint();

  const progress = el('div', { class: 'show-progress' });
  const playBtn = el('button', { class: 'btn primary', type: 'button' });
  const paintHeader = () => {
    const eps = allEps(), watched = eps.filter((e) => e.watch?.done).length;
    progress.replaceChildren(
      el('div', { class: 'bar' }, el('i', { style: { width: `${eps.length ? watched / eps.length * 100 : 0}%` } })),
      el('span', {}, el('span', { class: 'n' }, String(watched)), ` of ${eps.length} watched`));
    const next = pickNext(seasons);
    playBtn.replaceChildren(icon('play'), !next ? 'Play' : (next.watch && !next.watch.done && next.watch.pos > 60) ? `Resume ${epCode(next)}` : watched >= eps.length && eps.length ? `Play again · ${epCode(next)}` : `Play ${epCode(next)}`);
    playBtn.disabled = !next;
    playBtn.onclick = () => next && openItem(next);
  };
  paintHeader();

  // --- seasons
  const prefKey = `show.season.${d.show}`;
  const next = pickNext(seasons);
  let cur = pref.get(prefKey, null);
  if (!seasons.some((s) => s.season === cur)) cur = next ? next.season : (seasons[0]?.season ?? 0);
  const tabs = el('div', { class: 'tabs', role: 'tablist', 'aria-label': 'Seasons' });
  const list = el('div', { class: 'ep-list', role: 'list' });
  const secTitle = el('h2', {});
  const secCount = el('small', {});
  const paintSeason = () => {
    const s = seasons.find((x) => x.season === cur) || seasons[0];
    for (const b of tabs.children) { const on = Number(b.dataset.s) === cur; b.classList.toggle('on', on); b.setAttribute('aria-selected', String(on)); b.tabIndex = on ? 0 : -1; }
    if (!s) { list.replaceChildren(emptyState('No episodes', 'Nothing has been scanned for this show yet.', 'tv')); return; }
    secTitle.textContent = seasonLabel(s.season);
    const w = s.episodes.filter((e) => e.watch?.done).length;
    secCount.textContent = `${plural(s.episodes.length, 'episode')}${w ? ` · ${w} watched` : ''}`;
    const nextId = pickNext(seasons)?.id;
    list.replaceChildren(...s.episodes.map((e) => epRow(e, { highlight: e.id === nextId, onChange: () => { paintHeader(); paintSeason(); } })));
  };
  for (const s of seasons) {
    const w = s.episodes.filter((e) => e.watch?.done).length;
    tabs.append(el('button', { type: 'button', role: 'tab', dataset: { s: String(s.season) }, onclick: () => { cur = s.season; pref.set(prefKey, cur); paintSeason(); } },
      seasonLabel(s.season), el('small', {}, w >= s.episodes.length ? '✓' : `${s.episodes.length}`)));
  }
  tabs.addEventListener('keydown', (e) => {
    if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(e.key)) return;
    e.preventDefault();
    const btns = [...tabs.children];
    let i = btns.findIndex((b) => Number(b.dataset.s) === cur);
    i = e.key === 'ArrowLeft' ? Math.max(0, i - 1) : e.key === 'ArrowRight' ? Math.min(btns.length - 1, i + 1) : e.key === 'Home' ? 0 : btns.length - 1;
    btns[i].click(); btns[i].focus();
  });
  paintSeason();

  view.replaceChildren(el('div', { class: 'page detail-page' },
    el('div', { class: 'detail' },
      el('div', { class: 'backdrop-bleed', style: { backgroundImage: `url('${backdrop}')` } }),
      el('div', { class: 'backdrop', style: { backgroundImage: `url('${backdrop}')` } }),
      el('div', { class: 'scrim' }),
      el('div', { class: 'detail-body' },
        el('div', { class: 'detail-poster' }, img(artUrl(key, 'poster', 480), { alt: `${d.show} poster` })),
        el('div', { class: 'detail-main' },
          el('span', { class: 'eyebrow' }, 'Series', m.studio ? el('i', {}, '·') : null, m.studio || null),
          el('h1', {}, d.show),
          m.tagline ? el('p', { class: 'tagline' }, m.tagline) : null,
          chips,
          m.overview ? el('p', { class: 'overview' }, m.overview) : null,
          el('div', { class: 'actions' }, playBtn, refreshMetaBtn(key, () => navigate())),
          progress)),
      el('div', { class: 'detail-sections' },
        el('section', { class: 'section' },
          el('div', { class: 'section-head' }, secTitle, secCount),
          seasons.length > 1 ? tabs : null,
          list)))));
  return () => ambientFrom(null);
}

/// One episode row for the season list. Mutates ep.watch on toggle and re-renders itself.
function epRow(ep, opts = {}) {
  const w = ep.watch, done = !!w?.done;
  const prog = w && !done && w.dur > 0 ? Math.min(100, w.pos / w.dur * 100) : 0;
  const desc = ep.description || ep.meta?.overview || '';
  const thumb = el('button', { class: 'thumb', type: 'button', 'aria-label': `Play ${epCode(ep)} ${ep.title || ''}`, onclick: () => openItem(ep) },
    img(artUrl(ep.id, 'thumb', 400)),
    el('div', { class: 'play' }, el('i', {}, icon('play'))),
    ep.duration ? el('span', { class: 'dur' }, fmtTime(ep.duration)) : null,
    done ? el('span', { class: 'badge r done' }, icon('check')) : null,
    prog ? el('div', { class: 'prog' }, el('i', { style: { width: `${prog}%` } })) : null);
  const facts = [ep.duration ? fmtMins(ep.duration) : null, ep.airDate ? fmtDate(ep.airDate) : null, resLabel(ep.height) || null, prog ? `${Math.round(prog)}% watched` : null].filter(Boolean).join(' · ');
  const info = el('div', { class: 'info', role: 'link', tabindex: '0', title: 'Details', onclick: () => go(`#/item/${encodeURIComponent(ep.id)}`), onkeydown: (e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); go(`#/item/${encodeURIComponent(ep.id)}`); } } },
    el('b', {}, el('em', {}, epCode(ep)), ep.title || 'Untitled'),
    desc ? el('p', {}, desc) : null,
    facts ? el('small', {}, facts) : null);
  const toggle = el('button', { class: `btn small ${done ? 'on' : ''}`, type: 'button', title: done ? 'Mark unwatched' : 'Mark watched' }, icon(done ? 'check' : 'eye'), done ? 'Watched' : 'Mark watched');
  toggle.onclick = () => busy(toggle, async () => {
    if (done) { await api.del(`/api/watch/${encodeURIComponent(ep.id)}`); ep.watch = null; }
    else { const dur = ep.duration || w?.dur || 0; await api.post(`/api/watch/${encodeURIComponent(ep.id)}`, { pos: dur, dur, done: true }); ep.watch = { pos: dur, dur, done: true }; }
    row.replaceWith(epRow(ep, opts));
    opts.onChange && opts.onChange();
  });
  const acts = el('div', { class: 'acts' },
    el('button', { class: 'btn small', type: 'button', onclick: () => openItem(ep) }, icon('play'), prog ? 'Resume' : 'Play'),
    el('button', { class: 'btn small', type: 'button', onclick: () => go(`#/item/${encodeURIComponent(ep.id)}`) }, icon('info'), 'Details'),
    toggle);
  const row = el('div', { class: `ep ${done ? 'done' : ''} ${opts.highlight ? 'now' : ''}`, role: 'listitem' }, thumb, info, acts);
  return row;
}

// ---------------------------------------------------------------- item detail

const KIND_LABEL = { movie: 'Movie', episode: 'Episode', recording: 'Recording', track: 'Track' };
const HDR_LABEL = { dv: 'Dolby Vision', hlg: 'HLG', hdr10plus: 'HDR10+', hdr10: 'HDR10' };
const CODEC_LABEL = { h264: 'H.264', hevc: 'HEVC', av1: 'AV1', vp9: 'VP9', vp8: 'VP8', mpeg2video: 'MPEG-2', aac: 'AAC', ac3: 'Dolby Digital', eac3: 'Dolby Digital+', truehd: 'TrueHD', dts: 'DTS', flac: 'FLAC', mp3: 'MP3', opus: 'Opus', vorbis: 'Vorbis', pcm_s16le: 'PCM', subrip: 'SRT', ass: 'ASS', hdmv_pgs_subtitle: 'PGS', dvd_subtitle: 'VobSub', mov_text: 'MP4 text', webvtt: 'VTT' };
const codecName = (c) => (c ? CODEC_LABEL[c.toLowerCase()] || c.toUpperCase() : '');
let langNames;
const LANG_SPECIAL = { und: 'Unknown', zxx: 'No language', mul: 'Multiple', mis: 'Other' };
function langName(code) {
  if (!code) return '';
  if (LANG_SPECIAL[code.toLowerCase()]) return LANG_SPECIAL[code.toLowerCase()];
  try { langNames ||= new Intl.DisplayNames([navigator.language || 'en'], { type: 'language' }); const n = langNames.of(code); if (n && n.toLowerCase() !== code.toLowerCase()) return n; } catch {}
  return code.toUpperCase();
}
/// Native name of a language ("français"), so a stream titled that way isn't repeated after "French".
function endonym(code) {
  if (!code || LANG_SPECIAL[code.toLowerCase()]) return '';
  try { return new Intl.DisplayNames([code], { type: 'language' }).of(code) || ''; } catch { return ''; }
}
const channelsLabel = (n) => (n >= 8 ? '7.1' : n >= 6 ? '5.1' : n === 2 ? 'Stereo' : n === 1 ? 'Mono' : n ? `${n}ch` : '');
const ADS = { cut: ['Ads removed', 'state-cut'], ready: ['Ad-skip ready', 'state-ready'], pending: ['Scanning for ads…', 'state-pending'], failed: ['Ad scan failed', 'state-failed'] };

export async function renderItem(id) {
  view.replaceChildren(detailSkeleton(true));
  let it;
  try { it = await api.get(`/api/items/${encodeURIComponent(id)}`); } catch (e) {
    view.replaceChildren(e.status === 404 ? notFound('title', '#/home', 'Home') : el('div', { class: 'page' }, emptyState('Couldn’t load this title', e.message || 'The server did not answer.', 'info')));
    return;
  }
  const m = it.meta || {}, info = it.info || {};
  const kind = it.kind;
  const isAdmin = !!session.user?.isAdmin;

  // --- artwork
  let bg, blur = false, posterUrl, posterCls = '';
  if (kind === 'episode') { bg = artUrl(it.show ? `show:${it.show}` : it.id, 'backdrop', 1280); posterUrl = artUrl(it.id, 'thumb', 800); posterCls = 'wide'; }
  else if (kind === 'track') { bg = artUrl(`album:${it.albumId}`, 'poster', 1280); blur = true; posterUrl = artUrl(`album:${it.albumId}`, 'poster', 600); posterCls = 'square'; }
  else if (kind === 'recording') { bg = artUrl(it.id, 'thumb', 1280); posterUrl = artUrl(it.id, 'thumb', 800); posterCls = 'wide'; }
  else { bg = artUrl(it.id, 'backdrop', 1280); posterUrl = artUrl(it.id, 'poster', 600); }
  ambientFrom(bg);

  // --- eyebrow
  const eyebrow = el('span', { class: 'eyebrow' }, KIND_LABEL[kind] || kind);
  if (kind === 'episode' && it.show) eyebrow.append(el('i', {}, '·'), el('a', { href: `#/show/${encodeURIComponent(it.show)}` }, it.show), el('i', {}, '·'), epCode(it));
  if (kind === 'track') {
    if (it.album) eyebrow.append(el('i', {}, '·'), el('a', { href: `#/album/${encodeURIComponent(it.albumId || '')}` }, it.album));
    if (it.artist) eyebrow.append(el('i', {}, '·'), el('a', { href: `#/artist/${encodeURIComponent(it.artist)}` }, it.artist));
  }
  if (kind === 'recording' && it.channel) eyebrow.append(el('i', {}, '·'), `${it.channel}${it.channelId ? ' ' + it.channelId : ''}`);

  // --- chips
  const baseChips = metaChips(it);
  const seen = new Set(baseChips.map((c) => c.textContent.trim().toLowerCase()));
  if (it.hdr) seen.add('hdr');
  const autoChips = (it.autoTags || []).filter((t) => !seen.has(String(t).toLowerCase()) && seen.add(String(t).toLowerCase())).map((t) => el('span', { class: 'chip' }, t));
  const chips = el('div', { class: 'hero-meta' }, baseChips, autoChips);

  // --- play / resume / watched
  const resumable = () => it.watch && !it.watch.done && it.watch.pos > 60;
  const playable = kind !== 'recording' || ['done', 'recording'].includes(it.status || 'done');
  const playBtn = el('button', { class: 'btn primary', type: 'button', disabled: !playable });
  const startOver = el('button', { class: 'btn', type: 'button', onclick: () => openItem(it, { from: 0 }) }, 'Start over');
  const paintPlay = () => {
    playBtn.replaceChildren(icon('play'), resumable() ? `Resume · ${fmtTime(it.watch.pos)}` : kind === 'track' ? 'Play' : it.watch?.done ? 'Play again' : 'Play');
    playBtn.onclick = () => (kind === 'track' ? window.Music?.playTrack(it) : openItem(it));
    startOver.hidden = !resumable();
    if (kind === 'recording' && it.status === 'recording') playBtn.replaceChildren(icon('live'), 'Watch live'); // openItem routes to the channel
    if (kind === 'recording' && it.status === 'scheduled') playBtn.replaceChildren(icon('dvr'), `Records ${fmtDateLong(it.start)} ${fmtClock(it.start)}`);
    if (kind === 'recording' && it.status === 'failed') playBtn.replaceChildren(icon('x'), 'Recording failed');
  };
  paintPlay();
  const watchBtn = kind === 'track' ? null : el('button', { class: 'btn', type: 'button' });
  const paintWatch = () => {
    if (!watchBtn) return;
    const done = !!it.watch?.done;
    watchBtn.replaceChildren(icon(done ? 'check' : 'eye'), done ? 'Watched' : 'Mark watched');
    watchBtn.classList.toggle('on', done);
    watchBtn.title = done ? 'Mark unwatched' : 'Mark watched';
    watchBtn.setAttribute('aria-pressed', String(done));
  };
  paintWatch();
  if (watchBtn) watchBtn.onclick = () => busy(watchBtn, async () => {
    if (it.watch?.done) { await api.del(`/api/watch/${encodeURIComponent(id)}`); it.watch = null; }
    else { const dur = it.duration || info.durationSec || it.watch?.dur || 0; await api.post(`/api/watch/${encodeURIComponent(id)}`, { pos: dur, dur, done: true }); it.watch = { pos: dur, dur, done: true }; }
    paintWatch(); paintPlay();
  });

  // --- admin: fix match / delete
  const admin = [];
  if (isAdmin) {
    if (kind !== 'track' && kind !== 'recording') {
      admin.push(el('button', { class: 'btn', type: 'button', onclick: () => fixMatch(it) }, icon('search'), 'Fix match'));
    }
    const del = el('button', { class: 'btn danger', type: 'button' }, icon('trash'), 'Delete');
    del.onclick = async () => {
      const ok = await confirm(`Delete “${it.title}”?`, kind === 'recording' ? 'The recording file is removed from disk. This cannot be undone.' : 'The file is removed from disk and the item leaves the library. This cannot be undone.');
      if (!ok) return;
      busy(del, async () => {
        await api.del(kind === 'recording' ? `/api/dvr/recordings/${encodeURIComponent(id)}` : `/api/items/${encodeURIComponent(id)}`);
        go(kind === 'recording' ? '#/dvr' : kind === 'episode' && it.show ? `#/show/${encodeURIComponent(it.show)}` : kind === 'track' && it.albumId ? `#/album/${it.albumId}` : '#/movies');
      }, 'Deleted');
    };
    admin.push(del);
  }

  // --- recordings: ad tools
  const adTools = [];
  if (kind === 'recording' && it.status === 'done') {
    const rescan = el('button', { class: 'btn', type: 'button' }, icon('refresh'), 'Rescan ads');
    rescan.onclick = () => busy(rescan, () => api.post(`/api/dvr/recordings/${encodeURIComponent(id)}/adscan`), 'Ad scan queued');
    const cut = el('button', { class: 'btn', type: 'button', disabled: it.breaksState === 'cut' || !(it.breaks || []).length }, icon('scissors'), 'Cut ads');
    cut.onclick = async () => {
      const ok = await confirm('Cut ads from this recording?', `The ${plural((it.breaks || []).length, 'detected break')} will be removed from the file permanently. Keep the skip markers instead if you want to be able to undo.`, 'Cut ads');
      if (ok) busy(cut, () => api.post(`/api/dvr/recordings/${encodeURIComponent(id)}/adscan?cut=1`), 'Cutting ads in the background');
    };
    adTools.push(rescan, cut);
  }

  // --- left column: overview, up next, cast
  const overview = m.overview || it.description || '';
  const left = el('div', {},
    overview ? el('p', { class: 'overview' }, overview)
      : kind === 'track' ? el('p', { class: 'overview' }, `${it.trackNo ? `Track ${it.trackNo}` : 'A track'}${it.album ? ' on ' : ''}`, it.album ? el('a', { href: `#/album/${encodeURIComponent(it.albumId || '')}`, class: 'link' }, it.album) : null, it.artist ? ' by ' : '', it.artist ? el('a', { href: `#/artist/${encodeURIComponent(it.artist)}`, class: 'link' }, it.artist) : null, it.year ? ` (${it.year})` : '', '.')
      : el('p', { class: 'overview faint' }, 'No synopsis yet — try “Refresh metadata”.'),
    it.error ? el('div', { class: 'err-box' }, it.error) : null,
    kind === 'episode' && it.nextEpisode ? el('section', { class: 'section' },
      el('div', { class: 'section-head' }, el('h2', {}, 'Up next'), el('small', {}, `${epCode(it.nextEpisode)}${it.nextEpisode.title ? ' · ' + it.nextEpisode.title : ''}`)),
      el('div', { class: 'next-up' }, card(it.nextEpisode, { shape: 'wide' }))) : null,
    (m.cast || []).length ? el('section', { class: 'section' },
      el('div', { class: 'section-head' }, el('h2', {}, 'Cast'), el('small', {}, plural(m.cast.length, 'person', 'people'))),
      el('div', { class: 'cast' }, m.cast.slice(0, 24).map((p) => el('div', { class: 'person', title: p.name },
        el('div', { class: 'ph' }, p.profile ? img(p.profile, { alt: p.name }) : initials(p.name)),
        el('b', {}, p.name), p.character ? el('small', {}, p.character) : null)))) : null);

  // --- right column: technical facts
  const kv = el('div', { class: 'kv' });
  const fact = (k, v) => { if (v == null || v === '' || (Array.isArray(v) && !v.length)) return; kv.append(el('b', {}, k), typeof v === 'string' || typeof v === 'number' ? el('span', {}, String(v)) : v); };
  const lines = (arr) => el('div', { class: 'lines' }, arr.map((t) => el('span', {}, t)));
  const vid = info.video || {};
  const width = it.width || info.width, height = it.height || info.height;
  const vparts = [codecName(it.vcodec || info.vcodec), width && height ? `${width}×${height}` : resLabel(height), it.hdr ? HDR_LABEL[it.hdr] || 'HDR' : null, vid.fps ? `${Number(vid.fps).toFixed(vid.fps % 1 ? 3 : 0)} fps` : null, vid.bitDepth ? `${vid.bitDepth}-bit` : null, vid.profile, vid.interlaced ? 'interlaced' : null].filter(Boolean);
  if (kind === 'recording') {
    fact('Channel', it.channel ? `${it.channel}${it.channelId ? ` · ${it.channelId}` : ''}` : it.channelId);
    if (it.start) fact('Aired', `${fmtDateLong(it.start)} · ${fmtClock(it.start)}${it.end ? `–${fmtClock(it.end)}` : ''}`);
    fact('Status', el('span', {}, el('span', { class: `status ${it.status || ''}` }, it.status || 'done')));
    const a = ADS[it.breaksState];
    fact('Ads', el('span', {}, a ? el('span', { class: `chip ${a[1]}` }, a[0]) : 'Not scanned', (it.breaks || []).length ? el('span', { class: 'sub' }, ` ${plural(it.breaks.length, 'break')}`) : null));
  }
  if (kind === 'track') {
    if (it.album) fact('Album', el('a', { href: `#/album/${encodeURIComponent(it.albumId || '')}` }, it.album));
    if (it.artist) fact('Artist', el('a', { href: `#/artist/${encodeURIComponent(it.artist)}` }, it.artist));
    if (it.albumArtist && it.albumArtist !== it.artist) fact('Album artist', it.albumArtist);
    if (it.trackNo) fact('Track', `${it.discNo && it.discNo > 1 ? `Disc ${it.discNo} · ` : ''}#${it.trackNo}`);
    fact('Genre', it.genre);
  }
  if (kind === 'episode' && it.airDate) fact('Aired', fmtDateLong(it.airDate));
  fact('Runtime', (it.duration || info.durationSec) ? fmtTime(it.duration || info.durationSec) : null);
  fact('File', codecName(it.container || info.container) ? (it.container || info.container).toUpperCase() : null);
  if (vparts.length) fact('Video', vparts.join(' · '));
  const audio = info.audio || [];
  if (audio.length) fact('Audio', lines(audio.map((a) => {
    const parts = [langName(a.lang), codecName(a.codec), channelsLabel(a.channels)].filter(Boolean);
    const line = `${parts.join(' ')} ${endonym(a.lang)}`.toLowerCase();
    if (a.title && !a.title.split(/\s+/).every((w) => line.includes(w.toLowerCase()))) parts.push(a.title);
    if (a.default) parts.push('default');
    return parts.join(' · ');
  })));
  else if (it.acodec) fact('Audio', codecName(it.acodec));
  const subs = info.subtitles || [];
  if (subs.length) fact('Subtitles', lines(subs.map((s) => [langName(s.lang) || s.title || 'Unknown', codecName(s.codec), s.forced ? 'forced' : null, s.external ? 'external' : null].filter(Boolean).join(' · '))));
  if (info.bitrate) fact('Bitrate', `${(info.bitrate / 1e6).toFixed(1)} Mbps`);
  if (info.sizeBytes) fact('Size', fmtBytes(info.sizeBytes));
  if ((info.chapters || []).length) fact('Chapters', String(info.chapters.length));
  if (it.added) fact('Added', `${fmtDateLong(it.added)} · ${fmtAgo(it.added)}`);
  if (it.watch?.updated) fact('Last watched', fmtAgo(it.watch.updated));
  if (m.provider) fact('Metadata', `${m.provider}${m.tmdbId ? ` #${m.tmdbId}` : m.mbid ? ` ${String(m.mbid).slice(0, 8)}…` : ''}${m.updated ? ` · ${fmtAgo(m.updated)}` : ''}`);
  const codecs = [...new Set([it.vcodec || info.vcodec, ...audio.map((a) => a.codec), !audio.length && it.acodec, it.container || info.container, it.hdr && (HDR_LABEL[it.hdr] || 'HDR')].filter(Boolean))];
  const right = el('div', {},
    el('div', { class: 'kv-card' }, el('h3', {}, 'Technical'), kv,
      codecs.length ? el('div', { class: 'codecs' }, codecs.map((c) => el('span', { class: 'chip' }, codecName(c) || c))) : null),
    adTools.length ? el('div', { class: 'danger-zone' }, adTools) : null,
    admin.length ? el('div', { class: 'danger-zone' }, admin) : null);

  // --- title
  const title = it.title || (kind === 'episode' ? epCode(it) : 'Untitled');

  view.replaceChildren(el('div', { class: 'page detail-page' },
    el('div', { class: 'detail' },
      el('div', { class: 'backdrop-bleed', style: { backgroundImage: `url('${bg}')` } }),
      el('div', { class: `backdrop ${blur ? 'blur' : ''}`, style: { backgroundImage: `url('${bg}')` } }),
      el('div', { class: 'scrim' }),
      el('div', { class: `detail-body ${posterCls === 'wide' ? 'wide-art' : ''}` },
        el('div', { class: `detail-poster ${posterCls}` }, img(posterUrl, { alt: `${title} artwork` })),
        el('div', { class: 'detail-main' },
          eyebrow,
          el('h1', {}, title),
          kind === 'recording' && it.subtitle ? el('p', { class: 'tagline' }, it.subtitle) : m.tagline ? el('p', { class: 'tagline' }, m.tagline) : null,
          chips,
          el('div', { class: 'actions' }, playBtn, startOver, watchBtn, refreshMetaBtn(id, () => navigate())),
          el('div', { class: 'tags-row' }, tagEditor(it)))),
      el('div', { class: 'detail-content' }, left, right))));
  return () => ambientFrom(null);
}

/// Inline tag editor: chips with × + an input (Enter adds, commas split).
function tagEditor(it) {
  const id = encodeURIComponent(it.id);
  const box = el('div', { class: 'tag-editor', role: 'group', 'aria-label': 'Tags' });
  const input = el('input', { type: 'text', placeholder: 'Add tag…', 'aria-label': 'Add tag', autocomplete: 'off', spellcheck: 'false' });
  input.addEventListener('keydown', async (e) => {
    if (e.key !== 'Enter') return;
    const tags = input.value.split(',').map((s) => s.trim()).filter(Boolean);
    if (!tags.length) return;
    e.preventDefault();
    input.value = '';
    try {
      const r = await api.post(`/api/items/${id}/tags`, { tags });
      it.tags = [...new Set(Array.isArray(r.tags) ? r.tags : [...(it.tags || []), ...tags])];
      paint(); toast(`Tagged “${tags.join('”, “')}”`, 'beam');
    } catch (err) { toast(err.message, 'err'); }
  });
  const paint = () => set(box,
    icon('tag', 'faint'),
    [...new Set(it.tags || [])].map((t) => el('button', {
      class: 'chip tag-chip', type: 'button', title: `Remove tag “${t}”`, 'aria-label': `Remove tag ${t}`,
      onclick: async (e) => {
        const b = e.currentTarget; b.disabled = true;
        try {
          const r = await api.del(`/api/items/${id}/tags/${encodeURIComponent(t)}`);
          it.tags = Array.isArray(r.tags) ? r.tags : (it.tags || []).filter((x) => x !== t);
          paint();
        } catch (err) { b.disabled = false; toast(err.message, 'err'); }
      },
    }, t, el('span', { class: 'x' }, icon('x')))),
    input);
  paint();
  return box;
}

/// Admin: manual match → PUT /api/items/{id}/metadata
async function fixMatch(it) {
  const m = it.meta || {};
  const field = (label, input, hint) => el('div', { class: 'field' }, el('label', {}, label), input, hint ? el('div', { class: 'hint' }, hint) : null);
  const fTitle = el('input', { type: 'text', value: m.originalTitle || it.title || '' });
  const fYear = el('input', { type: 'number', min: '1880', max: '2100', value: it.year || '' });
  const fTmdb = el('input', { type: 'number', min: '1', value: m.tmdbId || '', placeholder: 'e.g. 949' });
  const body = el('div', { class: 'form', onkeydown: (e) => { if (e.key === 'Enter' && e.target.tagName === 'INPUT') { e.preventDefault(); body.closest('.modal-box')?.querySelector('.acts .primary')?.click(); } } },
    el('p', { class: 'muted', style: { margin: 0 } }, 'Override what the scanner guessed from the filename. A TMDB id wins over title + year.'),
    field('Title', fTitle),
    el('div', { class: 'form-row' }, field('Year', fYear), field('TMDB id', fTmdb, 'From the themoviedb.org URL')));
  const ok = await modal('Fix match', body, [{ label: 'Cancel', value: false }, { label: 'Save & re-match', value: true, class: 'primary' }]);
  if (!ok) return;
  const payload = {};
  if (fTitle.value.trim()) payload.title = fTitle.value.trim();
  if (fYear.value) payload.year = Number(fYear.value);
  if (fTmdb.value) payload.tmdbId = Number(fTmdb.value);
  try {
    await api.put(`/api/items/${encodeURIComponent(it.id)}/metadata`, payload);
    toast('Re-matched — refreshing', 'beam');
    navigate();
  } catch (e) { toast(e.message, 'err'); }
}
