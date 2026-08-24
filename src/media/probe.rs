// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! ffprobe wrapper → [`MediaInfo`]. Pure JSON parsing is separated from the
//! process spawn so it can be unit-tested with captured ffprobe output.

use crate::media::playback::{normalize_codec, normalize_container};
use crate::model::{AudioStream, Chapter, MediaInfo, SubtitleStream, VideoStream};
use anyhow::Context;
use serde_json::Value;
use std::{path::Path, process::Stdio, time::Duration};

/// ffprobe must answer within this long (network shares, huge remuxes).
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Sidecar subtitle streams are numbered from here so they never collide
/// with ffprobe stream indexes.
const EXTERNAL_INDEX_BASE: u32 = 1000;

/// Run `ffprobe -v quiet -print_format json -show_format -show_streams -show_chapters`.
pub async fn probe(ffprobe: &str, path: &Path) -> anyhow::Result<MediaInfo> {
    let meta = tokio::fs::metadata(path).await.with_context(|| format!("stat {}", path.display()))?;
    if !meta.is_file() {
        anyhow::bail!("{} is not a regular file", path.display());
    }
    let bin = if ffprobe.trim().is_empty() { "ffprobe" } else { ffprobe.trim() };
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("-v")
        .arg("quiet")
        .arg("-print_format")
        .arg("json")
        .arg("-show_format")
        .arg("-show_streams")
        .arg("-show_chapters")
        .arg("-i")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().with_context(|| format!("spawn {bin}"))?;
    let out = match tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output()).await {
        Ok(r) => r.with_context(|| format!("wait for {bin}"))?,
        Err(_) => anyhow::bail!("ffprobe timed out after {}s on {}", PROBE_TIMEOUT.as_secs(), path.display()),
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim();
        anyhow::bail!(
            "ffprobe failed ({}) on {}: {}",
            out.status,
            path.display(),
            if err.is_empty() { "no output" } else { err }
        );
    }
    let json = String::from_utf8_lossy(&out.stdout);
    let mut info = parse_probe_json(&json, meta.len() as i64)?;
    if info.container.is_empty() {
        info.container = crate::naming::ext_of(path);
    }
    Ok(info)
}

// ---- JSON helpers --------------------------------------------------------------

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn f64_of(v: &Value, key: &str) -> Option<f64> {
    match v.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|f| f.is_finite())
}

fn u64_of(v: &Value, key: &str) -> Option<u64> {
    match v.get(key)? {
        Value::Number(n) => n.as_u64().or_else(|| n.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64)),
        Value::String(s) => {
            s.trim().parse::<u64>().ok().or_else(|| s.trim().parse::<f64>().ok().map(|f| f.max(0.0) as u64))
        }
        _ => None,
    }
}

fn u32_of(v: &Value, key: &str) -> Option<u32> {
    u64_of(v, key).map(|n| n.min(u32::MAX as u64) as u32)
}

