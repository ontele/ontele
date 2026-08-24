/* Copyright 2026 The Ontele Authors — SPDX-License-Identifier: Apache-2.0 */
/* Full-screen video player: direct play or HLS sessions (hls.js / native),
   seek-by-restart outside the buffered window, ad-break skipping, up-next
   autoplay, sprite previews, text + burned subtitles, quality/audio/speed
   menus, Media Session, and the signature ambient light bleed. */

import { $, el, icon, api, toast, fmtTime, fmtDate, fmtClock, epCode, pref, CAPS, artUrl, navigate, img } from './core.js';

const root = $('#player'), video = $('#video'), ambient = $('#ambient');
const q = (id) => $('#p-' + id);
const U = {
  back: q('back'), eyebrow: q('eyebrow'), heading: q('heading'), mode: q('mode'), live: q('live'), center: q('center'),
  skip: q('skip'), skiptime: q('skiptime'), next: q('next'), scrub: q('scrub'), buffer: q('buffer'), fill: q('fill'),
  marks: q('marks'), chapters: q('chapters'), thumb: q('thumb'), preview: q('preview'), play: q('play'), r10: q('r10'),
  f30: q('f30'), mute: q('mute'), vol: q('vol'), cur: q('cur'), dur: q('dur'), chapter: q('chapter'), autoskip: q('autoskip'),
  subs: q('subs'), audio: q('audio'), settings: q('settings'), pip: q('pip'), fs: q('fs'), menu: q('menu'),
};
const controls = root.querySelector('.p-controls');
const actx = ambient.getContext('2d');
const IDLE_MS = 2800, KEEPALIVE_MS = 30000, PROGRESS_MS = 5000, AMBIENT_MS = 400, UPNEXT_AT = 30, COUNTDOWN_MS = 10000;
const LADDER = [2160, 1080, 720, 480, 360];
const SPEEDS = [0.75, 1, 1.25, 1.5, 2];
const RING = 2 * Math.PI * 8;

let S = null; // current session state; null when the player is closed

function fresh(item) {
  return {
    item, live: null, sid: null, url: '', offset: 0, mode: '', plan: null, hls: null, direct: false,
    quality: pref.get('quality', 'auto'), speed: 1, audio: null, burn: null, sub: null, subs: null, trackEl: null, trackUrl: null,
    sprites: null, sheet: null, breaks: [], chapters: [], fallback: false, hlsRecover: 0, skipped: null,
    upShown: false, upDismissed: false, upRaf: 0, upStart: 0, menu: null, dragging: false, dragT: 0, rect: null,
    timers: {}, raf: 0, pendingSeek: null, starting: false, ended: false, gen: 0,
  };
}

// ---------- public API ----------
export const Player = {
  async open(item, opts = {}) {
    if (!item || item.kind === 'track') return;
    if (S) await teardown();
    const my = (S = fresh({ ...item }));
    show();
    setTitle();
    // Fetch the detail shape (info, nextEpisode, breaks) unless we already have it.
    if (!my.item.info || (my.item.kind === 'episode' && !('nextEpisode' in my.item))) {
      try { Object.assign(my.item, await api.get(`/api/items/${encodeURIComponent(item.id)}`)); } catch {}
      if (S !== my) return;
    }
    setTitle();
    setupMarks();
    loadSprites(my);
    loadSubs(my);
    const w = my.item.watch;
    const from = opts.from != null ? opts.from : (w && !w.done && w.pos > 60 ? w.pos : 0);
    if (from > 0 && opts.from == null) toast(`Resuming from ${fmtTime(from)}`, 'beam', 1800);
    await start(from);
  },
  async openLive(channel, airing) {
    if (!channel) return;
    if (S) await teardown();
    const my = (S = fresh({ id: null, kind: 'live', title: airing?.title || channel.guideName, duration: 0 }));
    my.live = { channel, airing };
    my.quality = pref.get('liveQuality', 'auto');
    show();
    setTitle();
    await start(0);
  },
  close,
  isOpen: () => !!S,
  seek: (t) => seek(t),
};

// ---------- lifecycle ----------
function show() {
  root.hidden = false;
  root.classList.remove('idle', 'buffering');
  root.classList.toggle('live', !!S.live);
  document.body.style.overflow = 'hidden';
  U.live.hidden = !S.live;
  U.subs.classList.remove('on', 'dim');
  U.scrub.classList.toggle('disabled', !!S.live);
  U.scrub.setAttribute('aria-disabled', S.live ? 'true' : 'false');
  U.autoskip.parentElement.hidden = !!S.live || !(S.item.breaks?.length);
  U.autoskip.checked = pref.get('autoskip', true);
  U.audio.hidden = !!S.live;
  U.audio.classList.toggle('dim', (S.item.info?.audio || []).length < 2); // nothing to choose
  U.subs.hidden = !!S.live;
  U.mode.hidden = true;
  U.skip.hidden = true;
  U.next.hidden = true;
  U.menu.hidden = true;
  U.marks.replaceChildren();
  U.chapters.replaceChildren();
  U.chapter.textContent = '';
  U.fill.style.width = '0%';
  U.buffer.style.width = '0%';
  U.thumb.style.left = '0%';
  U.cur.textContent = '0:00';
  U.dur.textContent = S.item.duration ? fmtTime(S.item.duration) : '–:––';
  video.volume = pref.get('volume', 1);
  video.muted = pref.get('muted', false);
  U.vol.value = video.volume;
  paintVolume();
  paintPlay();
  root.focus({ preventScroll: true });
  wake();
  claimMediaSession();
  document.dispatchEvent(new CustomEvent('ontele:player', { detail: { open: true } }));
}

