/// Serves eunha's own web frontend: a React Router (Vite) single-page app that
/// is a first-party client of the Mastodon Client-to-Server (C2S) REST API.
///
/// The built SPA lives in `frontend/dist`. Static assets (hashed JS/CSS, fonts,
/// images, manifest, etc.) are served directly; every other path falls back to
/// `index.html` so the client-side router can take over.
use axum::{
    http::{header, StatusCode, Uri},
    response::{Html, IntoResponse, Response},
};

use crate::state::AppState;

const DIST: &str = "frontend/dist";

pub async fn serve(state: AppState, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');

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

async fn serve_index(state: &AppState) -> Response {
    let Ok(html) = tokio::fs::read_to_string(format!("{DIST}/index.html")).await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "The eunha web frontend is not built yet.",
        )
            .into_response();
    };

    let settings = crate::settings::Snapshot::load(state).await;
    let mut html = html.replace(
        "<title>eunha</title>",
        &format!(
            "<title>{}</title>",
            escape_html_text(&settings.site_title(&state.instance))
        ),
    );

    // What Mastodon's layout adds to the head from the settings: the uploaded
    // favicon in its sizes, and the custom stylesheet named by its digest.
    let mut head = String::new();
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

    // `WebAppControllerConcern#set_referer_header`.
    let referrer_policy = if settings.boolean("allow_referrer_origin") {
        "strict-origin-when-cross-origin"
    } else {
        "same-origin"
    };
    ([(header::REFERRER_POLICY, referrer_policy)], Html(html)).into_response()
}

fn escape_html_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