/// Case-insensitive lookup in a `tags` object (Matroska writes `LANGUAGE`,
/// MP4 writes `language`).
fn tag_of<'a>(stream: &'a Value, key: &str) -> Option<&'a str> {
    let tags = stream.get("tags")?.as_object()?;
    tags.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .and_then(|(_, v)| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn disposition(stream: &Value, key: &str) -> bool {
    stream.get("disposition").and_then(|d| d.get(key)).and_then(Value::as_i64).unwrap_or(0) != 0
}

/// Language tag with ffprobe's "undetermined" placeholders removed.
fn lang_of(stream: &Value) -> Option<String> {
    tag_of(stream, "language")
        .map(|l| l.to_ascii_lowercase())
        .filter(|l| !matches!(l.as_str(), "und" | "undefined" | "unknown" | "zxx" | "mis"))
}

/// `"24000/1001"` → 23.976; rejects zero/negative/absurd values.
fn parse_rate(s: &str) -> Option<f64> {
    let s = s.trim();
    let v = if let Some((n, d)) = s.split_once('/') {
        let n: f64 = n.trim().parse().ok()?;
        let d: f64 = d.trim().parse().ok()?;
        if d == 0.0 {
            return None;
        }
        n / d
    } else {
        s.parse::<f64>().ok()?
    };
    if v.is_finite() && v > 0.0 && v <= 1000.0 { Some((v * 1000.0).round() / 1000.0) } else { None }
}

/// Bit depth from a pix_fmt such as `yuv420p10le`, `yuv422p12be`, `gbrp16le`,
/// `rgb48le`, `yuv420p` (8).
fn bit_depth_of(pix_fmt: &str) -> Option<u32> {
    let p = pix_fmt.trim().to_ascii_lowercase();
    if p.is_empty() {
        return None;
    }
    // strip endianness suffix
    let core = p.strip_suffix("le").or_else(|| p.strip_suffix("be")).unwrap_or(&p);
    // "...p10" / "...p12" / "...p16"
    if let Some(pos) = core.rfind('p') {
        let digits = &core[pos + 1..];
        if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
            return digits.parse::<u32>().ok().filter(|d| (1..=64).contains(d));
        }
    }
    // rgb48 / rgba64 / gray16 / gray10 …
    let trailing: String =
        core.chars().rev().take_while(|c| c.is_ascii_digit()).collect::<String>().chars().rev().collect();
    if !trailing.is_empty()
        && let Ok(n) = trailing.parse::<u32>()
    {
        return Some(match core.trim_end_matches(&trailing) {
            "rgb" | "bgr" => n / 3,
            "rgba" | "bgra" | "argb" | "abgr" => n / 4,
            _ => n,
        })
        .filter(|d| (1..=64).contains(d));
    }
    if core.starts_with("yuv")
        || core.starts_with("nv")
        || core.starts_with("rgb")
        || core.starts_with("bgr")
        || core.starts_with("gray")
    {
        return Some(8);
    }
    None
}

/// HDR flavour from transfer characteristics and side data. Dolby Vision
/// wins over HDR10+ which wins over plain HDR10/HLG.
fn hdr_of(stream: &Value) -> Option<String> {
    let mut dv = false;
    let mut plus = false;
    if let Some(list) = stream.get("side_data_list").and_then(Value::as_array) {
        for sd in list {
            let t = str_of(sd, "side_data_type").unwrap_or("").to_ascii_lowercase();
            if t.contains("dovi") || t.contains("dolby vision") {
                dv = true;
            }
            if t.contains("smpte2094-40") || t.contains("hdr dynamic metadata") {
                plus = true;
            }
        }
    }
    if dv {
        return Some("dv".into());
    }
    if plus {
        return Some("hdr10plus".into());
    }
    match str_of(stream, "color_transfer").map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("smpte2084") => Some("hdr10".into()),
        Some("arib-std-b67") => Some("hlg".into()),
        _ => None,
    }
}

/// ffprobe reports cover art as a video stream; treat it as such only when
/// flagged `attached_pic` or when an image codec sits inside an audio container.
fn is_cover_art(stream: &Value, audio_container: bool) -> bool {
    if disposition(stream, "attached_pic") {
        return true;
    }
    let codec = str_of(stream, "codec_name").unwrap_or("").to_ascii_lowercase();
    let image = matches!(codec.as_str(), "mjpeg" | "png" | "bmp" | "gif" | "tiff" | "webp" | "jpeg2000");
    image && audio_container
}

fn is_audio_format(format_name: &str) -> bool {
    let first = format_name.split(',').next().unwrap_or("").trim();
    matches!(
        first,
        "mp3"
            | "flac"
            | "ogg"
            | "wav"
            | "aiff"
            | "ape"
            | "wv"
            | "mpc"
            | "mpc8"
            | "tta"
            | "dsf"
            | "dff"
            | "tak"
            | "aac"
            | "ac3"
            | "eac3"
            | "dts"
            | "opus"
            | "spx"
            | "amr"
            | "au"
            | "caf"
            | "mp2"
            | "truehd"
    )
}

