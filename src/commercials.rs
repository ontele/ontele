// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! Commercial detection and removal.
//!
//! Detection prefers Comskip (invoked with a generated ini forcing EDL
//! output). Without comskip we fall back to a pure-ffmpeg heuristic:
//! intersect `blackdetect` and `silencedetect` events and cluster the cut
//! points into ad pods (≥2 cuts within 240 s spanning ≥20 s).
//!
//! `skip` stores breaks (and optionally writes them as chapters into the
//! MKV); `delete` hard-cuts them with stream-copy segment extraction + the
//! concat demuxer (keyframe-snapped, no re-encode).

use crate::model::{Break, Chapter, Settings};
use anyhow::{Context, anyhow, bail};
use regex::Regex;
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::LazyLock,
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detector {
    Comskip,
    Ffmpeg,
}

impl Detector {
    pub fn as_str(self) -> &'static str {
        match self {
            Detector::Comskip => "comskip",
            Detector::Ffmpeg => "ffmpeg",
        }
    }
}

impl std::fmt::Display for Detector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Comskip / ffmpeg analysis of a multi-hour recording can be slow on a
/// NAS-class CPU; anything past this is a hung child.
const DETECT_TIMEOUT: Duration = Duration::from_secs(3 * 3600);
/// Stream-copy jobs (segment extraction, concat, chapter remux) are I/O bound.
const COPY_TIMEOUT: Duration = Duration::from_secs(3600);

/// Minimum black hold (seconds) that counts as a junction when the
/// black∩silence intersection yields fewer than two cuts.
const BLACK_HOLD_MIN: f64 = 1.2;
/// Cuts further apart than this belong to different pods.
const POD_GAP_MAX: f64 = 240.0;
/// A pod must span at least this long to be believed.
const POD_SPAN_MIN: f64 = 20.0;
/// Pad added around a pod so the cut lands on the black frame, not the show.
const POD_PAD: f64 = 0.5;
/// Keep segments shorter than this are noise (keyframe snapping would make
/// them empty anyway).
const KEEP_MIN: f64 = 1.0;

// ---- detection -----------------------------------------------------------------

/// Detect breaks; picks comskip when `set.comskip_path` resolves.
pub async fn detect(set: &Settings, path: &Path) -> anyhow::Result<(Vec<Break>, Detector)> {
    if !tokio::fs::try_exists(path).await.unwrap_or(false) {
        bail!("recording not found: {}", path.display());
    }
    if let Some(exe) = resolve_exe(&set.comskip_path) {
        match comskip(&exe, path).await {
            Ok(breaks) => {
                metrics::counter!("ontele_commercial_scans_total", "detector" => "comskip", "result" => "ok")
                    .increment(1);
                tracing::info!(path = %path.display(), breaks = breaks.len(), "comskip detection done");
                return Ok((breaks, Detector::Comskip));
            }
            Err(e) => {
                metrics::counter!("ontele_commercial_scans_total", "detector" => "comskip", "result" => "error")
                    .increment(1);
                tracing::warn!(path = %path.display(), error = %e, "comskip failed; falling back to ffmpeg heuristic");
            }
        }
    }
    match ffmpeg_detect(&set.ffmpeg_path, path).await {
        Ok(breaks) => {
            metrics::counter!("ontele_commercial_scans_total", "detector" => "ffmpeg", "result" => "ok").increment(1);
            tracing::info!(path = %path.display(), breaks = breaks.len(), "ffmpeg detection done");
            Ok((breaks, Detector::Ffmpeg))
        }
        Err(e) => {
            metrics::counter!("ontele_commercial_scans_total", "detector" => "ffmpeg", "result" => "error")
                .increment(1);
            Err(e)
        }
    }
}

/// Locate an executable: absolute / relative paths are checked directly,
/// bare names are searched on `PATH`.
pub fn resolve_exe(name: &str) -> Option<PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let p = Path::new(name);
    if p.components().count() > 1 || p.is_absolute() {
        return p.is_file().then(|| p.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|c| c.is_file())
}

