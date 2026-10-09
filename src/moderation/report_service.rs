//! `ReportService`: an account reporting another, from the API or from a
//! remote server's `Flag`.

use serde_json::json;

use crate::db::models::{report_category, Account};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// `ReportService#call`'s options.
#[derive(Debug, Default, Clone)]
pub struct Options {
    pub status_ids: Vec<i64>,
    pub collection_ids: Vec<i64>,
    pub comment: String,
    pub category: Option<String>,
    pub rule_ids: Vec<i64>,
    pub forward: bool,
    pub forward_to_domains: Option<Vec<String>>,
    pub uri: Option<String>,
    pub application_id: Option<i64>,
}

/// `ReportService#call`, returning the new report's id.
pub async fn call(
    state: &AppState,
    source: &Account,
    target: &Account,
    options: Options,
) -> AppResult<i64> {
    // `@category = options[:rule_ids].present? ? 'violation' : (category.presence || 'other')`
    let category = if !options.rule_ids.is_empty() {
        report_category::VIOLATION
    } else {
        let name = options
            .category
            .as_deref()
            .filter(|c| !c.is_empty())
            .unwrap_or("other");
        report_category::parse(name)
            .ok_or_else(|| AppError::Unprocessable(format!("'{name}' is not a valid category")))?
    };
    let rule_ids = (!options.rule_ids.is_empty()).then_some(options.rule_ids.clone());

    // `raise ActiveRecord::RecordNotFound if @target_account.unavailable?`
    if target.is_unavailable() {
        return Err(AppError::NotFound);
    }

    let forward = !target.is_local() && options.forward;
    let forward_to_domains: Vec<String> = {
        let given = options
            .forward_to_domains
            .clone()
            .unwrap_or_else(|| target.domain.clone().into_iter().collect());
        // `.filter_map { normalize_domain(_1) }.uniq`: a domain Addressable
        // refuses raises, and nothing rescues it.
        let mut domains: Vec<String> = Vec::with_capacity(given.len());
        for domain in &given {
            let domain = crate::federation::tag_manager::normalize_domain(domain)
                .map_err(|error| AppError::Unrescued(error.to_string()))?;
            if !domains.contains(&domain) {
                domains.push(domain);
            }
        }
        domains
    };
    let forward_to_origin = forward
        && target
            .domain
            .as_deref()
            .is_some_and(|d| forward_to_domains.iter().any(|f| f == d));

    let status_ids = reported_status_ids(state, source, target, &options.status_ids).await?;
    let collection_ids = reported_collection_ids(state, target, &options.collection_ids).await?;

    // Validations on create.
    if source.is_local() && options.comment.chars().count() > crate::moderation::COMMENT_SIZE_LIMIT
    {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: Comment is too long (maximum is {} characters)",
            crate::moderation::COMMENT_SIZE_LIMIT
        )));
    }
    if let Some(ids) = &rule_ids {
        if !crate::moderation::rules::all_exist(state, ids).await? {
            return Err(AppError::Unprocessable(
                "Validation failed: Rule ids does not reference valid rules".into(),
            ));
        }
    }

    // `set_uri`: a local reporter's report gets an id of its own.
    let uri = options.uri.clone().or_else(|| {
        source
            .is_local()
            .then(|| crate::federation::relationships::generate_uri(&state.instance.domain))
    });

    let mut tx = state.db.begin().await?;
    let report_id = sqlx::query_scalar!(
        r#"INSERT INTO reports
             (account_id, target_account_id, status_ids, comment, uri, forwarded, category,
              rule_ids, application_id, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, now(), now())
           RETURNING id"#,
        source.id,
        target.id,
        &status_ids,
        options.comment,
        uri,
        forward_to_origin,
        category,
        rule_ids.as_deref(),
        options.application_id,
    )
    .fetch_one(&mut *tx)
    .await?;
    for collection_id in &collection_ids {
        sqlx::query!(
            r#"INSERT INTO collection_reports (report_id, collection_id, created_at, updated_at)
               VALUES ($1, $2, now(), now())"#,
            report_id,
            collection_id,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    notify_staff(state, report_id, target.id).await;
    // `after_create_commit :trigger_create_webhooks`
    super::webhooks::trigger(
        state,
        "report.created",
        super::webhooks::Object::Report(report_id),
    )
    .await;

    if forward {
        if let Err(error) = forward_report(
            state,
            report_id,
            target,
            &status_ids,
            forward_to_origin,
            &forward_to_domains,
        )
        .await
        {
            tracing::warn!(report_id, %error, "could not forward a report");
        }
    }

    Ok(report_id)
}

/// `reported_status_ids`.
async fn reported_status_ids(
    state: &AppState,
    source: &Account,
    target: &Account,
    requested: &[i64],
) -> AppResult<Vec<i64>> {
    let mut requested: Vec<i64> = requested.to_vec();
    requested.sort_unstable();
    requested.dedup();
    if requested.is_empty() {
        return Ok(vec![]);
    }

    if source.is_local() {
        // `AccountStatusesFilter.new(target, source).results.with_discarded
        // .find(status_ids)`: every one must be a post of the target's the
        // reporter can see, or none is reported at all.
        let found: Vec<i64> = sqlx::query_scalar!(
            r#"SELECT s.id FROM statuses s
               WHERE s.account_id = $1 AND s.id = ANY($3)
                 AND (
                   $1 = $2
                   OR (
                     NOT EXISTS (SELECT 1 FROM blocks b WHERE b.account_id = $1 AND b.target_account_id = $2)
                     AND (
                       s.visibility IN (0, 1)
                       OR (s.visibility = 2 AND EXISTS (
                             SELECT 1 FROM follows f WHERE f.account_id = $2 AND f.target_account_id = $1))
                       OR EXISTS (SELECT 1 FROM mentions m WHERE m.status_id = s.id AND m.account_id = $2)
                     )
                   )
                 )
               ORDER BY s.id"#,
            target.id,
            source.id,
            &requested,
        )
        .fetch_all(&state.db)
        .await?;
        if found.len() != requested.len() {
            return Err(AppError::NotFound);
        }
        return Ok(found);
    }

    // A remote reporter is likely anonymized (the instance actor), so the
    // requirements are relaxed: public and unlisted posts, private ones too if
    // the reporter's server has followers of the target, and any post that
    // mentions someone there. Missing posts are dropped rather than failing.
    let reporter_domain = source.domain.clone().unwrap_or_default();
    Ok(sqlx::query_scalar!(
        r#"SELECT s.id FROM statuses s
           WHERE s.account_id = $1 AND s.id = ANY($3)
             AND (
               s.visibility IN (0, 1)
               OR (s.visibility = 2 AND EXISTS (
                     SELECT 1 FROM follows f JOIN accounts fa ON fa.id = f.account_id
                     WHERE f.target_account_id = $1 AND fa.domain = $2))
               OR EXISTS (
                     SELECT 1 FROM mentions m JOIN accounts ma ON ma.id = m.account_id
                     WHERE m.status_id = s.id AND ma.domain = $2)
             )
           ORDER BY s.id"#,
        target.id,
        reporter_domain,
        &requested,
    )
    .fetch_all(&state.db)
    .await?)
}

/// `reported_collection_ids`: `target.collections.find(collection_ids)`.
async fn reported_collection_ids(
    state: &AppState,
    target: &Account,
    requested: &[i64],
) -> AppResult<Vec<i64>> {
    let mut requested = requested.to_vec();
    requested.sort_unstable();
    requested.dedup();
    if requested.is_empty() {
        return Ok(vec![]);
    }
    let found: Vec<i64> = sqlx::query_scalar!(
        "SELECT id FROM collections WHERE account_id = $1 AND id = ANY($2) ORDER BY id",
        target.id,
        &requested,
    )
    .fetch_all(&state.db)
    .await?;
    if found.len() != requested.len() {
        return Err(AppError::NotFound);
    }
    Ok(found)
}

/// `notify_staff!`: unless an earlier report about the target is still open,
/// everyone who may manage reports gets `admin.report`, and an email unless
/// they turned report emails off.
pub async fn notify_staff(state: &AppState, report_id: i64, target_id: i64) {
    // `unresolved_siblings?`
    let siblings = sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM reports
             WHERE id <> $1 AND target_account_id = $2 AND action_taken_at IS NULL
           ) AS "e!""#,
        report_id,
        target_id,
    )
    .fetch_one(&state.db)
    .await
    .unwrap_or(false);
    if siblings {
        return;
    }
    let state = state.clone();
    async move {
        let staff = match crate::push::accounts_who_can(
            &state,
            &[crate::moderation::role::flag::MANAGE_REPORTS],
        )
        .await
        {
            Ok(staff) => staff,
            Err(error) => {
                tracing::warn!(%error, "could not list staff for a report");
                return;
            }
        };
        let report = sqlx::query!(
            r#"SELECT r.account_id, ra.username AS reporter_username, ra.domain AS reporter_domain,
                      ta.username AS target_username, ta.domain AS target_domain
               FROM reports r
               JOIN accounts ra ON ra.id = r.account_id
               JOIN accounts ta ON ta.id = r.target_account_id
               WHERE r.id = $1"#,
            report_id,
        )
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
        let Some(report) = report else {
            return;
        };
        for staff_id in staff {
            crate::push::notify_local(
                &state,
                staff_id,
                "admin.report",
                "Report",
                report_id,
                report.account_id,
            )
            .await;
            let Ok(Some(recipient)) = sqlx::query!(
                "SELECT email, settings FROM users WHERE account_id = $1",
                staff_id
            )
            .fetch_optional(&state.db)
            .await
            else {
                continue;
            };
            // `allows_report_emails?`
            if !crate::accounts::user_setting_bool(
                recipient.settings.as_deref(),
                "notification_emails.report",
                true,
            ) {
                continue;
            }
            let acct = |username: &str, domain: &Option<String>| match domain {
                Some(d) => format!("{username}@{d}"),
                None => username.to_owned(),
            };
            let target = acct(&report.target_username, &report.target_domain);
            // `AdminMailer#new_report`: a local reporter by name, a remote
            // one by its server.
            let (reporter, reporter_domain) = match &report.reporter_domain {
                None => (Some(report.reporter_username.clone()), None),
                Some(d) => (None, Some(d.clone())),
            };
            if let Err(error) = state
                .mailer()
                .send_new_report(
                    &recipient.email,
                    &state.instance.domain,
                    report_id,
                    reporter.as_deref(),
                    reporter_domain.as_deref(),
                    &target,
                )
                .await
            {
                tracing::warn!(%error, "could not mail staff about a report");
            }
        }
    }
    .await;
}

