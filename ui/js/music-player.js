/* Copyright 2026 The Ontele Authors — SPDX-License-Identifier: Apache-2.0 */
/* Music: persistent dock (mini player), queue panel, full "Now playing"
   overlay, shuffle/repeat, Media Session, keyboard. A single <audio>
   element plays /stream/audio/{id}; the server picks direct vs transcode. */

import { $, el, append, icon, api, toast, fmtTime, pref, artUrl, img, ambientFrom, setAmbient, initials } from './core.js';

const dock = $('#dock');
const audio = new Audio();
audio.preload = 'auto';
audio.crossOrigin = 'anonymous';

const DOCK_H = '96px';
const REPEATS = ['off', 'all', 'one'];

const S = {
  queue: [], order: [], pos: -1, // order = play order over queue indices; pos indexes into order
  shuffle: pref.get('shuffle', false), repeat: pref.get('repeat', 'off'),
  volume: pref.get('musicVolume', 1), muted: pref.get('musicMuted', false),
  drag: null, raf: 0, failures: 0, np: null, queueEl: null, saved: null,
  token: 0,
  offset: 0, // seconds the current stream was started at (`?t=`): transcoded audio cannot byte-range seek
};
audio.volume = S.volume;
audio.muted = S.muted;

// ---------- DOM (built once) ----------
const U = {};
function buildDock() {
  U.art = el('div', { class: 'art', role: 'button', tabindex: '0', title: 'Now playing', 'aria-label': 'Open now playing',
    onclick: () => openNowPlaying(), onkeydown: (e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); openNowPlaying(); } } });
  U.title = el('b', {});
  U.sub = el('small', {});
  U.shuffle = btn('shuffle', 'Shuffle', () => setShuffle(!S.shuffle));
  U.prev = btn('prev', 'Previous', () => Music.prev());
  U.play = el('button', { class: 'icon-btn big', type: 'button', title: 'Play/Pause (Space)', onclick: () => Music.toggle() }, icon('play'));
  U.next = btn('next', 'Next', () => Music.next());
  U.repeat = btn('repeat', 'Repeat', () => setRepeat(REPEATS[(REPEATS.indexOf(S.repeat) + 1) % REPEATS.length]));
  U.cur = el('span', {}, '0:00');
  U.dur = el('span', {}, '0:00');
  U.scrub = makeScrub('dock');
  U.queueBtn = btn('queue', 'Queue (Q)', () => toggleQueue());
  U.mute = btn(S.muted ? 'mute' : 'volume', 'Mute', () => setMuted(!audio.muted));
  U.vol = el('input', { type: 'range', min: '0', max: '1', step: '0.02', value: String(S.volume), 'aria-label': 'Volume',
    oninput: () => { audio.volume = +U.vol.value; setMuted(false); pref.set('musicVolume', audio.volume); } });
  U.expand = btn('expand', 'Now playing', () => openNowPlaying());
  U.close = btn('x', 'Stop', () => Music.stop());
  U.mini = el('i');
  dock.replaceChildren(
    el('div', { class: 'mini', 'aria-hidden': 'true' }, U.mini),
    el('div', { class: 'now' }, U.art, el('div', { class: 't' }, U.title, U.sub)),
    el('div', { class: 'center' },
      el('div', { class: 'transport' }, U.shuffle, U.prev, U.play, U.next, U.repeat),
      el('div', { class: 'bar' }, U.cur, U.scrub.el, U.dur)),
    el('div', { class: 'right' }, U.queueBtn, el('div', { class: 'vol' }, U.mute, U.vol), U.expand, U.close));
  paintModes();
}
const btn = (ic, title, onclick) => el('button', { class: 'icon-btn', type: 'button', title, 'aria-label': title, onclick }, icon(ic));