async fn comskip(exe: &Path, path: &Path) -> anyhow::Result<Vec<Break>> {
    let dir = std::env::temp_dir().join(format!("ontele-comskip-{}", crate::model::rand_id(6)));
    tokio::fs::create_dir_all(&dir).await.with_context(|| format!("create {}", dir.display()))?;
    let res = comskip_in(exe, path, &dir).await;
    if let Err(e) = tokio::fs::remove_dir_all(&dir).await {
        tracing::debug!(dir = %dir.display(), error = %e, "comskip tempdir cleanup");
    }
    res
}

async fn comskip_in(exe: &Path, path: &Path, dir: &Path) -> anyhow::Result<Vec<Break>> {
    let ini = dir.join("comskip.ini");
    tokio::fs::write(&ini, "output_edl=1\noutput_default=0\n").await.context("write comskip.ini")?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg(format!("--ini={}", ini.display()))
        .arg(format!("--output={}", dir.display()))
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let started = std::time::Instant::now();
    let out = tokio::time::timeout(DETECT_TIMEOUT, cmd.output())
        .await
        .map_err(|_| anyhow!("comskip timed out after {:?}", DETECT_TIMEOUT))?
        .context("spawn comskip")?;
    let edl = edl_path(dir, path);
    // comskip exits 1 when it found commercials and 0 when it found none;
    // the EDL is authoritative whenever it was written.
    match tokio::fs::read_to_string(&edl).await {
        Ok(text) => {
            tracing::debug!(elapsed = ?started.elapsed(), status = ?out.status.code(), "comskip finished");
            Ok(parse_edl(&text))
        }
        Err(_) if out.status.success() => Ok(vec![]),
        Err(_) => bail!("comskip exit {}: {}", out.status, tail(&String::from_utf8_lossy(&out.stderr))),
    }
}

/// `<dir>/<input file stem>.edl` — where comskip writes its EDL. The stem
/// may itself contain dots ("Dr. Phil - ..."), so this must not go through
/// `Path::with_extension`, which would replace everything after the last dot.
fn edl_path(dir: &Path, input: &Path) -> PathBuf {
    let mut name = input.file_stem().map(|s| s.to_os_string()).unwrap_or_default();
    name.push(".edl");
    dir.join(name)
}

/// Parse EDL text: `start<TAB>end<TAB>action`; actions 0 (cut) and 3
/// (commercial) are ads; malformed/inverted lines are skipped.
pub fn parse_edl(text: &str) -> Vec<Break> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut f = line.split_whitespace();
        let (Some(a), Some(b), Some(act)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        let (Ok(start), Ok(end)) = (a.parse::<f64>(), b.parse::<f64>()) else {
            continue;
        };
        if !start.is_finite() || !end.is_finite() || start < 0.0 || end <= start {
            continue;
        }
        // the action may be written as "3" or "3.0"
        let action = match act.parse::<f64>() {
            Ok(v) if v.is_finite() => v as i64,
            _ => continue,
        };
        if action == 0 || action == 3 {
            out.push(Break { start, end });
        }
    }
    out.sort_by(|x, y| x.start.total_cmp(&y.start));
    out
}

/// A (start, end) span in seconds.
pub type Span = (f64, f64);

static RE_BLACK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"black_start:\s*([0-9]+(?:\.[0-9]+)?)\s+black_end:\s*([0-9]+(?:\.[0-9]+)?)").unwrap());
static RE_SILENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"silence_end:\s*([0-9]+(?:\.[0-9]+)?)\s*\|\s*silence_duration:\s*([0-9]+(?:\.[0-9]+)?)").unwrap()
});

/// Parse ffmpeg stderr for blackdetect/silencedetect lines.
pub fn parse_detect_log(stderr: &str) -> (Vec<Span>, Vec<Span>) {
    let mut blacks = Vec::new();
    let mut silences = Vec::new();
    for line in stderr.lines() {
        if let Some(c) = RE_BLACK.captures(line)
            && let (Ok(s), Ok(e)) = (c[1].parse::<f64>(), c[2].parse::<f64>())
            && e > s
        {
            blacks.push((s, e));
        }
        if let Some(c) = RE_SILENCE.captures(line)
            && let (Ok(end), Ok(dur)) = (c[1].parse::<f64>(), c[2].parse::<f64>())
            && dur > 0.0
        {
            silences.push(((end - dur).max(0.0), end));
        }
    }
    blacks.sort_by(|a, b| a.0.total_cmp(&b.0));
    silences.sort_by(|a, b| a.0.total_cmp(&b.0));
    (blacks, silences)
}

