//! `Admin::InstancesController`, its notes and dashboard, and the domain block
//! and allow exports and imports.

use super::*;

const MANAGE_FEDERATION: i64 = 1 << 5;

async fn remote(ctx: &TestContext, username: &str, domain: &str) -> i64 {
    sqlx::query_scalar(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, protocol, created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', 'https://' || $3 || '/@' || $2,
                   'https://' || $3 || '/users/' || $2, 1, now(), now())
           RETURNING id"#,
    )
    .bind(eunha::snowflake::next_id())
    .bind(username)
    .bind(domain)
    .fetch_one(&ctx.db)
    .await
    .unwrap()
}

fn domains(list: &Value) -> Vec<String> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|i| i["domain"].as_str().unwrap().to_owned())
        .collect()
}

/// The instances list and page: known domains by accounts, filtered as
/// `InstanceFilter` filters, with their blocks, availability and notes; and
/// stopping, restarting and purging deliveries and data, logged as upstream.
#[tokio::test]
async fn test_instances() {
    let ctx = TestContext::new("srv-instances").await;
    // With `view_dashboard` for the instance measures.
    give_role(&ctx, &ctx.bob_id, 10, MANAGE_FEDERATION | (1 << 3)).await;
    let big = format!("big-{}.invalid", &ctx.domain[..8]);
    let small = format!("small-{}.invalid", &ctx.domain[..8]);
    for name in ["a", "b", "c"] {
        remote(&ctx, name, &big).await;
    }
    let far = remote(&ctx, "far", &small).await;
    sqlx::query("INSERT INTO follows (id, account_id, target_account_id, created_at, updated_at) VALUES ($1, $2, $3, now(), now())")
        .bind(eunha::snowflake::next_id())
        .bind(far)
        .bind(id(&ctx.alice_id))
        .execute(&ctx.db)
        .await
        .unwrap();
    let get = |path: String| {
        let api = &ctx.api;
        let token = ctx.bob_token.clone();
        async move { json_ok(api.get(&path, Some(&token)).await).await }
    };
    let post = |path: String| {
        let api = &ctx.api;
        let token = ctx.bob_token.clone();
        async move { api.post_json(&path, Some(&token), &json!({})).await }
    };

    let list = get("/api/v1/admin/instances".into()).await;
    assert_eq!(domains(&list), [big.clone(), small.clone()]);
    assert_eq!(list[0]["accounts_count"], 3);
    let by_domain = get("/api/v1/admin/instances?by_domain=small-".to_owned()).await;
    assert_eq!(domains(&by_domain), std::slice::from_ref(&small));

    // A block shows on the instance, and `limited` lists only blocked ones.
    json_ok(
        ctx.api
            .post_json(
                "/api/v1/admin/domain_blocks",
                Some(&ctx.bob_token),
                &json!({"domain": small, "severity": "noop", "reject_media": true}),
            )
            .await,
    )
    .await;
    let limited = get("/api/v1/admin/instances?limited=1".into()).await;
    assert_eq!(domains(&limited), std::slice::from_ref(&small));
    assert_eq!(limited[0]["domain_block"]["severity"], "noop");

    // Failures make it failing; clearing forgets them.
    ctx.state
        .delivery_failures
        .track_failure(&big)
        .await
        .unwrap();
    let failing = get("/api/v1/admin/instances?availability=failing".into()).await;
    assert_eq!(domains(&failing), std::slice::from_ref(&big));
    assert_eq!(failing[0]["failure_days"], 1);
    let detail = json_ok(
        post(format!(
            "/api/v1/admin/instances/{big}/clear_delivery_errors"
        ))
        .await,
    )
    .await;
    assert_eq!(detail["exhausted_deliveries_days"], json!([]));

    // Stopping marks it unavailable; restarting lifts the mark.
    let stopped = json_ok(post(format!("/api/v1/admin/instances/{big}/stop_delivery")).await).await;
    assert_eq!(stopped["unavailable"], true);
    assert_eq!(stopped["purgeable"], true);
    assert_eq!(
        error_of(
            post(format!("/api/v1/admin/instances/{big}/stop_delivery")).await,
            StatusCode::UNPROCESSABLE_ENTITY
        )
        .await,
        "Validation failed: Domain has already been taken"
    );
    let unavailable = get("/api/v1/admin/instances?availability=unavailable".into()).await;
    assert_eq!(domains(&unavailable), std::slice::from_ref(&big));
    let restarted =
        json_ok(post(format!("/api/v1/admin/instances/{big}/restart_delivery")).await).await;
    assert_eq!(restarted["unavailable"], false);

    // Notes: added by anyone who manages federation, removed by their author.
    let noted = json_ok(
        ctx.api
            .post_json(
                &format!("/api/v1/admin/instances/{small}/moderation_notes"),
                Some(&ctx.bob_token),
                &json!({"content": "Spam source"}),
            )
            .await,
    )
    .await;
    let note = &noted["moderation_notes"][0];
    assert_eq!(note["content"], "Spam source");
    assert_eq!(note["account"]["id"], ctx.bob_id.as_str());
    assert_eq!(
        error_of(
            ctx.api
                .post_json(
                    &format!("/api/v1/admin/instances/{small}/moderation_notes"),
                    Some(&ctx.bob_token),
                    &json!({"content": ""}),
                )
                .await,
            StatusCode::UNPROCESSABLE_ENTITY
        )
        .await,
        "Validation failed: Content can't be blank"
    );
    json_ok(
        ctx.api
            .delete(
                &format!(
                    "/api/v1/admin/instances/{small}/moderation_notes/{}",
                    note["id"].as_str().unwrap()
                ),
                &ctx.bob_token,
            )
            .await,
    )
    .await;

    // The instance measures of Mastodon's admin API.
    let measures = json_ok(
        ctx.api
            .post_json(
                "/api/v1/admin/measures",
                Some(&ctx.bob_token),
                &json!({
                    "keys": ["instance_accounts", "instance_followers"],
                    "instance_accounts": {"domain": big},
                    "instance_followers": {"domain": small},
                }),
            )
            .await,
    )
    .await;
    assert_eq!(measures[0]["total"], "3");
    assert!(measures[0].get("previous_total").is_none());
    assert_eq!(measures[1]["total"], "1");
    // Only the media measure defines `value_to_human_value`, in Rails' words.
    let media = json_ok(
        ctx.api
            .post_json(
                "/api/v1/admin/measures",
                Some(&ctx.bob_token),
                &json!({
                    "keys": ["instance_media_attachments"],
                    "instance_media_attachments": {"domain": big},
                }),
            )
            .await,
    )
    .await;
    assert_eq!(media[0]["unit"], "bytes");
    assert_eq!(media[0]["human_value"], "0 Bytes");
    // `params.require(:instance_accounts)`.
    let missing = ctx
        .api
        .post_json(
            "/api/v1/admin/measures",
            Some(&ctx.bob_token),
            &json!({"keys": ["instance_accounts"]}),
        )
        .await;
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST);

    // Purging deletes every account from the domain.
    json_ok(
        ctx.api
            .delete(&format!("/api/v1/admin/instances/{big}"), &ctx.bob_token)
            .await,
    )
    .await;
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM accounts WHERE domain = $1")
        .bind(&big)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(left, 0);

    let kinds: Vec<(String, String)> = logs(&ctx)
        .await
        .into_iter()
        .filter(|(_, kind)| kind != "DomainBlock")
        .collect();
    assert_eq!(
        kinds,
        [
            ("create".to_owned(), "UnavailableDomain".to_owned()),
            ("destroy".to_owned(), "UnavailableDomain".to_owned()),
            ("destroy".to_owned(), "Instance".to_owned()),
        ]
    );
}

