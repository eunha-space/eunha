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

    serve_index(&state).await
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
        None => site_title,
    };
    let mut html = html.replace(
        "<title>eunha</title>",
        &format!("<title>{}</title>", escape_html_text(&title)),
    );

    // What Mastodon's layout adds to the head from the settings: the uploaded
    // favicon in its sizes, and the custom stylesheet named by its digest.
    let mut head = page.head;
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
        &format!("{}@{}", account.username, state.urls.local_domain),
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
