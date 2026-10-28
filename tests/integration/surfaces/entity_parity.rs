//! Do Eunha's API entities carry the fields Mastodon's carry?
//!
//! Eunha aims for behavioural parity, and the most mechanical part of that is
//! shape: a client written against Mastodon reads fields by name, so a missing
//! one breaks it and an unexpected one is a divergence nobody decided on.
//!
//! `mastodon/entities.json` records what each of Mastodon 4.7.0's REST
//! serializers emits, split into fields it always includes and fields it
//! includes conditionally. These tests fetch real responses from a running
//! Eunha and compare the two.
//!
//! A missing field is a finding. An extra field is a finding. A conditional
//! field being absent is not — Mastodon omits those too, depending on who is
//! asking.

use std::collections::{BTreeSet, HashMap};

use serde_json::Value;

use crate::helpers::TestContext;

/// What Mastodon's serializer for one entity emits.
struct Definition {
    always: BTreeSet<String>,
    conditional: BTreeSet<String>,
}

fn definitions() -> HashMap<String, Definition> {
    #[derive(serde::Deserialize)]
    struct Raw {
        always: Vec<String>,
        conditional: Vec<String>,
    }

    let raw: HashMap<String, Value> =
        serde_json::from_str(include_str!("../../../mastodon/entities.json"))
            .expect("mastodon/entities.json is not readable");
    raw.into_iter()
        // `_comment` and anything else underscored documents the file rather
        // than describing an entity.
        .filter(|(k, _)| !k.starts_with('_'))
        .map(|(k, v)| {
            let v: Raw = serde_json::from_value(v)
                .unwrap_or_else(|e| panic!("entity `{k}` is not readable: {e}"));
            (
                k,
                Definition {
                    always: v.always.into_iter().collect(),
                    conditional: v.conditional.into_iter().collect(),
                },
            )
        })
        .collect()
}

/// Compare one response object against the serializer it corresponds to.
fn compare(entity: &str, actual: &Value, findings: &mut Vec<String>) {
    let definitions = definitions();
    let Some(definition) = definitions.get(entity) else {
        findings.push(format!("no Mastodon serializer recorded for `{entity}`"));
        return;
    };
    let Some(object) = actual.as_object() else {
        findings.push(format!("`{entity}` response is not an object"));
        return;
    };

    let present: BTreeSet<String> = object.keys().cloned().collect();

    for field in definition.always.difference(&present) {
        findings.push(format!("{entity}: missing `{field}`"));
    }
    for field in &present {
        if !definition.always.contains(field) && !definition.conditional.contains(field) {
            findings.push(format!("{entity}: unexpected `{field}`"));
        }
    }
}

