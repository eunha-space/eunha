//! What other servers fetch — actors, statuses, their collections, WebFinger,
//! host-meta and NodeInfo — served by feder.
//!
//! One [`Federation`] serves every instance in the process: the instance a
//! request is for rides on it as its [`AppState`], put there by the tenant
//! dispatcher, and is the federation's data. Each route is a dispatcher
//! registered with the template that also names it, and each is registered
//! twice where Mastodon serves an account under both its URI schemes,
//! `/users/{username}` and `/ap/users/{id}`. Whichever it is asked under, an
//! actor and its collections are named by the scheme the account uses.
//!
//! A request to one of these paths that asks for a page rather than
//! ActivityPub goes on to eunha's own routes (`super::router`), which send a
//! browser to the profile or the status.
//!
//! The inboxes are feder's too. What arrives in them is authenticated by
//! feder, with the keys eunha already holds in `accounts` tried first, and
//! handed to eunha's own dispatcher (`super::inbox::received`) reduced to
//! what its sender can vouch for.

use feder::federation::{
    ActorRef, Collection, Context, Federation, First, Found, NodeInfo, Page, Software, Usage,
};
use feder::template::Values;
use serde_json::{json, Value};
use url::Url;

use super::objects::{load_local_account, AccountRef};
use crate::db::models::Account;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// How many entries feder's in-memory store may hold, for the whole process.
/// Nearly all are the IDs of activities received, which feder remembers for a
/// day to drop a redelivery; past this it forgets the ones nearest to expiry
/// first, and a redelivery it no longer recognises is processed again, which
/// every activity's effect on the database already tolerates.
const KV_CAPACITY: usize = 100_000;

type Ctx = Context<AppState>;

