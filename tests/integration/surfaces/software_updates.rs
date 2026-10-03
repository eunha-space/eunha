//! Notices about the Mastodon release eunha implements.

use axum::{extract::State, routing::any, Router};
use std::sync::Arc;

use crate::helpers::TestContext;

/// Stand in for the update server, answering with a fixed body, on the
/// loopback network the guarded client reaches only when allowed, as
/// Mastodon's `Request` does.
async fn spawn_update_server(body: &'static str) -> String {
    let app = Router::new()
        .fallback(any(|State(body): State<Arc<&'static str>>| async move {
            ([("content-type", "application/json")], body.to_string())
        }))
        .with_state(Arc::new(body));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}/update-check")
}

/// Available releases are recorded where the admin surfaces read them.
#[tokio::test]
async fn test_available_updates_are_recorded() {
    let ctx = TestContext::reaching_loopback("sw-updates").await;
    let url = spawn_update_server(
        r#"{"updatesAvailable":[
             {"version":"4.8.0","releaseNotes":"https://example/4.8.0","urgent":false,"type":"minor"},
             {"version":"4.7.1","releaseNotes":"https://example/4.7.1","urgent":true,"type":"patch"}
           ],"currentVersion":{"endOfSupport":null}}"#,
    )
    .await;

    eunha::software_updates::check_once(&ctx.state, &url)
        .await
        .expect("the check should succeed");

    let rows = sqlx::query!(
        "SELECT version, urgent, type, release_notes FROM software_updates ORDER BY version",
    )
    .fetch_all(&ctx.db)
    .await
    .unwrap();

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].version, "4.7.1");
    assert!(rows[0].urgent, "a patch marked urgent should stay urgent");
    assert_eq!(rows[0].r#type, 0, "patch");
    assert_eq!(rows[1].version, "4.8.0");
    assert_eq!(rows[1].r#type, 1, "minor");
    assert_eq!(rows[1].release_notes, "https://example/4.8.0");
}

/// A release the server stops listing stops being advertised, so an update
/// that has been taken up (or withdrawn) does not linger.
#[tokio::test]
async fn test_withdrawn_updates_are_dropped() {
    let ctx = TestContext::reaching_loopback("sw-withdrawn").await;

    sqlx::query!(
        r#"INSERT INTO software_updates (version, urgent, type, release_notes, created_at, updated_at)
           VALUES ('4.0.0', false, 0, '', now(), now())"#,
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    let url = spawn_update_server(
        r#"{"updatesAvailable":[{"version":"4.8.0","releaseNotes":"","urgent":false,"type":"minor"}],
            "currentVersion":{"endOfSupport":null}}"#,
    )
    .await;
    eunha::software_updates::check_once(&ctx.state, &url)
        .await
        .unwrap();

    let versions: Vec<String> =
        sqlx::query_scalar("SELECT version FROM software_updates ORDER BY version")
            .fetch_all(&ctx.db)
            .await
            .unwrap();
    assert_eq!(versions, vec!["4.8.0".to_string()]);
}

/// An end-of-support date for the branch eunha implements is recorded against
/// that branch, with the warning its nearness has earned.
#[tokio::test]
async fn test_end_of_support_is_recorded_for_the_tracked_branch() {
    let ctx = TestContext::reaching_loopback("sw-eol").await;
    let url = spawn_update_server(
        r#"{"updatesAvailable":[],"currentVersion":{"endOfSupport":"2020-01-01"}}"#,
    )
    .await;

    eunha::software_updates::check_once(&ctx.state, &url)
        .await
        .unwrap();

    let row =
        sqlx::query!("SELECT branch, end_of_support, warning_issued FROM software_deprecations")
            .fetch_one(&ctx.db)
            .await
            .unwrap();

    let expected_branch: String = eunha::version::MASTODON
        .split('.')
        .take(2)
        .collect::<Vec<_>>()
        .join(".");
    assert_eq!(row.branch, expected_branch);
    assert_eq!(
        row.end_of_support,
        chrono::NaiveDate::from_ymd_opt(2020, 1, 1).unwrap()
    );
    assert_eq!(
        row.warning_issued, 3,
        "a date in the past is out of support"
    );
}

/// A branch eunha no longer implements is cleared out, so the table describes
/// this build rather than whatever came before it.
#[tokio::test]
async fn test_stale_branches_are_cleared() {
    let ctx = TestContext::reaching_loopback("sw-stale").await;

    sqlx::query!(
        r#"INSERT INTO software_deprecations (branch, end_of_support, warning_issued, created_at, updated_at)
           VALUES ('3.5', '2023-01-01', 3, now(), now())"#,
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    let url = spawn_update_server(
        r#"{"updatesAvailable":[],"currentVersion":{"endOfSupport":"2099-01-01"}}"#,
    )
    .await;
    eunha::software_updates::check_once(&ctx.state, &url)
        .await
        .unwrap();

    let branches: Vec<String> =
        sqlx::query_scalar("SELECT branch FROM software_deprecations ORDER BY branch")
            .fetch_all(&ctx.db)
            .await
            .unwrap();
    assert_eq!(branches.len(), 1);
    assert_ne!(branches[0], "3.5");

    let warning: i32 =
        sqlx::query_scalar("SELECT warning_issued FROM software_deprecations LIMIT 1")
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(warning, 0, "a date far off has earned no warning");
}

/// A release already recorded is not mailed about again: the check runs every
/// few hours and nobody wants that in their inbox.
#[tokio::test]
async fn test_only_newly_seen_releases_are_reported() {
    let ctx = TestContext::reaching_loopback("sw-repeat").await;
    let body = r#"{"updatesAvailable":[
             {"version":"4.8.0","releaseNotes":"","urgent":true,"type":"minor"}
           ],"currentVersion":{"endOfSupport":null}}"#;
    let url = spawn_update_server(body).await;

    eunha::software_updates::check_once(&ctx.state, &url)
        .await
        .unwrap();
    let first: i64 = sqlx::query_scalar("SELECT count(*) FROM software_updates")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(first, 1);

    // Running again records the same row and finds nothing new to report.
    eunha::software_updates::check_once(&ctx.state, &url)
        .await
        .unwrap();
    let second: i64 = sqlx::query_scalar("SELECT count(*) FROM software_updates")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(second, 1, "a repeat check should not duplicate the row");

    let updated_at: chrono::NaiveDateTime =
        sqlx::query_scalar("SELECT updated_at FROM software_updates WHERE version = '4.8.0'")
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert!(
        updated_at <= chrono::Utc::now().naive_utc(),
        "the row should still be refreshed on every check"
    );
}

