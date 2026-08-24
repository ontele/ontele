/* Copyright 2026 The Ontele Authors — SPDX-License-Identifier: Apache-2.0 */
/* Home: hero + rows (continue, up next, recordings, new movies, new episodes, new music). */

import { el, api, view, ambientFrom, artUrl, go, icon, skeletons, emptyState } from '../core.js';
import { card, albumCard, strip, hero } from '../cards.js';

const backdropKey = (it) => (it.kind === 'episode' && it.show ? `show:${it.show}` : it.id);

export async function render() {
  // skeleton: a shimmering hero + two strips, painted before the request returns
  view.replaceChildren(el('div', { class: 'page home' },
    el('div', { class: 'hero skel-hero', 'aria-hidden': 'true' }),
    skeletons(6, 'wide'), skeletons(10)));

  let h;
  try { h = await api.get('/api/home'); } catch (e) {
    view.replaceChildren(el('div', { class: 'page' }, emptyState('Couldn’t load your library', e.message || 'The server did not answer.', 'info')));
    return;
  }
  const cont = h.continue || [], up = h.upNext || [], recs = h.recordings || [], movies = h.movies || [], eps = h.episodes || [], albums = h.albums || [];
  const empty = !cont.length && !up.length && !recs.length && !movies.length && !eps.length && !albums.length;

  if (empty) {
    ambientFrom(null);
    view.replaceChildren(welcome());
    return () => ambientFrom(null);
  }

  const heroItem = cont[0] || up[0] || movies[0] || recs[0];
  const eyebrow = cont[0] ? 'Continue watching' : up[0] ? 'Up next' : movies[0] ? 'Recently added' : 'Recorded off-air';
  const bg = artUrl(backdropKey(heroItem), 'backdrop', 1280);
  ambientFrom(bg);

  const page = el('div', { class: 'page home' },
    hero(heroItem, { eyebrow }),
    el('div', { class: 'home-rows' },
      strip('Continue watching', cont.map((it) => card(it)), { shape: 'mixed' }),
      strip('Up next', up.map((it) => card(it, { shape: 'wide' })), { shape: 'wide' }),
      strip('Recorded off-air', recs.map((it) => card(it, { shape: 'wide' })), { shape: 'wide', more: '#/dvr' }),
      strip('Recently added movies', movies.map((it) => card(it, { shape: 'poster' })), { more: '#/movies' }),
      strip('New episodes', eps.map((it) => card(it, { shape: 'wide' })), { shape: 'wide' }),
      strip('New in music', albums.map((al) => albumCard(al)), { shape: 'square', more: '#/music' })));
  view.replaceChildren(page);
  return () => ambientFrom(null);
}

/// First-run state: nothing scanned yet. Warm, not apologetic.
function welcome() {
  const glyph = el('div', { class: 'glyph', 'aria-hidden': 'true', html: '<svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="4.4" fill="currentColor"/><g stroke="currentColor" stroke-width="1.6" stroke-linecap="round" fill="none"><path d="M12 2.2v3"/><path d="M12 18.8v3"/><path d="M2.2 12h3"/><path d="M18.8 12h3"/><path d="M5.1 5.1l2.1 2.1"/><path d="M16.8 16.8l2.1 2.1"/><path d="M18.9 5.1l-2.1 2.1"/><path d="M7.2 16.8l-2.1 2.1"/></g></svg>' });
  return el('div', { class: 'page home' },
    el('div', { class: 'welcome' },
      el('div', { class: 'welcome-body' },
        glyph,
        el('span', { class: 'eyebrow' }, 'Welcome'),
        el('h1', {}, 'The projector is warm.'),
        el('p', {}, 'Point Ontele at the folders that hold your movies, shows and music and it will catalog them, fetch artwork and keep an eye out for new files. Or switch straight to your tuner and watch what’s on air right now.'),
        el('div', { class: 'hero-actions' },
          el('button', { class: 'btn primary', type: 'button', onclick: () => go('#/settings') }, icon('info'), 'Set up your library'),
          el('button', { class: 'btn', type: 'button', onclick: () => go('#/live') }, icon('live'), 'Watch live TV')),
        el('div', { class: 'hint' }, 'Tip: press ', el('kbd', {}, '/'), ' to search anywhere, or ', el('kbd', {}, 'g'), ' then ', el('kbd', {}, 'l'), ' to jump to Live.'))));
}