/// The federation every instance in the process is served through.
///
/// # Panics
///
/// When a template is wrong, which the tests would have found.
#[must_use]
pub fn federation() -> Federation<AppState> {
    let mut builder = Federation::builder()
        .origin_with(|host, state: &AppState| {
            let instance = &state.instance;
            let ours = host.eq_ignore_ascii_case(&instance.domain)
                || instance
                    .aliases
                    .iter()
                    .any(|alias| host.eq_ignore_ascii_case(alias));
            ours.then(|| Url::parse(&format!("https://{}", instance.domain)).ok())
                .flatten()
        })
        .actor("instance", "/actor", |ctx: Ctx, _: String| async move {
            super::objects::instance_actor_json(ctx.data())
                .await
                .map(Found::Found)
        })
        .object(
            "collection",
            "/collections/{id}",
            |ctx: Ctx, values: Values| async move {
                let Some(id) = number(&values["id"]) else {
                    return Ok(Found::NotFound);
                };
                found(super::collections::collection_document(ctx.data(), domain(&ctx), id).await)
            },
        )
        .object(
            "feature_authorization",
            "/users/{username}/feature_authorizations/{id}",
            |ctx: Ctx, values: Values| async move {
                let Some(id) = number(&values["id"]) else {
                    return Ok(Found::NotFound);
                };
                found(
                    super::collections::feature_authorization_document(
                        ctx.data(),
                        domain(&ctx),
                        &values["username"],
                        id,
                    )
                    .await,
                )
            },
        )
        .object(
            "quote_authorization",
            "/users/{username}/quote_authorizations/{id}",
            |ctx: Ctx, values: Values| async move {
                let Some(id) = number(&values["id"]) else {
                    return Ok(Found::NotFound);
                };
                found(
                    super::collections::quote_authorization_document(
                        ctx.data(),
                        domain(&ctx),
                        &values["username"],
                        id,
                    )
                    .await,
                )
            },
        )
        .handle(|ctx: Ctx, username: String| async move { by_username(&ctx, &username).await })
        .map_alias(|ctx: Ctx, url: Url| async move {
            // A profile page, /@username, names whom the handle does.
            match url.path().strip_prefix("/@") {
                Some(username) if !username.is_empty() && !username.contains('/') => {
                    by_username(&ctx, username).await
                }
                _ => Ok(None),
            }
        })
        .webfinger_links(|ctx, _, actor| {
            // Mastodon's remote follow: where to send someone who wants to
            // interact with this account from their own server.
            match actor.get("preferredUsername").and_then(Value::as_str) {
                Some(username) if actor.get("type") != Some(&json!("Application")) => {
                    vec![json!({
                        "rel": "http://ostatus.org/schema/1.0/subscribe",
                        "template": format!(
                            "https://{}/@{username}/authorize_interaction?uri={{uri}}",
                            domain(ctx)
                        ),
                    })]
                }
                _ => Vec::new(),
            }
        })
        .nodeinfo(nodeinfo)
        .on_error(|error| tracing::error!(error = %error, "ActivityPub"))
        .inbox("actor", "/users/{username}/inbox")
        .inbox("actor_by_id", "/ap/users/{id}/inbox")
        .shared_inbox("/inbox")
        // Every tenant fetches with its own fetcher (`fetcher_for`); this
        // one is only what the builder needs to be given. The keys of
        // accounts eunha knows come from `accounts`, where a new actor's is
        // stored as it is fetched, so feder caches only what eunha does not
        // keep. What it does cache — the IDs of activities seen, replies
        // forwarded, keys eunha did not store — is one store for the process,
        // bounded so that a day of every tenant's traffic is not all held in
        // memory at once.
        .signed_fetch(
            std::sync::Arc::new(feder::fetch::Fetcher::new(
                feder::client::Client::new(feder::client::ClientConfig::default())
                    .expect("an HTTP client"),
                feder::delivery::Scheme::DraftCavage,
            )),
            feder::kv::MemoryKvStore::with_capacity(KV_CAPACITY),
            std::time::Duration::from_secs(60 * 60),
            // Signed as the instance actor, for peers in authorized-fetch mode.
            |ctx: Ctx| async move {
                crate::federation::fetch::instance_key(ctx.data())
                    .await
                    .map(Some)
            },
        )
        .fetcher_for(|state: &AppState| state.fetcher.clone())
        .known_key(|ctx: Ctx, key_id: String| async move { known_key(&ctx, &key_id).await })
        // An actor seen for the first time is created from the document
        // fetched for its key, as Mastodon does, rather than fetched again
        // by the worker for the activity it sent. Two first activities race
        // to create it; the loser's insert fails on the account's uniqueness,
        // and its worker finds the winner's row.
        .key_fetched(|ctx: Ctx, actor: Value| async move {
            let Some(id) = actor.get("id").and_then(Value::as_str).map(str::to_owned) else {
                return;
            };
            if actor.get("inbox").is_none() {
                return;
            }
            if let Err(error) =
                super::inbox::resolve_or_fetch_remote_account_prefetched(ctx.data(), &id, actor)
                    .await
            {
                tracing::debug!(actor = %id, %error, "account not created from its key fetch");
            }
        })
        // A suspended domain's activities are dropped before any key is
        // fetched for them, as Mastodon drops them.
        .blocked(|ctx: Ctx, host: String| async move {
            Ok::<_, AppError>(
                crate::federation::moderation::actor_is_suspended(
                    ctx.data(),
                    &format!("https://{host}/"),
                )
                .await,
            )
        })
        // Activities are read as JSON, as Mastodon reads them, never
        // expanded and compacted (docs/design/protocol.md, "JSON-LD in shape,
        // never in processing"). The one listener reads the activity as its
        // sender wrote it, so normalising it was work nothing used: under a
        // viral post, more than half the CPU.
        .read_inbox_as_written()
        .on_any(|ctx: Ctx, received: feder::federation::Received<feder_vocab::generated::AnyObject>| async move {
            // Which inbox a peer chose is otherwise invisible: Mastodon picks
            // the shared one only when two accounts here follow the same actor
            // there, and the federation harness checks that path is exercised.
            tracing::debug!(
                inbox = %if received.recipient.is_some() { "personal" } else { "shared" },
                sender = %received.sender,
                activity_type = received.vouched.get("type").and_then(serde_json::Value::as_str).unwrap_or(""),
                "received ActivityPub activity"
            );
            super::inbox::received(ctx.data(), received.vouched).await
        })
        // A reply to a local post, addressed to its author's followers, is
        // passed on to them (ActivityPub §7.1.2), signed by the author.
        .forward(|ctx: Ctx, forward: feder::federation::Forward| async move {
            forward_to_collections(&ctx, forward).await.map_err(|error| error.to_string())
        });

    for scheme in [Scheme::Username, Scheme::Id] {
        builder =
            builder
                .actor(
                    &scheme.kind("actor"),
                    &scheme.template(""),
                    move |ctx: Ctx, identifier: String| async move {
                        let Some(account) = scheme.account(&ctx, &identifier).await? else {
                            return Ok::<_, AppError>(Found::NotFound);
                        };
                        found(super::objects::actor_json(ctx.data(), domain(&ctx), &account).await)
                    },
                )
                .object(
                    &scheme.kind("status"),
                    &scheme.template("/statuses/{status_id}"),
                    move |ctx: Ctx, values: Values| async move {
                        status(&ctx, scheme, &values, false).await
                    },
                )
                .object(
                    &scheme.kind("status_activity"),
                    &scheme.template("/statuses/{status_id}/activity"),
                    move |ctx: Ctx, values: Values| async move {
                        status(&ctx, scheme, &values, true).await
                    },
                )
                .object(
                    &scheme.kind("account_collections"),
                    &scheme.template("/collections"),
                    move |ctx: Ctx, values: Values| async move {
                        let who = scheme.who(values.single().unwrap_or_default());
                        let Some(who) = who else {
                            return Ok(Found::NotFound);
                        };
                        found(
                            super::collections::account_collections(ctx.data(), domain(&ctx), who)
                                .await,
                        )
                    },
                )
                .collection(
                    &scheme.kind("outbox"),
                    &scheme.template("/outbox"),
                    outbox(scheme),
                )
                .collection(
                    &scheme.kind("followers"),
                    &scheme.template("/followers"),
                    relation(scheme, Relation::Followers),
                )
                .collection(
                    &scheme.kind("following"),
                    &scheme.template("/following"),
                    relation(scheme, Relation::Following),
                )
                .collection(
                    &scheme.kind("featured"),
                    &scheme.template("/collections/featured"),
                    featured(scheme),
                );
    }
    builder.build().expect("eunha's federation is well formed")
}

