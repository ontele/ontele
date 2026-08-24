// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! One error type for handlers and services. Converts into the JSON error
//! envelope `{"error": "..."}` the UI expects.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    BadRequest(String),
    #[error("authentication required")]
    Unauthorized,
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Gone(String),
    #[error("{0}")]
    Upstream(String),
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("{0}")]
    Internal(#[from] anyhow::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type AppResult<T> = Result<T, AppError>;

impl AppError {
    pub fn bad<S: Into<String>>(s: S) -> Self {
        AppError::BadRequest(s.into())
    }
    pub fn not_found<S: Into<String>>(s: S) -> Self {
        AppError::NotFound(s.into())
    }
    pub fn status(&self) -> StatusCode {
        match self {
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::Unauthorized => StatusCode::UNAUTHORIZED,
            AppError::Forbidden(_) => StatusCode::FORBIDDEN,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::Gone(_) => StatusCode::GONE,
            AppError::Upstream(_) => StatusCode::BAD_GATEWAY,
            AppError::Db(sqlx::Error::RowNotFound) => StatusCode::NOT_FOUND,
            AppError::Db(_) | AppError::Internal(_) | AppError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();
        let msg = match &self {
            AppError::Db(sqlx::Error::RowNotFound) => "not found".to_string(),
            AppError::Db(e) => {
                tracing::error!(error = %e, "database error");
                "database error".to_string()
            }
            // 5xx text stays generic: anyhow/io chains routinely carry
            // filesystem paths, command lines and upstream URLs.
            AppError::Internal(e) => {
                tracing::error!(error = ?e, "internal error");
                "internal error".to_string()
            }
            AppError::Io(e) => {
                tracing::error!(error = %e, "io error");
                "io error".to_string()
            }
            other => other.to_string(),
        };
        (status, Json(json!({ "error": msg }))).into_response()
    }
}

impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        AppError::BadRequest(e.to_string())
    }
}

impl From<axum::extract::rejection::JsonRejection> for AppError {
    fn from(e: axum::extract::rejection::JsonRejection) -> Self {
        AppError::BadRequest(e.body_text())
    }
}

impl From<reqwest::Error> for AppError {
    fn from(e: reqwest::Error) -> Self {
        AppError::Upstream(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_statuses() {
        assert_eq!(AppError::bad("x").status(), StatusCode::BAD_REQUEST);
        assert_eq!(AppError::not_found("x").status(), StatusCode::NOT_FOUND);
        assert_eq!(AppError::Db(sqlx::Error::RowNotFound).status(), StatusCode::NOT_FOUND);
        assert_eq!(AppError::Unauthorized.status(), StatusCode::UNAUTHORIZED);
    }
}
