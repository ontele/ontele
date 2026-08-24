/* Copyright 2026 The Ontele Authors — SPDX-License-Identifier: Apache-2.0 */
/* Settings (library, metadata, playback, live TV & DVR, commercials, users, system) and the Activity feed. */

import { $, el, api, icon, confirm, fmtAgo, fmtClock, fmtDateLong, busy, emptyState, initials, session, view, ambientFrom, navigate, pref, applyAppearance, toast } from '../core.js';

const BASE_SECTIONS = [
  ['library', 'Library'], ['trending', 'Trending'], ['appearance', 'Appearance'], ['metadata', 'Metadata'],
  ['playback', 'Playback'], ['livetv', 'Live TV & DVR'], ['commercials', 'Commercials'], ['users', 'Users'],
  ['system', 'System'],
];
const ADMIN_SECTIONS = [['health', 'Health'], ['setup', 'Setup guide']];
const fmtWatch = (s) => { const mins = Math.round(s / 60), h = Math.floor(mins / 60), m = mins % 60; return h ? `${h}h ${m}m` : m ? `${m}m` : `${Math.round(s)}s`; };
const fmtGB = (b) => (b >= 1e12 ? `${(b / 1e12).toFixed(1)} TB` : `${(b / 1e9).toFixed(1)} GB`);

/* Bar histogram on a canvas; newest sample at the right edge. */
function histo(canvas, values, { max = null, color } = {}) {
  const dpr = devicePixelRatio || 1;
  const w = canvas.clientWidth || 260, h = canvas.clientHeight || 56;
  canvas.width = w * dpr; canvas.height = h * dpr;
  const ctx = canvas.getContext('2d');
  ctx.scale(dpr, dpr);
  const css = getComputedStyle(document.documentElement);
  const beam = color || css.getPropertyValue('--beam').trim() || '#ffb454';
  const line = css.getPropertyValue('--glass-2').trim();
  // bucket the full ring (~1h at 15s) into 60 bars, keeping each bucket's
  // peak so short spikes stay visible
  const n = 60;
  const per = Math.max(1, Math.ceil(values.length / n));
  const vs = [];
  for (let i = Math.max(0, values.length - n * per); i < values.length; i += per) {
    vs.push(Math.max(...values.slice(i, i + per)));
  }
  const top = Math.max(max ?? 0, ...vs, 0.001);
  const bw = w / n;
  ctx.fillStyle = line;
  ctx.fillRect(0, h - 1, w, 1);
  vs.forEach((v, i) => {
    const bh = Math.max(1.5, (v / top) * (h - 6));
    const x = w - (vs.length - i) * bw;
    ctx.fillStyle = beam;
    ctx.globalAlpha = 0.35 + 0.65 * (i / vs.length);
    ctx.beginPath();
    ctx.roundRect(x + bw * 0.15, h - bh, bw * 0.7, bh, 1.5);
    ctx.fill();
  });
  ctx.globalAlpha = 1;
}
const MASK = '••••••';
const fmtUptime = (s) => { if (!s) return '—'; const d = Math.floor(s / 86400), h = Math.floor((s % 86400) / 3600), m = Math.floor((s % 3600) / 60); return d ? `${d}d ${h}h` : h ? `${h}h ${m}m` : `${m}m`; };

