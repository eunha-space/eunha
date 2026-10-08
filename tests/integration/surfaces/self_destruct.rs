//! Self-destruct mode: `SelfDestructHelper`, `check_self_destruct!` and
//! `Scheduler::SelfDestructScheduler` (docs/operating/self-destruct.md).

use reqwest::StatusCode;
use serde_json::Value;

use crate::helpers::TestContext;

async fn closing(label: &str) -> TestContext {
    TestContext::with_instance_config(label, |instance| {
        instance.self_destruct = Some(eunha::self_destruct::value(instance));
    })
    .await
}

async fn seed_remote(ctx: &TestContext, username: &str, domain: &str) -> String {
    let uri = format!("https://{domain}/users/{username}");
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, inbox_url,
                                 shared_inbox_url, protocol, created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', $4, $4, $4 || '/inbox', $5, 1, now(), now())"#,
    )
    .bind(eunha::snowflake::next_id())
    .bind(username)
    .bind(domain)
    .bind(&uri)
    .bind(format!("https://{domain}/inbox"))
    .execute(&ctx.db)
    .await
    .unwrap();
    format!("https://{domain}/inbox")
}

async fn give_signing_key(ctx: &TestContext, username: &str) {
    let (private, public) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    sqlx::query(
        "UPDATE accounts SET private_key = $1, public_key = $2 WHERE username = $3 AND domain IS NULL",
    )
    .bind(private)
    .bind(public)
    .bind(username)
    .execute(&ctx.db)
    .await
    .unwrap();
}

#[tokio::test]
async fn a_closing_instance_answers_gone_but_lets_members_take_their_data() {
    let ctx = closing("self-destruct-gate").await;

    let resp = ctx.api.get("/api/v1/instance", None).await;
    assert_eq!(resp.status(), StatusCode::GONE);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "Gone");

    let resp = ctx
        .api
        .get(
            "/api/v1/accounts/verify_credentials",
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::GONE);

    let resp = ctx.api.get("/", None).await;
    assert_eq!(resp.status(), StatusCode::GONE);
    assert!(resp.text().await.unwrap().contains(&ctx.domain));

    // An actor fetch is refused too, as Mastodon's ActivityPub controllers
    // are `ApplicationController`s.
    let resp = ctx.api.ap_get("/users/alice", None).await;
    assert_eq!(resp.status(), StatusCode::GONE);

    // Signing in and taking data out stay open.
    let resp = ctx.api.get("/account/login", None).await;
    assert_ne!(resp.status(), StatusCode::GONE);
    let resp = ctx
        .api
        .get("/api/eunha/v1/exports", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = ctx.api.get("/.well-known/nodeinfo", None).await;
    assert_ne!(resp.status(), StatusCode::GONE);
}

#[tokio::test]
async fn a_value_for_another_domain_changes_nothing() {
    let ctx = TestContext::with_instance_config("self-destruct-wrong", |instance| {
        let mut other = instance.clone();
        other.domain = format!("other-{}", instance.domain);
        instance.self_destruct = Some(eunha::self_destruct::value(&other));
    })
    .await;
    assert!(!eunha::self_destruct::enabled(&ctx.state.instance));
    let resp = ctx.api.get("/api/v1/instance", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn each_pass_tells_every_server_that_a_batch_of_accounts_is_gone() {
    let ctx = closing("self-destruct-pass").await;
    assert!(eunha::self_destruct::enabled(&ctx.state.instance));
    give_signing_key(&ctx, "alice").await;
    let inbox = seed_remote(&ctx, "zed", "far.test").await;

    // Bob asked to be deleted; his request goes once his notice is out.
    sqlx::query(
        "UPDATE accounts SET requested_deletion_at = now() WHERE username = 'bob' AND domain IS NULL",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO account_deletion_requests (account_id, created_at, updated_at)
         VALUES ($1, now(), now())",
    )
    .bind(ctx.bob_id.parse::<i64>().unwrap())
    .execute(&ctx.db)
    .await
    .unwrap();

    eunha::self_destruct::perform(&ctx.state).await.unwrap();

    let marked: Vec<(String, bool)> = sqlx::query_as(
        "SELECT username, requested_deletion_at IS NOT NULL FROM accounts
         WHERE domain IS NULL AND username IN ('alice', 'bob') ORDER BY username",
    )
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        marked,
        vec![("alice".to_owned(), true), ("bob".to_owned(), true)]
    );
    let requests: i64 = sqlx::query_scalar("SELECT count(*) FROM account_deletion_requests")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(requests, 0, "the deletion request is removed, not acted on");

    let queued: Vec<Value> = sqlx::query_scalar(
        "SELECT payload->'activity' FROM eunha.ojak_queue
         WHERE queue IN ('delivery', 'delivery-priority') AND payload->>'inbox' = $1",
    )
    .bind(&inbox)
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    let alice = queued
        .iter()
        .find(|a| {
            a["actor"]
                .as_str()
                .is_some_and(|actor| actor.ends_with("/users/alice"))
        })
        .expect("alice's Delete goes to the remote server's shared inbox");
    assert_eq!(alice["type"], "Delete");
    assert_eq!(alice["object"], alice["actor"]);
    assert!(
        alice["signature"].is_object(),
        "signed with a Linked Data Signature"
    );

    // Nothing is left to do, so another pass sends nothing more.
    let before = queued.len();
    eunha::self_destruct::perform(&ctx.state).await.unwrap();
    let after: i64 =
        sqlx::query_scalar("SELECT count(*) FROM eunha.ojak_queue WHERE payload->>'inbox' = $1")
            .bind(&inbox)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(after as usize, before);
}
