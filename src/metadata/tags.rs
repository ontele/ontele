// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! Music tags via `lofty` (ID3v1/v2, Vorbis comments, FLAC, MP4/iTunes, APE,
//! WavPack, AIFF, WAV, DSF…). Also yields duration/bitrate so untagged
//! audio never needs ffprobe.

use anyhow::Context;
use lofty::{
    config::ParseOptions,
    file::{AudioFile, FileType, TaggedFile, TaggedFileExt},
    tag::{ItemKey, Tag},
};
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub track_no: Option<i32>,
    pub disc_no: Option<i32>,
    pub year: Option<i32>,
    pub genre: Option<String>,
    pub duration_sec: f64,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
    pub codec: Option<String>,
    pub mb_release_id: Option<String>,
    pub mb_artist_id: Option<String>,
    pub has_picture: bool,
}

/// First non-empty string for `key` across the tag.
fn text(tag: &Tag, key: ItemKey) -> Option<String> {
    tag.get_strings(key).map(str::trim).find(|s| !s.is_empty()).map(str::to_string)
}

/// Leading integer of a value such as `3`, `3/12`, `03`, ` 7 `.
fn leading_int(s: &str) -> Option<i32> {
    let s = s.trim();
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<i64>().ok().filter(|n| *n >= 0 && *n <= i32::MAX as i64).map(|n| n as i32)
}

/// First four digits of a date-ish value (`2019`, `2019-05-01`, `2019.05`).
fn year_of(s: &str) -> Option<i32> {
    let s = s.trim();
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() >= 4 { digits[..4].parse::<i32>().ok().filter(|y| (1000..=9999).contains(y)) } else { None }
}

fn year_from_tag(tag: &Tag) -> Option<i32> {
    text(tag, ItemKey::Year)
        .and_then(|y| year_of(&y))
        .or_else(|| text(tag, ItemKey::RecordingDate).and_then(|d| year_of(&d)))
        .or_else(|| text(tag, ItemKey::OriginalReleaseDate).and_then(|d| year_of(&d)))
        .or_else(|| text(tag, ItemKey::ReleaseDate).and_then(|d| year_of(&d)))
}

/// Short codec name for a lofty file type. MP4 needs a second look because
/// the generic properties don't say whether it's AAC or ALAC.
fn codec_of(tagged: &TaggedFile, path: &Path) -> String {
    match tagged.file_type() {
        FileType::Flac => "flac".into(),
        FileType::Mpeg => "mp3".into(),
        FileType::Mp4 => mp4_codec(path).unwrap_or_else(|| "aac".into()),
        FileType::Vorbis => "vorbis".into(),
        FileType::Opus => "opus".into(),
        FileType::Wav => "pcm".into(),
        FileType::Aiff => "pcm".into(),
        FileType::Ape => "ape".into(),
        FileType::WavPack => "wavpack".into(),
        FileType::Speex => "speex".into(),
        FileType::Mpc => "mpc".into(),
        FileType::Aac => "aac".into(),
        other => format!("{other:?}").to_ascii_lowercase(),
    }
}

fn mp4_codec(path: &Path) -> Option<String> {
    use lofty::mp4::{Mp4Codec, Mp4File};
    let mut f = std::fs::File::open(path).ok()?;
    let mp4 = Mp4File::read_from(&mut f, ParseOptions::new().read_tags(false)).ok()?;
    Some(
        match mp4.properties().codec()? {
            Mp4Codec::AAC => "aac",
            Mp4Codec::ALAC => "alac",
            Mp4Codec::MP3 => "mp3",
            Mp4Codec::FLAC => "flac",
            _ => "aac",
        }
        .to_string(),
    )
}