export async function render() {
  ambientFrom(null);
  const isAdmin = !!session.user?.isAdmin;
  view.replaceChildren(el('div', { class: 'page' }, el('div', { class: 'page-head' }, el('h1', {}, 'Settings')),
    el('div', { class: 'settings-grid' }, el('div', {}), el('div', {}, Array.from({ length: 3 }, () => el('div', { class: 'card-box' }, el('div', { class: 'skel-line', style: { width: '30%' } }), el('div', { class: 'skel-line' }), el('div', { class: 'skel-line', style: { width: '70%' } })))))));

  let settings, probe = {}, stats = {}, users = null, scan = null;
  try {
    [settings, probe, stats, users, scan] = await Promise.all([
      api.get('/api/settings'),
      api.get('/api/settings/probe').catch(() => ({})),
      api.get('/api/stats').catch(() => ({})),
      isAdmin ? api.get('/api/users').catch(() => null) : null,
      api.get('/api/scan/status').catch(() => null),
    ]);
  } catch (e) {
    view.replaceChildren(el('div', { class: 'page' }, el('div', { class: 'page-head' }, el('h1', {}, 'Settings')), emptyState('Settings unavailable', e.message, 'info')));
    return;
  }
  const edits = {};            // camelCase key -> new value (merged on save)
  const dirty = new Set();
  const setVal = (k, v) => { edits[k] = v; dirty.add(k); saveBar.hidden = false; saveCount.textContent = `${dirty.size} change${dirty.size === 1 ? '' : 's'}`; };
  const ro = !isAdmin;

  // ---------- field builders ----------
  const hint = (t) => (t ? el('div', { class: 'hint' }, t) : null);
  function text(k, label, h, attrs = {}) {
    const i = el('input', { type: attrs.type || 'text', value: settings[k] ?? '', disabled: ro, ...attrs });
    i.oninput = () => setVal(k, attrs.type === 'number' ? (i.value === '' ? 0 : +i.value) : i.value);
    return el('div', { class: 'field' }, label ? el('label', {}, label) : null, i, hint(h));
  }
  function secret(k, label, h) {
    const i = el('input', { type: 'password', value: settings[k] || '', disabled: ro, autocomplete: 'new-password', placeholder: 'Not set' });
    i.addEventListener('focus', () => { if (i.value === MASK) i.select(); });
    i.oninput = () => setVal(k, i.value);
    const eye = el('button', { class: 'icon-btn small-eye', type: 'button', title: 'Show / hide', 'aria-label': 'Show or hide key' }, icon('eye'));
    eye.onclick = () => { i.type = i.type === 'password' ? 'text' : 'password'; };
    return el('div', { class: 'field' }, el('label', {}, label), el('div', { class: 'with-btn' }, i, eye), hint(h));
  }
  function lines(k, label, h) {
    const t = el('textarea', { rows: 3, disabled: ro, spellcheck: 'false', placeholder: '/path/to/media' }, (settings[k] || []).join('\n'));
    t.oninput = () => setVal(k, t.value.split('\n').map((s) => s.trim()).filter(Boolean));
    return el('div', { class: 'field' }, el('label', {}, label), t, hint(h));
  }
  function select(k, label, options, h) {
    const s = el('select', { disabled: ro }, options.map(([v, lbl, dis]) => el('option', { value: v, disabled: dis || null }, lbl)));
    s.value = String(settings[k] ?? options[0][0]);
    s.onchange = () => setVal(k, s.value);
    return el('div', { class: 'field' }, el('label', {}, label), s, hint(h));
  }
  function toggle(get, set, label, h) {
    const on = !!get();
    const sw = el('button', { class: `switch ${on ? 'on' : ''}`, type: 'button', role: 'switch', 'aria-checked': String(on), disabled: ro, 'aria-label': label });
    sw.onclick = () => { const v = !sw.classList.contains('on'); sw.classList.toggle('on', v); sw.setAttribute('aria-checked', String(v)); set(v); };
    return el('div', { class: 'switch-row' }, el('div', {}, el('b', {}, label), hint(h)), sw);
  }
  const switchFor = (k, label, h) => toggle(() => settings[k], (v) => setVal(k, v), label, h);
  const providers = { ...(settings.metadataProviders || {}) };
  const provSwitch = (p, label, h) => toggle(() => providers[p], (v) => { providers[p] = v; setVal('metadataProviders', { ...providers }); }, label, h);

  const card = (id, title, ...kids) => el('section', { class: 'card-box', id: `s-${id}` }, el('h2', {}, title), ...kids);

  // ---------- sections ----------
  const scanBtn = el('button', { class: 'btn small', type: 'button', disabled: ro }, icon('refresh'), 'Scan library');
  scanBtn.onclick = () => busy(scanBtn, () => api.post('/api/scan'), 'Library scan started');
  const library = card('library', 'Library',
    lines('mediaDirs', 'Movie & TV folders', 'One path per line. Subfolders are classified by filename (Movie (2019).mkv, Show/S01E02.mkv).'),
    lines('musicDirs', 'Music folders', 'One path per line; tags are read from the files.'),
    text('recordingsDir', 'Recordings folder', 'Where the DVR writes captures.'),
    el('div', { class: 'form-row' },
      text('scanIntervalMin', 'Rescan every (min)', null, { type: 'number', min: 1, max: 1440 }),
      el('div', { class: 'field' }, el('label', {}, 'Filesystem watcher'), switchFor('watchFilesystem', 'Watch for changes', 'Pick up new files within seconds instead of waiting for the next scan.'))),
    el('div', { class: 'acts-row' }, scanBtn, scan?.finishedAt ? el('span', { class: 'faint' }, `Last scan ${fmtAgo(scan.finishedAt)} · ${scan.found} files`) : null));

  const metadata = card('metadata', 'Metadata',
    el('div', { class: 'switch-grid' },
      provSwitch('nfo', 'Kodi NFO files', 'Read .nfo sidecars and local artwork first.'),
      provSwitch('tmdb', 'TMDB', 'Movies and TV: overview, cast, posters, backdrops.'),
      provSwitch('musicbrainz', 'MusicBrainz', 'Albums and artists, cover art from the Cover Art Archive.')),
    el('div', { class: 'form-row' },
      secret('tmdbApiKey', 'TMDB API key', 'Required for TMDB lookups. Stored server-side; never sent to the browser.'),
      text('metadataLanguage', 'Language', 'BCP-47, e.g. en-US, de-DE.', { placeholder: 'en-US' })));

  const hw = new Set(probe.hwaccels || []);
  const enc = (probe.encoders || []).join(' ');
  const hwOpt = (v, label, need, needEnc) => {
    if (v === 'none') return [v, label, false];
    const hasHw = hw.has(need), hasEnc = !needEnc || enc.includes(needEnc);
    return [v, `${label} · ${hasHw && hasEnc ? 'available' : hasHw ? 'decode only' : 'not detected'}`, false];
  };
  const playback = card('playback', 'Playback',
    el('div', { class: 'form-row' },
      select('hwaccel', 'Hardware acceleration', [
        hwOpt('none', 'None (libx264)'), hwOpt('vaapi', 'VA-API (Intel/AMD)', 'vaapi', 'vaapi'), hwOpt('qsv', 'Intel Quick Sync', 'qsv', 'qsv'),
        hwOpt('nvenc', 'NVIDIA NVENC', 'cuda', 'nvenc'), hwOpt('videotoolbox', 'Apple VideoToolbox', 'videotoolbox', 'videotoolbox'),
      ], probe.hwaccels?.length ? `ffmpeg reports: ${probe.hwaccels.join(', ')}` : 'Run the probe (System card) to see what ffmpeg supports.'),
      select('transcodePreset', 'x264 preset', ['ultrafast', 'superfast', 'veryfast', 'faster', 'fast', 'medium', 'slow'].map((p) => [p, p]), 'Faster presets cost quality per bitrate; slower ones cost CPU.')),
    el('div', { class: 'form-row' },
      text('maxTranscodes', 'Max concurrent transcodes', 'Additional sessions wait for a slot.', { type: 'number', min: 1, max: 32 }),
      el('div', { class: 'field' }, el('label', {}, 'Seek previews'), switchFor('thumbnails', 'Generate thumbnails', 'Sprite sheets for scrub-bar previews (background job after scan).'))));

  const livetv = card('livetv', 'Live TV & DVR',
    el('div', { class: 'form-row' },
      text('hdhrIp', 'HDHomeRun address', 'Leave blank to auto-discover on the LAN. Set it when Docker or VLANs block UDP discovery.', { placeholder: '192.168.1.50 (auto)' }),
      text('xmltvUrl', 'XMLTV guide source', 'URL or file path; .gz is fine.', { placeholder: 'https://… or /path/guide.xml' })),
    el('div', { class: 'form-row' },
      text('guideRefreshHours', 'Refresh guide every (h)', null, { type: 'number', min: 1, max: 48 }),
      text('prePadMin', 'Start early (min)', null, { type: 'number', min: 0, max: 30 }),
      text('postPadMin', 'End late (min)', null, { type: 'number', min: 0, max: 60 })),
    text('dvrPostCmd', 'DVR post-processing command', 'Runs after a recording (and its ad pass) finishes, as sh -c <cmd> with $ONTELE_FILE. Built in: /usr/local/bin/handbrake-postprocess.sh encodes and files it into the TV/movie library.', { placeholder: '/usr/local/bin/handbrake-postprocess.sh' }),
    switchFor('autoDeleteWatched', 'Auto-delete watched recordings', 'Remove a recording once everyone who started it has finished it.'),
    (() => { const b = el('button', { class: 'btn small', type: 'button', disabled: ro }, icon('refresh'), 'Refresh tuner & guide'); b.onclick = () => busy(b, () => api.post('/api/livetv/refresh'), 'Tuner discovery and guide refresh started'); return el('div', { class: 'acts-row' }, b, stats.channels != null ? el('span', { class: 'faint' }, `${stats.channels} channels${stats.guideUpdated ? ` · guide ${fmtAgo(stats.guideUpdated)}` : ''}`) : null); })());

  const modeDesc = { off: 'Recordings are kept as captured.', skip: 'Detect ad breaks and offer a Skip button (and auto-skip) during playback. Non-destructive.', delete: 'Detect ad breaks and hard-cut them out of the file. Saves space; cannot be undone.' };
  const modeHint = el('div', { class: 'hint' }, modeDesc[settings.commercialMode] || '');
  const modeSel = select('commercialMode', 'Commercial handling', [['off', 'Off'], ['skip', 'Skip markers (recommended)'], ['delete', 'Cut from file']]);
  modeSel.querySelector('select').addEventListener('change', (e) => { modeHint.textContent = modeDesc[e.target.value] || ''; });
  modeSel.append(modeHint);
  const adv = el('details', { class: 'adv' }, el('summary', {}, 'Advanced: tool paths'),
    el('div', { class: 'form-row' },
      text('comskipPath', 'comskip', probe.comskip === false ? 'Not found on PATH — falls back to ffmpeg silence/black detection.' : null),
      text('ffmpegPath', 'ffmpeg'), text('ffprobePath', 'ffprobe')));
  const commercials = card('commercials', 'Commercials',
    modeSel,
    switchFor('commercialChapters', 'Write ad-break chapters', 'In skip mode also embed the breaks as chapters so other players can jump them.'),
    adv);

  // ---- users ----
  let usersBox;
  if (isAdmin) {
    const list = el('div', { class: 'users-list' });
    const renderUsers = () => {
      list.replaceChildren();
      for (const u of users || []) {
        const me = u.id === session.user?.id;
        const tog = el('button', { class: `switch ${u.isAdmin ? 'on' : ''}`, type: 'button', role: 'switch', 'aria-checked': String(!!u.isAdmin), 'aria-label': `Admin: ${u.name || u.email}`, disabled: me && u.isAdmin, title: me && u.isAdmin ? 'You cannot demote yourself' : 'Toggle administrator' });
        tog.onclick = async () => {
          const want = !u.isAdmin;
          if (!(await confirm(want ? 'Make administrator?' : 'Remove administrator?', `${u.name || u.email || u.subject} will ${want ? 'be able to change settings, scan, and manage everyone’s recordings.' : 'become a regular member.'}`, want ? 'Make admin' : 'Remove admin'))) return;
          await busy(tog, async () => { await api.put(`/api/users/${u.id}/admin`, { admin: want }); u.isAdmin = want; renderUsers(); }, want ? 'Promoted to admin' : 'Admin removed');
        };
        list.append(el('div', { class: 'list-item user-row' },
          el('span', { class: 'avatar sm', 'aria-hidden': 'true' }, initials(u.name || u.email || u.subject)),
          el('div', { class: 'grow' }, el('b', {}, u.name || u.subject, me ? el('span', { class: 'faint' }, ' (you)') : null), el('small', {}, [u.email, u.lastSeen ? `seen ${fmtAgo(u.lastSeen)}` : null].filter(Boolean).join(' · '))),
          el('span', { class: `chip ${u.isAdmin ? 'on' : ''}` }, u.isAdmin ? 'Admin' : 'Member'),
          tog));
      }
      if (!users?.length) list.append(el('p', { class: 'muted' }, 'No users yet — they appear on first sign-in through the proxy.'));
    };
    renderUsers();
    usersBox = card('users', 'Users', el('p', { class: 'muted card-intro' }, `Identity comes from ${session.authMode === 'proxy' ? 'the OAuth2 proxy' : 'the local session'}; Ontele never stores passwords. Admins are set here or via ONTELE_ADMIN_GROUPS.`), list,
      el('div', { class: 'acts-row' }, el('a', { class: 'btn small', href: '#/activity' }, icon('eye'), 'Activity log')));
  } else {
    usersBox = card('users', 'Users', el('p', { class: 'muted' }, 'Only administrators can manage users.'), el('div', { class: 'acts-row' }, el('a', { class: 'btn small', href: '#/activity' }, icon('eye'), 'Activity log')));
  }

  // ---- system ----
  const chip = (ok, label, title) => el('span', { class: `chip ${ok ? 'ok' : 'bad'}`, title: title || '' }, icon(ok ? 'check' : 'x'), label);
  const verShort = (v) => (v ? String(v).replace(/^ff\w+ version /, '').split(' ')[0] : '');
  const probeRow = el('div', { class: 'probe' },
    chip(!!probe.ffmpeg, probe.ffmpeg ? `ffmpeg ${verShort(probe.ffmpeg)}` : 'ffmpeg missing', probe.ffmpeg),
    chip(!!probe.ffprobe, probe.ffprobe ? `ffprobe ${verShort(probe.ffprobe)}` : 'ffprobe missing', probe.ffprobe),
    chip(!!probe.comskip, probe.comskip ? 'comskip found' : 'comskip missing (ffmpeg fallback)'),
    ...(probe.hwaccels || []).map((h) => el('span', { class: 'chip' }, `hw: ${h}`)),
    ...(probe.encoders || []).slice(0, 6).map((e) => el('span', { class: 'chip faint' }, e)));
  const it = stats.items || {};
  const tile = (v, l) => el('div', { class: 'tile' }, el('b', {}, v == null ? '—' : String(v)), el('small', {}, l));
  const sys = card('system', 'System',
    probeRow,
    el('div', { class: 'stat-tiles sys-tiles' },
      tile(it.movie, 'Movies'), tile(it.episode, 'Episodes'), tile(it.track, 'Tracks'), tile(it.recording, 'Recordings'),
      tile(stats.streams, 'Streams'), tile(stats.transcodes, 'Transcodes'), tile(stats.recordingsActive, 'Recording now'), tile(stats.channels, 'Channels'),
      tile(fmtUptime(stats.uptimeSec ?? probe.uptimeSec), 'Uptime'), tile(stats.version || session.version, 'Version')),
    el('div', { class: 'kv' },
      el('b', {}, 'Data dir'), el('span', {}, probe.dataDir || '—'),
      el('b', {}, 'Last scan'), el('span', {}, scan?.finishedAt ? `${fmtDateLong(scan.finishedAt)} ${fmtClock(scan.finishedAt)} · ${scan.found} files, +${scan.added} / −${scan.removed}${scan.lastError ? ` · ${scan.lastError}` : ''}` : scan?.scanning ? `Scanning · ${scan.probed}/${scan.found}` : 'Never'),
      el('b', {}, 'Activity retention'), el('span', {}, text('activityRetentionDays', '', null, { type: 'number', min: 1, max: 3650, class: 'inline-num' })),
      el('b', {}, 'Identity'), el('span', {}, session.authMode === 'proxy' ? 'OAuth2 proxy headers' : 'Local (no auth)')));

  // ---------- trending ----------
  const trendBody = el('div', {}, el('p', { class: 'muted' }, 'Loading…'));
  const pills = el('div', { class: 'trend-pills' });
  let trendWin = pref.get('trendWindow', 'week');
  let trendSeq = 0;
  const loadTrending = async () => {
    const my = ++trendSeq; // a newer click supersedes this fetch entirely
    for (const b of pills.children) b.classList.toggle('on', b.dataset.w === trendWin);
    try {
      const t = await api.get(`/api/trending?window=${trendWin}`);
      if (my !== trendSeq) return;
      const itemRow = (x, i) => {
        const r = el('button', { class: 'trend-row', type: 'button' },
          el('span', { class: 'rank' }, String(i + 1)),
          el('div', { class: 'grow' }, el('b', {}, x.kind === 'episode' && x.show ? `${x.show} · ${x.title}` : x.title),
            el('small', {}, [x.year, `${x.views} view${x.views === 1 ? '' : 's'}`, x.users > 1 ? `${x.users} people` : null].filter(Boolean).join(' · '))),
          el('span', { class: 't' }, fmtWatch(x.seconds)));
        r.onclick = () => { location.hash = `#/item/${x.itemId}`; };
        return r;
      };
      const userRow = (x, i) => el('div', { class: 'trend-row' },
        el('span', { class: 'rank' }, String(i + 1)),
        el('span', { class: 'avatar sm', 'aria-hidden': 'true' }, initials(x.name)),
        el('div', { class: 'grow' }, el('b', {}, x.name), el('small', {}, `${x.items} title${x.items === 1 ? '' : 's'} · ${x.views} view${x.views === 1 ? '' : 's'}`)),
        el('span', { class: 't' }, fmtWatch(x.seconds)));
      trendBody.replaceChildren(t.items.length ? el('div', { class: 'trend-cols' },
        el('div', {}, el('h3', { class: 'muted-h' }, 'Most watched'), el('div', { class: 'trend-list' }, t.items.map(itemRow))),
        el('div', {}, el('h3', { class: 'muted-h' }, 'Top viewers'), el('div', { class: 'trend-list' }, t.users.map(userRow))))
        : el('p', { class: 'muted' }, 'Nothing watched in this window yet — trends appear as people play things.'));
    } catch (e) { if (my === trendSeq) trendBody.replaceChildren(el('p', { class: 'muted' }, `Trending unavailable: ${e.message}`)); }
  };
  for (const [w, lbl] of [['day', 'Today'], ['week', 'Week'], ['month', 'Month'], ['year', 'Year'], ['all', 'All time']]) {
    const b = el('button', { type: 'button', 'data-w': w }, lbl);
    b.onclick = () => { trendWin = w; pref.set('trendWindow', w); loadTrending(); };
    pills.append(b);
  }
  loadTrending();
  const trending = card('trending', 'Trending', pills, trendBody);

  // ---------- appearance (device-local, saved instantly) ----------
  const themeSel = el('select', {},
    el('option', { value: 'auto' }, 'Follow system'), el('option', { value: 'day' }, 'Day'), el('option', { value: 'night' }, 'Night'));
  themeSel.value = pref.get('theme', 'auto');
  themeSel.onchange = () => { pref.set('theme', themeSel.value); applyAppearance(); };
  const dots = el('div', { class: 'accent-dots' });
  for (const a of ['amber', 'green', 'purple']) {
    const b = el('button', { class: `dot-${a} ${pref.get('accent', 'amber') === a ? 'on' : ''}`, type: 'button', title: a[0].toUpperCase() + a.slice(1), 'aria-label': `${a} accent` });
    b.onclick = () => { pref.set('accent', a); applyAppearance(); for (const d of dots.children) d.classList.toggle('on', d === b); };
    dots.append(b);
  }
  const appearance = card('appearance', 'Appearance',
    el('p', { class: 'muted card-intro' }, 'Per-device; changes apply immediately.'),
    el('div', { class: 'field' }, el('label', {}, 'Theme'), themeSel, hint('Day, night, or follow this device’s system setting.')),
    el('div', { class: 'field' }, el('label', {}, 'Accent'), dots, hint('The beam color across the whole interface.')));

  // ---------- health (admins) ----------
  let healthBox = null, healthTimer = 0;
  if (isAdmin) {
    const grid = el('div', { class: 'health-grid' });
    const kicker = el('div', { class: 'health-kicker' });
    const cell = (title) => {
      const val = el('div', { class: 'val' }, '—');
      const cv = el('canvas');
      const c = el('div', { class: 'health-cell' }, el('h4', {}, title), val, cv);
      return { c, val, cv };
    };
    const cpu = cell('CPU'), mem = cell('Memory'), req = cell('Requests'), net = cell('Data out');
    const storage = el('div', { class: 'health-cell', style: { gridColumn: '1 / -1' } }, el('h4', {}, 'Storage'));
    grid.append(cpu.c, mem.c, req.c, net.c, storage);
    const loadHealth = async () => {
      let h; try { h = await api.get('/api/health'); } catch { return; }
      const ss = h.samples || [], last = ss[ss.length - 1];
      if (!last) { kicker.textContent = 'First sample lands within 15 seconds…'; return; }
      kicker.replaceChildren(icon('check'), el('span', {}, `${last.streams} stream${last.streams === 1 ? '' : 's'} · ${last.transcodes} transcode${last.transcodes === 1 ? '' : 's'} · ${last.recordings} recording · up ${fmtUptime(h.uptimeSec)}`));
      cpu.val.replaceChildren(`${last.cpuPct}`, el('small', {}, '% of one core'));
      histo(cpu.cv, ss.map((x) => x.cpuPct), { max: 100 });
      mem.val.replaceChildren(`${last.rssMb >= 1024 ? (last.rssMb / 1024).toFixed(1) : last.rssMb}`, el('small', {}, last.rssMb >= 1024 ? 'GB resident' : 'MB resident'));
      histo(mem.cv, ss.map((x) => x.rssMb));
      req.val.replaceChildren(`${last.reqPerS}`, el('small', {}, 'req/s'));
      histo(req.cv, ss.map((x) => x.reqPerS));
      const mb = last.kbOutPerS / 1024;
      net.val.replaceChildren(mb >= 1 ? mb.toFixed(1) : `${last.kbOutPerS}`, el('small', {}, mb >= 1 ? 'MB/s out' : 'KB/s out'));
      histo(net.cv, ss.map((x) => x.kbOutPerS));
      storage.replaceChildren(el('h4', {}, 'Storage'), ...(h.disks || []).map((d) => {
        const used = d.totalBytes - d.freeBytes, p = d.totalBytes ? Math.round((used / d.totalBytes) * 100) : 0;
        return el('div', { class: 'disk-row' },
          el('div', { class: 'disk-head' }, el('b', {}, d.label), el('span', { class: 'muted' }, `${fmtGB(used)} of ${fmtGB(d.totalBytes)} · ${fmtGB(d.freeBytes)} free`)),
          el('div', { class: 'disk-bar' }, el('i', { class: p >= 90 ? 'warn' : '', style: { width: `${p}%` } })));
      }), ...((h.disks || []).length ? [] : [el('p', { class: 'muted' }, 'No storage roots visible yet.')]));
    };
    loadHealth();
    healthTimer = setInterval(loadHealth, 15000);
    healthBox = card('health', 'Health', el('p', { class: 'muted card-intro' }, 'Compute, network and storage for this server — sampled every 15 seconds, last hour shown.'), kicker, grid);
  }

  // ---------- setup guide (admins) ----------
  let setupBox = null;
  if (isAdmin) {
    const cmd = (text) => {
      const copy = el('button', { type: 'button', title: 'Copy', 'aria-label': 'Copy command' }, icon('copy'));
      copy.onclick = () => { navigator.clipboard?.writeText(text).then(() => toast('Copied', 'check')); };
      return el('pre', {}, text, copy);
    };
    const step = (n, title, body, ...pres) => el('div', { class: 'setup-step' },
      el('span', { class: 'n' }, String(n)), el('div', { class: 'grow' }, el('b', {}, title), el('p', { class: 'muted', style: { margin: '2px 0 0' } }, body), ...pres));
    setupBox = card('setup', 'Setup guide',
      el('p', { class: 'muted card-intro' }, 'From zero to playing on Kubernetes. Full details live in the chart README.'),
      step(1, 'Storage first: namespace + static volumes', 'Edit the example (namespace, node name, paths), then apply. PVCs are created by the chart and bind to these.',
        cmd(`kubectl create namespace ontele
kubectl apply -f https://raw.githubusercontent.com/ontele/ontele/main/deploy/helm/ontele/examples/static-volumes.yaml`)),
      step(2, 'Install the chart', 'The static-volumes values make every claim bind to your volumes instead of a provisioner.',
        cmd(`helm repo add ontele https://ontele.github.io/ontele
helm install ontele ontele/ontele -n ontele \
  -f deploy/helm/ontele/examples/static-volumes-values.yaml \
  --set persistence.media.hostPath=/media --set dex.enabled=true`)),
      step(3, 'Reach it — pick one', 'NodePort needs no extra infrastructure; Gateway uses Cilium’s Gateway API.',
        cmd(`# NodePort (front door on every node at :30080)
helm upgrade ontele ontele/ontele -n ontele --reuse-values \
  --set ingress.enabled=false --set externalUrl=http://NODE-IP:30080 \
  --set oauth2Proxy.service.type=NodePort --set oauth2Proxy.service.nodePort=30080`),
        cmd(`# Cilium Gateway API (LB IP + TLS)
helm upgrade ontele ontele/ontele -n ontele --reuse-values \
  --set ingress.enabled=false --set httpRoute.host=ontele.example.com \
  --set gateway.enabled=true --set gateway.address=192.168.1.240 \
  --set gateway.tls.secretName=ontele-edge-tls`)),
      step(4, 'Verify', 'Every claim Bound, pods Running, then sign in — the first user becomes admin.',
        cmd(`kubectl -n ontele get pvc,pods`)));
  }

  // ---------- save bar ----------
  const saveBtn = el('button', { class: 'btn primary', type: 'button' }, icon('check'), 'Save settings');
  const discard = el('button', { class: 'btn', type: 'button' }, 'Discard');
  const saveCount = el('span', { class: 'muted' });
  const saveBar = el('div', { class: 'save-bar glass', hidden: true }, saveCount, el('span', { class: 'spacer' }), discard, saveBtn);
  discard.onclick = () => navigate(); // re-runs the route through the router (runs cleanup)
  saveBtn.onclick = () => busy(saveBtn, async () => {
    const body = { ...settings, ...edits };
    if (body.tmdbApiKey === '' && settings.tmdbApiKey === MASK && !dirty.has('tmdbApiKey')) body.tmdbApiKey = MASK;
    const saved = await api.put('/api/settings', body);
    settings = saved && typeof saved === 'object' && Object.keys(saved).length ? saved : body;
    for (const k of Object.keys(edits)) delete edits[k];
    dirty.clear(); saveBar.hidden = true;
  }, 'Settings saved');

  // ---------- nav ----------
  const SECTIONS = isAdmin ? [...BASE_SECTIONS, ...ADMIN_SECTIONS] : BASE_SECTIONS;
  const nav = el('nav', { class: 'settings-nav', 'aria-label': 'Settings sections' });
  const navBtns = new Map();
  for (const [id, label] of SECTIONS) {
    const b = el('button', { type: 'button' }, label);
    b.onclick = () => { $(`#s-${id}`)?.scrollIntoView({ behavior: 'smooth', block: 'start' }); };
    nav.append(b); navBtns.set(id, b);
  }
  const notice = ro ? el('div', { class: 'notice' }, icon('info'), el('span', {}, 'You are viewing settings read-only. Ask an administrator to make changes.')) : null;
  const body = el('div', { class: 'settings-body' }, notice, library, trending, appearance, metadata, playback, livetv, commercials, usersBox, sys, healthBox, setupBox);
  const page = el('div', { class: 'page settings-page' },
    el('div', { class: 'page-head' }, el('h1', {}, 'Settings'), el('span', { class: 'count' }, ro ? 'read-only' : ''), el('span', { class: 'spacer' })),
    el('div', { class: 'settings-grid' }, nav, body),
    saveBar);
  view.replaceChildren(page);

  // scroll spy (rAF-throttled)
  let ticking = false;
  const spy = () => {
    if (ticking) return; ticking = true;
    requestAnimationFrame(() => {
      ticking = false;
      const y = window.scrollY + 140;
      let best = SECTIONS[0][0];
      for (const [id] of SECTIONS) { const s = $(`#s-${id}`); if (s && s.offsetTop <= y) best = id; }
      navBtns.forEach((b, id) => b.classList.toggle('on', id === best));
    });
  };
  window.addEventListener('scroll', spy, { passive: true });
  spy();
  const onKey = (e) => { if ((e.metaKey || e.ctrlKey) && e.key === 's' && !saveBar.hidden) { e.preventDefault(); saveBtn.click(); } };
  document.addEventListener('keydown', onKey);
  return () => { window.removeEventListener('scroll', spy); document.removeEventListener('keydown', onKey); clearInterval(healthTimer); };
}

