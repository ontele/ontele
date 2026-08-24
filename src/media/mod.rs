// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! Library scanning: walk the configured folders, detect new/changed/removed
//! files cheaply (size + mtime vs the DB index), probe what changed with
//! ffprobe (videos) or lofty (music), classify via [`crate::naming`], and
//! upsert. Concurrent probes are bounded by a semaphore; scans serialize.

pub mod art;
pub mod playback;
pub mod probe;

use crate::{
    db::{
        self,
        items::{NewItem, ScanEntry},
    },
    metadata::{nfo, tags},
    model::{self, Kind, MediaInfo, ScanStatus, Settings},
    naming,
    state::SettingsCache,
    telemetry::Activity,
};
use chrono::{DateTime, Utc};
use futures::{StreamExt, stream};
use parking_lot::RwLock;
use serde_json::json;
use sqlx::PgPool;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

/// Videos smaller than this are menus, samples or junk.
const MIN_VIDEO_BYTES: u64 = 1024 * 1024;
/// Audio smaller than this is a click track or a broken download.
const MIN_AUDIO_BYTES: u64 = 64 * 1024;
/// Filesystem-watcher debounce window.
const WATCH_DEBOUNCE: Duration = Duration::from_secs(2);
/// Folders that never hold library content.
const SKIP_DIRS: &[&str] = &["@eadir", "#recycle", "lost+found", "$recycle.bin", "system volume information", ".trash"];
/// Folders holding previews rather than the feature.
const SAMPLE_DIRS: &[&str] = &[
    "sample",
    "samples",
    "trailer",
    "trailers",
    "extras",
    "featurettes",
    "behind the scenes",
    "deleted scenes",
    "interviews",
    "shorts",
    "other",
];
/// Download-in-progress suffixes the watcher should ignore.
const PARTIAL_EXTS: &[&str] = &["part", "tmp", "crdownload", "!qb", "download", "partial", "aria2", "swp"];

/// Which library a folder belongs to, which decides what file types count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum LibKind {
    Video,
    Audio,
}

/// A file the walker found that belongs in the library.
#[derive(Debug, Clone)]
struct Candidate {
    path: PathBuf,
    kind: LibKind,
    size: u64,
    mtime: Option<DateTime<Utc>>,
}

/// Outcome of processing one candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Added,
    Updated,
    Failed,
}

/// Per-scan counters shared by the concurrent probe tasks.
#[derive(Debug, Default, Clone, Copy)]
struct Counts {
    found: u64,
    probed: u64,
    added: u64,
    updated: u64,
    removed: u64,
    failed: u64,
}

pub struct Scanner {
    pub pool: PgPool,
    pub settings: Arc<SettingsCache>,
    pub activity: Activity,
    status: RwLock<ScanStatus>,
    lock: tokio::sync::Mutex<()>,
    probe_limit: Arc<tokio::sync::Semaphore>,
    /// Set by the scanner after changes so the metadata enricher wakes up.
    pub on_change: tokio::sync::Notify,
}

impl Scanner {
    pub fn new(pool: PgPool, settings: Arc<SettingsCache>, activity: Activity) -> Self {
        Self {
            pool,
            settings,
            activity,
            status: RwLock::new(ScanStatus::default()),
            lock: tokio::sync::Mutex::new(()),
            probe_limit: Arc::new(tokio::sync::Semaphore::new(4)),
            on_change: tokio::sync::Notify::new(),
        }
    }

    pub fn status(&self) -> ScanStatus {
        self.status.read().clone()
    }

    /// Full scan of every configured folder. Concurrent calls queue up
    /// behind the running one. Returns the final status.
    pub async fn scan(&self) -> anyhow::Result<ScanStatus> {
        let _guard = self.lock.lock().await;
        let started = Instant::now();
        let set = self.settings.get();
        {
            let mut st = self.status.write();
            *st = ScanStatus { scanning: true, started_at: Some(Utc::now()), ..Default::default() };
        }
        let result = self.full_scan(&set).await;
        let secs = started.elapsed().as_secs_f64();
        metrics::histogram!("ontele_scan_duration_seconds").record(secs);

        let status = {
            let mut st = self.status.write();
            st.scanning = false;
            st.finished_at = Some(Utc::now());
            match &result {
                Ok(c) => {
                    st.found = c.found;
                    st.probed = c.probed;
                    st.added = c.added;
                    st.updated = c.updated;
                    st.removed = c.removed;
                    st.last_error = None;
                }
                Err(e) => st.last_error = Some(e.to_string()),
            }
            st.clone()
        };

        match result {
            Ok(c) => {
                tracing::info!(
                    found = c.found,
                    probed = c.probed,
                    added = c.added,
                    updated = c.updated,
                    removed = c.removed,
                    failed = c.failed,
                    seconds = secs,
                    "library scan done"
                );
                self.activity.record(
                    None,
                    "scan.done",
                    None,
                    json!({
                        "found": c.found, "probed": c.probed, "added": c.added, "updated": c.updated,
                        "removed": c.removed, "failed": c.failed, "seconds": (secs * 10.0).round() / 10.0,
                    }),
                );
                self.refresh_gauges().await;
                if c.added + c.updated + c.removed > 0 {
                    self.on_change.notify_one();
                }
                Ok(status)
            }
            Err(e) => {
                tracing::error!(error = %e, "library scan failed");
                Err(e)
            }
        }
    }