/// Parse ffprobe JSON. `size_bytes` is taken from the filesystem when the
/// format section lacks it. Detects HDR (color_transfer smpte2084/arib-std-b67,
/// Dolby Vision side data), interlacing (field_order), subtitle text-vs-bitmap.
pub fn parse_probe_json(json: &str, size_bytes: i64) -> anyhow::Result<MediaInfo> {
    let root: Value = serde_json::from_str(json).context("ffprobe output is not valid JSON")?;
    let empty = Value::Object(Default::default());
    let format = root.get("format").unwrap_or(&empty);
    let streams: Vec<&Value> =
        root.get("streams").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
    if format.as_object().is_none_or(|o| o.is_empty()) && streams.is_empty() {
        anyhow::bail!("ffprobe returned no format or stream information");
    }

    let format_name = str_of(format, "format_name").unwrap_or("").to_string();
    let filename = str_of(format, "filename").unwrap_or("");
    let ext = crate::naming::ext_of(Path::new(filename));
    let audio_container = is_audio_format(&format_name);

    let mut info = MediaInfo {
        container: normalize_container(&format_name, &ext),
        size_bytes: u64_of(format, "size").filter(|s| *s > 0).map(|s| s as i64).unwrap_or(size_bytes),
        bitrate: u64_of(format, "bit_rate").filter(|b| *b > 0),
        ..Default::default()
    };

    // duration: format first, then the longest stream
    let mut duration = f64_of(format, "duration").filter(|d| *d > 0.0).unwrap_or(0.0);
    if duration <= 0.0 {
        duration = streams.iter().filter_map(|s| f64_of(s, "duration")).fold(0.0, f64::max);
    }
    info.duration_sec = duration.max(0.0);

    for s in &streams {
        let codec_type = str_of(s, "codec_type").unwrap_or("").to_ascii_lowercase();
        let index = u32_of(s, "index").unwrap_or(0);
        let codec_raw = str_of(s, "codec_name").unwrap_or("").to_string();
        match codec_type.as_str() {
            "video" => {
                if info.video.is_some() || is_cover_art(s, audio_container) {
                    continue;
                }
                let fps = str_of(s, "r_frame_rate")
                    .and_then(parse_rate)
                    .or_else(|| str_of(s, "avg_frame_rate").and_then(parse_rate));
                let bit_depth = u32_of(s, "bits_per_raw_sample")
                    .filter(|d| *d > 0)
                    .or_else(|| str_of(s, "pix_fmt").and_then(bit_depth_of));
                let interlaced = str_of(s, "field_order")
                    .map(|f| f.to_ascii_lowercase())
                    .is_some_and(|f| f != "progressive" && f != "unknown");
                let width = u32_of(s, "width").unwrap_or(0);
                let height = u32_of(s, "height").unwrap_or(0);
                info.video = Some(VideoStream {
                    index,
                    codec: normalize_codec(&codec_raw),
                    profile: str_of(s, "profile").map(str::to_string),
                    width,
                    height,
                    fps,
                    bit_depth,
                    hdr: hdr_of(s),
                    interlaced,
                });
            }
            "audio" => {
                info.audio.push(AudioStream {
                    index,
                    codec: normalize_codec(&codec_raw),
                    channels: u32_of(s, "channels").unwrap_or(0),
                    lang: lang_of(s),
                    title: tag_of(s, "title").map(str::to_string),
                    default: disposition(s, "default"),
                });
            }
            "subtitle" => {
                let codec = codec_raw.to_ascii_lowercase();
                info.subtitles.push(SubtitleStream {
                    index,
                    text: is_text_subtitle(&codec),
                    codec,
                    lang: lang_of(s),
                    title: tag_of(s, "title").map(str::to_string),
                    forced: disposition(s, "forced"),
                    external: None,
                });
            }
            _ => {}
        }
    }

    if let Some(chapters) = root.get("chapters").and_then(Value::as_array) {
        for (i, c) in chapters.iter().enumerate() {
            let start = f64_of(c, "start_time").unwrap_or(0.0).max(0.0);
            let end = f64_of(c, "end_time").unwrap_or(start).max(start);
            let title = tag_of(c, "title").map(str::to_string).unwrap_or_else(|| format!("Chapter {}", i + 1));
            info.chapters.push(Chapter { start, end, title });
        }
    }

    if let Some(v) = &info.video {
        info.vcodec = Some(v.codec.clone()).filter(|c| !c.is_empty());
        info.width = Some(v.width).filter(|w| *w > 0);
        info.height = Some(v.height).filter(|h| *h > 0);
    }
    if let Some(a) = info.audio.iter().find(|a| a.default).or_else(|| info.audio.first()) {
        info.acodec = Some(a.codec.clone()).filter(|c| !c.is_empty());
    }
    Ok(info)
}

