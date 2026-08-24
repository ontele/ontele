// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! Text subtitle extraction to WebVTT (embedded stream or external file),
//! cached under `<data>/subs/<id>-<idx>.vtt`.

use crate::model::{MediaInfo, SubtitleStream};
use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

/// Upper bound for one ffmpeg extraction (large remuxes are read end to end).
const CONVERT_TIMEOUT: Duration = Duration::from_secs(60);

/// All selectable subtitle tracks for an item (embedded + external).
pub fn list(info: &MediaInfo) -> Vec<SubtitleStream> {
    info.subtitles.clone()
}

/// Cache keys are `<itemId>-<idx>`; anything else could escape `cache_dir`.
pub fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 128
        && !key.starts_with('.')
        && key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !key.contains("..")
}

/// ffmpeg argument list converting one subtitle source to WebVTT at `out`
/// (pure, tested). `charenc` forces the text decoder's input charset for
/// external files that are not UTF-8.
pub fn convert_args(
    media: &Path,
    stream_index: Option<u32>,
    external: Option<&Path>,
    charenc: Option<&str>,
    out: &Path,
) -> Vec<String> {
    let mut a: Vec<String> =
        ["-hide_banner", "-loglevel", "error", "-nostats", "-nostdin", "-y"].iter().map(|s| s.to_string()).collect();
    match external {
        Some(ext) => {
            if let Some(enc) = charenc {
                a.push("-sub_charenc".into());
                a.push(enc.to_string());
            }
            a.push("-i".into());
            a.push(ext.to_string_lossy().to_string());
            a.push("-map".into());
            a.push("0:s:0".into());
        }
        None => {
            a.push("-i".into());
            a.push(media.to_string_lossy().to_string());
            a.push("-map".into());
            a.push(format!("0:{}", stream_index.unwrap_or(0)));
        }
    }
    a.extend(["-vn", "-an", "-dn", "-c:s", "webvtt", "-f", "webvtt"].iter().map(|s| s.to_string()));
    a.push(out.to_string_lossy().to_string());
    a
}

/// Convert to WebVTT. `stream_index` selects an embedded track; `external`
/// a sidecar file. Returns the cached VTT path.
pub async fn to_vtt(
    ffmpeg: &str,
    media: &Path,
    stream_index: Option<u32>,
    external: Option<&Path>,
    cache_dir: &Path,
    key: &str,
) -> anyhow::Result<PathBuf> {
    if !valid_key(key) {
        anyhow::bail!("invalid subtitle cache key");
    }
    if external.is_none() && stream_index.is_none() {
        anyhow::bail!("no subtitle source: need a stream index or an external file");
    }
    let source: &Path = external.unwrap_or(media);
    let src_meta = tokio::fs::metadata(source)
        .await
        .map_err(|e| anyhow::anyhow!("subtitle source unavailable ({}): {e}", source.display()))?;
    if !src_meta.is_file() {
        anyhow::bail!("subtitle source is not a file: {}", source.display());
    }
    let src_mtime = src_meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);

    tokio::fs::create_dir_all(cache_dir).await?;
    let out = cache_dir.join(format!("{key}.vtt"));
    if is_fresh(&out, src_mtime).await {
        return Ok(out);
    }

    // Write to a private temp name, then rename: a concurrent request never
    // sees a half-written cache file.
    let tmp = cache_dir.join(format!(".{key}.{}.tmp.vtt", crate::model::rand_id(4)));
    let result = produce(ffmpeg, media, stream_index, external, &tmp).await;
    match result {
        Ok(()) => {
            tokio::fs::rename(&tmp, &out).await?;
            tracing::debug!(key, out = %out.display(), "subtitle converted");
            Ok(out)
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            Err(e)
        }
    }
}

/// Cached file exists, is non-empty and is at least as new as the source.
async fn is_fresh(out: &Path, src_mtime: SystemTime) -> bool {
    match tokio::fs::metadata(out).await {
        Ok(m) if m.is_file() && m.len() > 0 => m.modified().map(|t| t >= src_mtime).unwrap_or(false),
        _ => false,
    }
}