/// Forward `forward`'s activity to the followers of the local accounts whose
/// followers collections it names. Eunha is no portable actor's gateway, so
/// there is nothing to forward to gateways.
async fn forward_to_collections(ctx: &Ctx, forward: feder::federation::Forward) -> AppResult<()> {
    let feder::federation::ForwardTo::Collections(collections) = forward.to else {
        return Ok(());
    };
    for collection in collections {
        let scheme = if collection.kind == Scheme::Username.kind("followers") {
            Scheme::Username
        } else if collection.kind == Scheme::Id.kind("followers") {
            Scheme::Id
        } else {
            continue;
        };
        let Some(account) = scheme.account(ctx, &collection.identifier).await? else {
            continue;
        };
        let actor = crate::federation::tag::account_uri_of(domain(ctx), &account);
        crate::federation::delivery::forward_to_followers(
            ctx.data(),
            forward.activity.clone(),
            account.id,
            format!("{actor}#main-key"),
        )
        .await
        .map_err(AppError::Internal)?;
    }
    Ok(())
}

/// The key eunha holds for `key_id`: the public key of the remote account
/// whose actor the key ID names.
async fn known_key(ctx: &Ctx, key_id: &str) -> AppResult<Option<feder::federation::KnownKey>> {
    let owner = feder_runtime::verification::key_owner(key_id);
    let pem = sqlx::query_scalar!(
        "SELECT public_key FROM accounts WHERE uri = $1 AND domain IS NOT NULL AND public_key != ''",
        owner,
    )
    .fetch_optional(&ctx.data().db)
    .await?;
    Ok(pem.and_then(|pem| {
        Some(feder::federation::KnownKey {
            pem,
            actor: Url::parse(owner).ok()?,
        })
    }))
}

