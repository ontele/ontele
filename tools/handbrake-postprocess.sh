#!/bin/sh
# Copyright 2026 The Ontele Authors
# SPDX-License-Identifier: Apache-2.0
#
# DVR post-processing: encode a finished recording (HandBrakeCLI when
# available, ffmpeg otherwise) and file it into the TV or movie library,
# then ask Ontele to rescan. Inspired by scrathe's plexEncode.sh, rewritten
# for Ontele (POSIX sh, no bashisms — runs in the ontele container).
#
# Wire it up (Settings → Live TV & DVR → "DVR post-processing command", or
# ONTELE_DVR_POST_CMD / helm --set config.dvrPostCmd):
#     /usr/local/bin/handbrake-postprocess.sh
# Ontele invokes it as:  sh -c <cmd> ontele-post <file>
# with ONTELE_FILE / ONTELE_TITLE / ONTELE_ID exported.
#
# Configuration (environment; every value has a sane in-container default):
#   OUT_TV=/media/tv          destination for episodes (Show/Season NN/)
#   OUT_MOVIES=/media/movies  destination for movies
#   ENCODER=auto              auto | handbrake | ffmpeg | none  (none = move only)
#   HB_OPTS=...               HandBrakeCLI options (default: x264 q20 mkv)
#   FF_OPTS=...               ffmpeg options       (default: x265 crf20)
#   KEEP_ORIGINAL=0           1 = leave the recording in place
#   ONTELE_URL=http://127.0.0.1:7979   rescan endpoint (in-pod default)
#   LOG=/data/postprocess.log
#   LOCK=/tmp/ontele-post.lock         one encode at a time

set -u

file="${1:-${ONTELE_FILE:-}}"
[ -n "$file" ] || { echo "usage: $0 <recording>"; exit 2; }
[ -f "$file" ] || { echo "ERROR no such file: $file"; exit 2; }

OUT_TV="${OUT_TV:-/media/tv}"
OUT_MOVIES="${OUT_MOVIES:-/media/movies}"
ENCODER="${ENCODER:-auto}"
KEEP_ORIGINAL="${KEEP_ORIGINAL:-0}"
ONTELE_URL="${ONTELE_URL:-http://127.0.0.1:7979}"
LOG="${LOG:-/data/postprocess.log}"
LOCK="${LOCK:-/tmp/ontele-post.lock}"
HB_OPTS="${HB_OPTS:--e x264 -q 20 --optimize -f av_mkv --auto-anamorphic --aencoder copy --audio-fallback av_aac -m}"
FF_OPTS="${FF_OPTS:--map 0 -dn -c:a copy -c:s copy -c:v libx265 -crf 20 -preset fast}"

log() {
    line="$(date -Iseconds) $(basename "$file"): $*"
    echo "$line"
    echo "$line" >> "$LOG" 2>/dev/null || true
}

# ---- one encode at a time (atomic via mkdir; dead-owner locks are reaped) ----
waited=0
while ! mkdir "$LOCK" 2>/dev/null; do
    owner=$(cat "$LOCK/pid" 2>/dev/null)
    if [ -n "$owner" ] && ! kill -0 "$owner" 2>/dev/null; then
        log "reaping stale lock (pid $owner is gone)"
        rm -rf "$LOCK"
        continue
    fi
    waited=$((waited + 60))
    [ "$waited" -ge 21600 ] && { log "ERROR lock held for 6h, giving up"; exit 1; }
    [ "$waited" -eq 60 ] && log "waiting for encode lock"
    sleep 60
done
echo $$ > "$LOCK/pid"
trap 'rm -rf "$LOCK" 2>/dev/null' EXIT INT TERM

base=$(basename "$file")
stem="${base%.*}"

# ---- classify: SxxEyy / 1x02 / YYYY-MM-DD → episode, else movie -------------
kind=movie
show=""
season=""
if printf '%s' "$stem" | grep -qiE '(^|[ ._-])s[0-9]{1,2}[ ._-]?e[0-9]{1,3}([ ._-]|$)'; then
    kind=episode
    # season from the matched, separator-bounded token only ("Alias1 S02E03"
    # must not read the 1 in the title)
    tok=$(printf '%s' "$stem" | grep -oiE '(^|[ ._-])s[0-9]{1,2}[ ._-]?e[0-9]{1,3}' | head -1)
    season=$(printf '%s' "$tok" | grep -oiE 's[0-9]{1,2}' | head -1 | tr -dc 0-9)
    show=$(printf '%s' "$stem" | sed -E 's/[ ._-]*[sS][0-9]{1,2}[ ._-]?[eE][0-9]{1,3}.*$//; s/[._]/ /g; s/ +$//')
