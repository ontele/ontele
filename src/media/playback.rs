// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! Direct-play / remux / transcode decision matrix and codec tables.

use crate::model::{ClientCaps, MediaInfo, PlaybackPlan, SegmentKind};

/// Quality ladder heights offered to the UI.
pub const LADDER: &[u32] = &[2160, 1440, 1080, 720, 480, 360];

/// Default height for live TV when the client does not ask for one.
const LIVE_DEFAULT_HEIGHT: u32 = 720;

/// Video codecs that are remuxed into fragmented MP4 segments (MPEG-TS has
/// no sane mapping for them in browsers).
const FMP4_CODECS: &[&str] = &["hevc", "av1", "vp9", "vp8"];

/// Audio codecs ffmpeg's mpegts muxer can carry as-is.
const TS_AUDIO_CODECS: &[&str] = &["aac", "mp3", "mp2", "ac3", "eac3", "dts", "opus"];

/// How the client asked for the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Requested {
    /// `auto` / `direct` / empty: play the file as-is whenever possible.
    PreferDirect,
    /// `original`: remux, never re-encode video.
    Original,
    /// A ladder rung (height in pixels).
    Height(u32),
}

fn parse_quality(q: &str) -> Requested {
    let q = q.trim().to_ascii_lowercase();
    match q.as_str() {
        "" | "auto" | "direct" => Requested::PreferDirect,
        "original" | "copy" | "source" => Requested::Original,
        _ => {
            let digits: String = q.chars().take_while(|c| c.is_ascii_digit()).collect();
            match digits.parse::<u32>() {
                Ok(h) if h > 0 => Requested::Height(h),
                _ => Requested::PreferDirect,
            }
        }
    }
}

fn has(list: &[String], name: &str) -> bool {
    list.iter().any(|c| c.eq_ignore_ascii_case(name))
}

/// Source height, if known, taking the flattened field first.
fn source_height(info: &MediaInfo) -> Option<u32> {
    info.height.filter(|h| *h > 0).or_else(|| info.video.as_ref().map(|v| v.height).filter(|h| *h > 0))
}

/// Normalized video codec name, if the file has a real video stream.
fn source_vcodec(info: &MediaInfo) -> Option<String> {
    info.vcodec
        .as_deref()
        .or_else(|| info.video.as_ref().map(|v| v.codec.as_str()))
        .filter(|c| !c.is_empty())
        .map(normalize_codec)
}

/// (normalized codec, channels) of the audio track that will be used — the
/// default-flagged stream, else the first one, else the flattened field.
fn source_audio(info: &MediaInfo) -> Option<(String, u32)> {
    let stream = info.audio.iter().find(|a| a.default).or_else(|| info.audio.first());
    match stream {
        Some(a) => Some((normalize_codec(&a.codec), a.channels)),
        None => info.acodec.as_deref().filter(|c| !c.is_empty()).map(|c| (normalize_codec(c), 2)),
    }
}

/// Audio copy rule: codec in caps.audio and (≤ 2 channels or the client
/// handles surround). Returns `(copy, reason_when_not_copied)`.
fn audio_decision(audio: Option<&(String, u32)>, caps: &ClientCaps) -> (bool, Option<String>) {
    match audio {
        None => (true, None),
        Some((codec, channels)) => {
            if !has(&caps.audio, codec) {
                (false, Some(format!("audio codec {codec} not decodable by client")))
            } else if *channels > 2 && !caps.surround {
                (false, Some(format!("{channels}-channel {codec} needs a stereo downmix")))
            } else {
                (true, None)
            }
        }
    }
}

fn segment_for(vcodec: Option<&str>, video_copy: bool) -> SegmentKind {
    match vcodec {
        Some(c) if video_copy && FMP4_CODECS.contains(&c) => SegmentKind::Fmp4,
        _ => SegmentKind::Ts,
    }
}