fn domain(ctx: &Ctx) -> &str {
    &ctx.data().instance.domain
}

fn number(value: &str) -> Option<i64> {
    value.parse().ok()
}

/// A document, or `NotFound` and `Gone` as feder says them.
fn found(result: AppResult<Value>) -> AppResult<Found<Value>> {
    match result {
        Ok(document) => Ok(Found::Found(document)),
        Err(AppError::NotFound) => Ok(Found::NotFound),
        Err(AppError::Gone(_)) => Ok(Found::Gone(None)),
        Err(error) => Err(error),
    }
}

/// Which of Mastodon's two URI schemes a route is under.
#[derive(Clone, Copy)]
enum Scheme {
    /// `/users/{username}`, Mastodon's `username_ap_id`.
    Username,
    /// `/ap/users/{id}`, Mastodon's `numeric_ap_id`.
    Id,
}

impl Scheme {
    fn kind(self, what: &str) -> String {
        match self {
            Self::Username => what.to_owned(),
            Self::Id => format!("{what}_by_id"),
        }
    }

    fn template(self, suffix: &str) -> String {
        match self {
            Self::Username => format!("/users/{{username}}{suffix}"),
            Self::Id => format!("/ap/users/{{id}}{suffix}"),
        }
    }

    fn who(self, identifier: &str) -> Option<AccountRef<'_>> {
        match self {
            Self::Username => Some(AccountRef::Username(identifier)),
            Self::Id => number(identifier).map(AccountRef::Id),
        }
    }

    /// The local account `identifier` names under this scheme.
    async fn account(self, ctx: &Ctx, identifier: &str) -> AppResult<Option<Account>> {
        let Some(who) = self.who(identifier) else {
            return Ok(None);
        };
        match load_local_account(ctx.data(), who).await {
            Ok(account) => Ok(Some(account)),
            Err(AppError::NotFound) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The URI of `suffix` beneath the account's own actor URI, whichever
    /// scheme it was asked under.
    async fn own_uri(self, ctx: &Ctx, identifier: &str, suffix: &str) -> AppResult<Option<Url>> {
        Ok(self.account(ctx, identifier).await?.and_then(|account| {
            let actor = crate::federation::tag::account_uri_of(domain(ctx), &account);
            Url::parse(&format!("{actor}{suffix}")).ok()
        }))
    }
}

async fn status(
    ctx: &Ctx,
    scheme: Scheme,
    values: &Values,
    activity: bool,
) -> AppResult<Found<Value>> {
    let identifier = match scheme {
        Scheme::Username => &values["username"],
        Scheme::Id => &values["id"],
    };
    let (Some(who), Some(status_id)) = (scheme.who(identifier), number(&values["status_id"]))
    else {
        return Ok(Found::NotFound);
    };
    let bundle = super::objects::status_bundle(ctx.data(), domain(ctx), who, status_id).await;
    found(bundle.map(|bundle| {
        if activity {
            bundle.into_create()
        } else {
            bundle.into_note()
        }
    }))
}

/// An outbox cursor: the newest page, or the page before or after a status.
enum OutboxCursor {
    Newest,
    Below(i64),
    Above(i64),
}

impl OutboxCursor {
    fn parse(cursor: &str) -> Option<Self> {
        if cursor.is_empty() {
            return Some(Self::Newest);
        }
        let (direction, id) = cursor.split_once(':')?;
        let id = number(id)?;
        match direction {
            "max" => Some(Self::Below(id)),
            "min" => Some(Self::Above(id)),
            _ => None,
        }
    }
}

/// An account's own public and unlisted statuses, newest first, as the
/// `Create` activities that posted them; boosts are not in it.
fn outbox(scheme: Scheme) -> Collection<AppState> {
    Collection::new(
        move |ctx: Ctx, identifier: String, cursor: Option<String>| async move {
            let Some(account) = scheme.account(&ctx, &identifier).await? else {
                return Ok::<_, AppError>(None);
            };
            let Some(cursor) = OutboxCursor::parse(cursor.as_deref().unwrap_or_default()) else {
                return Ok(None);
            };
            let (max_id, min_id) = match cursor {
                OutboxCursor::Newest => (None, None),
                OutboxCursor::Below(id) => (Some(id), None),
                OutboxCursor::Above(id) => (None, Some(id)),
            };
            let state = ctx.data();
            let status_ids: Vec<i64> = sqlx::query_scalar!(
                r#"SELECT s.id
                   FROM statuses s
                   WHERE s.account_id = $1
                     AND s.deleted_at IS NULL
                     AND s.reblog_of_id IS NULL
                     AND s.visibility IN (0, 1) /* vis::PUBLIC, vis::UNLISTED */
                     AND ($2::bigint IS NULL OR s.id < $2)
                     AND ($3::bigint IS NULL OR s.id > $3)
                   ORDER BY s.id DESC
                   LIMIT 20"#,
                account.id,
                max_id,
                min_id,
            )
            .fetch_all(&state.db)
            .await?;
            let mut items = Vec::with_capacity(status_ids.len());
            for id in &status_ids {
                if let Some(bundle) = super::note::build_note(state, domain(&ctx), *id).await? {
                    items.push(bundle.into_create());
                }
            }
            Ok(Some(Page {
                items,
                next: status_ids.last().map(|id| format!("max:{id}")),
                prev: status_ids.first().map(|id| format!("min:{id}")),
            }))
        },
    )
    .count(move |ctx: Ctx, identifier: String| async move {
        let Some(account) = scheme.account(&ctx, &identifier).await? else {
            return Ok::<_, AppError>(None);
        };
        let count = sqlx::query_scalar!(
            "SELECT COALESCE(statuses_count, 0) FROM account_stats WHERE account_id = $1",
            account.id,
        )
        .fetch_optional(&ctx.data().db)
        .await?
        .flatten()
        .unwrap_or(0);
        Ok(Some(u64::try_from(count).unwrap_or(0)))
    })
    .first_cursor(move |ctx: Ctx, identifier: String| async move {
        Ok::<_, AppError>(
            scheme
                .account(&ctx, &identifier)
                .await?
                .map(|_| First::At(String::new())),
        )
    })
    .last_cursor(|_, _| async { Ok::<_, AppError>(Some("min:0".to_owned())) })
    .uri(move |ctx: Ctx, identifier: String| async move {
        scheme.own_uri(&ctx, &identifier, "/outbox").await
    })
}

#[derive(Clone, Copy)]
enum Relation {
    Followers,
    Following,
}

impl Relation {
    fn suffix(self) -> &'static str {
        match self {
            Self::Followers => "/followers",
            Self::Following => "/following",
        }
    }
}

