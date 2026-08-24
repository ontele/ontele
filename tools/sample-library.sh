#!/usr/bin/env bash
# Copyright 2026 The Ontele Authors — SPDX-License-Identifier: Apache-2.0
# Generates a small synthetic media library with ffmpeg (colour bars + tones)
# for trying Ontele without real files:
#   tools/sample-library.sh /tmp/ontele-sample
#   cargo run -- --auth none --database-url ... --media-dirs /tmp/ontele-sample/movies,/tmp/ontele-sample/tv --music-dirs /tmp/ontele-sample/music
set -euo pipefail
ROOT="${1:-/tmp/ontele-sample}"
DUR="${DUR:-45}"
mkdir -p "$ROOT"/{movies,tv,music}

# drawtext needs an ffmpeg built with libfreetype; skip the timestamp overlay otherwise
if ffmpeg -hide_banner -filters 2>/dev/null | grep -q ' drawtext '; then
  DRAW=",drawtext=text='%{pts\\:hms}':x=40:y=40:fontsize=42:fontcolor=white:box=1:boxcolor=black@0.5"
else
  DRAW=""
fi
vid() { # vid <out> <seed> <seconds> [extra ffmpeg args]
  local out="$1" seed="$2" secs="$3"; shift 3
  ffmpeg -v error -y -f lavfi -i "testsrc2=size=1280x720:rate=25" -f lavfi -i "sine=frequency=$((200 + seed * 37)):sample_rate=48000" \
    -t "$secs" -vf "hue=h=$((seed * 47 % 360)):s=1.4$DRAW" \
    -c:v libx264 -preset veryfast -pix_fmt yuv420p -g 50 -c:a aac -b:a 128k "$@" "$out"
}

i=1
for m in "Blade Circuit (2023)" "Static Signal (2021)" "Midnight Arcade (2024)" "The Lantern Road (2019)" "Orbital Decay (2022)" "Saltwater (2020)"; do
  d="$ROOT/movies/$m"; mkdir -p "$d"
  [ -f "$d/$m.mp4" ] || vid "$d/$m.mp4" "$i" "$DUR" -movflags +faststart
  i=$((i + 1))
done
# an mkv with an embedded subtitle track + a sidecar srt
d="$ROOT/movies/Heat (1995)"; mkdir -p "$d"
if [ ! -f "$d/Heat.1995.1080p.BluRay.x264-GRP.mkv" ]; then
  printf '1\n00:00:02,000 --> 00:00:05,000\nA Los Angeles crime saga.\n\n2\n00:00:08,000 --> 00:00:12,000\nSynthetic subtitles, real pipeline.\n' > "$d/Heat.1995.1080p.BluRay.x264-GRP.en.srt"
  vid "$d/Heat.1995.1080p.BluRay.x264-GRP.mkv" 9 "$DUR"
fi

s=1
for show in "Severance" "The Expanse"; do
  for season in 1 2; do
    d="$ROOT/tv/$show/Season 0$season"; mkdir -p "$d"
    for ep in 1 2 3; do
      f="$d/$show - S0${season}E0${ep} - Episode $ep.mkv"
      [ -f "$f" ] || vid "$f" "$((s * 10 + season * 3 + ep))" "$((DUR / 2))"
    done
  done
  s=$((s + 1))
done

t=1
for album in "Daft Punk|Discovery|2001|One More Time,Aerodynamic,Digital Love,Harder Better Faster Stronger" \
             "Radiohead|In Rainbows|2007|15 Step,Bodysnatchers,Nude,Weird Fishes" \
             "Khruangbin|Mordechai|2020|First Class,Time (You and I),Pelota"; do
  IFS='|' read -r artist title year tracks <<< "$album"
  d="$ROOT/music/$artist/$title ($year)"; mkdir -p "$d"
  n=1
  IFS=',' read -ra TR <<< "$tracks"
  for tr in "${TR[@]}"; do
    f="$d/$(printf '%02d' "$n") - $tr.flac"
    [ -f "$f" ] || ffmpeg -v error -y -f lavfi -i "sine=frequency=$((220 + t * 23 + n * 11)):sample_rate=44100" -t 20 \
      -metadata title="$tr" -metadata artist="$artist" -metadata album_artist="$artist" -metadata album="$title" \
      -metadata track="$n" -metadata date="$year" -metadata genre="Electronic" -c:a flac "$f"
    n=$((n + 1)); t=$((t + 1))
  done
done
echo "sample library at $ROOT"
find "$ROOT" -type f | wc -l