// =====================================================================
// Activity feed
// =====================================================================
const FAMILIES = [['all', 'All'], ['play', 'Playback'], ['dvr', 'DVR'], ['scan', 'Scans'], ['metadata', 'Metadata'], ['settings', 'Settings'], ['tag', 'Tags'], ['watch', 'Watched']];
const family = (kind) => String(kind || '').split('.')[0];
function detailSummary(ev) {
  const d = ev.detail || {};
  const parts = [];
  const push = (k, v) => { if (v != null && v !== '' && !(Array.isArray(v) && !v.length)) parts.push(`${k} ${Array.isArray(v) ? v.join(', ') : typeof v === 'object' ? JSON.stringify(v) : v}`); };
  if (d.mode) push('mode', d.mode);
  if (d.detector) push('detector', d.detector);
  if (d.breaks != null) push('breaks', d.breaks);
  if (d.channel) push('ch', d.channel);
  if (d.tags) push('tags', d.tags);
  if (d.provider) push('via', d.provider);
  if (d.added != null) push('+', d.added);
  if (d.removed != null) push('−', d.removed);
  if (d.title) push('title', d.title);
  if (d.cut != null) push('cut', d.cut ? 'yes' : 'no');
  if (d.librariesChanged) parts.push('libraries changed');
  if (d.admin != null) push('admin', d.admin ? 'granted' : 'revoked');
  for (const [k, v] of Object.entries(d)) if (!['mode', 'detector', 'breaks', 'channel', 'tags', 'provider', 'added', 'removed', 'title', 'cut', 'librariesChanged', 'admin', 'path', 'updated'].includes(k) && parts.length < 5) push(k, v);
  return parts.join(' · ');
}

