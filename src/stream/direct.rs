// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! Direct play (range requests via tower-http `ServeFile`) and progressive
//! audio streaming (direct or ffmpeg → stdout as ADTS AAC / MP3 / Ogg Opus).

use crate::media::playback;
use crate::model::{MediaInfo, Settings};
use axum::{
    Json,
    body::{Body, Bytes},
    extract::Request,
    http::{HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures::Stream;
use serde_json::json;
use std::{
    path::Path,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::io::ReaderStream;
use tower::ServiceExt;
use tower_http::services::ServeFile;

/// Range-capable file response with the given MIME type.
pub async fn serve_file(path: &Path, mime: &str, req: Request) -> Response {
    let mime: mime_guess::Mime = match mime.parse() {
        Ok(m) => m,
        Err(_) => mime_guess::mime::APPLICATION_OCTET_STREAM,
    };
    let svc = ServeFile::new_with_mime(path, &mime);
    match svc.oneshot(req).await {
        Ok(res) => {
            let (parts, body) = res.into_parts();
            if parts.status == StatusCode::NOT_FOUND {
                return (StatusCode::NOT_FOUND, Json(json!({ "error": "file not found" }))).into_response();
            }
            let mut res = Response::from_parts(parts, Body::new(body));
            res.headers_mut().entry(header::ACCEPT_RANGES).or_insert(HeaderValue::from_static("bytes"));
            res
        }
        Err(never) => match never {},
    }
}

/// Encoder targets for on-the-fly audio transcoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioTarget {
    Aac,
    Mp3,
    Opus,
}

impl AudioTarget {
    pub fn parse(fmt: &str) -> Option<Self> {
        match fmt {
            "aac" => Some(AudioTarget::Aac),
            "mp3" => Some(AudioTarget::Mp3),
            "opus" => Some(AudioTarget::Opus),
            _ => None,
        }
    }
    pub fn codec_name(self) -> &'static str {
        match self {
            AudioTarget::Aac => "aac",
            AudioTarget::Mp3 => "mp3",
            AudioTarget::Opus => "opus",
        }
    }
    pub fn mime(self) -> &'static str {
        match self {
            AudioTarget::Aac => "audio/aac",
            AudioTarget::Mp3 => "audio/mpeg",
            AudioTarget::Opus => "audio/ogg",
        }
    }
}

/// ffmpeg argument list for streaming `path` from `start` seconds as
/// `target` to stdout (pure, tested).
pub fn audio_args(path: &Path, start: f64, target: AudioTarget) -> Vec<String> {
    let mut a: Vec<String> =
        vec!["-hide_banner", "-loglevel", "error", "-nostats", "-nostdin"].into_iter().map(String::from).collect();
    if start > 0.0 {
        a.push("-ss".into());
        a.push(format!("{start:.3}"));
    }
    a.push("-i".into());
    a.push(path.to_string_lossy().to_string());
    a.extend(["-vn", "-sn", "-dn", "-map", "0:a:0", "-ac", "2"].into_iter().map(String::from));
    let codec: &[&str] = match target {
        AudioTarget::Aac => &["-c:a", "aac", "-b:a", "192k", "-f", "adts"],
        AudioTarget::Mp3 => &["-c:a", "libmp3lame", "-b:a", "192k", "-id3v2_version", "0", "-f", "mp3"],
        AudioTarget::Opus => &["-c:a", "libopus", "-b:a", "128k", "-ar", "48000", "-f", "ogg"],
    };
    a.extend(codec.iter().map(|s| s.to_string()));
    a.push("-".into());
    a
}

/// MIME type for serving an audio file as-is.
pub fn audio_mime(container: &str, codec: &str, ext: &str) -> String {
    let c = container.to_ascii_lowercase();
    let k = codec.to_ascii_lowercase();
    let e = ext.to_ascii_lowercase();
    let m = match (c.as_str(), k.as_str(), e.as_str()) {
        ("flac", _, _) | (_, "flac", "flac") => "audio/flac",
        ("mp3", _, _) | (_, "mp3", "mp3") => "audio/mpeg",
        ("aac" | "adts", _, _) | (_, "aac", "aac") => "audio/aac",
        ("mp4" | "m4a" | "mov", _, _) | (_, _, "m4a" | "mp4") => "audio/mp4",
        ("ogg" | "oga" | "opus", _, _) | (_, _, "ogg" | "oga" | "opus") => "audio/ogg",
        ("wav", _, _) | (_, _, "wav") => "audio/wav",
        ("webm" | "mkv" | "matroska", _, _) | (_, _, "webm") => "audio/webm",
        ("aiff" | "aif", _, _) | (_, _, "aiff" | "aif") => "audio/aiff",
        _ => "",
    };
    if !m.is_empty() {
        return m.to_string();
    }
    if !e.is_empty()
        && let Some(g) = mime_guess::from_ext(&e).first()
    {
        return g.to_string();
    }
    "application/octet-stream".to_string()
}

