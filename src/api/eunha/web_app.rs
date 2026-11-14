//! The OAuth client eunha's own web app signs in with: the instance's own
//! application (`superapp`), so that the authorization page answers it at
//! once instead of asking each new member to authorize the site they just
//! joined.
//!
//! Mastodon's web client needs no client of its own (its pages are served
//! with a session token), and its `Web` application is made by the seeds
//! (*db/seeds/01_web_app.rb*).
//! eunha's web app signs in through `/oauth/authorize`, so this application
//! is made the first time the web app asks for it. It is public
//! (`confidential = false`): every browser shares it, so it has no secret
//! to keep, and the web app proves the code is its own with PKCE. Its only
//! redirect URI is the web app's callback on the instance's web domain, so a
//! code it is given goes nowhere else.
//!
//! Mastodon: none

use axum::Json;
use serde::Serialize;

use crate::{error::AppResult, state::AppState};

/// What the web app asks for (`SCOPES` in *frontend/src/auth.ts*).
pub const SCOPES: &str = "read write follow push admin:read admin:write";

#[derive(Debug, Serialize)]
pub struct WebApp {
    pub client_id: String,
    pub redirect_uri: String,
    pub scopes: String,
}

/// GET /api/eunha/v1/web_app: public, as registering an app is.
pub async fn show(state: AppState) -> AppResult<Json<WebApp>> {
    let redirect_uri = format!("https://{}/auth/callback", state.instance.web_domain());
    let mut tx = state.db.begin().await?;
    // One at a time, so two first visits make one application.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('eunha web app'))")
        .execute(&mut *tx)
        .await?;
    let existing: Option<(i64, String, String)> = sqlx::query_as(
        "SELECT id, uid, scopes FROM oauth_applications
         WHERE superapp AND NOT confidential AND redirect_uri = $1
         ORDER BY id LIMIT 1",
    )
    .bind(&redirect_uri)
    .fetch_optional(&mut *tx)
    .await?;
    let client_id = match existing {
        Some((id, uid, scopes)) => {
            // The web app asks for more as it grows.
            if scopes != SCOPES {
                sqlx::query(
                    "UPDATE oauth_applications SET scopes = $2, updated_at = now() WHERE id = $1",
                )
                .bind(id)
                .bind(SCOPES)
                .execute(&mut *tx)
                .await?;
            }
            uid
        }
        None => {
            let uid = crate::crypto::generate_token(32);
            sqlx::query(
                "INSERT INTO oauth_applications
                   (name, uid, secret, redirect_uri, scopes, website, superapp, confidential,
                    created_at, updated_at)
                 VALUES ('eunha', $1, $2, $3, $4, $5, true, false, now(), now())",
            )
            .bind(&uid)
            .bind(crate::crypto::generate_token(64))
            .bind(&redirect_uri)
            .bind(SCOPES)
            .bind(format!("https://{}", state.instance.web_domain()))
            .execute(&mut *tx)
            .await?;
            uid
        }
    };
    tx.commit().await?;
    Ok(Json(WebApp {
        client_id,
        redirect_uri,
        scopes: SCOPES.to_owned(),
    }))
}