/// Cluster black∩silence junctions into ad pods (pure).
pub fn cluster(blacks: &[Span], silences: &[Span]) -> Vec<Break> {
    // A cut is the midpoint of a black hold that coincides with silence.
    let mut cuts: Vec<f64> = Vec::new();
    for &(bs, be) in blacks {
        for &(ss, se) in silences {
            let lo = bs.max(ss);
            let hi = be.min(se);
            if hi > lo {
                cuts.push((lo + hi) / 2.0);
            }
        }
    }
    // Too few junctions (no audio track, or ads without silence): trust long
    // black holds on their own.
    if cuts.len() < 2 {
        cuts = blacks.iter().filter(|(s, e)| e - s >= BLACK_HOLD_MIN).map(|(s, e)| (s + e) / 2.0).collect();
    }
    cuts.sort_by(|a, b| a.total_cmp(b));
    cuts.dedup_by(|a, b| (*a - *b).abs() < 0.05);

    let mut pods = Vec::new();
    let mut i = 0;
    while i < cuts.len() {
        let first = cuts[i];
        let mut last = first;
        let mut j = i + 1;
        while j < cuts.len() && cuts[j] - last <= POD_GAP_MAX {
            last = cuts[j];
            j += 1;
        }
        if last - first >= POD_SPAN_MIN {
            pods.push(Break { start: (first - POD_PAD).max(0.0), end: last + POD_PAD });
        }
        i = j;
    }
    pods
}

