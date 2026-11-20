//! Whether the configured ActiveRecord encryption keys open what the database
//! holds.
//!
//! Nothing else notices keys that do not: an instance imported with fresh
//! secrets rather than its Mastodon's starts, serves, and signs with every key
//! it encrypted itself, while each member who set up TOTP under Mastodon is
//! told their code is wrong. So a few values of each of Mastodon's encrypted
//! columns are opened when a tenant starts and when `eunha migrate` runs: the
//! oldest, which are the likeliest to have been sealed by keys from before,
//! and the newest. A failure is reported as an error with `kind =
//! "encryption_keys"` ([`crate::error_reporting`]).
//!
//! Mastodon: none

use sqlx::PgPool;

use crate::rails_encryption::Encryptor;

/// Values opened from each end of each column.
const SAMPLE: i64 = 5;

/// What one encrypted column's sample came to.
#[derive(Debug, PartialEq, Eq)]
pub struct ColumnCheck {
    /// `table.column`.
    pub column: &'static str,
    /// Values tried.
    pub checked: usize,
    /// Values the keys did not open.
    pub failed: usize,
}

/// Mastodon's encrypted columns, and the rows to sample each from.
const COLUMNS: &[(&str, &str)] = &[
    (
        "keypairs.private_key",
        "SELECT private_key FROM keypairs WHERE private_key <> '' AND id IN (
             (SELECT id FROM keypairs WHERE private_key <> '' ORDER BY id LIMIT $1)
             UNION
             (SELECT id FROM keypairs WHERE private_key <> '' ORDER BY id DESC LIMIT $1))",
    ),
    (
        "users.otp_secret",
        "SELECT otp_secret FROM users WHERE otp_secret <> '' AND id IN (
             (SELECT id FROM users WHERE otp_secret <> '' ORDER BY id LIMIT $1)
             UNION
             (SELECT id FROM users WHERE otp_secret <> '' ORDER BY id DESC LIMIT $1))",
    ),
];

/// Open a sample of every encrypted column with `encryptor`.
pub async fn check(db: &PgPool, encryptor: &Encryptor) -> sqlx::Result<Vec<ColumnCheck>> {
    let mut checks = Vec::with_capacity(COLUMNS.len());
    for (column, query) in COLUMNS {
        let sealed: Vec<String> = sqlx::query_scalar(*query)
            .bind(SAMPLE)
            .fetch_all(db)
            .await?;
        let failed = sealed
            .iter()
            .filter(|value| encryptor.decrypt(value).is_err())
            .count();
        checks.push(ColumnCheck {
            column,
            checked: sealed.len(),
            failed,
        });
    }
    Ok(checks)
}

/// [`check`], reporting each column the keys failed on, and returning what
/// to tell someone running a command.
pub async fn check_and_report(db: &PgPool, encryptor: &Encryptor) -> Vec<String> {
    let checks = match check(db, encryptor).await {
        Ok(checks) => checks,
        Err(error) => {
            tracing::warn!(%error, "could not check the ActiveRecord encryption keys");
            return Vec::new();
        }
    };
    checks
        .into_iter()
        .filter(|check| check.failed > 0)
        .map(|check| {
            tracing::error!(
                kind = "encryption_keys",
                column = check.column,
                checked = check.checked,
                failed = check.failed,
                "the configured ActiveRecord encryption keys do not open what the database \
                 holds; were they copied from the Mastodon this database came from?"
            );
            format!(
                "The configured ActiveRecord encryption keys did not open {} of {} sampled \
                 value(s) in `{}`. Were they copied from the Mastodon this database came from? \
                 See docs/operating/importing.md.",
                check.failed, check.checked, check.column
            )
        })
        .collect()
}