    /// Incremental: (re)index just these files/dirs (filesystem watcher).
    pub async fn scan_paths(&self, paths: &[PathBuf]) -> anyhow::Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let _guard = self.lock.lock().await;
        let set = self.settings.get();
        let libs = libraries(&set);
        if libs.is_empty() {
            return Ok(());
        }

        // Resolve each path to the library it lives in; ignore strangers.
        let mut scoped: Vec<(PathBuf, LibKind)> = vec![];
        for p in paths {
            let mut matched = false;
            for (root, kind) in &libs {
                if p.starts_with(root) {
                    scoped.push((p.clone(), *kind));
                    matched = true;
                }
            }
            if !matched {
                tracing::debug!(path = %p.display(), "ignoring change outside configured libraries");
            }
        }
        if scoped.is_empty() {
            return Ok(());
        }

        let index = db::items::scan_index(&self.pool).await?;
        let mut candidates: Vec<Candidate> = vec![];
        let mut gone: Vec<String> = vec![];
        for (p, kind) in &scoped {
            // A full walk never descends into hidden/@eaDir/sample folders;
            // the incremental path must not index what a full scan would skip.
            let excluded = libs.iter().any(|(root, _)| excluded_under(root, p));
            match tokio::fs::metadata(p).await {
                Ok(_) if excluded => {
                    tracing::debug!(path = %p.display(), "ignoring change inside a skipped folder");
                }
                Ok(md) if md.is_dir() => {
                    let root = p.clone();
                    let k = *kind;
                    let found = tokio::task::spawn_blocking(move || walk_dir(&root, k)).await?;
                    candidates.extend(found);
                }
                Ok(md) if md.is_file() => {
                    if let Some(c) = candidate_from_file(p, *kind, &md) {
                        candidates.push(c);
                    }
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // The library root itself vanishing is an unmount, not a
                    // deletion: leave its rows alone (mirrors the full-scan rule).
                    if libs.iter().any(|(root, _)| root == p) {
                        tracing::warn!(dir = %p.display(), "library folder disappeared; not pruning under it");
                        continue;
                    }
                    // Deleted file or folder: drop every indexed row at or under it.
                    let ps = p.to_string_lossy().to_string();
                    let prefix = format!("{}/", ps.trim_end_matches('/'));
                    for (path, entry) in &index {
                        if path == &ps || path.starts_with(&prefix) {
                            gone.push(entry.id.clone());
                        }
                    }
                }
                Err(e) => tracing::warn!(path = %p.display(), error = %e, "stat failed"),
            }
        }
        dedupe_candidates(&mut candidates);

