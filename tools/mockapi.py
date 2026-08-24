#!/usr/bin/env python3
# Copyright 2026 The Ontele Authors — SPDX-License-Identifier: Apache-2.0
"""Mock Ontele API for UI development without the Rust backend.

    python3 tools/mockapi.py [--port 7980] [--ui ui]

Serves the SPA from ./ui plus fixture JSON for every /api route the UI uses,
SVG placeholder artwork, and (if ffmpeg is on PATH) a real sample video/audio
for the players. Not used in production.
"""
import argparse, hashlib, json, os, random, subprocess, sys, time
from datetime import datetime, timedelta, timezone
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse, parse_qs, unquote

NOW = datetime.now(timezone.utc)
ISO = lambda d: d.isoformat().replace('+00:00', 'Z')
random.seed(7)

def info(h=1080, dur=5400, v='h264', a='aac', c='mkv'):
    return {"durationSec": dur, "container": c, "sizeBytes": 4_000_000_000, "vcodec": v, "acodec": a, "width": 1920 if h == 1080 else 3840, "height": h,
            "video": {"index": 0, "codec": v, "width": 1920, "height": h, "fps": 23.976, "hdr": "hdr10" if h == 2160 else None},
            "audio": [{"index": 1, "codec": a, "channels": 6, "lang": "eng", "title": "English 5.1", "default": True}, {"index": 2, "codec": "ac3", "channels": 2, "lang": "fra", "title": "Français"}],
            "subtitles": [{"index": 3, "codec": "subrip", "lang": "eng", "text": True}, {"index": 4, "codec": "hdmv_pgs_subtitle", "lang": "eng", "forced": True, "text": False}],
            "chapters": [{"start": 0, "end": 900, "title": "Opening"}, {"start": 900, "end": 2700, "title": "Act II"}, {"start": 2700, "end": dur, "title": "Finale"}]}

def meta(overview, genres, rating, year, tagline=None, cast=None, cr='PG-13', runtime=None, studio=None):
    return {"provider": "tmdb", "overview": overview, "genres": genres, "rating": rating, "votes": 12345, "releaseDate": f"{year}-05-12", "contentRating": cr,
            "tagline": tagline, "runtimeMin": runtime, "studio": studio, "cast": cast or [{"name": n, "character": c} for n, c in [("Mara Vance", "Lena"), ("Theo Okafor", "Jules"), ("Priya Natarajan", "Dr. Ines"), ("Sam Whitlock", "The Courier"), ("Ada Lindqvist", "Nova")]]}

