//! `eunha accounts create` and `eunha accounts modify`, which are `tootctl`'s.
//!
//! An instance's first account is made this way rather than by signing up:
//! Mastodon's `mastodon:setup` and `tootctl accounts create --role Owner` both
//! write a confirmed account holding the seeded Owner role, and hand back a
//! random password to sign in with. Whoever runs the instance recovers that
//! account with `tootctl accounts modify --reset-password`.

use reqwest::StatusCode;
use serde_json::{json, Value};

use eunha::accounts::{
    create_from_command, modify_from_command, reset_password, CreateOptions, Created, ModifyOptions,
};

use crate::helpers::TestContext;

fn owner(username: &str) -> CreateOptions {
    CreateOptions {
        username: username.to_string(),
        email: format!("{username}@example.com"),
        role: Some("Owner".to_string()),
        confirmed: true,
        approve: true,
        reattach: false,
        force: false,
    }
}

async fn create(ctx: &TestContext, options: CreateOptions) -> anyhow::Result<String> {
    match create_from_command(&ctx.state, options).await? {
        Created::Account(password) => Ok(password),
        Created::UsernameInUse => anyhow::bail!("the chosen username is currently in use"),
    }
}

async fn account_id(ctx: &TestContext, username: &str) -> Option<i64> {
    sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1 AND domain IS NULL")
        .bind(username)
        .fetch_optional(&ctx.db)
        .await
        .unwrap()
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

    let token_id: i64 = sqlx::query_scalar("SELECT id FROM oauth_access_tokens WHERE token = $1")
        .bind(&old_token)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    let mut stream = ctx
        .state
        .streaming
        .subscribe(&format!("timeline:access_token:{token_id}"))
        .await;

    let second = reset_password(&ctx.state, "Gardener").await.unwrap();
    // `revoke_access!` publishes `kill` for each token, through Redis, so a
    // stream opened with it in any process closes.
    let killed = tokio::time::timeout(std::time::Duration::from_secs(5), stream.recv())
        .await
        .expect("a kill for the revoked token")
        .unwrap();
    assert_eq!(*killed, json!({"event": "kill"}));
    assert!(ctx
        .mail_to("gardener@example.com", "Password changed")
        .await
        .is_some());
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

    let missing = reset_password(&ctx.state, "nobody").await.unwrap_err();
    assert!(missing.to_string().contains("no user with such username"));
}

/// Without `--confirmed` the account waits, as a Mastodon sign-up does, in a
/// `users` row with no `confirmed_at`, and its owner is mailed the link that
/// confirms it.
#[tokio::test]
async fn test_without_confirmed_the_account_waits_for_its_link() {
    let ctx = TestContext::new("accounts-create-unconfirmed").await;
    let password = create(
        &ctx,
        CreateOptions {
            confirmed: false,
            ..owner("sprout")
        },
    )
    .await
    .unwrap();

    let (confirmed, token): (bool, Option<String>) = sqlx::query_as(
        "SELECT u.confirmed_at IS NOT NULL, u.confirmation_token
         FROM users u JOIN accounts a ON a.id = u.account_id WHERE a.username = 'sprout'",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert!(!confirmed);
    let token = token.expect("a confirmation token");
    let mail = ctx
        .mail_to("sprout@example.com", "Confirm your email address")
        .await
        .expect("the confirmation mail");
    assert!(mail.html.contains(&format!("/auth/confirm?token={token}")));
    assert!(
        sign_in(&ctx, "sprout@example.com", &password)
            .await
            .is_none(),
        "an unconfirmed account cannot sign in"
    );

    let confirmed = ctx
        .api
        .get(&format!("/auth/confirm?token={token}"), None)
        .await;
    assert!(confirmed.status().is_redirection());
    assert!(sign_in(&ctx, "sprout@example.com", &password)
        .await
        .is_some());
    assert!(ctx
        .mail_to("sprout@example.com", "Welcome to Mastodon")
        .await
        .is_some());
}

/// A confirmed, approved account is prepared as a new user is: welcomed by
/// mail, and announced with `admin.sign_up` to everyone who may manage users.
#[tokio::test]
async fn test_a_confirmed_account_is_welcomed_and_announced_to_staff() {
    let ctx = TestContext::new("accounts-create-welcome").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    crate::helpers::make_admin(&ctx.db, alice).await;

    create(&ctx, owner("gardener")).await.unwrap();
    let gardener = account_id(&ctx, "gardener").await.unwrap();

    let mail = ctx
        .mail_to("gardener@example.com", "Welcome to Mastodon")
        .await
        .expect("the welcome mail");
    assert!(mail.html.contains("Welcome aboard, gardener!"));
    let queued: Option<f64> = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM run_at - created_at)::float8 FROM eunha.jobs
         WHERE kind = 'ActionMailer::MailDeliveryJob(UserMailer#welcome)'",
    )
    .fetch_optional(&ctx.db)
    .await
    .unwrap_or(None);
    if let Some(delay) = queued {
        assert!((3590.0..3610.0).contains(&delay), "queued an hour ahead");
    }

    let announced: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notifications
         WHERE account_id = $1 AND type = 'admin.sign_up' AND from_account_id = $2",
    )
    .bind(alice)
    .bind(gardener)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(announced, 1);
    // The new owner may manage users too, but is not told about itself.
    let own: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notifications WHERE account_id = $1 AND type = 'admin.sign_up'",
    )
    .bind(gardener)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(own, 0);
}