async fn upload(ctx: &TestContext, path: &str, csv: &str) -> reqwest::Response {
    let form = reqwest::multipart::Form::new().part(
        "data",
        reqwest::multipart::Part::bytes(csv.as_bytes().to_vec())
            .file_name("blocks.csv")
            .mime_str("text/csv")
            .unwrap(),
    );
    ctx.api
        .http
        .post(ctx.api.url(path))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .multipart(form)
        .send()
        .await
        .unwrap()
}

/// Domain blocks and allows export as Mastodon's CSV, and import back: blocks
/// as candidates to confirm, allows at once, each logged.
#[tokio::test]
async fn test_domain_block_and_allow_csv() {
    let ctx = TestContext::new("srv-domain-csv").await;
    make_admin(&ctx).await;
    for body in [
        json!({"domain": "silenced.example", "severity": "silence", "public_comment": "spam, mostly"}),
        json!({"domain": "noop.example", "severity": "noop"}),
    ] {
        json_ok(
            ctx.api
                .post_json("/api/v1/admin/domain_blocks", Some(&ctx.alice_token), &body)
                .await,
        )
        .await;
    }
    let export = ctx
        .api
        .get(
            "/api/v1/admin/export_domain_blocks/export",
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(export.status(), StatusCode::OK);
    assert_eq!(export.headers()["content-type"], "text/csv");
    // A noop block with nothing rejected has no limitations and is left out.
    assert_eq!(
        export.text().await.unwrap(),
        "#domain,#severity,#reject_media,#reject_reports,#public_comment,#obfuscate\n\
         silenced.example,silence,false,false,\"spam, mostly\",false\n"
    );

    let imported = json_ok(
        upload(
            &ctx,
            "/api/v1/admin/export_domain_blocks/import",
            "#domain,#severity,#reject_media,#reject_reports,#public_comment,#obfuscate\n\
             Bad.Example,suspend,true,false,Spam,false\n\
             sub.silenced.example,suspend,false,false,,false\n\
             odd.example,sometimes,false,false,,false\n",
        )
        .await,
    )
    .await;
    let blocks = imported["domain_blocks"].as_array().unwrap();
    // The subdomain of an existing block is skipped; the bad severity reported.
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["domain"], "bad.example");
    assert_eq!(blocks[0]["severity"], "suspend");
    assert_eq!(blocks[0]["reject_media"], true);
    assert!(blocks[0]["private_comment"]
        .as_str()
        .unwrap()
        .starts_with("Imported from blocks.csv on "));
    assert_eq!(imported["errors"].as_array().unwrap().len(), 1);

    // A file of bare domains is read as `#domain`.
    let allowed = json_ok(
        upload(
            &ctx,
            "/api/v1/admin/export_domain_allows/import",
            "friendly.example\nnice.example\n",
        )
        .await,
    )
    .await;
    assert_eq!(allowed, json!(["friendly.example", "nice.example"]));
    let export = ctx
        .api
        .get(
            "/api/v1/admin/export_domain_allows/export",
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(
        export.text().await.unwrap(),
        "#domain\nfriendly.example\nnice.example\n"
    );
    let allows: Vec<(String, String)> = logs(&ctx)
        .await
        .into_iter()
        .filter(|(_, kind)| kind == "DomainAllow")
        .collect();
    assert_eq!(allows.len(), 2);

    let missing = ctx
        .api
        .post_json(
            "/api/v1/admin/export_domain_allows/import",
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(
        error_of(missing, StatusCode::UNPROCESSABLE_ENTITY).await,
        "Validation failed: Data can't be blank"
    );
}

/// `Instance.refresh`: the `instances` materialized view, created empty, is
/// filled on the first refresh and refreshed concurrently after that.
#[tokio::test]
async fn test_instances_view_is_refreshed() {
    let ctx = crate::helpers::TestContext::new("instances-refresh").await;
    sqlx::query(
        "INSERT INTO accounts (id, username, domain, uri, url, protocol, created_at, updated_at)
         VALUES (777001, 'far', 'far.example', 'https://far.example/users/far',
                 'https://far.example/@far', 1, now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    eunha::background::refresh_instances(&ctx.state)
        .await
        .unwrap();
    // A second refresh runs concurrently against the populated view.
    eunha::background::refresh_instances(&ctx.state)
        .await
        .unwrap();
    let domains: Vec<String> = sqlx::query_scalar("SELECT domain FROM instances")
        .fetch_all(&ctx.db)
        .await
        .unwrap();
    assert!(domains.contains(&"far.example".to_owned()), "{domains:?}");
}