async function teardown() {
  const my = S;
  if (!my) return;
  saveProgress();
  for (const t of Object.values(my.timers)) { clearInterval(t); clearTimeout(t); }
  cancelAnimationFrame(my.raf);
  cancelAnimationFrame(my.upRaf);
  endSession(my);
  clearTrack(my);
  try { video.pause(); } catch {}
  video.removeAttribute('src');
  video.load();
  U.preview.hidden = true;
  S = null;
}

async function close() {
  if (!S) return;
  await teardown();
  root.hidden = true;
  root.classList.remove('idle', 'buffering');
  document.body.style.overflow = '';
  document.title = 'Ontele';
  if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
  if (document.pictureInPictureElement) document.exitPictureInPicture().catch(() => {});
  if (navigator.mediaSession) { try { navigator.mediaSession.metadata = null; navigator.mediaSession.playbackState = 'none'; } catch {} }
  document.dispatchEvent(new CustomEvent('ontele:player', { detail: { open: false } }));
  // let the final progress write land before the page beneath re-reads watch state
  await Promise.race([lastSaveRequest, new Promise((r) => setTimeout(r, 800))]);
  navigate(); // refresh watch chips on the page beneath
}

function endSession(my) {
  if (my.hls) { try { my.hls.destroy(); } catch {} my.hls = null; }
  if (my.sid) {
    fetch(`/api/stream/${encodeURIComponent(my.sid)}`, { method: 'DELETE', keepalive: true, credentials: 'same-origin' }).catch(() => {});
    my.sid = null;
  }
  clearInterval(my.timers.keepalive);
}

// ---------- titles / badges ----------
function setTitle() {
  const it = S.item;
  let eyebrow = '', heading = it.title || '';
  if (S.live) {
    const { channel, airing } = S.live;
    eyebrow = `${channel.guideNumber} · ${channel.guideName}${channel.hd ? ' HD' : ''}`;
    heading = airing?.title || 'Live TV';
    if (airing?.subtitle) heading += ` · ${airing.subtitle}`;
  } else if (it.kind === 'episode') {
    eyebrow = it.show || '';
    heading = `${epCode(it)} · ${it.title}`;
  } else if (it.kind === 'recording') {
    eyebrow = [it.channel, it.start ? `${fmtDate(it.start)} ${fmtClock(it.start)}` : ''].filter(Boolean).join(' · ');
    heading = it.subtitle ? `${it.title} · ${it.subtitle}` : it.title;
  } else {
    eyebrow = [it.year, it.meta?.genres?.slice(0, 2).join(', ')].filter(Boolean).join(' · ') || 'Movie';
  }
  U.eyebrow.textContent = eyebrow;
  U.heading.textContent = heading;
  document.title = `${heading} · Ontele`;
  if ('mediaSession' in navigator) {
    try {
      const artKey = S.live ? null : (it.kind === 'episode' && it.show ? `show:${it.show}` : it.id);
      navigator.mediaSession.metadata = new MediaMetadata({
        title: heading, artist: eyebrow, album: 'Ontele',
        artwork: artKey ? [{ src: artUrl(artKey, 'poster', 480), sizes: '480x720', type: 'image/jpeg' }] : [],
      });
    } catch {}
  }
}

function badge(r) {
  const h = r.plan?.height || (r.live ? 720 : 0);
  const label = r.mode === 'direct' ? 'Direct play' : r.mode === 'copy' ? 'Remux' : `Transcode${h ? ' ' + h + 'p' : ''}`;
  U.mode.textContent = label;
  U.mode.title = (r.plan?.reasons || []).join(' · ');
  U.mode.dataset.mode = r.mode;
  U.mode.hidden = false;
}

// ---------- stream sessions ----------
async function start(t, extra = {}) {
  const my = S;
  if (!my) return;
  my.starting = true;
  my.ended = false;
  const gen = ++my.gen; // a newer start() (seek / quality / audio change) supersedes this one
  root.classList.add('buffering');
  hideUpNext();
  my.upShown = false;
  endSession(my);
  const body = my.live
    ? { channel: my.live.channel.guideNumber, quality: my.quality === 'direct' ? 'auto' : my.quality, caps: CAPS }
    : { id: my.item.id, start: Math.max(0, t || 0), quality: my.quality, caps: CAPS };
  if (my.audio != null) body.audio = my.audio;
  if (my.burn != null) body.subtitle = `burn:${my.burn}`;
  Object.assign(body, extra);
  let r;
  try { r = await api.post('/api/stream/start', body); }
  catch (e) {
    if (S !== my || gen !== my.gen) return;
    root.classList.remove('buffering');
    my.starting = false;
    toast(e.message || 'Playback failed', 'err', 4000);
    if (e.status === 404 || e.status === 400) close();
    return;
  }
  if (S !== my || gen !== my.gen) {
    // superseded while the request was in flight: release the session we were just handed
    if (r.sessionId) fetch(`/api/stream/${encodeURIComponent(r.sessionId)}`, { method: 'DELETE', keepalive: true, credentials: 'same-origin' }).catch(() => {});
    return;
  }
  my.sid = r.sessionId || null;
  my.offset = r.offset || 0;
  my.mode = r.mode;
  my.plan = r.plan || null;
  my.direct = r.mode === 'direct';
  my.url = r.url;
  badge(r);
  attach(r.url, my.direct ? t : 0);
  if (my.sid) {
    my.timers.keepalive = setInterval(() => {
      if (!my.sid) return;
      api.post(`/api/stream/${encodeURIComponent(my.sid)}/keepalive`).catch((e) => { if (e.status === 410) toast('Stream session expired', 'err'); });
    }, KEEPALIVE_MS);
  }
  my.starting = false;
}