/// Read tags + properties. Errors only on unreadable files; untagged files
/// return a mostly-empty struct with duration filled.
pub fn read(path: &Path) -> anyhow::Result<TrackTags> {
    let tagged = lofty::read_from_path(path).with_context(|| format!("read tags from {}", path.display()))?;
    let props = tagged.properties();
    let mut out = TrackTags {
        duration_sec: props.duration().as_secs_f64(),
        bitrate_kbps: props.audio_bitrate().or_else(|| props.overall_bitrate()).filter(|b| *b > 0),
        sample_rate: props.sample_rate().filter(|r| *r > 0),
        channels: props.channels().map(u32::from).filter(|c| *c > 0),
        codec: Some(codec_of(&tagged, path)),
        has_picture: tagged.tags().iter().any(|t| !t.pictures().is_empty()),
        ..Default::default()
    };

    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        out.title = text(tag, ItemKey::TrackTitle);
        out.artist = text(tag, ItemKey::TrackArtist);
        out.album_artist = text(tag, ItemKey::AlbumArtist);
        out.album = text(tag, ItemKey::AlbumTitle);
        out.track_no = text(tag, ItemKey::TrackNumber).and_then(|s| leading_int(&s)).filter(|n| *n > 0);
        out.disc_no = text(tag, ItemKey::DiscNumber).and_then(|s| leading_int(&s)).filter(|n| *n > 0);
        out.year = year_from_tag(tag);
        out.genre = text(tag, ItemKey::Genre);
        out.mb_release_id = text(tag, ItemKey::MusicBrainzReleaseId);
        out.mb_artist_id = text(tag, ItemKey::MusicBrainzArtistId);

        // Some taggers leave the fields in a secondary tag only (ID3v1 next
        // to an empty ID3v2, say): fill gaps from the other tags.
        for other in tagged.tags().iter().filter(|t| !std::ptr::eq(*t, tag)) {
            if out.title.is_none() {
                out.title = text(other, ItemKey::TrackTitle);
            }
            if out.artist.is_none() {
                out.artist = text(other, ItemKey::TrackArtist);
            }
            if out.album_artist.is_none() {
                out.album_artist = text(other, ItemKey::AlbumArtist);
            }
            if out.album.is_none() {
                out.album = text(other, ItemKey::AlbumTitle);
            }
            if out.track_no.is_none() {
                out.track_no = text(other, ItemKey::TrackNumber).and_then(|s| leading_int(&s)).filter(|n| *n > 0);
            }
            if out.disc_no.is_none() {
                out.disc_no = text(other, ItemKey::DiscNumber).and_then(|s| leading_int(&s)).filter(|n| *n > 0);
            }
            if out.year.is_none() {
                out.year = year_from_tag(other);
            }
            if out.genre.is_none() {
                out.genre = text(other, ItemKey::Genre);
            }
        }
    }
    Ok(out)
}

/// First embedded picture as (mime, bytes).
pub fn picture(path: &Path) -> Option<(String, Vec<u8>)> {
    let tagged = lofty::read_from_path(path).ok()?;
    let pic = tagged
        .primary_tag()
        .into_iter()
        .chain(tagged.tags().iter())
        .flat_map(|t| t.pictures().iter())
        .find(|p| !p.data().is_empty())?;
    let data = pic.data().to_vec();
    let mime = pic
        .mime_type()
        .map(|m| m.as_str().to_string())
        .filter(|m| !m.is_empty() && m.contains('/'))
        .unwrap_or_else(|| sniff_mime(&data).to_string());
    Some((mime, data))
}

