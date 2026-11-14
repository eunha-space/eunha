//! Staff alert digests: the first alert of a quiet hour is mailed at once,
//! what follows within the hour goes out as one summary, and a summary opens
//! the next hour's window.

use eunha::admin_digest::{self, Alert};

use crate::helpers::{make_admin, TestContext};

const STAFF: &str = "alice@test.invalid";

fn report(id: i64) -> Alert {
    Alert::Report {
        report_id: id,
        reporter: Some("bob".into()),
        reporter_domain: None,
        target: format!("spammer{id}"),
    }
}

async fn digest_jobs(ctx: &TestContext) -> Vec<eunha::jobs::Queued> {
    eunha::jobs::queued(&ctx.state, "Eunha::AdminDigestWorker")
        .await
        .unwrap()
        .into_iter()
        .filter(|job| !job.dead)
        .collect()
}

/// Run the summary that is waiting, as though its hour had passed.
async fn close_window(ctx: &TestContext, alice: i64) {
    let mut redis = ctx.state.redis_coordination.clone();
    let _: () = redis::cmd("DEL")
        .arg(
            ctx.state
                .redis_keys
                .key(format!("admin_digest:{alice}:window")),
        )
        .query_async(&mut redis)
        .await
        .unwrap();
    eunha::jobs::make_due(&ctx.state).await.unwrap();
    eunha::jobs::drain(&ctx.state).await.unwrap();
}

/// Raise an alert, and send what it mailed: mail waits in the queue too.
async fn alert(ctx: &TestContext, account_id: i64, to: &str, alert: Alert) {
    admin_digest::alert(&ctx.state, account_id, to, alert).await;
    eunha::jobs::drain(&ctx.state).await.unwrap();
}

fn summaries(ctx: &TestContext) -> Vec<eunha::email::SentMail> {
    ctx.sent_to(STAFF)
        .into_iter()
        .filter(|mail| mail.subject.starts_with("New for review on"))
        .collect()
}

#[tokio::test]
async fn first_alert_is_mailed_and_the_rest_wait_for_the_summary() {
    let ctx = TestContext::new("admin-digest").await;
    // Jobs wait until due, so that a summary runs when its window closes.
    ctx.state.jobs.set_mode(eunha::jobs::Mode::Durable);
    let alice: i64 = ctx.alice_id.parse().unwrap();
    make_admin(&ctx.db, alice).await;

    // The first alert goes out as Mastodon sends it, and opens a window
    // whose summary is due a little after an hour.
    alert(&ctx, alice, STAFF, report(1)).await;
    let first = format!("New report for {} (#1)", ctx.domain);
    assert!(ctx.mail_to(STAFF, &first).await.is_some());
    let jobs = digest_jobs(&ctx).await;
    assert_eq!(jobs.len(), 1);
    assert!(jobs[0].seconds_until_due > 3600.0 && jobs[0].seconds_until_due <= 3660.0);

    // What follows within the hour is held, not mailed.
    alert(&ctx, alice, STAFF, report(2)).await;
    alert(
        &ctx,
        alice,
        STAFF,
        Alert::PendingAccount {
            account_id: 99,
            username: "newcomer".into(),
            invite_request: Some("I garden & write".into()),
        },
    )
    .await;
    assert_eq!(
        admin_digest::held(&ctx.state, alice).await,
        vec![
            report(2),
            Alert::PendingAccount {
                account_id: 99,
                username: "newcomer".into(),
                invite_request: Some("I garden & write".into()),
            },
        ]
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(ctx.sent_to(STAFF).len(), 1, "only the first was mailed");
    assert_eq!(digest_jobs(&ctx).await.len(), 1, "one summary per window");

    // When the window closes, the held alerts go out as one summary, and
    // the summary opens the next window.
    close_window(&ctx, alice).await;
    let subject = format!(
        "New for review on {}: 1 report, 1 account up for review",
        ctx.domain
    );
    let summary = ctx.mail_to(STAFF, &subject).await.expect("the summary");
    assert!(summary.html.is_empty(), "admin mail is text alone");
    assert!(summary
        .text
        .contains("Report #2: bob has reported spammer2"));
    assert!(summary.text.contains("/admin/reports/2"));
    assert!(summary.text.contains("newcomer"));
    assert!(summary.text.contains("I garden & write"));
    assert!(summary.text.contains("/admin/accounts/99"));
    assert!(admin_digest::held(&ctx.state, alice).await.is_empty());
    assert_eq!(
        digest_jobs(&ctx).await.len(),
        1,
        "the next window's summary"
    );

    // An alert in that next window is held for its summary too.
    alert(&ctx, alice, STAFF, report(3)).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(ctx
        .sent_to(STAFF)
        .iter()
        .all(|mail| !mail.subject.contains("(#3)")));
    close_window(&ctx, alice).await;
    let subject = format!("New for review on {}: 1 report", ctx.domain);
    assert!(ctx.mail_to(STAFF, &subject).await.is_some());

    // An hour with nothing held ends quietly, and opens no further window:
    // the next alert is mailed at once again.
    close_window(&ctx, alice).await;
    assert!(digest_jobs(&ctx).await.is_empty());
    assert_eq!(summaries(&ctx).len(), 2);
    alert(&ctx, alice, STAFF, report(4)).await;
    let fourth = format!("New report for {} (#4)", ctx.domain);
    assert!(ctx.mail_to(STAFF, &fourth).await.is_some());
}

#[tokio::test]
async fn each_staff_member_has_a_window_of_their_own() {
    let ctx = TestContext::new("admin-digest-staff").await;
    ctx.state.jobs.set_mode(eunha::jobs::Mode::Durable);
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let bob: i64 = ctx.bob_id.parse().unwrap();
    let bob_email = "bob@test.invalid";

    alert(&ctx, alice, STAFF, report(1)).await;
    alert(&ctx, bob, bob_email, report(1)).await;
    let first = format!("New report for {} (#1)", ctx.domain);
    assert!(ctx.mail_to(STAFF, &first).await.is_some());
    assert!(ctx.mail_to(bob_email, &first).await.is_some());
    assert_eq!(digest_jobs(&ctx).await.len(), 2);
}

/// Reports filed through the API reach staff through the digest: the first
/// is mailed, the next is held for the summary.
#[tokio::test]
async fn reports_go_through_the_digest() {
    let ctx = TestContext::new("admin-digest-reports").await;
    ctx.state.jobs.set_mode(eunha::jobs::Mode::Durable);
    let alice: i64 = ctx.alice_id.parse().unwrap();
    make_admin(&ctx.db, alice).await;
    let (_, carol_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    let (dave, _) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "dave", "dave@test.invalid").await;
    for target in [ctx.bob_id.clone(), dave.to_string()] {
        let resp = ctx
            .api
            .post_json(
                "/api/v1/reports",
                Some(&carol_token),
                &serde_json::json!({ "account_id": target, "comment": "spam" }),
            )
            .await;
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        eunha::jobs::drain(&ctx.state).await.unwrap();
    }
    let reports = |ctx: &TestContext| {
        ctx.sent_to(STAFF)
            .into_iter()
            .filter(|mail| mail.subject.starts_with("New report for"))
            .count()
    };
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    eunha::jobs::drain(&ctx.state).await.unwrap();
    assert_eq!(reports(&ctx), 1);
    let held = admin_digest::held(&ctx.state, alice).await;
    assert!(
        matches!(held.as_slice(), [Alert::Report { target, .. }] if target == "dave"),
        "{held:?}"
    );
}
