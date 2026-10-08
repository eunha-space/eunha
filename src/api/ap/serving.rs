//! What other servers fetch — actors, statuses, their collections, WebFinger,
//! host-meta and NodeInfo — served by ojak.
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
//! browser to the profile or the status. A status is served at its page,
//! `/@{username}/{id}`, too, and an account's followers and following at
//! `/@{username}/followers` and `/@{username}/following`, as Mastodon serves
//! them there.
//!
//! The inboxes are ojak's too. What arrives in them is authenticated by
//! ojak, with the keys eunha already holds in `accounts` tried first, and
//! handed to eunha's own dispatcher (`super::inbox::received`) reduced to
//! what its sender can vouch for.

use ojak::federation::{
    Access, ActorRef, Collection, Context, Federation, First, Found, NodeInfo, Page, Signing,
    Software, Uris, Usage, Values,
};
use serde_json::{json, Value};
use url::Url;

use super::objects::{load_local_account, AccountRef};
use crate::db::models::Account;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

type Ctx = Context<AppState>;

/// The URIs of what [`federation`] serves, in some origin: its templates,
/// built once for the process.
static URIS: std::sync::LazyLock<Uris> =
    std::sync::LazyLock::new(|| federation().uris(Url::parse("https://localhost").expect("a URL")));