elif printf '%s' "$stem" | grep -qE '(^|[ ._-])[0-9]{1,2}x[0-9]{2}([ ._-]|$)'; then
    kind=episode
    tok=$(printf '%s' "$stem" | grep -oE '(^|[ ._-])[0-9]{1,2}x[0-9]{2}' | head -1)
    season=$(printf '%s' "$tok" | grep -oE '[0-9]{1,2}x' | head -1 | tr -dc 0-9)
    show=$(printf '%s' "$stem" | sed -E 's/[ ._-]*[0-9]{1,2}x[0-9]{2}.*$//; s/[._]/ /g; s/ +$//')
elif printf '%s' "$stem" | grep -qE '[0-9]{4}-[0-9]{2}-[0-9]{2}'; then
    kind=episode
    show=$(printf '%s' "$stem" | sed -E 's/[ ._-]*[0-9]{4}-[0-9]{2}-[0-9]{2}.*$//; s/[._]/ /g; s/ +$//')
fi
[ "$kind" = episode ] && [ -z "$show" ] && show="${ONTELE_TITLE:-Unknown Show}"
log "classified as $kind${show:+ (show: $show${season:+, season $season})}"

# ---- pick an encoder --------------------------------------------------------
enc="$ENCODER"
if [ "$enc" = auto ]; then
    if command -v HandBrakeCLI >/dev/null 2>&1; then enc=handbrake
    elif command -v ffmpeg >/dev/null 2>&1; then enc=ffmpeg
    else enc=none; fi
fi

# ---- encode to an atomic temp file next to the source -----------------------
srcdir=$(dirname "$file")
out_ext=mkv
tmp="$srcdir/.ontele-post.$$.$out_ext"
start=$(date +%s)
case "$enc" in
    handbrake)
        log "encoding with HandBrakeCLI"
        # shellcheck disable=SC2086
        HandBrakeCLI -i "$file" -o "$tmp" $HB_OPTS </dev/null >/dev/null 2>&1 \
            || { log "ERROR HandBrakeCLI failed"; rm -f "$tmp"; exit 1; }
        ;;
    ffmpeg)
        log "encoding with ffmpeg"
        # shellcheck disable=SC2086
        ffmpeg -nostdin -hide_banner -loglevel error -i "$file" $FF_OPTS -y "$tmp" \
            || { log "ERROR ffmpeg failed"; rm -f "$tmp"; exit 1; }
        ;;
    none)
        log "no encoder: moving as-is"
        out_ext="${base##*.}"
        tmp="$srcdir/.ontele-post.$$.$out_ext"
        cp "$file" "$tmp" || { log "ERROR copy failed"; rm -f "$tmp"; exit 1; }
        ;;
    *)
        log "ERROR unknown ENCODER=$enc"; exit 2 ;;
esac
secs=$(( $(date +%s) - start ))

# ---- file it into the library ----------------------------------------------
if [ "$kind" = episode ]; then
    dest_dir="$OUT_TV/$show"
    season=$(printf '%s' "$season" | sed 's/^0*//')
    [ -n "$season" ] && dest_dir="$dest_dir/Season $(printf '%02d' "$season")"
else
    dest_dir="$OUT_MOVIES"
fi
dest="$dest_dir/$stem.$out_ext"
if [ "$dest" = "$file" ]; then
    # recording already lives at its library destination: nothing to move,
    # and KEEP_ORIGINAL must not delete the file we would be "filing"
    log "already at $dest; leaving in place"
    rm -f "$tmp"
    exit 0
fi
mkdir -p "$dest_dir" || { log "ERROR mkdir $dest_dir"; rm -f "$tmp"; exit 1; }
if [ -e "$dest" ]; then
    n=1
    while [ -e "$dest_dir/$stem ($n).$out_ext" ]; do n=$((n + 1)); done
    dest="$dest_dir/$stem ($n).$out_ext"
    log "destination exists; using $(basename "$dest")"
fi
mv "$tmp" "$dest" || { log "ERROR move to $dest"; rm -f "$tmp"; exit 1; }

isz=$(du -k "$file" | cut -f1); osz=$(du -k "$dest" | cut -f1)
log "done in ${secs}s: ${isz}K -> ${osz}K at $dest"

if [ "$KEEP_ORIGINAL" != 1 ]; then
    rm -f "$file" && log "removed original"
fi

# ---- tell ontele to pick it up ----------------------------------------------
# -f: a 401/403 (proxy auth in front of the API) must not read as success.
# Harmless if it fails — the library folder watcher spots the new file too.
if curl -sf -m 10 -X POST "$ONTELE_URL/api/scan" -o /dev/null 2>/dev/null; then
    log "library rescan requested"
else
    log "NOTE rescan request not accepted ($ONTELE_URL); folder watcher will pick it up"
fi
exit 0