/// Decide how to play `info` for a client with `caps`.
/// `quality`: "auto" | "direct" | "original" | "1080" | "720" | … ; `live`
/// forces transcode paths suitable for broadcast MPEG-2 (yadif, TS).
/// Rules:
/// 1. direct when container+vcodec+acodec are all client-decodable and the
///    requested quality is auto/direct;
/// 2. copy (HLS remux) when only the container is the problem (or quality is
///    "original"); fMP4 segments for hevc/av1/vp9/vp8, TS otherwise;
/// 3. transcode (h264, requested height ≤ source height, TS) otherwise.
///
/// Audio is copied when the codec is in caps.audio (and ≤2ch unless
/// caps.surround), else AAC.
pub fn decide(info: &MediaInfo, caps: &ClientCaps, quality: &str, live: bool) -> PlaybackPlan {
    let requested = parse_quality(quality);

    // ---- live: always a fresh h264/aac TS transcode (yadif for broadcast) ----
    if live {
        let height = match requested {
            Requested::Height(h) => h,
            Requested::Original => source_height(info).unwrap_or(1080),
            Requested::PreferDirect => LIVE_DEFAULT_HEIGHT,
        };
        // max_height 0 means "no limit", same as the VOD path below.
        let max_height = if caps.max_height == 0 { u32::MAX } else { caps.max_height.max(360) };
        return PlaybackPlan {
            mode: "transcode".into(),
            video_copy: false,
            audio_copy: false,
            height: height.min(max_height),
            segment: SegmentKind::Ts,
            reasons: vec!["live broadcast (MPEG-2)".into()],
        };
    }

    let vcodec = source_vcodec(info);
    let src_height = source_height(info);
    let audio = source_audio(info);
    let container = info.container.to_ascii_lowercase();
    let max_height = if caps.max_height == 0 { u32::MAX } else { caps.max_height };
    let mut reasons: Vec<String> = Vec::new();

    // ---- what the client can decode of the source ----
    let video_decodable = match vcodec.as_deref() {
        Some(c) => has(&caps.video, c),
        None => true, // audio-only: nothing to decode
    };
    let height_ok = src_height.is_none_or(|h| h <= max_height);
    let (audio_copy, audio_reason) = audio_decision(audio.as_ref(), caps);
    let container_ok = !container.is_empty() && has(&caps.containers, &container);

    // HDR outside mp4/webm is unreliable for direct play (mkv in Chrome is a
    // coin flip); HLS remux through MSE behaves.
    let hdr = info.hdr();
    let hdr_direct_ok = match hdr {
        None => true,
        Some(_) => video_decodable && matches!(container.as_str(), "mp4" | "webm"),
    };

    let direct_ok = container_ok && video_decodable && height_ok && audio_copy && hdr_direct_ok;

    // ---- 1. direct ----
    if requested == Requested::PreferDirect && direct_ok {
        return PlaybackPlan {
            mode: "direct".into(),
            video_copy: true,
            audio_copy: true,
            height: 0,
            segment: segment_for(vcodec.as_deref(), true),
            reasons: vec!["client decodes container, video and audio natively".into()],
        };
    }

    // Collect why direct was not possible (useful in the UI / logs).
    if requested == Requested::PreferDirect {
        if !container_ok {
            reasons.push(format!(
                "container {} not playable natively",
                if container.is_empty() { "unknown" } else { &container }
            ));
        }
        if let Some(r) = audio_reason.clone() {
            reasons.push(r);
        }
        if hdr.is_some() && !hdr_direct_ok && video_decodable {
            reasons.push(format!(
                "{} in {} is unreliable for direct play; remuxing to HLS",
                hdr.unwrap_or("hdr"),
                container
            ));
        }
    } else if requested == Requested::Original {
        reasons.push("original quality requested".into());
    }
    if let Some(c) = vcodec.as_deref().filter(|_| !video_decodable) {
        reasons.push(format!("video codec {c} not decodable by client"));
    }
    if !height_ok {
        reasons.push(format!("source height {} exceeds client maximum {}", src_height.unwrap_or(0), max_height));
    }

    let can_copy_video = video_decodable && height_ok;

    // ---- 2. copy (remux) ----
    let copy_plan = |mut reasons: Vec<String>, audio_copy: bool| -> PlaybackPlan {
        let segment = segment_for(vcodec.as_deref(), true);
        let audio_copy = finalize_audio(segment, audio.as_ref(), audio_copy, &mut reasons);
        PlaybackPlan { mode: "copy".into(), video_copy: true, audio_copy, height: 0, segment, reasons }
    };

    match requested {
        Requested::PreferDirect | Requested::Original if can_copy_video => {
            if requested == Requested::Original
                && let Some(r) = audio_reason.clone()
            {
                reasons.push(r);
            }
            copy_plan(reasons, audio_copy)
        }
        Requested::Height(h) if can_copy_video && src_height.is_some_and(|sh| sh <= h) => {
            reasons.push(format!(
                "source height {} is within requested {}; remuxing instead of transcoding",
                src_height.unwrap_or(0),
                h
            ));
            if let Some(r) = audio_reason.clone() {
                reasons.push(r);
            }
            copy_plan(reasons, audio_copy)
        }
        _ => {
            // ---- 3. transcode ----
            let mut target = match requested {
                Requested::Height(h) => h,
                _ => src_height.unwrap_or(0),
            };
            if let Some(sh) = src_height {
                target = target.min(sh);
            }
            if target > max_height {
                target = max_height;
            }
            if let Some(r) = audio_reason {
                reasons.push(r);
            }
            if reasons.is_empty() {
                reasons.push(format!("transcode to {target}p requested"));
            }
            let segment = SegmentKind::Ts;
            let audio_copy = finalize_audio(segment, audio.as_ref(), audio_copy, &mut reasons);
            PlaybackPlan { mode: "transcode".into(), video_copy: false, audio_copy, height: target, segment, reasons }
        }
    }
}