// ---- sidecar subtitles -----------------------------------------------------------

/// Language names that show up in sidecar file names, mapped to ISO codes.
const LANG_NAMES: &[(&str, &str)] = &[
    ("english", "en"),
    ("eng", "en"),
    ("french", "fr"),
    ("fre", "fr"),
    ("fra", "fr"),
    ("german", "de"),
    ("ger", "de"),
    ("deu", "de"),
    ("spanish", "es"),
    ("spa", "es"),
    ("italian", "it"),
    ("ita", "it"),
    ("portuguese", "pt"),
    ("por", "pt"),
    ("brazilian", "pt-br"),
    ("dutch", "nl"),
    ("nld", "nl"),
    ("dut", "nl"),
    ("swedish", "sv"),
    ("swe", "sv"),
    ("norwegian", "no"),
    ("nor", "no"),
    ("danish", "da"),
    ("dan", "da"),
    ("finnish", "fi"),
    ("fin", "fi"),
    ("japanese", "ja"),
    ("jpn", "ja"),
    ("chinese", "zh"),
    ("chi", "zh"),
    ("zho", "zh"),
    ("korean", "ko"),
    ("kor", "ko"),
    ("russian", "ru"),
    ("rus", "ru"),
    ("polish", "pl"),
    ("pol", "pl"),
    ("czech", "cs"),
    ("cze", "cs"),
    ("ces", "cs"),
    ("hungarian", "hu"),
    ("hun", "hu"),
    ("turkish", "tr"),
    ("tur", "tr"),
    ("arabic", "ar"),
    ("ara", "ar"),
    ("hebrew", "he"),
    ("heb", "he"),
    ("greek", "el"),
    ("gre", "el"),
    ("ell", "el"),
    ("hindi", "hi"),
    ("hin", "hi"),
    ("thai", "th"),
    ("tha", "th"),
    ("vietnamese", "vi"),
    ("vie", "vi"),
    ("indonesian", "id"),
    ("ind", "id"),
    ("romanian", "ro"),
    ("ron", "ro"),
    ("rum", "ro"),
    ("ukrainian", "uk"),
    ("ukr", "uk"),
    ("bulgarian", "bg"),
    ("bul", "bg"),
    ("croatian", "hr"),
    ("hrv", "hr"),
    ("serbian", "sr"),
    ("srp", "sr"),
    ("slovak", "sk"),
    ("slo", "sk"),
    ("slk", "sk"),
    ("slovenian", "sl"),
    ("slv", "sl"),
    ("catalan", "ca"),
    ("cat", "ca"),
    ("latin", "la"),
];

/// Interpret one dot-separated token of a sidecar name: language code,
/// flag, or free-form title.
#[derive(Debug, PartialEq)]
enum Token {
    Lang(String),
    Forced,
    Sdh,
    Default,
    Title(String),
}

fn classify_token(tok: &str) -> Token {
    let t = tok.trim();
    let l = t.to_ascii_lowercase();
    match l.as_str() {
        "forced" | "foreign" => return Token::Forced,
        "sdh" | "hi" | "cc" => return Token::Sdh,
        "default" => return Token::Default,
        _ => {}
    }
    if let Some((_, code)) = LANG_NAMES.iter().find(|(n, _)| *n == l) {
        return Token::Lang((*code).to_string());
    }
    // "en", "eng", "pt-BR", "zh_Hans"
    let (base, rest) = l.split_once(['-', '_']).map(|(a, b)| (a, Some(b))).unwrap_or((&l, None));
    let base_ok = (2..=3).contains(&base.len()) && base.chars().all(|c| c.is_ascii_alphabetic());
    let rest_ok = rest.is_none_or(|r| (2..=4).contains(&r.len()) && r.chars().all(|c| c.is_ascii_alphanumeric()));
    if base_ok && rest_ok {
        let base = LANG_NAMES.iter().find(|(n, _)| *n == base).map(|(_, c)| *c).unwrap_or(base);
        return Token::Lang(match rest {
            Some(r) => format!("{base}-{}", r.to_ascii_lowercase()),
            None => base.to_string(),
        });
    }
    Token::Title(t.to_string())
}