function attach(url, seekTo) {
  const my = S;
  const isM3u8 = /\.m3u8(\?|$)/i.test(url);
  if (my.hls) { try { my.hls.destroy(); } catch {} my.hls = null; }
  my.hlsRecover = 0;
  video.playbackRate = my.speed;
  if (isM3u8 && window.Hls && Hls.isSupported()) {
    const hls = (my.hls = new Hls({ maxBufferLength: 30, lowLatencyMode: false, backBufferLength: 60 }));
    hls.on(Hls.Events.ERROR, (_, data) => onHlsError(my, data));
    hls.on(Hls.Events.MANIFEST_PARSED, () => { if (S === my) video.play().catch(() => {}); });
    hls.loadSource(url);
    hls.attachMedia(video);
  } else if (isM3u8 && video.canPlayType('application/vnd.apple.mpegurl')) {
    video.src = url;
    video.play().catch(() => {});
  } else {
    // Plain file (direct play) — or a non-HLS url from the server.
    video.src = url;
    if (seekTo > 0) {
      const onMeta = () => { if (S === my) video.currentTime = seekTo; };
      video.addEventListener('loadedmetadata', onMeta, { once: true });
    }
    video.play().catch(() => {});
  }
  applyTextTrack(my);
}

function onHlsError(my, data) {
  if (S !== my || !data.fatal) return;
  my.hlsRecover++;
  if (my.hlsRecover > 3) {
    toast(`Stream error: ${data.details || data.type}`, 'err', 4000);
    return;
  }
  if (data.type === Hls.ErrorTypes.NETWORK_ERROR) my.hls.startLoad();
  else if (data.type === Hls.ErrorTypes.MEDIA_ERROR) my.hls.recoverMediaError();
  else { toast('Restarting stream…', 'beam'); start(now()); }
}

// ---------- time math ----------
const now = () => (S ? S.offset + (video.currentTime || 0) : 0);
function total() {
  if (!S) return 0;
  if (S.live) return 0;
  if (S.direct && isFinite(video.duration) && video.duration > 0) return video.duration;
  return S.item.duration || (isFinite(video.duration) ? S.offset + video.duration : 0);
}
function seek(t) {
  const my = S;
  if (!my || my.live) return;
  const dur = total();
  t = Math.max(0, Math.min(dur ? dur - 0.25 : t, t));
  if (my.skipped && t < my.skipped.start) my.skipped = null; // rewound past a skipped break: arm it again
  if (my.direct) { video.currentTime = t; return; }
  const rel = t - my.offset;
  let ok = false;
  try {
    const sk = video.seekable;
    for (let i = 0; i < sk.length; i++) if (rel >= sk.start(i) && rel <= sk.end(i)) ok = true;
  } catch {}
  if (ok && rel >= 0) video.currentTime = rel;
  else start(t); // restart the session at the target (-ss)
  paintScrub(t);
}
const skipBy = (d) => { if (S && !S.live) { seek(now() + d); flash(d < 0 ? 'prev' : 'next'); } };

// ---------- painting (rAF-throttled) ----------
function schedule() {
  if (!S || S.raf) return;
  S.raf = requestAnimationFrame(() => { if (S) { S.raf = 0; paint(); } });
}
function paint() {
  const my = S;
  const t = now(), dur = total();
  if (!my.dragging) paintScrub(t);
  U.cur.textContent = fmtTime(t);
  if (dur) U.dur.textContent = fmtTime(dur);
  try {
    const b = video.buffered;
    if (b.length && dur) U.buffer.style.width = `${Math.min(100, (my.offset + b.end(b.length - 1)) / dur * 100)}%`;
  } catch {}
}
/// Time-driven logic (chapters, ad breaks, up-next). Runs on every timeupdate — not rAF —
/// so skipping keeps working when the tab is hidden or the video is in PiP.
function tick() {
  const my = S;
  if (!my) return;
  const t = now(), dur = total();
  if (my.chapters.length) {
    const ch = my.chapters.find((c) => t >= c.start && t < c.end);
    const name = ch?.title || '';
    if (U.chapter.textContent !== name) U.chapter.textContent = name;
  }
  if (my.breaks.length && !my.starting) {
    const b = my.breaks.find((x) => t >= x.start && t < x.end - 0.5);
    if (b) {
      if (U.autoskip.checked && my.skipped !== b) {
        my.skipped = b;
        U.skip.hidden = true;
        seek(b.end + 0.05);
        toast(`Skipped ${fmtTime(b.end - b.start)} ad break`, 'beam');
      } else if (!U.autoskip.checked) {
        U.skiptime.textContent = fmtTime(b.end - t);
        if (U.skip.hidden) U.skip.hidden = false;
      }
    } else if (!U.skip.hidden) U.skip.hidden = true;
  }
  if (my.item.nextEpisode && dur && dur - t <= UPNEXT_AT && !my.upShown && !my.upDismissed) showUpNext();
}
function paintScrub(t) {
  const dur = total();
  const pct = dur ? Math.min(100, Math.max(0, t / dur * 100)) : 0;
  U.fill.style.width = `${pct}%`;
  U.thumb.style.left = `${pct}%`;
  U.scrub.setAttribute('aria-valuenow', Math.round(t));
  U.scrub.setAttribute('aria-valuemax', Math.round(dur));
  U.scrub.setAttribute('aria-valuetext', `${fmtTime(t)} of ${fmtTime(dur)}`);
}
function paintPlay() {
  U.play.replaceChildren(icon(video.paused ? 'play' : 'pause'));
  U.play.title = video.paused ? 'Play (Space)' : 'Pause (Space)';
}
function paintVolume() {
  U.mute.replaceChildren(icon(video.muted || video.volume === 0 ? 'mute' : 'volume'));
}
function flash(name) {
  U.center.replaceChildren(icon(name));
  U.center.classList.remove('flash');
  void U.center.offsetWidth; // restart the animation
  U.center.classList.add('flash');
}