/// MPEG-TS cannot carry every codec the browser could decode (flac, alac,
/// vorbis, pcm, truehd); those get re-encoded to AAC even when decodable.
fn finalize_audio(segment: SegmentKind, audio: Option<&(String, u32)>, copy: bool, reasons: &mut Vec<String>) -> bool {
    if !copy {
        return false;
    }
    if segment == SegmentKind::Ts
        && let Some((codec, _)) = audio
        && !TS_AUDIO_CODECS.contains(&codec.as_str())
    {
        reasons.push(format!("audio codec {codec} cannot be muxed into MPEG-TS segments"));
        return false;
    }
    true
}

/// MIME type for direct play by container name (ffprobe format) / extension.
pub fn direct_mime(container: &str, ext: &str) -> &'static str {
    fn by_name(name: &str) -> Option<&'static str> {
        Some(match name {
            "mp4" | "m4v" => "video/mp4",
            "webm" => "video/webm",
            "mkv" | "matroska" => "video/x-matroska",
            "mov" | "quicktime" => "video/quicktime",
            "ts" | "m2ts" | "mts" | "mpegts" => "video/mp2t",
            "avi" => "video/x-msvideo",
            "flv" => "video/x-flv",
            "ogv" => "video/ogg",
            "mp3" => "audio/mpeg",
            "flac" => "audio/flac",
            "m4a" | "m4b" | "aac" => "audio/mp4",
            "ogg" | "oga" | "opus" => "audio/ogg",
            "wav" => "audio/wav",
            _ => return None,
        })
    }
    let c = container.trim().to_ascii_lowercase();
    let e = ext.trim().trim_start_matches('.').to_ascii_lowercase();
    by_name(&c).or_else(|| by_name(&e)).unwrap_or("application/octet-stream")
}