/// A .scrub bar (same classes as the video player) with pointer-capture drag and keyboard.
function makeScrub(name) {
  const fill = el('div', { class: 'scrub-fill' }), thumb = el('div', { class: 'scrub-thumb' });
  const root = el('div', { class: 'scrub', role: 'slider', tabindex: '0', 'aria-label': 'Seek', 'aria-valuemin': '0' }, el('div', { class: 'scrub-buffer' }), fill, thumb);
  let rect = null;
  const frac = (x) => Math.min(1, Math.max(0, (x - rect.left) / rect.width));
  root.addEventListener('pointerdown', (e) => {
    if (e.button !== 0 || !duration()) return;
    e.preventDefault();
    rect = root.getBoundingClientRect();
    try { root.setPointerCapture(e.pointerId); } catch {}
    root.classList.add('drag');
    S.drag = { name, t: frac(e.clientX) * duration() };
    paintTime(S.drag.t);
  });
  root.addEventListener('pointermove', (e) => { if (S.drag?.name === name) { S.drag.t = frac(e.clientX) * duration(); paintTime(S.drag.t); } });
  const end = (e) => {
    if (S.drag?.name !== name) return;
    const t = S.drag.t; S.drag = null;
    root.classList.remove('drag');
    try { root.releasePointerCapture(e.pointerId); } catch {}
    seek(t);
  };
  root.addEventListener('pointerup', end);
  root.addEventListener('pointercancel', end);
  root.addEventListener('keydown', (e) => {
    if (e.key === 'ArrowLeft') { e.preventDefault(); seek(pos() - 5); }
    if (e.key === 'ArrowRight') { e.preventDefault(); seek(pos() + 5); }
  });
  return { el: root, fill, thumb };
}

// ---------- public API ----------
export const Music = {
  /// Play a track. With a queue it plays inside that queue; otherwise inside its album.
  async playTrack(track, opts = {}) {
    if (!track) return;
    if (opts.queue?.length) {
      const i = opts.index ?? opts.queue.findIndex((t) => t.id === track.id);
      return Music.playQueue(opts.queue, i < 0 ? 0 : i);
    }
    if (track.albumId) {
      try {
        const r = await api.get(`/api/music/albums/${encodeURIComponent(track.albumId)}`);
        const i = (r.tracks || []).findIndex((t) => t.id === track.id);
        if (i >= 0) return Music.playQueue(r.tracks, i);
      } catch {}
    }
    return Music.playQueue([track], 0);
  },
  async playAlbum(albumId, opts = {}) {
    try {
      const r = await api.get(`/api/music/albums/${encodeURIComponent(albumId)}`);
      if (!r.tracks?.length) return toast('Album has no tracks', 'err');
      if (opts.shuffle) { setShuffle(true, true); return Music.playQueue(r.tracks, Math.floor(Math.random() * r.tracks.length)); }
      return Music.playQueue(r.tracks, 0);
    } catch (e) { toast(e.message, 'err'); }
  },
  playQueue(tracks, index = 0) {
    if (!tracks?.length) return;
    S.queue = tracks.slice();
    S.failures = 0;
    reorder(index);
    showDock();
    load(true);
    renderQueue();
  },
  /// Append tracks to the end of the queue (starts playing if idle).
  enqueue(tracks) {
    if (!tracks?.length) return;
    if (!S.queue.length) return Music.playQueue(tracks, 0);
    const start = S.queue.length;
    S.queue.push(...tracks);
    const extra = tracks.map((_, i) => start + i);
    if (S.shuffle) for (let i = extra.length - 1; i > 0; i--) { const j = Math.floor(Math.random() * (i + 1)); [extra[i], extra[j]] = [extra[j], extra[i]]; }
    S.order.push(...extra);
    renderQueue();
    toast(`Added ${tracks.length} track${tracks.length === 1 ? '' : 's'} to queue`, 'beam', 1500);
  },
  toggle() {
    if (!current()) return;
    if (audio.paused) audio.play().catch(() => {}); else audio.pause();
  },
  next() { step(1, true); },
  prev() {
    if (!current()) return;
    if (pos() > 3 || S.pos <= 0) { seek(0); return; }
    step(-1, true);
  },
  current,
  isPlaying: () => !!current() && !audio.paused,
  queue: () => ({ tracks: S.queue, order: S.order, pos: S.pos }),
  stop,
  openNowPlaying, toggleQueue,
  setShuffle, setRepeat,
  state: () => ({ shuffle: S.shuffle, repeat: S.repeat }),
};
function current() { return S.pos >= 0 && S.pos < S.order.length ? S.queue[S.order[S.pos]] : null; }
/// Position in the track (stream offset + element time).
const pos = () => S.offset + (audio.currentTime || 0);
const duration = () => (isFinite(audio.duration) && audio.duration > 0 ? S.offset + audio.duration : (current()?.duration || 0));