async fn ffmpeg_detect(ffmpeg: &str, path: &Path) -> anyhow::Result<Vec<Break>> {
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-nostats", "-i"])
        .arg(path)
        .args(["-vf", "blackdetect=d=0.4:pix_th=0.10", "-af", "silencedetect=n=-35dB:d=0.3", "-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().with_context(|| format!("spawn {ffmpeg}"))?;
    let stderr = child.stderr.take().ok_or_else(|| anyhow!("ffmpeg stderr not captured"))?;

    // stderr can run to megabytes on a long recording: keep only the
    // detector lines plus a short tail for error reporting.
    let run = async {
        let mut lines = BufReader::new(stderr).lines();
        let mut kept = String::new();
        let mut tail_lines: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        while let Some(line) = lines.next_line().await? {
            if line.contains("black_start") || line.contains("silence_end") {
                kept.push_str(&line);
                kept.push('\n');
            } else {
                if tail_lines.len() >= 20 {
                    tail_lines.pop_front();
                }
                tail_lines.push_back(line);
            }
        }
        let status = child.wait().await?;
        Ok::<_, anyhow::Error>((status, kept, tail_lines.into_iter().collect::<Vec<_>>().join("\n")))
    };
    let (status, log, tail) = tokio::time::timeout(DETECT_TIMEOUT, run)
        .await
        .map_err(|_| anyhow!("ffmpeg detection timed out after {:?}", DETECT_TIMEOUT))??;
    if !status.success() {
        bail!("ffmpeg detection exit {status}: {}", tail);
    }
    let (blacks, silences) = parse_detect_log(&log);
    tracing::debug!(blacks = blacks.len(), silences = silences.len(), "ffmpeg detection events");
    Ok(cluster(&blacks, &silences))
}

// ---- cutting ------------------------------------------------------------------

/// Normalize breaks: clamp to `[0, total]`, drop empties, sort and merge
/// overlaps.
fn normalize_breaks(total: f64, breaks: &[Break]) -> Vec<Break> {
    let mut v: Vec<Break> = breaks
        .iter()
        .filter(|b| b.start.is_finite() && b.end.is_finite())
        .map(|b| Break { start: b.start.max(0.0), end: b.end.min(total) })
        .filter(|b| b.end > b.start)
        .collect();
    v.sort_by(|a, b| a.start.total_cmp(&b.start));
    let mut out: Vec<Break> = Vec::with_capacity(v.len());
    for b in v {
        match out.last_mut() {
            Some(last) if b.start <= last.end => last.end = last.end.max(b.end),
            _ => out.push(b),
        }
    }
    out
}

/// Complement of the breaks within `[0, total]`, dropping slivers.
fn keep_segments(total: f64, breaks: &[Break]) -> Vec<Span> {
    let mut keeps = Vec::new();
    let mut pos = 0.0;
    for b in normalize_breaks(total, breaks) {
        if b.start - pos > KEEP_MIN {
            keeps.push((pos, b.start));
        }
        pos = pos.max(b.end);
    }
    if total - pos > KEEP_MIN {
        keeps.push((pos, total));
    }
    keeps
}

/// Quote a path for the concat demuxer's `file` directive.
fn concat_escape(path: &Path) -> String {
    let s = path.to_string_lossy();
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn tail(s: &str) -> String {
    let s = s.trim();
    let n = s.len();
    if n <= 2000 {
        s.to_string()
    } else {
        let mut cut = n - 2000;
        while !s.is_char_boundary(cut) {
            cut += 1;
        }
        format!("…{}", &s[cut..])
    }
}

/// Run ffmpeg (or any ffmpeg-like tool) to completion, bounded by `timeout`,
/// returning a descriptive error (with the stderr tail) on failure.
pub async fn run_ffmpeg<I, S>(ffmpeg: &str, args: I, timeout: Duration) -> anyhow::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let out = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| anyhow!("{ffmpeg} timed out after {timeout:?}"))?
        .with_context(|| format!("spawn {ffmpeg}"))?;
    if !out.status.success() {
        bail!("{ffmpeg} exit {}: {}", out.status, tail(&String::from_utf8_lossy(&out.stderr)));
    }
    Ok(())
}

/// Cut breaks out of `path` in place. Returns (new path — always .mkv, kept
/// seconds). Refuses when breaks cover everything.
pub async fn cut(set: &Settings, path: &Path, total_dur: f64, breaks: &[Break]) -> anyhow::Result<(PathBuf, f64)> {
    if !(total_dur.is_finite() && total_dur > 0.0) {
        bail!("cannot cut: unknown duration");
    }
    let keeps = keep_segments(total_dur, breaks);
    if keeps.is_empty() {
        bail!("refusing to cut: breaks cover the whole recording");
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "recording".into());
    let workdir = parent.join(format!(".{stem}.cut-{}", crate::model::rand_id(4)));
    tokio::fs::create_dir_all(&workdir).await.with_context(|| format!("create {}", workdir.display()))?;

    let res = cut_in(set, path, &parent, &stem, &workdir, &keeps).await;
    if let Err(e) = tokio::fs::remove_dir_all(&workdir).await {
        tracing::debug!(dir = %workdir.display(), error = %e, "cut workdir cleanup");
    }
    res
}

async fn cut_in(
    set: &Settings,
    path: &Path,
    parent: &Path,
    stem: &str,
    workdir: &Path,
    keeps: &[Span],
) -> anyhow::Result<(PathBuf, f64)> {
    let mut list = String::new();
    let mut kept = 0.0;
    for (i, &(a, b)) in keeps.iter().enumerate() {
        let part = workdir.join(format!("part{i:03}.ts"));
        let args: Vec<std::ffi::OsString> = vec![
            "-ss".into(),
            format!("{a:.3}").into(),
            "-i".into(),
            path.as_os_str().to_os_string(),
            "-t".into(),
            format!("{:.3}", b - a).into(),
            "-map".into(),
            "0:v:0".into(),
            "-map".into(),
            "0:a?".into(),
            "-c".into(),
            "copy".into(),
            "-avoid_negative_ts".into(),
            "make_zero".into(),
            "-y".into(),
            part.as_os_str().to_os_string(),
        ];
        run_ffmpeg(&set.ffmpeg_path, &args, COPY_TIMEOUT)
            .await
            .with_context(|| format!("extract segment {i} [{a:.1}, {b:.1}]"))?;
        list.push_str(&format!("file {}\n", concat_escape(&part)));
        kept += b - a;
    }
    let list_path = workdir.join("concat.txt");
    tokio::fs::write(&list_path, &list).await.context("write concat list")?;

    let cut_path = parent.join(format!("{stem}.cut.mkv"));
    let args: Vec<std::ffi::OsString> = vec![
        "-f".into(),
        "concat".into(),
        "-safe".into(),
        "0".into(),
        "-i".into(),
        list_path.as_os_str().to_os_string(),
        "-c".into(),
        "copy".into(),
        "-y".into(),
        cut_path.as_os_str().to_os_string(),
    ];
    if let Err(e) = run_ffmpeg(&set.ffmpeg_path, &args, COPY_TIMEOUT).await {
        let _ = tokio::fs::remove_file(&cut_path).await;
        return Err(e.context("concat segments"));
    }

    if let Err(e) = tokio::fs::remove_file(path).await {
        // don't leave a stray .cut.mkv next to a recording we could not replace
        let _ = tokio::fs::remove_file(&cut_path).await;
        return Err(anyhow!(e).context(format!("remove original {}", path.display())));
    }
    let final_path = parent.join(format!("{stem}.mkv"));
    match tokio::fs::rename(&cut_path, &final_path).await {
        Ok(()) => Ok((final_path, kept)),
        Err(e) => {
            tracing::warn!(from = %cut_path.display(), to = %final_path.display(), error = %e, "rename after cut failed; keeping .cut.mkv");
            Ok((cut_path, kept))
        }
    }
}

// ---- chapters -----------------------------------------------------------------

/// Alternating Content/Commercial chapters for a recording.
pub fn chapters_from_breaks(total_dur: f64, breaks: &[Break]) -> Vec<Chapter> {
    let total = if total_dur.is_finite() && total_dur > 0.0 {
        total_dur
    } else {
        breaks.iter().map(|b| b.end).fold(0.0, f64::max)
    };
    let mut out = Vec::new();
    let (mut n_content, mut n_ad) = (0usize, 0usize);
    let mut pos = 0.0;
    for b in normalize_breaks(total, breaks) {
        if b.start > pos {
            n_content += 1;
            out.push(Chapter { start: pos, end: b.start, title: format!("Content {n_content}") });
        }
        n_ad += 1;
        out.push(Chapter { start: b.start, end: b.end, title: format!("Commercial {n_ad}") });
        pos = b.end;
    }
    if total > pos {
        n_content += 1;
        out.push(Chapter { start: pos, end: total, title: format!("Content {n_content}") });
    }
    out
}

fn ffmeta_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '=' | ';' | '#' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\\n"),
            '\r' => {}
            _ => out.push(c),
        }
    }
    out
}