/// `GET /stream/audio/{id}?fmt=auto|aac|mp3|opus&t=`: serve the file as-is
/// when the browser can play it, else transcode on the fly from offset `t`.
pub async fn audio(set: &Settings, path: &Path, info: &MediaInfo, fmt: &str, start: f64, req: Request) -> Response {
    if !tokio::fs::metadata(path).await.map(|m| m.is_file()).unwrap_or(false) {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "media file unavailable" }))).into_response();
    }
    let fmt = fmt.trim().to_ascii_lowercase();
    let fmt = if fmt.is_empty() { "auto".to_string() } else { fmt };
    let codec = info
        .acodec
        .clone()
        .or_else(|| info.audio.first().map(|a| a.codec.clone()))
        .unwrap_or_default()
        .to_ascii_lowercase();
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();

    let target = match fmt.as_str() {
        "auto" => {
            if playback::audio_direct_ok(&info.container, &codec) {
                None
            } else {
                Some(AudioTarget::Aac)
            }
        }
        f => match AudioTarget::parse(f) {
            // explicit target equal to the source codec → no re-encode when the browser can play it
            Some(t) if t.codec_name() == codec && playback::audio_direct_ok(&info.container, &codec) => None,
            Some(t) => Some(t),
            // e.g. fmt=flac on a flac: hand out the original
            None if f == codec => None,
            None => {
                return (StatusCode::BAD_REQUEST, Json(json!({ "error": format!("unsupported audio format '{f}'") })))
                    .into_response();
            }
        },
    };

    let Some(target) = target else {
        // direct: byte ranges do the seeking, `t` is ignored
        let mime = audio_mime(&info.container, &codec, &ext);
        return serve_file(path, &mime, req).await;
    };

    let start = if start.is_finite() { start.max(0.0) } else { 0.0 };
    let headers = |res: &mut Response| {
        let h = res.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static(target.mime()));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("none"));
        h.insert("X-Accel-Buffering", HeaderValue::from_static("no"));
    };
    if req.method() == Method::HEAD {
        let mut res = Response::new(Body::empty());
        headers(&mut res);
        return res;
    }

    let args = audio_args(path, start, target);
    tracing::debug!(ffmpeg = %set.ffmpeg_path, args = ?args, "audio transcode");
    let mut cmd = tokio::process::Command::new(&set.ffmpeg_path);
    cmd.args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(ffmpeg = %set.ffmpeg_path, error = %e, "cannot start ffmpeg for audio");
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("cannot start ffmpeg: {e}") })))
                .into_response();
        }
    };
    let Some(stdout) = child.stdout.take() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "ffmpeg stdout unavailable" })))
            .into_response();
    };
    if let Some(stderr) = child.stderr.take() {
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if !line.trim().is_empty() {
                    tracing::debug!(file = %name, "ffmpeg audio: {}", line.trim());
                }
            }
        });
    }
    let stream = ChildStream { _child: child, inner: ReaderStream::with_capacity(stdout, 32 * 1024) };
    let mut res = Response::new(Body::from_stream(stream));
    headers(&mut res);
    res
}

/// Body stream that owns the ffmpeg child so dropping the response (client
/// disconnect) kills the encoder via `kill_on_drop`.
struct ChildStream {
    _child: tokio::process::Child,
    inner: ReaderStream<tokio::process::ChildStdout>,
}

impl Stream for ChildStream {
    type Item = std::io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[test]
    fn audio_arg_builder() {
        let p = Path::new("/music/a.flac");
        let a = audio_args(p, 0.0, AudioTarget::Aac);
        assert!(!a.contains(&"-ss".to_string()));
        let i = a.iter().position(|s| s == "-i").unwrap();
        assert_eq!(a[i + 1], "/music/a.flac");
        assert!(a.windows(2).any(|w| w[0] == "-c:a" && w[1] == "aac"));
        assert!(a.windows(2).any(|w| w[0] == "-f" && w[1] == "adts"));
        assert!(a.windows(2).any(|w| w[0] == "-b:a" && w[1] == "192k"));
        assert_eq!(a.last().unwrap(), "-");
        assert!(a.contains(&"-vn".to_string()));

        let a = audio_args(p, 12.3456, AudioTarget::Mp3);
        let s = a.iter().position(|s| s == "-ss").unwrap();
        assert_eq!(a[s + 1], "12.346");
        assert!(s < a.iter().position(|s| s == "-i").unwrap());
        assert!(a.windows(2).any(|w| w[0] == "-c:a" && w[1] == "libmp3lame"));
        assert!(a.windows(2).any(|w| w[0] == "-f" && w[1] == "mp3"));

        let a = audio_args(p, 1.0, AudioTarget::Opus);
        assert!(a.windows(2).any(|w| w[0] == "-c:a" && w[1] == "libopus"));
        assert!(a.windows(2).any(|w| w[0] == "-b:a" && w[1] == "128k"));
        assert!(a.windows(2).any(|w| w[0] == "-f" && w[1] == "ogg"));
    }

