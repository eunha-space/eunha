//! What Mastodon says about its OAuth server, and about the user behind a
//! token: `WellKnown::OAuthMetadataController` and
//! `OAuth::UserinfoController`.

use axum::{extract::Extension, Json};
use serde::Serialize;

use crate::{
    error::{AppError, AppResult},
    middleware::{AuthenticatedUser, ResolvedInstance},
    state::AppState,
};

/// `Doorkeeper.configuration.scopes`: the default scope, then the optional
/// ones, as Mastodon's *config/initializers/doorkeeper.rb* lists them, each
/// once.
pub const SCOPES_SUPPORTED: &[&str] = &[
    "read",
    "profile",
    "write",
    "write:accounts",
    "write:blocks",
    "write:bookmarks",
    "write:collections",
    "write:conversations",
    "write:favourites",
    "write:filters",
    "write:follows",
    "write:lists",
    "write:media",
    "write:mutes",
    "write:notifications",
    "write:reports",
    "write:statuses",
    "read:accounts",
    "read:blocks",
    "read:bookmarks",
    "read:collections",
    "read:favourites",
    "read:filters",
    "read:follows",
    "read:lists",
    "read:mutes",
    "read:notifications",
    "read:search",
    "read:statuses",
    "follow",
    "push",
    "admin:read",
    "admin:read:accounts",
    "admin:read:reports",
    "admin:read:domain_allows",
    "admin:read:domain_blocks",
    "admin:read:ip_blocks",
    "admin:read:email_domain_blocks",
    "admin:read:canonical_email_blocks",
    "admin:write",
    "admin:write:accounts",
    "admin:write:reports",
    "admin:write:domain_allows",
    "admin:write:domain_blocks",
    "admin:write:ip_blocks",
    "admin:write:email_domain_blocks",
    "admin:write:canonical_email_blocks",
];

/// `OAuthMetadataSerializer`, in its order.
#[derive(Serialize)]
pub struct OAuthMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    revocation_endpoint: String,
    userinfo_endpoint: String,
    scopes_supported: &'static [&'static str],
    response_types_supported: &'static [&'static str],
    response_modes_supported: &'static [&'static str],
    grant_types_supported: &'static [&'static str],
    token_endpoint_auth_methods_supported: &'static [&'static str],
    code_challenge_methods_supported: &'static [&'static str],
    service_documentation: &'static str,
    app_registration_endpoint: String,
}

/// `GET /.well-known/oauth-authorization-server`: RFC 8414 metadata, as
/// `OAuthMetadataPresenter` fills it in for `grant_flows %w(authorization_code
/// client_credentials)`, no refresh tokens, and PKCE with `S256` alone.
pub async fn oauth_metadata(
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
) -> Json<OAuthMetadata> {
    let root = format!("https://{}", instance.domain);
    Json(OAuthMetadata {
        issuer: format!("{root}/"),
        authorization_endpoint: format!("{root}/oauth/authorize"),
        token_endpoint: format!("{root}/oauth/token"),
        revocation_endpoint: format!("{root}/oauth/revoke"),
        userinfo_endpoint: format!("{root}/oauth/userinfo"),
        scopes_supported: SCOPES_SUPPORTED,
        // `authorization_response_types` of the code flow.
        response_types_supported: &["code"],
        // Its `response_mode_matches`.
        response_modes_supported: &["query", "fragment", "form_post"],
        grant_types_supported: &["authorization_code", "client_credentials"],
        token_endpoint_auth_methods_supported: &["client_secret_basic", "client_secret_post"],
        code_challenge_methods_supported: &["S256"],
        service_documentation: "https://docs.joinmastodon.org/",
        app_registration_endpoint: format!("{root}/api/v1/apps"),
    })
}

/// `OAuthUserinfoSerializer`.
#[derive(Serialize)]
pub struct Userinfo {
    iss: String,
    sub: String,
    name: String,
    preferred_username: String,
    profile: String,
    picture: String,
}

/// `GET` and `POST /oauth/userinfo`, OpenID Connect's UserInfo endpoint,
/// which the specification has answer both: `doorkeeper_authorize!
/// :profile`, then `require_user!`.
pub async fn userinfo(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<Userinfo>> {
    let Extension(auth) = auth.ok_or(AppError::Unauthorized)?;
    if !auth.scopes.iter().any(|s| s == "profile") {
        return Err(AppError::ForbiddenScope);
    }
    crate::middleware::require_user(Some(&auth))?;
    let account = super::accounts::fetch_account(&state, auth.account_id).await?;
    let api = super::convert::account_from_db(&state.urls, &account);
    Ok(Json(Userinfo {
        iss: format!("https://{}/", instance.domain),
        sub: api.uri,
        name: account.display_name.clone(),
        preferred_username: account.username.clone(),
        profile: api.url,
        picture: api.avatar,
    }))
}