        let counts = self.process_candidates(&set, &index, candidates).await;
        let mut removed = 0;
        gone.sort();
        gone.dedup();
        if !gone.is_empty() {
            removed = db::items::delete_ids(&self.pool, &gone).await?;
        }
        let changed = counts.added + counts.updated + removed;
        if changed > 0 {
            tracing::info!(
                found = counts.found,
                added = counts.added,
                updated = counts.updated,
                removed,
                "incremental scan done"
            );
            self.activity.record(
                None,
                "scan.done",
                None,
                json!({ "incremental": true, "found": counts.found, "probed": counts.probed, "added": counts.added,
                        "updated": counts.updated, "removed": removed, "failed": counts.failed }),
            );
            {
                let mut st = self.status.write();
                st.added += counts.added;
                st.updated += counts.updated;
                st.removed += removed;
            }
            self.refresh_gauges().await;
            self.on_change.notify_one();
        }
        Ok(())
    }

    /// Periodic scan + optional filesystem watcher, until cancelled.
    pub async fn run_loop(self: Arc<Self>, every: Duration, cancel: CancellationToken) {
        let every = if every.is_zero() { Duration::from_secs(900) } else { every };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<PathBuf>>();
        let mut watcher = FsWatch::default();

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                r = self.scan() => {
                    if let Err(e) = r {
                        tracing::warn!(error = %e, "scheduled scan failed");
                    }
                }
            }
            watcher.sync(&self.settings.get(), &tx);

            let deadline = tokio::time::sleep(every);
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = &mut deadline => break,
                    ev = rx.recv() => {
                        let Some(mut paths) = ev else { break };
                        // coalesce whatever else the debouncer already queued
                        while let Ok(more) = rx.try_recv() {
                            paths.extend(more);
                        }
                        paths.sort();
                        paths.dedup();
                        let set = self.settings.get();
                        watcher.sync(&set, &tx);
                        if !set.watch_filesystem {
                            continue;
                        }
                        tracing::debug!(n = paths.len(), "filesystem changes");
                        tokio::select! {
                            _ = cancel.cancelled() => return,
                            r = self.scan_paths(&paths) => {
                                if let Err(e) = r {
                                    tracing::warn!(error = %e, "incremental scan failed");
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // ---- internals ------------------------------------------------------------

    async fn full_scan(&self, set: &Settings) -> anyhow::Result<Counts> {
        let libs = libraries(set);
        let index = db::items::scan_index(&self.pool).await?;

        // Walk every library in a blocking task (walkdir is sync); remember
        // which roots were actually readable so pruning stays safe.
        let mut candidates: Vec<Candidate> = vec![];
        let mut readable_roots: Vec<PathBuf> = vec![];
        for (root, kind) in &libs {
            match std::fs::read_dir(root) {
                Ok(_) => readable_roots.push(root.clone()),
                Err(e) => {
                    tracing::warn!(dir = %root.display(), error = %e, "library folder missing or unreadable; skipping (and not pruning under it)");
                    continue;
                }
            }
            let r = root.clone();
            let k = *kind;
            let found = tokio::task::spawn_blocking(move || walk_dir(&r, k)).await?;
            tracing::debug!(dir = %root.display(), files = found.len(), "walked library");
            // An empty-but-readable root that previously held files is almost
            // certainly an unmounted NAS behind a persistent mount point:
            // never prune under it (a genuinely emptied library is re-pruned
            // once it has at least one file again, or via a manual delete).
            if found.is_empty() && index.keys().any(|p| Path::new(p).starts_with(root)) {
                tracing::warn!(dir = %root.display(), "library folder is empty but the index has files under it; not pruning (unmounted?)");
                readable_roots.retain(|r| r != root);
            }
            candidates.extend(found);
        }
        dedupe_candidates(&mut candidates);
        {
            let mut st = self.status.write();
            st.found = candidates.len() as u64;
        }

        let mut counts = self.process_candidates(set, &index, candidates).await;

        // Prune rows whose file vanished, but only under roots we could read.
        // Rows under no configured library at all (the user removed the
        // folder from Settings) are dropped outright.
        let mut gone: Vec<String> = vec![];
        for (path, entry) in &index {
            let p = Path::new(path);
            if !libs.iter().any(|(r, _)| p.starts_with(r)) {
                gone.push(entry.id.clone());
                continue;
            }
            if !readable_roots.iter().any(|r| p.starts_with(r)) {
                continue;
            }
            match tokio::fs::symlink_metadata(p).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => gone.push(entry.id.clone()),
                Err(e) => tracing::debug!(path, error = %e, "stat during prune"),
            }
        }
        if !gone.is_empty() {
            for chunk in gone.chunks(500) {
                counts.removed += db::items::delete_ids(&self.pool, chunk).await?;
            }
            tracing::info!(removed = counts.removed, "pruned missing files");
        }
        Ok(counts)
    }

    /// Probe + upsert everything that is new or changed, `probe_limit` at a time.
    async fn process_candidates(
        &self,
        set: &Settings,
        index: &HashMap<String, ScanEntry>,
        candidates: Vec<Candidate>,
    ) -> Counts {
        let mut counts = Counts { found: candidates.len() as u64, ..Default::default() };
        let mut work: Vec<Candidate> = vec![];
        for c in candidates {
            let key = c.path.to_string_lossy();
            match index.get(key.as_ref()) {
                Some(e) if unchanged(e, &c) => {}
                _ => work.push(c),
            }
        }
        if work.is_empty() {
            return counts;
        }

        let parallel = self.probe_limit.available_permits().max(1) * 2;
        let mut results = stream::iter(work)
            .map(|c| async move {
                let _permit = match self.probe_limit.acquire().await {
                    Ok(p) => p,
                    Err(_) => return Outcome::Failed,
                };
                let is_new = !index.contains_key(c.path.to_string_lossy().as_ref());
                match self.index_file(set, &c).await {
                    Ok(inserted) => {
                        if inserted || is_new {
                            Outcome::Added
                        } else {
                            Outcome::Updated
                        }
                    }
                    Err(e) => {
                        tracing::warn!(path = %c.path.display(), error = %e, "indexing failed");
                        Outcome::Failed
                    }
                }
            })
            .buffer_unordered(parallel);

        while let Some(o) = results.next().await {
            counts.probed += 1;
            match o {
                Outcome::Added => counts.added += 1,
                Outcome::Updated => counts.updated += 1,
                Outcome::Failed => counts.failed += 1,
            }
            let mut st = self.status.write();
            st.probed += 1;
            match o {
                Outcome::Added => st.added += 1,
                Outcome::Updated => st.updated += 1,
                _ => {}
            }
        }
        counts
    }

    /// Probe/classify one file and write it. Returns true when the row was new.
    async fn index_file(&self, set: &Settings, c: &Candidate) -> anyhow::Result<bool> {
        let item = match c.kind {
            LibKind::Video => build_video_item(set, c).await?,
            LibKind::Audio => build_track_item(c).await?,
        };
        let inserted = db::items::upsert_scanned(&self.pool, &item).await?;
        tracing::debug!(path = %c.path.display(), kind = %item.kind, title = %item.title, new = inserted, "indexed");
        Ok(inserted)
    }

    async fn refresh_gauges(&self) {
        if let Ok(counts) = db::items::counts_by_kind(&self.pool).await {
            for (kind, n) in counts {
                metrics::gauge!("ontele_library_items", "kind" => kind).set(n as f64);
            }
        }
    }
}

// ---- filesystem watcher ---------------------------------------------------------

/// Wraps the debounced notify watcher; rebuilt whenever the library list
/// or the `watch_filesystem` toggle changes.
#[derive(Default)]
struct FsWatch {
    debouncer: Option<notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>>,
    roots: Vec<PathBuf>,
}

impl FsWatch {
    fn sync(&mut self, set: &Settings, tx: &tokio::sync::mpsc::UnboundedSender<Vec<PathBuf>>) {
        let wanted: Vec<PathBuf> = if set.watch_filesystem {
            let mut v: Vec<PathBuf> = libraries(set).into_iter().map(|(p, _)| p).collect();
            v.sort();
            v.dedup();
            v
        } else {
            vec![]
        };
        if wanted == self.roots && (self.debouncer.is_some() || wanted.is_empty()) {
            return;
        }
        self.debouncer = None;
        self.roots = wanted.clone();
        if wanted.is_empty() {
            return;
        }
        let tx = tx.clone();
        let handler = move |res: notify_debouncer_mini::DebounceEventResult| match res {
            Ok(events) => {
                let paths: Vec<PathBuf> =
                    events.into_iter().map(|e| e.path).filter(|p| !is_partial_or_hidden(p)).collect();
                if !paths.is_empty() {
                    let _ = tx.send(paths);
                }
            }
            Err(e) => tracing::warn!(error = %e, "filesystem watcher error"),
        };
        let mut deb = match notify_debouncer_mini::new_debouncer(WATCH_DEBOUNCE, handler) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, "filesystem watcher unavailable");
                return;
            }
        };
        let mut watched = 0;
        for root in &wanted {
            match deb.watcher().watch(root, notify::RecursiveMode::Recursive) {
                Ok(()) => watched += 1,
                Err(e) => {
                    tracing::warn!(dir = %root.display(), error = %e, "cannot watch library folder")
                }
            }
        }
        if watched > 0 {
            tracing::info!(dirs = watched, "filesystem watcher active");
            self.debouncer = Some(deb);
        }
    }
}

