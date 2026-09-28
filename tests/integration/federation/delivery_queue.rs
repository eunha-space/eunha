//! The delivery queue in `eunha.ojak_queue`, as eunha's migrations build it.

use crate::helpers::TestContext;

/// The table migration 011 builds is the one ojak-postgres expects: ojak's
/// checks for a queue backend pass against it.
#[tokio::test]
async fn the_migrated_queue_passes_ojaks_checks() {
    let ctx = TestContext::new("ojak-queue-checks").await;
    let queue = ojak_postgres::PostgresQueue::with_table(ctx.db.clone(), "eunha.ojak_queue")
        .expect("table name");
    ojak::testing::check_queue(&queue).await;
}