/// The URIs of what eunha serves, in the instance at `domain`, built from
/// the templates its routes are registered with: for code outside a
/// request, which has no [`Context`] to ask. [`AppState::uris`] holds its
/// instance's.
///
/// # Errors
///
/// When `https://{domain}` is no URL.
pub fn uris(domain: &str) -> Result<Uris, url::ParseError> {
    Ok(URIS.with_origin(Url::parse(&format!("https://{domain}"))?))
}

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
            let actor = super::objects::instance_actor_json(ctx.data()).await?;
            expires_in(&ctx, Vary::Nothing, 600, true).await;
            Ok::<_, AppError>(Found::Found(actor))
        })
        .object(
            "collection",
            "/collections/{id}",
            |ctx: Ctx, values: Values| async move {
                let Some(id) = number(&values["id"]) else {
                    return Ok(Found::NotFound);
                };
                collection(&ctx, None, id).await
            },
        )
        .object(
            "feature_authorization",
            "/users/{username}/feature_authorizations/{id}",
            |ctx: Ctx, values: Values| async move {
                let Some(id) = number(&values["id"]) else {
                    return Ok(Found::NotFound);
                };
                feature_authorization(&ctx, AccountRef::Username(&values["username"]), id).await
            },
        )
        // What Mastodon serves only under `/ap/users/{id}`: an account's
        // collections, one of them, one of their items, and the stamp by
        // which a local account consented to being in one.
        .object(
            "featured_collections",
            "/ap/users/{id}/featured_collections",
            |ctx: Ctx, values: Values| async move {
                let Some(id) = number(&values["id"]) else {
                    return Ok::<_, AppError>(Found::NotFound);
                };
                let signer = signer(&ctx).await;
                let page = ctx.query_value("page");
                let document = found(
                    super::collections::featured_collections(
                        ctx.data(),
                        domain(&ctx),
                        id,
                        page.as_deref(),
                        signer.as_deref(),
                    )
                    .await,
                )?;
                if matches!(document, Found::Found(_)) {
                    // `expires_in(page_requested? ? 0 : 3.minutes, public: public_fetch_mode?)`.
                    let requested = page.is_some_and(|page| !page.is_empty());
                    let public = public_fetch_mode(&ctx).await;
                    expires_in(&ctx, Vary::Page, if requested { 0 } else { 180 }, public).await;
                }
                Ok(document)
            },
        )
        .object(
            "account_collection",
            "/ap/users/{id}/collections/{collection_id}",
            |ctx: Ctx, values: Values| async move {
                let (Some(owner), Some(id)) =
                    (number(&values["id"]), number(&values["collection_id"]))
                else {
                    return Ok(Found::NotFound);
                };
                collection(&ctx, Some(owner), id).await
            },
        )
        .object(
            "collection_item",
            "/ap/users/{id}/collection_items/{item_id}",
            |ctx: Ctx, values: Values| async move {
                let (Some(owner), Some(id)) = (number(&values["id"]), number(&values["item_id"]))
                else {
                    return Ok::<_, AppError>(Found::NotFound);
                };
                let signer = signer(&ctx).await;
                let document = found(
                    super::collections::collection_item_document(
                        ctx.data(),
                        domain(&ctx),
                        owner,
                        id,
                        signer.as_deref(),
                    )
                    .await,
                )?;
                if matches!(document, Found::Found(_)) {
                    let public = public_fetch_mode(&ctx).await;
                    expires_in(&ctx, Vary::Page, 180, public).await;
                }
                Ok(document)
            },
        )
        .object(
            "feature_authorization_by_id",
            "/ap/users/{id}/feature_authorizations/{item_id}",
            |ctx: Ctx, values: Values| async move {
                let (Some(account), Some(id)) =
                    (number(&values["id"]), number(&values["item_id"]))
                else {
                    return Ok(Found::NotFound);
                };
                feature_authorization(&ctx, AccountRef::Id(account), id).await
            },
        )
        // A thread started here, which its posts name as their `context`
        // (`ActivityPub::ContextsController`).
        .object("context", "/contexts/{id}", |ctx: Ctx, values: Values| async move {
            let query = super::status_collections::Query::parse(ctx.query());
            let document = super::status_collections::context(ctx.data(), &values["id"], &query).await;
            cached_for_public(&ctx, Vary::Signature, 180, found(document)).await
        })
        .object(
            "context_items",
            "/contexts/{id}/items",
            |ctx: Ctx, values: Values| async move {
                let query = super::status_collections::Query::parse(ctx.query());
                let document =
                    super::status_collections::context_items(ctx.data(), &values["id"], &query)
                        .await;
                cached_for_public(&ctx, Vary::Signature, 180, found(document)).await
            },
        )
        // The instance actor's outbox (`Account.representative`'s).
        .collection("instance_outbox", "/actor/outbox", instance_outbox())
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
            // Mastodon's avatar link (`show_avatar?`, which limited federation
            // mode turns off) has no counterpart: eunha's WebFinger names no
            // avatar in any mode.
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
        .inbox("instance", "/actor/inbox")
        .shared_inbox("/inbox")
        // Every tenant fetches with its own fetcher (`fetcher_for`) and
        // caches in its own Redis namespace (`kv_for`); the fetcher and store
        // given here are only what the builder needs to be given. The keys
        // of accounts eunha knows come from `accounts`, where a new actor's
        // is stored as it is fetched, so ojak caches only what eunha does not
        // keep: the IDs of activities seen, replies forwarded, keys eunha did
        // not store. In Redis, every process serving the instance shares
        // them, so a redelivery that reaches another process is still
        // dropped (docs/operating/redis.md).
        .signed_fetch(
            std::sync::Arc::new(ojak::fetch::Fetcher::new(
                ojak::client::Client::new(ojak::client::ClientConfig::default())
                    .expect("an HTTP client"),
                ojak::sig::Scheme::DraftCavage,
            )),
            ojak::kv::MemoryKvStore::with_capacity(1),
            std::time::Duration::from_secs(60 * 60),
            // Signed as the instance actor, for peers in authorized-fetch mode.
            |ctx: Ctx| async move {
                crate::federation::fetch::instance_key(ctx.data())
                    .await
                    .map(Some)
            },
        )
        .fetcher_for(|state: &AppState| state.fetcher.clone())
        .kv_for(|state: &AppState| state.federation_kv.clone())
        .known_key(|ctx: Ctx, key_id: String| async move { known_key(&ctx, &key_id).await })
        // An actor seen for the first time is created from the document
        // fetched for its key, as Mastodon does, rather than fetched again
        // by the worker for the activity it sent; a known actor whose key
        // no longer verified has its keys refreshed from it, or all of it
        // when it was last refreshed a day ago. Two first activities race
        // to create it; `ProcessAccountService`'s lock holds the second
        // until the first has stored it.
        .key_fetched(|ctx: Ctx, actor: Value| async move {
            let id = actor.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
            if let Err(error) = super::inbox::store_key_fetched_actor(ctx.data(), actor).await {
                tracing::debug!(actor = %id, %error, "account not stored from its key fetch");
            }
        })
        // A relayed activity's Linked Data Signature is checked over the
        // contexts its signer named, those ojak does not ship fetched and
        // cached as Mastodon's document loader fetches and caches them.
        .remote_contexts(ojak::contexts::Limits::default(), |ctx: Ctx, iri: String| async move {
            crate::federation::json_ld_contexts::load(ctx.data(), &iri).await
        })
        // A domain this instance does not federate with (`domain_not_allowed?`:
        // suspended, or off the allow list in limited federation mode): a
        // request signed with a key there is refused 403 before any key is
        // fetched, as `SignatureVerification#keypair_from_key_id` refuses it,
        // and an activity whose actor is there is dropped.
        .blocked(|ctx: Ctx, host: String| async move {
            Ok::<_, AppError>(
                crate::federation::moderation::domain_not_allowed(ctx.data(), &host).await,
            )
        })
        // Activities are read as JSON, as Mastodon reads them, never
        // expanded and compacted (docs/design/protocol.md, "JSON-LD in shape,
        // never in processing"). The one listener reads the activity as its
        // sender wrote it, so normalising it was work nothing used: under a
        // viral post, more than half the CPU.
        .read_inbox_as_written()
        .on_any(|ctx: Ctx, received: ojak::federation::Received<ojak_vocab::generated::AnyObject>| async move {
            // Which inbox a peer chose is otherwise invisible: Mastodon picks
            // the shared one only when two accounts here follow the same actor
            // there, and the federation harness checks that path is exercised.
            tracing::debug!(
                inbox = %if received.recipient.is_some() { "personal" } else { "shared" },
                sender = %received.sender,
                activity_type = received.vouched.get("type").and_then(serde_json::Value::as_str).unwrap_or(""),
                "received ActivityPub activity"
            );
            // A server that delivers to us is up: Mastodon clears its failures
            // (`DeliveryFailureTracker.reset!` in `InboxesController`), and
            // its mark if it had one, by the host of the inbox of the actor
            // that signed the request (`signed_request_actor.inbox_url`): the
            // forwarder when another server passed the activity on.
            let signer = received.forwarder.as_ref().unwrap_or(&received.sender);
            let inbox: Option<String> = sqlx::query_scalar(
                "SELECT inbox_url FROM accounts WHERE uri = $1 AND inbox_url <> '' LIMIT 1",
            )
            .bind(signer.as_str())
            .fetch_optional(&ctx.data().db)
            .await
            .ok()
            .flatten();
            if let Some(host) =
                ojak::origin::host_of(inbox.as_deref().unwrap_or(signer.as_str()))
            {
                if let Err(error) = ctx.data().delivery_failures.track_success(&host).await {
                    tracing::warn!(host, %error, "could not clear a server's delivery failures");
                }
            }
            super::inbox::received_from(
                ctx.data(),
                received.vouched,
                received.forwarder.as_ref().map(url::Url::as_str),
            )
            .await
        })
        // A reply to a local post, addressed to its author's followers, is
        // passed on to them (ActivityPub §7.1.2), signed by the author.
        .forward(|ctx: Ctx, forward: ojak::federation::Forward| async move {
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
                        // `permanently_unavailable?`: unavailable with nothing
                        // left to undo is gone.
                        if account.is_unavailable() {
                            let reversible = sqlx::query_scalar!(
                                r#"SELECT EXISTS (SELECT 1 FROM account_deletion_requests WHERE account_id = $1) AS "e!""#,
                                account.id,
                            )
                            .fetch_one(&ctx.data().db)
                            .await?;
                            if !reversible {
                                return Ok(Found::Gone(None));
                            }
                        }
                        let actor = super::objects::actor_json(ctx.data(), domain(&ctx), &account).await?;
                        // `expires_in 3.minutes, public: !(authorized_fetch_mode? &&
                        // signed_request_account.present?)`.
                        let signed = ctx.signer().await.is_some();
                        let public = !(!public_fetch_mode(&ctx).await && signed);
                        expires_in(&ctx, Vary::Page, 180, public).await;
                        Ok(Found::Found(actor))
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
                    &scheme.kind("quote_authorization"),
                    &scheme.template("/quote_authorizations/{quote_id}"),
                    move |ctx: Ctx, values: Values| async move {
                        let identifier = match scheme {
                            Scheme::Username => &values["username"],
                            Scheme::Id => &values["id"],
                        };
                        let (Some(who), Some(id)) =
                            (scheme.who(identifier), number(&values["quote_id"]))
                        else {
                            return Ok(Found::NotFound);
                        };
                        let reader = reader(&ctx).await?;
                        match super::collections::quote_authorization_document(
                            ctx.data(),
                            who,
                            id,
                            reader.as_ref(),
                        )
                        .await
                        {
                            Ok((document, distributable)) => {
                                // `expires_in 30.seconds, public: true if
                                // @quote.quoted_status.distributable? && public_fetch_mode?`.
                                if distributable && public_fetch_mode(&ctx).await {
                                    expires_in(&ctx, Vary::Signature, 30, true).await;
                                }
                                Ok(Found::Found(document))
                            }
                            Err(error) => found(Err(error)),
                        }
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
                        let document =
                            super::collections::account_collections(ctx.data(), domain(&ctx), who)
                                .await;
                        cached_for_public(&ctx, Vary::Page, 180, found(document)).await
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
                )
                .collection(
                    &scheme.kind("featured_tags"),
                    &scheme.template("/collections/tags"),
                    featured_tags(scheme),
                )
                .object(
                    &scheme.kind("replies"),
                    &scheme.template("/statuses/{status_id}/replies"),
                    move |ctx: Ctx, values: Values| async move {
                        let Some((who, status_id)) = scheme.status(&values) else {
                            return Ok(Found::NotFound);
                        };
                        let query = super::status_collections::Query::parse(ctx.query());
                        let reader = reader(&ctx).await?;
                        let document = super::status_collections::replies(
                            ctx.data(),
                            who,
                            status_id,
                            &query,
                            reader.as_ref(),
                        )
                        .await;
                        of_status(&ctx, document).await
                    },
                );
        for (kind, which) in [
            ("likes", super::status_collections::Interactions::Likes),
            ("shares", super::status_collections::Interactions::Shares),
        ] {
            builder = builder.object(
                &scheme.kind(kind),
                &scheme.template(&format!("/statuses/{{status_id}}/{kind}")),
                move |ctx: Ctx, values: Values| async move {
                    let Some((who, status_id)) = scheme.status(&values) else {
                        return Ok(Found::NotFound);
                    };
                    let reader = reader(&ctx).await?;
                    let document = super::status_collections::interactions(
                        ctx.data(),
                        who,
                        status_id,
                        which,
                        reader.as_ref(),
                    )
                    .await;
                    of_status(&ctx, document).await
                },
            );
        }
        // Every ActivityPub controller of Mastodon's runs
        // `require_account_signature!` in authorized fetch mode, and says
        // what its response varies by (`vary_by`) before anything else.
        for (kind, vary) in [
            ("actor", Vary::Page),
            ("account_collections", Vary::Page),
            ("outbox", Vary::Outbox),
            ("followers", Vary::Page),
            ("following", Vary::Page),
            ("featured", Vary::Signature),
            ("featured_tags", Vary::Signature),
            ("status", Vary::Page),
            ("status_activity", Vary::Page),
            ("quote_authorization", Vary::Signature),
            ("replies", Vary::Signature),
            ("likes", Vary::Signature),
            ("shares", Vary::Signature),
        ] {
            builder = builder.guard(&scheme.kind(kind), move |ctx: Ctx, _| async move {
                vary_by(&ctx, vary).await;
                require_signature(&ctx).await
            });
        }
    }
    // The instance actor is exempt, as `InstanceActorsController` is: a peer
    // in authorized fetch mode has to fetch its key before it can sign.
    builder = builder.guard("instance", |ctx: Ctx, _| async move {
        vary_by(&ctx, Vary::Nothing).await;
        Ok::<_, AppError>(Access::Allow)
    });
    for (kind, vary) in [
        ("collection", Vary::Page),
        ("feature_authorization", Vary::Signature),
        ("featured_collections", Vary::Page),
        ("account_collection", Vary::Page),
        ("collection_item", Vary::Page),
        ("feature_authorization_by_id", Vary::Signature),
        ("context", Vary::Signature),
        ("context_items", Vary::Signature),
        ("instance_outbox", Vary::Outbox),
    ] {
        builder = builder.guard(kind, move |ctx: Ctx, _| async move {
            vary_by(&ctx, vary).await;
            require_signature(&ctx).await
        });
    }
    // A status's page, /@{username}/{id}, is where Mastodon also serves its
    // Note to whoever asks for ActivityPub (`statuses#show`). Anything else
    // there, and any other request, goes on to eunha's pages.
    builder = builder.object_alias(&Scheme::Username.kind("status"), "/@{username}/{status_id}");
    // So are an account's followers and following, at the pages Mastodon
    // serves them at too (`follower_accounts#index`, `following_accounts#index`),
    // still named by their own URIs.
    for collection in ["followers", "following"] {
        builder = builder.collection_alias(
            &Scheme::Username.kind(collection),
            &format!("/@{{username}}/{collection}"),
        );
    }
    builder.build().expect("eunha's federation is well formed")
}

