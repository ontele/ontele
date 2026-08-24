# Ontele architecture (Rust)

Ontele is a single-binary media server: library scanning (movies, TV, music),
metadata enrichment, direct-play + HLS transcoding, HDHomeRun live TV, DVR with
series passes, and commercial detection (skip markers, chapter tags, or hard
cuts). State lives in **PostgreSQL**; identity comes from an **OAuth2 proxy**
in front of the server; logs/metrics go to **Loki / Prometheus / Grafana**.

This document is the contract every module is written against.

## Process layout

```
ontele (axum + tokio)
├── src/main.rs          CLI (clap) → config → db pool + migrations → services → background tasks → HTTP
├── src/lib.rs           module tree; `build_router(state)` for tests
├── src/config.rs        Config (env/flags), bootstrap-only values
├── src/error.rs         AppError → JSON {"error": msg} with status
├── src/state.rs         AppState: PgPool, SettingsCache, Scanner, Art, Streams, Hdhr, Guide, Dvr, Metadata, Activity
├── src/model.rs         serde domain types shared by db + api (camelCase JSON)
├── src/auth.rs          Identity extractor (oauth2-proxy headers), admin guard
├── src/telemetry.rs     tracing (json|pretty), Prometheus recorder, activity() helper
├── src/naming.rs        filename → movie / episode / track classification, junk stripping, auto-tags
├── src/db/              sqlx queries, one file per aggregate (settings, users, items, watch, rules, tags, activity, channels)
├── src/media/           scanner (walk + change detection + probe + upsert), ffprobe, playback decision matrix, artwork + sprites
├── src/metadata/        Provider trait; nfo (Kodi), tmdb, musicbrainz (+Cover Art Archive), music tags (lofty)
├── src/stream/          HLS session manager (ffmpeg children, GC, semaphore), direct play (range), audio streaming, subtitles→VTT
├── src/commercials.rs   comskip / ffmpeg fallback detection, EDL parse, hard cut, chapter tagging
├── src/hdhr.rs          HDHomeRun UDP discovery (libhdhomerun wire format) + discover.json / lineup.json
├── src/epg.rs           XMLTV streaming parser → in-memory guide index (per-channel sorted airings)
├── src/dvr.rs           recording engine: rule matching, capture, post-process, prune
├── src/api/             axum handlers grouped by area
└── src/web.rs           embedded SPA (rust-embed) with ETag/caching + hash-router fallback
ui/                      no-build SPA: index.html, styles.css, js/*.js (ES modules), vendor/ (hls.js, fonts)
migrations/              sqlx migrations (applied at boot)
tests/                   integration tests (#[sqlx::test] needs DATABASE_URL)
deploy/                  docker-compose + k8s + grafana/loki/promtail/prometheus/oauth2-proxy config
```

## Identity

