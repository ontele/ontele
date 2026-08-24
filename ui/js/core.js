/* Copyright 2026 The Ontele Authors — SPDX-License-Identifier: Apache-2.0 */
/* Core runtime shared by every view: DOM helpers, API client, formatting,
   preferences, router, ambient color, modals, toasts, client capabilities. */

export const $ = (s, r = document) => r.querySelector(s);
export const $$ = (s, r = document) => [...r.querySelectorAll(s)];

// ---------- DOM ----------
export function el(tag, attrs = {}, ...kids) {
  const n = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null || v === false) continue;
    if (k === 'class') n.className = v;
    else if (k === 'html') n.innerHTML = v; // trusted, static markup only
    else if (k === 'style' && typeof v === 'object') Object.assign(n.style, v);
    else if (k.startsWith('on') && typeof v === 'function') n.addEventListener(k.slice(2), v);
    else if (k === 'dataset') Object.assign(n.dataset, v);
    else n.setAttribute(k, v === true ? '' : v);
  }
  append(n, kids);
  return n;
}
export function append(n, kids) {
  for (const kid of kids.flat(Infinity)) {
    if (kid == null || kid === false) continue;
    n.append(kid.nodeType ? kid : document.createTextNode(String(kid)));
  }
  return n;
}
export const svg = (paths, attrs = {}) =>
  el('span', { html: `<svg viewBox="0 0 24 24"${attrs.class ? ` class="${attrs.class}"` : ''}>${paths}</svg>` }).firstChild;
export const ICONS = {
  play: '<path d="M8 5.5v13l11-6.5z"/>',
  pause: '<path d="M7 5h3.6v14H7zM13.4 5H17v14h-3.6z"/>',
  prev: '<path d="M6 5h2.2v14H6zM18 5.5v13L9 12z"/>',
  next: '<path d="M15.8 5H18v14h-2.2zM6 5.5v13L15 12z"/>',
  check: '<path d="m5 12.5 4.5 4.5L19 7.5" class="thin"/>',
  copy: '<rect x="9" y="9" width="11" height="11" rx="2" class="thin"/><path d="M5 15V6a2 2 0 0 1 2-2h9" class="thin"/>',
  plus: '<path d="M12 5v14M5 12h14" class="thin"/>',
  x: '<path d="M6 6l12 12M18 6 6 18" class="thin"/>',
  more: '<circle cx="5" cy="12" r="1.8"/><circle cx="12" cy="12" r="1.8"/><circle cx="19" cy="12" r="1.8"/>',
  rec: '<circle cx="12" cy="12" r="6"/>',
  refresh: '<path d="M20 12a8 8 0 1 1-2.3-5.7M20 4v5h-5" class="thin"/>',
  shuffle: '<path d="M4 7h3l3.5 5L14 17h6M4 17h3l2-2.8M12.3 9.8 14 7h6M17 4l3 3-3 3M17 14l3 3-3 3" class="thin"/>',
  repeat: '<path d="M17 3l3 3-3 3M20 6H8a4 4 0 0 0-4 4v1M7 21l-3-3 3-3M4 18h12a4 4 0 0 0 4-4v-1" class="thin"/>',
  queue: '<path d="M4 7h12M4 12h12M4 17h8M19 14v6M16 17h6" class="thin"/>',
  volume: '<path d="M4 9v6h3.5L13 20V4L7.5 9z"/><path d="M16.5 8.5a5 5 0 0 1 0 7" class="thin"/>',
  mute: '<path d="M4 9v6h3.5L13 20V4L7.5 9z"/><path d="m16 9 5 6M21 9l-5 6" class="thin"/>',
  expand: '<path d="M4 9V4h5M20 9V4h-5M4 15v5h5M20 15v5h-5" class="thin"/>',
  chevronL: '<path d="M14.5 5.5 8 12l6.5 6.5" class="thin"/>',
  chevronR: '<path d="m9.5 5.5 6.5 6.5-6.5 6.5" class="thin"/>',
  film: '<path d="M4 5h16v14H4z" class="thin"/><path d="M4 9h16M8 5v14M16 5v14" class="thin"/>',
  tv: '<path d="M3 7h18v12H3z" class="thin"/><path d="m9 3 3 3 3-3" class="thin"/>',
  music: '<path d="M9 18.5a2.5 2.5 0 1 1-5 0 2.5 2.5 0 0 1 5 0zm11-2a2.5 2.5 0 1 1-5 0 2.5 2.5 0 0 1 5 0z"/><path d="M9 18.5V6l11-2.5v13" class="thin"/>',
  live: '<circle cx="12" cy="13" r="3.2"/><path d="M5 13a7 7 0 0 1 14 0M2.5 13a9.5 9.5 0 0 1 19 0" class="thin"/>',
  dvr: '<circle cx="12" cy="12" r="8" class="thin"/><circle cx="12" cy="12" r="3.4"/>',
  tag: '<path d="M3 12V4h8l9 9-8 8z" class="thin"/><circle cx="7.5" cy="8.5" r="1.4"/>',
  eye: '<path d="M2.5 12S6 5.5 12 5.5 21.5 12 21.5 12 18 18.5 12 18.5 2.5 12 2.5 12z" class="thin"/><circle cx="12" cy="12" r="3" class="thin"/>',
  trash: '<path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13" class="thin"/>',
  scissors: '<circle cx="6" cy="6" r="2.5" class="thin"/><circle cx="6" cy="18" r="2.5" class="thin"/><path d="M8.2 7.5 20 18M8.2 16.5 20 6" class="thin"/>',
  search: '<circle cx="11" cy="11" r="6.5" class="thin"/><path d="m16 16 4.5 4.5" class="thin"/>',
  info: '<circle cx="12" cy="12" r="9" class="thin"/><path d="M12 11v6M12 7.5v.5" class="thin"/>',
  star: '<path d="m12 3.5 2.6 5.6 6.1.7-4.5 4.2 1.2 6-5.4-3-5.4 3 1.2-6L3.3 9.8l6.1-.7z"/>',
};
export const icon = (name, cls = '') => svg(ICONS[name] || ICONS.info, { class: cls });

