use std::net::SocketAddr;
use std::str::FromStr;

use argon2::password_hash::{rand_core::OsRng, SaltString};
use argon2::{Argon2, PasswordHasher};
use axum::http::StatusCode as AxumStatus;
use reqwest::Client;
use sqlx::postgres::PgConnectOptions;
use sqlx::{postgres::PgPoolOptions, PgPool};
use uuid::Uuid;

/// Swap the database name in a Postgres connection URL, preserving any query.
fn replace_db_name(url: &str, db: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (url, None),
    };
    let prefix = match base.rfind('/') {
        Some(i) => &base[..i],
        None => base,
    };
    let mut out = format!("{prefix}/{db}");
    if let Some(q) = query {
        out.push('?');
        out.push_str(q);
    }
    out
}

// ── fake S3 server ──────────────────────────────────────────────────────────

/// Spawns a minimal HTTP server that accepts all S3-style PUT/DELETE requests
/// and returns success responses. Returns the base URL of the server.
pub async fn spawn_fake_s3() -> String {
    use axum::http::Request;
    use axum::{body::Body, response::Response, routing::any, Router};

    let app = Router::new().fallback(any(|req: Request<Body>| async move {
        match req.method().as_str() {
            "PUT" => Response::builder()
                .status(AxumStatus::OK)
                .header("ETag", "\"test-etag-000\"")
                .body(Body::empty())
                .unwrap(),
            "DELETE" => Response::builder()
                .status(AxumStatus::NO_CONTENT)
                .body(Body::empty())
                .unwrap(),
            _ => Response::builder()
                .status(AxumStatus::OK)
                .body(Body::empty())
                .unwrap(),
        }
    }));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{}", addr)
}

// ── client wrapper ─────────────────────────────────────────────────────────

/// Thin wrapper around `reqwest::Client` plus the base URL / Host header.
///
/// Swap this struct (or its constructor) to point tests at a different server.
pub struct ApiClient {
    pub http: Client,
    pub base_url: String,
    /// Value for the `Host` header; only needed when the server is a
    /// multi-tenant instance that routes by host (like eunha).
    pub host: String,
}

