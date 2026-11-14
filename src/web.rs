//! eunha's web frontend, and the pages served beside it.
//!
//! Mastodon: app/controllers/custom_css_controller.rb
//! Mastodon: app/controllers/wrapstodon_controller.rb
//! Mastodon: app/views/wrapstodon/show.html.haml
//! Mastodon: app/controllers/concerns/web_app_controller_concern.rb
//! Mastodon: app/views/layouts/application.html.haml
//! Mastodon: app/controllers/remote_accounts_controller.rb
//! Mastodon: app/controllers/health_controller.rb
//! Mastodon: app/controllers/intents_controller.rb
//! Mastodon: app/controllers/manifests_controller.rb
//! Mastodon: app/serializers/manifest_serializer.rb

pub mod pages;
pub mod rss;

/// Serves eunha's own web frontend: a React Router (Vite) single-page app that
/// is a first-party client of the Mastodon Client-to-Server (C2S) REST API.
///
/// The built SPA lives in `frontend/dist`. Static assets (hashed JS/CSS, fonts,
/// images, manifest, etc.) are served directly; every other path falls back to
/// `index.html` so the client-side router can take over.
use axum::{
    http::{header, HeaderValue, StatusCode, Uri},
    response::{Html, IntoResponse, Response},
};

use crate::{middleware::AuthenticatedUser, state::AppState};

const DIST: &str = "frontend/dist";

pub async fn serve(state: AppState, uri: Uri, viewer: Option<AuthenticatedUser>) -> Response {
    let path = uri.path().trim_start_matches('/');

    if let Some((username, year, share_key)) = wrapstodon_path(path) {
        return wrapstodon(&state, viewer.as_ref(), username, year, share_key).await;
    }

    // `CustomCssController`, at both of Mastodon's paths.
    if path == "custom.css" || (path.starts_with("css/") && path.ends_with(".css")) {
        return custom_css(&state).await;
    }

    match path {
        // `HealthController#show`: the process answers, and nothing more.
        "health" => {
            return ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "OK").into_response();
        }
        "manifest" | "manifest.json" => return manifest(&state).await,
        // `get 'change-password', to: redirect('/auth/edit')`.
        ".well-known/change-password" => {
            return redirect(
                StatusCode::MOVED_PERMANENTLY,
                "/account/password".to_owned(),
            );
        }
        // Both Mastodon's old ways to the remote follow page.
        ".well-known/proxy" | "authorize_follow" => {
            return redirect(
                StatusCode::MOVED_PERMANENTLY,
                format!("/authorize_interaction?{}", uri.query().unwrap_or_default()),
            );
        }
        "intent" => return intent(&state, uri.query()).await,
        _ => {}
    }

    if let Some(route) = pages::route(path) {
        return public_page(&state, &uri, route).await;
    }

    // Serve static assets directly. Path traversal guard: reject anything with "..".
    if !path.is_empty() && !path.contains("..") {
        let file_path = format!("{DIST}/{path}");
        if let Ok(bytes) = tokio::fs::read(&file_path).await {
            let mime = mime_guess::from_path(&file_path)
                .first_or_octet_stream()
                .to_string();
            return ([(header::CONTENT_TYPE, mime)], bytes).into_response();
        }
    }

    let head = pages::instance_head(&state, &public_url(&state, &uri)).await;
    let index = serve_page(
        &state,
        Page {
            head: head.head,
            ..Page::default()
        },
    )
    .await;
    if remote_path_missing(&state, path).await {
        // `RemoteAccountsController`: the page renders, under a 404.
        let (mut parts, body) = index.into_parts();
        parts.status = axum::http::StatusCode::NOT_FOUND;
        return Response::from_parts(parts, body);
    }
    index
}

/// `url_for(only_path: false)`, on the instance's own domain.
fn public_url(state: &AppState, uri: &Uri) -> String {
    let path = uri
        .path_and_query()
        .map_or(uri.path(), |path_and_query| path_and_query.as_str());
    format!("https://{}{path}", state.urls.web_domain)
}