/// `--reattach` hands a deleted account's username, and the account itself,
/// to the new user; one still in use only with `--force`, which deletes it.
#[tokio::test]
async fn test_reattach_takes_over_a_username() {
    let ctx = TestContext::new("accounts-create-reattach").await;
    create(&ctx, owner("gardener")).await.unwrap();
    let first = account_id(&ctx, "gardener").await.unwrap();

    // In use: refused, and left alone, unless forced.
    let error = create(&ctx, owner("gardener")).await.unwrap_err();
    assert!(error
        .to_string()
        .contains("username has already been taken"));
    let held = create_from_command(
        &ctx.state,
        CreateOptions {
            email: "other@example.com".into(),
            reattach: true,
            ..owner("gardener")
        },
    )
    .await
    .unwrap();
    assert!(matches!(held, Created::UsernameInUse));
    assert_eq!(account_id(&ctx, "gardener").await, Some(first));

    // Forced but invalid: nothing is deleted for a user that would be refused.
    let error = create(
        &ctx,
        CreateOptions {
            email: "not-an-address".into(),
            reattach: true,
            force: true,
            ..owner("gardener")
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("email is invalid"));
    assert_eq!(account_id(&ctx, "gardener").await, Some(first));

    // Forced: the old user and account go, and a new account takes the name.
    create(
        &ctx,
        CreateOptions {
            email: "gardener@example.com".into(),
            reattach: true,
            force: true,
            ..owner("gardener")
        },
    )
    .await
    .unwrap();
    let second = account_id(&ctx, "gardener").await.unwrap();
    assert_ne!(second, first);

    // A deleted account keeps its row and username but loses its user; the
    // new user is attached to that very account.
    sqlx::query("DELETE FROM users WHERE account_id = $1")
        .bind(second)
        .execute(&ctx.db)
        .await
        .unwrap();
    sqlx::query("UPDATE accounts SET requested_deletion_at = now() WHERE id = $1")
        .bind(second)
        .execute(&ctx.db)
        .await
        .unwrap();
    let error = create(
        &ctx,
        CreateOptions {
            email: "third@example.com".into(),
            ..owner("gardener")
        },
    )
    .await
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("username has already been taken"));
    let password = create(
        &ctx,
        CreateOptions {
            email: "third@example.com".into(),
            reattach: true,
            ..owner("gardener")
        },
    )
    .await
    .unwrap();
    assert_eq!(account_id(&ctx, "gardener").await, Some(second));
    let deleted: bool =
        sqlx::query_scalar("SELECT requested_deletion_at IS NOT NULL FROM accounts WHERE id = $1")
            .bind(second)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert!(!deleted);
    assert!(sign_in(&ctx, "third@example.com", &password)
        .await
        .is_some());
}