const RELATION_PAGE: i64 = 40;

/// Who follows an account, or whom it follows, newest follow first, forty to
/// a page. An account that hides its collections shows the count and not
/// the members.
fn relation(scheme: Scheme, relation: Relation) -> Collection<AppState> {
    Collection::new(
        move |ctx: Ctx, identifier: String, cursor: Option<String>| async move {
            let Some(account) = scheme.account(&ctx, &identifier).await? else {
                return Ok::<_, AppError>(None);
            };
            if account.hide_collections.unwrap_or(false) {
                return Ok(Some(Page::default()));
            }
            let max_id = match cursor.as_deref().unwrap_or_default() {
                "" => None,
                cursor => match number(cursor) {
                    Some(id) => Some(id),
                    None => return Ok(None),
                },
            };
            let state = ctx.data();
            let domain = domain(&ctx);
            let rows: Vec<(i64, String)> = match relation {
                Relation::Followers => sqlx::query!(
                    r#"SELECT f.id, a.id AS account_id, a.id_scheme, a.uri AS account_uri, a.username, (a.domain IS NULL) AS "is_local!"
                       FROM follows f JOIN accounts a ON a.id = f.account_id
                       WHERE f.target_account_id = $1 AND ($2::bigint IS NULL OR f.id < $2)
                       ORDER BY f.id DESC LIMIT $3"#,
                    account.id,
                    max_id,
                    RELATION_PAGE,
                )
                .fetch_all(&state.db)
                .await?
                .into_iter()
                .map(|r| (r.id, super::collections::resolve_actor_uri(domain, r.account_uri, r.is_local, r.account_id, r.id_scheme, &r.username)))
                .collect(),
                Relation::Following => sqlx::query!(
                    r#"SELECT f.id, a.id AS account_id, a.id_scheme, a.uri AS account_uri, a.username, (a.domain IS NULL) AS "is_local!"
                       FROM follows f JOIN accounts a ON a.id = f.target_account_id
                       WHERE f.account_id = $1 AND ($2::bigint IS NULL OR f.id < $2)
                       ORDER BY f.id DESC LIMIT $3"#,
                    account.id,
                    max_id,
                    RELATION_PAGE,
                )
                .fetch_all(&state.db)
                .await?
                .into_iter()
                .map(|r| (r.id, super::collections::resolve_actor_uri(domain, r.account_uri, r.is_local, r.account_id, r.id_scheme, &r.username)))
                .collect(),
            };
            let next = (rows.len() as i64 == RELATION_PAGE)
                .then(|| rows.last().map(|(id, _)| id.to_string()))
                .flatten();
            Ok(Some(Page {
                items: rows.into_iter().map(|(_, uri)| Value::String(uri)).collect(),
                next,
                prev: None,
            }))
        },
    )
    .count(move |ctx: Ctx, identifier: String| async move {
        let Some(account) = scheme.account(&ctx, &identifier).await? else {
            return Ok::<_, AppError>(None);
        };
        let total = match relation {
            Relation::Followers => sqlx::query_scalar!(
                "SELECT COUNT(*) FROM follows WHERE target_account_id = $1",
                account.id,
            )
            .fetch_one(&ctx.data().db)
            .await?,
            Relation::Following => sqlx::query_scalar!(
                "SELECT COUNT(*) FROM follows WHERE account_id = $1",
                account.id,
            )
            .fetch_one(&ctx.data().db)
            .await?,
        }
        .unwrap_or(0);
        Ok(Some(u64::try_from(total).unwrap_or(0)))
    })
    .first_cursor(move |ctx: Ctx, identifier: String| async move {
        Ok::<_, AppError>(scheme.account(&ctx, &identifier).await?.map(|account| {
            if account.hide_collections.unwrap_or(false) {
                First::Hidden
            } else {
                First::At(String::new())
            }
        }))
    })
    .uri(move |ctx: Ctx, identifier: String| async move {
        scheme.own_uri(&ctx, &identifier, relation.suffix()).await
    })
}