function setupMarks() {
  const it = S.item;
  S.breaks = it.breaksState === 'ready' && Array.isArray(it.breaks) ? it.breaks.filter((b) => b.end > b.start) : [];
  S.chapters = Array.isArray(it.info?.chapters) ? it.info.chapters.filter((c) => c.end > c.start) : [];
  U.autoskip.parentElement.hidden = !S.breaks.length;
  const dur = it.duration || 0;
  U.marks.replaceChildren(...(dur ? S.breaks.map((b) => el('i', { style: { left: `${b.start / dur * 100}%`, width: `${(b.end - b.start) / dur * 100}%` }, title: `Ad break · ${fmtTime(b.end - b.start)}` })) : []));
  U.chapters.replaceChildren(...(dur ? S.chapters.slice(1).map((c) => el('i', { style: { left: `${c.start / dur * 100}%` }, title: c.title || '' })) : []));
}

// ---------- ambient bleed ----------
function startAmbient() {
  clearInterval(S.timers.ambient);
  S.timers.ambient = setInterval(() => {
    if (!S || video.paused || video.readyState < 2) return;
    try { actx.drawImage(video, 0, 0, ambient.width, ambient.height); } catch {}
  }, AMBIENT_MS);
}

// ---------- progress ----------
function saveProgress() {
  const my = S;
  if (!my || my.live || !my.item.id) return;
  const pos = now(), dur = total();
  if (pos < 15 || !dur) return;
  const done = pos / dur > 0.95;
  const last = my.lastSave;
  if (last && Math.abs(last.pos - pos) < 1 && last.done === done && Date.now() - last.at < 2000) return; // pause+ended+close fire back-to-back
  my.lastSave = { pos, done, at: Date.now() };
  let req = Promise.resolve();
  try {
    req = fetch(`/api/watch/${encodeURIComponent(my.item.id)}`, {
      method: 'POST', headers: { 'Content-Type': 'application/json' }, credentials: 'same-origin', keepalive: true,
      body: JSON.stringify({ pos: Math.round(pos * 10) / 10, dur: Math.round(dur), done }),
    }).catch(() => {});
  } catch {}
  my.item.watch = { pos, dur, done, updated: new Date().toISOString() };
  lastSaveRequest = req;
  return req;
}
let lastSaveRequest = Promise.resolve();

// ---------- idle chrome ----------
function wake() {
  if (!S) return;
  root.classList.remove('idle');
  clearTimeout(S.timers.idle);
  S.timers.idle = setTimeout(armIdle, IDLE_MS);
}
function armIdle() {
  if (!S) return;
  if (video.paused || S.menu || !U.next.hidden || controls.matches(':hover') || S.dragging) { S.timers.idle = setTimeout(armIdle, IDLE_MS); return; }
  root.classList.add('idle');
}

// ---------- up next ----------
function showUpNext() {
  const my = S, nx = my.item.nextEpisode;
  if (!nx) return;
  my.upShown = true;
  const auto = pref.get('autoplay', true);
  const ring = auto ? el('span', { class: 'ring', html: `<svg viewBox="0 0 20 20" width="20" height="20"><circle cx="10" cy="10" r="8" class="bg"/><circle cx="10" cy="10" r="8" class="fg" stroke-dasharray="${RING}" stroke-dashoffset="0"/></svg>` }) : null;
  const playBtn = el('button', { class: 'btn small primary', onclick: () => playNext() }, ring, icon('play'), 'Play now');
  const dismiss = el('button', { class: 'btn small', onclick: () => { my.upDismissed = true; hideUpNext(); } }, 'Dismiss');
  U.next.replaceChildren(
    el('div', { class: 'thumb' }, img(artUrl(nx.id, 'thumb', 320))),
    el('div', { class: 'txt' }, el('span', { class: 'eyebrow' }, 'Up next'), el('b', {}, nx.title), el('small', {}, `${nx.show} · ${epCode(nx)}${nx.duration ? ' · ' + fmtTime(nx.duration) : ''}`)),
    el('div', { class: 'acts' }, playBtn, dismiss));
  U.next.hidden = false;
  wake();
  if (auto) {
    const fg = ring.querySelector('.fg');
    my.upStart = performance.now();
    // The ring is cosmetic (rAF); the actual autoplay is a timeout so it still fires in a hidden tab.
    clearTimeout(my.timers.upnext);
    my.timers.upnext = setTimeout(() => { if (S === my && !U.next.hidden) playNext(); }, COUNTDOWN_MS);
    const spin = () => {
      if (S !== my || U.next.hidden) return;
      const frac = Math.min(1, (performance.now() - my.upStart) / COUNTDOWN_MS);
      fg.setAttribute('stroke-dashoffset', RING * frac);
      if (frac < 1) my.upRaf = requestAnimationFrame(spin);
    };
    my.upRaf = requestAnimationFrame(spin);
  }
}
function hideUpNext() {
  if (!S) return;
  cancelAnimationFrame(S.upRaf);
  clearTimeout(S.timers.upnext);
  U.next.hidden = true;
  U.next.replaceChildren();
}
function playNext() {
  const nx = S?.item.nextEpisode;
  if (!nx) return close();
  Player.open(nx, { from: 0 });
}

