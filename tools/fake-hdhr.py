#!/usr/bin/env python3
# Copyright 2026 The Ontele Authors — SPDX-License-Identifier: Apache-2.0
"""A fake HDHomeRun + XMLTV guide for developing without a tuner.

    python3 tools/fake-hdhr.py --port 5004 --xmltv /tmp/guide.xml

Serves /discover.json and /lineup.json like a real tuner, streams an MPEG-2
transport stream (ffmpeg testsrc, looped) at /auto/v<channel>, and writes an
XMLTV guide for the fake lineup. Point Ontele at it:

    Settings → HDHomeRun IP = 127.0.0.1:5004, XMLTV = /tmp/guide.xml
"""
import argparse, json, subprocess, sys, threading
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

CHANNELS = [("2.1", "KCBS-DT", 1), ("4.1", "KNBC-DT", 1), ("7.1", "KABC-DT", 1), ("11.1", "KTTV-DT", 1), ("28.1", "KCET-DT", 1), ("13.1", "KCOP-DT", 0)]
PROGS = ["Evening News", "Jeopardy!", "Wheel of Fortune", "Nature", "NOVA", "Friends", "Seinfeld", "Frasier", "Local Weather", "The Simpsons", "Bob's Burgers", "60 Minutes", "Masterpiece", "Antiques Roadshow", "Saturday Night Live", "The Late Show"]


def write_xmltv(path, hours=36):
    now = datetime.now(timezone.utc).replace(minute=0, second=0, microsecond=0) - timedelta(hours=2)
    fmt = lambda d: d.strftime("%Y%m%d%H%M%S +0000")
    out = ['<?xml version="1.0" encoding="UTF-8"?>', '<tv generator-info-name="fake-hdhr">']
    for num, name, _ in CHANNELS:
        cid = f"I{num.replace('.', '')}.fake"
        out.append(f'  <channel id="{cid}"><display-name>{num} {name}</display-name><display-name>{name}</display-name><display-name>{num}</display-name><icon src="http://127.0.0.1:{ARGS.port}/icon/{num}.svg"/></channel>')
    k = 0
    for num, name, _ in CHANNELS:
        cid = f"I{num.replace('.', '')}.fake"
        t = now
        while t < now + timedelta(hours=hours):
            dur = [30, 30, 60, 60, 90, 120][(k * 7 + int(t.timestamp() // 1800)) % 6]
            title = PROGS[(k * 5 + int(t.timestamp() // 1800)) % len(PROGS)]
            ep = int(t.timestamp() // 3600) % 22
            out.append(f'  <programme start="{fmt(t)}" stop="{fmt(t + timedelta(minutes=dur))}" channel="{cid}">'
                       f'<title lang="en">{title}</title><sub-title lang="en">Episode {ep + 1}</sub-title>'
                       f'<desc lang="en">{title}: a synthetic guide entry for testing the DVR and the EPG grid.</desc>'
                       f'<category lang="en">Series</category><episode-num system="xmltv_ns">3.{ep}.0/1</episode-num>'
                       f'{"<new/>" if ep % 4 == 0 else ""}</programme>')
            t += timedelta(minutes=dur)
        k += 1
    out.append("</tv>")
    with open(path, "w") as f:
        f.write("\n".join(out))
    print(f"wrote {path}")


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def send_json(self, obj):
        b = json.dumps(obj).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def do_GET(self):
        base = f"http://127.0.0.1:{ARGS.port}"
        if self.path == "/discover.json":
            return self.send_json({"FriendlyName": "Fake HDHomeRun", "ModelNumber": "HDHR5-4US", "FirmwareName": "hdhomerun5_atsc", "FirmwareVersion": "20240101", "DeviceID": "FAKE0001", "DeviceAuth": "x", "BaseURL": base, "LineupURL": f"{base}/lineup.json", "TunerCount": 4})
        if self.path == "/lineup.json":
            return self.send_json([{"GuideNumber": n, "GuideName": name, "HD": hd, "URL": f"{base}/auto/v{n}"} for n, name, hd in CHANNELS])
        if self.path.startswith("/icon/"):
            n = self.path[6:].replace(".svg", "")
            b = f'<svg xmlns="http://www.w3.org/2000/svg" width="120" height="80"><rect width="120" height="80" rx="12" fill="#fff"/><text x="60" y="50" text-anchor="middle" font-family="sans-serif" font-weight="700" font-size="26" fill="#0a0b10">{n}</text></svg>'.encode()
            self.send_response(200); self.send_header("Content-Type", "image/svg+xml"); self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b); return
        if self.path.startswith("/auto/v"):
            num = self.path[7:]
            hue = (int(float(num or "1") * 37)) % 360
            # broadcast-like: MPEG-2 video, AC-3 audio, interlaced 1080i-ish, in a TS
            cmd = ["ffmpeg", "-v", "error", "-re", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30", "-f", "lavfi", "-i", f"sine=frequency={300 + hue}:sample_rate=48000",
                   "-vf", f"hue=h={hue}:s=1.3", "-c:v", "mpeg2video", "-b:v", "6M", "-c:a", "ac3", "-b:a", "192k", "-f", "mpegts", "-"]
            self.send_response(200)
            self.send_header("Content-Type", "video/mp2t")
            self.end_headers()
            p = subprocess.Popen(cmd, stdout=subprocess.PIPE)
            try:
                while True:
                    chunk = p.stdout.read(188 * 64)
                    if not chunk:
                        break
                    self.wfile.write(chunk)
            except (BrokenPipeError, ConnectionResetError):
                pass
            finally:
                p.kill()
            return
        self.send_response(404); self.end_headers()


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=5004)
    ap.add_argument("--xmltv", default="/tmp/fake-guide.xml")
    ARGS = ap.parse_args()
    write_xmltv(ARGS.xmltv)
    print(f"fake HDHomeRun on http://127.0.0.1:{ARGS.port}  (set hdhrIp=127.0.0.1:{ARGS.port})")
    ThreadingHTTPServer(("127.0.0.1", ARGS.port), H).serve_forever()
