// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! Process configuration. Flags/env only *bootstrap* settings on first run —
//! after that the `settings` row in Postgres (edited via the UI or
//! `PUT /api/settings`) is the source of truth, so redeploys never clobber
//! values tuned in the UI.

use clap::{Parser, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AuthMode {
    /// Trust oauth2-proxy identity headers; reject requests without them.
    Proxy,
    /// No identity: everyone is the single `local` user (dev / trusted LAN).
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LogFormat {
    Json,
    Pretty,
}

#[derive(Debug, Clone, Parser)]
#[command(name = "ontele", version, about = "Ontele media server: library, live TV, DVR, commercial skip")]
pub struct Config {
    /// Listen address.
    #[arg(long, env = "ONTELE_ADDR", default_value = "0.0.0.0:7979")]
    pub addr: String,

    /// PostgreSQL connection string.
    #[arg(long, env = "DATABASE_URL")]
    pub database_url: String,

    /// Max DB pool connections.
    #[arg(long, env = "ONTELE_DB_POOL", default_value_t = 16)]
    pub db_pool: u32,

    /// State + cache directory (artwork, HLS scratch, sprites).
    #[arg(long, env = "ONTELE_DATA", default_value = "./ontele-data")]
    pub data_dir: PathBuf,

    /// Comma-separated video library dirs (first-run bootstrap).
    #[arg(long, env = "ONTELE_MEDIA", default_value = "")]
    pub media_dirs: String,

    /// Comma-separated music library dirs (first-run bootstrap).
    #[arg(long, env = "ONTELE_MUSIC", default_value = "")]
    pub music_dirs: String,

    /// DVR output dir (first-run bootstrap).
    #[arg(long, env = "ONTELE_RECORDINGS", default_value = "")]
    pub recordings_dir: String,

    /// XMLTV guide URL or file (first-run bootstrap).
    #[arg(long, env = "ONTELE_XMLTV", default_value = "")]
    pub xmltv: String,

    /// DVR post-processing command (first-run bootstrap).
    #[arg(long, env = "ONTELE_DVR_POST_CMD", default_value = "")]
    pub dvr_post_cmd: String,

    /// HDHomeRun IP (first-run bootstrap; empty = UDP auto-discover).
    #[arg(long, env = "ONTELE_HDHR", default_value = "")]
    pub hdhr_ip: String,

    /// off|skip|delete (first-run bootstrap).
    #[arg(long, env = "ONTELE_COMMERCIALS", default_value = "")]
    pub commercials: String,

    /// TMDB API key (first-run bootstrap).
    #[arg(long, env = "ONTELE_TMDB_API_KEY", default_value = "")]
    pub tmdb_api_key: String,

    /// Identity source.
    #[arg(long, env = "ONTELE_AUTH", value_enum, default_value_t = AuthMode::Proxy)]
    pub auth: AuthMode,

    /// Comma-separated admin emails/usernames.
    #[arg(long, env = "ONTELE_ADMIN_USERS", default_value = "")]
    pub admin_users: String,

    /// Comma-separated admin groups (from X-Forwarded-Groups).
    #[arg(long, env = "ONTELE_ADMIN_GROUPS", default_value = "")]
    pub admin_groups: String,

    /// Log output format.
    #[arg(long, env = "ONTELE_LOG_FORMAT", value_enum, default_value_t = LogFormat::Json)]
    pub log_format: LogFormat,

    /// Disable all background loops (scan, DVR, guide). Used by tests.
    #[arg(long, env = "ONTELE_NO_BACKGROUND", default_value_t = false)]
    pub no_background: bool,
}

impl Config {
    pub fn split_list(s: &str) -> Vec<String> {
        s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect()
    }
    pub fn admin_users(&self) -> Vec<String> {
        Self::split_list(&self.admin_users).into_iter().map(|s| s.to_lowercase()).collect()
    }
    pub fn admin_groups(&self) -> Vec<String> {
        Self::split_list(&self.admin_groups)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_list_trims_and_drops_empties() {
        assert_eq!(Config::split_list(" /a, /b ,,"), vec!["/a", "/b"]);
        assert!(Config::split_list("").is_empty());
    }

    #[test]
    fn parses_from_args() {
        let c = Config::try_parse_from(["ontele", "--database-url", "postgres://x", "--auth", "none"]).unwrap();
        assert_eq!(c.auth, AuthMode::None);
        assert_eq!(c.addr, "0.0.0.0:7979");
    }
}