export async function renderActivity() {
  ambientFrom(null);
  view.replaceChildren(el('div', { class: 'page' }, el('div', { class: 'page-head' }, el('h1', {}, 'Activity')), el('div', { class: 'feed' }, Array.from({ length: 8 }, () => el('div', { class: 'ev' }, el('div', { class: 'skel-line', style: { width: '48px' } }), el('div', { class: 'skel-line', style: { width: `${40 + Math.random() * 40}%` } }))))));
  let filter = 'all';
  let events = [];
  const feed = el('div', { class: 'feed', role: 'list' });
  const chips = el('div', { class: 'chips' });
  const count = el('span', { class: 'count' });
  const updated = el('span', { class: 'faint act-updated' });

  const renderChips = () => {
    chips.replaceChildren(...FAMILIES.map(([k, label]) => {
      const n = k === 'all' ? events.length : events.filter((e) => family(e.kind) === k).length;
      if (k !== 'all' && !n) return null;
      const b = el('button', { type: 'button', class: `chip ${filter === k ? 'on' : ''}` }, label, el('span', { class: 'faint' }, ` ${n}`));
      b.onclick = () => { filter = k; renderChips(); renderFeed(); };
      return b;
    }).filter(Boolean));
  };
  const renderFeed = () => {
    const shown = events.filter((e) => filter === 'all' || family(e.kind) === filter);
    count.textContent = `${shown.length} event${shown.length === 1 ? '' : 's'}`;
    const frag = document.createDocumentFragment();
    let lastDay = '';
    for (const ev of shown) {
      const day = fmtDateLong(ev.ts);
      if (day !== lastDay) { frag.append(el('div', { class: 'feed-day eyebrow' }, day)); lastDay = day; }
      const full = new Date(ev.ts).toLocaleString();
      const who = ev.user ? el('span', { class: 'who' }, (ev.user.split('@')[0])) : null;
      const title = ev.itemTitle ? (ev.itemId ? el('a', { href: `#/item/${encodeURIComponent(ev.itemId)}`, class: 'ev-title' }, ev.itemTitle) : el('b', {}, ev.itemTitle)) : null;
      const sum = detailSummary(ev);
      frag.append(el('div', { class: 'ev', role: 'listitem' },
        el('time', { datetime: ev.ts, title: full }, fmtAgo(ev.ts)),
        el('div', { class: 'ev-body' },
          el('span', { class: `k f-${family(ev.kind)}` }, ev.kind),
          who, who && title ? ' · ' : null, title, sum ? el('span', { class: 'muted ev-sum' }, (title || who) ? ' · ' : '', sum) : null)));
    }
    feed.replaceChildren(frag);
    if (!shown.length) feed.append(el('p', { class: 'muted' }, 'No events yet.'));
  };
  async function load() {
    try {
      events = await api.get('/api/activity?limit=200');
      updated.textContent = `Updated ${fmtClock(Date.now())}`;
      renderChips(); renderFeed();
    } catch (e) { feed.replaceChildren(emptyState('Activity unavailable', e.message, 'info')); }
  }
  await load();
  view.replaceChildren(el('div', { class: 'page activity-page' },
    el('div', { class: 'page-head' }, el('h1', {}, 'Activity'), count, el('span', { class: 'spacer' }), updated),
    el('div', { class: 'toolbar' }, chips),
    feed));
  const t = setInterval(load, 30000);
  return () => clearInterval(t);
}