/// The rest of `tootctl accounts modify`.
#[tokio::test]
async fn test_modify_changes_what_it_is_asked_to() {
    let ctx = TestContext::with_approval_required("accounts-modify").await;
    create(
        &ctx,
        CreateOptions {
            role: None,
            confirmed: false,
            approve: false,
            ..owner("sprout")
        },
    )
    .await
    .unwrap();
    let user = |ctx: &TestContext| {
        let db = ctx.db.clone();
        async move {
            sqlx::query_as::<_, (Option<String>, bool, bool, bool, String, Option<String>)>(
                "SELECT r.name, u.disabled, u.approved, u.confirmed_at IS NOT NULL, u.email,
                        u.unconfirmed_email
                 FROM users u JOIN accounts a ON a.id = u.account_id
                 LEFT JOIN user_roles r ON r.id = u.role_id
                 WHERE a.username = 'sprout'",
            )
            .fetch_one(&db)
            .await
            .unwrap()
        }
    };

    let password = modify_from_command(
        &ctx.state,
        "sprout",
        ModifyOptions {
            role: Some("Moderator".into()),
            disable: true,
            approve: true,
            email: Some("Sprout2@Example.com".into()),
            ..ModifyOptions::default()
        },
    )
    .await
    .unwrap();
    assert!(password.is_none());
    let (role, disabled, approved, confirmed, email, unconfirmed) = user(&ctx).await;
    assert_eq!(role.as_deref(), Some("Moderator"));
    assert!(disabled && approved && !confirmed);
    assert_eq!(email, "sprout@example.com");
    assert_eq!(unconfirmed.as_deref(), Some("sprout2@example.com"));
    assert!(ctx
        .mail_to("sprout2@example.com", "Confirm email")
        .await
        .is_some());

    modify_from_command(
        &ctx.state,
        "sprout",
        ModifyOptions {
            remove_role: true,
            enable: true,
            confirm: true,
            ..ModifyOptions::default()
        },
    )
    .await
    .unwrap();
    let (role, disabled, _, confirmed, email, unconfirmed) = user(&ctx).await;
    assert_eq!(role, None);
    assert!(!disabled && confirmed);
    assert_eq!(email, "sprout2@example.com");
    assert_eq!(unconfirmed, None);

    let missing = modify_from_command(&ctx.state, "nobody", ModifyOptions::default())
        .await
        .unwrap_err();
    assert!(missing.to_string().contains("no user with such username"));
}

/// The email blocks hold for an account made unconfirmed, as
/// `UserEmailValidator` does for any user not yet confirmed; a confirmed one
/// is past them.
#[tokio::test]
async fn test_email_blocks_hold_until_confirmed() {
    let ctx = TestContext::new("accounts-create-email-block").await;
    sqlx::query(
        "INSERT INTO email_domain_blocks (domain, allow_with_approval, created_at, updated_at)
         VALUES ('blocked.example', false, now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    let error = create(
        &ctx,
        CreateOptions {
            email: "sprout@blocked.example".into(),
            confirmed: false,
            ..owner("sprout")
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("disallowed e-mail provider"));
    assert_eq!(account_id(&ctx, "sprout").await, None);

    create(
        &ctx,
        CreateOptions {
            email: "sprout@blocked.example".into(),
            ..owner("sprout")
        },
    )
    .await
    .unwrap();
    assert!(account_id(&ctx, "sprout").await.is_some());
}

/// `Scheduler::UserCleanupScheduler`: an account whose link went out a week
/// ago and was never followed is removed, and its username freed.
#[tokio::test]
async fn test_unconfirmed_accounts_are_cleaned_up_after_a_week() {
    let ctx = TestContext::new("accounts-clean-unconfirmed").await;
    for name in ["stale", "fresh"] {
        create(
            &ctx,
            CreateOptions {
                confirmed: false,
                ..owner(name)
            },
        )
        .await
        .unwrap();
    }
    sqlx::query(
        "UPDATE users SET confirmation_sent_at = now() - interval '8 days'
         WHERE email = 'stale@example.com'",
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    assert_eq!(
        eunha::accounts::clean_unconfirmed(&ctx.db).await.unwrap(),
        1
    );
    assert_eq!(account_id(&ctx, "stale").await, None);
    assert!(account_id(&ctx, "fresh").await.is_some());
}