// ---------- queue order ----------
function reorder(startIndex) {
  const n = S.queue.length;
  let order = Array.from({ length: n }, (_, i) => i);
  if (S.shuffle) {
    order = order.filter((i) => i !== startIndex);
    for (let i = order.length - 1; i > 0; i--) { const j = Math.floor(Math.random() * (i + 1)); [order[i], order[j]] = [order[j], order[i]]; }
    order.unshift(startIndex);
    S.pos = 0;
  } else S.pos = Math.max(0, Math.min(n - 1, startIndex));
  S.order = order;
}
function step(dir, manual) {
  if (!S.queue.length) return;
  let p = S.pos + dir;
  if (p >= S.order.length) {
    if (S.repeat !== 'all' && !manual) { finish(); return; }
    if (S.shuffle) reorder(S.order[Math.floor(Math.random() * S.order.length)]); // fresh shuffle each lap
    p = 0;
  }
  if (p < 0) p = S.order.length - 1;
  S.pos = p;
  load(true);
  renderQueue();
}
function finish() {
  audio.pause();
  if (S.offset) load(false, 0); // stream was started mid-track: rewind means a fresh stream
  else try { audio.currentTime = 0; } catch {}
  paintPlay();
  paintTime(0);
}

// ---------- loading / playback ----------
function load(play, at = 0) {
  const t = current();
  if (!t) return;
  const my = ++S.token;
  S.offset = at > 0 ? at : 0;
  audio.src = `/stream/audio/${encodeURIComponent(t.id)}${S.offset ? `?t=${S.offset.toFixed(2)}` : ''}`;
  audio.load();
  if (play) audio.play().catch((e) => { if (my === S.token && e?.name !== 'AbortError') toast('Tap play to start audio', '', 2000); });
  paintTrack();
  paintTime(S.offset);
  mediaSession(t);
  emit();
}
function seek(t) {
  const d = duration();
  if (!d) return;
  t = Math.max(0, Math.min(d - 0.1, t));
  const rel = t - S.offset;
  let ok = false;
  try { const sk = audio.seekable; for (let i = 0; i < sk.length; i++) if (rel >= sk.start(i) && rel <= sk.end(i)) ok = true; } catch {}
  if (ok && rel >= 0) { try { audio.currentTime = rel; } catch {} }
  else load(!audio.paused, t); // outside the seekable window (transcoded stream): restart at `t`
  paintTime(t);
}
function stop() {
  S.token++;
  S.offset = 0;
  audio.pause();
  audio.removeAttribute('src');
  audio.load();
  S.queue = []; S.order = []; S.pos = -1;
  closeNowPlaying();
  closeQueue();
  hideDock();
  document.title = 'Ontele';
  if ('mediaSession' in navigator) { try { navigator.mediaSession.metadata = null; navigator.mediaSession.playbackState = 'none'; } catch {} }
  emit();
}
function setShuffle(on, quiet) {
  S.shuffle = !!on;
  pref.set('shuffle', S.shuffle);
  if (S.queue.length) {
    const curIdx = S.order[S.pos];
    reorder(curIdx);
    renderQueue();
  }
  paintModes();
  if (!quiet) toast(S.shuffle ? 'Shuffle on' : 'Shuffle off', '', 1200);
}
function setRepeat(mode) {
  S.repeat = REPEATS.includes(mode) ? mode : 'off';
  pref.set('repeat', S.repeat);
  audio.loop = S.repeat === 'one';
  paintModes();
  toast(S.repeat === 'off' ? 'Repeat off' : S.repeat === 'all' ? 'Repeat queue' : 'Repeat track', '', 1200);
}
function setMuted(m) {
  audio.muted = !!m;
  pref.set('musicMuted', audio.muted);
  U.mute.replaceChildren(icon(audio.muted || audio.volume === 0 ? 'mute' : 'volume'));
  if (S.np) S.np.mute.replaceChildren(icon(audio.muted || audio.volume === 0 ? 'mute' : 'volume'));
}