MOVIES = [
    ("m1", "Blade Circuit", 2023, 2160, "A courier with a stolen neural key crosses a city that wants her erased before dawn.", ["Science Fiction", "Thriller"], 7.8, "Every signal has a price."),
    ("m2", "Heat", 1995, 1080, "A relentless detective and a disciplined master thief circle each other across Los Angeles.", ["Crime", "Drama"], 8.3, "A Los Angeles crime saga."),
    ("m3", "Static Signal", 2021, 1080, "A radio astronomer intercepts a broadcast that is answering questions she hasn't asked yet.", ["Mystery", "Science Fiction"], 7.1, "Someone is listening back."),
    ("m4", "The Lantern Road", 2019, 1080, "Two estranged brothers walk the length of a mountain pilgrimage carrying their mother's ashes.", ["Drama"], 7.4),
    ("m5", "Midnight Arcade", 2024, 2160, "After hours, the machines in a dying arcade keep playing a game nobody programmed.", ["Horror", "Comedy"], 6.9, "Insert coin. Keep breathing."),
    ("m6", "Paper Tigers", 2017, 720, "Three friends who peaked in a kung-fu school reunite to avenge their master.", ["Action", "Comedy"], 6.7),
    ("m7", "Orbital Decay", 2022, 1080, "The last crew aboard a decommissioned station has 96 hours before re-entry.", ["Science Fiction"], 7.0),
    ("m8", "Saltwater", 2020, 1080, "A marine biologist returns to the fishing village that never forgave her father.", ["Drama", "Romance"], 7.2),
    ("m9", "Dune Part Two", 2024, 2160, "Paul Atreides unites with the Fremen while seeking revenge against the conspirators who destroyed his family.", ["Science Fiction", "Adventure"], 8.5, "Long live the fighters."),
    ("m10", "Alien", 1979, 1080, "The crew of a commercial spacecraft encounter a deadly lifeform after investigating an unknown transmission.", ["Horror", "Science Fiction"], 8.5, "In space no one can hear you scream."),
]
SHOWS = {
    "Severance": [(1, 1, "Good News About Hell"), (1, 2, "Half Loop"), (1, 3, "In Perpetuity"), (1, 4, "The You You Are"), (1, 9, "The We We Are"), (2, 1, "Hello, Ms. Cobel"), (2, 2, "Goodbye, Mrs. Selvig")],
    "The Expanse": [(1, 1, "Dulcinea"), (1, 2, "The Big Empty"), (2, 11, "Here There Be Dragons"), (3, 1, "Fight or Flight")],
    "Cowboy Bebop": [(1, 1, "Asteroid Blues"), (1, 5, "Ballad of Fallen Angels"), (1, 26, "The Real Folk Blues Part 2")],
    "Frieren": [(1, 1, "The Journey's End"), (1, 7, "Like a Fairy Tale")],
}
SHOW_META = {
    "Severance": meta("Mark leads a team of office workers whose memories have been surgically divided between their work and personal lives.", ["Drama", "Mystery", "Science Fiction"], 8.7, 2022, cr="TV-MA", studio="Apple"),
    "The Expanse": meta("Two hundred years in the future, a detective and a ship's officer uncover a conspiracy that threatens the solar system.", ["Science Fiction", "Drama"], 8.5, 2015, cr="TV-14"),
    "Cowboy Bebop": meta("The futuristic misadventures of a bounty hunter crew aboard the Bebop.", ["Animation", "Action"], 8.9, 1998, cr="TV-14"),
    "Frieren": meta("An elf mage sets out on a new journey after the hero's party disbands.", ["Animation", "Adventure", "Fantasy"], 8.9, 2023, cr="TV-14"),
}
ALBUMS = [
    ("Daft Punk", "Discovery", 2001, ["One More Time", "Aerodynamic", "Digital Love", "Harder, Better, Faster, Stronger", "Crescendolls", "Nightvision", "Superheroes", "High Life"]),
    ("Radiohead", "In Rainbows", 2007, ["15 Step", "Bodysnatchers", "Nude", "Weird Fishes/Arpeggi", "All I Need", "Reckoner", "House of Cards", "Videotape"]),
    ("Khruangbin", "Mordechai", 2020, ["First Class", "Time (You and I)", "Connaissais de Face", "Father Bird, Mother Bird", "Pelota", "So We Won't Forget"]),
    ("Hiroshi Yoshimura", "Green", 1986, ["Creek", "Feel", "Sleep", "Street", "Green", "Teevee", "Feet"]),
    ("Nils Frahm", "All Melody", 2018, ["The Whole Universe Wants to Be Touched", "Sunson", "A Place", "My Friend the Forest", "Human Range", "Forever Changeless"]),
    ("Little Simz", "Sometimes I Might Be Introvert", 2021, ["Introvert", "Woman", "Two Worlds Apart", "I Love You, I Hate You", "Point and Kill"]),
]
CHANNELS = [("2.1", "KCBS", True), ("4.1", "KNBC", True), ("5.1", "KTLA", True), ("7.1", "KABC", True), ("11.1", "KTTV", True), ("13.1", "KCOP", False), ("28.1", "KCET", True), ("34.1", "KMEX", True), ("50.1", "KOCE", True), ("58.1", "KLCS", False)]
PROGS = ["Evening News", "Jeopardy!", "Wheel of Fortune", "The Late Show", "Nature", "NOVA", "Antiques Roadshow", "Friends", "Seinfeld", "Frasier", "Local Weather", "Movie: Heat", "Sunday Night Football", "Masterpiece", "Cooking with Ana", "The Simpsons", "Bob's Burgers", "60 Minutes", "Dateline", "Saturday Night Live"]

def aid(s): return hashlib.blake2b(s.encode(), digest_size=8).hexdigest()
WATCH = {"m2": {"pos": 2100, "dur": 10200, "done": False, "updated": ISO(NOW - timedelta(hours=3))},
         "e-Severance-1-1": {"pos": 3000, "dur": 3100, "done": True, "updated": ISO(NOW - timedelta(days=1))},
         "e-Severance-1-2": {"pos": 1200, "dur": 3100, "done": False, "updated": ISO(NOW - timedelta(hours=20))}}
TAGS = {"m2": ["crime classic", "date night"], "m9": ["4k showcase"]}

def movie(mid, title, year, h, over, genres, rating, tag=None, card=True):
    it = {"id": mid, "kind": "movie", "title": title, "year": year, "duration": 6000 + (hash(mid) % 3600), "vcodec": "hevc" if h == 2160 else "h264", "acodec": "eac3" if h == 2160 else "aac",
          "width": 3840 if h == 2160 else 1920, "height": h, "container": "mkv", "hdr": "hdr10" if h == 2160 else None, "added": ISO(NOW - timedelta(days=int(mid[1:]) * 3)),
          "tags": TAGS.get(mid, []), "autoTags": ["4K", "HDR", "Remux"] if h == 2160 else ["1080p", "Blu-ray"], "meta": meta(over, genres, rating, year, tag, runtime=118)}
    if mid in WATCH: it["watch"] = WATCH[mid]
    if not card: it["info"] = info(h, it["duration"])
    return it