// ---------- API ----------
export class ApiError extends Error {
  constructor(message, status) { super(message); this.status = status; }
}
async function req(method, url, body) {
  const res = await fetch(url, {
    method,
    headers: body ? { 'Content-Type': 'application/json' } : undefined,
    body: body ? JSON.stringify(body) : undefined,
    credentials: 'same-origin',
  });
  if (res.status === 204) return {};
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new ApiError(data.error || res.statusText || `HTTP ${res.status}`, res.status);
  return data;
}
export const api = {
  get: (u) => req('GET', u),
  post: (u, b) => req('POST', u, b || {}),
  put: (u, b) => req('PUT', u, b),
  del: (u) => req('DELETE', u),
};

// ---------- toasts / modal ----------
export function toast(msg, kind = '', ms = 2600) {
  const t = el('div', { class: `toast ${kind}` }, msg);
  $('#toasts').append(t);
  setTimeout(() => { t.style.opacity = '0'; t.style.transition = 'opacity .3s'; }, ms);
  setTimeout(() => t.remove(), ms + 400);
  return t;
}
export function modal(title, body, actions = []) {
  return new Promise((resolve) => {
    const root = $('#modal-root');
    const close = (v) => { wrap.remove(); document.removeEventListener('keydown', onKey); resolve(v); };
    const onKey = (e) => { if (e.key === 'Escape') close(null); };
    const wrap = el('div', { class: 'modal', onclick: (e) => { if (e.target === wrap) close(null); } },
      el('div', { class: 'modal-box', role: 'dialog', 'aria-modal': 'true' },
        el('h2', {}, title),
        body,
        el('div', { class: 'acts' },
          actions.map((a) => el('button', { class: `btn ${a.class || ''}`, onclick: () => close(a.value) }, a.label)))));
    root.append(wrap);
    document.addEventListener('keydown', onKey);
    const first = wrap.querySelector('input,button');
    first && first.focus();
  });
}
export async function confirm(title, text, okLabel = 'Delete') {
  return modal(title, el('p', { class: 'muted' }, text), [
    { label: 'Cancel', value: false },
    { label: okLabel, value: true, class: 'danger' },
  ]);
}

