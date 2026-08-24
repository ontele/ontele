// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! End-to-end over real media: ffmpeg synthesizes a tiny library, the
//! scanner indexes it, and the HTTP API serves artwork, direct play, HLS
//! transcodes, sprites and subtitles. Skips itself when ffmpeg is absent.

mod common;

use axum::http::{Method, StatusCode};
use common::app_with_pool;
use serde_json::json;
use sqlx::PgPool;
use std::{path::Path, process::Command};

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg").arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn ff(args: &[&str]) {
    let st = Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().expect("ffmpeg");
    assert!(st.success(), "ffmpeg {args:?}");
}

/// movies/Blade Circuit (2023)/Blade Circuit (2023).mp4 (h264/aac, 12 s)
/// tv/Static Signal/Season 01/Static Signal S01E03 Cold Boot.mkv (h264/aac + srt)
/// music/Daft Punk/Discovery/01 - One More Time.flac (tagged)
fn make_library(root: &Path) {
    let movie_dir = root.join("movies/Blade Circuit (2023)");
    let tv_dir = root.join("tv/Static Signal/Season 01");
    let music_dir = root.join("music/Daft Punk/Discovery");
    for d in [&movie_dir, &tv_dir, &music_dir] {
        std::fs::create_dir_all(d).unwrap();
    }
    ff(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=640x360:rate=25",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000",
        "-t",
        "12",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        "-movflags",
        "+faststart",
        movie_dir.join("Blade Circuit (2023).mp4").to_str().unwrap(),
    ]);
    std::fs::write(
        tv_dir.join("sub.srt"),
        "1\n00:00:01,000 --> 00:00:03,000\nHello from the episode\n\n2\n00:00:04,000 --> 00:00:06,000\nSecond cue\n",
    )
    .unwrap();
    ff(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=640x360:rate=25",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=330:sample_rate=48000",
        "-i",
        tv_dir.join("sub.srt").to_str().unwrap(),
        "-t",
        "8",
        "-map",
        "0:v",
        "-map",
        "1:a",
        "-map",
        "2:s",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        "-c:s",
        "srt",
        "-metadata:s:s:0",
        "language=eng",
        tv_dir.join("Static Signal S01E03 Cold Boot.mkv").to_str().unwrap(),
    ]);
    std::fs::remove_file(tv_dir.join("sub.srt")).unwrap();
    // a second movie so the movies root never becomes empty (an empty root is treated as unmounted)
    std::fs::copy(movie_dir.join("Blade Circuit (2023).mp4"), root.join("movies/Static Signal (2021).mp4")).unwrap();
    ff(&[
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=220:sample_rate=44100",
        "-t",
        "5",
        "-c:a",
        "flac",
        "-metadata",
        "title=One More Time",
        "-metadata",
        "artist=Daft Punk",
        "-metadata",
        "album_artist=Daft Punk",
        "-metadata",
        "album=Discovery",
        "-metadata",
        "track=1",
        "-metadata",
        "date=2001",
        "-metadata",
        "genre=House",
        music_dir.join("01 - One More Time.flac").to_str().unwrap(),
    ]);
    // pad every file over the scanner's minimum size thresholds (1 MiB video / 64 KiB audio)
    for (p, min) in [
        (movie_dir.join("Blade Circuit (2023).mp4"), 1_100_000u64),
        (tv_dir.join("Static Signal S01E03 Cold Boot.mkv"), 1_100_000),
        (music_dir.join("01 - One More Time.flac"), 70_000),
    ] {
        let len = std::fs::metadata(&p).unwrap().len();
        if len < min {
            // MP4 with faststart tolerates trailing junk; MKV/FLAC ignore trailing bytes as well
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
            f.write_all(&vec![0u8; (min - len) as usize]).unwrap();
        }
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn scan_probe_art_and_playback(pool: PgPool) {
    if !have_ffmpeg() {
        eprintln!("ffmpeg not found; skipping");
        return;
    }
    let t = app_with_pool(pool.clone(), "none").await;
    let lib = tempfile::tempdir().unwrap();
    make_library(lib.path());

    // point settings at the library and scan synchronously
    let mut s = t.state.settings.get().as_ref().clone();
    s.media_dirs = vec![
        lib.path().join("movies").to_string_lossy().to_string(),
        lib.path().join("tv").to_string_lossy().to_string(),
    ];
    s.music_dirs = vec![lib.path().join("music").to_string_lossy().to_string()];
    s.metadata_providers.tmdb = false;
    s.metadata_providers.musicbrainz = false;
    t.state.settings.set(s).await.unwrap();
    let status = t.state.scanner.scan().await.unwrap();
    assert_eq!(status.found, 4, "{status:?}");
    assert_eq!(status.added, 4, "{status:?}");

    // ---- classification + probe ----
    let movies = t.get("/api/movies", None).await.json();
    assert_eq!(movies.as_array().unwrap().len(), 2);
    let m = &movies[0]; // title sort: Blade Circuit first
    assert_eq!(m["title"], "Blade Circuit");
    assert_eq!(m["year"], 2023);
    assert_eq!(m["vcodec"], "h264");
    assert_eq!(m["acodec"], "aac");
    assert_eq!(m["container"], "mp4");
    assert_eq!(m["height"], 360);
    assert!((m["duration"].as_f64().unwrap() - 12.0).abs() < 1.0, "{}", m["duration"]);
    let movie_id = m["id"].as_str().unwrap().to_string();

    let shows = t.get("/api/shows", None).await.json();
    assert_eq!(shows[0]["show"], "Static Signal");
    let show = t.get("/api/shows/Static%20Signal", None).await.json();
    let ep = &show["seasons"][0]["episodes"][0];
    assert_eq!((ep["season"].as_i64(), ep["episode"].as_i64()), (Some(1), Some(3)));
    assert_eq!(ep["title"], "Cold Boot");
    let ep_id = ep["id"].as_str().unwrap().to_string();
    let ep_detail = t.get(&format!("/api/items/{ep_id}"), None).await.json();
    assert_eq!(ep_detail["info"]["subtitles"][0]["codec"], "subrip");
    assert_eq!(ep_detail["info"]["subtitles"][0]["text"], true);
    assert_eq!(ep_detail["info"]["audio"][0]["codec"], "aac");

    let albums = t.get("/api/music/albums", None).await.json();
    assert_eq!(albums[0]["title"], "Discovery");
    assert_eq!(albums[0]["artist"], "Daft Punk");
    assert_eq!(albums[0]["year"], 2001);
    let album_id = albums[0]["id"].as_str().unwrap().to_string();
    let album = t.get(&format!("/api/music/albums/{album_id}"), None).await.json();
    assert_eq!(album["tracks"][0]["title"], "One More Time");
    assert_eq!(album["tracks"][0]["trackNo"], 1);
    assert_eq!(album["tracks"][0]["genre"], "House");
    let track_id = album["tracks"][0]["id"].as_str().unwrap().to_string();

    // ---- artwork: frame grab for the movie, placeholder for the track ----
    let poster = t.get(&format!("/api/img/{movie_id}?type=poster"), None).await;
    assert_eq!(poster.status, StatusCode::OK, "{}", poster.text());
    assert_eq!(poster.headers.get("content-type").unwrap(), "image/jpeg");
    assert!(poster.body.starts_with(&[0xFF, 0xD8]), "jpeg magic");
    let backdrop = t.get(&format!("/api/img/{movie_id}?type=backdrop&w=320"), None).await;
    assert_eq!(backdrop.status, StatusCode::OK);
    let show_poster = t.get("/api/img/show:Static%20Signal?type=poster", None).await;
    assert_eq!(show_poster.status, StatusCode::OK, "show art falls back to an episode frame");
    let album_art = t.get(&format!("/api/img/album:{album_id}?type=poster"), None).await;
    assert_eq!(album_art.status, StatusCode::NOT_FOUND, "no cover anywhere → 404 so the UI shows its text fallback");
    let _ = track_id;

    // ---- direct play honours Range ----
    let r = t.get(&format!("/stream/direct/{movie_id}"), None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers.get("content-type").unwrap(), "video/mp4");
    assert!(r.headers.get("accept-ranges").is_some());
    let req = axum::http::Request::builder()
        .uri(format!("/stream/direct/{movie_id}"))
        .header("range", "bytes=0-99")
        .body(axum::body::Body::empty())
        .unwrap();
    let res = tower::ServiceExt::oneshot(t.app.clone(), req).await.unwrap();
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    let body = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
    assert_eq!(body.len(), 100);

    // ---- playback decision: h264/aac/mp4 + capable client → direct ----
    let caps = json!({ "video": ["h264"], "audio": ["aac", "mp3"], "containers": ["mp4"], "hls": "mse", "maxHeight": 2160, "surround": false });
    let direct = t.post("/api/stream/start", None, json!({ "id": movie_id, "quality": "auto", "caps": caps })).await;
    assert_eq!(direct.status, StatusCode::OK, "{}", direct.text());
    assert_eq!(direct.json()["mode"], "direct");
    assert_eq!(direct.json()["url"], format!("/stream/direct/{movie_id}"));

    // mkv episode with a client that cannot play mkv → remux (copy) into HLS
    let copy = t.post("/api/stream/start", None, json!({ "id": ep_id, "quality": "auto", "caps": caps })).await;
    assert_eq!(copy.status, StatusCode::OK, "{}", copy.text());
    let cj = copy.json();
    assert_eq!(cj["mode"], "copy", "{cj}");
    let sid = cj["sessionId"].as_str().unwrap().to_string();
    let playlist = t.get(cj["url"].as_str().unwrap(), None).await;
    assert_eq!(playlist.status, StatusCode::OK);
    assert!(playlist.text().starts_with("#EXTM3U"), "{}", playlist.text());
    assert!(playlist.headers.get("content-type").unwrap().to_str().unwrap().contains("mpegurl"));
    let seg = playlist
        .text()
        .lines()
        .find(|l| l.ends_with(".ts") || l.ends_with(".m4s"))
        .expect("a segment listed")
        .to_string();
    let segr = t.get(&format!("/stream/hls/{sid}/{seg}"), None).await;
    assert_eq!(segr.status, StatusCode::OK);
    assert!(segr.body.len() > 1000);
    // traversal inside a single segment is rejected; a literal ../ never reaches the route
    assert_eq!(t.get(&format!("/stream/hls/{sid}/..%2F..%2Fetc%2Fpasswd"), None).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(t.get(&format!("/stream/hls/{sid}/%2e%2e%2fsecret.ts"), None).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(t.get(&format!("/stream/hls/{sid}/nope.ts"), None).await.status, StatusCode::NOT_FOUND);
    assert_eq!(t.post(&format!("/api/stream/{sid}/keepalive"), None, json!({})).await.status, StatusCode::OK);
    assert_eq!(t.delete(&format!("/api/stream/{sid}"), None).await.status, StatusCode::OK);
    assert_eq!(t.get(&format!("/stream/hls/{sid}/index.m3u8"), None).await.status, StatusCode::GONE);

    // explicit 240p transcode (below the 360p source) with a seek offset
    let tr = t
        .post("/api/stream/start", None, json!({ "id": movie_id, "quality": "240", "start": 5.0, "caps": caps }))
        .await;
    assert_eq!(tr.status, StatusCode::OK, "{}", tr.text());
    let tj = tr.json();
    assert_eq!(tj["mode"], "transcode", "{tj}");
    assert_eq!(tj["plan"]["height"], 240);
    assert_eq!(tj["offset"], 5.0);
    assert_eq!(t.get(tj["url"].as_str().unwrap(), None).await.status, StatusCode::OK);
    let (sessions, transcodes) = t.state.streams.active();
    assert_eq!((sessions, transcodes), (1, 1));
    t.delete(&format!("/api/stream/{}", tj["sessionId"].as_str().unwrap()), None).await;

    // ---- subtitles → WebVTT, sprites → VTT + JPEG ----
    let subs = t.get(&format!("/api/items/{ep_id}/subtitles"), None).await.json();
    assert_eq!(subs[0]["lang"], "eng");
    let vtt = t.get(subs[0]["url"].as_str().unwrap(), None).await;
    assert_eq!(vtt.status, StatusCode::OK, "{}", vtt.text());
    assert!(vtt.text().starts_with("WEBVTT"), "{}", vtt.text());
    assert!(vtt.text().contains("Hello from the episode"));
    let sv = t.get(&format!("/api/items/{movie_id}/sprites.vtt"), None).await;
    // clips shorter than 30 s have no sprites by design
    assert!(sv.status == StatusCode::NOT_FOUND || sv.text().starts_with("WEBVTT"));

    // ---- audio streaming: flac is browser-playable → direct ----
    let au = t.req(Method::GET, &format!("/stream/audio/{track_id}"), None, None).await;
    assert_eq!(au.status, StatusCode::OK);
    assert!(au.headers.get("content-type").unwrap().to_str().unwrap().contains("flac"));
    let au2 = t.req(Method::GET, &format!("/stream/audio/{track_id}?fmt=aac"), None, None).await;
    assert_eq!(au2.status, StatusCode::OK, "transcoded aac stream");
    assert!(au2.headers.get("content-type").unwrap().to_str().unwrap().contains("aac"));
    assert!(au2.body.len() > 500);

    // ---- rescan is a no-op, removal prunes ----
    let again = t.state.scanner.scan().await.unwrap();
    assert_eq!((again.added, again.updated, again.removed), (0, 0, 0), "{again:?}");
    std::fs::remove_file(lib.path().join("movies/Blade Circuit (2023)/Blade Circuit (2023).mp4")).unwrap();
    let pruned = t.state.scanner.scan().await.unwrap();
    assert_eq!(pruned.removed, 1);
    assert_eq!(t.get("/api/movies", None).await.json().as_array().unwrap().len(), 1);

    // removing the movies library from settings prunes its remaining rows
    let mut s2 = t.state.settings.get().as_ref().clone();
    s2.media_dirs.retain(|d| !d.ends_with("/movies"));
    t.state.settings.set(s2).await.unwrap();
    let pruned2 = t.state.scanner.scan().await.unwrap();
    assert_eq!(pruned2.removed, 1, "{pruned2:?}");
    assert!(t.get("/api/movies", None).await.json().as_array().unwrap().is_empty());
    assert_eq!(t.get("/api/shows", None).await.json().as_array().unwrap().len(), 1, "tv library untouched");
}