/// Forward `forward`'s activity to the followers of the local accounts whose
/// followers collections it names. Eunha is no portable actor's gateway, so
/// there is nothing to forward to gateways.
async fn forward_to_collections(ctx: &Ctx, forward: ojak::federation::Forward) -> AppResult<()> {
    let ojak::federation::ForwardTo::Collections(collections) = forward.to else {
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
        let key_id = AccountUris::of(&ctx.uris(), &account).key_id()?;
        crate::federation::delivery::forward_to_followers(
            ctx.data(),
            forward.activity.clone(),
            account.id,
            key_id.into(),
        )
        .await
        .map_err(AppError::Internal)?;
    }
    Ok(())
}

/// The key eunha holds for `key_id`: the public key of the remote account
/// whose actor the key ID names.
async fn known_key(ctx: &Ctx, key_id: &str) -> AppResult<Option<ojak::federation::KnownKey>> {
    // `Keypair.from_keyid`: a usable RSA key stored under this id…
    let stored = sqlx::query!(
        r#"SELECT k.public_key, a.uri AS "actor!"
           FROM keypairs k JOIN accounts a ON a.id = k.account_id
           WHERE k.uri = $1 AND k.type = 0 AND NOT k.revoked
             AND (k.expires_at IS NULL OR k.expires_at > now())
             AND a.domain IS NOT NULL"#,
        key_id,
    )
    .fetch_optional(&ctx.data().db)
    .await?;
    if let Some(stored) = stored {
        return Ok(Url::parse(&stored.actor)
            .ok()
            .map(|actor| ojak::federation::KnownKey {
                pem: stored.public_key,
                actor,
            }));
    }
    // …or, the way RSA keys used to be stored, its owner's `public_key`.
    let owner = ojak::sig::verification::key_owner(key_id);
    let pem = sqlx::query_scalar!(
        "SELECT public_key FROM accounts WHERE uri = $1 AND domain IS NOT NULL AND public_key != ''",
        owner,
    )
    .fetch_optional(&ctx.data().db)
    .await?;
    Ok(pem.and_then(|pem| {
        Some(ojak::federation::KnownKey {
            pem,
            actor: Url::parse(owner).ok()?,
        })
    }))
}

/// The verified signer of the request, if it was signed.
async fn signer(ctx: &Ctx) -> Option<String> {
    ctx.signer().await.map(String::from)
}

fn domain(ctx: &Ctx) -> &str {
    &ctx.data().instance.domain
}

fn number(value: &str) -> Option<i64> {
    value.parse().ok()
}