/// ffmetadata text for `-map_chapters`.
pub fn ffmetadata(chapters: &[Chapter]) -> String {
    let mut s = String::from(";FFMETADATA1\n");
    for c in chapters {
        let start = (c.start.max(0.0) * 1000.0).round() as i64;
        let end = (c.end.max(0.0) * 1000.0).round() as i64;
        if end <= start {
            continue;
        }
        s.push_str("[CHAPTER]\nTIMEBASE=1/1000\n");
        s.push_str(&format!("START={start}\nEND={end}\ntitle={}\n", ffmeta_escape(&c.title)));
    }
    s
}

/// Remux `path` (stream copy) with ad-break chapters. Returns the new path
/// (same stem, `.mkv`).
pub async fn write_chapters(set: &Settings, path: &Path, total_dur: f64, breaks: &[Break]) -> anyhow::Result<PathBuf> {
    let chapters = chapters_from_breaks(total_dur, breaks);
    if chapters.is_empty() {
        bail!("no chapters to write");
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "recording".into());
    let meta_path = std::env::temp_dir().join(format!("ontele-chapters-{}.ffmeta", crate::model::rand_id(6)));
    tokio::fs::write(&meta_path, ffmetadata(&chapters)).await.context("write ffmetadata")?;
    let tmp_out = parent.join(format!("{stem}.chapters.mkv"));
    let args: Vec<std::ffi::OsString> = vec![
        "-i".into(),
        path.as_os_str().to_os_string(),
        "-i".into(),
        meta_path.as_os_str().to_os_string(),
        "-map".into(),
        "0".into(),
        "-map_metadata".into(),
        "0".into(),
        "-map_chapters".into(),
        "1".into(),
        "-c".into(),
        "copy".into(),
        "-y".into(),
        tmp_out.as_os_str().to_os_string(),
    ];
    let res = run_ffmpeg(&set.ffmpeg_path, &args, COPY_TIMEOUT).await;
    let _ = tokio::fs::remove_file(&meta_path).await;
    if let Err(e) = res {
        let _ = tokio::fs::remove_file(&tmp_out).await;
        return Err(e.context("write chapters"));
    }
    let final_path = parent.join(format!("{stem}.mkv"));
    // rename(2) replaces atomically; readers holding the old inode keep it.
    tokio::fs::rename(&tmp_out, &final_path).await.with_context(|| format!("replace {}", final_path.display()))?;
    if final_path != path {
        // the source was a different container (.ts); it is superseded
        if let Err(e) = tokio::fs::remove_file(path).await {
            tracing::warn!(path = %path.display(), error = %e, "remove pre-chapter file");
        }
    }
    Ok(final_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ffmpeg_paths() -> Option<(String, String)> {
        let f = resolve_exe("ffmpeg").or_else(|| resolve_exe("/opt/homebrew/bin/ffmpeg"))?;
        let p = resolve_exe("ffprobe").or_else(|| resolve_exe("/opt/homebrew/bin/ffprobe"))?;
        Some((f.to_string_lossy().to_string(), p.to_string_lossy().to_string()))
    }

    #[test]
    fn edl_parses_ads_and_skips_junk() {
        let text = "0.00\t30.50\t3\n100.0\t120.0\t0\n200\t190\t3\nfoo\tbar\t3\n300.0\t310.0\t1\n# comment\n\n400.25\t420.75\t3.0\n";
        let b = parse_edl(text);
        assert_eq!(b.len(), 3);
        assert_eq!(b[0], Break { start: 0.0, end: 30.5 });
        assert_eq!(b[1], Break { start: 100.0, end: 120.0 });
        assert_eq!(b[2], Break { start: 400.25, end: 420.75 });
        assert!(parse_edl("").is_empty());
        assert!(parse_edl("1.0\t2.0").is_empty());
        // sorted even when the file isn't
        let b = parse_edl("50\t60\t3\n10\t20\t3\n");
        assert_eq!(b[0].start, 10.0);
    }

    #[test]
    fn edl_path_keeps_dotted_stems() {
        let d = Path::new("/tmp/out");
        assert_eq!(
            edl_path(d, Path::new("/rec/Dr. Phil/Dr. Phil - 2026-03-04 20-30.mkv")),
            d.join("Dr. Phil - 2026-03-04 20-30.edl")
        );
        assert_eq!(
            edl_path(d, Path::new("/rec/Show/Show - 2026-01-01 20-00.ts")),
            d.join("Show - 2026-01-01 20-00.edl")
        );
        assert_eq!(edl_path(d, Path::new("/rec/a.b.c.mkv")), d.join("a.b.c.edl"));
    }

    #[test]
    fn edl_windows_line_endings_and_spaces() {
        let b = parse_edl("12.5   20.0   3\r\n30 40 0\r\n");
        assert_eq!(b, vec![Break { start: 12.5, end: 20.0 }, Break { start: 30.0, end: 40.0 }]);
    }

    #[test]
    fn detect_log_parses_both_filters() {
        let log = "[blackdetect @ 0x1] black_start:12.04 black_end:13.2 black_duration:1.16\n\
                   [silencedetect @ 0x2] silence_start: 12.1\n\
                   [silencedetect @ 0x2] silence_end: 13.3 | silence_duration: 1.2\n\
                   frame=  100 fps=0.0\n\
                   [blackdetect @ 0x1] black_start:600 black_end:601.5 black_duration:1.5\n\
                   [silencedetect @ 0x2] silence_end: 5 | silence_duration: 10\n";
        let (b, s) = parse_detect_log(log);
        assert_eq!(b, vec![(12.04, 13.2), (600.0, 601.5)]);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0], (0.0, 5.0)); // clamped
        assert!((s[1].0 - 12.1).abs() < 1e-9);
        assert_eq!(s[1].1, 13.3);
    }

    #[test]
    fn cluster_finds_three_pods() {
        // three ad pods: 300..420, 1200..1330, 2400..2500; each has cuts every ~30 s
        let mut blacks = Vec::new();
        let mut silences = Vec::new();
        for pod in [300.0, 1200.0, 2400.0] {
            for k in 0..5 {
                let t = pod + 30.0 * k as f64;
                blacks.push((t, t + 0.6));
                silences.push((t - 0.1, t + 0.5));
            }
        }
        // an isolated scene change with silence (not a pod)
        blacks.push((800.0, 800.5));
        silences.push((800.0, 800.4));
        let pods = cluster(&blacks, &silences);
        assert_eq!(pods.len(), 3, "{pods:?}");
        assert!((pods[0].start - (300.25 - 0.5)).abs() < 0.01, "{pods:?}");
        assert!((pods[0].end - (420.25 + 0.5)).abs() < 0.01, "{pods:?}");
        assert!(pods[1].start > 1199.0 && pods[1].end < 1322.0);
        assert!(pods[2].start > 2399.0);
    }

    #[test]
    fn cluster_falls_back_to_black_holds() {
        // no silence at all → only black holds ≥ 1.2 s count
        let blacks = vec![(100.0, 101.5), (130.0, 131.3), (160.0, 162.0), (500.0, 500.5), (900.0, 902.0)];
        let pods = cluster(&blacks, &[]);
        assert_eq!(pods.len(), 1, "{pods:?}");
        assert!((pods[0].start - (100.75 - 0.5)).abs() < 0.01);
        assert!((pods[0].end - (161.0 + 0.5)).abs() < 0.01);
        // one cut only → no pod
        assert!(cluster(&[(10.0, 12.0)], &[]).is_empty());
        assert!(cluster(&[], &[]).is_empty());
        // pad never goes negative
        let pods = cluster(&[(0.0, 0.5), (25.0, 25.5)], &[(0.0, 0.5), (25.0, 25.5)]);
        assert_eq!(pods.len(), 1);
        assert_eq!(pods[0].start, 0.0);
        assert!((pods[0].end - 25.75).abs() < 1e-9);
    }

    #[test]
    fn chapters_alternate_and_cover_everything() {
        let ch = chapters_from_breaks(100.0, &[Break { start: 20.0, end: 30.0 }, Break { start: 60.0, end: 70.0 }]);
        let titles: Vec<&str> = ch.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, ["Content 1", "Commercial 1", "Content 2", "Commercial 2", "Content 3"]);
        assert_eq!(ch[0].start, 0.0);
        assert_eq!(ch.last().unwrap().end, 100.0);
        for w in ch.windows(2) {
            assert_eq!(w[0].end, w[1].start);
        }
        // break at the very start: no empty leading content chapter
        let ch = chapters_from_breaks(50.0, &[Break { start: 0.0, end: 10.0 }]);
        assert_eq!(ch[0].title, "Commercial 1");
        assert_eq!(ch.len(), 2);
        // overlapping breaks merge; breaks past the end clamp
        let ch = chapters_from_breaks(50.0, &[Break { start: 5.0, end: 15.0 }, Break { start: 10.0, end: 80.0 }]);
        assert_eq!(ch.len(), 2);
        assert_eq!(ch[1].end, 50.0);
        assert!(chapters_from_breaks(0.0, &[]).is_empty());
    }

    #[test]
    fn ffmetadata_format() {
        let s = ffmetadata(&[
            Chapter { start: 0.0, end: 20.5, title: "Content 1".into() },
            Chapter { start: 20.5, end: 30.0, title: "Ad; #2 = x".into() },
        ]);
        assert!(s.starts_with(";FFMETADATA1\n"));
        assert!(s.contains("[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=20500\ntitle=Content 1\n"));
        assert!(s.contains("START=20500\nEND=30000\ntitle=Ad\\; \\#2 \\= x\n"));
        assert_eq!(s.matches("[CHAPTER]").count(), 2);
    }

    #[test]
    fn concat_escaping() {
        assert_eq!(concat_escape(Path::new("/a/b.ts")), "'/a/b.ts'");
        assert_eq!(concat_escape(Path::new("/rec/Bob's Show/part000.ts")), "'/rec/Bob'\\''s Show/part000.ts'");
    }

    #[test]
    fn keep_segments_complement() {
        let k = keep_segments(100.0, &[Break { start: 10.0, end: 20.0 }, Break { start: 99.5, end: 100.0 }]);
        assert_eq!(k, vec![(0.0, 10.0), (20.0, 99.5)]);
        assert!(keep_segments(100.0, &[Break { start: 0.0, end: 100.0 }]).is_empty());
        assert_eq!(keep_segments(100.0, &[]), vec![(0.0, 100.0)]);
        assert_eq!(keep_segments(100.0, &[Break { start: 0.0, end: 0.5 }]), vec![(0.5, 100.0)]);
        assert!(keep_segments(0.0, &[]).is_empty());
    }

    #[test]
    fn resolve_exe_handles_paths() {
        assert!(resolve_exe("").is_none());
        assert!(resolve_exe("/definitely/not/here/comskip").is_none());
        assert!(resolve_exe("ontele-no-such-binary-xyz").is_none());
        assert!(resolve_exe("/bin/sh").is_some());
    }

    async fn probe_duration(ffprobe: &str, p: &Path) -> f64 {
        let out = tokio::process::Command::new(ffprobe)
            .args(["-v", "error", "-show_entries", "format=duration", "-of", "default=nw=1:nk=1"])
            .arg(p)
            .output()
            .await
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
    }

    async fn synth_clip(ffmpeg: &str, dir: &Path, secs: u32) -> PathBuf {
        let p = dir.join("Show - 2026-01-01 20-00.mkv");
        let args: Vec<std::ffi::OsString> = vec![
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            "testsrc=size=320x180:rate=25".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            "sine=frequency=440".into(),
            "-t".into(),
            secs.to_string().into(),
            "-c:v".into(),
            "mpeg2video".into(),
            "-g".into(),
            "12".into(),
            "-q:v".into(),
            "4".into(),
            "-c:a".into(),
            "mp2".into(),
            "-y".into(),
            p.as_os_str().to_os_string(),
        ];
        run_ffmpeg(ffmpeg, &args, Duration::from_secs(120)).await.unwrap();
        p
    }

    #[tokio::test]
    async fn cut_removes_break_from_real_clip() {
        let Some((ffmpeg, ffprobe)) = ffmpeg_paths() else {
            eprintln!("ffmpeg not available; skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let clip = synth_clip(&ffmpeg, dir.path(), 12).await;
        let set = Settings { ffmpeg_path: ffmpeg.clone(), ffprobe_path: ffprobe.clone(), ..Default::default() };
        let total = probe_duration(&ffprobe, &clip).await;
        assert!((total - 12.0).abs() < 0.5, "synth clip duration {total}");

        let (out, kept) = cut(&set, &clip, total, &[Break { start: 3.0, end: 5.0 }]).await.unwrap();
        assert_eq!(out, clip, "same stem, .mkv");
        assert!((kept - (total - 2.0)).abs() < 0.01);
        let d = probe_duration(&ffprobe, &out).await;
        assert!((9.0..=11.5).contains(&d), "cut duration {d}");
        // workdir cleaned up
        let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(leftovers.len(), 1, "{leftovers:?}");

        // covering everything is refused
        let err = cut(&set, &clip, d, &[Break { start: 0.0, end: d }]).await.unwrap_err();
        assert!(err.to_string().contains("refusing"));
    }

    #[tokio::test]
    async fn chapters_written_into_mkv() {
        let Some((ffmpeg, ffprobe)) = ffmpeg_paths() else {
            eprintln!("ffmpeg not available; skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let clip = synth_clip(&ffmpeg, dir.path(), 6).await;
        let set = Settings { ffmpeg_path: ffmpeg.clone(), ffprobe_path: ffprobe.clone(), ..Default::default() };
        let out = write_chapters(&set, &clip, 6.0, &[Break { start: 2.0, end: 3.0 }]).await.unwrap();
        assert_eq!(out, clip);
        let probe = tokio::process::Command::new(&ffprobe)
            .args(["-v", "error", "-print_format", "json", "-show_chapters"])
            .arg(&out)
            .output()
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
        let chapters = v["chapters"].as_array().unwrap();
        assert_eq!(chapters.len(), 3);
        assert_eq!(chapters[1]["tags"]["title"], "Commercial 1");
    }

    #[tokio::test]
    async fn ffmpeg_detect_on_synthetic_clip_runs() {
        let Some((ffmpeg, ffprobe)) = ffmpeg_paths() else {
            eprintln!("ffmpeg not available; skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let clip = synth_clip(&ffmpeg, dir.path(), 3).await;
        let set = Settings {
            ffmpeg_path: ffmpeg,
            ffprobe_path: ffprobe,
            comskip_path: "/no/such/comskip".into(),
            ..Default::default()
        };
        let (breaks, det) = detect(&set, &clip).await.unwrap();
        assert_eq!(det, Detector::Ffmpeg);
        assert!(breaks.is_empty(), "test pattern has no black+silence pods: {breaks:?}");
        assert!(detect(&set, Path::new("/no/such/file.mkv")).await.is_err());
    }
}