/// Hidden files and download-in-progress names never trigger a rescan.
fn is_partial_or_hidden(p: &Path) -> bool {
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name.starts_with('.') {
        return true;
    }
    let ext = naming::ext_of(p);
    PARTIAL_EXTS.contains(&ext.as_str())
        || p.components().any(|c| {
            let s = c.as_os_str().to_string_lossy();
            (s.starts_with('.') && s != "." && s != "..") || SKIP_DIRS.contains(&s.to_ascii_lowercase().as_str())
        })
}

// ---- walking ----------------------------------------------------------------------

/// Configured libraries as (absolute-ish path, kind). A folder listed in
/// both lists yields both kinds.
fn libraries(set: &Settings) -> Vec<(PathBuf, LibKind)> {
    let mut out: Vec<(PathBuf, LibKind)> = vec![];
    fn norm(d: &str) -> Option<PathBuf> {
        let d = d.trim();
        if d.is_empty() {
            return None;
        }
        let t = d.trim_end_matches('/');
        // "/" trims to "" which would match every path; keep the root as-is
        Some(PathBuf::from(if t.is_empty() { "/" } else { t }))
    }
    for d in &set.media_dirs {
        if let Some(p) = norm(d)
            && !out.contains(&(p.clone(), LibKind::Video))
        {
            out.push((p, LibKind::Video));
        }
    }
    for d in &set.music_dirs {
        if let Some(p) = norm(d)
            && !out.contains(&(p.clone(), LibKind::Audio))
        {
            out.push((p, LibKind::Audio));
        }
    }
    out
}

fn is_hidden_name(name: &str) -> bool {
    name.starts_with('.') || SKIP_DIRS.contains(&name.to_ascii_lowercase().as_str())
}

fn is_sample_dir(name: &str) -> bool {
    SAMPLE_DIRS.contains(&name.trim().to_ascii_lowercase().as_str())
}

/// True when `path` (under `root`) has a hidden/skip/sample folder anywhere
/// between the root and itself — i.e. [`walk_dir`] would not have reached it.
fn excluded_under(root: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    rel.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        is_hidden_name(&s) || is_sample_dir(&s)
    })
}

/// `sample.mkv`, `Movie.2019.sample.mkv`, `movie-sample.mkv`.
fn is_sample_file(stem: &str) -> bool {
    let s = stem.to_ascii_lowercase();
    s == "sample"
        || s.ends_with("-sample")
        || s.ends_with(".sample")
        || s.ends_with(" sample")
        || s.ends_with("_sample")
        || s.starts_with("sample-")
        || s.starts_with("sample.")
}