/// The end-of-support warning level only ever rises, so crossing a threshold
/// is noted once rather than every time the check runs.
#[tokio::test]
async fn test_a_warning_is_not_reissued() {
    let ctx = TestContext::reaching_loopback("sw-warn-once").await;
    let url = spawn_update_server(
        r#"{"updatesAvailable":[],"currentVersion":{"endOfSupport":"2020-01-01"}}"#,
    )
    .await;

    eunha::software_updates::check_once(&ctx.state, &url)
        .await
        .unwrap();
    eunha::software_updates::check_once(&ctx.state, &url)
        .await
        .unwrap();

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM software_deprecations")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(rows, 1);

    let warning: i32 =
        sqlx::query_scalar("SELECT warning_issued FROM software_deprecations LIMIT 1")
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(warning, 3, "out of support, and it stays there");
}

/// A date that moves further out does not withdraw a warning already given.
#[tokio::test]
async fn test_a_later_date_does_not_lower_the_warning() {
    let ctx = TestContext::reaching_loopback("sw-warn-keep").await;

    let near = spawn_update_server(
        r#"{"updatesAvailable":[],"currentVersion":{"endOfSupport":"2020-01-01"}}"#,
    )
    .await;
    eunha::software_updates::check_once(&ctx.state, &near)
        .await
        .unwrap();

    let far = spawn_update_server(
        r#"{"updatesAvailable":[],"currentVersion":{"endOfSupport":"2099-01-01"}}"#,
    )
    .await;
    eunha::software_updates::check_once(&ctx.state, &far)
        .await
        .unwrap();

    let row = sqlx::query!("SELECT end_of_support, warning_issued FROM software_deprecations")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(
        row.end_of_support,
        chrono::NaiveDate::from_ymd_opt(2099, 1, 1).unwrap(),
        "the date itself follows the server"
    );
    assert_eq!(
        row.warning_issued, 3,
        "but a warning given is not withdrawn"
    );
}

/// A process asks the update server once however many instances it serves —
/// the question is the same for all of them — and records the answer in each of
/// their databases, which is where their own administrators read it.
#[tokio::test]
async fn test_one_request_serves_every_instance() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let asked = Arc::new(AtomicUsize::new(0));
    let counted = asked.clone();
    let app = Router::new().fallback(any(move || {
        let asked = counted.clone();
        async move {
            asked.fetch_add(1, Ordering::SeqCst);
            (
                [("content-type", "application/json")],
                r#"{"updatesAvailable":[
                     {"version":"4.8.0","releaseNotes":"https://example/4.8.0","urgent":false,"type":"minor"}
                   ],"currentVersion":{"endOfSupport":null}}"#
                    .to_string(),
            )
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let url = format!("http://{addr}/update-check");

    let a = TestContext::reaching_loopback("sw-shared-a").await;
    let b = TestContext::reaching_loopback("sw-shared-b").await;
    eunha::software_updates::check_once_for(&[a.state.clone(), b.state.clone()], &url)
        .await
        .expect("the check should succeed");

    assert_eq!(
        asked.load(Ordering::SeqCst),
        1,
        "both instances were served by one request"
    );
    for ctx in [&a, &b] {
        let rows = sqlx::query!(
            "SELECT version, urgent, type, release_notes FROM software_updates ORDER BY version",
        )
        .fetch_all(&ctx.db)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1, "{} recorded the answer", ctx.domain);
        assert_eq!(rows[0].version, "4.8.0");
    }
}