/// An account's pinned, publicly visible statuses, newest pin first, in one
/// document (Mastodon's `featured`).
fn featured(scheme: Scheme) -> Collection<AppState> {
    Collection::new(
        move |ctx: Ctx, identifier: String, _: Option<String>| async move {
            let Some(account) = scheme.account(&ctx, &identifier).await? else {
                return Ok::<_, AppError>(None);
            };
            let rows = sqlx::query!(
                r#"SELECT s.id, s.uri AS "uri?"
                   FROM status_pins p JOIN statuses s ON s.id = p.status_id
                   WHERE p.account_id = $1 AND s.deleted_at IS NULL AND s.visibility IN (0, 1)
                   ORDER BY p.id DESC"#,
                account.id,
            )
            .fetch_all(&ctx.data().db)
            .await?;
            let actor = crate::federation::tag::account_uri_of(domain(&ctx), &account);
            Ok(Some(Page {
                items: rows
                    .into_iter()
                    .map(|r| {
                        Value::String(
                            r.uri
                                .filter(|u| !u.is_empty())
                                .unwrap_or_else(|| format!("{actor}/statuses/{}", r.id)),
                        )
                    })
                    .collect(),
                ..Page::default()
            }))
        },
    )
    .uri(move |ctx: Ctx, identifier: String| async move {
        scheme
            .own_uri(&ctx, &identifier, "/collections/featured")
            .await
    })
}