async fn produce(
    ffmpeg: &str,
    media: &Path,
    stream_index: Option<u32>,
    external: Option<&Path>,
    tmp: &Path,
) -> anyhow::Result<()> {
    if let Some(ext) = external {
        let is_vtt = ext.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("vtt")).unwrap_or(false);
        if is_vtt {
            // Already WebVTT: copy (strip a UTF-8 BOM so the header check passes).
            let bytes = tokio::fs::read(ext).await?;
            let body = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
            if !body.starts_with(b"WEBVTT") {
                anyhow::bail!("{} is not a WebVTT file", ext.display());
            }
            tokio::fs::write(tmp, body).await?;
            return Ok(());
        }
        // ffmpeg's text demuxers insist on UTF-8; sniff and fall back to
        // Latin-1 for legacy SRT files.
        let charenc = match tokio::fs::read(ext).await {
            Ok(bytes) if std::str::from_utf8(&bytes).is_err() => Some("ISO-8859-1"),
            _ => None,
        };
        return run_ffmpeg(ffmpeg, &convert_args(media, None, Some(ext), charenc, tmp), tmp).await;
    }
    run_ffmpeg(ffmpeg, &convert_args(media, stream_index, None, None, tmp), tmp).await
}

async fn run_ffmpeg(ffmpeg: &str, args: &[String], tmp: &Path) -> anyhow::Result<()> {
    tracing::debug!(ffmpeg, ?args, "subtitle extraction");
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().map_err(|e| anyhow::anyhow!("cannot start ffmpeg ({ffmpeg}): {e}"))?;
    let output = match tokio::time::timeout(CONVERT_TIMEOUT, child.wait_with_output()).await {
        Ok(r) => r?,
        Err(_) => anyhow::bail!("subtitle extraction timed out after {} s", CONVERT_TIMEOUT.as_secs()),
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    let tail: Vec<&str> = stderr.lines().map(str::trim).filter(|l| !l.is_empty()).rev().take(5).collect();
    if !output.status.success() {
        anyhow::bail!("ffmpeg failed ({}): {}", output.status, tail.into_iter().rev().collect::<Vec<_>>().join(" | "));
    }
    let len = tokio::fs::metadata(tmp).await.map(|m| m.len()).unwrap_or(0);
    if len == 0 {
        anyhow::bail!(
            "ffmpeg produced no subtitle output{}",
            if tail.is_empty() { String::new() } else { format!(": {}", tail.join(" | ")) }
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arg_builder_embedded_and_external() {
        let out = Path::new("/cache/abc-0.vtt");
        let a = convert_args(Path::new("/m/x.mkv"), Some(5), None, None, out);
        assert_eq!(&a[..6], &["-hide_banner", "-loglevel", "error", "-nostats", "-nostdin", "-y"]);
        let i = a.iter().position(|s| s == "-i").unwrap();
        assert_eq!(a[i + 1], "/m/x.mkv");
        assert!(a.windows(2).any(|w| w[0] == "-map" && w[1] == "0:5"));
        assert!(a.windows(2).any(|w| w[0] == "-f" && w[1] == "webvtt"));
        assert!(a.windows(2).any(|w| w[0] == "-c:s" && w[1] == "webvtt"));
        assert_eq!(a.last().unwrap(), "/cache/abc-0.vtt");
        assert!(!a.contains(&"-sub_charenc".to_string()));

        let a = convert_args(Path::new("/m/x.mkv"), None, Some(Path::new("/m/x.en.srt")), Some("ISO-8859-1"), out);
        let i = a.iter().position(|s| s == "-i").unwrap();
        assert_eq!(a[i + 1], "/m/x.en.srt");
        assert!(a.windows(2).any(|w| w[0] == "-map" && w[1] == "0:s:0"));
        let c = a.iter().position(|s| s == "-sub_charenc").unwrap();
        assert_eq!(a[c + 1], "ISO-8859-1");
        assert!(c < i, "charenc is an input option");
        assert!(!a.contains(&"/m/x.mkv".to_string()));
    }

    #[test]
    fn key_validation() {
        assert!(valid_key("0123abcd-0"));
        assert!(valid_key("id_x.1"));
        assert!(!valid_key(""));
        assert!(!valid_key("../x"));
        assert!(!valid_key("a/b"));
        assert!(!valid_key(".hidden"));
        assert!(!valid_key("a b"));
    }

    #[tokio::test]
    async fn rejects_bad_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let e = to_vtt("ffmpeg", Path::new("/nope.mkv"), Some(0), None, dir.path(), "../x").await.unwrap_err();
        assert!(e.to_string().contains("invalid"));
        let e = to_vtt("ffmpeg", Path::new("/nope.mkv"), None, None, dir.path(), "k").await.unwrap_err();
        assert!(e.to_string().contains("no subtitle source"));
        let e = to_vtt("ffmpeg", Path::new("/nope.mkv"), Some(0), None, dir.path(), "k").await.unwrap_err();
        assert!(e.to_string().contains("unavailable"));
    }

    #[tokio::test]
    async fn external_vtt_is_copied_and_cached() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("m.mkv");
        std::fs::write(&media, b"").unwrap();
        let ext = dir.path().join("m.en.vtt");
        std::fs::write(&ext, "\u{FEFF}WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nhi\n").unwrap();
        let cache = dir.path().join("subs");
        let out = to_vtt("/definitely/not/ffmpeg", &media, None, Some(&ext), &cache, "k-0").await.unwrap();
        assert_eq!(out, cache.join("k-0.vtt"));
        let body = std::fs::read_to_string(&out).unwrap();
        assert!(body.starts_with("WEBVTT"));
        // second call: fresh cache, no work (ffmpeg path is bogus and unused)
        let again = to_vtt("/definitely/not/ffmpeg", &media, None, Some(&ext), &cache, "k-0").await.unwrap();
        assert_eq!(again, out);
        // non-VTT content with a .vtt name is rejected
        std::fs::write(&ext, "not vtt").unwrap();
        assert!(to_vtt("/x", &media, None, Some(&ext), &cache, "k-1").await.is_err());
        assert!(!cache.join("k-1.vtt").exists());
        // no stray temp files
        let leftovers: Vec<_> = std::fs::read_dir(&cache)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    /// SRT → WebVTT through ffmpeg (skipped when ffmpeg is unavailable).
    #[tokio::test]
    async fn srt_converts_to_webvtt() {
        let Some(ffmpeg) = which_ffmpeg() else { return };
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("m.mkv");
        std::fs::write(&media, b"").unwrap();
        let srt = dir.path().join("m.en.srt");
        std::fs::write(
            &srt,
            "1\n00:00:01,000 --> 00:00:02,500\nHello <i>world</i>\n\n2\n00:00:03,000 --> 00:00:04,000\nBye\n",
        )
        .unwrap();
        let cache = dir.path().join("subs");
        let out = to_vtt(&ffmpeg, &media, None, Some(&srt), &cache, "abc-1").await.expect("convert");
        let body = std::fs::read_to_string(&out).unwrap();
        assert!(body.starts_with("WEBVTT"), "{body}");
        assert!(body.contains("00:01.000 --> 00:02.500") || body.contains("00:00:01.000 --> 00:00:02.500"), "{body}");
        assert!(body.contains("Hello"));

        // Latin-1 file: charenc fallback kicks in
        let latin = dir.path().join("m.de.srt");
        std::fs::write(&latin, b"1\n00:00:01,000 --> 00:00:02,000\nGr\xFC\xDFe\n").unwrap();
        let out = to_vtt(&ffmpeg, &media, None, Some(&latin), &cache, "abc-2").await.expect("latin1 convert");
        let body = std::fs::read_to_string(&out).unwrap();
        assert!(body.contains("Grüße"), "{body}");

        // embedded track from a synthesized MKV
        let mkv = dir.path().join("e.mkv");
        let st = std::process::Command::new(&ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=160x90:rate=10"])
            .arg("-i")
            .arg(&srt)
            .args(["-t", "5", "-map", "0:v", "-map", "1:s", "-c:v", "libx264", "-preset", "ultrafast", "-c:s", "srt"])
            .arg(&mkv)
            .status()
            .unwrap();
        if st.success() {
            let out = to_vtt(&ffmpeg, &mkv, Some(1), None, &cache, "abc-3").await.expect("embedded convert");
            let body = std::fs::read_to_string(&out).unwrap();
            assert!(body.starts_with("WEBVTT"));
            assert!(body.contains("Bye"));
        }
        // wrong stream index → error, no cache file
        assert!(to_vtt(&ffmpeg, &mkv, Some(9), None, &cache, "abc-4").await.is_err());
        assert!(!cache.join("abc-4.vtt").exists());
    }

    fn which_ffmpeg() -> Option<String> {
        ["ffmpeg", "/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/usr/bin/ffmpeg"]
            .into_iter()
            .find(|c| {
                std::process::Command::new(c)
                    .arg("-version")
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
            })
            .map(String::from)
    }
}