fn report(findings: Vec<String>) {
    assert!(
        findings.is_empty(),
        "{} entity field difference(s) from Mastodon {}:\n{}",
        findings.len(),
        eunha::version::MASTODON,
        findings
            .iter()
            .map(|f| format!("  {f}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// `Account`, as returned for another account and for one's own credentials.
/// `verify_credentials` returns an Account with `source` added, so the same
/// field set applies.
#[tokio::test]
async fn test_account_entities_match() {
    let ctx = TestContext::new("parity-account").await;
    let mut findings = Vec::new();

    let account: Value = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}", ctx.bob_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    compare("account", &account, &mut findings);

    let credentials: Value = ctx
        .api
        .get(
            "/api/v1/accounts/verify_credentials",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    compare("credential_account", &credentials, &mut findings);

    report(findings);
}

/// `Status`, with its nested `MediaAttachment` and `Poll`.
#[tokio::test]
async fn test_status_entities_match() {
    let ctx = TestContext::new("parity-status").await;
    let mut findings = Vec::new();

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "a status to inspect", "public")
        .await;
    compare("status", &status, &mut findings);

    // The account nested inside a status is the same entity.
    compare("account", &status["account"], &mut findings);

    report(findings);
}

/// `Relationship`, which clients poll constantly and read field by field.
#[tokio::test]
async fn test_relationship_entity_matches() {
    let ctx = TestContext::new("parity-relationship").await;
    let mut findings = Vec::new();

    let relationships: Value = ctx
        .api
        .get(
            &format!("/api/v1/accounts/relationships?id[]={}", ctx.bob_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    compare("relationship", &relationships[0], &mut findings);

    report(findings);
}

/// The instance description, which every client fetches before anything else.
#[tokio::test]
async fn test_instance_entity_matches() {
    let ctx = TestContext::new("parity-instance").await;
    let mut findings = Vec::new();

    let instance: Value = ctx
        .api
        .get("/api/v2/instance", None)
        .await
        .json()
        .await
        .unwrap();
    compare("instance", &instance, &mut findings);

    report(findings);
}

/// `Poll`, and the `MediaAttachment` and `PreviewCard` a status can carry.
#[tokio::test]
async fn test_poll_and_attachment_entities_match() {
    let ctx = TestContext::new("parity-poll").await;
    let mut findings = Vec::new();

    let posted = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &serde_json::json!({
                "status": "which one?",
                "poll": {"options": ["this", "that"], "expires_in": 3600},
            }),
        )
        .await;
    assert_eq!(posted.status().as_u16(), 200, "posting a poll");
    let status: Value = posted.json().await.unwrap();

    compare("poll", &status["poll"], &mut findings);
    report(findings);
}

/// `List` and `Marker`, both small enough that a missing field is the whole
/// entity's worth of difference.
#[tokio::test]
async fn test_list_and_marker_entities_match() {
    let ctx = TestContext::new("parity-list").await;
    let mut findings = Vec::new();

    let created = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &serde_json::json!({"title": "a list"}),
        )
        .await;
    assert_eq!(created.status().as_u16(), 200, "creating a list");
    let list: Value = created.json().await.unwrap();
    compare("list", &list, &mut findings);

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "something to mark", "public")
        .await;
    let marked = ctx
        .api
        .post_json(
            "/api/v1/markers",
            Some(&ctx.alice_token),
            &serde_json::json!({"home": {"last_read_id": status["id"]}}),
        )
        .await;
    assert_eq!(marked.status().as_u16(), 200, "setting a marker");
    let markers: Value = marked.json().await.unwrap();
    compare("marker", &markers["home"], &mut findings);

    report(findings);
}

/// `Tag`, as returned when following one.
#[tokio::test]
async fn test_tag_entity_matches() {
    let ctx = TestContext::new("parity-tag").await;
    let mut findings = Vec::new();

    ctx.api
        .post_status(&ctx.alice_token, "tagged #parity", "public")
        .await;

    let response: Value = ctx
        .api
        .get("/api/v1/tags/parity", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    compare("tag", &response, &mut findings);

    report(findings);
}

/// `Notification`, which clients poll and branch on field by field.
#[tokio::test]
async fn test_notification_entity_matches() {
    let ctx = TestContext::new("parity-notification").await;
    let mut findings = Vec::new();

    // Bob follows Alice, so Alice has a notification to read.
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;

    let notifications: Value = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let first = notifications
        .as_array()
        .and_then(|a| a.first())
        .expect("a follow should notify");
    compare("notification", first, &mut findings);

    report(findings);
}

/// `Context`, and the `StatusSource` and `StatusEdit` of an edited status.
#[tokio::test]
async fn test_status_context_and_source_entities_match() {
    let ctx = TestContext::new("parity-context").await;
    let mut findings = Vec::new();

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "the first post", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let context: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{id}/context"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    compare("context", &context, &mut findings);

    let source: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{id}/source"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    compare("status_source", &source, &mut findings);

    report(findings);
}

/// `Filter`, whose keywords a client applies itself.
#[tokio::test]
async fn test_filter_entity_matches() {
    let ctx = TestContext::new("parity-filter").await;
    let mut findings = Vec::new();

    let created = ctx
        .api
        .post_json(
            "/api/v2/filters",
            Some(&ctx.alice_token),
            &serde_json::json!({
                "title": "a filter",
                "context": ["home"],
                "filter_action": "warn",
                "keywords_attributes": [{"keyword": "spoiler", "whole_word": true}],
            }),
        )
        .await;
    assert_eq!(created.status().as_u16(), 200, "creating a filter");
    let filter: Value = created.json().await.unwrap();
    compare("filter", &filter, &mut findings);

    if let Some(keyword) = filter["keywords"].as_array().and_then(|a| a.first()) {
        compare("filter_keyword", keyword, &mut findings);
    }

    report(findings);
}

/// `Application`, as a client reads back its own registration.
#[tokio::test]
async fn test_application_entity_matches() {
    let ctx = TestContext::new("parity-application").await;
    let mut findings = Vec::new();

    let application: Value = ctx
        .api
        .get("/api/v1/apps/verify_credentials", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    compare("application", &application, &mut findings);

    report(findings);
}
