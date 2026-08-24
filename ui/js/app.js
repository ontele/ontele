/* Copyright 2026 The Ontele Authors — SPDX-License-Identifier: Apache-2.0 */
/* Boot: session, routes, global shortcuts, scan indicator. */

import { $, api, el, icon, initials, navigate, route, session, toast, pref, modal } from './core.js';
import { openPalette } from './palette.js';
import * as home from './views/home.js';
import * as library from './views/library.js';
import * as music from './views/music.js';
import * as live from './views/live.js';
import * as dvr from './views/dvr.js';
import * as settings from './views/settings.js';
import { Player } from './player.js';
import { Music } from './music-player.js';

window.Player = Player;
window.Music = Music;

route('home', home.render);
route('movies', library.renderMovies);
route('shows', library.renderShows);
route('show', library.renderShow);
route('item', library.renderItem);
route('music', music.renderMusic);
route('artist', music.renderArtist);
route('album', music.renderAlbum);
route('live', live.renderLive);
route('guide', live.renderGuide);
route('dvr', dvr.render);
route('settings', settings.render);
route('activity', settings.renderActivity);

async function boot() {
  try {
    const me = await api.get('/api/me');
    session.user = me.user;
    session.authMode = me.authMode;
    session.version = me.version;
  } catch (e) {
    if (e.status === 401) {
      document.body.innerHTML = '<div class="page" style="padding:40vh 40px 0;text-align:center"><h1>Sign in required</h1><p class="muted">Ontele is behind an identity proxy. Reload to sign in.</p></div>';
      return;
    }
    toast(e.message, 'err');
  }
  const btn = $('#user-btn');
  const name = session.user?.name || session.user?.email || session.user?.subject || '?';
  btn.textContent = initials(name);
  btn.title = `${name}${session.user?.isAdmin ? ' · admin' : ''}`;
  btn.onclick = () => modal('Account',
    el('div', { class: 'kv' },
      el('b', {}, 'Signed in as'), el('span', {}, name),
      el('b', {}, 'Role'), el('span', {}, session.user?.isAdmin ? 'Administrator' : 'Member'),
      el('b', {}, 'Identity'), el('span', {}, session.authMode === 'proxy' ? 'OAuth2 proxy' : 'Local (no auth)'),
      el('b', {}, 'Ontele'), el('span', {}, `v${session.version}`)),
    [{ label: 'Close', value: true }]);

  window.addEventListener('hashchange', navigate);
  if (!location.hash) location.hash = '#/home'; // hashchange triggers the first render
  else await navigate();
  pollScan();
}

// ---- global shortcuts ----
document.addEventListener('keydown', (e) => {
  const typing = ['INPUT', 'TEXTAREA', 'SELECT'].includes(e.target.tagName) || e.target.isContentEditable;
  if ((e.key === 'k' && (e.metaKey || e.ctrlKey)) || (e.key === '/' && !typing && $('#player').hidden)) {
    e.preventDefault();
    openPalette();
  }
  if (e.key === 'Escape' && !$('#palette').hidden) return;
  if (typing || !$('#player').hidden) return;
  // g + key navigation (vim-ish)
  if (e.key === 'g') {
    const once = (ev) => {
      const map = { h: '#/home', m: '#/movies', s: '#/shows', u: '#/music', l: '#/live', d: '#/dvr', ',': '#/settings', g: '#/guide' };
      const typingNow = ['INPUT', 'TEXTAREA', 'SELECT'].includes(ev.target.tagName) || ev.target.isContentEditable;
      if (map[ev.key] && !typingNow && !(ev.metaKey || ev.ctrlKey || ev.altKey) && $('#player').hidden) { ev.preventDefault(); location.hash = map[ev.key]; }
      document.removeEventListener('keydown', once);
    };
    document.addEventListener('keydown', once, { once: true });
  }
});
$('#search-btn').onclick = () => openPalette();

// ---- scan indicator ----
let scanTimer;
async function pollScan() {
  clearTimeout(scanTimer);
  try {
    const s = await api.get('/api/scan/status');
    const pill = $('#scan-pill');
    if (s.scanning) {
      pill.hidden = false;
      pill.lastElementChild.textContent = `Scanning · ${s.probed}/${s.found}`;
      scanTimer = setTimeout(pollScan, 1500);
    } else {
      if (!pill.hidden) {
        pill.hidden = true;
        if (s.added || s.removed) {
          toast(`Library updated · +${s.added} / −${s.removed}`, 'beam');
          if (location.hash.startsWith('#/home') || location.hash.startsWith('#/movies') || location.hash.startsWith('#/shows')) navigate();
        }
      }
      scanTimer = setTimeout(pollScan, 20000);
    }
  } catch {
    scanTimer = setTimeout(pollScan, 30000);
  }
}
export const refreshScanPill = () => pollScan();

// first-run hint
if (!pref.get('seenHint', false)) {
  pref.set('seenHint', true);
  setTimeout(() => toast('Tip: press / to search, g then a letter to jump around', 'beam', 5000), 1500);
}

boot();
