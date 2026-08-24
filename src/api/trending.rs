// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! `GET /api/trending?window=day|week|month|year|all` — most-watched items
//! and viewers over the window, from the `play_log` day-grain aggregates.

use crate::auth::CurrentUser;
use crate::db;
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use axum::extract::{Query, State};
use axum::response::Json;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Deserialize)]
pub struct TrendingQuery {
    #[serde(default)]
    pub window: String,
}

pub async fn trending(
    State(st): State<Arc<AppState>>,
    CurrentUser(_u): CurrentUser,
    Query(q): Query<TrendingQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let days = match q.window.as_str() {
        "" | "week" => Some(7),
        "day" => Some(1),
        "month" => Some(30),
        "year" => Some(365),
        "all" => None,
        other => return Err(AppError::BadRequest(format!("unknown window {other:?}"))),
    };
    let (items, users) =
        tokio::try_join!(db::trending::top_items(&st.pool, days, 20), db::trending::top_users(&st.pool, days, 10))?;
    Ok(Json(json!({ "window": if q.window.is_empty() { "week" } else { &q.window }, "items": items, "users": users })))
}