/// A document, or `NotFound` and `Gone` as ojak says them.
fn found(result: AppResult<Value>) -> AppResult<Found<Value>> {
    match result {
        Ok(document) => Ok(Found::Found(document)),
        Err(AppError::NotFound) => Ok(Found::NotFound),
        Err(AppError::Gone(_)) => Ok(Found::Gone(None)),
        Err(error) => Err(error),
    }
}

/// `SignatureVerification#require_account_signature!`, run in authorized
/// fetch mode: an unsigned fetch, or one whose signature does not hold, is
/// 401; one signed with a key on a domain this instance does not federate
/// with is 403, its key never fetched. Outside authorized fetch mode nothing
/// is verified here.
async fn require_signature(ctx: &Ctx) -> AppResult<Access> {
    if !crate::settings::authorized_fetch_mode(ctx.data()).await {
        return Ok(Access::Allow);
    }
    Ok(match ctx.signing().await {
        Signing::Verified(_) => Access::Allow,
        Signing::Blocked(_) => Access::Forbidden,
        Signing::Unsigned | Signing::Invalid(_) => Access::Unauthorized,
    })
}

/// What a response varies by (`vary_by`), as each of Mastodon's
/// ActivityPub controllers says.
#[derive(Clone, Copy)]
enum Vary {
    /// Nothing (`InstanceActorsController`).
    Nothing,
    /// `'Signature' if authorized_fetch_mode?`.
    Signature,
    /// `'Accept, Accept-Language, Cookie'`, and `Signature` outside public
    /// fetch mode: the controllers that also serve a page.
    Page,
    /// `'Signature' if authorized_fetch_mode? || page_requested?`
    /// (`OutboxesController`).
    Outbox,
}

impl Vary {
    async fn value(self, ctx: &Ctx) -> Option<&'static str> {
        let authorized = !public_fetch_mode(ctx).await;
        match self {
            Self::Nothing => None,
            Self::Signature => authorized.then_some("Signature"),
            Self::Page => Some(if authorized {
                "Accept, Accept-Language, Cookie, Signature"
            } else {
                "Accept, Accept-Language, Cookie"
            }),
            Self::Outbox => (authorized || outbox_page_requested(ctx)).then_some("Signature"),
        }
    }
}

/// `public_fetch_mode?`: not authorized fetch mode.
async fn public_fetch_mode(ctx: &Ctx) -> bool {
    !crate::settings::authorized_fetch_mode(ctx.data()).await
}

/// `vary_by`, and `ApplicationController#set_cache_control_defaults`:
/// nothing a cache may keep unless the controller says otherwise.
async fn vary_by(ctx: &Ctx, vary: Vary) {
    ctx.set_response_header("vary", vary.value(ctx).await);
    ctx.set_response_header("cache-control", Some("private, no-store"));
}

/// `expires_in seconds, public:`, then `CacheConcern#enforce_cache_control!`:
/// a response that varies by a signature the request carried is kept by no
/// cache.
async fn expires_in(ctx: &Ctx, vary: Vary, seconds: u32, public: bool) {
    let signed = vary
        .value(ctx)
        .await
        .is_some_and(|vary| vary.contains("Signature"))
        && ctx.request_header("signature").is_some();
    if signed {
        ctx.set_response_header("cache-control", Some("private, no-store"));
        return;
    }
    let scope = if public { "public" } else { "private" };
    ctx.set_response_header(
        "cache-control",
        Some(&format!("max-age={seconds}, {scope}")),
    );
}

/// A document found, cached for `seconds`, publicly in public fetch mode.
async fn cached_for_public(
    ctx: &Ctx,
    vary: Vary,
    seconds: u32,
    document: AppResult<Found<Value>>,
) -> AppResult<Found<Value>> {
    let document = document?;
    if matches!(document, Found::Found(_)) {
        let public = public_fetch_mode(ctx).await;
        expires_in(ctx, vary, seconds, public).await;
    }
    Ok(document)
}

/// A post's replies, likes or shares: `expires_in 0, public:
/// @status.distributable? && public_fetch_mode?`.
async fn of_status(ctx: &Ctx, document: AppResult<(Value, bool)>) -> AppResult<Found<Value>> {
    match document {
        Ok((document, distributable)) => {
            let public = distributable && public_fetch_mode(ctx).await;
            expires_in(ctx, Vary::Signature, 0, public).await;
            Ok(Found::Found(document))
        }
        Err(error) => found(Err(error)),
    }
}

/// Who signed the request, as an account eunha knows
/// (`signed_request_account`).
async fn reader(ctx: &Ctx) -> AppResult<Option<super::objects::Reader>> {
    let signer = signer(ctx).await;
    super::objects::reader(ctx.data(), signer.as_deref()).await
}

/// `CollectionsController#show` on a collection: cached for thirty
/// seconds when it was updated in the last quarter hour and five minutes
/// otherwise, publicly, in public fetch mode alone.
async fn collection(ctx: &Ctx, owner: Option<i64>, id: i64) -> AppResult<Found<Value>> {
    let signer = signer(ctx).await;
    match super::collections::collection_document(
        ctx.data(),
        domain(ctx),
        owner,
        id,
        signer.as_deref(),
    )
    .await
    {
        Ok((document, updated_at)) => {
            if public_fetch_mode(ctx).await {
                let recent =
                    updated_at > chrono::Utc::now().naive_utc() - chrono::Duration::minutes(15);
                expires_in(ctx, Vary::Page, if recent { 30 } else { 300 }, true).await;
            }
            Ok(Found::Found(document))
        }
        Err(error) => found(Err(error)),
    }
}

/// `ActivityPub::FeatureAuthorizationsController#show`: `expires_in
/// 30.seconds, public: true if public_fetch_mode?`.
async fn feature_authorization(ctx: &Ctx, who: AccountRef<'_>, id: i64) -> AppResult<Found<Value>> {
    let signer = signer(ctx).await;
    let document = super::collections::feature_authorization_document(
        ctx.data(),
        domain(ctx),
        who,
        id,
        signer.as_deref(),
    )
    .await;
    let document = found(document)?;
    if matches!(document, Found::Found(_)) && public_fetch_mode(ctx).await {
        expires_in(ctx, Vary::Signature, 30, true).await;
    }
    Ok(document)
}