impl ApiClient {
    pub fn new(base_url: impl Into<String>, host: impl Into<String>) -> Self {
        Self {
            http: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            base_url: base_url.into(),
            host: host.into(),
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    pub async fn get(&self, path: &str, token: Option<&str>) -> reqwest::Response {
        let mut req = self.http.get(self.url(path)).header("host", &self.host);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        req.send().await.unwrap()
    }

    /// GET asking for ActivityPub, as another server does. Without it an
    /// actor's or a status's URI answers with the page a browser is sent to.
    pub async fn ap_get(&self, path: &str, token: Option<&str>) -> reqwest::Response {
        let mut req = self
            .http
            .get(self.url(path))
            .header("host", &self.host)
            .header("accept", "application/activity+json");
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        req.send().await.unwrap()
    }

    pub async fn post_json(
        &self,
        path: &str,
        token: Option<&str>,
        body: &serde_json::Value,
    ) -> reqwest::Response {
        let mut req = self
            .http
            .post(self.url(path))
            .header("host", &self.host)
            .json(body);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        req.send().await.unwrap()
    }

    /// POST an activity signed with RFC 9421 HTTP Message Signatures, as a
    /// peer that has moved on from the cavage draft would.
    pub async fn post_signed_rfc9421(
        &self,
        path: &str,
        body: &serde_json::Value,
        key_id: &str,
        private_key_pem: &str,
    ) -> reqwest::Response {
        let body_bytes = serde_json::to_vec(body).unwrap();
        // Signed against the public host, which is what the sender addressed.
        let signing_url = format!("https://{}{}", self.host, path);
        let signed = ojak::sig::rfc9421::sign_request(
            "post",
            &signing_url,
            Some(&body_bytes),
            key_id,
            &ojak::sig::rfc9421::SigningKey::RsaPem(private_key_pem),
            chrono::Utc::now().timestamp(),
        )
        .expect("sign request");
        let mut request = self
            .http
            .post(self.url(path))
            .header("host", &self.host)
            .header("signature-input", signed.signature_input)
            .header("signature", signed.signature)
            .header("content-type", "application/activity+json");
        if let Some(digest) = signed.content_digest {
            request = request.header("content-digest", digest);
        }
        request.body(body_bytes).send().await.unwrap()
    }

    /// POST an ActivityPub activity with a valid HTTP Signature, as a remote
    /// server would. `key_id` is the signing actor's key (e.g.
    /// `https://host/users/alice#main-key`) whose public key the receiving
    /// instance must already know; `private_key_pem` is its private key.
    pub async fn post_signed(
        &self,
        path: &str,
        body: &serde_json::Value,
        key_id: &str,
        private_key_pem: &str,
    ) -> reqwest::Response {
        // Sign against the public host, which ojak also puts in the Host
        // header the server sees, not the loopback base_url.
        let signing_url = format!("https://{}{}", self.host, path);
        let (parts, body) =
            ojak::testing::signed_post(&signing_url, body, key_id, private_key_pem).into_parts();
        self.http
            .post(self.url(path))
            .headers(parts.headers)
            .body(body)
            .send()
            .await
            .unwrap()
    }

    pub async fn post_form(
        &self,
        path: &str,
        token: Option<&str>,
        form: &[(&str, &str)],
    ) -> reqwest::Response {
        let mut req = self
            .http
            .post(self.url(path))
            .header("host", &self.host)
            .form(form);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        req.send().await.unwrap()
    }

    pub async fn delete(&self, path: &str, token: &str) -> reqwest::Response {
        self.http
            .delete(self.url(path))
            .header("host", &self.host)
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
    }

    pub async fn delete_json(
        &self,
        path: &str,
        token: &str,
        body: &serde_json::Value,
    ) -> reqwest::Response {
        self.http
            .delete(self.url(path))
            .header("host", &self.host)
            .bearer_auth(token)
            .json(body)
            .send()
            .await
            .unwrap()
    }

    pub async fn put_json(
        &self,
        path: &str,
        token: Option<&str>,
        body: &serde_json::Value,
    ) -> reqwest::Response {
        let mut req = self
            .http
            .put(self.url(path))
            .header("host", &self.host)
            .json(body);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        req.send().await.unwrap()
    }

    pub async fn patch_json(
        &self,
        path: &str,
        token: Option<&str>,
        body: &serde_json::Value,
    ) -> reqwest::Response {
        let mut req = self
            .http
            .patch(self.url(path))
            .header("host", &self.host)
            .json(body);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        req.send().await.unwrap()
    }

    /// POST with multipart/form-data including a file part (for media upload).
    pub async fn post_multipart_file(
        &self,
        path: &str,
        token: &str,
        file_name: &str,
        content_type: &str,
        data: Vec<u8>,
        extra_fields: &[(&'static str, &str)],
    ) -> reqwest::Response {
        let part = reqwest::multipart::Part::bytes(data)
            .file_name(file_name.to_string())
            .mime_str(content_type)
            .unwrap();
        let mut form = reqwest::multipart::Form::new().part("file", part);
        for (k, v) in extra_fields {
            form = form.text(*k, v.to_string());
        }
        self.http
            .post(self.url(path))
            .header("host", &self.host)
            .bearer_auth(token)
            .multipart(form)
            .send()
            .await
            .unwrap()
    }

    /// PATCH with multipart/form-data (required by update_credentials).
    pub async fn patch_multipart(
        &self,
        path: &str,
        token: &str,
        fields: &[(&'static str, &str)],
    ) -> reqwest::Response {
        let mut form = reqwest::multipart::Form::new();
        for (k, v) in fields {
            form = form.text(*k, v.to_string());
        }
        self.http
            .patch(self.url(path))
            .header("host", &self.host)
            .bearer_auth(token)
            .multipart(form)
            .send()
            .await
            .unwrap()
    }

    // ── convenience helpers ──────────────────────────────────────────────

    /// POST /api/v1/statuses with JSON body, returns the status JSON.
    pub async fn post_status(
        &self,
        token: &str,
        text: &str,
        visibility: &str,
    ) -> serde_json::Value {
        let resp = self
            .post_json(
                "/api/v1/statuses",
                Some(token),
                &serde_json::json!({"status": text, "visibility": visibility}),
            )
            .await;
        assert_eq!(
            resp.status().as_u16(),
            200,
            "post_status failed for visibility={visibility}"
        );
        resp.json().await.unwrap()
    }

    /// Follow an account; returns the relationship JSON.
    pub async fn follow(&self, token: &str, account_id: &str) -> serde_json::Value {
        let resp = self
            .post_json(
                &format!("/api/v1/accounts/{account_id}/follow"),
                Some(token),
                &serde_json::json!({}),
            )
            .await;
        assert_eq!(resp.status().as_u16(), 200);
        resp.json().await.unwrap()
    }

    /// GET /api/v1/timelines/public?local=true; returns status array.
    ///
    /// Uses `local=true` so parallel tests don't spill into each other's
    /// instance timeline.  Pass `local=false` to opt into the full federated
    /// view when testing against a remote server.
    pub async fn public_timeline(&self) -> Vec<serde_json::Value> {
        self.get("/api/v1/timelines/public?local=true", None)
            .await
            .json()
            .await
            .unwrap()
    }

    /// GET /api/v1/timelines/home; returns status array.
    pub async fn home_timeline(&self, token: &str) -> Vec<serde_json::Value> {
        self.get("/api/v1/timelines/home", Some(token))
            .await
            .json()
            .await
            .unwrap()
    }
}

// ── test context ────────────────────────────────────────────────────────────

pub struct TestContext {
    pub api: ApiClient,
    pub domain: String,
    pub alice_token: String,
    pub alice_id: String,
    pub bob_token: String,
    pub bob_id: String,
    pub db: PgPool,
    /// Cloned AppState — use this to call internal functions (e.g. background jobs) in tests.
    pub state: eunha::state::AppState,
    /// Kept alive so the server task isn't dropped while tests run.
    pub _server: tokio::task::JoinHandle<()>,
    /// Maintenance connection URL and per-test database name, for teardown.
    admin_url: String,
    test_db_name: String,
}

impl Drop for TestContext {
    fn drop(&mut self) {
        let admin_url = self.admin_url.clone();
        let db_name = self.test_db_name.clone();
        // Drop the per-test database on a separate thread (we can't block the
        // async runtime in Drop). FORCE terminates any lingering connections.
        let _ = std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(_) => return,
            };
            rt.block_on(async {
                if let Ok(opts) = PgConnectOptions::from_str(&admin_url) {
                    if let Ok(pool) = PgPoolOptions::new()
                        .max_connections(1)
                        .connect_with(opts.database("postgres"))
                        .await
                    {
                        let _ = sqlx::query(&format!(
                            "DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)"
                        ))
                        .execute(&pool)
                        .await;
                    }
                }
            });
        })
        .join();
    }
}

impl TestContext {
    /// A context configured the way a real instance is out of the box.
    pub async fn new(label: &str) -> Self {
        Self::with_integrity_proofs(label, eunha::config::default_sign_integrity_proofs()).await
    }

    /// A context that signs (or does not sign) FEP-8b32 integrity proofs,
    /// whatever the shipped default happens to be.
    pub async fn with_integrity_proofs(label: &str, sign_integrity_proofs: bool) -> Self {
        Self::build(label, sign_integrity_proofs, false).await
    }

    /// A context whose instance reviews new accounts before they may sign in.
    /// `approval_required` is read off the config at startup, so a test that
    /// needs it has to ask for it here rather than set it afterwards.
    pub async fn with_approval_required(label: &str) -> Self {
        Self::build(label, eunha::config::default_sign_integrity_proofs(), true).await
    }

    async fn build(label: &str, sign_integrity_proofs: bool, approval_required: bool) -> Self {
        // Make fanout/populate/backfill run inline so tests don't race with background tasks.
        eunha::feed::enable_sync_fanout();
        eunha::moderation::signup::skip_mx_check();
        // Likewise for inbound activities: handle them in the request rather
        // than on the ingress queue, so a POST to /inbox has taken effect by
        // the time it returns.
        eunha::api::ap::inbox::enable_sync_ingress();

        // Use a unique subdomain per test so instances are isolated.
        let uid = &Uuid::new_v4().to_string()[..8];
        let domain = format!("{}-{}.c2s-test.invalid", label, uid);

        let admin_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");

        // Each test runs against its own freshly-migrated database. Local
        // accounts have a NULL domain, so the global (username, domain)
        // uniqueness constraint means "alice"/"bob" can only exist once per
        // database — isolating tests by database keeps them independent.
        let test_db_name = format!("c2s_{uid}");
        let base_opts = PgConnectOptions::from_str(&admin_url).expect("invalid DATABASE_URL");
        {
            let admin = PgPoolOptions::new()
                .max_connections(1)
                .connect_with(base_opts.clone().database("postgres"))
                .await
                .expect("connect to maintenance database");
            sqlx::query(&format!("CREATE DATABASE \"{test_db_name}\""))
                .execute(&admin)
                .await
                .expect("create per-test database");
            admin.close().await;
        }

        let db_opts = base_opts.database(&test_db_name);
        let db_url = replace_db_name(&admin_url, &test_db_name);

        // Migrate the way the server does. `eunha::tenants::connect` resolves
        // unqualified names against the eunha schema first, which is where
        // sqlx's migration ledger then lives. Migrating on a pool without that
        // search path leaves the ledger in `public`, and a server opened on the
        // same database finds none of its own and calls every migration
        // pending — which is what a tenant started from one of these databases
        // did, until this used the same path.
        let db = eunha::tenants::connect(
            &db_url,
            &eunha::config::DatabasePoolConfig {
                max_connections: 5,
                ..Default::default()
            },
        )
        .await
        .expect("connect to per-test database");
        eunha::migrate::run(&db)
            .await
            .expect("run migrations on per-test database");

        let (alice_id, alice_token) = seed_user(&db, &domain, "alice", "alice@test.invalid").await;
        let (bob_id, bob_token) = seed_user(&db, &domain, "bob", "bob@test.invalid").await;

        // Keep a separate pool for test-side operations (seeding scoped tokens etc.)
        // since `db` is moved into AppState below.
        let test_db = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(db_opts)
            .await
            .expect("failed to connect to test database (test_db)");

        // A database of its own, so a run cannot read or overwrite whatever a
        // development Redis holds. Isolation between *tests* still depends on
        // the keys themselves: eunha derives them from actor and object URIs,
        // and `TestContext` gives each test a unique domain, so a test that
        // builds its URIs from `ctx.domain` gets unique keys. One that hardcodes
        // a domain does not, and will read the key its previous run left behind
        // — which is how a test here once passed with the code it covers
        // deleted.
        let redis_url =
            std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379/15".into());
        let fake_s3 = spawn_fake_s3().await;
        let (vapid_private_key, vapid_public_key) =
            eunha::push::generate_vapid_keypair().expect("generate test VAPID keypair");
        let config = eunha::config::Config {
            database_url: db_url,
            pooled_database_url: None,
            pooled_client_slots: None,
            redis_url,
            redis_coordination_url: None,
            redis_key_prefix: String::new(),
            redis_process_metrics: true,
            database_pool: Default::default(),
            // Nothing declared: the tests exercise the default, which refuses
            // every private address.
            allowed_private_networks: Vec::new(),
            bind_address: "127.0.0.1:0".into(),
            media_storage: eunha::config::MediaStorageConfig {
                bucket: "test-bucket".into(),
                key_prefix: String::new(),
                region: "us-east-1".into(),
                endpoint: Some(fake_s3.clone()),
                access_key_id: "test-key".into(),
                secret_access_key: "test-secret".into(),
                base_url: fake_s3,
            },
            smtp: None,
            resend: eunha::config::ResendConfig {
                api_key: "test-key".into(),
                from: "test@test.invalid".into(),
            },
            instance: eunha::config::InstanceConfig {
                domain: domain.clone(),
                aliases: Vec::new(),
                previous_domains: vec![format!("old-{domain}")],
                title: "c2s test".into(),
                description: String::new(),
                short_description: String::new(),
                contact_email: None,
                registrations_open: true,
                approval_required,
                vapid_private_key,
                vapid_public_key,
                icon_url: None,
                privacy_policy: String::new(),
                terms_of_service: String::new(),
            },
            // Exercise the same path a Mastodon 4.7 database uses: local
            // signing keys in `keypairs`, encrypted with these secrets.
            active_record_encryption: Some(eunha::config::ActiveRecordEncryptionConfig {
                primary_key: "0123456789abcdef0123456789abcdef".into(),
                key_derivation_salt: "fedcba9876543210fedcba9876543210".into(),
            }),
            // Never reach for a third party's update server in tests.
            software_update_url: None,
            sign_integrity_proofs,
            workers: Default::default(),
            limits: Default::default(),
        };
        let state = eunha::state::AppState::new(db, config)
            .await
            .expect("failed to initialize AppState");
        let state_clone = state.clone();
        let app = eunha::build_app().layer(axum::Extension(state));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let base_url = format!("http://{}", addr);
        let api = ApiClient::new(&base_url, &domain);

        TestContext {
            api,
            domain,
            alice_token,
            alice_id: alice_id.to_string(),
            bob_token,
            bob_id: bob_id.to_string(),
            db: test_db,
            state: state_clone,
            _server: server,
            admin_url,
            test_db_name,
        }
    }
}

// ── database seeding helpers ────────────────────────────────────────────────

pub async fn seed_user(db: &PgPool, domain: &str, username: &str, email: &str) -> (i64, String) {
    seed_account_and_token(db, domain, username, email).await
}

pub async fn seed_account_and_token(
    db: &PgPool,
    domain: &str,
    username: &str,
    email: &str,
) -> (i64, String) {
    let url = format!("https://{}/{}", domain, username);
    let uri = format!("https://{}/users/{}", domain, username);

    let account_id = sqlx::query_scalar!(
        r#"INSERT INTO accounts
             (id, username, display_name, note,
              url, uri, public_key, inbox_url, outbox_url, shared_inbox_url, discoverable,
              id_scheme, created_at, updated_at)
           VALUES ($1,$2,$2,'', $3,$4::text,'test-public-key',$4::text||'/inbox',$4::text||'/outbox',''::text, true,
                   0, now(), now())
           RETURNING id"#,
        eunha::snowflake::next_id(),
        username,
        url,
        uri,
    )
    .fetch_one(db)
    .await
    .unwrap();

    let encrypted_password = hash_password("testpassword123");
    let user_id: i64 = sqlx::query_scalar!(
        r#"INSERT INTO users
             (account_id, email, encrypted_password, confirmed_at, approved, created_at, updated_at)
           VALUES ($1,$2,$3,now(),true,now(),now())
           RETURNING id"#,
        account_id,
        email,
        encrypted_password,
    )
    .fetch_one(db)
    .await
    .unwrap();

    let app_id: i64 = sqlx::query_scalar!(
        r#"INSERT INTO oauth_applications
             (name, uid, secret, redirect_uri, scopes)
           VALUES ('test',gen_random_uuid()::text,gen_random_uuid()::text,'urn:ietf:wg:oauth:2.0:oob','read write follow')
           RETURNING id"#,
    )
    .fetch_one(db)
    .await
    .unwrap();

    let token = Uuid::new_v4().to_string().replace("-", "");
    sqlx::query!(
        "INSERT INTO oauth_access_tokens (application_id, resource_owner_id, token, scopes, created_at) VALUES ($1,$2,$3,'read write follow push', now())",
        app_id,
        user_id,
        token,
    )
    .execute(db)
    .await
    .unwrap();

    (account_id, token)
}

pub fn hash_password(password: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .unwrap()
        .to_string()
}

/// Minimal 1×1 PNG image (valid, 67 bytes), for media-upload tests.
pub fn tiny_png() -> Vec<u8> {
    vec![
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, // PNG signature
        0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, // IHDR chunk length + type
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // width=1, height=1
        0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, // bit depth, color, crc
        0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, // IDAT chunk
        0x54, 0x08, 0xd7, 0x63, 0xf8, 0xcf, 0xc0, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01, 0xe2, 0x21,
        0xbc, 0x33, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, // IEND chunk
        0x44, 0xae, 0x42, 0x60, 0x82,
    ]
}

/// A 64×32 JPEG stored the way a camera held upright stores it: sideways, red
/// on the left and blue on the right, with EXIF orientation 6 saying to turn it
/// clockwise into a 32×64 portrait.
pub fn sideways_jpeg() -> Vec<u8> {
    use image::{codecs::jpeg::JpegEncoder, ImageEncoder, Rgb, RgbImage};

    let mut exif = b"MM\0\x2a\0\0\0\x08".to_vec();
    exif.extend_from_slice(&[0, 1, 0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0, 0, 0, 0, 0]);
    let image = RgbImage::from_fn(64, 32, |x, _| {
        if x < 32 {
            Rgb([255, 0, 0])
        } else {
            Rgb([0, 0, 255])
        }
    });
    let mut bytes = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut bytes, 95);
    encoder.set_exif_metadata(exif).unwrap();
    encoder.encode_image(&image).unwrap();
    bytes
}

/// Grant a user admin privileges: a role carrying the `administrator` flag,
/// and its tokens the `admin:read` and `admin:write` scopes the admin API
/// asks for.
pub async fn make_admin(db: &PgPool, account_id: i64) {
    let role_id = sqlx::query_scalar!(
        r#"INSERT INTO user_roles (id, name, position, permissions, highlighted, created_at, updated_at)
           VALUES ($1, 'Admin', 100, 1, true, now(), now())
           RETURNING id"#,
        eunha::snowflake::next_id(),
    )
    .fetch_one(db)
    .await
    .unwrap();

    sqlx::query!(
        "UPDATE users SET role_id = $1 WHERE account_id = $2",
        role_id,
        account_id,
    )
    .execute(db)
    .await
    .unwrap();
    grant_admin_scopes(db, account_id).await;
}

/// Add `admin:read admin:write` to every token of `account_id`.
pub async fn grant_admin_scopes(db: &PgPool, account_id: i64) {
    let user_id = user_id_for(db, account_id).await;
    sqlx::query!(
        "UPDATE oauth_access_tokens SET scopes = scopes || ' admin:read admin:write' WHERE resource_owner_id = $1",
        user_id,
    )
    .execute(db)
    .await
    .unwrap();
}

/// Look up the `users.id` for a given account.
pub async fn user_id_for(db: &PgPool, account_id: i64) -> i64 {
    sqlx::query_scalar!("SELECT id FROM users WHERE account_id = $1", account_id)
        .fetch_one(db)
        .await
        .unwrap()
}

/// Create an additional access token for `account_id` with the given scopes.
/// Use this to test scope enforcement (e.g. a read-only token trying a write endpoint).
pub async fn seed_token_with_scopes(db: &PgPool, account_id: i64, scopes: &str) -> String {
    let user_id = user_id_for(db, account_id).await;
    let app_id: i64 = sqlx::query_scalar!(
        r#"SELECT application_id as "application_id!: i64" FROM oauth_access_tokens
           WHERE resource_owner_id = $1 AND application_id IS NOT NULL LIMIT 1"#,
        user_id,
    )
    .fetch_one(db)
    .await
    .unwrap();

    let token = Uuid::new_v4().to_string().replace("-", "");
    sqlx::query!(
        "INSERT INTO oauth_access_tokens (application_id, resource_owner_id, token, scopes, created_at) VALUES ($1,$2,$3,$4, now())",
        app_id,
        user_id,
        token,
        scopes,
    )
    .execute(db)
    .await
    .unwrap();

    token
}

/// Set one of Mastodon's site settings (`Setting[var] = value`), YAML-encoded
/// the way `Setting#value=` stores it.
pub async fn set_setting(db: &PgPool, var: &str, yaml: &str) {
    sqlx::query(
        "INSERT INTO settings (var, value, created_at, updated_at) VALUES ($1, $2, now(), now())
         ON CONFLICT (var) DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
    )
    .bind(var)
    .bind(format!("--- {yaml}\n"))
    .execute(db)
    .await
    .unwrap();
}

/// What trends need to show anything without review: `trendable_by_default`,
/// and the test accounts discoverable.
pub async fn open_trends(db: &PgPool) {
    set_setting(db, "trendable_by_default", "true").await;
    sqlx::query("UPDATE accounts SET discoverable = true WHERE domain IS NULL")
        .execute(db)
        .await
        .unwrap();
}
