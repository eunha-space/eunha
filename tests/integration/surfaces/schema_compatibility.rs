//! The claim eunha is built on: that its database is one a Mastodon could be
//! pointed at.
//!
//! Every test context runs the migrations against a fresh database, so one of
//! them can be read back and compared against a reference: the structure of a
//! database that Mastodon's own ActiveRecord built from its `db/schema.rb`,
//! recorded by `scripts/build_mastodon_schema.sh`.
//!
//! Recorded rather than parsed, so that nothing of ours stands between the
//! comparison and the truth — and recorded rather than rebuilt, so this needs
//! neither Ruby nor the network. That the vendored `schema.rb` still matches
//! upstream is what `mise run mastodon:status` answers.

use crate::helpers::TestContext;

/// A migrated database is structurally the schema upstream builds — every
/// table, column, type, nullability, index and foreign key the checker knows
/// how to compare.
#[tokio::test]
async fn test_migrations_produce_the_tracked_mastodon_schema() {
    let ctx = TestContext::new("schema-compat").await;

    let live = eunha::schema_check::introspect(&ctx.db)
        .await
        .expect("introspect the migrated database");
    let expected = eunha::upstream::reference_schema();

    let findings = eunha::schema_check::diff(&live, &expected);
    assert!(
        findings.is_empty(),
        "migrations drifted from Mastodon {}:\n{}",
        eunha::version::MASTODON,
        findings
            .iter()
            .map(|f| format!("  {f}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The recorded reference is a whole Mastodon schema, not a truncated or empty
/// one — which is what a comparison against it would otherwise silently pass.
#[tokio::test]
async fn test_the_reference_is_a_whole_schema() {
    let reference = eunha::upstream::reference_schema();
    assert!(
        reference.tables.len() > 100,
        "the recorded reference looks truncated: {} tables",
        reference.tables.len()
    );
    assert!(
        reference.constraints.len() > 500,
        "the recorded reference has only {} constraints",
        reference.constraints.len()
    );
    assert!(
        reference.constraints.iter().any(|c| c.kind == "f"),
        "no foreign keys were recorded"
    );
    assert!(
        reference.constraints.iter().any(|c| c.kind == "n"),
        "no not-null constraints were recorded"
    );
    assert!(
        !reference.views.is_empty(),
        "Mastodon's views were not recorded"
    );
    // Recorded from a real database, so defaults and index definitions came
    // with it; a reference missing those would compare on less than it claims.
    let accounts = reference.tables.get("accounts").expect("accounts");
    assert!(
        accounts.columns.values().any(|c| c.default.is_some()),
        "no column defaults were recorded"
    );
    assert!(
        accounts.indexes.values().all(|i| !i.definition.is_empty()),
        "an index was recorded without its definition"
    );
}

/// `public.schema_migrations` names the Mastodon migrations this schema was
/// built by, so a database can say what it is without being interrogated —
/// and so a Mastodon booted on it does not try to re-run them.
#[tokio::test]
async fn test_the_database_reports_the_schema_version_it_is_at() {
    let ctx = TestContext::new("schema-version").await;

    let max: Option<String> =
        sqlx::query_scalar("SELECT max(version) FROM public.schema_migrations")
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(
        max.as_deref(),
        Some(eunha::version::MASTODON_SCHEMA),
        "the ledger should end at the release eunha tracks"
    );

    let environment: Option<String> = sqlx::query_scalar(
        "SELECT value FROM public.ar_internal_metadata WHERE key = 'environment'",
    )
    .fetch_optional(&ctx.db)
    .await
    .unwrap()
    .flatten();
    assert!(
        environment.is_some(),
        "Rails checks ar_internal_metadata before it will touch a database"
    );
}