fn mtime_of(md: &std::fs::Metadata) -> Option<DateTime<Utc>> {
    let t = md.modified().ok()?;
    // whole seconds: Postgres keeps microseconds, the fs nanoseconds — round
    // so the comparison is stable across the DB round-trip
    let secs = t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    DateTime::<Utc>::from_timestamp(secs as i64, 0)
}

fn candidate_from_file(path: &Path, kind: LibKind, md: &std::fs::Metadata) -> Option<Candidate> {
    let name = path.file_name()?.to_str()?;
    if is_hidden_name(name) {
        return None;
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let ok = match kind {
        LibKind::Video => naming::is_video(path) && md.len() >= MIN_VIDEO_BYTES && !is_sample_file(stem),
        LibKind::Audio => naming::is_audio(path) && md.len() >= MIN_AUDIO_BYTES,
    };
    if !ok {
        return None;
    }
    Some(Candidate { path: path.to_path_buf(), kind, size: md.len(), mtime: mtime_of(md) })
}

/// Synchronous recursive walk (run inside `spawn_blocking`).
fn walk_dir(root: &Path, kind: LibKind) -> Vec<Candidate> {
    let mut out = vec![];
    let walker = walkdir::WalkDir::new(root).follow_links(true).into_iter().filter_entry(|e| {
        // the root itself is always entered, even if it's called ".hidden"
        if e.depth() == 0 {
            return true;
        }
        let name = e.file_name().to_str().unwrap_or("");
        if is_hidden_name(name) {
            return false;
        }
        if e.file_type().is_dir() && is_sample_dir(name) {
            return false;
        }
        true
    });
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(error = %e, "walk error");
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(md) = entry.metadata() else { continue };
        if let Some(c) = candidate_from_file(entry.path(), kind, &md) {
            out.push(c);
        }
    }
    out
}

fn dedupe_candidates(v: &mut Vec<Candidate>) {
    let mut seen: HashSet<PathBuf> = HashSet::with_capacity(v.len());
    v.retain(|c| seen.insert(c.path.clone()));
}

/// Same size and mtime (to the second) as the indexed row.
fn unchanged(e: &ScanEntry, c: &Candidate) -> bool {
    if e.size_bytes != Some(c.size as i64) {
        return false;
    }
    match (e.mtime, c.mtime) {
        (Some(a), Some(b)) => (a - b).num_seconds().abs() < 1,
        (None, None) => true,
        _ => false,
    }
}

// ---- item builders ------------------------------------------------------------------

async fn build_video_item(set: &Settings, c: &Candidate) -> anyhow::Result<NewItem> {
    let path = &c.path;
    let mut info = probe::probe(&set.ffprobe_path, path).await?;
    info.size_bytes = c.size as i64;
    let parsed = naming::parse_video(path);

    // sidecar subtitles join the embedded ones
    let path_for_subs = path.clone();
    let external =
        tokio::task::spawn_blocking(move || probe::external_subtitles(&path_for_subs)).await.unwrap_or_default();
    info.subtitles.extend(external);

    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
    let mut title = if parsed.title.trim().is_empty() { naming::clean_title(&stem) } else { parsed.title.clone() };
    if title.trim().is_empty() {
        title = stem.clone();
    }
    let mut year = parsed.year;
    let mut season = parsed.season;
    let mut episode = parsed.episode;
    let mut meta = None;
    let mut description = None;

    if set.metadata_providers.nfo
        && let Some(nfo_path) = nfo::find_nfo(path)
    {
        match tokio::fs::read_to_string(&nfo_path).await {
            Ok(xml) => match nfo::parse(&xml) {
                Some(n) => {
                    if let Some(t) = n.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                        title = t.to_string();
                    }
                    if n.year.is_some() {
                        year = n.year;
                    }
                    if parsed.is_episode {
                        if n.season.is_some() {
                            season = n.season;
                        }
                        if n.episode.is_some() {
                            episode = n.episode;
                        }
                    }
                    description = n.meta.overview.clone();
                    let mut m = n.meta;
                    if m.provider.is_none() {
                        m.provider = Some("nfo".into());
                    }
                    if m.updated.is_none() {
                        m.updated = Some(Utc::now());
                    }
                    meta = Some(m);
                }
                None => tracing::debug!(nfo = %nfo_path.display(), "nfo has no recognised root"),
            },
            Err(e) => tracing::debug!(nfo = %nfo_path.display(), error = %e, "nfo unreadable"),
        }
    }

    let mut auto_tags = parsed.auto_tags.clone();
    augment_tags(&mut auto_tags, &info);

    Ok(NewItem {
        id: model::item_id(&path.to_string_lossy()),
        kind: if parsed.is_episode { Kind::Episode } else { Kind::Movie },
        path: path.to_string_lossy().to_string(),
        sort_title: Some(naming::sort_title(&title)),
        title,
        year,
        show: parsed.show.clone().filter(|s| !s.trim().is_empty()),
        season,
        episode,
        episode_end: parsed.episode_end,
        air_date: parsed.air_date,
        description,
        info,
        meta,
        auto_tags,
        size_bytes: c.size as i64,
        mtime: c.mtime,
        ..Default::default()
    })
}

