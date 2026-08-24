# Ontele

Single-binary media server in Rust: movies, TV and music with real metadata,
direct play or HLS transcoding for anything ffmpeg can read, HDHomeRun live
TV with an EPG grid, DVR with series passes, and commercial detection that
can mark, chapter-tag or hard-cut ad breaks. Identity comes from an OAuth2
proxy; state lives in PostgreSQL; logs and metrics flow to Loki, Prometheus
and Grafana. The web UI is a no-build SPA embedded in the binary.

```
┌──────────┐   X-Forwarded-Email    ┌─────────────┐     ┌────────────┐
│ browser  │ ─► oauth2-proxy ─────► │   ontele    │ ──► │ PostgreSQL │
└──────────┘        │               │ axum+tokio  │     └────────────┘
                    └──► grafana    │ ffmpeg/comskip      ▲ promtail ─► loki
                         (/grafana) └─────────────┘ ──► /metrics ─► prometheus
```

## What it does

| Area | Details |
|---|---|
| **Library** | Scans folders for 40+ video and 30+ audio container types; classifies by filename *and* folder structure (`Show/Season 02/S02E05 - Title.mkv`, `Movie (2019)/movie.mkv`, `[Group] Anime - 07.mkv`, date-based episodes, `Artist/Album/01 - Track.flac`); strips release junk; derives quality tags (4K, HDR, Dolby Vision, Remux…). Rescans are change-detected by size+mtime and a filesystem watcher picks up new files within seconds. |
| **Metadata** | Kodi NFO sidecars → TMDB (movies, shows, episodes, DVR recordings) → embedded music tags (ID3/Vorbis/FLAC/MP4 via lofty) + MusicBrainz/Cover Art Archive for albums. Posters, backdrops, episode stills, cast, genres, ratings, content ratings. Sidecar art (`poster.jpg`, `fanart.jpg`, `cover.jpg`) wins; frame grabs are the fallback. Manual "fix match" per item. User tags on anything. |
| **Playback** | The browser reports what it can decode; the server picks **direct play** (range requests), **remux** (copy video into HLS — fMP4 for HEVC/AV1/VP9, TS for H.264) or **transcode** (x264 or VA-API/QSV/NVENC/VideoToolbox, HDR tone-mapped, ladder 2160→360). Audio track selection, text subtitles as WebVTT, bitmap subtitles burned in, scrub-bar thumbnails, chapters, resume, up-next autoplay. Music streams direct or transcodes to AAC/MP3/Opus on the fly. |
| **Live TV** | HDHomeRun discovery (UDP broadcast or pinned IP), lineup, any XMLTV guide (URL/file/.gz, streamed so 300 MB guides are fine), channel logos, EPG grid with a now-line, one-click watch/record/series pass. |
| **DVR** | Series passes with keep-N, pre/post padding, manual recordings, missed-airing detection, crash recovery. Capture is a raw copy of the tuner's MPEG-TS, then a stream-copy remux to MKV. |
| **Commercials** | Comskip when installed (EDL), else a black-frame ∩ silence heuristic. `skip` stores breaks (player auto-skips, gold markers on the scrubber) and optionally writes them as MKV chapters; `delete` cuts them out with keyframe-snapped stream copies. Re-scan / cut on demand. |
| **Identity** | Trusts oauth2-proxy headers (`X-Forwarded-Email/User/Groups`). Per-user watch state, continue-watching and up-next. Admins from `ONTELE_ADMIN_USERS`/`_GROUPS` (first user becomes admin otherwise). |
| **Observability** | JSON logs with an `ontele.activity` event stream and an `ontele.http` access log; Prometheus metrics (`/metrics`); provisioned Grafana dashboard (requests, latency, streams, activity, scans, errors). In-app Activity page. |

## Quick start (Docker Compose)

**Zero-config trial** — a bundled identity provider (Dex) with two local
users, so nothing needs to be registered with Google/GitHub/etc.:

```bash
cd deploy
docker compose -f docker-compose.yml -f docker-compose.dev-idp.yml up -d --build
open http://localhost:4180            # sign in: admin@example.com / password  (admin)
                                      #          viewer@example.com / password (member)
open http://localhost:4180/grafana/   # same login, same identity
```

Library paths default to `./media`, `./music`, `./recordings` next to the
compose file; override with `MEDIA_DIR=… MUSIC_DIR=… docker compose …` or a
`.env`. Users live in `deploy/dex/config.yaml` (bcrypt hashes).