// ---------- painting ----------
function showDock() {
  if (!U.art) buildDock();
  dock.hidden = false;
  document.documentElement.style.setProperty('--dock', DOCK_H);
}
function hideDock() {
  dock.hidden = true;
  document.documentElement.style.removeProperty('--dock');
}
const artKey = (t) => (t.albumId ? `album:${t.albumId}` : t.id);
function paintTrack() {
  const t = current();
  if (!t || !U.art) return;
  U.art.replaceChildren(img(artUrl(artKey(t), 'poster', 160), { alt: t.album || '' }));
  U.title.textContent = t.title || 'Untitled';
  U.sub.replaceChildren();
  append(U.sub, [
    t.artist ? el('a', { href: `#/artist/${encodeURIComponent(t.albumArtist || t.artist)}`, onclick: (e) => e.stopPropagation() }, t.artist) : null,
    t.artist && t.album ? ' · ' : null,
    t.album ? (t.albumId ? el('a', { href: `#/album/${encodeURIComponent(t.albumId)}`, onclick: (e) => e.stopPropagation() }, t.album) : el('span', {}, t.album)) : null]);
  U.dur.textContent = fmtTime(duration());
  document.title = `${t.title} · ${t.artist || 'Ontele'}`;
  if (S.np) paintNowPlaying();
  paintPlay();
}
function paintTime(t) {
  const d = duration();
  const pct = d ? Math.min(100, Math.max(0, (t / d) * 100)) : 0;
  for (const sc of [U.scrub, S.np?.scrub]) {
    if (!sc) continue;
    sc.fill.style.width = `${pct}%`;
    sc.thumb.style.left = `${pct}%`;
    sc.el.setAttribute('aria-valuenow', Math.round(t));
    sc.el.setAttribute('aria-valuemax', Math.round(d));
    sc.el.setAttribute('aria-valuetext', `${fmtTime(t)} of ${fmtTime(d)}`);
  }
  U.cur.textContent = fmtTime(t);
  U.mini.style.width = `${pct}%`;
  if (d) U.dur.textContent = fmtTime(d);
  if (S.np) { S.np.cur.textContent = fmtTime(t); S.np.dur.textContent = fmtTime(d); }
}
function paintPlay() {
  const playing = !audio.paused && !!current();
  U.play.replaceChildren(icon(playing ? 'pause' : 'play'));
  U.play.title = playing ? 'Pause (Space)' : 'Play (Space)';
  if (S.np) S.np.play.replaceChildren(icon(playing ? 'pause' : 'play'));
  dock.classList.toggle('playing', playing);
  if ('mediaSession' in navigator) { try { navigator.mediaSession.playbackState = playing ? 'playing' : 'paused'; } catch {} }
  emit();
}
function paintModes() {
  for (const set of [U, S.np]) {
    if (!set?.shuffle) continue;
    set.shuffle.classList.toggle('on', S.shuffle);
    set.shuffle.setAttribute('aria-pressed', String(S.shuffle));
    set.repeat.classList.toggle('on', S.repeat !== 'off');
    set.repeat.classList.toggle('one', S.repeat === 'one');
    set.repeat.title = S.repeat === 'one' ? 'Repeat: track' : S.repeat === 'all' ? 'Repeat: queue' : 'Repeat: off';
  }
}
function schedule() {
  if (S.raf) return;
  S.raf = requestAnimationFrame(() => { S.raf = 0; if (!S.drag) paintTime(pos()); });
}
function emit() {
  const t = current();
  document.dispatchEvent(new CustomEvent('ontele:music', { detail: { track: t, id: t?.id || null, playing: !!t && !audio.paused, pos: S.pos, queue: S.queue } }));
}