/// Normalize ffprobe codec names to the short names the UI's caps use
/// (`h264`, `hevc`, `av1`, `vp9`, `vp8`, `mpeg2`, `aac`, `mp3`, `opus`,
/// `vorbis`, `flac`, `ac3`, `eac3`, `dts`, `truehd`, `pcm`, `alac`).
pub fn normalize_codec(name: &str) -> String {
    let n = name.trim().to_ascii_lowercase();
    let short = match n.as_str() {
        "h264" | "avc" | "avc1" | "x264" => "h264",
        "hevc" | "h265" | "hvc1" | "hev1" | "x265" => "hevc",
        "av1" | "av01" => "av1",
        "vp9" | "vp09" => "vp9",
        "vp8" | "vp08" => "vp8",
        "mpeg2video" | "mpeg2" => "mpeg2",
        "mpeg4" | "xvid" | "divx" | "mp4v" => "mpeg4",
        "aac" | "aac_latm" | "mp4a" => "aac",
        "mp3" | "mp3float" | "mp3adu" | "mp3on4" => "mp3",
        "opus" => "opus",
        "vorbis" => "vorbis",
        "flac" => "flac",
        "ac3" | "ac-3" => "ac3",
        "eac3" | "ec-3" | "e-ac-3" => "eac3",
        "dts" | "dca" => "dts",
        "truehd" => "truehd",
        "alac" => "alac",
        "wmav1" | "wmav2" | "wmapro" | "wmalossless" | "wmavoice" => "wma",
        "mp2" | "mp2float" => "mp2",
        _ if n.starts_with("pcm_") || n == "pcm" => "pcm",
        _ => return n,
    };
    short.to_string()
}

/// Browser-facing container name from ffprobe `format_name`
/// (`matroska,webm` → `mkv`/`webm` by extension, `mov,mp4,m4a,…` → `mp4`).
pub fn normalize_container(format_name: &str, ext: &str) -> String {
    let f = format_name.trim().to_ascii_lowercase();
    let e = ext.trim().trim_start_matches('.').to_ascii_lowercase();
    let first = f.split(',').next().unwrap_or("").trim().to_string();
    match f.as_str() {
        "matroska,webm" | "matroska" | "webm" => {
            if e == "webm" {
                "webm".into()
            } else {
                "mkv".into()
            }
        }
        "mov,mp4,m4a,3gp,3g2,mj2" | "mp4" | "mov" => {
            if e == "mov" {
                "mov".into()
            } else {
                "mp4".into()
            }
        }
        "mpegts" => "ts".into(),
        "avi" => "avi".into(),
        "flv" => "flv".into(),
        "ogg" => match e.as_str() {
            "oga" | "ogv" | "opus" | "spx" | "ogg" => e,
            _ => "ogg".into(),
        },
        "mp3" => "mp3".into(),
        "flac" => "flac".into(),
        "wav" => "wav".into(),
        "asf" => "wmv".into(),
        "" => e,
        _ => first,
    }
}

