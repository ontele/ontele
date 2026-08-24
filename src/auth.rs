// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! Identity. Ontele never sees credentials: an OAuth2 proxy (oauth2-proxy,
//! Pomerium, an Ingress auth annotation…) authenticates the browser and
//! forwards the identity in headers. We trust those headers, upsert the user,
//! and attach a [`User`] to the request.
//!
//! Security model: the server must only be reachable through the proxy
//! (compose: no published port; k8s: NetworkPolicy). In `ONTELE_AUTH=none`
//! mode every request is the `local` user.

use crate::{config::AuthMode, error::AppError, model::User, state::AppState};
use axum::{
    extract::{FromRequestParts, Request, State},
    http::{header::HeaderMap, request::Parts},
    middleware::Next,
    response::Response,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    pub subject: String,
    pub email: Option<String>,
    pub username: Option<String>,
    pub groups: Vec<String>,
}

fn first_header<'a>(h: &'a HeaderMap, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|n| h.get(*n).and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty()))
}

/// Parse the oauth2-proxy header set. Returns `None` when no identity is present.
pub fn identity_from_headers(h: &HeaderMap) -> Option<Identity> {
    let email = first_header(h, &["x-forwarded-email", "x-auth-request-email"]).map(str::to_string);
    let username =
        first_header(h, &["x-forwarded-preferred-username", "x-auth-request-preferred-username"]).map(str::to_string);
    let user = first_header(h, &["x-forwarded-user", "x-auth-request-user"]).map(str::to_string);
    let groups = first_header(h, &["x-forwarded-groups", "x-auth-request-groups"])
        .map(|g| g.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    // Subject preference: stable user id > email > username.
    let subject = user.clone().or_else(|| email.clone()).or_else(|| username.clone())?;
    Some(Identity { subject, email, username, groups })
}

/// Per-process cache so a 60-request page load costs one upsert, not sixty.
#[derive(Default)]
pub struct UserCache {
    map: dashmap::DashMap<String, (Arc<User>, Instant)>,
}

impl UserCache {
    const TTL: Duration = Duration::from_secs(60);

    pub fn get(&self, subject: &str) -> Option<Arc<User>> {
        self.map.get(subject).filter(|e| e.1.elapsed() < Self::TTL).map(|e| e.0.clone())
    }
    pub fn put(&self, user: Arc<User>) {
        self.map.insert(user.subject.clone(), (user, Instant::now()));
    }
    pub fn invalidate(&self, subject: &str) {
        self.map.remove(subject);
    }
    pub fn clear(&self) {
        self.map.clear();
    }
}

pub async fn resolve_user(state: &AppState, headers: &HeaderMap) -> Result<Arc<User>, AppError> {
    let ident = match state.cfg.auth {
        AuthMode::None => Identity { subject: "local".into(), username: Some("local".into()), ..Default::default() },
        AuthMode::Proxy => identity_from_headers(headers).ok_or(AppError::Unauthorized)?,
    };
    if let Some(u) = state.users.get(&ident.subject) {
        return Ok(u);
    }
    let admin_users = state.cfg.admin_users();
    let admin_groups = state.cfg.admin_groups();
    let is_admin = state.cfg.auth == AuthMode::None
        || ident.email.as_deref().map(|e| admin_users.contains(&e.to_lowercase())).unwrap_or(false)
        || ident.username.as_deref().map(|u| admin_users.contains(&u.to_lowercase())).unwrap_or(false)
        || admin_users.contains(&ident.subject.to_lowercase())
        || ident.groups.iter().any(|g| admin_groups.contains(g));
    // Only fall back to "first user seen becomes admin" when the operator has
    // not named any admins; otherwise the first visitor would silently outrank
    // the configured ones.
    let bootstrap = admin_users.is_empty() && admin_groups.is_empty();
    let user = crate::db::users::upsert(
        &state.pool,
        &ident.subject,
        ident.email.as_deref(),
        ident.username.as_deref(),
        &ident.groups,
        is_admin,
        bootstrap,
    )
    .await?;
    let user = Arc::new(user);
    state.users.put(user.clone());
    Ok(user)
}

/// Cross-site state-changing requests are rejected. The proxy cookie rides
/// along on a form POST from another origin, so a page elsewhere could
/// trigger `/api/scan`, deletes, etc. Browsers stamp `Sec-Fetch-Site` on
/// every request; same-origin / same-site / none (typed URL) are fine.
pub fn is_cross_site_write(method: &axum::http::Method, headers: &HeaderMap) -> bool {
    use axum::http::Method;
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return false;
    }
    headers
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().eq_ignore_ascii_case("cross-site"))
        .unwrap_or(false)
}

/// Middleware: resolve identity once per request and stash it in extensions.
pub async fn require_user(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Result<Response, AppError> {
    if is_cross_site_write(req.method(), req.headers()) {
        return Err(AppError::Forbidden("cross-site request rejected".into()));
    }
    let user = resolve_user(&state, req.headers()).await?;
    req.extensions_mut().insert(user.clone());
    let mut res = next.run(req).await;
    // surfaced to the request logger, which runs outside this middleware
    res.extensions_mut().insert(user);
    Ok(res)
}

/// Extractor for handlers: `CurrentUser(user)`.
#[derive(Debug, Clone)]
pub struct CurrentUser(pub Arc<User>);

impl<S: Send + Sync> FromRequestParts<S> for CurrentUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts.extensions.get::<Arc<User>>().cloned().map(CurrentUser).ok_or(AppError::Unauthorized)
    }
}

/// Extractor that additionally requires admin.
#[derive(Debug, Clone)]
pub struct AdminUser(pub Arc<User>);

impl<S: Send + Sync> FromRequestParts<S> for AdminUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let user = parts.extensions.get::<Arc<User>>().cloned().ok_or(AppError::Unauthorized)?;
        if user.is_admin { Ok(AdminUser(user)) } else { Err(AppError::Forbidden("admin required".into())) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn hm(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn parses_oauth2_proxy_headers() {
        let h = hm(&[
            ("x-forwarded-email", "a@b.c"),
            ("x-forwarded-user", "sub-123"),
            ("x-forwarded-preferred-username", "alice"),
            ("x-forwarded-groups", "admins, media"),
        ]);
        let id = identity_from_headers(&h).unwrap();
        assert_eq!(id.subject, "sub-123");
        assert_eq!(id.email.as_deref(), Some("a@b.c"));
        assert_eq!(id.username.as_deref(), Some("alice"));
        assert_eq!(id.groups, vec!["admins", "media"]);
    }

    #[test]
    fn falls_back_to_email_and_xauthrequest() {
        let h = hm(&[("x-auth-request-email", "only@mail")]);
        let id = identity_from_headers(&h).unwrap();
        assert_eq!(id.subject, "only@mail");
        assert!(identity_from_headers(&hm(&[])).is_none());
        assert!(identity_from_headers(&hm(&[("x-forwarded-user", "  ")])).is_none());
    }

    #[test]
    fn cross_site_writes_are_flagged() {
        use axum::http::Method;
        let cross = hm(&[("sec-fetch-site", "cross-site")]);
        assert!(is_cross_site_write(&Method::POST, &cross));
        assert!(is_cross_site_write(&Method::DELETE, &cross));
        assert!(!is_cross_site_write(&Method::GET, &cross));
        assert!(!is_cross_site_write(&Method::POST, &hm(&[("sec-fetch-site", "same-origin")])));
        assert!(!is_cross_site_write(&Method::POST, &hm(&[])));
    }
}