    #[test]
    fn targets_and_mimes() {
        assert_eq!(AudioTarget::parse("aac"), Some(AudioTarget::Aac));
        assert_eq!(AudioTarget::parse("flac"), None);
        assert_eq!(AudioTarget::Aac.mime(), "audio/aac");
        assert_eq!(AudioTarget::Mp3.mime(), "audio/mpeg");
        assert_eq!(AudioTarget::Opus.mime(), "audio/ogg");
        assert_eq!(audio_mime("flac", "flac", "flac"), "audio/flac");
        assert_eq!(audio_mime("mp3", "mp3", "mp3"), "audio/mpeg");
        assert_eq!(audio_mime("mp4", "aac", "m4a"), "audio/mp4");
        assert_eq!(audio_mime("ogg", "opus", "opus"), "audio/ogg");
        assert_eq!(audio_mime("ogg", "vorbis", "ogg"), "audio/ogg");
        assert_eq!(audio_mime("wav", "pcm", "wav"), "audio/wav");
        assert!(audio_mime("", "", "mid").starts_with("audio/"));
        assert_eq!(audio_mime("", "", ""), "application/octet-stream");
    }

    #[tokio::test]
    async fn serve_file_supports_ranges_and_404() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.bin");
        std::fs::write(&f, b"0123456789").unwrap();

        let res = serve_file(&f, "application/x-test", Request::new(Body::empty())).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers().get(header::CONTENT_TYPE).unwrap(), "application/x-test");
        assert_eq!(res.headers().get(header::ACCEPT_RANGES).unwrap(), "bytes");
        let body = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"0123456789");

        let req = Request::builder().header(header::RANGE, "bytes=2-4").body(Body::empty()).unwrap();
        let res = serve_file(&f, "application/x-test", req).await;
        assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(res.headers().get(header::CONTENT_RANGE).unwrap(), "bytes 2-4/10");
        let body = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"234");

        let res = serve_file(&dir.path().join("missing"), "text/plain", Request::new(Body::empty())).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // an unparsable mime falls back to octet-stream instead of panicking
        let res = serve_file(&f, "not a mime", Request::new(Body::empty())).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers().get(header::CONTENT_TYPE).unwrap(), "application/octet-stream");
    }

    #[tokio::test]
    async fn audio_rejects_bad_format_and_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.wav");
        std::fs::write(&f, b"RIFF").unwrap();
        let set = Settings::default();
        let info = MediaInfo { container: "wav".into(), acodec: Some("pcm".into()), ..Default::default() };
        let res = audio(&set, &f, &info, "wma", 0.0, Request::new(Body::empty())).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let res = audio(&set, &dir.path().join("nope.wav"), &info, "aac", 0.0, Request::new(Body::empty())).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        // explicit fmt equal to a non-transcodable source codec → direct
        let res = audio(&set, &f, &info, "pcm", 0.0, Request::new(Body::empty())).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers().get(header::CONTENT_TYPE).unwrap(), "audio/wav");
        // HEAD on a transcode: headers only, no ffmpeg
        let req = Request::builder().method(Method::HEAD).body(Body::empty()).unwrap();
        let res = audio(&set, &f, &info, "aac", 0.0, req).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers().get(header::CONTENT_TYPE).unwrap(), "audio/aac");
        assert_eq!(res.headers().get(header::ACCEPT_RANGES).unwrap(), "none");
    }

    /// Transcode a synthesized WAV to ADTS AAC and check the sync word
    /// (needs ffmpeg on PATH; skipped otherwise).
    #[tokio::test]
    async fn audio_transcodes_fixture() {
        let Some(ffmpeg) = which_ffmpeg() else { return };
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("tone.wav");
        let st = std::process::Command::new(&ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440", "-t", "2"])
            .arg(&src)
            .status()
            .unwrap();
        if !st.success() {
            return;
        }
        let set = Settings { ffmpeg_path: ffmpeg, ..Default::default() };
        let info =
            MediaInfo { container: "wav".into(), acodec: Some("pcm".into()), duration_sec: 2.0, ..Default::default() };
        let res = audio(&set, &src, &info, "aac", 0.5, Request::new(Body::empty())).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers().get(header::CONTENT_TYPE).unwrap(), "audio/aac");
        assert_eq!(res.headers().get(header::CACHE_CONTROL).unwrap(), "no-store");
        let body = tokio::time::timeout(std::time::Duration::from_secs(30), res.into_body().collect())
            .await
            .unwrap()
            .unwrap()
            .to_bytes();
        assert!(body.len() > 1000, "got {} bytes", body.len());
        assert_eq!(body[0], 0xFF);
        assert_eq!(body[1] & 0xF0, 0xF0, "ADTS sync word");

        let res = audio(&set, &src, &info, "mp3", 0.0, Request::new(Body::empty())).await;
        assert_eq!(res.headers().get(header::CONTENT_TYPE).unwrap(), "audio/mpeg");
        let body = tokio::time::timeout(std::time::Duration::from_secs(30), res.into_body().collect())
            .await
            .unwrap()
            .unwrap()
            .to_bytes();
        assert!(body.len() > 1000);
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