def episode(show, s, e, title, card=True):
    i = f"e-{show.replace(' ', '')}-{s}-{e}"
    it = {"id": i, "kind": "episode", "title": title, "show": show, "season": s, "episode": e, "duration": 2700 + (e * 60), "vcodec": "h264", "acodec": "aac", "width": 1920, "height": 1080, "container": "mkv",
          "added": ISO(NOW - timedelta(days=(s * 10 + e))), "tags": [], "autoTags": ["1080p", "Web"], "description": f"{title}: the team faces a new test as the truth behind the severed floor comes closer.",
          "meta": {"provider": "tmdb", "overview": f"{title}: the team faces a new test as the truth behind the severed floor comes closer.", "genres": [], "stillUrl": None, "rating": 8.1 + (e % 5) / 10}}
    if i in WATCH: it["watch"] = WATCH[i]
    if not card: it["info"] = info(1080, it["duration"])
    return it

def track(artist, album, n, title, card=True):
    al = aid(f"{artist.lower()}|{album.lower()}")
    tid = f"t-{al}-{n}"
    it = {"id": tid, "kind": "track", "title": title, "artist": artist, "albumArtist": artist, "album": album, "albumId": al, "trackNo": n, "discNo": 1, "duration": 180 + (n * 23) % 200, "acodec": "flac", "container": "flac",
          "added": ISO(NOW - timedelta(days=n)), "tags": [], "autoTags": [], "genre": "Electronic"}
    if not card: it["info"] = {"durationSec": it["duration"], "container": "flac", "sizeBytes": 30_000_000, "acodec": "flac", "audio": [{"index": 0, "codec": "flac", "channels": 2}], "subtitles": [], "chapters": []}
    return it

def album_summary(artist, album, year, tracks):
    al = aid(f"{artist.lower()}|{album.lower()}")
    return {"id": al, "artist": artist, "title": album, "year": year, "tracks": len(tracks), "duration": sum(180 + (n * 23) % 200 for n in range(1, len(tracks) + 1)), "artId": f"t-{al}-1", "added": ISO(NOW - timedelta(days=year % 30)),
            "meta": {"provider": "musicbrainz", "genres": ["Electronic", "House"] if "Daft" in artist else ["Alternative"], "releaseDate": f"{year}-03-01"}}

RECORDINGS = []
def mkrec(i, title, sub, ch, start_off, status, bs=None, breaks=None):
    st = NOW + timedelta(minutes=start_off)
    r = {"id": f"r{i}", "kind": "recording", "title": title, "subtitle": sub, "channelId": ch[0], "channel": ch[1], "start": ISO(st), "end": ISO(st + timedelta(minutes=60)), "status": status, "duration": 3600 if status == "done" else 0,
         "vcodec": "mpeg2video" if status == "done" else None, "acodec": "ac3", "width": 1920, "height": 1080, "container": "mkv", "added": ISO(st), "tags": [], "autoTags": [], "breaksState": bs, "breaks": breaks,
         "meta": {"provider": "tmdb", "overview": f"{title} — {sub}", "genres": ["Talk"]}}
    if status == "failed": r["error"] = "tuner: 503 Service Unavailable (all tuners busy?)"
    RECORDINGS.append(r); return r
mkrec(1, "Jeopardy!", "Tournament of Champions, Game 4", CHANNELS[3], -1440, "done", "ready", [{"start": 410, "end": 600}, {"start": 1250, "end": 1440}, {"start": 2100, "end": 2280}])
mkrec(2, "Evening News", None, CHANNELS[0], -2880, "done", "cut")
mkrec(3, "NOVA", "Secrets of the Sun", CHANNELS[6], -4320, "done", "pending")
mkrec(4, "The Late Show", "Guest: Priya Natarajan", CHANNELS[0], -5, "recording")
mkrec(5, "Nature", "Wild Scandinavia", CHANNELS[6], 120, "scheduled")
mkrec(6, "Jeopardy!", "Tournament of Champions, Game 5", CHANNELS[3], 1380, "scheduled")
mkrec(7, "Antiques Roadshow", None, CHANNELS[8], -7000, "failed")
RULES = [{"id": "ru1", "title": "Jeopardy!", "channelId": "7.1", "keep": 5, "created": ISO(NOW - timedelta(days=30))}, {"id": "ru2", "title": "NOVA", "keep": 0, "created": ISO(NOW - timedelta(days=12))}]
SETTINGS = {"mediaDirs": ["/tank/movies", "/tank/tv"], "musicDirs": ["/tank/music"], "recordingsDir": "/tank/dvr", "xmltvUrl": "/tank/guide/xmltv.xml.gz", "hdhrIp": "", "commercialMode": "skip", "commercialChapters": True,
            "comskipPath": "comskip", "ffmpegPath": "ffmpeg", "ffprobePath": "ffprobe", "prePadMin": 1, "postPadMin": 2, "autoDeleteWatched": False, "tmdbApiKey": "••••••", "metadataProviders": {"nfo": True, "tmdb": True, "musicbrainz": True},
            "metadataLanguage": "en-US", "hwaccel": "none", "transcodePreset": "veryfast", "maxTranscodes": 3, "scanIntervalMin": 15, "watchFilesystem": True, "guideRefreshHours": 4, "thumbnails": True, "activityRetentionDays": 90}