// ---------- subtitles ----------
async function loadSubs(my) {
  if (!my.item.id) return;
  try { my.subs = await api.get(`/api/items/${encodeURIComponent(my.item.id)}/subtitles`); } catch { my.subs = []; }
  if (S !== my) return;
  if (!Array.isArray(my.subs)) my.subs = [];
  U.subs.classList.toggle('dim', !my.subs.length);
  const want = pref.get('subLang', 'off');
  if (want && want !== 'off') {
    const s = my.subs.find((x) => x.text && x.url && !x.forced && (x.lang === want || x.title === want));
    if (s) { my.sub = s; applyTextTrack(my); }
  }
}
async function selectSub(s) {
  const my = S;
  const wasBurn = my.burn != null;
  clearTrack(my);
  my.sub = s || null;
  my.burn = null;
  if (!s) {
    pref.set('subLang', 'off');
    if (wasBurn) start(now());
    return;
  }
  pref.set('subLang', s.lang || s.title || 'on');
  if (s.text && s.url) {
    if (wasBurn) start(now()); else applyTextTrack(my);
  } else {
    my.burn = s.streamIndex ?? s.index;
    toast('Burning in subtitles — transcoding', 'beam');
    start(now());
  }
  U.subs.classList.toggle('on', !!my.sub);
}
function clearTrack(my) {
  if (my.trackEl) { try { my.trackEl.track.mode = 'disabled'; } catch {} my.trackEl.remove(); my.trackEl = null; }
  if (my.trackUrl) { URL.revokeObjectURL(my.trackUrl); my.trackUrl = null; }
  for (const t of video.textTracks) { try { t.mode = 'disabled'; } catch {} }
}
async function applyTextTrack(my) {
  const s = my.sub;
  U.subs.classList.toggle('on', !!s);
  if (!s || !s.text || !s.url) return;
  let txt;
  try { const r = await fetch(s.url, { credentials: 'same-origin' }); if (!r.ok) throw 0; txt = await r.text(); }
  catch { if (S === my) toast('Subtitles unavailable', 'err'); return; }
  if (S !== my || my.sub !== s) return;
  if (my.offset > 0) txt = shiftVtt(txt, -my.offset);
  clearTrack(my);
  const url = URL.createObjectURL(new Blob([txt], { type: 'text/vtt' }));
  const tr = el('track', { kind: 'subtitles', src: url, srclang: s.lang || 'und', label: s.title || s.lang || 'Subtitles', default: true });
  video.append(tr);
  my.trackEl = tr;
  my.trackUrl = url;
  try { tr.track.mode = 'showing'; } catch {}
}
const tsRe = /(?:(\d{1,2}):)?(\d{2}):(\d{2})\.(\d{3})/g;
const parseTs = (s) => { const m = s.match(/(?:(\d+):)?(\d{2}):(\d{2})[.,](\d{3})/); return m ? (+(m[1] || 0)) * 3600 + (+m[2]) * 60 + (+m[3]) + (+m[4]) / 1000 : 0; };
const fmtTs = (t) => { t = Math.max(0, t); const h = Math.floor(t / 3600), m = Math.floor((t % 3600) / 60), s = Math.floor(t % 60), ms = Math.round((t - Math.floor(t)) * 1000); return `${String(h).padStart(2, '0')}:${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}.${String(ms).padStart(3, '0')}`; };
/// Shift every cue timestamp in a WebVTT document by `delta` seconds (drops cues ending before 0).
function shiftVtt(txt, delta) {
  const out = [];
  const blocks = txt.split(/\r?\n\r?\n/);
  for (const block of blocks) {
    const lines = block.split(/\r?\n/);
    const i = lines.findIndex((l) => l.includes('-->'));
    if (i < 0) { out.push(block); continue; }
    const [a, b] = lines[i].split('-->').map((x) => x.trim().split(/\s+/)[0]);
    const s = parseTs(a) + delta, e = parseTs(b) + delta;
    if (e <= 0) continue;
    lines[i] = lines[i].replace(tsRe, (m, h, mm, ss, ms) => fmtTs(((+(h || 0)) * 3600 + (+mm) * 60 + (+ss) + (+ms) / 1000) + delta));
    out.push(lines.join('\n'));
  }
  return out.join('\n\n');
}
function cycleSubs() {
  const my = S;
  if (!my || my.live) return;
  const list = (my.subs || []);
  if (!list.length) return toast('No subtitles available');
  const i = my.sub ? list.indexOf(my.sub) : -1;
  const nxt = i + 1 >= list.length ? null : list[i + 1];
  selectSub(nxt);
  toast(nxt ? `Subtitles: ${subLabel(nxt)}` : 'Subtitles off', 'beam', 1500);
}
const LANG = { eng: 'English', en: 'English', fra: 'French', fre: 'French', fr: 'French', spa: 'Spanish', es: 'Spanish', deu: 'German', ger: 'German', de: 'German', ita: 'Italian', jpn: 'Japanese', ja: 'Japanese', kor: 'Korean', por: 'Portuguese', rus: 'Russian', zho: 'Chinese', chi: 'Chinese', nld: 'Dutch', swe: 'Swedish', und: 'Unknown' };
const langName = (l) => (l ? LANG[l.toLowerCase()] || l.toUpperCase() : '');
const subLabel = (s) => { const base = s.title && s.title !== s.lang ? s.title : langName(s.lang) || `Track ${s.index + 1}`; return s.forced && !/forced/i.test(base) ? `${base} (forced)` : base; };
const chLabel = (c) => (c >= 7 ? '7.1' : c === 6 ? '5.1' : c === 2 ? 'Stereo' : c === 1 ? 'Mono' : c ? `${c}ch` : '');
const audioLabel = (a) => { const base = a.title || langName(a.lang) || `Track ${a.index}`; const ch = chLabel(a.channels); return ch && !base.includes(ch) ? `${base} · ${ch}` : base; };