/// The actor a WebFinger username names: a local account, under the scheme
/// it uses, or the instance actor, whose username is the domain.
async fn by_username(ctx: &Ctx, username: &str) -> AppResult<Option<ActorRef>> {
    if username.eq_ignore_ascii_case(domain(ctx)) {
        return Ok(Some(ActorRef::new("instance", "")));
    }
    let account = sqlx::query!(
        "SELECT id, username, id_scheme FROM accounts WHERE username = $1 AND domain IS NULL",
        username,
    )
    .fetch_optional(&ctx.data().db)
    .await?;
    Ok(account.map(|account| {
        if account.id_scheme == Some(crate::federation::tag::NUMERIC_AP_ID) {
            ActorRef::new("actor_by_id", account.id.to_string())
        } else {
            ActorRef::new("actor", account.username)
        }
    }))
}

async fn nodeinfo(ctx: Ctx) -> AppResult<NodeInfo> {
    let state = ctx.data();
    let (user_count, active_month, active_halfyear, status_count) = tokio::try_join!(
        sqlx::query_scalar!(
            "SELECT COUNT(*) FROM accounts WHERE domain IS NULL AND suspended_at IS NULL AND requested_deletion_at IS NULL",
        )
        .fetch_one(&state.db),
        sqlx::query_scalar!(
            r#"SELECT COUNT(DISTINCT s.account_id) FROM statuses s
               WHERE s.account_id IN (
                   SELECT id FROM accounts WHERE domain IS NULL
               ) AND s.deleted_at IS NULL
                 AND s.created_at > now() - interval '30 days'"#,
        )
        .fetch_one(&state.db),
        sqlx::query_scalar!(
            r#"SELECT COUNT(DISTINCT s.account_id) FROM statuses s
               WHERE s.account_id IN (
                   SELECT id FROM accounts WHERE domain IS NULL
               ) AND s.deleted_at IS NULL
                 AND s.created_at > now() - interval '180 days'"#,
        )
        .fetch_one(&state.db),
        sqlx::query_scalar!(
            r#"SELECT COALESCE(SUM(ast.statuses_count), 0)::bigint
               FROM account_stats ast
               JOIN accounts a ON a.id = ast.account_id
               WHERE a.domain IS NULL"#,
        )
        .fetch_one(&state.db),
    )?;
    let count = |n: Option<i64>| u64::try_from(n.unwrap_or(0)).ok();
    let instance = &state.instance;
    let mut nodeinfo = NodeInfo::new(Software {
        name: "eunha".to_owned(),
        version: crate::version::EUNHA_FULL.to_owned(),
        repository: None,
        homepage: None,
    });
    nodeinfo.open_registrations = instance.registrations_open;
    nodeinfo.usage = Usage {
        users_total: count(user_count),
        users_active_month: count(active_month),
        users_active_halfyear: count(active_halfyear),
        local_posts: count(status_count),
        local_comments: None,
    };
    nodeinfo.metadata = json!({
        "nodeName": instance.title,
        "nodeDescription": instance.description,
    });
    Ok(nodeinfo)
}