fn redirect(status: StatusCode, location: String) -> Response {
    match HeaderValue::from_str(&location) {
        Ok(location) => (status, [(header::LOCATION, location)]).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

fn query_param(query: Option<&str>, name: &str) -> Option<String> {
    url::form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

/// `IntentsController#show`: a `web+mastodon://` link, sent where it means.
async fn intent(state: &AppState, query: Option<&str>) -> Response {
    let target = query_param(query, "uri")
        .and_then(|uri| url::Url::parse(&uri).ok())
        .filter(|uri| uri.scheme() == "web+mastodon")
        .and_then(|uri| {
            let param = |name: &str| {
                uri.query_pairs()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.into_owned())
            };
            match uri.host_str()? {
                "follow" => {
                    let target = param("uri")?;
                    let target = target.strip_prefix("acct:").unwrap_or(&target);
                    Some(format!(
                        "/authorize_interaction?uri={}",
                        urlencoding::encode(target)
                    ))
                }
                "share" => Some(format!(
                    "/share?text={}",
                    urlencoding::encode(&param("text").unwrap_or_default())
                )),
                _ => None,
            }
        });
    match target {
        Some(target) => redirect(StatusCode::FOUND, target),
        None => with_status(serve_index(state).await, StatusCode::NOT_FOUND),
    }
}

/// `ManifestsController#show`: `ManifestSerializer`, without Mastodon's
/// `related_applications`, which would offer an eunha's visitors Mastodon's
/// apps in place of the page.
async fn manifest(state: &AppState) -> Response {
    let settings = crate::settings::Snapshot::load(state).await;
    let title = settings.site_title(&state.instance);
    let app_icon = crate::site_uploads::find(state, "app_icon")
        .await
        .ok()
        .flatten();
    let icons: Vec<serde_json::Value> = crate::site_uploads::ANDROID_ICON_SIZES
        .iter()
        .map(|size| {
            let src = app_icon
                .as_ref()
                .and_then(|upload| upload.url(state, &size.to_string()))
                .unwrap_or_else(|| {
                    format!(
                        "https://{}/icons/android-chrome-{size}x{size}.png",
                        state.urls.web_domain
                    )
                });
            serde_json::json!({
                "src": src,
                "sizes": format!("{size}x{size}"),
                "type": "image/png",
                "purpose": "any maskable",
            })
        })
        .collect();
    let body = serde_json::json!({
        "id": "/home",
        "name": title,
        "short_name": title,
        "icons": icons,
        "theme_color": "#191b22",
        "background_color": "#191b22",
        "display": "standalone",
        "start_url": "/",
        "scope": "/",
        "share_target": {
            "url_template": "share?title={title}&text={text}&url={url}",
            "action": "share",
            "method": "GET",
            "enctype": "application/x-www-form-urlencoded",
            "params": { "title": "title", "text": "text", "url": "url" },
        },
        "shortcuts": [
            { "name": "Compose new post", "url": "/share" },
            { "name": "Notifications", "url": "/notifications" },
            { "name": "Explore", "url": "/explore" },
        ],
        "prefer_related_applications": false,
    });
    with_cache(
        (
            [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
            body.to_string(),
        )
            .into_response(),
        "max-age=180, public",
    )
}

const VARY_PAGE: HeaderValue = HeaderValue::from_static("Accept, Accept-Language, Cookie");

/// A local account's, a post's or a hashtag's page, feed or embed. The server
/// does not know who is reading it, because the web app signs in with a token
/// it keeps to itself, so each answers as to a visitor; the app that loads
/// under a refusal still shows a signed-in member what they may see.
async fn public_page(state: &AppState, uri: &Uri, route: pages::Route) -> Response {
    let mut response = match public_answer(state, uri, route).await {
        Ok(response) => response,
        Err(error) => return error.into_response(),
    };
    response.headers_mut().insert(header::VARY, VARY_PAGE);
    response
}

/// A refusal: the web app under its status, and for a page or a feed of an
/// unavailable account, `expires_in(3.minutes, public: true)`.
async fn refused(state: &AppState, status: StatusCode, html: bool) -> Response {
    let response = if html {
        with_status(serve_index(state).await, status)
    } else {
        status.into_response()
    };
    if status == StatusCode::NOT_FOUND {
        response
    } else {
        with_cache(response, "max-age=180, public")
    }
}

fn with_link(mut response: Response, link: Option<String>) -> Response {
    if let Some(value) = link.and_then(|link| HeaderValue::from_str(&link).ok()) {
        response.headers_mut().insert(header::LINK, value);
    }
    response
}

fn rss_response(feed: String, cache: &'static str) -> Response {
    with_cache(
        (
            [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
            feed,
        )
            .into_response(),
        cache,
    )
}

async fn public_answer(
    state: &AppState,
    uri: &Uri,
    route: pages::Route,
) -> crate::error::AppResult<Response> {
    use pages::{Owner, Route};

    let limited = state.instance.limited_federation_mode;
    let limit = rss::limit_param(query_param(uri.query(), "limit").as_deref());
    match route {
        // `authenticate_user!, if: :limited_federation_mode?`: nothing about
        // the page for a visitor, whom the web app asks to sign in.
        Route::Account { rss: false, .. }
        | Route::Status { .. }
        | Route::Tag { rss: false, .. }
            if limited =>
        {
            Ok(serve_index(state).await)
        }
        Route::Account { rss: true, .. } | Route::Embed { .. } | Route::Tag { rss: true, .. }
            if limited =>
        {
            Ok(StatusCode::NOT_FOUND.into_response())
        }

        Route::Account { username, tab, rss } => {
            let owner = pages::owner(state, &username).await?;
            let Owner::Found(account) = &owner else {
                let status = pages::refusal(&owner).unwrap_or(StatusCode::NOT_FOUND);
                return Ok(refused(state, status, !rss).await);
            };
            if rss {
                let statuses = rss::account_statuses(state, account, &tab, limit).await?;
                let feed = rss::account_feed(state, account, &tab, statuses).await?;
                return Ok(rss_response(feed, "max-age=60, public"));
            }
            let head = pages::account_head(state, account, &tab).await;
            let page = serve_page(
                state,
                Page {
                    title: head.title,
                    head: head.head,
                    ..Page::default()
                },
            )
            .await;
            Ok(with_link(
                with_cache(
                    page,
                    "max-age=15, public, stale-while-revalidate=30, stale-if-error=3600",
                ),
                head.link,
            ))
        }

        Route::Status { username, id } => {
            let owner = pages::owner(state, &username).await?;
            let Owner::Found(account) = &owner else {
                let status = pages::refusal(&owner).unwrap_or(StatusCode::NOT_FOUND);
                return Ok(refused(state, status, true).await);
            };
            let Some(status) = pages::readable_status(state, account.id, id).await? else {
                return Ok(refused(state, StatusCode::NOT_FOUND, true).await);
            };
            let entity =
                crate::api::mastodon::statuses::serialize_status(state, &status, None).await?;
            // `redirect_to_original`.
            if let Some(original) = &entity.reblog {
                let target = original.url.clone().unwrap_or_else(|| original.uri.clone());
                return Ok(redirect(StatusCode::FOUND, target));
            }
            let head = pages::status_head(state, account, &status, &entity).await?;
            let page = serve_page(
                state,
                Page {
                    title: head.title,
                    head: head.head,
                    ..Page::default()
                },
            )
            .await;
            Ok(with_link(with_cache(page, "max-age=10, public"), head.link))
        }

        Route::Embed { username, id } => {
            let owner = pages::owner(state, &username).await?;
            let Owner::Found(account) = &owner else {
                let status = pages::refusal(&owner).unwrap_or(StatusCode::NOT_FOUND);
                return Ok(refused(state, status, false).await);
            };
            // `verify_embed_allowed`: never a boost, nor anything hidden.
            let status = pages::readable_status(state, account.id, id)
                .await?
                .filter(|status| status.reblog_of_id.is_none());
            let Some(status) = status else {
                return Ok(StatusCode::NOT_FOUND.into_response());
            };
            let entity =
                crate::api::mastodon::statuses::serialize_status(state, &status, None).await?;
            Ok(with_cache(
                Html(pages::embed_page(state, &entity)).into_response(),
                "max-age=180, public",
            ))
        }

        Route::Tag { name, rss } => {
            // `Tag.usable.find_normalized!`.
            let tag = crate::search::tags::find_normalized(state, &name).await?;
            let usable = match &tag {
                Some(tag) => {
                    sqlx::query_scalar!(
                        r#"SELECT usable IS NOT FALSE AS "usable!" FROM tags WHERE id = $1"#,
                        tag.id,
                    )
                    .fetch_one(&state.db)
                    .await?
                }
                None => false,
            };
            let Some(tag) = tag.filter(|_| usable) else {
                return Ok(refused(state, StatusCode::NOT_FOUND, !rss).await);
            };
            if rss {
                let local = query_param(uri.query(), "local")
                    .is_some_and(|local| !matches!(local.as_str(), "" | "0" | "false" | "f"));
                let statuses = rss::tag_statuses(state, &tag, local, limit).await?;
                let feed = rss::tag_feed(state, &tag, statuses).await?;
                return Ok(rss_response(feed, "max-age=0, public"));
            }
            let head = pages::tag_head(state, &tag.name, &public_url(state, uri)).await;
            Ok(with_cache(
                serve_page(
                    state,
                    Page {
                        head: head.head,
                        ..Page::default()
                    },
                )
                .await,
                "max-age=15, public, stale-while-revalidate=30, stale-if-error=3600",
            ))
        }
    }
}

/// `RemoteAccountsController#require_resource_exists!`, for
/// `/@username@domain/(*any)`: whether `PermalinkRedirector` finds nothing.
async fn remote_path_missing(state: &AppState, path: &str) -> bool {
    let Some(rest) = path.strip_prefix('@') else {
        return false;
    };
    let mut segments = rest.split('/');
    let first = segments.next().unwrap_or_default();
    let Some((username, domain)) = first.split_once('@') else {
        return false;
    };
    let domain = domain.split('@').next().unwrap_or_default();
    let second = segments.next().filter(|segment| !segment.is_empty());
    if let Some(id) = second.filter(|segment| segment.bytes().all(|b| b.is_ascii_digit())) {
        // `at_username_status_request?`.
        let found = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM statuses s JOIN accounts a ON a.id = s.account_id
                 WHERE s.id = $1 AND s.deleted_at IS NULL AND s.visibility IN (0, 1)
                   AND s.local IS NOT TRUE AND a.domain IS NOT NULL
                   AND a.suspended_at IS NULL) AS "e!""#,
            id.parse::<i64>().unwrap_or(-1),
        )
        .fetch_one(&state.db)
        .await
        .unwrap_or(true);
        return !found;
    }
    // `Account.find_remote`, ignoring case; a local account is no answer.
    let found = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM accounts
             WHERE lower(username) = lower($1) AND lower(domain) = lower($2)
               AND suspended_at IS NULL) AS "e!""#,
        username,
        domain,
    )
    .fetch_one(&state.db)
    .await
    .unwrap_or(true);
    !found
}