ACTIVITY = [{"id": i, "ts": ISO(NOW - timedelta(minutes=i * 17)), "user": ["alice@example.com", "bob@example.com"][i % 2], "kind": k, "itemId": "m2", "itemTitle": t, "detail": d} for i, (k, t, d) in enumerate([
    ("play.start", "Heat", {"mode": "direct"}), ("dvr.finished", "Jeopardy!", {"detector": "comskip", "breaks": 3}), ("watch.done", "Severance", {}), ("scan.done", None, {"added": 12, "removed": 0}),
    ("metadata.enriched", "Blade Circuit", {"provider": "tmdb"}), ("play.live", None, {"channel": "7.1"}), ("dvr.rule.add", None, {"title": "NOVA"}), ("settings.update", None, {}), ("tag.add", "Heat", {"tags": ["date night"]})])]

def guide(hours, frm):
    chans = []
    for gn, name, hd in CHANNELS:
        t = frm.replace(minute=0, second=0, microsecond=0) - timedelta(minutes=30)
        airs = []
        k = int(gn.split('.')[0])
        while t < frm + timedelta(hours=hours + 1):
            dur = random.choice([30, 30, 60, 60, 90, 120])
            title = PROGS[(k * 7 + int(t.timestamp() // 1800)) % len(PROGS)]
            airs.append({"channelId": gn, "title": title, "subtitle": random.choice([None, "Episode 12", "Season premiere", "Part 2"]), "description": f"{title}. A long-running favorite with regular guests and the occasional surprise.", "start": ISO(t), "end": ISO(t + timedelta(minutes=dur)), "categories": ["Series"], "new": random.random() < .2, "season": 3, "episode": 12})
            t += timedelta(minutes=dur)
        chans.append({"guideNumber": gn, "guideName": name, "hd": hd, "icon": f"/api/livetv/icon/{gn}", "airings": airs})
    return chans

def svg_art(key, kind):
    h = int(hashlib.md5(key.encode()).hexdigest()[:6], 16)
    hue, hue2 = h % 360, (h // 360) % 360
    w, ht = (480, 720) if kind == "poster" else (1280, 720) if kind == "backdrop" else (640, 360)
    label = key.split(':')[-1][:22]
    return f'''<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{ht}" viewBox="0 0 {w} {ht}"><defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="hsl({hue},70%,38%)"/><stop offset="1" stop-color="hsl({hue2},80%,18%)"/></linearGradient><radialGradient id="r" cx=".3" cy=".25" r=".8"><stop offset="0" stop-color="hsl({(hue+40)%360},90%,70%)" stop-opacity=".7"/><stop offset="1" stop-color="#000" stop-opacity="0"/></radialGradient></defs><rect width="100%" height="100%" fill="url(#g)"/><rect width="100%" height="100%" fill="url(#r)"/><circle cx="{w*0.72}" cy="{ht*0.68}" r="{min(w,ht)*0.28}" fill="hsl({(hue+180)%360},70%,50%)" opacity=".35"/><text x="50%" y="{ht-40 if kind=='poster' else ht/2}" text-anchor="middle" fill="#fff" opacity=".85" font-family="Sora,Inter,sans-serif" font-weight="700" font-size="{int(w/14)}">{label}</text></svg>'''.encode()

SAMPLE = {}
def ensure_samples(tmp):
    os.makedirs(tmp, exist_ok=True)
    vid, aud = os.path.join(tmp, "sample.mp4"), os.path.join(tmp, "sample.m4a")
    try:
        if not os.path.exists(vid):
            subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-t", "120", "-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p", "-c:a", "aac", "-movflags", "+faststart", vid], check=True)
        if not os.path.exists(aud):
            subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=330:sample_rate=44100", "-t", "90", "-c:a", "aac", "-b:a", "128k", aud], check=True)
        SAMPLE["video"], SAMPLE["audio"] = vid, aud
    except Exception as e:
        print("no ffmpeg samples:", e, file=sys.stderr)

class H(SimpleHTTPRequestHandler):
    def log_message(self, fmt, *a): pass
    def send_json(self, obj, code=200):
        b = json.dumps(obj).encode()
        self.send_response(code); self.send_header("Content-Type", "application/json"); self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def send_bytes(self, b, ctype):
        self.send_response(200); self.send_header("Content-Type", ctype); self.send_header("Content-Length", str(len(b))); self.send_header("Cache-Control", "public, max-age=3600"); self.end_headers(); self.wfile.write(b)
    def send_file(self, path, ctype):
        size = os.path.getsize(path); rng = self.headers.get("Range")
        start, end = 0, size - 1
        if rng and rng.startswith("bytes="):
            a, _, b = rng[6:].partition("-"); start = int(a or 0); end = int(b) if b else size - 1
            self.send_response(206); self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        else: self.send_response(200)
        self.send_header("Content-Type", ctype); self.send_header("Accept-Ranges", "bytes"); self.send_header("Content-Length", str(end - start + 1)); self.end_headers()
        with open(path, "rb") as f:
            f.seek(start); rem = end - start + 1
            while rem > 0:
                chunk = f.read(min(65536, rem));
                if not chunk: break
                self.wfile.write(chunk); rem -= len(chunk)
    def do_DELETE(self): self.send_json({"ok": True})
    def do_PUT(self):
        n = int(self.headers.get("Content-Length", 0)); body = json.loads(self.rfile.read(n) or b"{}")
        if self.path.startswith("/api/settings"): SETTINGS.update(body); return self.send_json(SETTINGS)
        self.send_json({"ok": True})
    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0)); body = json.loads(self.rfile.read(n) or b"{}")
        p = urlparse(self.path).path
        if p == "/api/stream/start":
            if body.get("channel"): return self.send_json({"sessionId": "live1", "url": "/stream/direct/sample", "offset": 0, "live": True, "mode": "transcode", "segment": "ts"})
            q = body.get("quality", "auto")
            if q in ("auto", "direct"): return self.send_json({"sessionId": None, "url": "/stream/direct/sample", "offset": 0, "live": False, "mode": "direct", "plan": {"mode": "direct", "videoCopy": True, "audioCopy": True, "height": 0, "segment": "ts", "reasons": ["h264/aac in mp4"]}})
            return self.send_json({"sessionId": "s1", "url": "/stream/direct/sample", "offset": body.get("start", 0), "live": False, "mode": "transcode", "segment": "ts", "plan": {"mode": "transcode", "videoCopy": False, "audioCopy": False, "height": int(q) if q.isdigit() else 720, "segment": "ts", "reasons": ["requested " + q]}})
        if p.startswith("/api/watch/"):
            WATCH[p.split("/")[3]] = {"pos": body.get("pos", 0), "dur": body.get("dur", 0), "done": body.get("done", False) or (body.get("dur", 0) and body["pos"] / body["dur"] > .95), "updated": ISO(datetime.now(timezone.utc))}
            return self.send_json({"ok": True})
        if p == "/api/scan": return self.send_json({"status": "scanning"}, 202)
        if p == "/api/dvr/rules": r = {"id": f"ru{len(RULES)+1}", "title": body["title"], "channelId": body.get("channelId"), "keep": body.get("keep", 0), "created": ISO(NOW)}; RULES.insert(0, r); return self.send_json(r)
        if p == "/api/dvr/record": r = mkrec(len(RECORDINGS) + 1, body.get("title", "Manual"), body.get("subtitle"), CHANNELS[0], 60, "scheduled"); return self.send_json(r)
        if p.endswith("/tags"): TAGS.setdefault(p.split("/")[3], []).extend(body.get("tags", [])); return self.send_json({"tags": TAGS[p.split("/")[3]]})
        if p.endswith("/adscan"): return self.send_json(RECORDINGS[0])
        if p.endswith("/refresh-metadata"): return self.send_json(MOVIE_ITEMS[0])
        if p == "/api/livetv/refresh": return self.send_json({"channels": []})
        self.send_json({"ok": True})
    def do_GET(self):
        u = urlparse(self.path); p = unquote(u.path); q = parse_qs(u.query)
        if p.startswith("/api/img/"):
            key = p[len("/api/img/"):]; kind = q.get("type", ["poster"])[0]
            return self.send_bytes(svg_art(key, kind), "image/svg+xml")
        if p.startswith("/api/livetv/icon/"):
            n = p.rsplit("/", 1)[1]; return self.send_bytes(f'<svg xmlns="http://www.w3.org/2000/svg" width="120" height="80"><rect width="120" height="80" rx="12" fill="#fff" opacity=".9"/><text x="60" y="50" text-anchor="middle" font-family="Sora,sans-serif" font-weight="700" font-size="26" fill="#0a0b10">{n}</text></svg>'.encode(), "image/svg+xml")
        if p.startswith("/stream/direct/"):
            if SAMPLE.get("video"): return self.send_file(SAMPLE["video"], "video/mp4")
            return self.send_json({"error": "no ffmpeg sample"}, 404)
        if p.startswith("/stream/audio/"):
            if SAMPLE.get("audio"): return self.send_file(SAMPLE["audio"], "audio/mp4")
            return self.send_json({"error": "no ffmpeg sample"}, 404)
        if p.startswith("/api/"): return self.api(p, q)
        return super().do_GET()
    def api(self, p, q):
        if p == "/api/trending":
            w = q.get("window", ["week"])[0]
            items = [{"itemId": f"m{i}", "title": t, "kind": "movie", "show": None, "year": 2020 + i,
                      "seconds": 9000 - i * 1700, "views": 6 - i, "users": max(1, 4 - i)}
                     for i, t in enumerate(["Signal Fade", "Northern Static", "Glass Harbor", "Cold Boot"])]
            users = [{"userId": i + 1, "name": n, "seconds": 12000 - i * 3500, "views": 8 - i * 2, "items": 5 - i}
                     for i, n in enumerate(["Alice Rivera", "Sam Okafor", "Ren Ito"])]
            return self.send_json({"window": w, "items": items, "users": users})
        if p == "/api/health":
            import math, time as _t
            now = int(_t.time())
            samples = [{"at": now - (120 - i) * 15, "cpuPct": round(18 + 14 * math.sin(i / 7) + (i % 5), 1),
                        "rssMb": round(210 + 30 * math.sin(i / 11), 1), "streams": 1 + (i // 40) % 2,
                        "transcodes": (i // 60) % 2, "recordings": 0,
                        "reqPerS": round(3 + 2.5 * abs(math.sin(i / 5)), 1),
                        "kbOutPerS": round(800 + 700 * abs(math.sin(i / 6)), 1)} for i in range(120)]
            disks = [{"label": "data", "path": "/data", "totalBytes": 500 * 10**9, "freeBytes": 320 * 10**9},
                     {"label": "recordings", "path": "/recordings", "totalBytes": 4 * 10**12, "freeBytes": 900 * 10**9},
                     {"label": "media", "path": "/media", "totalBytes": 8 * 10**12, "freeBytes": 2 * 10**12}]
            return self.send_json({"samples": samples, "disks": disks, "sampleEverySec": 15, "uptimeSec": 86400 * 3 + 7200})
        if p == "/api/me": return self.send_json({"user": {"id": 1, "subject": "alice", "email": "alice@example.com", "name": "Alice Rivera", "isAdmin": True, "groups": [], "created": ISO(NOW), "lastSeen": ISO(NOW)}, "authMode": "proxy", "version": "0.1.0-mock"})
        if p == "/api/home":
            cont = [it for it in MOVIE_ITEMS + EP_ITEMS if it.get("watch") and not it["watch"]["done"]]
            return self.send_json({"continue": cont, "upNext": [episode("Severance", 1, 3, "In Perpetuity")], "recordings": [r for r in RECORDINGS if r["status"] == "done"], "movies": MOVIE_ITEMS, "episodes": [EP_ITEMS[0], EP_ITEMS[7], EP_ITEMS[11]], "albums": ALBUM_ITEMS})
        if p == "/api/movies":
            items = list(MOVIE_ITEMS); s = q.get("sort", ["title"])[0]; tag = q.get("tag", [None])[0]; g = q.get("genre", [None])[0]
            if tag: items = [i for i in items if tag in i["tags"] or tag in i["autoTags"]]
            if g: items = [i for i in items if g in i["meta"]["genres"]]
            if q.get("unwatched"): items = [i for i in items if not i.get("watch", {}).get("done")]
            items.sort(key={"added": lambda i: i["added"], "year": lambda i: -i["year"], "rating": lambda i: -i["meta"]["rating"]}.get(s, lambda i: i["title"].lower()), reverse=(s == "added"))
            return self.send_json(items)
        if p == "/api/genres": return self.send_json({"movies": [{"name": g, "count": sum(1 for m in MOVIE_ITEMS if g in m["meta"]["genres"])} for g in sorted({g for m in MOVIE_ITEMS for g in m["meta"]["genres"]})], "shows": []})
        if p == "/api/shows": return self.send_json([{"show": s, "episodes": len(eps), "seasons": len({e[0] for e in eps}), "posterId": f"e-{s.replace(' ', '')}-{eps[0][0]}-{eps[0][1]}", "meta": SHOW_META[s], "year": int(SHOW_META[s]["releaseDate"][:4]), "added": ISO(NOW - timedelta(days=len(s))), "watched": sum(1 for e in eps if WATCH.get(f"e-{s.replace(' ', '')}-{e[0]}-{e[1]}", {}).get("done"))} for s, eps in SHOWS.items()])
        if p.startswith("/api/shows/"):
            name = p[len("/api/shows/"):]; key = next((s for s in SHOWS if s.lower() == name.lower()), None)
            if not key: return self.send_json({"error": "unknown show"}, 404)
            seasons = {}
            for s, e, t in SHOWS[key]: seasons.setdefault(s, []).append(episode(key, s, e, t))
            return self.send_json({"show": key, "meta": SHOW_META[key], "seasons": [{"season": s, "episodes": eps} for s, eps in sorted(seasons.items())]})
        if p.startswith("/api/items/") and p.endswith("/subtitles"): return self.send_json([{"index": 0, "streamIndex": 3, "lang": "eng", "title": "English", "codec": "subrip", "text": True, "url": "/api/items/x/subtitles/0.vtt"}, {"index": 1, "streamIndex": 4, "lang": "eng", "title": "English (forced)", "codec": "hdmv_pgs_subtitle", "forced": True, "text": False}])
        if p.startswith("/api/items/") and ".vtt" in p: return self.send_bytes(b"WEBVTT\n\n00:00:01.000 --> 00:00:04.000\nEvery signal has a price.\n\n00:00:05.000 --> 00:00:09.000\n<i>Mock subtitles look like this.</i>\n", "text/vtt")
        if p.startswith("/api/items/") and "sprites" in p: return self.send_json({"error": "no sprites in mock"}, 404)
        if p.startswith("/api/items/"):
            i = p.split("/")[3]; it = ALL.get(i)
            if not it: return self.send_json({"error": "unknown item"}, 404)
            it = dict(it); it["tags"] = TAGS.get(i, it.get("tags", []))
            if it["kind"] == "episode":
                eps = SHOWS[it["show"]]; idx = [(s, e) for s, e, _ in eps].index((it["season"], it["episode"]))
                if idx + 1 < len(eps): s, e, t = eps[idx + 1]; it["nextEpisode"] = episode(it["show"], s, e, t)
                it["meta"] = dict(it["meta"], backdropUrl="x", genres=SHOW_META[it["show"]]["genres"], contentRating="TV-MA")
            return self.send_json(it)
        if p == "/api/search":
            s = q.get("q", [""])[0].lower()
            return self.send_json({"movies": [m for m in MOVIE_ITEMS if s in m["title"].lower()][:8], "episodes": [e for e in EP_ITEMS if s in e["title"].lower() or s in e["show"].lower()][:8], "shows": [{"show": k, "episodes": len(v), "seasons": 2, "posterId": "x", "watched": 0, "added": ISO(NOW)} for k, v in SHOWS.items() if s in k.lower()], "albums": [a for a in ALBUM_ITEMS if s in a["title"].lower() or s in a["artist"].lower()], "tracks": [t for t in TRACK_ITEMS if s in t["title"].lower()][:8], "artists": [{"name": a, "albums": 1, "tracks": 8, "artId": ALBUM_ITEMS[i]["artId"]} for i, (a, *_r) in enumerate(ALBUMS) if s in a.lower()], "channels": [{"guideNumber": g, "guideName": n, "hd": hd, "now": {"title": "Evening News"}} for g, n, hd in CHANNELS if s in n.lower() or g.startswith(s)], "recordings": [r for r in RECORDINGS if s in r["title"].lower()], "airings": [{"channelId": "7.1", "title": "Jeopardy!", "subtitle": "Game 6", "start": ISO(NOW + timedelta(hours=5)), "end": ISO(NOW + timedelta(hours=5, minutes=30))}] if "jeo" in s else []})
        if p == "/api/scan/status": return self.send_json({"scanning": False, "found": 4210, "probed": 4210, "added": 0, "updated": 0, "removed": 0, "startedAt": ISO(NOW - timedelta(minutes=40)), "finishedAt": ISO(NOW - timedelta(minutes=38))})
        if p == "/api/tags": return self.send_json([{"name": t, "count": 1} for ts in TAGS.values() for t in ts])
        if p == "/api/music/artists": return self.send_json([{"name": a, "albums": 1, "tracks": len(tr), "artId": ALBUM_ITEMS[i]["artId"]} for i, (a, _al, _y, tr) in enumerate(ALBUMS)])
        if p.startswith("/api/music/artists/"):
            name = p.rsplit("/", 1)[1]; albs = [a for a in ALBUM_ITEMS if a["artist"].lower() == name.lower()]
            return self.send_json({"name": name, "artId": albs[0]["artId"] if albs else "x", "albums": albs}) if albs else self.send_json({"error": "unknown artist"}, 404)
        if p == "/api/music/albums":
            art = q.get("artist", [None])[0]; return self.send_json([a for a in ALBUM_ITEMS if not art or a["artist"].lower() == art.lower()])
        if p.startswith("/api/music/albums/"):
            i = p.rsplit("/", 1)[1]; al = next((a for a in ALBUM_ITEMS if a["id"] == i), None)
            if not al: return self.send_json({"error": "unknown album"}, 404)
            return self.send_json({"album": al, "tracks": [t for t in TRACK_ITEMS if t["albumId"] == i]})
        if p == "/api/music/tracks": return self.send_json(TRACK_ITEMS[:50])
        if p == "/api/livetv/channels":
            g = guide(3, NOW); chans = []
            for c in g:
                cur = next((a for a in c["airings"] if a["start"] <= ISO(NOW) < a["end"]), None); nxt = next((a for a in c["airings"] if a["start"] > ISO(NOW)), None)
                chans.append({"guideNumber": c["guideNumber"], "guideName": c["guideName"], "hd": c["hd"], "icon": c["icon"], "url": "", "now": cur, "next": nxt})
            return self.send_json({"device": {"DeviceID": "1234ABCD", "ModelNumber": "HDHR5-4US", "TunerCount": 4, "FirmwareVersion": "20240101", "BaseURL": "http://192.168.1.50"}, "channels": chans, "guideUpdated": ISO(NOW - timedelta(hours=2))})
        if p == "/api/guide":
            hours = int(q.get("hours", ["6"])[0]); frm = datetime.fromisoformat(q["from"][0].replace("Z", "+00:00")) if q.get("from") else NOW
            return self.send_json({"updated": ISO(NOW - timedelta(hours=2)), "from": ISO(frm), "to": ISO(frm + timedelta(hours=hours)), "channels": guide(hours, frm)})
        if p == "/api/dvr/recordings": return self.send_json(RECORDINGS)
        if p == "/api/dvr/rules": return self.send_json(RULES)
        if p == "/api/settings/probe": return self.send_json({"ffmpeg": "ffmpeg version 7.1.1", "ffprobe": "ffprobe version 7.1.1", "comskip": True, "hwaccels": ["videotoolbox", "vaapi"], "encoders": ["libx264", "h264_videotoolbox", "hevc_videotoolbox"], "dataDir": "/data", "uptimeSec": 86400 * 3})
        if p == "/api/settings": return self.send_json(SETTINGS)
        if p == "/api/activity": return self.send_json(ACTIVITY)
        if p == "/api/stats": return self.send_json({"items": {"movie": 412, "episode": 3120, "track": 8421, "recording": 37}, "streams": 2, "transcodes": 1, "recordingsActive": 1, "channels": len(CHANNELS), "guideUpdated": ISO(NOW - timedelta(hours=2)), "scan": {"scanning": False}, "uptimeSec": 86400 * 3, "version": "0.1.0-mock"})
        if p == "/api/users": return self.send_json([{"id": 1, "subject": "alice", "email": "alice@example.com", "name": "Alice Rivera", "groups": ["admins"], "isAdmin": True, "created": ISO(NOW), "lastSeen": ISO(NOW)}, {"id": 2, "subject": "bob", "email": "bob@example.com", "name": "Bob Tanaka", "groups": [], "isAdmin": False, "created": ISO(NOW), "lastSeen": ISO(NOW - timedelta(days=2))}])
        self.send_json({"error": "mock: no route " + p}, 404)

MOVIE_ITEMS = [movie(*m) for m in MOVIES]
EP_ITEMS = [episode(s, *e) for s, eps in SHOWS.items() for e in eps]
ALBUM_ITEMS = [album_summary(*a) for a in ALBUMS]
TRACK_ITEMS = [track(a, al, n, t) for a, al, _y, tr in ALBUMS for n, t in enumerate(tr, 1)]
ALL = {i["id"]: i for i in MOVIE_ITEMS + EP_ITEMS + TRACK_ITEMS + RECORDINGS}
for m in MOVIES: ALL[m[0]] = movie(*m, card=False)
for s, eps in SHOWS.items():
    for e in eps: ALL[f"e-{s.replace(' ', '')}-{e[0]}-{e[1]}"] = episode(s, *e, card=False)
for r in RECORDINGS: r["info"] = info(1080, 3600, "mpeg2video", "ac3")

if __name__ == "__main__":
    ap = argparse.ArgumentParser(); ap.add_argument("--port", type=int, default=7980); ap.add_argument("--ui", default="ui"); ap.add_argument("--tmp", default="/tmp/ontele-mock")
    a = ap.parse_args()
    ensure_samples(a.tmp)
    os.chdir(a.ui)
    class Handler(H):
        def translate_path(self, path):
            p = urlparse(path).path
            full = super().translate_path(p)
            return full if os.path.isfile(full) else os.path.join(os.getcwd(), "index.html")
    print(f"mock ontele on http://localhost:{a.port}")
    ThreadingHTTPServer(("0.0.0.0", a.port), Handler).serve_forever()