/// Probe-derived chips the file name may not carry.
fn augment_tags(tags: &mut Vec<String>, info: &MediaInfo) {
    fn push(tags: &mut Vec<String>, t: &str) {
        if !tags.iter().any(|x| x.eq_ignore_ascii_case(t)) {
            tags.push(t.to_string());
        }
    }
    match info.hdr() {
        Some("dv") => push(tags, "Dolby Vision"),
        Some("hdr10plus") => push(tags, "HDR10+"),
        Some("hdr10") | Some("hlg") => push(tags, "HDR"),
        _ => {}
    }
    // The probed height is the truth; a release name that says 1080p for a
    // 720p file is just wrong, so resolution chips always come from ffprobe.
    if let Some(h) = info.height.filter(|h| *h > 0) {
        tags.retain(|t| !matches!(t.as_str(), "4K" | "1440p" | "1080p" | "720p" | "480p"));
        if h >= 2000 {
            push(tags, "4K");
        } else if h >= 1400 {
            push(tags, "1440p");
        } else if h >= 1000 {
            push(tags, "1080p");
        } else if h >= 700 {
            push(tags, "720p");
        } else if h >= 400 {
            push(tags, "480p");
        }
    }
    match info.vcodec.as_deref() {
        Some("hevc") => push(tags, "HEVC"),
        Some("av1") => push(tags, "AV1"),
        _ => {}
    }
}

/// Browser-facing container name for a music file, by extension (mirrors
/// [`playback::normalize_container`] without running ffprobe).
fn audio_container(ext: &str) -> String {
    match ext {
        "m4a" | "m4b" | "m4p" | "alac" => "mp4".into(),
        "aif" | "aiff" => "aiff".into(),
        "wv" => "wavpack".into(),
        "mka" => "mkv".into(),
        "" => "unknown".into(),
        e => e.into(),
    }
}