**Real identity provider** (any OIDC issuer, or GitHub/Google/Entra via
oauth2-proxy's providers):

```bash
cd deploy
cp .env.example .env           # OIDC client id/secret, cookie secret, paths, admin email
docker compose up -d --build
```

Every `${VAR:-default}` in `docker-compose.yml` is a development default
(`ontele` database password, a fixed cookie secret, the Dex issuer URL);
set real values in `.env` before exposing the stack beyond localhost.
Only oauth2-proxy publishes a port; Ontele, Postgres, Loki, Prometheus and
Grafana sit on the compose network. Point the HDHomeRun at
`ONTELE_HDHR=<tuner ip>` unless you switch the container to host networking.

## Quick start (Kubernetes)

Helm (recommended — every companion is a toggle, secrets are generated and
kept, a values schema catches typos):

```bash
helm repo add ontele https://ontele.github.io/ontele   # or install straight from the checkout:
helm install ontele deploy/helm/ontele -n media --create-namespace \
  --set ingress.host=ontele.example.com --set persistence.media.hostPath=/tank/media \
  --set dex.enabled=true            # trial login: admin@example.com / password
helm test ontele -n media
```

See [deploy/helm/ontele/README.md](deploy/helm/ontele/README.md) for
production values (your OIDC provider, existing secrets, NFS libraries,
hostNetwork for tuner discovery, `/dev/dri` for hardware transcoding) and
for pre-provisioned/static PVs — create them before installing with
[deploy/helm/ontele/examples/static-volumes.yaml](deploy/helm/ontele/examples/static-volumes.yaml).

Plain manifests (same stack, hand-editable):

```bash
kubectl apply -f deploy/k8s/          # or: kubectl apply -k deploy/k8s
```

Edit `10-config.yaml` (admin users, tuner IP, XMLTV), replace
`11-secrets.example.yaml` with real secrets, set the host in
`40-oauth2-proxy.yaml`, and bind `ontele-media` to your library volume.
`deploy/k8s/gen-configmaps.sh` regenerates the Loki/Promtail/Grafana
ConfigMaps from the shared files under `deploy/`. The manifests reference
`ghcr.io/ontele/ontele:latest`; the CI workflow publishes that image from
`main`, or build your own with
`docker buildx build --platform linux/amd64,linux/arm64 -t <registry>/ontele --push .`
and change the image in `30-ontele.yaml`.

## Running from source

```bash
# Postgres anywhere you like
docker run -d --name pg -e POSTGRES_PASSWORD=ontele -e POSTGRES_USER=ontele -e POSTGRES_DB=ontele -p 5432:5432 postgres:18-alpine

cargo run --release -- \
  --database-url postgres://ontele:ontele@localhost/ontele \
  --auth none \
  --media-dirs /tank/movies,/tank/tv --music-dirs /tank/music \
  --recordings-dir /tank/dvr --xmltv /tank/guide/xmltv.xml --commercials skip
# open http://localhost:7979
```

Flags / env only **bootstrap** settings on first run; afterwards the Settings
page (persisted in Postgres) owns them, so redeploys never clobber tuned
values. `--auth none` makes everyone the `local` admin — use it on a trusted
LAN or for development only.

Runtime dependencies: `ffmpeg` + `ffprobe` (required), `comskip`
(recommended, built into the Docker image), PostgreSQL ≥ 14 with `pg_trgm`
(the stack ships 18). The image is Debian trixie with ffmpeg 7.1.

## Configuration

| Env / flag | Default | Purpose |
|---|---|---|
| `DATABASE_URL` | — | Postgres connection string (required) |
| `ONTELE_ADDR` | `0.0.0.0:7979` | listen address |
| `ONTELE_DATA` | `./ontele-data` | artwork cache, HLS scratch, sprites, subtitles |
| `ONTELE_MEDIA`, `ONTELE_MUSIC` | — | comma-separated library dirs (bootstrap) |
| `ONTELE_RECORDINGS` | `<data>/recordings` | DVR output (bootstrap) |
| `ONTELE_XMLTV`, `ONTELE_HDHR` | — | guide source, tuner IP (bootstrap) |
| `ONTELE_COMMERCIALS` | `skip` | `off` / `skip` / `delete` (bootstrap) |
| `ONTELE_TMDB_API_KEY` | — | TMDB v3 key (bootstrap; also in Settings) |
| `ONTELE_AUTH` | `proxy` | `proxy` (trust oauth2-proxy headers) or `none` |
| `ONTELE_ADMIN_USERS` / `ONTELE_ADMIN_GROUPS` | — | who may change settings, scan, delete |
| `ONTELE_LOG_FORMAT` | `json` | `json` or `pretty` |
| `RUST_LOG` | `info,sqlx=warn` | tracing filter |

Everything else (hardware acceleration, transcode preset and concurrency,
metadata providers/language, padding, auto-delete-watched, chapter tagging,
scan interval, filesystem watching, thumbnails, activity retention) lives in
**Settings**.

## Naming

Movies: `Title (2019).mkv`, `Title.2019.2160p.Remux-GRP.mkv`, or a
`Title (2019)/` folder with any file name. Episodes: `Show S01E02 Title.mkv`,
`Show - 1x02.mkv`, `Show/Season 01/E02 - Title.mkv`, `Show S01E01-E02`,
`Show 2024-01-15.mkv`, `[Group] Show - 07 [1080p].mkv`. Music: tags first,
then `Artist/Album (Year)/01 - Title.flac`. Sidecars: `poster.jpg`,
`fanart.jpg`, `<stem>-thumb.jpg`, `cover.jpg`, `<stem>.en.srt`,
`movie.nfo`/`tvshow.nfo`/`<stem>.nfo` (Kodi).

## API

All JSON, all under identity. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)
for the full surface; highlights:

```
GET  /api/home | /api/movies?sort=&genre=&tag= | /api/shows | /api/shows/{show} | /api/items/{id}
GET  /api/search?q= | /api/genres | /api/tags         POST /api/items/{id}/tags   DELETE /api/items/{id}/tags/{tag}
GET  /api/music/artists | /api/music/albums | /api/music/albums/{id} | /api/music/tracks
GET  /api/img/{id}?type=poster|backdrop|thumb|still&w=   GET /api/items/{id}/sprites.vtt | /subtitles/{n}.vtt
POST /api/watch/{id} {pos,dur}   POST /api/scan   GET /api/scan/status   POST /api/items/{id}/refresh-metadata
GET  /api/livetv/channels | /api/guide?hours=&from=     POST /api/livetv/refresh
GET  /api/dvr/recordings | /api/dvr/rules   POST /api/dvr/record | /api/dvr/rules   POST /api/dvr/recordings/{id}/adscan[?cut=1]
POST /api/stream/start {id|channel,start,quality,audio,subtitle,caps} → {url,sessionId,mode}
GET  /stream/direct/{id} | /stream/hls/{sid}/{file} | /stream/audio/{id}?fmt=
GET/PUT /api/settings   GET /api/settings/probe | /api/activity | /api/stats | /api/me   GET /healthz | /readyz | /metrics
```

## Development

```bash
docker run -d --name ontele-pg -e POSTGRES_PASSWORD=ontele -e POSTGRES_USER=ontele -e POSTGRES_DB=ontele -p 55432:5432 postgres:18-alpine
cargo test --lib                                                         # unit tests (no services needed)
DATABASE_URL=postgres://ontele:ontele@localhost:55432/ontele cargo test --all-targets   # + Postgres/ffmpeg integration tests
cargo clippy --all-targets -- -D warnings && cargo fmt --all --check

tools/sample-library.sh /tmp/ontele-sample   # synthetic movies / shows / tagged FLACs (needs ffmpeg)
python3 tools/fake-hdhr.py --port 5004 --xmltv /tmp/guide.xml   # fake tuner + XMLTV; set hdhrIp=127.0.0.1:5004
python3 tools/mockapi.py                     # UI against fixtures on :7980, no backend at all
```

Tests use Rust's built-in harness (`cargo test`): ~160 unit tests cover the
pure logic (naming, probe parsing, playback matrix, ffmpeg argument builders,
EDL/XMLTV/HDHomeRun wire parsing, tags); `tests/` holds `#[sqlx::test]`
integration suites (API + identity, DVR scheduler/capture against a fake
tuner, and a full scan → art → direct play → HLS → subtitles → audio run on
ffmpeg-synthesized media). Database tests get a throwaway database per test
with `migrations/` applied; ffmpeg-dependent tests skip themselves when ffmpeg
is absent. CI (`.github/workflows/ci.yml`) runs all of it plus the deploy
manifest validation and a multi-arch image build.

## License

Apache-2.0 — see `LICENSE`; bundled components in `NOTICE` (hls.js, Sora and
Inter fonts). Comskip (GPL-2.0) is an optional external program executed at
runtime; it is neither bundled with nor linked into this binary.