/// Whether the verified signer of the request is an account `owner_id`
/// blocks, or is on a domain it blocks: `AccountStatusesFilter#blocked?`, and
/// the author half of `StatusPolicy#show?`. An unsigned request, and a signer
/// with no account here, is blocked by nobody.
async fn signer_blocked(ctx: &Ctx, owner_id: i64) -> AppResult<bool> {
    let Some(signer) = ctx.signer().await else {
        return Ok(false);
    };
    let blocked = sqlx::query_scalar!(
        r#"SELECT (EXISTS (SELECT 1 FROM blocks WHERE account_id = $1 AND target_account_id = a.id)
                   OR EXISTS (SELECT 1 FROM account_domain_blocks
                              WHERE account_id = $1 AND domain = a.domain)) AS "blocked!"
           FROM accounts a WHERE a.uri = $2 AND a.domain IS NOT NULL"#,
        owner_id,
        signer.as_str(),
    )
    .fetch_optional(&ctx.data().db)
    .await?;
    Ok(blocked.unwrap_or(false))
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

    /// The account and the post a status route's values name.
    fn status(self, values: &Values) -> Option<(AccountRef<'_>, i64)> {
        let identifier = match self {
            Self::Username => &values["username"],
            Self::Id => &values["id"],
        };
        Some((self.who(identifier)?, number(&values["status_id"])?))
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

    /// The scheme a local account uses, as Mastodon's `id_scheme` says, and
    /// what it is identified by in it.
    fn of(id: i64, id_scheme: Option<i32>, username: &str) -> (Self, String) {
        if id_scheme == Some(crate::federation::tag::NUMERIC_AP_ID) {
            (Self::Id, id.to_string())
        } else {
            (Self::Username, username.to_owned())
        }
    }

    /// The name of the expression that identifies the account in this
    /// scheme's templates.
    fn expression(self) -> &'static str {
        match self {
            Self::Username => "username",
            Self::Id => "id",
        }
    }

    /// The URI of `what` of the account `identifier` names under this scheme,
    /// named by the scheme the account uses, whichever it was asked under.
    async fn own_uri(self, ctx: &Ctx, identifier: &str, what: Own) -> AppResult<Option<Url>> {
        match self.account(ctx, identifier).await? {
            Some(account) => Ok(Some(AccountUris::of(&ctx.uris(), &account).uri(what)?)),
            None => Ok(None),
        }
    }
}

/// What a local actor has beneath its own URI.
#[derive(Clone, Copy)]
pub enum Own {
    Inbox,
    Outbox,
    Followers,
    Following,
    Featured,
    /// Its featured hashtags, `/collections/tags`.
    Tags,
    /// Its featured collections, `/ap/users/{id}/featured_collections`
    /// whichever scheme it uses, as `ap_account_featured_collections_url`
    /// names them.
    Collections,
}

impl Own {
    /// The kind it is registered as, in the username scheme.
    fn kind(self) -> &'static str {
        match self {
            Self::Inbox => "actor",
            Self::Outbox => "outbox",
            Self::Followers => "followers",
            Self::Following => "following",
            Self::Featured => "featured",
            Self::Tags => "featured_tags",
            Self::Collections => "featured_collections",
        }
    }

    /// Its path beneath the actor's.
    fn suffix(self) -> &'static str {
        match self {
            Self::Inbox => "/inbox",
            Self::Outbox => "/outbox",
            Self::Followers => "/followers",
            Self::Following => "/following",
            Self::Featured => "/collections/featured",
            Self::Tags => "/collections/tags",
            Self::Collections => "/featured_collections",
        }
    }
}

/// A local account's URIs, in the scheme it uses, built from the templates
/// it is served at.
pub struct AccountUris<'a> {
    uris: &'a Uris,
    served: Served,
}

enum Served {
    /// An account, under the scheme it uses, by what it is identified by
    /// there, and its id.
    Account(Scheme, String, i64),
    /// The instance actor, which is served at `/actor` rather than under
    /// either scheme. Asked for under one, it names its inbox and
    /// collections beneath `/actor`, as Mastodon's serializer does, where no
    /// template of eunha's serves them.
    Instance,
}

impl<'a> AccountUris<'a> {
    /// The URIs of the local account `id`, whose `id_scheme` and `username`
    /// these are, in `uris`'s origin.
    #[must_use]
    pub fn new(uris: &'a Uris, id: i64, id_scheme: Option<i32>, username: &str) -> Self {
        let served = if id == crate::federation::instance_actor::INSTANCE_ACTOR_ID {
            Served::Instance
        } else {
            let (scheme, identifier) = Scheme::of(id, id_scheme, username);
            Served::Account(scheme, identifier, id)
        };
        Self { uris, served }
    }

    /// The URIs of a loaded local account.
    #[must_use]
    pub fn of(uris: &'a Uris, account: &Account) -> Self {
        Self::new(uris, account.id, account.id_scheme, &account.username)
    }

    /// Its actor's URI.
    ///
    /// # Errors
    ///
    /// When the account's identifier does not fill its template.
    pub fn actor(&self) -> anyhow::Result<Url> {
        Ok(match &self.served {
            Served::Account(scheme, identifier, _) => {
                self.uris.actor_uri(&scheme.kind("actor"), identifier)?
            }
            Served::Instance => self.uris.actor_uri("instance", "")?,
        })
    }

    /// The ID of the key it signs with.
    ///
    /// # Errors
    ///
    /// As [`AccountUris::actor`].
    pub fn key_id(&self) -> anyhow::Result<Url> {
        Ok(match &self.served {
            Served::Account(scheme, identifier, _) => {
                self.uris.key_id(&scheme.kind("actor"), identifier)?
            }
            Served::Instance => self.uris.key_id("instance", "")?,
        })
    }

    /// The URI of `what` beneath its actor.
    ///
    /// # Errors
    ///
    /// As [`AccountUris::actor`].
    pub fn uri(&self, what: Own) -> anyhow::Result<Url> {
        let Served::Account(scheme, identifier, id) = &self.served else {
            return self.beneath_actor(what.suffix());
        };
        let kind = scheme.kind(what.kind());
        Ok(match what {
            Own::Inbox => self.uris.inbox_uri(&kind, identifier)?,
            Own::Collections => self
                .uris
                .object_uri(what.kind(), &[("id", &id.to_string())])?,
            _ => self.uris.collection_uri(&kind, identifier)?,
        })
    }

    /// The URI of its status `id`.
    ///
    /// # Errors
    ///
    /// As [`AccountUris::actor`].
    pub fn status(&self, id: i64) -> anyhow::Result<Url> {
        let Served::Account(scheme, identifier, _) = &self.served else {
            return self.beneath_actor(&format!("/statuses/{id}"));
        };
        Ok(self.uris.object_uri(
            &scheme.kind("status"),
            &[
                (scheme.expression(), identifier),
                ("status_id", &id.to_string()),
            ],
        )?)
    }

    fn beneath_actor(&self, suffix: &str) -> anyhow::Result<Url> {
        Ok(Url::parse(&format!("{}{suffix}", self.actor()?))?)
    }
}