// ---------- queue panel ----------
function toggleQueue() { if (S.queueEl) closeQueue(); else openQueue(); }
function closeQueue() { S.queueEl?.remove(); S.queueEl = null; U.queueBtn?.classList.remove('on'); }
function openQueue() {
  if (!S.queue.length) return;
  S.queueEl = el('div', { class: 'queue glass', role: 'dialog', 'aria-label': 'Queue' });
  document.body.append(S.queueEl);
  U.queueBtn.classList.add('on');
  renderQueue();
}
function renderQueue() {
  if (!S.queueEl) return;
  const total = S.queue.reduce((a, t) => a + (t.duration || 0), 0);
  const rows = S.order.map((qi, p) => {
    const t = S.queue[qi];
    const isCur = p === S.pos;
    const num = isCur && !audio.paused ? el('span', { class: 'eq' }, el('i'), el('i'), el('i')) : String(p + 1);
    return el('div', { class: `track ${isCur ? 'playing' : ''}`, role: 'button', tabindex: '0', dataset: { p: String(p) },
      onclick: () => { S.pos = p; load(true); renderQueue(); },
      onkeydown: (e) => { if (e.key === 'Enter') { S.pos = p; load(true); renderQueue(); } } },
      el('span', { class: 'n' }, num),
      el('div', { class: 't' }, el('b', {}, t.title), el('small', {}, [t.artist, t.album].filter(Boolean).join(' · '))),
      el('span', { class: 'd' }, fmtTime(t.duration || 0)),
      el('button', { class: 'icon-btn rm', type: 'button', title: 'Remove from queue', 'aria-label': 'Remove', onclick: (e) => { e.stopPropagation(); removeAt(p); } }, icon('x')));
  });
  S.queueEl.replaceChildren(
    el('div', { class: 'q-head' },
      el('h3', {}, 'Queue ', el('small', {}, `${S.queue.length} tracks · ${fmtTime(total)}`)),
      el('button', { class: 'btn tiny', type: 'button', onclick: () => Music.stop() }, 'Clear')),
    el('div', { class: 'tracks' }, rows));
  const cur = S.queueEl.querySelector('.track.playing');
  if (cur) cur.scrollIntoView({ block: 'nearest' });
}
function removeAt(p) {
  const qi = S.order[p];
  const wasCur = p === S.pos;
  S.queue.splice(qi, 1);
  S.order = S.order.filter((x) => x !== qi).map((x) => (x > qi ? x - 1 : x));
  if (!S.queue.length) return stop();
  if (p < S.pos) S.pos--;
  else if (wasCur) { if (S.pos >= S.order.length) S.pos = 0; load(!audio.paused); }
  renderQueue();
}