// ---------- sprites ----------
async function loadSprites(my) {
  if (!my.item.id) return;
  try {
    const r = await fetch(`/api/items/${encodeURIComponent(my.item.id)}/sprites.vtt`, { credentials: 'same-origin' });
    if (!r.ok) return;
    const txt = await r.text();
    if (S !== my) return;
    my.sprites = parseSprites(txt);
    if (my.sprites.length) {
      const im = new Image();
      im.onload = () => { if (S === my) my.sheet = { w: im.naturalWidth, h: im.naturalHeight }; };
      im.src = `/api/items/${encodeURIComponent(my.item.id)}/sprites.jpg`;
    }
  } catch {}
}
function parseSprites(txt) {
  const cues = [];
  const lines = txt.split(/\r?\n/);
  for (let i = 0; i < lines.length; i++) {
    const m = lines[i].match(/([\d:.]+)\s+-->\s+([\d:.]+)/);
    if (!m) continue;
    const xy = (lines[i + 1] || '').match(/#xywh=(\d+),(\d+),(\d+),(\d+)/);
    if (!xy) continue;
    cues.push({ start: parseTs(m[1]), end: parseTs(m[2]), x: +xy[1], y: +xy[2], w: +xy[3], h: +xy[4] });
  }
  return cues;
}
function showPreview(clientX) {
  const my = S;
  if (!my || my.live) return;
  const rect = my.rect || (my.rect = U.scrub.getBoundingClientRect());
  const frac = Math.min(1, Math.max(0, (clientX - rect.left) / rect.width));
  const t = frac * total();
  const sp = U.preview.firstElementChild, lab = U.preview.lastElementChild;
  lab.textContent = fmtTime(t);
  const cue = my.sprites?.find((c) => t >= c.start && t < c.end);
  if (cue && my.sheet) {
    const scale = 160 / cue.w;
    sp.classList.add('has');
    sp.style.width = '160px';
    sp.style.height = `${Math.round(cue.h * scale)}px`;
    sp.style.backgroundImage = `url("/api/items/${encodeURIComponent(my.item.id)}/sprites.jpg")`;
    sp.style.backgroundSize = `${Math.round(my.sheet.w * scale)}px ${Math.round(my.sheet.h * scale)}px`;
    sp.style.backgroundPosition = `-${Math.round(cue.x * scale)}px -${Math.round(cue.y * scale)}px`;
  } else sp.classList.remove('has');
  const half = 90; // keep the preview inside the bar
  const x = Math.min(rect.width - half, Math.max(half, frac * rect.width));
  U.preview.style.left = `${x}px`;
  U.preview.hidden = false;
  return t;
}

// ---------- menus ----------
function closeMenu() { if (S) S.menu = null; U.menu.hidden = true; U.menu.replaceChildren(); for (const b of [U.settings, U.audio, U.subs]) b.setAttribute('aria-expanded', 'false'); }
function openMenu(kind) {
  if (!S) return;
  if (S.menu === kind) return closeMenu();
  closeMenu();
  S.menu = kind;
  const item = (label, on, onclick, sub) => el('button', { class: on ? 'on' : '', type: 'button', onclick: () => { onclick(); } }, label, sub ? el('small', {}, sub) : null);
  const frag = [];
  if (kind === 'settings') {
    const srcH = S.item.height || S.item.info?.height || 0;
    frag.push(el('h4', {}, 'Quality'));
    if (!S.live) {
      frag.push(item('Auto', S.quality === 'auto', () => setQuality('auto'), 'direct when possible'));
      frag.push(item('Original', S.quality === 'original', () => setQuality('original'), 'remux'));
    } else frag.push(item('Auto', S.quality === 'auto', () => setQuality('auto'), '720p'));
    for (const h of LADDER) {
      if (!S.live && srcH && h > srcH) continue;
      if (S.live && ![1080, 720, 480, 360].includes(h)) continue;
      frag.push(item(`${h}p`, S.quality === String(h), () => setQuality(String(h)), h >= 2160 ? '4K' : ''));
    }
    if (!S.live) {
      frag.push(el('h4', {}, 'Speed'));
      for (const s of SPEEDS) frag.push(item(s === 1 ? 'Normal' : `${s}×`, S.speed === s, () => { S.speed = s; video.playbackRate = s; closeMenu(); toast(`Speed ${s}×`, '', 1200); }));
      frag.push(el('h4', {}, 'Playback'));
      const auto = pref.get('autoplay', true);
      frag.push(item('Autoplay next episode', auto, () => { pref.set('autoplay', !auto); openMenu('settings'); S.menu = 'settings'; }));
    }
    U.settings.setAttribute('aria-expanded', 'true');
  } else if (kind === 'audio') {
    const tracks = S.item.info?.audio || [];
    frag.push(el('h4', {}, 'Audio'));
    if (!tracks.length) frag.push(el('button', { disabled: true }, 'Default track'));
    const cur = S.audio ?? tracks.find((a) => a.default)?.index ?? tracks[0]?.index;
    for (const a of tracks) frag.push(item(audioLabel(a), a.index === cur, () => { if (a.index === cur) return closeMenu(); S.audio = a.index; closeMenu(); start(now()); }, (a.codec || '').toUpperCase()));
    U.audio.setAttribute('aria-expanded', 'true');
  } else if (kind === 'subs') {
    const list = S.subs || [];
    frag.push(el('h4', {}, 'Subtitles'));
    frag.push(item('Off', !S.sub, () => { selectSub(null); closeMenu(); }));
    if (!list.length) frag.push(el('button', { disabled: true }, 'None available'));
    for (const s of list) frag.push(item(subLabel(s), S.sub === s, () => { selectSub(s); closeMenu(); }, s.text ? (s.external ? 'external' : 'text') : 'image · burn-in'));
    U.subs.setAttribute('aria-expanded', 'true');
  }
  U.menu.replaceChildren(...frag);
  U.menu.hidden = false;
  wake();
}
function setQuality(qv) {
  if (!S) return;
  if (S.live) pref.set('liveQuality', qv); else pref.set('quality', qv);
  S.quality = qv;
  S.fallback = false;
  closeMenu();
  start(now());
}

// ---------- fullscreen / pip ----------
function toggleFs() {
  if (document.fullscreenElement) return document.exitFullscreen().catch(() => {});
  if (root.requestFullscreen) root.requestFullscreen().catch(() => {});
  else if (video.webkitEnterFullscreen) video.webkitEnterFullscreen();
}
async function togglePip() {
  try {
    if (document.pictureInPictureElement) await document.exitPictureInPicture();
    else if (document.pictureInPictureEnabled) await video.requestPictureInPicture();
  } catch (e) { toast('Picture in picture unavailable', 'err'); }
}
if (!document.pictureInPictureEnabled) U.pip.hidden = true;

// ---------- wiring ----------
U.back.onclick = () => close();
U.play.onclick = () => togglePlay();
U.r10.onclick = () => skipBy(-10);
U.f30.onclick = () => skipBy(30);
U.mute.onclick = () => { video.muted = !video.muted; pref.set('muted', video.muted); paintVolume(); };
U.vol.addEventListener('input', () => { video.volume = +U.vol.value; video.muted = false; pref.set('volume', video.volume); pref.set('muted', false); paintVolume(); });
U.autoskip.addEventListener('change', () => { pref.set('autoskip', U.autoskip.checked); if (S) S.skipped = null; });
U.skip.onclick = () => skipBreak();
U.settings.onclick = () => openMenu('settings');
U.audio.onclick = () => openMenu('audio');
U.subs.onclick = () => openMenu('subs');
U.pip.onclick = () => togglePip();
U.fs.onclick = () => toggleFs();
root.tabIndex = -1;

function togglePlay() {
  if (!S) return;
  if (video.paused) { video.play().catch(() => {}); flash('play'); }
  else { video.pause(); flash('pause'); }
}
function skipBreak() {
  const my = S;
  if (!my) return;
  const t = now();
  const b = my.breaks.find((x) => t >= x.start - 1 && t < x.end);
  if (!b) return;
  my.skipped = b;
  U.skip.hidden = true;
  seek(b.end + 0.05);
  toast(`Skipped ${fmtTime(b.end - b.start)} ad break`, 'beam');
}

// click on the picture toggles play; double-click toggles fullscreen
root.addEventListener('click', (e) => {
  if (!S) return;
  if (!U.menu.hidden && !U.menu.contains(e.target) && ![U.settings, U.audio, U.subs].some((b) => b.contains(e.target))) closeMenu();
  if (e.target === video || e.target === ambient || e.target.classList.contains('p-shade') || e.target === root) togglePlay();
});
root.addEventListener('dblclick', (e) => {
  if (!S) return;
  if (e.target === video || e.target === ambient || e.target.classList.contains('p-shade') || e.target === root) { togglePlay(); toggleFs(); }
});
root.addEventListener('pointermove', () => wake(), { passive: true });
root.addEventListener('pointerdown', () => wake(), { passive: true });

// scrub bar: pointer capture drag, hover preview
U.scrub.addEventListener('pointerenter', () => { if (S) S.rect = U.scrub.getBoundingClientRect(); });
U.scrub.addEventListener('pointerdown', (e) => {
  if (!S || S.live || e.button !== 0) return;
  e.preventDefault();
  S.rect = U.scrub.getBoundingClientRect();
  try { U.scrub.setPointerCapture(e.pointerId); } catch {}
  S.dragging = true;
  U.scrub.classList.add('drag');
  S.dragT = showPreview(e.clientX);
  paintScrub(S.dragT);
  U.cur.textContent = fmtTime(S.dragT);
});
U.scrub.addEventListener('pointermove', (e) => {
  if (!S || S.live) return;
  const t = showPreview(e.clientX);
  if (S.dragging) { S.dragT = t; paintScrub(t); U.cur.textContent = fmtTime(t); }
});
const endDrag = (e) => {
  if (!S || !S.dragging) return;
  S.dragging = false;
  U.scrub.classList.remove('drag');
  try { U.scrub.releasePointerCapture(e.pointerId); } catch {}
  seek(S.dragT);
  if (e.type === 'pointerup' && !U.scrub.matches(':hover')) U.preview.hidden = true;
};
U.scrub.addEventListener('pointerup', endDrag);
U.scrub.addEventListener('pointercancel', endDrag);
U.scrub.addEventListener('pointerleave', () => { if (S && !S.dragging) U.preview.hidden = true; });
U.scrub.addEventListener('keydown', (e) => {
  if (!S || S.live) return;
  if (e.key === 'ArrowLeft') { e.preventDefault(); skipBy(-10); }
  if (e.key === 'ArrowRight') { e.preventDefault(); skipBy(30); }
});

// video events
video.addEventListener('play', () => { if (!S) return; paintPlay(); wake(); startAmbient(); clearInterval(S.timers.progress); S.timers.progress = setInterval(saveProgress, PROGRESS_MS); if (navigator.mediaSession) navigator.mediaSession.playbackState = 'playing'; });
video.addEventListener('pause', () => { if (!S) return; paintPlay(); wake(); clearInterval(S.timers.ambient); clearInterval(S.timers.progress); if (!S.ended) saveProgress(); if (navigator.mediaSession) navigator.mediaSession.playbackState = 'paused'; });
video.addEventListener('timeupdate', () => { tick(); schedule(); });
video.addEventListener('progress', schedule);
video.addEventListener('durationchange', schedule);
video.addEventListener('seeking', schedule);
video.addEventListener('waiting', () => root.classList.add('buffering'));
video.addEventListener('playing', () => root.classList.remove('buffering'));
video.addEventListener('canplay', () => root.classList.remove('buffering'));
video.addEventListener('loadedmetadata', () => { if (!S) return; root.classList.remove('buffering'); schedule(); });
video.addEventListener('volumechange', paintVolume);
video.addEventListener('ratechange', () => { if (S) S.speed = video.playbackRate; });
video.addEventListener('ended', () => {
  const my = S;
  if (!my || my.live) return;
  my.ended = true;
  saveProgress();
  if (my.item.nextEpisode && !my.upDismissed) {
    if (pref.get('autoplay', true)) return playNext();
    if (!my.upShown) showUpNext();
    return;
  }
  close();
});
video.addEventListener('error', () => {
  const my = S;
  if (!my || my.starting) return;
  const err = video.error;
  if (my.direct && !my.fallback) {
    my.fallback = true;
    my.quality = my.item.height ? String(Math.min(1080, my.item.height)) : '1080';
    toast('Direct play failed — switching to transcode', 'beam', 3000);
    start(now());
    return;
  }
  if (my.hls) return; // hls.js reports its own errors
  toast(`Playback error${err?.message ? ': ' + err.message : ''}`, 'err', 4000);
});

// keyboard
document.addEventListener('keydown', (e) => {
  if (!S || root.hidden) return;
  if (!$('#palette').hidden || $('#modal-root').childElementCount) return;
  const tag = e.target.tagName;
  if (tag === 'INPUT' && e.target.type !== 'checkbox' && e.target.type !== 'range') return;
  if (tag === 'INPUT' && e.target.type === 'range' && (e.key.startsWith('Arrow'))) return;
  if (e.metaKey || e.ctrlKey || e.altKey) return;
  const k = e.key;
  let handled = true;
  if (k === ' ' || k === 'k' || k === 'K') togglePlay();
  else if (k === 'ArrowLeft') skipBy(-10);
  else if (k === 'ArrowRight') skipBy(30);
  else if (k === 'j' || k === 'J') skipBy(-10);
  else if (k === 'l' || k === 'L') skipBy(10);
  else if (k === 'ArrowUp') { video.volume = Math.min(1, video.volume + 0.05); video.muted = false; pref.set('volume', video.volume); U.vol.value = video.volume; }
  else if (k === 'ArrowDown') { video.volume = Math.max(0, video.volume - 0.05); pref.set('volume', video.volume); U.vol.value = video.volume; }
  else if (k === 'm' || k === 'M') U.mute.click();
  else if (k === 'f' || k === 'F') toggleFs();
  else if (k === 'p' || k === 'P') togglePip();
  else if (k === 'c' || k === 'C') cycleSubs();
  else if (k === 's' || k === 'S') skipBreak();
  else if (k === 'n' || k === 'N') { if (S.item.nextEpisode) playNext(); }
  else if (k === 'Escape') { if (S.menu) closeMenu(); else if (document.fullscreenElement) document.exitFullscreen().catch(() => {}); else close(); }
  else if (/^[0-9]$/.test(k) && !S.live) seek(total() * (+k) / 10);
  else if (k === ',' && video.paused) { video.currentTime = Math.max(0, video.currentTime - 1 / 24); }
  else if (k === '.' && video.paused) { video.currentTime += 1 / 24; }
  else handled = false;
  if (handled) { e.preventDefault(); e.stopPropagation(); wake(); }
}, true);

// page lifecycle
window.addEventListener('pagehide', () => { if (S) { saveProgress(); endSession(S); } });
document.addEventListener('visibilitychange', () => { if (S && document.hidden) saveProgress(); });
document.addEventListener('fullscreenchange', () => U.fs.classList.toggle('on', !!document.fullscreenElement));
video.addEventListener('enterpictureinpicture', () => U.pip.classList.add('on'));
video.addEventListener('leavepictureinpicture', () => U.pip.classList.remove('on'));

// media session actions (claimed while the player is open; the music dock re-claims on close)
function claimMediaSession() {
  if (!('mediaSession' in navigator)) return;
  const ms = navigator.mediaSession;
  const set = (a, fn) => { try { ms.setActionHandler(a, fn); } catch {} };
  set('play', () => { if (S) video.play().catch(() => {}); });
  set('pause', () => { if (S) video.pause(); });
  set('seekbackward', (d) => skipBy(-(d.seekOffset || 10)));
  set('seekforward', (d) => skipBy(d.seekOffset || 30));
  set('seekto', (d) => { if (S && d.seekTime != null) seek(d.seekTime); });
  set('previoustrack', null);
  set('nexttrack', () => { if (S?.item.nextEpisode) playNext(); });
  set('stop', () => close());
}