// ---------- formatting ----------
export function fmtTime(s) {
  if (!isFinite(s) || s < 0) s = 0;
  s = Math.floor(s);
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), sec = s % 60;
  return h ? `${h}:${String(m).padStart(2, '0')}:${String(sec).padStart(2, '0')}` : `${m}:${String(sec).padStart(2, '0')}`;
}
export const fmtMins = (s) => {
  if (s > 0 && s < 60) return `${Math.round(s)} s`;
  const m = Math.round(s / 60);
  return m >= 60 ? `${Math.floor(m / 60)}h ${m % 60 ? `${m % 60}m` : ''}`.trim() : `${m} min`;
};
export const fmtClock = (d) => new Date(d).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
export const fmtDate = (d) => new Date(d).toLocaleDateString([], { month: 'short', day: 'numeric' });
export const fmtDateLong = (d) => new Date(d).toLocaleDateString([], { weekday: 'short', month: 'short', day: 'numeric' });
export function fmtAgo(d) {
  const s = (Date.now() - new Date(d).getTime()) / 1000;
  if (s < 60) return 'just now';
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}
export const fmtBytes = (b) => {
  if (!b) return '';
  const u = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (b >= 1024 && i < u.length - 1) { b /= 1024; i++; }
  return `${b.toFixed(i >= 3 ? 1 : 0)} ${u[i]}`;
};
export const epCode = (it) => `S${String(it.season ?? 0).padStart(2, '0')}E${String(it.episode ?? 0).padStart(2, '0')}${it.episodeEnd ? `-${String(it.episodeEnd).padStart(2, '0')}` : ''}`;
export const resLabel = (h) => (!h ? '' : h >= 2000 ? '4K' : h >= 1400 ? '1440p' : h >= 1000 ? '1080p' : h >= 700 ? '720p' : `${h}p`);
export const escapeHtml = (s) => String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));

/// Human label pair {title, sub} for any item kind.
export function itemLabel(it) {
  if (it.kind === 'episode') return { title: it.show || it.title, sub: `${epCode(it)}${it.title ? ' · ' + it.title : ''}` };
  if (it.kind === 'recording') return { title: it.title, sub: it.subtitle || (it.start ? `${fmtDate(it.start)} · ${it.channel || ''}` : '') };
  if (it.kind === 'track') return { title: it.title, sub: [it.artist, it.album].filter(Boolean).join(' · ') };
  return { title: it.title, sub: it.year ? String(it.year) : (it.duration ? fmtMins(it.duration) : '') };
}

// ---------- prefs ----------
export const pref = {
  get: (k, d) => { try { const v = JSON.parse(localStorage.getItem('ontele.' + k)); return v ?? d; } catch { return d; } },
  set: (k, v) => localStorage.setItem('ontele.' + k, JSON.stringify(v)),
};

// ---------- appearance: theme (day / night / auto) + accent ----------
export function applyAppearance() {
  const t = pref.get('theme', 'auto');
  const dark = t === 'night' || (t !== 'day' && matchMedia('(prefers-color-scheme: dark)').matches);
  const r = document.documentElement;
  r.dataset.theme = dark ? 'dark' : 'light';
  const acc = pref.get('accent', 'amber');
  if (acc === 'amber') delete r.dataset.accent; else r.dataset.accent = acc;
  document.querySelector('meta[name="color-scheme"]')?.setAttribute('content', dark ? 'dark' : 'light');
}
applyAppearance();
matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => { if (pref.get('theme', 'auto') === 'auto') applyAppearance(); });