// ---------- now playing overlay ----------
function openNowPlaying() {
  const t = current();
  if (!t || S.np) return;
  const scrub = makeScrub('np');
  const np = {
    scrub,
    cur: el('span', {}, '0:00'), dur: el('span', {}, '0:00'),
    shuffle: btn('shuffle', 'Shuffle', () => setShuffle(!S.shuffle)),
    prev: btn('prev', 'Previous', () => Music.prev()),
    play: el('button', { class: 'icon-btn big', type: 'button', title: 'Play/Pause', onclick: () => Music.toggle() }, icon('play')),
    next: btn('next', 'Next', () => Music.next()),
    repeat: btn('repeat', 'Repeat', () => setRepeat(REPEATS[(REPEATS.indexOf(S.repeat) + 1) % REPEATS.length])),
    mute: btn(audio.muted ? 'mute' : 'volume', 'Mute', () => setMuted(!audio.muted)),
    art: el('div', { class: 'art' }),
    bg: el('div', { class: 'bg' }),
    title: el('h1', {}), artist: el('div', { class: 'artist' }), upnext: el('div', { class: 'np-next' }),
  };
  np.vol = el('input', { type: 'range', min: '0', max: '1', step: '0.02', value: String(audio.volume), 'aria-label': 'Volume',
    oninput: () => { audio.volume = +np.vol.value; U.vol.value = np.vol.value; setMuted(false); pref.set('musicVolume', audio.volume); } });
  np.root = el('div', { class: 'nowplaying', role: 'dialog', 'aria-modal': 'true', 'aria-label': 'Now playing',
    onclick: (e) => { if (e.target === np.root) closeNowPlaying(); } },
    np.bg,
    el('button', { class: 'icon-btn close', type: 'button', title: 'Close (Esc)', 'aria-label': 'Close', onclick: () => closeNowPlaying() }, icon('x')),
    el('div', { class: 'card-np' },
      np.art,
      el('div', { class: 'np-body' },
        el('span', { class: 'eyebrow' }, 'Now playing'),
        np.title, np.artist,
        el('div', { class: 'np-bar' }, np.cur, scrub.el, np.dur),
        el('div', { class: 'transport' }, np.shuffle, np.prev, np.play, np.next, np.repeat, el('span', { class: 'flex' }),
          el('div', { class: 'vol' }, np.mute, np.vol)),
        np.upnext)));
  S.np = np;
  closeQueue();
  const cs = getComputedStyle(document.documentElement);
  S.saved = [cs.getPropertyValue('--ambient-a').trim() || null, cs.getPropertyValue('--ambient-b').trim() || null,
    document.documentElement.style.getPropertyValue('--ambient-a') === ''];
  document.body.append(np.root);
  document.body.style.overflow = 'hidden';
  paintNowPlaying();
  paintModes();
  paintTime(pos());
  paintPlay();
  np.play.focus({ preventScroll: true });
}
function paintNowPlaying() {
  const np = S.np, t = current();
  if (!np || !t) return;
  const art = artUrl(artKey(t), 'poster', 800);
  np.art.replaceChildren(el('div', { class: 'fallback' }, initials(t.album || t.title)), img(art, { alt: t.album || '' }));
  np.bg.style.backgroundImage = `url("${art}")`;
  np.title.textContent = t.title || 'Untitled';
  np.artist.replaceChildren();
  append(np.artist, [
    t.artist ? el('a', { href: `#/artist/${encodeURIComponent(t.albumArtist || t.artist)}`, onclick: () => closeNowPlaying() }, t.artist) : null,
    t.artist && t.album ? el('span', { class: 'faint' }, '  ·  ') : null,
    t.album ? (t.albumId ? el('a', { href: `#/album/${encodeURIComponent(t.albumId)}`, onclick: () => closeNowPlaying() }, t.album) : el('span', {}, t.album)) : null]);
  const nx = S.pos + 1 < S.order.length ? S.queue[S.order[S.pos + 1]] : (S.repeat === 'all' ? S.queue[S.order[0]] : null);
  np.upnext.replaceChildren(...(nx ? [el('span', { class: 'faint' }, 'Up next · '), el('b', {}, nx.title), el('span', { class: 'faint' }, nx.artist ? ` — ${nx.artist}` : '')] : [el('span', { class: 'faint' }, 'End of queue')]));
  ambientFrom(artUrl(artKey(t), 'backdrop', 640));
}
function closeNowPlaying() {
  if (!S.np) return;
  S.np.root.remove();
  S.np = null;
  document.body.style.overflow = '';
  // restore the page's ambient colour
  if (S.saved) { if (S.saved[2]) setAmbient(null); else setAmbient(S.saved[0], S.saved[1]); S.saved = null; }
}

