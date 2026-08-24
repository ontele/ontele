// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

use clap::Parser;
use ontele::config::Config;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::parse();
    ontele::telemetry::init_logging(cfg.log_format);
    let addr = cfg.addr.clone();
    let no_bg = cfg.no_background;

    let state = ontele::build_state(cfg).await?;
    let cancel = CancellationToken::new();
    if !no_bg {
        ontele::spawn_background(state.clone(), cancel.clone());
    }

    let app = ontele::build_app(state.clone());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(addr = %addr, data = %state.data_dir().display(), version = env!("CARGO_PKG_VERSION"), "ontele listening");

    let shutdown = {
        let cancel = cancel.clone();
        async move {
            let ctrl_c = tokio::signal::ctrl_c();
            #[cfg(unix)]
            {
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("sigterm");
                tokio::select! { _ = ctrl_c => {}, _ = term.recv() => {} }
            }
            #[cfg(not(unix))]
            {
                let _ = ctrl_c.await;
            }
            tracing::info!("shutdown requested");
            cancel.cancel();
        }
    };
    axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .with_graceful_shutdown(shutdown)
        .await?;
    // give background loops a moment to stop ffmpeg children
    tokio::time::sleep(Duration::from_millis(300)).await;
    state.pool.close().await;
    Ok(())
}
