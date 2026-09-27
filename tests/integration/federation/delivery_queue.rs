//! The delivery queue in `eunha.feder_queue`, as eunha's migrations build it.

use crate::helpers::TestContext;

/// The table migration 011 builds is the one feder-postgres expects: feder's
/// checks for a queue backend pass against it.
#[tokio::test]
async fn the_migrated_queue_passes_feders_checks() {
    let ctx = TestContext::new("feder-queue-checks").await;
    let queue = feder_postgres::PostgresQueue::with_table(ctx.db.clone(), "eunha.feder_queue")
        .expect("table name");
    feder::testing::check_queue(&queue).await;
}

/// Deliveries still waiting in the old table move to the new queue when the
/// migration runs, and are marked finished where they were, so that the
/// release before it, started again, does not send them twice.
#[tokio::test]
async fn waiting_deliveries_move_to_the_new_queue() {
    let ctx = TestContext::new("feder-queue-move").await;
    for (inbox, delivered) in [
        ("https://a.invalid/inbox", false),
        ("https://b.invalid/inbox", true),
    ] {
        sqlx::query(
            r#"INSERT INTO eunha.activity_delivery_jobs
                 (activity, inbox_url, key_id, attempts, delivered_at)
               VALUES ('{"type":"Create"}'::jsonb, $1, 'https://eunha.invalid/users/alice#main-key', 3,
                       CASE WHEN $2 THEN now() END)"#,
        )
        .bind(inbox)
        .bind(delivered)
        .execute(&ctx.db)
        .await
        .unwrap();
    }

    // The migration's statements, run again: its CREATEs are idempotent, and
    // what it moves is what is waiting now.
    sqlx::raw_sql(include_str!("../../../migrations/011_feder_queue.sql"))
        .execute(&ctx.db)
        .await
        .unwrap();

    let moved: Vec<(serde_json::Value, i32)> =
        sqlx::query_as("SELECT payload, attempts FROM eunha.feder_queue WHERE queue = 'delivery'")
            .fetch_all(&ctx.db)
            .await
            .unwrap();
    assert_eq!(moved.len(), 1, "only the waiting delivery moves");
    assert_eq!(moved[0].0["inbox"], "https://a.invalid/inbox");
    assert_eq!(
        moved[0].0["sender"],
        "https://eunha.invalid/users/alice#main-key"
    );
    assert_eq!(moved[0].0["activity"]["type"], "Create");
    assert_eq!(moved[0].1, 3, "its attempts come with it");

    let still_waiting: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM eunha.activity_delivery_jobs
         WHERE delivered_at IS NULL AND failed_at IS NULL",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        still_waiting, 0,
        "nothing is left for the old queue to send"
    );
}