/// Parsed sidecar name: `(lang, forced, title)` from the part between the
/// media stem and the subtitle extension.
fn parse_sidecar_suffix(suffix: &str) -> (Option<String>, bool, Option<String>) {
    let mut lang = None;
    let mut forced = false;
    let mut sdh = false;
    let mut titles: Vec<String> = vec![];
    for tok in suffix.split('.').filter(|t| !t.trim().is_empty()) {
        match classify_token(tok) {
            Token::Lang(l) => {
                if lang.is_none() {
                    lang = Some(l);
                } else {
                    titles.push(tok.to_string());
                }
            }
            Token::Forced => forced = true,
            Token::Sdh => sdh = true,
            Token::Default => {}
            Token::Title(t) => titles.push(t),
        }
    }
    if sdh {
        titles.push("SDH".into());
    }
    let title = if titles.is_empty() { None } else { Some(titles.join(" ")) };
    (lang, forced, title)
}

/// ffprobe-style codec name for a sidecar extension, and whether it's text.
fn sidecar_codec(ext: &str) -> Option<(&'static str, bool)> {
    Some(match ext {
        "srt" => ("subrip", true),
        "vtt" => ("webvtt", true),
        "ass" => ("ass", true),
        "ssa" => ("ssa", true),
        "idx" => ("dvd_subtitle", false),
        "sup" => ("hdmv_pgs_subtitle", false),
        _ => return None,
    })
}

/// Sidecar subtitles next to the file: `Movie.srt`, `Movie.en.srt`,
/// `Movie.en.forced.vtt`, `Movie.ass` …
pub fn external_subtitles(path: &Path) -> Vec<SubtitleStream> {
    let Some(dir) = path.parent() else {
        return vec![];
    };
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return vec![];
    };
    let Ok(rd) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let stem_lc = stem.to_ascii_lowercase();

    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file() || t.is_symlink()).unwrap_or(false))
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .collect();
    names.sort();
    let has_file = |name: &str| names.iter().any(|n| n.eq_ignore_ascii_case(name));

    let mut out = vec![];
    for name in &names {
        let lc = name.to_ascii_lowercase();
        let Some(rest) = lc.strip_prefix(&stem_lc) else {
            continue;
        };
        // "<stem>.<...>.<ext>" — the remainder must start with a dot
        if !rest.starts_with('.') {
            continue;
        }
        let ext = rest.rsplit('.').next().unwrap_or("");
        let middle = rest[1..rest.len() - ext.len()].trim_end_matches('.');
        let (codec, text, file) = match ext {
            "sub" => {
                // VobSub pair → handled through the .idx; lone .sub may be MicroDVD text
                let idx_name = format!("{}.idx", &name[..name.len() - 4]);
                if has_file(&idx_name) {
                    continue;
                }
                let full = dir.join(name);
                if !looks_like_text_sub(&full) {
                    continue;
                }
                ("microdvd", true, name.clone())
            }
            "idx" => {
                let sub_name = format!("{}.sub", &name[..name.len() - 4]);
                if !has_file(&sub_name) {
                    continue;
                }
                ("dvd_subtitle", false, name.clone())
            }
            e => match sidecar_codec(e) {
                Some((c, t)) => (c, t, name.clone()),
                None => continue,
            },
        };
        let (lang, forced, title) = parse_sidecar_suffix(middle);
        out.push(SubtitleStream {
            index: EXTERNAL_INDEX_BASE + out.len() as u32,
            codec: codec.to_string(),
            lang,
            title,
            forced,
            text,
            external: Some(dir.join(file).to_string_lossy().to_string()),
        });
    }
    out
}

