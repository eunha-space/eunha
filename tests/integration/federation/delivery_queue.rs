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