/// `CustomCssController#show`: the `custom_css` setting, cached for a month.
async fn custom_css(state: &AppState) -> Response {
    let css = crate::settings::string(state, "custom_css").await;
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "max-age=2592000, public"),
        ],
        css,
    )
        .into_response()
}

/// The page's own additions to the web app's `index.html`.
#[derive(Default)]
struct Page {
    /// `content_for :page_title`, which the layout puts before the site title.
    title: Option<String>,
    head: String,
    body: String,
}

async fn serve_index(state: &AppState) -> Response {
    serve_page(state, Page::default()).await
}

async fn serve_page(state: &AppState, page: Page) -> Response {
    let Ok(html) = tokio::fs::read_to_string(format!("{DIST}/index.html")).await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "The eunha web frontend is not built yet.",
        )
            .into_response();
    };

    let settings = crate::settings::Snapshot::load(state).await;
    let site_title = settings.site_title(&state.instance);
    let title = match &page.title {
        Some(title) => format!("{title} - {site_title}"),
        None => site_title.clone(),
    };
    let mut html = html.replace(
        "<title>eunha</title>",
        &format!("<title>{}</title>", escape_html_text(&title)),
    );

    // What Mastodon's layout adds to the head from the settings: the uploaded
    // favicon in its sizes, and the custom stylesheet named by its digest.
    let mut head = page.head;
    // The site's name apart from the page's: `<title>` on a profile says the
    // profile first, and the web app names the server in its rail from this
    // until `/api/v2/instance` answers.
    head.push_str(&format!(
        r#"<meta name="application-name" content="{}" />"#,
        escape_html_attribute(&site_title)
    ));
    head.push_str(r#"<link rel="manifest" href="/manifest.json" />"#);
    if let Ok(Some(favicon)) = crate::site_uploads::find(state, "favicon").await {
        for size in crate::site_uploads::FAVICON_SIZES {
            if let Some(url) = favicon.url(state, &size.to_string()) {
                head.push_str(&format!(
                    r#"<link rel="icon" type="image/png" sizes="{size}x{size}" href="{}" />"#,
                    escape_html_text(&url).replace('"', "&quot;")
                ));
            }
        }
    }
    let custom_css = settings.string("custom_css");
    if !custom_css.trim().is_empty() {
        use sha2::{Digest, Sha256};
        let digest = hex::encode(Sha256::digest(custom_css.as_bytes()));
        head.push_str(&format!(
            r#"<link rel="stylesheet" media="all" href="/css/custom-{}.css" />"#,
            &digest[..8]
        ));
    }
    if !head.is_empty() {
        html = html.replacen("</head>", &format!("{head}</head>"), 1);
    }
    if !page.body.is_empty() {
        html = html.replacen("</body>", &format!("{}</body>", page.body), 1);
    }

    // `WebAppControllerConcern#set_referer_header`.
    let referrer_policy = if settings.boolean("allow_referrer_origin") {
        "strict-origin-when-cross-origin"
    } else {
        "same-origin"
    };
    ([(header::REFERRER_POLICY, referrer_policy)], Html(html)).into_response()
}