/// Audio formats browsers play via `<audio>` without help.
pub fn audio_direct_ok(container: &str, codec: &str) -> bool {
    let c = container.trim().to_ascii_lowercase();
    let k = normalize_codec(codec);
    matches!(
        (c.as_str(), k.as_str()),
        ("mp3", "mp3")
            | ("flac", "flac")
            | ("mp4", "aac")
            | ("mp4", "mp3")
            | ("ogg", "vorbis")
            | ("ogg", "opus")
            | ("oga", "vorbis")
            | ("oga", "opus")
            | ("opus", "opus")
            | ("wav", "pcm")
            | ("webm", "opus")
            | ("webm", "vorbis")
            | ("aac", "aac")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AudioStream, VideoStream};

    fn video(container: &str, vcodec: &str, acodec: &str, channels: u32, height: u32) -> MediaInfo {
        MediaInfo {
            duration_sec: 100.0,
            container: container.into(),
            size_bytes: 1,
            vcodec: Some(vcodec.into()),
            acodec: Some(acodec.into()),
            width: Some(height * 16 / 9),
            height: Some(height),
            video: Some(VideoStream {
                index: 0,
                codec: vcodec.into(),
                width: height * 16 / 9,
                height,
                ..Default::default()
            }),
            audio: vec![AudioStream { index: 1, codec: acodec.into(), channels, default: true, ..Default::default() }],
            ..Default::default()
        }
    }

    fn chrome() -> ClientCaps {
        ClientCaps {
            video: vec!["h264".into(), "vp9".into(), "av1".into()],
            audio: vec!["aac".into(), "mp3".into(), "opus".into(), "flac".into(), "vorbis".into()],
            containers: vec!["mp4".into(), "webm".into()],
            hls: "mse".into(),
            max_height: 2160,
            surround: false,
        }
    }

    fn safari() -> ClientCaps {
        ClientCaps {
            video: vec!["h264".into(), "hevc".into()],
            audio: vec!["aac".into(), "mp3".into(), "ac3".into(), "eac3".into(), "alac".into(), "flac".into()],
            containers: vec!["mp4".into(), "mov".into()],
            hls: "native".into(),
            max_height: 2160,
            surround: true,
        }
    }

    #[test]
    fn direct_when_everything_is_supported() {
        let p = decide(&video("mp4", "h264", "aac", 2, 1080), &chrome(), "auto", false);
        assert_eq!(p.mode, "direct");
        assert!(p.video_copy && p.audio_copy);
        assert_eq!(p.height, 0);
        let p = decide(&video("mp4", "h264", "aac", 2, 1080), &chrome(), "direct", false);
        assert_eq!(p.mode, "direct");
    }

    #[test]
    fn mkv_h264_is_remuxed_to_ts() {
        let p = decide(&video("mkv", "h264", "aac", 2, 1080), &chrome(), "auto", false);
        assert_eq!(p.mode, "copy");
        assert!(p.video_copy && p.audio_copy);
        assert_eq!(p.segment, SegmentKind::Ts);
        assert!(p.reasons.iter().any(|r| r.contains("container mkv")));
    }

    #[test]
    fn mkv_hevc_on_safari_is_fmp4_copy() {
        let p = decide(&video("mkv", "hevc", "eac3", 6, 2160), &safari(), "auto", false);
        assert_eq!(p.mode, "copy");
        assert_eq!(p.segment, SegmentKind::Fmp4);
        assert!(p.audio_copy, "surround-capable client keeps eac3 6ch");
    }

    #[test]
    fn surround_is_downmixed_without_caps_surround() {
        let p = decide(&video("mkv", "h264", "aac", 6, 1080), &chrome(), "auto", false);
        assert_eq!(p.mode, "copy");
        assert!(!p.audio_copy);
        assert!(p.reasons.iter().any(|r| r.contains("downmix")));
    }

    #[test]
    fn undecodable_video_transcodes_at_source_height() {
        let p = decide(&video("mkv", "hevc", "dts", 6, 1080), &chrome(), "auto", false);
        assert_eq!(p.mode, "transcode");
        assert!(!p.video_copy && !p.audio_copy);
        assert_eq!(p.height, 1080);
        assert_eq!(p.segment, SegmentKind::Ts);
        assert!(p.reasons.iter().any(|r| r.contains("hevc")));
    }

    #[test]
    fn numeric_quality_caps_at_source_height_and_downgrades_to_copy() {
        // 720 requested, 1080 source, decodable → transcode to 720
        let p = decide(&video("mkv", "h264", "aac", 2, 1080), &chrome(), "720", false);
        assert_eq!(p.mode, "transcode");
        assert_eq!(p.height, 720);
        // 1080 requested, 720 source, decodable → copy instead (with a reason)
        let p = decide(&video("mkv", "h264", "aac", 2, 720), &chrome(), "1080", false);
        assert_eq!(p.mode, "copy");
        assert!(p.video_copy);
        assert!(p.reasons.iter().any(|r| r.contains("remuxing instead of transcoding")));
        // 1080 requested, 720 source, NOT decodable → transcode at 720 (capped)
        let p = decide(&video("mkv", "hevc", "aac", 2, 720), &chrome(), "1080", false);
        assert_eq!(p.mode, "transcode");
        assert_eq!(p.height, 720);
    }

    #[test]
    fn original_forces_copy_even_when_direct_is_possible() {
        let p = decide(&video("mp4", "h264", "aac", 2, 1080), &chrome(), "original", false);
        assert_eq!(p.mode, "copy");
        assert!(p.video_copy);
    }

    #[test]
    fn hdr_in_mkv_never_plays_direct_but_mp4_does() {
        let mut info = video("mkv", "hevc", "aac", 2, 2160);
        info.video.as_mut().unwrap().hdr = Some("hdr10".into());
        let mut caps = safari();
        caps.containers.push("mkv".into());
        let p = decide(&info, &caps, "auto", false);
        assert_eq!(p.mode, "copy");
        assert!(p.reasons.iter().any(|r| r.contains("hdr10 in mkv")));
        info.container = "mp4".into();
        let p = decide(&info, &caps, "auto", false);
        assert_eq!(p.mode, "direct");
    }

    #[test]
    fn max_height_forces_transcode() {
        let mut caps = chrome();
        caps.max_height = 1080;
        let p = decide(&video("mp4", "h264", "aac", 2, 2160), &caps, "auto", false);
        assert_eq!(p.mode, "transcode");
        assert_eq!(p.height, 1080);
    }

    #[test]
    fn live_is_always_ts_transcode() {
        let p = decide(&video("ts", "mpeg2", "ac3", 6, 1080), &chrome(), "auto", true);
        assert_eq!(p.mode, "transcode");
        assert_eq!(p.height, 720);
        assert_eq!(p.segment, SegmentKind::Ts);
        assert!(!p.video_copy && !p.audio_copy);
        let p = decide(&video("ts", "mpeg2", "ac3", 6, 1080), &chrome(), "480", true);
        assert_eq!(p.height, 480);
        // max_height 0 = unlimited, same as the VOD path (regression: was capped at 360)
        let mut caps = chrome();
        caps.max_height = 0;
        let p = decide(&video("ts", "mpeg2", "ac3", 6, 1080), &caps, "1080", true);
        assert_eq!(p.height, 1080);
        let p = decide(&video("ts", "mpeg2", "ac3", 6, 1080), &caps, "auto", true);
        assert_eq!(p.height, 720);
    }

    #[test]
    fn ts_segments_refuse_flac_audio_copy() {
        // h264 copy → TS; flac is decodable but not TS-muxable → re-encode audio
        let p = decide(&video("mkv", "h264", "flac", 2, 1080), &chrome(), "auto", false);
        assert_eq!(p.mode, "copy");
        assert!(!p.audio_copy);
        // vp9 copy → fMP4; opus stays
        let p = decide(&video("mkv", "vp9", "opus", 2, 1080), &chrome(), "auto", false);
        assert_eq!(p.segment, SegmentKind::Fmp4);
        assert!(p.audio_copy);
    }

    #[test]
    fn audio_only_files_take_the_copy_path() {
        let info = MediaInfo {
            container: "flac".into(),
            acodec: Some("flac".into()),
            audio: vec![AudioStream { index: 0, codec: "flac".into(), channels: 2, ..Default::default() }],
            ..Default::default()
        };
        let p = decide(&info, &chrome(), "auto", false);
        assert_eq!(p.mode, "copy");
        assert!(p.video_copy);
    }

    #[test]
    fn mime_table() {
        assert_eq!(direct_mime("mp4", "mp4"), "video/mp4");
        assert_eq!(direct_mime("mkv", "mkv"), "video/x-matroska");
        assert_eq!(direct_mime("ts", "ts"), "video/mp2t");
        assert_eq!(direct_mime("", "m2ts"), "video/mp2t");
        assert_eq!(direct_mime("webm", "webm"), "video/webm");
        assert_eq!(direct_mime("mov", "mov"), "video/quicktime");
        assert_eq!(direct_mime("avi", "avi"), "video/x-msvideo");
        assert_eq!(direct_mime("flv", "flv"), "video/x-flv");
        assert_eq!(direct_mime("ogv", "ogv"), "video/ogg");
        assert_eq!(direct_mime("mp3", "mp3"), "audio/mpeg");
        assert_eq!(direct_mime("flac", "flac"), "audio/flac");
        assert_eq!(direct_mime("mp4", "m4a"), "video/mp4");
        assert_eq!(direct_mime("", "m4a"), "audio/mp4");
        assert_eq!(direct_mime("aac", "aac"), "audio/mp4");
        assert_eq!(direct_mime("ogg", "ogg"), "audio/ogg");
        assert_eq!(direct_mime("opus", "opus"), "audio/ogg");
        assert_eq!(direct_mime("wav", "wav"), "audio/wav");
        assert_eq!(direct_mime("rm", "rm"), "application/octet-stream");
    }

    #[test]
    fn codec_normalization() {
        assert_eq!(normalize_codec("h264"), "h264");
        assert_eq!(normalize_codec("avc1"), "h264");
        assert_eq!(normalize_codec("hevc"), "hevc");
        assert_eq!(normalize_codec("H265"), "hevc");
        assert_eq!(normalize_codec("hvc1"), "hevc");
        assert_eq!(normalize_codec("av01"), "av1");
        assert_eq!(normalize_codec("vp09"), "vp9");
        assert_eq!(normalize_codec("vp8"), "vp8");
        assert_eq!(normalize_codec("mpeg2video"), "mpeg2");
        assert_eq!(normalize_codec("xvid"), "mpeg4");
        assert_eq!(normalize_codec("mp3float"), "mp3");
        assert_eq!(normalize_codec("dca"), "dts");
        assert_eq!(normalize_codec("pcm_s16le"), "pcm");
        assert_eq!(normalize_codec("pcm_s24be"), "pcm");
        assert_eq!(normalize_codec("wmapro"), "wma");
        assert_eq!(normalize_codec("alac"), "alac");
        assert_eq!(normalize_codec("TrueHD"), "truehd");
        assert_eq!(normalize_codec("mp2"), "mp2");
        assert_eq!(normalize_codec("prores"), "prores");
    }

    #[test]
    fn container_normalization() {
        assert_eq!(normalize_container("matroska,webm", "mkv"), "mkv");
        assert_eq!(normalize_container("matroska,webm", "webm"), "webm");
        assert_eq!(normalize_container("mov,mp4,m4a,3gp,3g2,mj2", "mp4"), "mp4");
        assert_eq!(normalize_container("mov,mp4,m4a,3gp,3g2,mj2", "m4a"), "mp4");
        assert_eq!(normalize_container("mov,mp4,m4a,3gp,3g2,mj2", "mov"), "mov");
        assert_eq!(normalize_container("mpegts", "ts"), "ts");
        assert_eq!(normalize_container("avi", "avi"), "avi");
        assert_eq!(normalize_container("flv", "flv"), "flv");
        assert_eq!(normalize_container("ogg", "ogv"), "ogv");
        assert_eq!(normalize_container("ogg", "ogg"), "ogg");
        assert_eq!(normalize_container("ogg", ""), "ogg");
        assert_eq!(normalize_container("mp3", "mp3"), "mp3");
        assert_eq!(normalize_container("flac", "flac"), "flac");
        assert_eq!(normalize_container("wav", "wav"), "wav");
        assert_eq!(normalize_container("asf", "wmv"), "wmv");
        assert_eq!(normalize_container("mpeg,vob", "vob"), "mpeg");
    }

    #[test]
    fn audio_direct_table() {
        assert!(audio_direct_ok("mp3", "mp3"));
        assert!(audio_direct_ok("flac", "flac"));
        assert!(audio_direct_ok("mp4", "aac"));
        assert!(!audio_direct_ok("mp4", "alac"));
        assert!(audio_direct_ok("ogg", "vorbis"));
        assert!(audio_direct_ok("ogg", "opus"));
        assert!(audio_direct_ok("wav", "pcm_s16le"));
        assert!(audio_direct_ok("webm", "opus"));
        assert!(audio_direct_ok("aac", "aac"));
        assert!(!audio_direct_ok("mkv", "aac"));
        assert!(!audio_direct_ok("wma", "wma"));
    }

    #[test]
    fn quality_parsing() {
        assert_eq!(parse_quality("auto"), Requested::PreferDirect);
        assert_eq!(parse_quality(""), Requested::PreferDirect);
        assert_eq!(parse_quality("original"), Requested::Original);
        assert_eq!(parse_quality("1080"), Requested::Height(1080));
        assert_eq!(parse_quality("720p"), Requested::Height(720));
        assert_eq!(parse_quality("garbage"), Requested::PreferDirect);
    }
}