fn sniff_mime(data: &[u8]) -> &'static str {
    if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if data.starts_with(b"GIF8") {
        "image/gif"
    } else if data.starts_with(b"RIFF") && data.len() > 12 && &data[8..12] == b"WEBP" {
        "image/webp"
    } else if data.starts_with(b"BM") {
        "image/bmp"
    } else {
        "image/jpeg"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn which(bin: &str) -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path).map(|p| p.join(bin)).find(|p| p.is_file())
    }

    fn synth(dir: &Path, name: &str, extra: &[&str]) -> std::path::PathBuf {
        let out = dir.join(name);
        let st = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100", "-t", "3"])
            .args([
                "-metadata",
                "title=Around the World",
                "-metadata",
                "artist=Daft Punk",
                "-metadata",
                "album_artist=Daft Punk",
                "-metadata",
                "album=Homework",
                "-metadata",
                "track=3/16",
                "-metadata",
                "disc=1",
                "-metadata",
                "date=1997",
                "-metadata",
                "genre=House",
            ])
            .args(extra)
            .arg("-y")
            .arg(&out)
            .status()
            .expect("spawn ffmpeg");
        assert!(st.success(), "ffmpeg failed for {name}");
        out
    }

    #[test]
    fn parsers() {
        assert_eq!(leading_int("3/16"), Some(3));
        assert_eq!(leading_int(" 07 "), Some(7));
        assert_eq!(leading_int("x"), None);
        assert_eq!(year_of("1997"), Some(1997));
        assert_eq!(year_of("2019-05-01"), Some(2019));
        assert_eq!(year_of("97"), None);
        assert_eq!(sniff_mime(&[0x89, b'P', b'N', b'G', 0, 0]), "image/png");
        assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), "image/jpeg");
    }

    #[test]
    fn unreadable_file_is_an_error() {
        assert!(read(Path::new("/nonexistent/dir/song.flac")).is_err());
        assert!(picture(Path::new("/nonexistent/dir/song.flac")).is_none());
        let dir = tempfile::tempdir().unwrap();
        let junk = dir.path().join("junk.mp3");
        std::fs::write(&junk, b"definitely not audio").unwrap();
        assert!(read(&junk).is_err());
    }

    #[test]
    fn reads_flac_and_mp3_tags() {
        if which("ffmpeg").is_none() {
            eprintln!("ffmpeg not on PATH; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let flac = synth(dir.path(), "03 - Around the World.flac", &["-c:a", "flac"]);
        let t = read(&flac).unwrap();
        assert_eq!(t.title.as_deref(), Some("Around the World"));
        assert_eq!(t.artist.as_deref(), Some("Daft Punk"));
        assert_eq!(t.album_artist.as_deref(), Some("Daft Punk"));
        assert_eq!(t.album.as_deref(), Some("Homework"));
        assert_eq!(t.track_no, Some(3));
        assert_eq!(t.disc_no, Some(1));
        assert_eq!(t.year, Some(1997));
        assert_eq!(t.genre.as_deref(), Some("House"));
        assert_eq!(t.codec.as_deref(), Some("flac"));
        assert_eq!(t.sample_rate, Some(44100));
        assert_eq!(t.channels, Some(1));
        assert!((t.duration_sec - 3.0).abs() < 0.2, "{}", t.duration_sec);
        assert!(!t.has_picture);
        assert!(picture(&flac).is_none());

        let mp3 = synth(
            dir.path(),
            "03 - Around the World.mp3",
            &["-c:a", "libmp3lame", "-b:a", "128k", "-id3v2_version", "3"],
        );
        let t = read(&mp3).unwrap();
        assert_eq!(t.title.as_deref(), Some("Around the World"));
        assert_eq!(t.album.as_deref(), Some("Homework"));
        assert_eq!(t.track_no, Some(3));
        assert_eq!(t.year, Some(1997));
        assert_eq!(t.codec.as_deref(), Some("mp3"));
        assert!(t.bitrate_kbps.is_some_and(|b| (100..=160).contains(&b)), "{:?}", t.bitrate_kbps);
        assert!((t.duration_sec - 3.0).abs() < 0.3, "{}", t.duration_sec);

        let m4a = synth(dir.path(), "03 - Around the World.m4a", &["-c:a", "aac"]);
        let t = read(&m4a).unwrap();
        assert_eq!(t.codec.as_deref(), Some("aac"));
        assert_eq!(t.title.as_deref(), Some("Around the World"));
        let alac = synth(dir.path(), "03 - Around the World (lossless).m4a", &["-c:a", "alac"]);
        let t = read(&alac).unwrap();
        assert_eq!(t.codec.as_deref(), Some("alac"));

        // untagged: duration still filled, fields empty
        let out = dir.path().join("plain.flac");
        let st = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440",
                "-t",
                "2",
                "-map_metadata",
                "-1",
                "-c:a",
                "flac",
                "-y",
            ])
            .arg(&out)
            .status()
            .unwrap();
        assert!(st.success());
        let t = read(&out).unwrap();
        assert_eq!(t.title, None);
        assert!(t.duration_sec > 1.5);
    }

    #[test]
    fn embedded_picture_roundtrip() {
        if which("ffmpeg").is_none() {
            eprintln!("ffmpeg not on PATH; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let cover = dir.path().join("cover.png");
        let st = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i", "color=c=red:size=16x16", "-frames:v", "1", "-y"])
            .arg(&cover)
            .status()
            .unwrap();
        assert!(st.success());
        let mp3 = dir.path().join("art.mp3");
        let st = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i", "sine=frequency=440", "-i"])
            .arg(&cover)
            .args([
                "-t",
                "2",
                "-map",
                "0:a",
                "-map",
                "1:v",
                "-c:a",
                "libmp3lame",
                "-c:v",
                "png",
                "-id3v2_version",
                "3",
                "-metadata:s:v",
                "title=Album cover",
                "-metadata:s:v",
                "comment=Cover (front)",
                "-y",
            ])
            .arg(&mp3)
            .status()
            .unwrap();
        assert!(st.success());
        let t = read(&mp3).unwrap();
        assert!(t.has_picture);
        let (mime, bytes) = picture(&mp3).expect("picture");
        assert_eq!(mime, "image/png");
        assert!(bytes.starts_with(&[0x89, b'P', b'N', b'G']));
    }
}