/// `/@:account_username/wrapstodon/:year/:share_key`, Mastodon's
/// `public_wrapstodon` route.
fn wrapstodon_path(path: &str) -> Option<(&str, &str, &str)> {
    let rest = path.strip_prefix('@')?;
    let mut segments = rest.split('/');
    let (username, literal, year, share_key) = (
        segments.next()?,
        segments.next()?,
        segments.next()?,
        segments.next()?,
    );
    (segments.next().is_none()
        && literal == "wrapstodon"
        && ![username, year, share_key].iter().any(|s| s.is_empty()))
    .then_some((username, year, share_key))
}

/// `WrapstodonController#show`: the web app, carrying the report as
/// `wrapstodon/show` does, for anyone with the link. A refusal is the web app
/// too, under the refusal's status, for the page to say so.
async fn wrapstodon(
    state: &AppState,
    viewer: Option<&AuthenticatedUser>,
    username: &str,
    year: &str,
    share_key: &str,
) -> Response {
    use crate::api::mastodon::annual_reports::{shared, Shared};

    let found = match shared(state, viewer, username, year, share_key).await {
        Ok(found) => found,
        Err(error) => return error.into_response(),
    };
    let mut response = match found {
        Shared::SignInRequired => serve_index(state).await,
        Shared::NotFound => with_status(serve_index(state).await, StatusCode::NOT_FOUND),
        // `expires_in(3.minutes, public: true)`.
        Shared::Gone => with_cache(
            with_status(serve_index(state).await, StatusCode::GONE),
            "max-age=180, public",
        ),
        Shared::Suspended => with_cache(
            with_status(serve_index(state).await, StatusCode::FORBIDDEN),
            "max-age=180, public",
        ),
        Shared::Found(report) => {
            let site_title = crate::settings::site_title(state).await;
            let page = wrapstodon_page(state, &site_title, &report);
            let response = serve_page(state, page).await;
            // `expires_in 10.minutes, public: true if current_account.nil?`.
            if viewer.is_some_and(|v| v.user_id.is_some()) {
                with_cache(response, "private, no-store")
            } else {
                with_cache(response, "max-age=600, public")
            }
        }
    };
    // `vary_by 'Accept, Accept-Language, Cookie'`.
    response.headers_mut().insert(
        header::VARY,
        HeaderValue::from_static("Accept, Accept-Language, Cookie"),
    );
    response
}