/// A `.sub` without `.idx` is MicroDVD (`{0}{25}Text`) or SubViewer (`[INFORMATION]`)
/// when it starts with a brace/bracket; anything else is skipped.
fn looks_like_text_sub(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 64];
    let n = f.read(&mut buf).unwrap_or(0);
    let head = String::from_utf8_lossy(&buf[..n]);
    let head = head.trim_start_matches('\u{feff}').trim_start();
    head.starts_with('{') || head.starts_with('[')
}

/// True when ffprobe's codec name is a text subtitle we can convert to WebVTT.
pub fn is_text_subtitle(codec: &str) -> bool {
    matches!(
        codec,
        "subrip"
            | "srt"
            | "ass"
            | "ssa"
            | "webvtt"
            | "mov_text"
            | "text"
            | "ttml"
            | "microdvd"
            | "subviewer"
            | "eia_608"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const MKV_JSON: &str = r#"{
      "streams": [
        {"index": 0, "codec_name": "hevc", "profile": "Main 10", "codec_type": "video",
         "width": 3840, "height": 2160, "pix_fmt": "yuv420p10le", "field_order": "progressive",
         "color_transfer": "smpte2084", "r_frame_rate": "24000/1001", "avg_frame_rate": "24000/1001",
         "disposition": {"default": 1, "attached_pic": 0},
         "side_data_list": [{"side_data_type": "DOVI configuration record", "dv_profile": 8}],
         "tags": {"language": "eng"}},
        {"index": 1, "codec_name": "truehd", "codec_type": "audio", "channels": 8,
         "disposition": {"default": 1}, "tags": {"LANGUAGE": "eng", "title": "TrueHD Atmos 7.1"}},
        {"index": 2, "codec_name": "ac3", "codec_type": "audio", "channels": 6,
         "disposition": {"default": 0}, "tags": {"language": "fre"}},
        {"index": 3, "codec_name": "subrip", "codec_type": "subtitle",
         "disposition": {"default": 0, "forced": 1}, "tags": {"language": "eng", "title": "Forced"}},
        {"index": 4, "codec_name": "hdmv_pgs_subtitle", "codec_type": "subtitle",
         "disposition": {"default": 0, "forced": 0}, "tags": {"language": "und"}},
        {"index": 5, "codec_name": "mjpeg", "codec_type": "video", "width": 600, "height": 900,
         "disposition": {"attached_pic": 1}}
      ],
      "chapters": [
        {"id": 0, "start_time": "0.000000", "end_time": "300.500000", "tags": {"title": "Opening"}},
        {"id": 1, "start_time": "300.500000", "end_time": "900.000000"}
      ],
      "format": {"filename": "/m/Movie (2019).mkv", "format_name": "matroska,webm",
                 "duration": "7200.123000", "size": "30000000000", "bit_rate": "33333333"}
    }"#;

    #[test]
    fn parses_a_4k_hdr_mkv() {
        let info = parse_probe_json(MKV_JSON, 1).unwrap();
        assert_eq!(info.container, "mkv");
        assert_eq!(info.size_bytes, 30_000_000_000);
        assert_eq!(info.bitrate, Some(33_333_333));
        assert!((info.duration_sec - 7200.123).abs() < 1e-6);
        let v = info.video.as_ref().unwrap();
        assert_eq!(v.codec, "hevc");
        assert_eq!(v.profile.as_deref(), Some("Main 10"));
        assert_eq!((v.width, v.height), (3840, 2160));
        assert_eq!(v.fps, Some(23.976));
        assert_eq!(v.bit_depth, Some(10));
        assert_eq!(v.hdr.as_deref(), Some("dv"));
        assert!(!v.interlaced);
        assert_eq!(info.vcodec.as_deref(), Some("hevc"));
        assert_eq!(info.acodec.as_deref(), Some("truehd"));
        assert_eq!((info.width, info.height), (Some(3840), Some(2160)));
        assert_eq!(info.audio.len(), 2);
        assert_eq!(info.audio[0].lang.as_deref(), Some("eng"));
        assert_eq!(info.audio[0].title.as_deref(), Some("TrueHD Atmos 7.1"));
        assert!(info.audio[0].default);
        assert_eq!(info.audio[1].codec, "ac3");
        assert_eq!(info.audio[1].channels, 6);
        assert_eq!(info.subtitles.len(), 2);
        assert!(info.subtitles[0].forced && info.subtitles[0].text);
        assert_eq!(info.subtitles[0].codec, "subrip");
        assert!(!info.subtitles[1].text);
        assert_eq!(info.subtitles[1].lang, None);
        assert_eq!(info.chapters.len(), 2);
        assert_eq!(info.chapters[0].title, "Opening");
        assert_eq!(info.chapters[1].title, "Chapter 2");
        assert!((info.chapters[1].start - 300.5).abs() < 1e-9);
    }

    #[test]
    fn hdr_variants_and_interlacing() {
        let j = |extra: &str| {
            format!(
                r#"{{"streams":[{{"index":0,"codec_type":"video","codec_name":"h264","width":1920,"height":1080,{extra}"pix_fmt":"yuv420p"}}],
                     "format":{{"format_name":"mpegts","filename":"x.ts","duration":"10"}}}}"#
            )
        };
        let i = parse_probe_json(&j(r#""color_transfer":"smpte2084","#), 5).unwrap();
        assert_eq!(i.hdr(), Some("hdr10"));
        let i = parse_probe_json(&j(r#""color_transfer":"arib-std-b67","#), 5).unwrap();
        assert_eq!(i.hdr(), Some("hlg"));
        let i = parse_probe_json(
            &j(r#""color_transfer":"smpte2084","side_data_list":[{"side_data_type":"HDR Dynamic Metadata SMPTE2094-40 (HDR10+)"}],"#),
            5,
        )
        .unwrap();
        assert_eq!(i.hdr(), Some("hdr10plus"));
        let i = parse_probe_json(&j(r#""field_order":"tt","#), 5).unwrap();
        assert!(i.video.unwrap().interlaced);
        assert_eq!(i.container, "ts");
        assert_eq!(i.size_bytes, 5, "size falls back to the filesystem");
        let i = parse_probe_json(&j(""), 5).unwrap();
        assert_eq!(i.video.as_ref().unwrap().bit_depth, Some(8));
        assert_eq!(i.hdr(), None);
    }

    #[test]
    fn duration_falls_back_to_longest_stream_and_cover_art_is_skipped() {
        let json = r#"{"streams":[
            {"index":0,"codec_type":"audio","codec_name":"flac","channels":2,"duration":"245.5"},
            {"index":1,"codec_type":"video","codec_name":"png","width":500,"height":500,"disposition":{"attached_pic":0}}],
            "format":{"format_name":"flac","filename":"/a/b.flac"}}"#;
        let i = parse_probe_json(json, 77).unwrap();
        assert!((i.duration_sec - 245.5).abs() < 1e-9);
        assert!(i.video.is_none(), "png in a flac container is cover art");
        assert_eq!(i.container, "flac");
        assert_eq!(i.acodec.as_deref(), Some("flac"));
        assert_eq!(i.size_bytes, 77);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_probe_json("not json", 1).is_err());
        assert!(parse_probe_json("{}", 1).is_err());
    }

    #[test]
    fn helpers() {
        assert_eq!(parse_rate("30000/1001"), Some(29.97));
        assert_eq!(parse_rate("25/1"), Some(25.0));
        assert_eq!(parse_rate("0/0"), None);
        assert_eq!(parse_rate("x"), None);
        assert_eq!(bit_depth_of("yuv420p10le"), Some(10));
        assert_eq!(bit_depth_of("yuv422p12be"), Some(12));
        assert_eq!(bit_depth_of("yuv420p"), Some(8));
        assert_eq!(bit_depth_of("gbrp16le"), Some(16));
        assert_eq!(bit_depth_of("rgb48le"), Some(16));
        assert_eq!(bit_depth_of("gray"), Some(8));
        assert_eq!(bit_depth_of(""), None);
        assert!(is_text_subtitle("subrip"));
        assert!(!is_text_subtitle("hdmv_pgs_subtitle"));
    }

    #[test]
    fn sidecar_suffix_parsing() {
        assert_eq!(parse_sidecar_suffix(""), (None, false, None));
        assert_eq!(parse_sidecar_suffix("en"), (Some("en".into()), false, None));
        assert_eq!(parse_sidecar_suffix("en.forced"), (Some("en".into()), true, None));
        assert_eq!(parse_sidecar_suffix("English"), (Some("en".into()), false, None));
        assert_eq!(parse_sidecar_suffix("pt-BR"), (Some("pt-br".into()), false, None));
        assert_eq!(parse_sidecar_suffix("eng.sdh"), (Some("en".into()), false, Some("SDH".into())));
        assert_eq!(parse_sidecar_suffix("Commentary"), (None, false, Some("Commentary".into())));
    }

    #[test]
    fn finds_sidecars_next_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let media = d.join("Movie (2019).mkv");
        std::fs::write(&media, b"x").unwrap();
        std::fs::write(d.join("Movie (2019).srt"), b"1\n").unwrap();
        std::fs::write(d.join("Movie (2019).en.forced.vtt"), b"WEBVTT\n").unwrap();
        std::fs::write(d.join("Movie (2019).de.ass"), b"[Script Info]\n").unwrap();
        std::fs::write(d.join("Movie (2019).fr.sub"), b"\x00\x00\x01\xba").unwrap();
        std::fs::write(d.join("Movie (2019).fr.idx"), b"# VobSub index file\n").unwrap();
        std::fs::write(d.join("Movie (2019).es.sub"), b"{1}{25}Hola\n").unwrap();
        std::fs::write(d.join("Movie (2019).nfo"), b"<movie/>").unwrap();
        std::fs::write(d.join("Other Movie.srt"), b"1\n").unwrap();
        std::fs::write(d.join("Movie (2019)-extra.srt"), b"1\n").unwrap();

        let subs = external_subtitles(&media);
        let names: Vec<(String, Option<String>, bool, bool)> =
            subs.iter().map(|s| (s.codec.clone(), s.lang.clone(), s.forced, s.text)).collect();
        assert_eq!(subs.len(), 5, "{names:?}");
        assert!(names.contains(&("subrip".into(), None, false, true)));
        assert!(names.contains(&("webvtt".into(), Some("en".into()), true, true)));
        assert!(names.contains(&("ass".into(), Some("de".into()), false, true)));
        assert!(names.contains(&("dvd_subtitle".into(), Some("fr".into()), false, false)));
        assert!(names.contains(&("microdvd".into(), Some("es".into()), false, true)));
        for (i, s) in subs.iter().enumerate() {
            assert_eq!(s.index, 1000 + i as u32);
            assert!(s.external.as_deref().unwrap().starts_with(d.to_str().unwrap()));
        }
        let idx = subs.iter().find(|s| s.codec == "dvd_subtitle").unwrap();
        assert!(idx.external.as_deref().unwrap().ends_with(".idx"));
        assert!(external_subtitles(Path::new("/nonexistent/dir/x.mkv")).is_empty());
    }

    #[tokio::test]
    async fn probes_a_synthesized_file() {
        if which("ffmpeg").is_none() || which("ffprobe").is_none() {
            eprintln!("ffmpeg/ffprobe not on PATH; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("clip.mp4");
        let st = tokio::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x180:rate=25",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440",
                "-t",
                "3",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-y",
            ])
            .arg(&out)
            .status()
            .await
            .unwrap();
        assert!(st.success());
        let info = probe("ffprobe", &out).await.unwrap();
        assert_eq!(info.container, "mp4");
        assert_eq!(info.vcodec.as_deref(), Some("h264"));
        assert_eq!(info.acodec.as_deref(), Some("aac"));
        assert_eq!((info.width, info.height), (Some(320), Some(180)));
        assert!((info.duration_sec - 3.0).abs() < 0.2, "{}", info.duration_sec);
        assert_eq!(info.video.as_ref().unwrap().fps, Some(25.0));
        assert_eq!(info.audio[0].channels, 1);
        assert!(info.size_bytes > 0);
        assert!(probe("ffprobe", &dir.path().join("missing.mp4")).await.is_err());
    }

    fn which(bin: &str) -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path).map(|p| p.join(bin)).find(|p| p.is_file())
    }
}