/// `forward_to_origin!` and `forward_to_replied_to!`: a `Flag` from the
/// instance actor to the target's server, and to the servers of the accounts
/// the reported posts reply to, among `forward_to_domains`.
async fn forward_report(
    state: &AppState,
    report_id: i64,
    target: &Account,
    status_ids: &[i64],
    to_origin: bool,
    forward_to_domains: &[String],
) -> anyhow::Result<()> {
    let report = sqlx::query!("SELECT uri, comment FROM reports WHERE id = $1", report_id)
        .fetch_one(&state.db)
        .await?;
    let domain = &state.instance.domain;
    let mut object = vec![json!(target.stored_uri().unwrap_or_default())];
    let statuses = sqlx::query!(
        r#"SELECT s.id, s.uri, a.domain, a.id AS account_id, a.id_scheme, a.username
           FROM statuses s JOIN accounts a ON a.id = s.account_id
           WHERE s.id = ANY($1) ORDER BY s.id"#,
        status_ids,
    )
    .fetch_all(&state.db)
    .await?;
    for s in statuses {
        let uri = match (s.domain, s.uri) {
            (Some(_), Some(uri)) => uri,
            _ => crate::federation::tag::status_uri(
                domain,
                s.account_id,
                s.id_scheme,
                &s.username,
                s.id,
            ),
        };
        object.push(json!(uri));
    }
    let collections = sqlx::query_scalar!(
        "SELECT c.uri FROM collection_reports cr JOIN collections c ON c.id = cr.collection_id WHERE cr.report_id = $1",
        report_id,
    )
    .fetch_all(&state.db)
    .await?;
    for uri in collections.into_iter().flatten() {
        object.push(json!(uri));
    }
    let actor = crate::federation::instance_actor::actor_url(domain);
    let flag = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": report.uri.unwrap_or_else(|| crate::federation::relationships::generate_uri(domain)),
        "type": "Flag",
        "actor": actor,
        "content": report.comment,
        "object": object,
    });
    // `Account.representative` signs, and must have keys.
    crate::federation::instance_actor::get_or_create(state).await?;
    let key_id = crate::federation::instance_actor::key_id(domain);

    if to_origin && !target.inbox_url.is_empty() {
        crate::federation::delivery::deliver_to_inboxes(
            state,
            flag.clone(),
            vec![target.inbox_url.clone()],
            key_id.clone(),
        )
        .await?;
    }

    let inboxes: Vec<String> = sqlx::query_scalar!(
        r#"SELECT DISTINCT COALESCE(NULLIF(a.shared_inbox_url, ''), a.inbox_url) AS "inbox!"
           FROM accounts a
           WHERE a.domain = ANY($1) AND a.protocol = 1
             AND a.id IN (SELECT s.in_reply_to_account_id FROM statuses s
                          WHERE s.id = ANY($2) AND s.in_reply_to_account_id IS NOT NULL)"#,
        forward_to_domains,
        status_ids,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .filter(|inbox| {
        !inbox.is_empty() && *inbox != target.inbox_url && *inbox != target.shared_inbox_url
    })
    .collect();
    if !inboxes.is_empty() {
        crate::federation::delivery::deliver_to_inboxes(state, flag, inboxes, key_id).await?;
    }
    Ok(())
}