Ontele never handles credentials. It trusts the headers that
[oauth2-proxy](https://oauth2-proxy.github.io/oauth2-proxy/) injects when run
with `--pass-user-headers --set-xauthrequest`:

| Header (first non-empty wins)                                         | Field      |
|-----------------------------------------------------------------------|------------|
| `X-Forwarded-Email`, `X-Auth-Request-Email`                           | email      |
| `X-Forwarded-Preferred-Username`, `X-Auth-Request-Preferred-Username` | username   |
| `X-Forwarded-User`, `X-Auth-Request-User`                             | subject    |
| `X-Forwarded-Groups`, `X-Auth-Request-Groups`                         | groups     |

`ONTELE_AUTH=proxy` (default in containers): a request without a subject/email
is `401`. `ONTELE_AUTH=none`: everyone is the single `local` user (dev/LAN).
Admins: members of `ONTELE_ADMIN_GROUPS` or listed in `ONTELE_ADMIN_USERS`
(email or username). If neither is set, the **first user ever seen** becomes
admin. Admin is required for `PUT /api/settings`, scan, DVR rule deletion of
other users' rules, and tag deletion.

Users are upserted into `users` on every request (cheap: one indexed upsert,
cached in-process for 60 s per subject).

## Database (PostgreSQL ≥ 14)

```sql
users        (id bigserial pk, subject text unique, email text, name text, groups text[], is_admin bool, created timestamptz, last_seen timestamptz)
settings     (id int pk check (id = 1), data jsonb, updated timestamptz)
items        (id text pk, kind text, path text unique, title text, sort_title text, year int,
              show text, season int, episode int, episode_end int, air_date date,
              artist text, album_artist text, album text, album_id text, track_no int, disc_no int, genre text,
              subtitle text, description text,
              channel_id text, channel_name text, start_at timestamptz, end_at timestamptz, status text, error text, rule_id text,
              breaks jsonb, breaks_state text,
              info jsonb, meta jsonb, auto_tags text[], size_bytes bigint, mtime timestamptz,
              added timestamptz, updated timestamptz)
shows        (key text pk /* lower(show) */, name text, meta jsonb, updated timestamptz)
albums       (id text pk /* blake3(lower(album_artist)|lower(album))[..16] */, artist text, title text, year int, meta jsonb, updated timestamptz)
watch        (user_id bigint, item_id text, pos double precision, dur double precision, done bool, updated timestamptz, pk (user_id, item_id))
rules        (id text pk, title text, channel_id text, keep int, user_id bigint, created timestamptz)
tags         (id serial pk, name text unique)
item_tags    (item_id text, tag_id int, pk (item_id, tag_id))
activity     (id bigserial pk, ts timestamptz, user_id bigint, kind text, item_id text, detail jsonb)
channels     (guide_number text pk, guide_name text, url text, hd bool, icon text, updated timestamptz)
```

`items.kind ∈ {movie, episode, track, recording}`. Recording-only columns are
null for library items and vice versa. `info` is `MediaInfo`, `meta` is
`Metadata` (below). Search uses `pg_trgm` GIN indexes on `title`, `show`,
`album`, `artist`.

`items.id` = first 16 hex chars of `blake3(path)`.

## JSON shapes (camelCase)

```ts
MediaInfo {
  durationSec: number, container: string, sizeBytes: number, bitrate?: number,
  vcodec?: string, acodec?: string, width?: number, height?: number,     // flattened convenience
  video?: { index, codec, profile?, width, height, fps?, bitDepth?, hdr?: "hdr10"|"hdr10plus"|"hlg"|"dv", interlaced?: bool },
  audio:  [{ index, codec, channels, lang?, title?, default: bool }],
  subtitles: [{ index, codec, lang?, title?, forced: bool, text: bool, external?: string }],
  chapters: [{ start, end, title }]
}
Metadata {
  provider?: "tmdb"|"musicbrainz"|"nfo", providerId?: string,
  tmdbId?, imdbId?, tvdbId?, mbid?: string,
  originalTitle?, overview?, tagline?, genres: string[], rating?: number, votes?: number,
  runtimeMin?, releaseDate?, contentRating?, studio?,
  cast: [{ name, character?, profile? }],
  posterUrl?, backdropUrl?, stillUrl?, logoUrl?,
  updated?: string
}
Item {  // unified card/detail shape
  id, kind, title, subtitle?, sortTitle?, year?, show?, season?, episode?, episodeEnd?, airDate?,
  artist?, albumArtist?, album?, albumId?, trackNo?, discNo?, genre?,
  description?, duration, vcodec?, acodec?, width?, height?, container?, hdr?,
  added, watch?: WatchState, breaks?: Break[], breaksState?, status?, error?, channel?, channelId?, start?, end?,
  tags: string[], autoTags: string[], meta?: Metadata, info?: MediaInfo (detail only)
}
WatchState { pos, dur, done, updated }
Break { start, end }
Settings (see model.rs; superset of the Go version; camelCase)
```

## API

All JSON. Errors: `{"error": "..."}` with 4xx/5xx. Times are RFC 3339.

```
GET  /healthz | /readyz | /metrics
GET  /api/me                                  → { user:{id,subject,email,name,isAdmin}, authMode }
GET  /api/home                                → { continue[], recordings[], movies[], episodes[], albums[], upNext[] }
GET  /api/movies?sort=title|added|year&tag=&genre=&q=
GET  /api/shows                               → [{ show, episodes, seasons, posterId, meta? }]
GET  /api/shows/{show}                        → { show, meta, seasons:[{ season, episodes:Item[] }] }
GET  /api/items/{id}                          → Item (detail, with info + meta + tags + nextEpisode?)
GET  /api/items/{id}/subtitles                → [{ index, lang, title, codec, text, url }]
GET  /api/items/{id}/subtitles/{idx}.vtt
GET  /api/items/{id}/sprites.vtt | sprites.jpg
POST /api/items/{id}/refresh-metadata
PUT  /api/items/{id}/metadata   { title?, year?, tmdbId? }  (manual match)
GET  /api/search?q=                           → { movies[], episodes[], shows[], albums[], tracks[], artists[], channels[], recordings[], airings[] }
POST /api/scan                                → 202 { status: "scanning" }
GET  /api/scan/status                         → { scanning, found, probed, added, removed, startedAt?, finishedAt?, lastError? }
GET  /api/img/{id}?type=poster|backdrop|thumb|still&w=
POST /api/watch/{id}  { pos, dur, done? }
GET  /api/tags ; POST /api/items/{id}/tags { tags:[..] } ; DELETE /api/items/{id}/tags/{tag}
GET  /api/music/artists ; GET /api/music/artists/{name}
GET  /api/music/albums?artist= ; GET /api/music/albums/{id}   → { album, tracks:Item[] }
GET  /api/music/tracks?album=&q=
GET  /stream/audio/{id}?fmt=auto|aac|mp3|opus&t=
GET  /api/livetv/channels ; POST /api/livetv/refresh ; GET /api/livetv/icon/{num}
GET  /api/guide?hours=N&from=RFC3339          → { updated, from, to, channels:[{ guideNumber, guideName, icon, airings:[..] }] }
GET  /api/dvr/recordings ; POST /api/dvr/record ; DELETE /api/dvr/recordings/{id}
POST /api/dvr/recordings/{id}/adscan?cut=1
GET  /api/dvr/rules ; POST /api/dvr/rules ; DELETE /api/dvr/rules/{id}
GET  /api/settings ; PUT /api/settings (admin) ; GET /api/settings/probe → { ffmpeg, ffprobe, comskip, hwaccels[], encoders[] }
GET  /api/activity?limit=  ; GET /api/stats
POST /api/stream/start { id?|channel?, start?, quality?, audio?, subtitle?, caps? }
                                              → { sessionId, url, offset, live, mode:"copy"|"transcode", segment:"ts"|"fmp4" }
POST /api/stream/{sid}/keepalive ; DELETE /api/stream/{sid}
GET  /stream/direct/{id} ; GET /stream/hls/{sid}/{file}
```

## Playback decision

The client reports capabilities (`caps`) from `MediaSource.isTypeSupported` /
`canPlayType`; the server decides:

1. **direct** — container ∈ caps.containers and vcodec ∈ caps.video and acodec
   ∈ caps.audio and no forced-burn subtitle → `/stream/direct/{id}`.
2. **copy** (remux to HLS) — vcodec ∈ caps.video: `-c:v copy`; segments are
   fMP4 for hevc/av1/vp9/vp8, MPEG-TS for h264/mpeg2 (transcoded). Audio is
   copied when the codec is in caps.audio, else AAC 2.0 @160k.
3. **transcode** — libx264 (or hwaccel encoder) at the requested ladder rung
   (2160/1080/720/480/360), `-force_key_frames` every segment, `yadif` for live,
   HDR sources tone-mapped to BT.709 (`zscale` + `tonemap=hable`).

Text subtitles are never burned in: they are served as WebVTT sidecars
(`/api/items/{id}/subtitles/{n}.vtt`) and attached by the player. Bitmap
subtitles (PGS/VobSub) are burned in via `overlay`, which forces a transcode.
Audio that MPEG-TS cannot carry (flac/alac/vorbis/pcm/truehd) is re-encoded
to AAC even when the client could decode it.

## Observability

- Logs: JSON lines to stdout (`ONTELE_LOG_FORMAT=json|pretty`). Every request
  logs one line (`target=ontele.http`) with method, route, status, latency,
  user. Domain events log with `target=ontele.activity` and are also inserted
  into `activity`.
- Metrics: Prometheus at `/metrics` — `ontele_http_requests_total{method,route,status}`,
  `ontele_http_request_duration_seconds{route}`, `ontele_streams_active`,
  `ontele_transcodes_active`, `ontele_recordings_active`,
  `ontele_playback_starts_total{mode}`, `ontele_library_items{kind}`,
  `ontele_scan_duration_seconds`, `ontele_metadata_lookups_total{provider,result}`,
  `ontele_commercial_scans_total{detector,result}`, `ontele_activity_total{kind}`,
  `ontele_guide_airings`, `ontele_livetv_channels`.
- Promtail ships container logs to Loki; Grafana is provisioned with Loki +
  Prometheus datasources and an "Ontele" dashboard.

## Background tasks

| task               | cadence                       |
|--------------------|-------------------------------|
| library scan       | boot + every `scanIntervalMin` + fs watcher debounce (2 s) + `POST /api/scan` |
| metadata enrich    | queue drained after scan, rate-limited per provider |
| tuner + guide      | boot + every `guideRefreshHours` |
| DVR tick           | 15 s |
| stream GC          | 30 s (idle > 90 s killed) |
| activity retention | daily |
| settings flush     | immediate (single-row upsert) |