// ---------- client capabilities (for the server's playback decision) ----------
export function detectCaps() {
  const mse = window.MediaSource;
  const v = document.createElement('video');
  const can = (t) => (mse && mse.isTypeSupported(t)) || v.canPlayType(t) === 'probably' || v.canPlayType(t) === 'maybe';
  const video = [];
  if (can('video/mp4; codecs="avc1.640028"')) video.push('h264');
  if (can('video/mp4; codecs="hvc1.1.6.L153.B0"') || can('video/mp4; codecs="hev1.1.6.L153.B0"')) video.push('hevc');
  if (can('video/mp4; codecs="av01.0.08M.08"')) video.push('av1');
  if (can('video/webm; codecs="vp9"') || can('video/mp4; codecs="vp09.00.10.08"')) video.push('vp9');
  if (can('video/webm; codecs="vp8"')) video.push('vp8');
  const audio = [];
  if (can('audio/mp4; codecs="mp4a.40.2"')) audio.push('aac');
  if (can('audio/mpeg')) audio.push('mp3');
  if (can('audio/webm; codecs="opus"') || can('audio/mp4; codecs="opus"')) audio.push('opus');
  if (can('audio/webm; codecs="vorbis"')) audio.push('vorbis');
  if (can('audio/mp4; codecs="flac"') || v.canPlayType('audio/flac')) audio.push('flac');
  if (can('audio/mp4; codecs="ac-3"')) audio.push('ac3');
  if (can('audio/mp4; codecs="ec-3"')) audio.push('eac3');
  const containers = ['mp4'];
  if (v.canPlayType('video/webm')) containers.push('webm');
  if (v.canPlayType('video/x-matroska') || /Chrome|Chromium|Edg/.test(navigator.userAgent)) containers.push('mkv');
  const nativeHls = !!v.canPlayType('application/vnd.apple.mpegurl') && !(window.Hls && Hls.isSupported());
  return { video, audio, containers, hls: nativeHls ? 'native' : 'mse', maxHeight: Math.max(screen.height, screen.width) >= 2000 ? 2160 : 1080, surround: false };
}
export const CAPS = detectCaps();

// ---------- ambient color (the signature move) ----------
const ambientCanvas = document.createElement('canvas');
ambientCanvas.width = 24; ambientCanvas.height = 14;
const actx = ambientCanvas.getContext('2d', { willReadFrequently: true });
let ambientToken = 0;
/// Sample an image and bleed its dominant warm/cool tones into the background.
export function ambientFrom(url) {
  const my = ++ambientToken;
  if (!url) { setAmbient(null); return; }
  const img = new Image();
  img.crossOrigin = 'anonymous';
  img.onload = () => {
    if (my !== ambientToken) return;
    try {
      actx.drawImage(img, 0, 0, ambientCanvas.width, ambientCanvas.height);
      const d = actx.getImageData(0, 0, ambientCanvas.width, ambientCanvas.height).data;
      let ra = 0, ga = 0, ba = 0, na = 0, rb = 0, gb = 0, bb = 0, nb = 0;
      for (let i = 0; i < d.length; i += 4) {
        const r = d[i], g = d[i + 1], b = d[i + 2];
        const max = Math.max(r, g, b), min = Math.min(r, g, b);
        const sat = max === 0 ? 0 : (max - min) / max;
        if (max < 30 || sat < 0.12) continue; // skip near-black / grey
        const x = (i / 4) % ambientCanvas.width;
        if (x < ambientCanvas.width / 2) { ra += r; ga += g; ba += b; na++; } else { rb += r; gb += g; bb += b; nb++; }
      }
      const boost = (r, g, b, n) => {
        if (!n) return null;
        r /= n; g /= n; b /= n;
        const max = Math.max(r, g, b) || 1;
        const k = Math.min(1.6, 255 / max); // push toward vivid
        return `rgba(${Math.round(r * k)},${Math.round(g * k)},${Math.round(b * k)},.32)`;
      };
      setAmbient(boost(ra, ga, ba, na), boost(rb, gb, bb, nb));
    } catch { setAmbient(null); }
  };
  img.onerror = () => { if (my === ambientToken) setAmbient(null); };
  img.src = url;
}
export function setAmbient(a, b) {
  const root = document.documentElement.style;
  if (!a && !b) { root.removeProperty('--ambient-a'); root.removeProperty('--ambient-b'); return; }
  root.setProperty('--ambient-a', a || b);
  root.setProperty('--ambient-b', b || a);
}