/// `wrapstodon/show`: its title, `noindex`, the OpenGraph tags, and the
/// report in `#wrapstodon-data`.
fn wrapstodon_page(
    state: &AppState,
    site_title: &str,
    report: &crate::api::mastodon::annual_reports::SharedReport,
) -> Page {
    let account = &report.account;
    // `display_name(account)`.
    let name = Some(account.display_name.as_str())
        .filter(|n| !n.trim().is_empty())
        .unwrap_or(&account.username);
    let title = format!("Wrapstodon {} for {name}", report.year);
    let description = format!("See how {name} used Mastodon this year!");
    let meta = |attribute: &str, key: &str, content: &str| {
        format!(
            r#"<meta {attribute}="{}" content="{}" />"#,
            escape_html_attribute(key),
            escape_html_attribute(content)
        )
    };
    let mut head = String::new();
    head.push_str(&meta("name", "robots", "noindex, noarchive"));
    head.push_str(&meta("property", "og:site_name", site_title));
    head.push_str(&meta("property", "og:type", "article"));
    head.push_str(&meta("property", "og:title", &title));
    // `acct(@account)[1..]`.
    head.push_str(&meta(
        "property",
        "profile:username",
        &format!("{}@{}", account.username, state.instance.local_domain),
    ));
    head.push_str(&meta("name", "description", &description));
    head.push_str(&meta("property", "og:description", &description));
    Page {
        title: Some(title),
        head,
        body: format!(
            r#"<script type="application/json" id="wrapstodon-data">{}</script>"#,
            json_escape(&report.payload.to_string())
        ),
    }
}