// ---------- media session ----------
function mediaSession(t) {
  if (!('mediaSession' in navigator)) return;
  try {
    navigator.mediaSession.metadata = new MediaMetadata({
      title: t.title || 'Untitled', artist: t.artist || '', album: t.album || '',
      artwork: [256, 512].map((s) => ({ src: artUrl(artKey(t), 'poster', s), sizes: `${s}x${s}`, type: 'image/jpeg' })),
    });
  } catch {}
  claimMediaSession();
}
function claimMediaSession() {
  if (!('mediaSession' in navigator)) return;
  const set = (a, fn) => { try { navigator.mediaSession.setActionHandler(a, fn); } catch {} };
  set('play', () => Music.toggle());
  set('pause', () => Music.toggle());
  set('previoustrack', () => Music.prev());
  set('nexttrack', () => Music.next());
  set('seekto', (d) => { if (d.seekTime != null) seek(d.seekTime); });
  set('seekbackward', (d) => seek(pos() - (d.seekOffset || 10)));
  set('seekforward', (d) => seek(pos() + (d.seekOffset || 10)));
  set('stop', () => stop());
}

// ---------- audio events ----------
audio.addEventListener('play', () => { S.failures = 0; paintPlay(); renderQueue(); });
audio.addEventListener('pause', () => { paintPlay(); renderQueue(); });
audio.addEventListener('timeupdate', schedule);
audio.addEventListener('durationchange', () => { paintTime(pos()); if (U.dur) U.dur.textContent = fmtTime(duration()); });
audio.addEventListener('loadedmetadata', () => paintTime(pos()));
audio.addEventListener('volumechange', () => { if (U.vol) U.vol.value = String(audio.volume); if (S.np) S.np.vol.value = String(audio.volume); });
audio.addEventListener('ended', () => {
  if (S.repeat === 'one') { seek(0); audio.play().catch(() => {}); return; }
  step(1, false);
});
audio.addEventListener('error', () => {
  const t = current();
  if (!t || !audio.getAttribute('src')) return;
  S.failures++;
  toast(`Can't play “${t.title}”`, 'err');
  if (S.failures >= S.queue.length) { finish(); return; }
  step(1, false);
});

// ---------- keyboard / other players ----------
document.addEventListener('keydown', (e) => {
  if (!current() || e.defaultPrevented) return;
  const tag = e.target.tagName;
  const typing = ['INPUT', 'TEXTAREA', 'SELECT'].includes(tag) || e.target.isContentEditable;
  if (!$('#player').hidden || !$('#palette').hidden || $('#modal-root').childElementCount) return;
  if (e.key === 'Escape') {
    if (S.np) { e.preventDefault(); closeNowPlaying(); }
    else if (S.queueEl) { e.preventDefault(); closeQueue(); }
    return;
  }
  if (typing) return;
  if (e.metaKey || e.ctrlKey || e.altKey) return;
  if (e.key === ' ' && tag !== 'BUTTON') { e.preventDefault(); Music.toggle(); }
  else if (e.key === 'q' || e.key === 'Q') { e.preventDefault(); toggleQueue(); }
  else if (S.np && e.key === 'ArrowLeft') { e.preventDefault(); seek(pos() - 5); }
  else if (S.np && e.key === 'ArrowRight') { e.preventDefault(); seek(pos() + 5); }
  else if (S.np && (e.key === 'n' || e.key === 'N')) Music.next();
  else if (S.np && (e.key === 'p' || e.key === 'P')) Music.prev();
});
// The video player pauses music when it opens and hands Media Session back when it closes.
document.addEventListener('ontele:player', (e) => {
  if (e.detail?.open) { if (!audio.paused) audio.pause(); closeNowPlaying(); }
  else if (current()) { mediaSession(current()); const t = current(); document.title = `${t.title} · ${t.artist || 'Ontele'}`; }
});
// Close the floating queue when clicking elsewhere.
document.addEventListener('pointerdown', (e) => {
  if (S.queueEl && !S.queueEl.contains(e.target) && !dock.contains(e.target)) closeQueue();
});
window.addEventListener('hashchange', () => { closeQueue(); closeNowPlaying(); });
