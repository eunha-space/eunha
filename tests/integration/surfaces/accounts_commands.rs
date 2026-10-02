//! `eunha accounts create` and `eunha accounts modify`, which are `tootctl`'s.
//!
//! An instance's first account is made this way rather than by signing up:
//! Mastodon's `mastodon:setup` and `tootctl accounts create --role Owner` both
//! write a confirmed account holding the seeded Owner role, and hand back a
//! random password to sign in with. Whoever runs the instance recovers that
//! account with `tootctl accounts modify --reset-password`.

use reqwest::StatusCode;
use serde_json::{json, Value};

use eunha::accounts::{create_from_command, reset_password, CreateOptions};

use crate::helpers::TestContext;

fn owner(username: &str) -> CreateOptions {
    CreateOptions {
        username: username.to_string(),
        email: format!("{username}@example.com"),
        role: Some("Owner".to_string()),
        confirmed: true,
        approve: true,
    }
}

async fn create(ctx: &TestContext, options: CreateOptions) -> anyhow::Result<String> {
    create_from_command(
        &ctx.db,
        ctx.state.encryptor.as_ref(),
        &ctx.state.instance,
        options,
    )
    .await
}

/// An access token got through the authorization code flow, signing in with
/// `email` and `password` on the authorization page, or `None` if the
/// credentials are refused.
async fn sign_in(ctx: &TestContext, email: &str, password: &str) -> Option<String> {
    let redirect_uri = "https://client.example/cb";
    let app: Value = ctx
        .api
        .post_json(
            "/api/v1/apps",
            None,
            &json!({
                "client_name": "Owner sign-in",
                "redirect_uris": redirect_uri,
                "scopes": "read"
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let client_id = app["client_id"].as_str().unwrap();
    let authorized = ctx
        .api
        .post_form(
            "/oauth/authorize",
            None,
            &[
                ("client_id", client_id),
                ("redirect_uri", redirect_uri),
                ("scope", "read"),
                ("email", email),
                ("password", password),
            ],
        )
        .await;
    let location = authorized.headers().get("location")?.to_str().ok()?;
    let code = url::Url::parse(location)
        .ok()?
        .query_pairs()
        .find(|(name, _)| name == "code")?
        .1
        .into_owned();
    let response = ctx
        .api
        .post_json(
            "/oauth/token",
            None,
            &json!({
                "grant_type": "authorization_code",
                "client_id": client_id,
                "client_secret": app["client_secret"],
                "code": code,
                "redirect_uri": redirect_uri,
            }),
        )
        .await;
    if response.status() != StatusCode::OK {
        return None;
    }
    let body: Value = response.json().await.unwrap();
    Some(body["access_token"].as_str().unwrap().to_string())
}

/// The roles `db/seeds/03_roles.rb` creates from `config/roles.yml`.
#[tokio::test]
async fn test_the_default_roles_are_seeded() {
    let ctx = TestContext::new("roles-seeded").await;
    let roles: Vec<(String, i32, i64, bool)> = sqlx::query_as(
        "SELECT name, position, permissions, highlighted FROM user_roles
         WHERE id <> -99 AND name IN ('Moderator', 'Admin', 'Owner')
         ORDER BY position",
    )
    .fetch_all(&ctx.db)
    .await
    .unwrap();

    let moderator = (1 << 3) | (1 << 2) | (1 << 20) | (1 << 10) | (1 << 4) | (1 << 8);
    let admin = moderator
        | (1 << 18)
        | (1 << 19)
        | (1 << 5)
        | (1 << 6)
        | (1 << 7)
        | (1 << 9)
        | (1 << 12)
        | (1 << 11)
        | (1 << 13)
        | (1 << 14)
        | (1 << 15)
        | (1 << 17);
    assert_eq!(
        roles,
        vec![
            ("Moderator".to_string(), 10, moderator, true),
            ("Admin".to_string(), 100, admin, true),
            ("Owner".to_string(), 1000, 1, true),
        ]
    );
}

/// The owner can sign in with the password printed for them, holds the Owner
/// role, and is the instance's contact — the highest-ranked local account.
#[tokio::test]
async fn test_an_owner_created_on_the_command_line_can_sign_in() {
    let ctx = TestContext::new("accounts-create-owner").await;
    let password = create(&ctx, owner("gardener")).await.unwrap();
    assert_eq!(password.len(), 32);

    let (confirmed, approved, role): (bool, bool, String) = sqlx::query_as(
        "SELECT u.confirmed_at IS NOT NULL, u.approved, r.name
         FROM users u JOIN accounts a ON a.id = u.account_id
         JOIN user_roles r ON r.id = u.role_id
         WHERE a.username = 'gardener' AND a.domain IS NULL",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert!(confirmed);
    assert!(approved);
    assert_eq!(role, "Owner");

    let token = sign_in(&ctx, "gardener@example.com", &password)
        .await
        .expect("the printed password signs in");

    let me: Value = ctx
        .api
        .get("/api/v1/accounts/verify_credentials", Some(&token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(me["username"], "gardener");
    assert_eq!(me["role"]["name"], "Owner");

    // Signing needs the key to have been sealed into `keypairs`.
    let key = eunha::federation::keypair::signing_key(
        &ctx.state,
        me["id"].as_str().unwrap().parse().unwrap(),
    )
    .await
    .unwrap();
    assert!(key.private_key.contains("PRIVATE KEY"));

    let instance: Value = ctx
        .api
        .get("/api/v2/instance", None)
        .await
        .json()
        .await
        .unwrap();
    // The contact account is whoever `site_contact_username` names, as in
    // Mastodon; creating an owner does not change it.
    assert_eq!(instance["contact"]["account"]["username"], "alice");
}

/// Without `--approve`, an account is approved as a sign-up would be:
/// `User#set_approved` leaves it pending where the instance requires approval.
#[tokio::test]
async fn test_without_approve_an_approval_required_instance_leaves_it_pending() {
    let ctx = TestContext::with_approval_required("accounts-create-pending").await;
    create(
        &ctx,
        CreateOptions {
            approve: false,
            ..owner("pending")
        },
    )
    .await
    .unwrap();

    let approved: bool = sqlx::query_scalar(
        "SELECT u.approved FROM users u JOIN accounts a ON a.id = u.account_id
         WHERE a.username = 'pending'",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert!(!approved);
}

/// The registration checks are bypassed, but the account's own validations are
/// not, and nothing is written when one fails.
#[tokio::test]
async fn test_invalid_accounts_are_refused() {
    let ctx = TestContext::new("accounts-create-invalid").await;

    let refusals = [
        (
            CreateOptions {
                confirmed: false,
                ..owner("unconfirmed")
            },
            "--confirmed",
        ),
        (owner("Alice"), "username has already been taken"),
        (owner("has-hyphen"), "letters, numbers and underscores"),
        (owner(&"a".repeat(31)), "too long"),
        (
            CreateOptions {
                email: "ALICE@test.invalid".to_string(),
                ..owner("another_alice")
            },
            "email has already been taken",
        ),
        (
            CreateOptions {
                email: "not-an-address".to_string(),
                ..owner("no_address")
            },
            "email is invalid",
        ),
        (
            CreateOptions {
                role: Some("Gardener".to_string()),
                ..owner("no_role")
            },
            "cannot find user role",
        ),
    ];

    for (options, message) in refusals {
        let username = options.username.clone();
        let error = create(&ctx, options)
            .await
            .expect_err(&format!("{username} should be refused"));
        assert!(
            error.to_string().contains(message),
            "{username}: expected `{message}`, got `{error}`"
        );
    }

    let local: i64 = sqlx::query_scalar("SELECT count(*) FROM accounts WHERE domain IS NULL")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(local, 2, "only alice and bob");
}

/// `User#change_password!`: the old password stops working, and so does every
/// token the account had handed out.
#[tokio::test]
async fn test_resetting_a_password_signs_the_account_out_everywhere() {
    let ctx = TestContext::new("accounts-reset-password").await;
    let first = create(&ctx, owner("gardener")).await.unwrap();
    let old_token = sign_in(&ctx, "gardener@example.com", &first).await.unwrap();

    let second = reset_password(&ctx.db, "Gardener").await.unwrap();
    assert_ne!(first, second);
    assert_eq!(second.len(), 32);

    let revoked = ctx
        .api
        .get("/api/v1/accounts/verify_credentials", Some(&old_token))
        .await;
    assert_eq!(revoked.status(), StatusCode::UNAUTHORIZED);
    assert!(sign_in(&ctx, "gardener@example.com", &first)
        .await
        .is_none());
    assert!(sign_in(&ctx, "gardener@example.com", &second)
        .await
        .is_some());

    // Other accounts are untouched.
    let bob = ctx
        .api
        .get("/api/v1/accounts/verify_credentials", Some(&ctx.bob_token))
        .await;
    assert_eq!(bob.status(), StatusCode::OK);

    let missing = reset_password(&ctx.db, "nobody").await.unwrap_err();
    assert!(missing.to_string().contains("no user with such username"));
}