fn with_status(mut response: Response, status: StatusCode) -> Response {
    *response.status_mut() = status;
    response
}

fn with_cache(mut response: Response, value: &'static str) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(value));
    response
}

/// ActiveSupport's `json_escape`: JSON that is safe inside a `<script>`.
fn json_escape(json: &str) -> String {
    json.replace('&', "\\u0026")
        .replace('>', "\\u003e")
        .replace('<', "\\u003c")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

fn escape_html_attribute(s: &str) -> String {
    escape_html_text(s).replace('"', "&quot;")
}

fn escape_html_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    #[test]
    fn wrapstodon_path_is_mastodons_route() {
        assert_eq!(
            super::wrapstodon_path("@alice/wrapstodon/2025/0123abcd"),
            Some(("alice", "2025", "0123abcd"))
        );
        for path in [
            "@alice/wrapstodon/2025",
            "@alice/wrapstodon/2025/key/more",
            "@alice/wrapped/2025/key",
            "alice/wrapstodon/2025/key",
            "@alice/wrapstodon//key",
        ] {
            assert_eq!(super::wrapstodon_path(path), None, "{path}");
        }
    }

    #[test]
    fn json_escape_keeps_the_script_closed() {
        let json = serde_json::json!({ "note": "</script><b>&\u{2028}" }).to_string();
        let escaped = super::json_escape(&json);
        assert!(!escaped.contains('<') && !escaped.contains('>') && !escaped.contains('&'));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&escaped).unwrap()["note"],
            "</script><b>&\u{2028}"
        );
    }
}