async fn build_track_item(c: &Candidate) -> anyhow::Result<NewItem> {
    let path = c.path.clone();
    let p2 = path.clone();
    let tag = tokio::task::spawn_blocking(move || tags::read(&p2)).await??;
    let fallback = naming::parse_track_path(&path);

    let clean = |s: Option<String>| s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let title = clean(tag.title)
        .or_else(|| Some(fallback.title.clone()).filter(|t| !t.trim().is_empty()))
        .unwrap_or_else(|| path.file_stem().and_then(|s| s.to_str()).unwrap_or("Untitled").to_string());
    let artist = clean(tag.artist).or_else(|| clean(fallback.artist.clone()));
    let album_artist = clean(tag.album_artist);
    let album = clean(tag.album).or_else(|| clean(fallback.album.clone()));
    let track_no = tag.track_no.or(fallback.track_no);
    let disc_no = tag.disc_no.or(fallback.disc_no);
    let album_id =
        album.as_deref().map(|al| model::album_id(album_artist.as_deref().or(artist.as_deref()).unwrap_or(""), al));

    let codec = tag.codec.clone().unwrap_or_else(|| naming::ext_of(&path));
    let container = audio_container(&naming::ext_of(&path));
    let info = MediaInfo {
        duration_sec: tag.duration_sec,
        container,
        size_bytes: c.size as i64,
        bitrate: tag.bitrate_kbps.map(|k| k as u64 * 1000),
        acodec: Some(codec.clone()),
        audio: vec![model::AudioStream {
            index: 0,
            codec,
            channels: tag.channels.unwrap_or(2),
            default: true,
            ..Default::default()
        }],
        ..Default::default()
    };

    let mut meta = None;
    if tag.mb_release_id.is_some() {
        meta = Some(model::Metadata { mbid: tag.mb_release_id.clone(), ..Default::default() });
    }

    Ok(NewItem {
        id: model::item_id(&path.to_string_lossy()),
        kind: Kind::Track,
        path: path.to_string_lossy().to_string(),
        sort_title: Some(naming::sort_title(&title)),
        title,
        year: tag.year,
        artist,
        album_artist,
        album,
        album_id,
        track_no,
        disc_no,
        genre: clean(tag.genre),
        info,
        meta,
        auto_tags: vec![],
        size_bytes: c.size as i64,
        mtime: c.mtime,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn which(bin: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path).map(|p| p.join(bin)).find(|p| p.is_file())
    }

    #[test]
    fn library_list_dedupes_and_keeps_both_kinds() {
        let set = Settings {
            media_dirs: vec!["/tank/movies/".into(), "/tank/movies".into(), "/tank/mixed".into()],
            music_dirs: vec!["/tank/music".into(), "/tank/mixed".into(), "  ".into()],
            ..Default::default()
        };
        let libs = libraries(&set);
        assert_eq!(libs.len(), 4);
        assert!(libs.contains(&(PathBuf::from("/tank/mixed"), LibKind::Video)));
        assert!(libs.contains(&(PathBuf::from("/tank/mixed"), LibKind::Audio)));
        // "/" must stay "/" rather than collapsing to "" (which matches everything)
        let set = Settings { media_dirs: vec!["/".into()], ..Default::default() };
        assert_eq!(libraries(&set), vec![(PathBuf::from("/"), LibKind::Video)]);
    }

    #[test]
    fn incremental_paths_honour_the_walk_filters() {
        let root = Path::new("/tank/movies");
        assert!(!excluded_under(root, Path::new("/tank/movies/Movie (2019)/Movie (2019).mkv")));
        assert!(excluded_under(root, Path::new("/tank/movies/Movie (2019)/Sample/sample.mkv")));
        assert!(excluded_under(root, Path::new("/tank/movies/Movie (2019)/Trailers")));
        assert!(excluded_under(root, Path::new("/tank/movies/@eaDir/Movie.mkv")));
        assert!(excluded_under(root, Path::new("/tank/movies/.stversions/Movie.mkv")));
        assert!(!excluded_under(root, Path::new("/other/Sample/x.mkv")), "outside the root is not our call");
        assert!(!excluded_under(root, root));
    }

    #[test]
    fn name_filters() {
        assert!(is_hidden_name(".DS_Store"));
        assert!(is_hidden_name("@eaDir"));
        assert!(is_hidden_name("#recycle"));
        assert!(!is_hidden_name("Movies"));
        assert!(is_sample_dir("Sample"));
        assert!(is_sample_dir("trailers"));
        assert!(!is_sample_dir("Season 1"));
        assert!(is_sample_file("sample"));
        assert!(is_sample_file("Movie.2019.1080p-sample"));
        assert!(is_sample_file("Movie.2019.sample"));
        assert!(!is_sample_file("Sample Return Mission (2024)"));
        assert!(is_partial_or_hidden(Path::new("/m/Movie.mkv.part")));
        assert!(is_partial_or_hidden(Path::new("/m/.Movie.mkv")));
        assert!(is_partial_or_hidden(Path::new("/m/@eaDir/x/Movie.mkv")));
        assert!(!is_partial_or_hidden(Path::new("/m/Movie.mkv")));
    }

    #[test]
    fn change_detection_compares_size_and_mtime_to_the_second() {
        let now = Utc::now();
        let entry = |size: i64, mt: Option<DateTime<Utc>>| ScanEntry {
            id: "x".into(),
            kind: "movie".into(),
            size_bytes: Some(size),
            mtime: mt,
        };
        let cand = |size: u64, mt: Option<DateTime<Utc>>| Candidate {
            path: "/a".into(),
            kind: LibKind::Video,
            size,
            mtime: mt,
        };
        assert!(unchanged(&entry(10, Some(now)), &cand(10, Some(now))));
        assert!(unchanged(&entry(10, Some(now)), &cand(10, Some(now + chrono::Duration::milliseconds(400)))));
        assert!(!unchanged(&entry(10, Some(now)), &cand(11, Some(now))));
        assert!(!unchanged(&entry(10, Some(now)), &cand(10, Some(now + chrono::Duration::seconds(5)))));
        assert!(!unchanged(&entry(10, None), &cand(10, Some(now))));
        assert!(unchanged(&entry(10, None), &cand(10, None)));
    }

    #[test]
    fn walk_skips_hidden_small_and_sample_files() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let big = vec![0u8; MIN_VIDEO_BYTES as usize + 1];
        std::fs::create_dir_all(d.join("Movie (2019)/Sample")).unwrap();
        std::fs::create_dir_all(d.join(".hidden")).unwrap();
        std::fs::create_dir_all(d.join("@eaDir")).unwrap();
        std::fs::write(d.join("Movie (2019)/Movie (2019).mkv"), &big).unwrap();
        std::fs::write(d.join("Movie (2019)/Movie (2019).srt"), b"1").unwrap();
        std::fs::write(d.join("Movie (2019)/Sample/sample.mkv"), &big).unwrap();
        std::fs::write(d.join("Movie (2019)/movie-sample.mkv"), &big).unwrap();
        std::fs::write(d.join("Movie (2019)/tiny.mkv"), b"tiny").unwrap();
        std::fs::write(d.join("Movie (2019)/.partial.mkv"), &big).unwrap();
        std::fs::write(d.join(".hidden/secret.mkv"), &big).unwrap();
        std::fs::write(d.join("@eaDir/thumb.mkv"), &big).unwrap();
        std::fs::write(d.join("Movie (2019)/soundtrack.mp3"), vec![0u8; MIN_AUDIO_BYTES as usize + 1]).unwrap();

        let videos = walk_dir(d, LibKind::Video);
        assert_eq!(videos.len(), 1, "{videos:?}");
        assert!(videos[0].path.ends_with("Movie (2019).mkv"));
        assert!(videos[0].mtime.is_some());
        assert_eq!(videos[0].size, big.len() as u64);

        let audio = walk_dir(d, LibKind::Audio);
        assert_eq!(audio.len(), 1);
        assert!(audio[0].path.ends_with("soundtrack.mp3"));

        let mut both = walk_dir(d, LibKind::Video);
        both.extend(walk_dir(d, LibKind::Video));
        dedupe_candidates(&mut both);
        assert_eq!(both.len(), 1);
    }

    #[test]
    fn probe_tags_are_added_without_duplicates() {
        let mut tags = vec!["HDR".to_string(), "1080p".to_string()];
        let info = MediaInfo {
            height: Some(2160),
            vcodec: Some("hevc".into()),
            video: Some(model::VideoStream {
                codec: "hevc".into(),
                height: 2160,
                hdr: Some("hdr10".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        augment_tags(&mut tags, &info);
        // the probed 2160 line replaces the release name's wrong "1080p"
        assert_eq!(tags, vec!["HDR", "4K", "HEVC"]);
        let mut tags = vec![];
        let info = MediaInfo {
            height: Some(2160),
            video: Some(model::VideoStream { hdr: Some("dv".into()), ..Default::default() }),
            ..Default::default()
        };
        augment_tags(&mut tags, &info);
        assert_eq!(tags, vec!["Dolby Vision", "4K"]);
    }

    #[tokio::test]
    async fn builds_a_track_item_from_tags_with_path_fallback() {
        if which("ffmpeg").is_none() {
            eprintln!("ffmpeg not on PATH; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let album = dir.path().join("Daft Punk/Homework");
        std::fs::create_dir_all(&album).unwrap();
        let out = album.join("03 - Around the World.flac");
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
                "-metadata",
                "title=Around the World",
                "-metadata",
                "genre=House",
                "-c:a",
                "flac",
                "-y",
            ])
            .arg(&out)
            .status()
            .unwrap();
        assert!(st.success());
        let md = std::fs::metadata(&out).unwrap();
        let c = Candidate { path: out.clone(), kind: LibKind::Audio, size: md.len(), mtime: mtime_of(&md) };
        let item = build_track_item(&c).await.unwrap();
        assert_eq!(item.kind, Kind::Track);
        assert_eq!(item.title, "Around the World");
        assert_eq!(item.artist.as_deref(), Some("Daft Punk"), "artist from folder fallback");
        assert_eq!(item.album.as_deref(), Some("Homework"));
        assert_eq!(item.track_no, Some(3));
        assert_eq!(item.genre.as_deref(), Some("House"));
        assert_eq!(item.album_id.as_deref(), Some(model::album_id("Daft Punk", "Homework").as_str()));
        assert_eq!(item.info.container, "flac");
        assert_eq!(item.info.acodec.as_deref(), Some("flac"));
        assert!(item.info.duration_sec > 1.5);
        assert_eq!(item.sort_title.as_deref(), Some("around the world"));
        assert_eq!(item.id, model::item_id(&out.to_string_lossy()));
    }

    #[tokio::test]
    async fn builds_a_video_item_with_sidecar_subtitles() {
        if which("ffmpeg").is_none() || which("ffprobe").is_none() {
            eprintln!("ffmpeg not on PATH; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let show = dir.path().join("Severance/Season 01");
        std::fs::create_dir_all(&show).unwrap();
        let out = show.join("Severance.S01E02.Half.Loop.1080p.WEB-DL.mkv");
        let st = std::process::Command::new("ffmpeg")
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
                "2",
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
            .unwrap();
        assert!(st.success());
        std::fs::write(
            show.join("Severance.S01E02.Half.Loop.1080p.WEB-DL.en.srt"),
            b"1\n00:00:00,000 --> 00:00:01,000\nhi\n",
        )
        .unwrap();
        let md = std::fs::metadata(&out).unwrap();
        let set = Settings {
            metadata_providers: model::MetadataProviders { nfo: false, ..Default::default() },
            ..Default::default()
        };
        let c = Candidate { path: out.clone(), kind: LibKind::Video, size: md.len(), mtime: mtime_of(&md) };
        let item = build_video_item(&set, &c).await.unwrap();
        assert_eq!(item.kind, Kind::Episode);
        assert_eq!(item.show.as_deref(), Some("Severance"));
        assert_eq!((item.season, item.episode), (Some(1), Some(2)));
        assert_eq!(item.title, "Half Loop");
        assert_eq!(item.info.container, "mkv");
        assert_eq!(item.info.vcodec.as_deref(), Some("h264"));
        assert_eq!(item.info.subtitles.len(), 1);
        assert_eq!(item.info.subtitles[0].lang.as_deref(), Some("en"));
        assert!(item.info.subtitles[0].external.is_some());
        // the synthesized clip is 320x180, so the name's "1080p" is dropped
        assert!(!item.auto_tags.iter().any(|t| t == "1080p"), "{:?}", item.auto_tags);
        assert!(item.auto_tags.iter().any(|t| t == "Web"));
        assert_eq!(item.size_bytes, md.len() as i64);
        assert!(item.meta.is_none());
    }
}