// ---------- images ----------
export function img(src, attrs = {}) {
  const i = el('img', { loading: 'lazy', decoding: 'async', alt: '', ...attrs });
  i.addEventListener('load', () => i.classList.add('ready'));
  i.addEventListener('error', () => i.remove());
  i.src = src;
  return i;
}
export const artUrl = (id, type = 'poster', w) => `/api/img/${encodeURIComponent(id)}?type=${type}${w ? `&w=${w}` : ''}`;

// ---------- router ----------
const routes = new Map();
let currentCleanup = null;
let activeTransition = null;
export const view = $('#view');
export function route(name, handler) { routes.set(name, handler); }
export function go(hash) { location.hash = hash; }
const safeDecode = (s) => { try { return decodeURIComponent(s); } catch { return s; } }; // a stray '%' must not blank the app
export function currentRoute() {
  const [, r = 'home', ...rest] = location.hash.split('/');
  return { name: r, args: rest.map(safeDecode) };
}
let navSeq = 0;      // bumped per navigation; a handler that resolves after a newer one started is stale
let navSettled = 0;  // seq of the newest handler that has finished rendering
export async function navigate() {
  const { name, args } = currentRoute();
  const base = name.split('?')[0];
  $$('.rail a[data-nav]').forEach((a) => a.classList.toggle('active', a.dataset.nav === base || (base === 'show' && a.dataset.nav === 'shows') || (base === 'item' && false) || (['artist', 'album'].includes(base) && a.dataset.nav === 'music') || (base === 'guide' && a.dataset.nav === 'live')));
  const handler = routes.get(base) || routes.get('home');
  if (typeof currentCleanup === 'function') { try { currentCleanup(); } catch {} currentCleanup = null; }
  const params = new URLSearchParams(name.includes('?') ? name.split('?')[1] : (location.hash.split('?')[1] || ''));
  const seq = ++navSeq;
  const run = async () => {
    let cleanup;
    try { cleanup = await handler(...args, params); } catch (e) { console.error('route', base, e); }
    if (seq === navSeq) { currentCleanup = cleanup; navSettled = seq; return; }
    // Stale: a newer navigation started while this handler was awaiting. Release its timers/listeners
    // right away, and if it painted over an already-finished newer page, re-render the current route.
    if (typeof cleanup === 'function') { try { cleanup(); } catch {} }
    if (navSettled === navSeq) navigate();
  };
  if (document.startViewTransition && !matchMedia('(prefers-reduced-motion: reduce)').matches) {
    if (activeTransition) activeTransition.skipTransition();
    const t = (activeTransition = document.startViewTransition(run));
    t.ready.catch(() => {});
    t.finished.catch(() => {}).finally(() => { if (activeTransition === t) activeTransition = null; });
    await t.updateCallbackDone.catch(() => {});
  } else {
    await run();
  }
  window.scrollTo({ top: 0, behavior: 'instant' });
  view.focus({ preventScroll: true });
}

// ---------- misc ----------
export const debounce = (fn, ms) => { let t; return (...a) => { clearTimeout(t); t = setTimeout(() => fn(...a), ms); }; };
export function skeletons(n = 8, cls = '') {
  return el('div', { class: `row-strip ${cls}` }, Array.from({ length: n }, () => el('div', { class: `card skel ${cls}` }, el('div', { class: 'art' }))));
}
export function emptyState(title, text, iconName = 'film') {
  return el('div', { class: 'empty' }, icon(iconName), el('div', {}, el('b', {}, title), text));
}
export function busy(btn, fn, okMsg) {
  btn.disabled = true;
  return fn().then((r) => { okMsg && toast(okMsg, 'beam'); return r; }).catch((e) => toast(e.message, 'err')).finally(() => { btn.disabled = false; });
}
export function initials(name = '') {
  return name.split(/[\s@._-]+/).filter(Boolean).slice(0, 2).map((s) => s[0].toUpperCase()).join('') || '?';
}

/// Session-wide user info (filled by app.js on boot).
export const session = { user: null, authMode: 'proxy', version: '' };
