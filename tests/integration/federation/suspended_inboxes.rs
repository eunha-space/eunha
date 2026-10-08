//! Deliveries to a domain blocked at suspend severity are dropped, the
//! domain matched as `DomainBlock.rule_for` matches an account's: port and
//! all.

use serde_json::json;

use crate::helpers::TestContext;

async fn queued(ctx: &TestContext) -> Vec<String> {
    let mut inboxes: Vec<String> = sqlx::query_scalar(
        "SELECT payload->>'inbox' FROM eunha.ojak_queue
         WHERE queue IN ('delivery', 'delivery-priority')",
    )
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    inboxes.sort();
    inboxes
}

/// A block on a host covers its subdomains but not a server at a port of
/// it, and a block on a server at a port covers that server alone.
#[tokio::test]
async fn test_a_suspended_domain_is_not_delivered_to_port_and_all() {
    let ctx = TestContext::new("suspended-inboxes").await;
    let (priv_pem, pub_pem) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    sqlx::query(
        "UPDATE accounts SET private_key = $1, public_key = $2 WHERE username = 'alice' AND domain IS NULL",
    )
    .bind(&priv_pem)
    .bind(&pub_pem)
    .execute(&ctx.db)
    .await
    .unwrap();
    for domain in ["blocked.test", "ported.test:8443"] {
        sqlx::query(
            "INSERT INTO domain_blocks (domain, severity, created_at, updated_at)
             VALUES ($1, 1, now(), now())",
        )
        .bind(domain)
        .execute(&ctx.db)
        .await
        .unwrap();
    }
    let actor = format!("https://{}/users/alice", ctx.domain);
    let inboxes = [
        "https://blocked.test/inbox",
        "https://a.blocked.test/inbox",
        "https://blocked.test:8443/inbox",
        "https://ported.test:8443/inbox",
        "https://ported.test/inbox",
    ];
    eunha::federation::delivery::deliver_to_inboxes(
        &ctx.state,
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{actor}#test"),
            "type": "Update",
            "actor": actor,
            "object": actor,
        }),
        inboxes.iter().map(|i| (*i).to_owned()).collect(),
        format!("{actor}#main-key"),
    )
    .await
    .unwrap();
    assert_eq!(
        queued(&ctx).await,
        [
            "https://blocked.test:8443/inbox",
            "https://ported.test/inbox"
        ]
    );
}
