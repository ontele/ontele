// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! Logging (tracing → stdout JSON for Promtail/Loki), Prometheus metrics and
//! the activity stream (domain events logged under `target=ontele.activity`
//! *and* persisted so the UI can show them).

use crate::config::LogFormat;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use sqlx::PgPool;
use std::sync::OnceLock;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

static METRICS: OnceLock<PrometheusHandle> = OnceLock::new();

pub fn init_logging(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn,hyper=warn,h2=warn,tower_http=info"));
    let registry = tracing_subscriber::registry().with(filter);
    match format {
        LogFormat::Json => registry
            .with(
                fmt::layer()
                    .json()
                    .flatten_event(true)
                    .with_current_span(false)
                    .with_span_list(false)
                    .with_target(true),
            )
            .try_init()
            .ok(),
        LogFormat::Pretty => registry.with(fmt::layer().compact().with_target(true)).try_init().ok(),
    };
}

/// Install the global Prometheus recorder (idempotent) and return its handle.
pub fn metrics_handle() -> PrometheusHandle {
    METRICS
        .get_or_init(|| {
            let builder = PrometheusBuilder::new();
            let builder = builder
                .set_buckets_for_metric(
                    metrics_exporter_prometheus::Matcher::Full("ontele_http_request_duration_seconds".into()),
                    &[0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0],
                )
                .expect("buckets");
            match builder.install_recorder() {
                Ok(h) => h,
                // A recorder may already be installed (tests build several apps).
                Err(_) => PrometheusBuilder::new().build_recorder().handle(),
            }
        })
        .clone()
}

pub fn describe_metrics() {
    use metrics::{Unit, describe_counter, describe_gauge, describe_histogram};
    describe_counter!("ontele_http_requests_total", "HTTP requests by method, route and status");
    describe_histogram!("ontele_http_request_duration_seconds", Unit::Seconds, "HTTP request latency");
    describe_gauge!("ontele_streams_active", "Active playback sessions by mode");
    describe_gauge!("ontele_transcodes_active", "ffmpeg transcodes running");
    describe_gauge!("ontele_recordings_active", "DVR captures in progress");
    describe_gauge!("ontele_library_items", "Library items by kind");
    describe_histogram!("ontele_scan_duration_seconds", Unit::Seconds, "Library scan wall time");
    describe_counter!("ontele_metadata_lookups_total", "Metadata provider lookups by provider and result");
    describe_counter!("ontele_commercial_scans_total", "Commercial detections by detector and result");
    describe_counter!("ontele_activity_total", "Domain events by kind");
}

/// Domain-event sink: one structured log line + one `activity` row.
#[derive(Clone)]
pub struct Activity {
    pool: PgPool,
}

impl Activity {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Fire-and-forget. `detail` should be a small JSON object.
    pub fn record(&self, user_id: Option<i64>, kind: &'static str, item_id: Option<&str>, detail: serde_json::Value) {
        tracing::info!(
            target: "ontele.activity",
            kind,
            user_id = user_id.unwrap_or(0),
            item_id = item_id.unwrap_or(""),
            detail = %detail,
            "activity"
        );
        metrics::counter!("ontele_activity_total", "kind" => kind).increment(1);
        let pool = self.pool.clone();
        let item_id = item_id.map(str::to_string);
        tokio::spawn(async move {
            if let Err(e) = crate::db::activity::insert(&pool, user_id, kind, item_id.as_deref(), &detail).await {
                tracing::warn!(error = %e, "activity insert failed");
            }
        });
    }
}