/// `StatusesController#show` and `#activity`: a post that the signer, if
/// any, may be shown (`StatusPolicy#show?`), as its Note, or as the
/// activity that posted it: its `Create`, or a boost's `Announce`.
async fn status(
    ctx: &Ctx,
    scheme: Scheme,
    values: &Values,
    activity: bool,
) -> AppResult<Found<Value>> {
    let Some((who, status_id)) = scheme.status(values) else {
        return Ok(Found::NotFound);
    };
    let reader = reader(ctx).await?;
    let servable =
        match super::objects::servable_status(ctx.data(), who, status_id, reader.as_ref()).await {
            Ok(servable) => servable,
            Err(error) => return found(Err(error)),
        };
    let public = servable.distributable() && public_fetch_mode(ctx).await;
    let document = match servable.reblog_of_id {
        // A boost's page is the boosted post's (`redirect_to_original`),
        // which is no ActivityPub document.
        Some(_) if !activity => return Ok(Found::NotFound),
        Some(reblog_of_id) => {
            let row = sqlx::query!(
                "SELECT visibility, created_at FROM statuses WHERE id = $1",
                status_id,
            )
            .fetch_one(&ctx.data().db)
            .await?;
            let Some(mut announce) = crate::portability::backup::announce_item(
                ctx.data(),
                &servable.account,
                status_id,
                reblog_of_id,
                row.visibility,
                row.created_at,
            )
            .await?
            else {
                return Ok(Found::NotFound);
            };
            let context = if announce["object"].is_object() {
                super::note::note_context()
            } else {
                json!("https://www.w3.org/ns/activitystreams")
            };
            if let Some(members) = announce.as_object_mut() {
                let mut with_context = serde_json::Map::new();
                with_context.insert("@context".into(), context);
                with_context.append(members);
                *members = with_context;
            }
            announce
        }
        None => {
            let Some(bundle) = super::note::build_note(ctx.data(), domain(ctx), status_id).await?
            else {
                return Ok(Found::NotFound);
            };
            if activity {
                bundle.into_create()
            } else {
                bundle.into_note()
            }
        }
    };
    if activity {
        // `expires_in 3.minutes, public: @status.distributable? &&
        // public_fetch_mode?`.
        expires_in(ctx, Vary::Page, 180, public).await;
    } else if public {
        // `expires_in @status.quote&.pending? ? 5.seconds : 3.minutes,
        // public: true if @status.distributable? && public_fetch_mode?`.
        let seconds = if servable.quote_pending { 5 } else { 180 };
        expires_in(ctx, Vary::Page, seconds, true).await;
    }
    Ok(Found::Found(document))
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

/// `OutboxesController::LIMIT`.
const OUTBOX_PAGE: i64 = 20;

/// Whether an outbox request asks for a page (`truthy_param?(:page)`).
fn outbox_page_requested(ctx: &Ctx) -> bool {
    super::status_collections::Query::parse(ctx.query()).truthy("page")
}

/// The query an outbox cursor is written as, as `outbox_url(page: true,
/// …)` writes it: `page=true`, with the `max_id` or `min_id` it pages from.
fn outbox_query(cursor: &str) -> String {
    match cursor.split_once(':') {
        Some(("max", id)) => format!("max_id={id}&page=true"),
        Some(("min", id)) => format!("min_id={id}&page=true"),
        _ => "page=true".to_owned(),
    }
}

/// The cursor an outbox request asks for: none without `page`, and
/// otherwise the `max_id` or `min_id` it gives, or the newest page.
fn outbox_cursor(query: &str) -> Option<String> {
    let query = super::status_collections::Query::parse(Some(query));
    if !query.truthy("page") {
        return None;
    }
    if let Some(id) = query.number("max_id") {
        return Some(format!("max:{id}"));
    }
    if let Some(id) = query.number("min_id") {
        return Some(format!("min:{id}"));
    }
    Some(String::new())
}

/// A page of the outbox of the local account `account_id`, as
/// `OutboxesController` pages `AccountStatusesFilter`'s results for the
/// signer: its public and unlisted posts and boosts, its followers-only
/// ones too to a follower, and any that mention the signer, twenty to a
/// page, newest first, each as the `Create` or `Announce` that posted it.
async fn outbox_page(ctx: &Ctx, account_id: i64, cursor: Option<&str>) -> AppResult<Option<Page>> {
    let Some(cursor) = OutboxCursor::parse(cursor.unwrap_or_default()) else {
        return Ok(None);
    };
    let (max_id, min_id) = match cursor {
        OutboxCursor::Newest => (None, None),
        OutboxCursor::Below(id) => (Some(id), None),
        OutboxCursor::Above(id) => (None, Some(id)),
    };
    // `expires_in(1.minute, public: public_fetch_mode? &&
    // signed_request_account.nil?)`.
    let reader = reader(ctx).await?;
    let public = public_fetch_mode(ctx).await && ctx.signer().await.is_none();
    expires_in(ctx, Vary::Outbox, 60, public).await;
    let state = ctx.data();
    let owner = sqlx::query!(
        r#"SELECT (suspended_at IS NOT NULL OR requested_deletion_at IS NOT NULL) AS "unavailable!"
           FROM accounts WHERE id = $1"#,
        account_id,
    )
    .fetch_optional(&state.db)
    .await?;
    // `AccountStatusesFilter`: nothing of an unavailable account's, nor for
    // a signer it blocks.
    if owner.is_none_or(|owner| owner.unavailable) || signer_blocked(ctx, account_id).await? {
        return Ok(Some(Page::default()));
    }
    let reader_id = reader.as_ref().map(|reader| reader.id);
    let mut rows = sqlx::query!(
        r#"SELECT s.id, s.reblog_of_id, s.visibility, s.created_at
           FROM statuses s
           WHERE s.account_id = $1 AND s.deleted_at IS NULL
             AND (s.visibility IN (0, 1) /* distributable_visibility */
                  OR ($2::bigint IS NOT NULL
                      AND ((s.visibility = 2 AND EXISTS (SELECT 1 FROM follows
                                WHERE account_id = $2 AND target_account_id = $1))
                           OR EXISTS (SELECT 1 FROM mentions m
                                      WHERE m.status_id = s.id AND m.account_id = $2))))
             -- `filtered_reblogs_scope`: no boost of whom the signer
             -- blocks, is blocked by or mutes, nor of a domain it blocks.
             AND (s.reblog_of_id IS NULL OR $2::bigint IS NULL OR NOT EXISTS (
                   SELECT 1 FROM statuses r JOIN accounts ra ON ra.id = r.account_id
                   WHERE r.id = s.reblog_of_id
                     AND (EXISTS (SELECT 1 FROM blocks b WHERE (b.account_id = $2 AND b.target_account_id = ra.id)
                                                            OR (b.account_id = ra.id AND b.target_account_id = $2))
                          OR EXISTS (SELECT 1 FROM mutes mu WHERE mu.account_id = $2 AND mu.target_account_id = ra.id)
                          OR (ra.domain IS NOT NULL AND EXISTS (SELECT 1 FROM account_domain_blocks d
                                                                WHERE d.account_id = $2 AND d.domain = ra.domain)))))
             AND ($3::bigint IS NULL OR s.id < $3)
             AND ($4::bigint IS NULL OR s.id > $4)
           -- `paginate_by_min_id` reads up from `min_id`, the others down.
           ORDER BY CASE WHEN $4::bigint IS NULL THEN -s.id ELSE s.id END
           LIMIT $5"#,
        account_id,
        reader_id,
        max_id,
        min_id,
        OUTBOX_PAGE,
    )
    .fetch_all(&state.db)
    .await?;
    if min_id.is_some() {
        rows.reverse();
    }
    let account = load_local_account(state, AccountRef::Id(account_id)).await?;
    let mut items = Vec::with_capacity(rows.len());
    for row in &rows {
        let item = match row.reblog_of_id {
            Some(reblog_of_id) => {
                crate::portability::backup::announce_item(
                    state,
                    &account,
                    row.id,
                    reblog_of_id,
                    row.visibility,
                    row.created_at,
                )
                .await?
            }
            None => super::note::build_note(state, domain(ctx), row.id)
                .await?
                .map(super::note::NoteBundle::into_create),
        };
        if let Some(mut item) = item {
            // An item embedded in the collection, without its own context.
            if let Some(members) = item.as_object_mut() {
                members.remove("@context");
            }
            items.push(item);
        }
    }
    Ok(Some(Page {
        items,
        next: (rows.len() as i64 == OUTBOX_PAGE)
            .then(|| rows.last().map(|row| format!("max:{}", row.id)))
            .flatten(),
        prev: rows.first().map(|row| format!("min:{}", row.id)),
    }))
}

/// How many statuses the local account `account_id` has.
async fn statuses_count(ctx: &Ctx, account_id: i64) -> AppResult<u64> {
    let count = sqlx::query_scalar!(
        "SELECT COALESCE(statuses_count, 0) FROM account_stats WHERE account_id = $1",
        account_id,
    )
    .fetch_optional(&ctx.data().db)
    .await?
    .flatten()
    .unwrap_or(0);
    Ok(u64::try_from(count).unwrap_or(0))
}

/// An outbox's documents as `ActivityPub::OutboxSerializer` writes them:
/// paged as Mastodon pages them, its last page the one up from nought, and
/// its context the embedded posts'.
fn outbox_documents(collection: Collection<AppState>) -> Collection<AppState> {
    collection
        .page_query(outbox_query, outbox_cursor)
        .last_cursor(|_, _| async { Ok::<_, AppError>(Some("min:0".to_owned())) })
        .context(|items| {
            if items.is_empty() {
                json!("https://www.w3.org/ns/activitystreams")
            } else {
                super::note::note_context()
            }
        })
}

/// The collection itself: `expires_in 3.minutes, public: public_fetch_mode?`.
async fn outbox_first(ctx: &Ctx) -> First {
    let public = public_fetch_mode(ctx).await;
    expires_in(ctx, Vary::Outbox, 180, public).await;
    First::At(String::new())
}

/// An account's outbox ([`outbox_page`]).
fn outbox(scheme: Scheme) -> Collection<AppState> {
    outbox_documents(
        Collection::new(
            move |ctx: Ctx, identifier: String, cursor: Option<String>| async move {
                let Some(account) = scheme.account(&ctx, &identifier).await? else {
                    return Ok::<_, AppError>(None);
                };
                outbox_page(&ctx, account.id, cursor.as_deref()).await
            },
        )
        .count(move |ctx: Ctx, identifier: String| async move {
            let Some(account) = scheme.account(&ctx, &identifier).await? else {
                return Ok::<_, AppError>(None);
            };
            Ok(Some(statuses_count(&ctx, account.id).await?))
        })
        .first_cursor(move |ctx: Ctx, identifier: String| async move {
            match scheme.account(&ctx, &identifier).await? {
                Some(_) => Ok::<_, AppError>(Some(outbox_first(&ctx).await)),
                None => Ok(None),
            }
        })
        .uri(move |ctx: Ctx, identifier: String| async move {
            scheme.own_uri(&ctx, &identifier, Own::Outbox).await
        }),
    )
}

/// The instance actor's outbox, `/actor/outbox`: `OutboxesController` on
/// `Account.representative`, which posts nothing.
fn instance_outbox() -> Collection<AppState> {
    use crate::federation::instance_actor::INSTANCE_ACTOR_ID;
    outbox_documents(
        Collection::new(|ctx: Ctx, _: String, cursor: Option<String>| async move {
            outbox_page(&ctx, INSTANCE_ACTOR_ID, cursor.as_deref()).await
        })
        .count(|ctx: Ctx, _: String| async move {
            Ok::<_, AppError>(Some(statuses_count(&ctx, INSTANCE_ACTOR_ID).await?))
        })
        .first_cursor(|ctx: Ctx, _| async move {
            Ok::<_, AppError>(Some(outbox_first(&ctx).await))
        }),
    )
}

#[derive(Clone, Copy)]
enum Relation {
    Followers,
    Following,
}

impl Relation {
    fn own(self) -> Own {
        match self {
            Self::Followers => Own::Followers,
            Self::Following => Own::Following,
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
            // `expires_in(page_requested? ? 0 : 3.minutes, public:
            // public_fetch_mode?)`.
            let public = public_fetch_mode(&ctx).await;
            expires_in(&ctx, Vary::Page, 0, public).await;
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
        let public = public_fetch_mode(&ctx).await;
        expires_in(&ctx, Vary::Page, 180, public).await;
        Ok::<_, AppError>(scheme.account(&ctx, &identifier).await?.map(|account| {
            if account.hide_collections.unwrap_or(false) {
                First::Hidden
            } else {
                First::At(String::new())
            }
        }))
    })
    .uri(move |ctx: Ctx, identifier: String| async move {
        scheme.own_uri(&ctx, &identifier, relation.own()).await
    })
}

/// An account's pinned statuses, newest pin first, in one document
/// (`ActivityPub::CollectionsController` on `featured`): a public or
/// unlisted one embedded as its Note, any other named by its URI.
fn featured(scheme: Scheme) -> Collection<AppState> {
    Collection::new(
        move |ctx: Ctx, identifier: String, _: Option<String>| async move {
            let Some(account) = scheme.account(&ctx, &identifier).await? else {
                return Ok::<_, AppError>(None);
            };
            // `expires_in 3.minutes, public: public_fetch_mode?`.
            let public = public_fetch_mode(&ctx).await;
            expires_in(&ctx, Vary::Signature, 180, public).await;
            // `ActivityPub::CollectionsController#check_authorization`: in
            // authorized fetch mode, a signer the account blocks is shown it
            // empty.
            if crate::settings::authorized_fetch_mode(ctx.data()).await
                && signer_blocked(&ctx, account.id).await?
            {
                return Ok(Some(Page::default()));
            }
            let rows = sqlx::query!(
                r#"SELECT s.id, s.visibility
                   FROM status_pins p JOIN statuses s ON s.id = p.status_id
                   WHERE p.account_id = $1 AND s.deleted_at IS NULL
                   ORDER BY p.id DESC"#,
                account.id,
            )
            .fetch_all(&ctx.data().db)
            .await?;
            let uris = ctx.uris();
            let own = AccountUris::of(&uris, &account);
            let mut items = Vec::with_capacity(rows.len());
            for row in rows {
                // `distributable?`.
                if matches!(
                    row.visibility,
                    crate::db::models::vis::PUBLIC | crate::db::models::vis::UNLISTED
                ) {
                    if let Some(bundle) =
                        super::note::build_note(ctx.data(), domain(&ctx), row.id).await?
                    {
                        items.push(bundle.note);
                        continue;
                    }
                }
                items.push(Value::String(own.status(row.id)?.into()));
            }
            Ok(Some(Page {
                items,
                ..Page::default()
            }))
        },
    )
    .context(|items| {
        // `ActivityPub::Adapter`: an embedded Note brings its context.
        if items.iter().any(Value::is_object) {
            super::note::note_context()
        } else {
            json!("https://www.w3.org/ns/activitystreams")
        }
    })
    .uri(move |ctx: Ctx, identifier: String| async move {
        scheme.own_uri(&ctx, &identifier, Own::Featured).await
    })
}

/// The hashtags an account features, in one unordered document
/// (`ActivityPub::CollectionsController` on `tags`).
fn featured_tags(scheme: Scheme) -> Collection<AppState> {
    Collection::new(
        move |ctx: Ctx, identifier: String, _: Option<String>| async move {
            let Some(account) = scheme.account(&ctx, &identifier).await? else {
                return Ok::<_, AppError>(None);
            };
            let public = public_fetch_mode(&ctx).await;
            expires_in(&ctx, Vary::Signature, 180, public).await;
            // `check_authorization`, as for the featured posts.
            if crate::settings::authorized_fetch_mode(ctx.data()).await
                && signer_blocked(&ctx, account.id).await?
            {
                return Ok(Some(Page::default()));
            }
            let items = super::featured_tags::hashtags(
                ctx.data(),
                domain(&ctx),
                account.id,
                &account.username,
            )
            .await?;
            Ok(Some(Page {
                items,
                ..Page::default()
            }))
        },
    )
    .unordered()
    .context(|items| super::featured_tags::context(!items.is_empty()))
    .uri(move |ctx: Ctx, identifier: String| async move {
        scheme.own_uri(&ctx, &identifier, Own::Tags).await
    })
}

/// The actor a WebFinger username names: a local account, under the scheme
/// it uses, or the instance actor, whose username is the domain.
async fn by_username(ctx: &Ctx, username: &str) -> AppResult<Option<ActorRef>> {
    if username.eq_ignore_ascii_case(domain(ctx)) {
        return Ok(Some(ActorRef::new("instance", "")));
    }
    let account = sqlx::query!(
        "SELECT id, username, id_scheme FROM accounts WHERE lower(username) = lower($1) AND domain IS NULL",
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
    let (user_count, status_count) = tokio::try_join!(
        sqlx::query_scalar!(
            "SELECT COUNT(*) FROM accounts WHERE domain IS NULL AND suspended_at IS NULL AND requested_deletion_at IS NULL",
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
    // `InstancePresenter#active_user_count(4)` and `(24)`.
    let active_month = Some(crate::activity_tracker::active_user_count(state, 4).await);
    let active_halfyear = Some(crate::activity_tracker::active_user_count(state, 24).await);
    let count = |n: Option<i64>| u64::try_from(n.unwrap_or(0)).ok();
    let instance = &state.instance;
    let mut nodeinfo = NodeInfo::new(Software {
        name: "eunha".to_owned(),
        version: crate::version::EUNHA_FULL.to_owned(),
        repository: None,
        homepage: None,
    });
    let settings = crate::settings::Snapshot::load(state).await;
    nodeinfo.open_registrations = settings.registrations_mode(instance).enabled();
    nodeinfo.usage = Usage {
        users_total: count(user_count),
        users_active_month: count(active_month),
        users_active_halfyear: count(active_halfyear),
        local_posts: count(status_count),
        local_comments: None,
    };
    nodeinfo.metadata = json!({
        "nodeName": settings.site_title(instance),
        "nodeDescription": settings.site_short_description(instance),
    });
    Ok(nodeinfo)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::federation::{instance_actor::INSTANCE_ACTOR_ID, tag};

    /// What the templates build is what the actor URI with a suffix was, in
    /// either scheme and for the instance actor.
    #[test]
    fn account_uris_are_the_actor_uri_and_a_suffix() {
        let uris = uris("seoul.earth").unwrap();
        for (id, id_scheme, username) in [
            (42, Some(tag::NUMERIC_AP_ID), "alice"),
            (42, Some(0), "alice"),
            (42, None, "alice"),
            (INSTANCE_ACTOR_ID, Some(tag::NUMERIC_AP_ID), "seoul.earth"),
        ] {
            let actor = tag::account_uri("seoul.earth", id, id_scheme, username);
            let own = AccountUris::new(&uris, id, id_scheme, username);
            assert_eq!(own.actor().unwrap().as_str(), actor);
            assert_eq!(own.key_id().unwrap().as_str(), format!("{actor}#main-key"));
            for what in [
                Own::Inbox,
                Own::Outbox,
                Own::Followers,
                Own::Following,
                Own::Featured,
                Own::Tags,
            ] {
                assert_eq!(
                    own.uri(what).unwrap().as_str(),
                    format!("{actor}{}", what.suffix())
                );
            }
            if id != INSTANCE_ACTOR_ID {
                assert_eq!(
                    own.uri(Own::Collections).unwrap().as_str(),
                    format!("https://seoul.earth/ap/users/{id}/featured_collections")
                );
            }
            assert_eq!(
                own.status(99).unwrap().as_str(),
                tag::status_uri("seoul.earth", id, id_scheme, username, 99)
            );
        }
        assert_eq!(
            uris.shared_inbox_uri().unwrap().as_str(),
            "https://seoul.earth/inbox"
        );
        assert_eq!(
            uris.key_id("instance", "").unwrap().as_str(),
            crate::federation::instance_actor::key_id("seoul.earth")
        );
    }
}
