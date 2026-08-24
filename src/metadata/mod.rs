// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! Metadata enrichment. Precedence: Kodi NFO sidecars (user-curated) > TMDB
//! (movies, shows, episodes, DVR recordings) > embedded music tags +
//! MusicBrainz/Cover Art Archive (albums). The enricher runs as a background
//! worker drained after every scan, rate-limited per provider, and can be
//! invoked on demand for one item.

pub mod musicbrainz;
pub mod nfo;
pub mod tags;
pub mod tmdb;

use crate::{
    db,
    media::art::Art,
    model::{Item, Kind, Metadata},
    state::SettingsCache,
    telemetry::Activity,
};
use chrono::Utc;
use serde_json::json;
use sqlx::PgPool;
use std::{path::Path, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

/// Idle wake-up period when nobody kicks the worker.
const IDLE_PERIOD: Duration = Duration::from_secs(600);
/// Rows pulled per pass for each queue.
const BATCH: i64 = 50;
/// Upper bound on passes per drain, so a persistent DB write failure cannot
/// spin the worker forever.
const MAX_PASSES: usize = 40;

pub struct Enricher {
    pub pool: PgPool,
    pub settings: Arc<SettingsCache>,
    pub http: reqwest::Client,
    pub art: Arc<Art>,
    pub activity: Activity,
    pub wake: tokio::sync::Notify,
}

/// Show metadata that still needs a provider lookup: no provider matched and
/// the last attempt (if any) is more than a day old. Without the age check
/// every episode of an unmatched show would trigger a fresh TMDB search.
fn show_meta_stale(m: &Metadata) -> bool {
    m.provider.is_none() && m.updated.map(|u| Utc::now() - u > chrono::Duration::days(1)).unwrap_or(true)
}

/// Outcome label for `ontele_metadata_lookups_total`.
fn count(provider: &'static str, result: &'static str) {
    metrics::counter!("ontele_metadata_lookups_total", "provider" => provider, "result" => result).increment(1);
}

impl Enricher {
    pub fn new(
        pool: PgPool,
        settings: Arc<SettingsCache>,
        http: reqwest::Client,
        art: Arc<Art>,
        activity: Activity,
    ) -> Self {
        Self { pool, settings, http, art, activity, wake: tokio::sync::Notify::new() }
    }

    /// Wake the worker (called by the scanner and the DVR after changes).
    pub fn kick(&self) {
        self.wake.notify_one();
    }

    /// TMDB client for the current settings, if the provider is enabled and a
    /// key is configured.
    fn tmdb(&self) -> Option<tmdb::Tmdb> {
        let s = self.settings.get();
        if !s.metadata_providers.tmdb || s.tmdb_api_key.trim().is_empty() {
            return None;
        }
        Some(tmdb::Tmdb::new(self.http.clone(), s.tmdb_api_key.trim().to_string(), s.metadata_language.clone()))
    }

    fn musicbrainz(&self) -> Option<musicbrainz::MusicBrainz> {
        if !self.settings.get().metadata_providers.musicbrainz {
            return None;
        }
        Some(musicbrainz::MusicBrainz::new(self.http.clone()))
    }

    fn nfo_enabled(&self) -> bool {
        self.settings.get().metadata_providers.nfo
    }

    /// Background worker: drain items/shows/albums needing metadata.
    pub async fn run_loop(self: Arc<Self>, cancel: CancellationToken) {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(IDLE_PERIOD) => {}
            }
            self.drain(&cancel).await;
        }
    }

    /// One full pass over everything that needs metadata. Per-entry errors
    /// are logged and skipped; returns early when `cancel` fires.
    pub async fn drain(&self, cancel: &CancellationToken) {
        let mut done = 0usize;
        for _ in 0..MAX_PASSES {
            if cancel.is_cancelled() {
                return;
            }
            let mut progressed = 0usize;

            match db::items::needing_meta(&self.pool, &["movie", "episode", "recording"], BATCH).await {
                Ok(items) => {
                    for it in items {
                        if cancel.is_cancelled() {
                            return;
                        }
                        match self.enrich_loaded(it).await {
                            Ok(_) => progressed += 1,
                            Err(e) => {
                                progressed += 1; // the row got its `updated` stamp (or is unreachable); move on
                                tracing::warn!(error = %e, "metadata: item enrichment failed");
                            }
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e, "metadata: needing_meta query failed"),
            }

            match db::items::shows_needing_meta(&self.pool, BATCH).await {
                Ok(shows) => {
                    for (show, _year) in shows {
                        if cancel.is_cancelled() {
                            return;
                        }
                        if let Err(e) = self.enrich_show(&show).await {
                            tracing::warn!(show, error = %e, "metadata: show enrichment failed");
                        }
                        progressed += 1;
                    }
                }
                Err(e) => tracing::warn!(error = %e, "metadata: shows_needing_meta query failed"),
            }

            match db::music::albums_needing_meta(&self.pool, BATCH).await {
                Ok(albums) => {
                    for a in albums {
                        if cancel.is_cancelled() {
                            return;
                        }
                        if let Err(e) = self.enrich_album(&a.id).await {
                            tracing::warn!(album = %a.id, error = %e, "metadata: album enrichment failed");
                        }
                        progressed += 1;
                    }
                }
                Err(e) => tracing::warn!(error = %e, "metadata: albums_needing_meta query failed"),
            }

            done += progressed;
            if progressed == 0 {
                break;
            }
        }
        if done > 0 {
            tracing::info!(entries = done, "metadata pass complete");
        }
    }

    /// Enrich one item now (movie/episode/recording/track). Writes `meta`
    /// and invalidates cached art. Returns the stored metadata.
    pub async fn enrich_item(&self, id: &str) -> anyhow::Result<Option<Metadata>> {
        let Some(item) = db::items::get(&self.pool, 0, id).await? else {
            return Ok(None);
        };
        self.enrich_loaded(item).await
    }

    async fn enrich_loaded(&self, item: Item) -> anyhow::Result<Option<Metadata>> {
        let id = item.id.clone();
        let kind = item.kind;
        // `Item.meta` collapses id-only metadata to `None`; read the raw column.
        let existing = match db::items::get_meta(&self.pool, &id).await? {
            Some(m) => m,
            None => item.meta.clone().unwrap_or_default(),
        };
        // A manual fix (`PUT /api/items/{id}/metadata`) leaves `{tmdbId}` behind.
        let preset_tmdb = if existing.provider.is_none() { existing.tmdb_id } else { None };

        let result: anyhow::Result<Metadata> = match kind {
            Some(Kind::Movie) => self.enrich_movie(&item, preset_tmdb).await,
            Some(Kind::Episode) => self.enrich_episode(&item).await,
            Some(Kind::Recording) => self.enrich_recording(&item, preset_tmdb).await,
            Some(Kind::Track) => {
                // Tracks carry tag metadata only; the album aggregate is what gets provider data.
                if let Some(album_id) = item.album_id.as_deref() {
                    return self.enrich_album(album_id).await;
                }
                return Ok(item.meta);
            }
            None => anyhow::bail!("item {id} has unknown kind"),
        };

        // Whatever happened, stamp `updated` so the queue does not retry for a day.
        let mut meta = match result {
            Ok(m) => m,
            Err(e) => {
                let mut m = existing.clone();
                m.updated = Some(Utc::now());
                if let Err(de) = db::items::set_meta(&self.pool, &id, &m).await {
                    tracing::warn!(error = %de, "metadata: stamping failed item");
                }
                return Err(e);
            }
        };
        meta.updated = Some(Utc::now());
        // keep a manual tmdb id around even when nothing matched, so a later retry still uses it
        if meta.tmdb_id.is_none() {
            meta.tmdb_id = preset_tmdb;
        }
        db::items::set_meta(&self.pool, &id, &meta).await?;
        self.art.invalidate(&id);
        if let Some(p) = meta.provider.clone() {
            self.activity.record(
                None,
                "metadata.enriched",
                Some(&id),
                json!({ "provider": p, "kind": kind.map(|k| k.as_str()) }),
            );
        }
        Ok(Some(meta))
    }

    /// NFO sidecar for the item's file, when the provider is enabled.
    fn nfo_for(&self, item: &Item) -> Option<nfo::NfoInfo> {
        if !self.nfo_enabled() {
            return None;
        }
        let path = item.path.as_deref()?;
        let p = nfo::find_nfo(Path::new(path))?;
        match nfo::read(&p) {
            Some(n) => {
                count("nfo", "hit");
                Some(n)
            }
            None => {
                count("nfo", "miss");
                None
            }
        }
    }

    async fn enrich_movie(&self, item: &Item, preset_tmdb: Option<i64>) -> anyhow::Result<Metadata> {
        let nfo = self.nfo_for(item).filter(|n| n.meta.provider.is_some());
        let title = nfo.as_ref().and_then(|n| n.title.clone()).unwrap_or_else(|| item.title.clone());
        let year = nfo.as_ref().and_then(|n| n.year).or(item.year);
        let tmdb_id = nfo.as_ref().and_then(|n| n.meta.tmdb_id).or(preset_tmdb);

        let mut meta = Metadata::default();
        if let Some(t) = self.tmdb() {
            let found = match tmdb_id {
                Some(id) => t.movie_by_id(id).await,
                None => t.movie(&title, year).await,
            };
            match found {
                Ok(Some(m)) => {
                    count("tmdb", "hit");
                    meta = m;
                }
                Ok(None) => count("tmdb", "miss"),
                Err(e) => {
                    count("tmdb", "error");
                    if nfo.is_none() {
                        return Err(e);
                    }
                    tracing::warn!(id = %item.id, error = %e, "tmdb lookup failed; using NFO only");
                }
            }
        }
        if let Some(n) = nfo {
            meta.merge_from(n.meta);
        }
        Ok(meta)
    }

    async fn enrich_episode(&self, item: &Item) -> anyhow::Result<Metadata> {
        let nfo = self.nfo_for(item).filter(|n| n.meta.provider.is_some());
        let mut meta = Metadata::default();

        if let (Some(show), Some(t)) = (item.show.as_deref().filter(|s| !s.trim().is_empty()), self.tmdb()) {
            // Show-level metadata first (gives us the TMDB show id).
            let mut show_meta = db::items::get_show_meta(&self.pool, show).await?;
            if show_meta.as_ref().map(show_meta_stale).unwrap_or(true) {
                show_meta = self.enrich_show(show).await?;
            }
            let show_tmdb = show_meta.as_ref().and_then(|m| m.tmdb_id);
            match (show_tmdb, item.season, item.episode) {
                (Some(sid), Some(season), Some(ep)) => match t.episode(sid, season, ep).await {
                    Ok(Some(m)) => {
                        count("tmdb", "hit");
                        meta = m;
                    }
                    Ok(None) => count("tmdb", "miss"),
                    Err(e) => {
                        count("tmdb", "error");
                        if nfo.is_none() {
                            return Err(e);
                        }
                        tracing::warn!(id = %item.id, error = %e, "tmdb episode lookup failed; using NFO only");
                    }
                },
                (Some(sid), _, _) => {
                    // Show matched but the file has no S/E numbering: still link it to the show.
                    meta.tmdb_id = Some(sid);
                }
                _ => {}
            }
        }
        if let Some(n) = nfo {
            let nfo_title = n.title.clone();
            meta.merge_from(n.meta);
            if meta.original_title.is_none() {
                meta.original_title = nfo_title;
            }
            // An episode NFO's <thumb> is the episode still, not a poster (the
            // poster/backdrop of an episode come from the show).
            if let Some(thumb) = meta.poster_url.take() {
                meta.still_url = Some(thumb);
            }
        }
        // An episode scanned without a name gets one from the provider.
        if item.title.trim().is_empty()
            && let Some(name) = meta.original_title.as_deref().map(str::trim).filter(|s| !s.is_empty())
        {
            db::items::set_title(&self.pool, &item.id, name).await?;
        }
        Ok(meta)
    }

    async fn enrich_recording(&self, item: &Item, preset_tmdb: Option<i64>) -> anyhow::Result<Metadata> {
        let Some(t) = self.tmdb() else {
            return Ok(Metadata::default());
        };
        let title = item.title.trim();
        if title.is_empty() {
            return Ok(Metadata::default());
        }
        if let Some(id) = preset_tmdb {
            // A manual fix could point at either a movie or a show; try both.
            if let Some(m) = t.movie_by_id(id).await? {
                count("tmdb", "hit");
                return Ok(m);
            }
            if let Some(m) = t.show_by_id(id).await? {
                count("tmdb", "hit");
                return Ok(m);
            }
        }
        let year = item
            .year
            .or_else(|| item.start.map(|s| s.format("%Y").to_string().parse().unwrap_or(0)).filter(|y| *y > 0));
        // Broadcast programmes are mostly series; movies are the fallback.
        match t.show(title, None).await {
            Ok(Some(mut show)) => {
                count("tmdb", "hit");
                if let (Some(sid), Some(season), Some(ep)) = (show.tmdb_id, item.season, item.episode)
                    && let Ok(Some(e)) = t.episode(sid, season, ep).await
                {
                    show.still_url = e.still_url;
                    if e.overview.is_some() {
                        show.overview = e.overview;
                    }
                    show.original_title = e.original_title;
                    show.release_date = e.release_date.or(show.release_date);
                }
                return Ok(show);
            }
            Ok(None) => count("tmdb", "miss"),
            Err(e) => {
                count("tmdb", "error");
                return Err(e);
            }
        }
        match t.movie(title, year).await {
            Ok(Some(m)) => {
                count("tmdb", "hit");
                Ok(m)
            }
            Ok(None) => {
                count("tmdb", "miss");
                Ok(Metadata::default())
            }
            Err(e) => {
                count("tmdb", "error");
                Err(e)
            }
        }
    }

    /// Show-level metadata (poster/backdrop/overview) for a show name.
    pub async fn enrich_show(&self, show: &str) -> anyhow::Result<Option<Metadata>> {
        let show = show.trim();
        if show.is_empty() {
            return Ok(None);
        }
        let episodes = db::items::show_episodes(&self.pool, 0, show).await?;
        let year = episodes.iter().filter_map(|e| e.year).min();

        let mut nfo: Option<nfo::NfoInfo> = None;
        if self.nfo_enabled() {
            let found =
                episodes.iter().filter_map(|e| e.path.as_deref()).find_map(|p| nfo::find_tvshow_nfo(Path::new(p)));
            if let Some(p) = found {
                let text = std::fs::read(&p).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
                let parsed =
                    if nfo::detect_root(&text) == Some(nfo::NfoRoot::TvShow) { nfo::parse(&text) } else { None };
                match parsed {
                    Some(n) => {
                        count("nfo", "hit");
                        nfo = Some(n);
                    }
                    None => count("nfo", "miss"),
                }
            }
        }

        let name = nfo.as_ref().and_then(|n| n.title.clone()).unwrap_or_else(|| show.to_string());
        let year = nfo.as_ref().and_then(|n| n.year).or(year);
        let tmdb_id = nfo.as_ref().and_then(|n| n.meta.tmdb_id);

        let mut meta = Metadata::default();
        let mut provider_err: Option<anyhow::Error> = None;
        if let Some(t) = self.tmdb() {
            let found = match tmdb_id {
                Some(id) => t.show_by_id(id).await,
                None => t.show(&name, year).await,
            };
            match found {
                Ok(Some(m)) => {
                    count("tmdb", "hit");
                    meta = m;
                }
                Ok(None) => count("tmdb", "miss"),
                Err(e) => {
                    count("tmdb", "error");
                    provider_err = Some(e);
                }
            }
        }
        if let Some(n) = nfo {
            meta.merge_from(n.meta);
        }
        if let Some(e) = provider_err
            && meta.provider.is_none()
        {
            // Stamp so the queue does not hammer a failing provider; surface the error.
            meta.updated = Some(Utc::now());
            db::items::set_show_meta(&self.pool, show, &meta).await?;
            return Err(e);
        }
        meta.updated = Some(Utc::now());
        db::items::set_show_meta(&self.pool, show, &meta).await?;
        self.art.invalidate(&format!("show:{show}"));
        if let Some(p) = meta.provider.clone() {
            self.activity.record(
                None,
                "metadata.enriched",
                None,
                json!({ "provider": p, "kind": "show", "show": show }),
            );
        }
        Ok(Some(meta))
    }

    /// Album metadata via MusicBrainz + Cover Art Archive.
    pub async fn enrich_album(&self, album_id: &str) -> anyhow::Result<Option<Metadata>> {
        let Some(album) = db::music::album(&self.pool, album_id).await? else {
            return Ok(None);
        };
        let tracks = db::music::album_tracks(&self.pool, 0, album_id).await?;
        let mut mbid = tracks.iter().find_map(|t| t.meta.as_ref().and_then(|m| m.mbid.clone()));
        if mbid.is_none() {
            // id-only track metadata is collapsed by `Item`; check the raw column of the first few tracks
            for t in tracks.iter().take(3) {
                if let Some(m) = db::items::get_meta(&self.pool, &t.id).await?.and_then(|m| m.mbid) {
                    mbid = Some(m);
                    break;
                }
            }
        }

        let mut meta = Metadata::default();
        if let Some(mb) = self.musicbrainz() {
            match mb.release(&album.artist, &album.title, mbid.as_deref()).await {
                Ok(Some(m)) => {
                    count("musicbrainz", "hit");
                    meta = m;
                }
                Ok(None) => count("musicbrainz", "miss"),
                Err(e) => {
                    count("musicbrainz", "error");
                    meta.updated = Some(Utc::now());
                    db::music::set_album_meta(&self.pool, album_id, &album.artist, &album.title, album.year, &meta)
                        .await?;
                    return Err(e);
                }
            }
        }
        meta.updated = Some(Utc::now());
        let year =
            album.year.or_else(|| meta.release_date.as_deref().and_then(|d| d.get(0..4)).and_then(|y| y.parse().ok()));
        db::music::set_album_meta(&self.pool, album_id, &album.artist, &album.title, year, &meta).await?;
        self.art.invalidate(&format!("album:{album_id}"));
        if let Some(p) = meta.provider.clone() {
            self.activity.record(
                None,
                "metadata.enriched",
                Some(&album.art_id),
                json!({ "provider": p, "kind": "album", "album": album.title, "artist": album.artist }),
            );
        }
        Ok(Some(meta))
    }
}
