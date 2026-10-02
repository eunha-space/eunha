//! `Api::V1::Accounts::EmailSubscriptionsController`, and the pages a
//! subscriber reaches from their email: `EmailSubscriptions::ConfirmationsController`
//! and `UnsubscriptionsController`. See [`crate::email_subscriptions`].

use axum::{
    extract::{Path, Query},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    Form, Json,
};
use serde::Deserialize;

use super::extractors::Params;
use crate::{
    db::models::Account,
    email_subscriptions as subs,
    error::{AppError, AppResult},
    locale::Locale,
    state::AppState,
};

// ── POST /api/v1/accounts/:account_id/email_subscriptions ──────────────────

#[derive(Debug, Deserialize)]
pub struct CreateForm {
    pub email: Option<String>,
    pub lang: Option<String>,
}

/// `Localized#requested_locale`, short of a signed-in user's own: `lang`, then
/// `Accept-Language`, then the default.
fn requested_locale(lang: Option<&str>, headers: &HeaderMap) -> String {
    use crate::languages::valid_locale;
    if valid_locale(lang) {
        return lang.unwrap_or_default().to_string();
    }
    let accept = headers
        .get(axum::http::header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    for tag in accept.split(',') {
        let tag = tag.split(';').next().unwrap_or("").trim();
        if valid_locale(Some(tag)) {
            return tag.to_string();
        }
        let primary = tag.split(['-', '_']).next().unwrap_or("");
        if valid_locale(Some(primary)) {
            return primary.to_string();
        }
    }
    super::DEFAULT_LOCALE.to_string()
}

/// `create`: subscribe an address to a local account's posts. Mastodon's
/// `head 404` — an empty body — answers when the feature is off, or the
/// account cannot be subscribed to.
pub async fn create(
    state: AppState,
    Path(account_id): Path<i64>,
    headers: HeaderMap,
    Params(form): Params<CreateForm>,
) -> AppResult<Response> {
    // `set_account`: `Account.local.find`.
    let account = sqlx::query_as!(
        Account,
        "SELECT * FROM accounts WHERE id = $1 AND domain IS NULL",
        account_id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    // `require_feature_enabled!`
    if !subs::enabled(&state).await {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    // `require_account_permissions!`
    if account.is_unavailable() || !subs::offered_by(&state, account.id).await {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let locale = requested_locale(form.lang.as_deref(), &headers);
    match subs::create(
        &state,
        account.id,
        form.email.as_deref().unwrap_or(""),
        &locale,
    )
    .await
    {
        // `render_empty`
        Ok(_) => Ok(Json(serde_json::json!({})).into_response()),
        Err(subs::CreateError::Invalid(errors)) => Ok(errors.into_response()),
        Err(subs::CreateError::Database(e)) => Err(e.into()),
    }
}

// ── The pages ──────────────────────────────────────────────────────────────

fn page_locale(headers: &HeaderMap) -> Locale {
    Locale::detect(
        None,
        headers
            .get(axum::http::header::ACCEPT_LANGUAGE)
            .and_then(|v| v.to_str().ok()),
    )
}

struct Page<'a> {
    title: String,
    paragraphs: Vec<String>,
    form_token: Option<&'a str>,
    /// The notification type a user unsubscribes from, posted with the token.
    form_type: Option<&'a str>,
    form_button: &'static str,
    link: Option<(String, &'static str)>,
}

fn render(state: &AppState, locale: Locale, page: Page<'_>) -> Response {
    let (link_href, link_text) = page.link.unzip();
    Html(crate::templates::render(
        "email_subscription.html",
        minijinja::context! {
            lang => locale.as_str(),
            domain => &state.instance.domain,
            title => page.title,
            paragraphs => page.paragraphs,
            form_token => page.form_token,
            form_type => page.form_type,
            form_button => page.form_button,
            link_href => link_href,
            link_text => link_text,
        },
    ))
    .into_response()
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Html("<!DOCTYPE html><title>Not found</title><p>The page you are looking for isn't here.</p>"),
    )
        .into_response()
}

fn strong_name(account: &Account) -> String {
    format!(
        "<strong>{}</strong>",
        crate::email::html_escape(&subs::display_name(account))
    )
}

#[derive(Debug, Deserialize)]
pub struct ConfirmationQuery {
    pub confirmation_token: Option<String>,
}

/// GET /email_subscriptions/confirmation: confirm, and say what comes next.
pub async fn confirmation(
    state: AppState,
    headers: HeaderMap,
    Query(q): Query<ConfirmationQuery>,
) -> Response {
    let Some(token) = q.confirmation_token.filter(|t| !t.is_empty()) else {
        return not_found();
    };
    let (id, account) = match subs::confirm(&state, &token).await {
        Ok(Some(found)) => found,
        Ok(None) => return not_found(),
        Err(error) => {
            tracing::error!(%error, "could not confirm an email subscription");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let locale = page_locale(&headers);
    let ko = locale == Locale::Ko;
    let sender = format!(
        "<strong>{}</strong>",
        crate::email::html_escape(state.email.from_address())
    );
    let unsubscribe = format!(
        "{} <a href=\"{}\">{}</a>",
        if ko {
            "마음이 바뀌었나요?"
        } else {
            "Changed your mind?"
        },
        crate::email::html_escape(&subs::unsubscribe_url(&state, id, &token)),
        if ko { "구독 해제" } else { "Unsubscribe" },
    );
    render(
        &state,
        locale,
        Page {
            title: if ko {
                "가입되었습니다"
            } else {
                "You're signed up"
            }
            .to_string(),
            paragraphs: vec![
                format!(
                    "You'll now start receiving emails when {} publishes new posts. Add {sender} \
                     to your contacts so these posts don't end up in your Spam folder.",
                    strong_name(&account)
                ),
                unsubscribe,
            ],
            form_token: None,
            form_type: None,
            form_button: "",
            link: None,
        },
    )
}

#[derive(Debug, Deserialize)]
pub struct UnsubscribeParams {
    pub token: Option<String>,
    /// For a user's link, the notification type to stop mailing.
    #[serde(rename = "type")]
    pub kind: Option<String>,
}

/// GET /unsubscribe: ask before unsubscribing.
pub async fn unsubscribe_page(
    state: AppState,
    headers: HeaderMap,
    Query(q): Query<UnsubscribeParams>,
) -> Response {
    let Some(token) = q.token.filter(|t| !t.is_empty()) else {
        return not_found();
    };
    if let Some(user_id) = crate::notification_mail::user_from_token(&state, &token) {
        return user_page(&state, &headers, &token, user_id, q.kind.as_deref(), false).await;
    }
    let account = match subs::subscription_account(&state, &token).await {
        Ok(Some(account)) => account,
        Ok(None) => return not_found(),
        Err(error) => {
            tracing::error!(%error, "could not find an email subscription");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let locale = page_locale(&headers);
    let ko = locale == Locale::Ko;
    let name = subs::display_name(&account);
    render(
        &state,
        locale,
        Page {
            title: if ko {
                format!("{name} 구독을 해지할까요?")
            } else {
                format!("Unsubscribe from {name}?")
            },
            paragraphs: vec![
                "You'll stop receiving emails when this account publishes new posts.".into(),
            ],
            form_token: Some(&token),
            form_type: None,
            form_button: if ko { "구독 해지" } else { "Unsubscribe" },
            link: None,
        },
    )
}

/// POST /unsubscribe: unsubscribe. The token comes in the form the page
/// posts, or in the query string of the `List-Unsubscribe` link that a mail
/// client posts `List-Unsubscribe=One-Click` to.
pub async fn unsubscribe(
    state: AppState,
    headers: HeaderMap,
    Query(q): Query<UnsubscribeParams>,
    form: Result<Form<UnsubscribeParams>, axum::extract::rejection::FormRejection>,
) -> Response {
    let form = form.ok().map(|Form(f)| f);
    let kind = form.as_ref().and_then(|f| f.kind.clone()).or(q.kind);
    let token = form
        .and_then(|f| f.token)
        .or(q.token)
        .filter(|t| !t.is_empty());
    let Some(token) = token else {
        return not_found();
    };
    if let Some(user_id) = crate::notification_mail::user_from_token(&state, &token) {
        return user_page(&state, &headers, &token, user_id, kind.as_deref(), true).await;
    }
    let account = match subs::unsubscribe(&state, &token).await {
        Ok(Some(account)) => account,
        Ok(None) => return not_found(),
        Err(error) => {
            tracing::error!(%error, "could not unsubscribe");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let locale = page_locale(&headers);
    let ko = locale == Locale::Ko;
    let name = crate::email::html_escape(&subs::display_name(&account));
    render(
        &state,
        locale,
        Page {
            title: if ko {
                "구독이 해지되었습니다"
            } else {
                "You are unsubscribed"
            }
            .to_string(),
            paragraphs: vec![if ko {
                format!("{name}로부터 더이상 메일을 받지 않게 됩니다.")
            } else {
                format!("You'll no longer receive emails from {name}.")
            }],
            form_token: None,
            form_type: None,
            form_button: "",
            link: Some((
                "/".to_string(),
                if ko {
                    "서버 홈페이지로 이동"
                } else {
                    "Go to server homepage"
                },
            )),
        },
    )
}

/// `UnsubscriptionsController` for a user's link from a notification email:
/// `show` asks (`create` false), `create` turns `notification_emails.<type>`
/// off. A type it cannot unsubscribe from is a 404, as
/// `require_type_if_user!` makes it.
async fn user_page(
    state: &AppState,
    headers: &HeaderMap,
    token: &str,
    user_id: i64,
    kind: Option<&str>,
    create: bool,
) -> Response {
    let Some(kind) = kind.and_then(|k| {
        crate::notification_mail::UNSUBSCRIBABLE_TYPES
            .iter()
            .find(|t| **t == k)
    }) else {
        return not_found();
    };
    let locale = page_locale(headers);
    let ko = locale == Locale::Ko;
    // `unsubscriptions.notification_emails.<type>`
    let label = match (*kind, ko) {
        ("favourite", false) => "favorite notification emails",
        ("follow", false) => "follow notification emails",
        ("follow_request", false) => "follow request emails",
        ("mention", false) => "mention notification emails",
        ("reblog", false) => "boost notification emails",
        ("quote", false) => "quote notification emails",
        ("favourite", true) => "좋아요 알림 이메일",
        ("follow", true) => "팔로우 알림 이메일",
        ("follow_request", true) => "팔로우 요청 이메일",
        ("mention", true) => "멘션 알림 이메일",
        ("reblog", true) => "부스트 알림 이메일",
        _ => "인용 알림 이메일",
    };
    let domain = crate::email::html_escape(&state.instance.domain);
    if !create {
        let exists = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM users WHERE id = $1) AS "e!""#,
            user_id
        )
        .fetch_one(&state.db)
        .await
        .unwrap_or(false);
        if !exists {
            return not_found();
        }
        return render(
            state,
            locale,
            Page {
                title: if ko {
                    format!("{label} 구독을 해지할까요?")
                } else {
                    format!("Unsubscribe from {label}?")
                },
                paragraphs: vec![format!(
                    "You'll stop receiving {label} from Mastodon on {domain}."
                )],
                form_token: Some(token),
                form_type: Some(kind),
                form_button: if ko { "구독 해지" } else { "Unsubscribe" },
                link: None,
            },
        );
    }
    match crate::notification_mail::unsubscribe(state, user_id, kind).await {
        Ok(true) => {}
        Ok(false) => return not_found(),
        Err(error) => {
            tracing::error!(%error, "could not unsubscribe a user from notification emails");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    render(
        state,
        locale,
        Page {
            title: if ko {
                "구독이 해지되었습니다"
            } else {
                "You are unsubscribed"
            }
            .to_string(),
            paragraphs: vec![format!(
                "You'll no longer receive {label} from Mastodon on {domain}."
            )],
            form_token: None,
            form_type: None,
            form_button: "",
            link: Some((
                "/".to_string(),
                if ko {
                    "서버 홈페이지로 이동"
                } else {
                    "Go to server homepage"
                },
            )),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::requested_locale;
    use axum::http::HeaderMap;

    #[test]
    fn locale_comes_from_lang_then_accept_language() {
        let mut headers = HeaderMap::new();
        headers.insert("accept-language", "ko-KR,ko;q=0.9".parse().unwrap());
        assert_eq!(requested_locale(Some("ja"), &headers), "ja");
        assert_eq!(requested_locale(None, &headers), "ko");
        assert_eq!(requested_locale(None, &HeaderMap::new()), "en");
    }
}
