#!/usr/bin/env python3
# Copyright 2026 The Ontele Authors
# SPDX-License-Identifier: Apache-2.0
"""Generate an XMLTV guide from an HDHomeRun tuner.

The tuner's own guide API (api.hdhomerun.com) is queried with the device's
DeviceAuth token, so no external EPG subscription is needed. Point Ontele's
"XMLTV guide source" at the output file (or serve it over HTTP).

Usage:
    hdhr-xmltv.py --device 192.168.1.27 --hours 24 --output /media/guide.xml

Cron example (host with the media share; Ontele sees /media/guide.xml):
    17 */4 * * * /usr/local/bin/hdhr-xmltv.py --device 192.168.1.27 \
        --output /media/video/guide.xml

Only the Python standard library is used.
"""

import argparse
import json
import sys
import time
import urllib.request
from datetime import datetime, timezone
from xml.sax.saxutils import escape, quoteattr

GUIDE_API = "https://api.hdhomerun.com/api/guide"
WINDOW = 4 * 3600  # the guide API returns roughly four-hour slices


def fetch_json(url: str, timeout: float = 15.0):
    req = urllib.request.Request(url, headers={"User-Agent": "ontele-hdhr-xmltv/1.0"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def xmltv_time(epoch: int) -> str:
    return datetime.fromtimestamp(epoch, tz=timezone.utc).strftime("%Y%m%d%H%M%S +0000")


def episode_numbers(prog: dict):
    """XMLTV episode-num lines from an 'SnnEnn' EpisodeNumber, if present."""
    ep = prog.get("EpisodeNumber", "")
    out = []
    if ep.startswith("S") and "E" in ep:
        try:
            season = int(ep[1 : ep.index("E")])
            episode = int(ep[ep.index("E") + 1 :])
            # xmltv_ns is zero-based and must be non-negative: S00Exx
            # specials get only the onscreen form
            if season >= 1 and episode >= 1:
                out.append(("xmltv_ns", f"{season - 1}.{episode - 1}."))
            out.append(("onscreen", ep))
        except ValueError:
            out.append(("onscreen", ep))
    elif ep:
        out.append(("onscreen", ep))
    return out


def collect_guide(device: str, hours: int, verbose: bool = False):
    base = device if device.startswith("http") else f"http://{device}"
    try:
        discover = fetch_json(f"{base}/discover.json")
    except Exception as e:  # noqa: BLE001
        sys.exit(f"ERROR: cannot reach HDHomeRun at {base}: {e}")
    auth = discover.get("DeviceAuth")
    if not auth:
        sys.exit(f"ERROR: {base}/discover.json has no DeviceAuth (tuner too old?)")
    if verbose:
        print(f"# {discover.get('FriendlyName')} {discover.get('DeviceID')}", file=sys.stderr)

    channels: dict[str, dict] = {}
    programmes: dict[str, dict] = {}
    now = int(time.time())
    auth_q = urllib.request.quote(auth, safe="")
    for start in range(now, now + hours * 3600, WINDOW):
        url = f"{GUIDE_API}?DeviceAuth={auth_q}&Start={start}"
        try:
            slice_ = fetch_json(url, timeout=30.0)
        except Exception as e:  # noqa: BLE001 - a missed slice shouldn't kill the run
            print(f"WARNING: guide slice @{start}: {e}", file=sys.stderr)
            continue
        if not slice_:
            break
        for ch in slice_:
            num = ch.get("GuideNumber")
            if not num:
                continue
            channels.setdefault(
                num,
                {
                    "name": ch.get("GuideName", num),
                    "affiliate": ch.get("Affiliate"),
                    "icon": ch.get("ImageURL"),
                },
            )
            for prog in ch.get("Guide", []):
                key = f"{num}/{prog.get('StartTime')}"
                programmes.setdefault(key, {**prog, "_channel": num})
    return channels, programmes


def write_xmltv(out, channels: dict, programmes: dict):
    out.write('<?xml version="1.0" encoding="UTF-8"?>\n')
    out.write('<tv generator-info-name="ontele hdhr-xmltv">\n')
    for num, ch in sorted(channels.items()):
        out.write(f"  <channel id={quoteattr(num)}>\n")
        out.write(f"    <display-name>{escape(ch['name'])}</display-name>\n")
        out.write(f"    <display-name>{escape(num)}</display-name>\n")
        if ch.get("affiliate"):
            out.write(f"    <display-name>{escape(ch['affiliate'])}</display-name>\n")
        if ch.get("icon"):
            out.write(f"    <icon src={quoteattr(ch['icon'])} />\n")
        out.write("  </channel>\n")
    for _, prog in sorted(programmes.items(), key=lambda kv: (kv[1]["_channel"], kv[1].get("StartTime", 0))):
        start, end = prog.get("StartTime"), prog.get("EndTime")
        if not start or not end:
            continue
        out.write(
            f"  <programme start={quoteattr(xmltv_time(start))} "
            f"stop={quoteattr(xmltv_time(end))} channel={quoteattr(prog['_channel'])}>\n"
        )
        # element order per the XMLTV DTD: title, sub-title, desc, date,
        # category, icon, episode-num, previously-shown
        out.write(f"    <title>{escape(prog.get('Title', ''))}</title>\n")
        if prog.get("EpisodeTitle"):
            out.write(f"    <sub-title>{escape(prog['EpisodeTitle'])}</sub-title>\n")
        if prog.get("Synopsis"):
            out.write(f"    <desc>{escape(prog['Synopsis'])}</desc>\n")
        if prog.get("OriginalAirdate"):
            date = datetime.fromtimestamp(prog["OriginalAirdate"], tz=timezone.utc).strftime("%Y%m%d")
            out.write(f"    <date>{date}</date>\n")
        for cat in prog.get("Filter", []):
            out.write(f"    <category>{escape(cat)}</category>\n")
        if prog.get("ImageURL"):
            out.write(f"    <icon src={quoteattr(prog['ImageURL'])} />\n")
        for system, value in episode_numbers(prog):
            out.write(f"    <episode-num system={quoteattr(system)}>{escape(value)}</episode-num>\n")
        # first-run airings carry OriginalAirdate == air date; only mark a
        # repeat when the airing is clearly later than the original
        if prog.get("OriginalAirdate") and start - prog["OriginalAirdate"] >= 86400:
            out.write("    <previously-shown />\n")
        out.write("  </programme>\n")
    out.write("</tv>\n")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--device", required=True, help="HDHomeRun IP/host (or its http URL)")
    ap.add_argument("--hours", type=int, default=24, help="guide depth to fetch (default 24)")
    ap.add_argument("--output", default="-", help="output file (default stdout); written atomically")
    ap.add_argument("-v", "--verbose", action="store_true")
    args = ap.parse_args()

    channels, programmes = collect_guide(args.device, args.hours, args.verbose)
    if not channels:
        sys.exit("ERROR: no guide data returned")
    if args.output == "-":
        write_xmltv(sys.stdout, channels, programmes)
    else:
        tmp = f"{args.output}.tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            write_xmltv(f, channels, programmes)
        import os

        os.replace(tmp, args.output)
    print(
        f"wrote {len(programmes)} programmes across {len(channels)} channels",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
