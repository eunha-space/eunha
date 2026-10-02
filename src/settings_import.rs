//! `eunha settings import-config`: copy what the instance configuration said
//! about the site into the tables Mastodon reads it from, once.
//!
//! Before eunha read Mastodon's `settings` and `terms_of_services` tables it
//! took the site's identity, registrations, terms of service and privacy
//! policy from the instance configuration, and served them as defaults until
//! an administrator saved the settings. It now reads only the tables, as
//! Mastodon does, so an instance upgrading runs this once to keep what it
//! served. Nothing already saved is overwritten, and running it again
//! changes nothing.
//!
//! `eunha migrate` runs it by itself, once, for a database that was serving
//! when migration 023 was applied ([`import_if_owed`]), so that no deploy has
//! to remember it.

use serde_yaml::Value;
use sqlx::PgPool;

use crate::config::InstanceConfig;
use crate::settings::RegistrationsMode;

/// What [`import_config`] wrote, and what it left because it was saved.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Settings written, by `var`.
    pub written: Vec<String>,
    /// Settings the configuration had a value for that were already saved.
    pub kept: Vec<String>,
    /// Whether the configured terms of service were published.
    pub terms_published: bool,
}

/// Write the configuration's site settings, privacy policy and terms of
/// service where nothing is saved yet. With `dry_run`, only report.
///
/// # Errors
///
/// When the database cannot be read or written.
pub async fn import_config(
    db: &PgPool,
    instance: &InstanceConfig,
    dry_run: bool,
) -> anyhow::Result<Report> {
    let mut report = Report::default();
    let mut tx = db.begin().await?;

    let short_description = if instance.short_description.trim().is_empty() {
        &instance.description
    } else {
        &instance.short_description
    };
    let mut settings: Vec<(&str, Value)> = vec![
        ("site_title", Value::from(instance.title.clone())),
        (
            "site_short_description",
            Value::from(short_description.clone()),
        ),
        (
            "site_extended_description",
            Value::from(instance.description.clone()),
        ),
        (
            "site_description",
            Value::from(instance.description.clone()),
        ),
        (
            "site_contact_email",
            Value::from(instance.contact_email.clone().unwrap_or_default()),
        ),
        ("site_terms", Value::from(instance.privacy_policy.clone())),
    ];
    settings.retain(|(_, value)| value.as_str().is_some_and(|v| !v.trim().is_empty()));
    settings.push((
        "registrations_mode",
        Value::from(RegistrationsMode::from_config(instance).as_str()),
    ));
    // The contact account eunha showed while none was saved: the local
    // account with the highest role.
    let contact: Option<String> = sqlx::query_scalar(
        "SELECT a.username FROM accounts a
         JOIN users u ON u.account_id = a.id
         LEFT JOIN user_roles ur ON ur.id = u.role_id
         WHERE a.domain IS NULL
         ORDER BY COALESCE(ur.position, 0) DESC, a.created_at ASC
         LIMIT 1",
    )
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(contact) = contact {
        settings.push(("site_contact_username", Value::from(contact)));
    }

    for (var, value) in settings {
        let saved: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM settings WHERE var = $1)")
                .bind(var)
                .fetch_one(&mut *tx)
                .await?;
        if saved {
            report.kept.push(var.to_owned());
            continue;
        }
        if !dry_run {
            crate::settings::set_in(&mut *tx, var, value).await?;
        }
        report.written.push(var.to_owned());
    }

    // The configured terms, as the published version eunha served them as:
    // effective on 2025-01-01, nobody to be told of them.
    if !instance.terms_of_service.trim().is_empty() {
        let published: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM terms_of_services WHERE published_at IS NOT NULL)",
        )
        .fetch_one(&mut *tx)
        .await?;
        if !published {
            let date = crate::terms_of_service::config_effective_date();
            let midnight = date.and_hms_opt(0, 0, 0).expect("valid time");
            if !dry_run {
                sqlx::query(
                    "INSERT INTO terms_of_services
                       (text, changelog, published_at, notification_sent_at, effective_date,
                        created_at, updated_at)
                     VALUES ($1, '', $2, $2, $3, now(), now())",
                )
                .bind(&instance.terms_of_service)
                .bind(midnight)
                .bind(date)
                .execute(&mut *tx)
                .await?;
            }
            report.terms_published = true;
        }
    }

    if dry_run {
        tx.rollback().await?;
    } else {
        // Whatever `eunha migrate` still owed this database, this was it.
        sqlx::query("DELETE FROM eunha.site_settings_import")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    Ok(report)
}

/// `eunha migrate`'s share of [`import_config`]: run it if the database
/// still owes it — it was serving when migration 023 was applied, and has not
/// been imported into since — and say what it wrote; `None` when nothing was
/// owed.
///
/// # Errors
///
/// When the database cannot be read or written.
pub async fn import_if_owed(
    db: &PgPool,
    instance: &InstanceConfig,
) -> anyhow::Result<Option<Report>> {
    if !owed(db).await? {
        return Ok(None);
    }
    import_config(db, instance, false).await.map(Some)
}

/// Whether `eunha migrate` still owes this database [`import_config`].
///
/// # Errors
///
/// When the database cannot be read.
pub async fn owed(db: &PgPool) -> anyhow::Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM eunha.site_settings_import)")
            .fetch_one(db)
            .await?,
    )
}
