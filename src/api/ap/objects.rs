//! The documents local actors and their statuses are served as; ojak serves
//! them (`super::serving`).

use serde_json::{json, Value};

use super::serving::Own;
use crate::{
    error::{AppError, AppResult},
    state::AppState,
};

pub const ACTIVITY_STREAMS: &str = "application/activity+json";

/// The public collection, `ActivityPub::TagManager::COLLECTIONS[:public]`.
pub const PUBLIC_COLLECTION: &str = "https://www.w3.org/ns/activitystreams#Public";

/// The context extensions `ActivityPub::ActorSerializer` declares, in its
/// order; the ones its nested serializers add follow them when they are used.
const ACTOR_EXTENSIONS: [&str; 12] = [
    "manually_approves_followers",
    "featured",
    "also_known_as",
    "moved_to",
    "property_value",
    "discoverable",
    "suspended",
    "memorial",
    "indexable",
    "attribution_domains",
    "profile_settings",
    "interaction_policies",
];

/// The `@context` of an actor document, as `ActivityPub::Adapter` folds it
/// from `ActivityPub::ActorSerializer` and the nested serializers it used:
/// `Emoji` when it has custom emoji tags, `Hashtag` when it has profile
/// hashtags, `focalPoint` when it has an avatar, a header or an emoji's
/// image. The
/// Multikey context, Mastodon's only by way of eunha's integrity proofs, is
/// there when the actor publishes an `assertionMethod`.
fn actor_context(emoji: bool, hashtag: bool, image: bool, multikey: bool) -> Value {
    let mut extensions: Vec<&str> = ACTOR_EXTENSIONS.to_vec();
    if emoji {
        extensions.push("emoji");
    }
    if hashtag {
        extensions.push("hashtag");
    }
    // An emoji's icon is an `ActivityPub::ImageSerializer` too.
    if image || emoji {
        extensions.push("focal_point");
    }
    let mut context = super::context_helper::serialized_context(
        &["activitystreams", "security", "webfinger"],
        &extensions,
    );
    if multikey {
        if let Some(array) = context.as_array_mut() {
            array.insert(3, json!("https://w3id.org/security/multikey/v1"));
        }
    }
    context
}

/// The instance actor, served at `/actor`, as `InstanceActorsController`
/// serves `Account.representative`: `ActivityPub::ActorSerializer` limited
/// to its `id`, `type`, `preferredUsername`, `inbox`, `outbox`,
/// `publicKey`, `endpoints`, `url` and `manuallyApprovesFollowers`. Remote
/// servers fetch it for the key that verifies our signed fetches.
pub async fn instance_actor_json(state: &AppState) -> AppResult<Value> {
    let instance = &state.instance;
    let public_key = crate::federation::instance_actor::public_key(state)
        .await
        .map_err(AppError::Internal)?;
    let actor_url = crate::federation::instance_actor::actor_url(&instance.domain);
    let uris = &state.uris;
    let own = super::serving::AccountUris::new(
        uris,
        crate::federation::instance_actor::INSTANCE_ACTOR_ID,
        None,
        &instance.domain,
    );

    let actor = json!({
        "@context": actor_context(false, false, false, false),
        "id": actor_url,
        "type": "Application",
        "inbox": own.uri(Own::Inbox)?,
        "outbox": own.uri(Own::Outbox)?,
        "preferredUsername": instance.domain,
        // `about_more_url(instance_actor: true)`.
        "url": format!("https://{}/about/more?instance_actor=true", instance.domain),
        "manuallyApprovesFollowers": true,
        "publicKey": {
            "id": uris.key_id("instance", "").map_err(anyhow::Error::from)?,
            "owner": actor_url,
            "publicKeyPem": public_key,
        },
        "endpoints": {
            "sharedInbox": uris.shared_inbox_uri().map_err(anyhow::Error::from)?,
        },
    });

    Ok(actor)
}

/// How a local account is addressed in a request path: by `username`
/// (`/users/...`) or by numeric `id` (`/ap/users/...`).
#[derive(Clone, Copy)]
pub enum AccountRef<'a> {
    Username(&'a str),
    Id(i64),
}

/// Load a local account addressed by either scheme.
pub async fn load_local_account(
    state: &AppState,
    who: AccountRef<'_>,
) -> AppResult<crate::db::models::Account> {
    let account = match who {
        AccountRef::Username(username) => {
            sqlx::query_as!(
                crate::db::models::Account,
                "SELECT * FROM accounts WHERE lower(username) = lower($1) AND domain IS NULL",
                username,
            )
            .fetch_optional(&state.db)
            .await?
        }
        AccountRef::Id(id) => {
            sqlx::query_as!(
                crate::db::models::Account,
                "SELECT * FROM accounts WHERE id = $1 AND domain IS NULL",
                id,
            )
            .fetch_optional(&state.db)
            .await?
        }
    };
    account.ok_or(AppError::NotFound)
}

/// Who signed a request, as an account eunha knows: Mastodon's
/// `signed_request_account`, the `current_account` its policies ask about.
#[derive(Clone, Debug)]
pub struct Reader {
    pub id: i64,
    pub domain: Option<String>,
}

/// The account whose actor is `signer`, if eunha knows it.
pub(crate) async fn reader(state: &AppState, signer: Option<&str>) -> AppResult<Option<Reader>> {
    let Some(signer) = signer else {
        return Ok(None);
    };
    Ok(sqlx::query!(
        "SELECT id, domain FROM accounts WHERE uri = $1 AND domain IS NOT NULL LIMIT 1",
        signer,
    )
    .fetch_optional(&state.db)
    .await?
    .map(|r| Reader {
        id: r.id,
        domain: r.domain,
    }))
}

/// A local post that may be served, and what its serving depends on.
pub(crate) struct Servable {
    pub account: crate::db::models::Account,
    pub visibility: i32,
    pub reblog_of_id: Option<i64>,
    /// `@status.quote&.pending?`.
    pub quote_pending: bool,
}

impl Servable {
    /// `Status#distributable?`: public or unlisted.
    pub fn distributable(&self) -> bool {
        matches!(
            self.visibility,
            crate::db::models::vis::PUBLIC | crate::db::models::vis::UNLISTED
        )
    }
}

/// `StatusPolicy#show?` for `reader`, who may be no one: a direct or limited
/// post to whoever it mentions, a followers-only one to its author's
/// followers too, and any other to everyone but whom its author blocks or
/// whose domain it blocks. Nothing of an unavailable author's.
pub(crate) async fn may_show(
    state: &AppState,
    author_id: i64,
    status_id: i64,
    visibility: i32,
    reader: Option<&Reader>,
) -> AppResult<bool> {
    use crate::db::models::vis;
    let mentioned = |reader: &Reader| {
        sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM mentions WHERE status_id = $1 AND account_id = $2) AS "e!""#,
            status_id,
            reader.id,
        )
        .fetch_one(&state.db)
    };
    Ok(match visibility {
        vis::DIRECT | vis::LIMITED => match reader {
            Some(reader) => reader.id == author_id || mentioned(reader).await?,
            None => false,
        },
        vis::PRIVATE => match reader {
            Some(reader) => {
                reader.id == author_id
                    || sqlx::query_scalar!(
                        r#"SELECT EXISTS (SELECT 1 FROM follows
                                          WHERE account_id = $1 AND target_account_id = $2) AS "e!""#,
                        reader.id,
                        author_id,
                    )
                    .fetch_one(&state.db)
                    .await?
                    || mentioned(reader).await?
            }
            None => false,
        },
        _ => match reader {
            Some(reader) => {
                let blocked = sqlx::query_scalar!(
                    r#"SELECT (EXISTS (SELECT 1 FROM blocks WHERE account_id = $1 AND target_account_id = $2)
                               OR EXISTS (SELECT 1 FROM account_domain_blocks
                                          WHERE account_id = $1 AND domain = $3)) AS "b!""#,
                    author_id,
                    reader.id,
                    reader.domain,
                )
                .fetch_one(&state.db)
                .await?;
                !blocked
            }
            None => true,
        },
    })
}

/// The local post `id` of `who`'s, when `reader` may be shown it
/// (`@account.statuses.find` and `authorize @status, :show?`).
pub(crate) async fn servable_status(
    state: &AppState,
    who: AccountRef<'_>,
    id: i64,
    reader: Option<&Reader>,
) -> AppResult<Servable> {
    let account = load_local_account(state, who).await?;
    // `StatusPolicy#show?`: nothing of an unavailable author's.
    if account.is_unavailable() {
        return Err(AppError::NotFound);
    }
    let status = sqlx::query!(
        r#"SELECT s.visibility, s.reblog_of_id,
                  EXISTS (SELECT 1 FROM quotes q WHERE q.status_id = s.id AND q.state = 0) AS "quote_pending!"
           FROM statuses s
           WHERE s.id = $1 AND s.account_id = $2 AND s.deleted_at IS NULL"#,
        id,
        account.id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    if !may_show(state, account.id, id, status.visibility, reader).await? {
        return Err(AppError::NotFound);
    }
    Ok(Servable {
        account,
        visibility: status.visibility,
        reblog_of_id: status.reblog_of_id,
        quote_pending: status.quote_pending,
    })
}

/// A local account's profile hashtags (`Account#tags`, the `accounts_tags`
/// that `UpdateAccountService#process_hashtags` keeps), as
/// `ActivityPub::ActorSerializer::TagSerializer` writes them.
async fn hashtag_tags(state: &AppState, domain: &str, account_id: i64) -> AppResult<Vec<Value>> {
    let names = sqlx::query_scalar!(
        r#"SELECT t.name FROM accounts_tags at JOIN tags t ON t.id = at.tag_id
           WHERE at.account_id = $1 ORDER BY t.id"#,
        account_id,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(names
        .into_iter()
        .map(|name| {
            json!({
                "type": "Hashtag",
                "href": format!("https://{domain}/tags/{name}"),
                "name": format!("#{name}"),
            })
        })
        .collect())
}

/// `ActivityPub::ImageSerializer` on an avatar or header.
fn image(url: String, content_type: Option<&str>, description: &str) -> Value {
    let mut image = json!({
        "type": "Image",
        "mediaType": content_type,
        "url": url,
    });
    if !description.is_empty() {
        image["summary"] = json!(description);
    }
    image
}

/// A local account's actor document, as `ActivityPub::ActorSerializer`
/// writes it.
pub async fn actor_json(
    state: &AppState,
    domain: &str,
    account: &crate::db::models::Account,
) -> AppResult<Value> {
    let actor_url = crate::federation::tag::account_uri_of(domain, account);
    let own = super::serving::AccountUris::of(&state.uris, account);
    let available = !account.is_unavailable();

    // Account migration metadata: aliases (alsoKnownAs) + movedTo target URI.
    // `alsoKnownAs` is the account's `also_known_as`, which creating and
    // removing an alias keeps (`AccountAlias#add_to_account`).
    let mut also_known_as: Vec<String> = account.also_known_as.clone().unwrap_or_default();
    // The account's actors under the domains the instance had before, which
    // its followers were following: `eunha accounts move` names them.
    for previous in &state.instance.previous_domains {
        let old = crate::federation::tag::account_uri(
            previous,
            account.id,
            account.id_scheme,
            &account.username,
        );
        if old != actor_url && !also_known_as.contains(&old) {
            also_known_as.push(old);
        }
    }
    let moved_to: Option<String> = if let Some(moved_id) = account.moved_to_account_id {
        sqlx::query!(
            "SELECT id, id_scheme, username, domain, uri FROM accounts WHERE id = $1",
            moved_id,
        )
        .fetch_optional(&state.db)
        .await?
        .and_then(|moved| {
            if moved.domain.is_none() {
                Some(crate::federation::tag::account_uri(
                    domain,
                    moved.id,
                    moved.id_scheme,
                    &moved.username,
                ))
            } else {
                moved.uri.filter(|u| !u.is_empty())
            }
        })
    } else {
        None
    };

    let assertion_method = crate::federation::keypair::assertion_multikey(state, account.id)
        .await
        .unwrap_or_default()
        .map(|multikey| {
            let id = format!(
                "{actor_url}{}",
                crate::federation::keypair::ED25519_FRAGMENT
            );
            json!([{
                "id": id,
                "type": "Multikey",
                "controller": actor_url,
                "publicKeyMultibase": multikey,
            }])
        });

    // Since 4.7.0 the key may live in `keypairs`; an account without one
    // advertises an empty key rather than none, as it did before.
    let public_key = crate::federation::keypair::public_key(state, account.id)
        .await
        .unwrap_or_default()
        .unwrap_or_default();

    // `avatar_exists?` and `header_exists?`: never on an unavailable account.
    let has_avatar = available
        && (account
            .avatar_file_name
            .as_ref()
            .is_some_and(|s| !s.is_empty())
            || account
                .avatar_remote_url
                .as_ref()
                .is_some_and(|s| !s.is_empty()));
    let has_header = available
        && (account
            .header_file_name
            .as_ref()
            .is_some_and(|s| !s.is_empty())
            || !account.header_remote_url.is_empty());

    // Profile metadata fields, serialized as `PropertyValue` attachments so
    // remote servers show the account's fields (Mastodon's `virtual_attachments`),
    // each value through `account_field_value_format`, and the bio through
    // `account_bio_format`, mentions in both looked up as `TextFormatter` does.
    // `Account#fields`, each sanitized as `Account::Field` reads it.
    let raw_fields: Vec<(String, String)> = account
        .fields
        .as_ref()
        .and_then(|f| f.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|f| {
                    Some((
                        crate::api::mastodon::convert::sanitize_field(
                            f.get("name")?.as_str()?,
                            true,
                        ),
                        crate::api::mastodon::convert::sanitize_field(
                            f.get("value")?.as_str()?,
                            true,
                        ),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut texts: Vec<&str> = vec![&account.note];
    texts.extend(raw_fields.iter().map(|(_, value)| value.as_str()));
    let lookup = crate::api::mastodon::formatting::mention_lookup(state, &texts).await;
    let attachment: Vec<Value> = if available {
        raw_fields
            .iter()
            .map(|(name, value)| {
                json!({
                    "type": "PropertyValue",
                    "name": name,
                    "value": crate::formatter::local_field_value(value, domain, &lookup),
                })
            })
            .collect()
    } else {
        Vec::new()
    };

    // `virtual_tags`: the custom emoji of the profile (`Account#emojis`,
    // read from its `emojifiable_text`), then its hashtags.
    let (emoji_tags, hashtags) = if available {
        // `[note, display_name, fields.map(&:name), fields.map(&:value)]`.
        let mut emojifiable: Vec<&str> = vec![&account.note, &account.display_name];
        emojifiable.extend(raw_fields.iter().map(|(name, _)| name.as_str()));
        emojifiable.extend(raw_fields.iter().map(|(_, value)| value.as_str()));
        (
            crate::api::ap::note::emoji_tags_for(state, &emojifiable).await?,
            hashtag_tags(state, domain, account.id).await?,
        )
    } else {
        (Vec::new(), Vec::new())
    };
    let context = actor_context(
        !emoji_tags.is_empty(),
        !hashtags.is_empty(),
        has_avatar || has_header,
        assertion_method.is_some(),
    );
    let mut tag = emoji_tags;
    tag.extend(hashtags);

    // Local accounts store an empty `url` column; the human profile URL is
    // `/@username` (matching the Mastodon API serializer and Mastodon core).
    let profile_url = format!("https://{}/@{}", domain, account.username);
    // `type`: `Service` for a bot (`AUTOMATED_ACTOR_TYPES`), `Group` for a
    // group, and `Person` for everyone else.
    let actor_type = match account.actor_type.as_deref() {
        Some("Application" | "Service") => "Service",
        Some("Group") => "Group",
        _ => "Person",
    };
    let discoverable = account.discoverable.unwrap_or(false);
    // `interaction_policy`: who may feature the account in a collection
    // without asking.
    let can_feature = if !discoverable {
        actor_url.clone()
    } else if account.locked {
        own.uri(Own::Followers)?.into()
    } else {
        PUBLIC_COLLECTION.to_owned()
    };

    let mut actor = json!({
        "@context": context,
        "id": actor_url,
        // `local_username_and_domain`.
        "webfinger": format!("{}@{}", account.username, domain),
        "type": actor_type,
        "following": own.uri(Own::Following)?,
        "followers": own.uri(Own::Followers)?,
        "inbox": own.uri(Own::Inbox)?,
        "outbox": own.uri(Own::Outbox)?,
        "featured": own.uri(Own::Featured)?,
        "featuredTags": own.uri(Own::Tags)?,
        "preferredUsername": account.username,
        "name": if !available || account.display_name.is_empty() {
            &account.username
        } else {
            &account.display_name
        },
        "summary": if available {
            crate::formatter::local_bio(&account.note, domain, &lookup)
        } else {
            String::new()
        },
        "url": profile_url,
        "manuallyApprovesFollowers": available && account.locked,
        "discoverable": available && discoverable,
        "indexable": available && account.indexable,
        // `created_at.midnight.iso8601`.
        "published": account.created_at.format("%Y-%m-%dT00:00:00Z").to_string(),
        "memorial": account.memorial,
        "showFeatured": account.show_featured,
        "showMedia": account.show_media,
        "showRepliesInMedia": account.show_media_replies,
        "interactionPolicy": {
            "canFeature": { "automaticApproval": [can_feature] },
        },
        "featuredCollections": own.uri(Own::Collections)?,
    });
    let members = actor.as_object_mut().expect("an object");
    if available {
        if let Some(moved_to) = moved_to {
            members.insert("movedTo".into(), json!(moved_to));
        }
        if !also_known_as.is_empty() {
            members.insert("alsoKnownAs".into(), json!(also_known_as));
        }
    }
    if account.suspended_at.is_some() {
        members.insert("suspended".into(), json!(true));
    }
    if let Some(domains) = account
        .attribution_domains
        .as_ref()
        .filter(|domains| !domains.is_empty())
    {
        members.insert("attributionDomains".into(), json!(domains));
    }
    members.insert(
        "publicKey".into(),
        json!({
            "id": own.key_id()?,
            "owner": actor_url,
            "publicKeyPem": public_key,
        }),
    );
    members.insert("tag".into(), json!(tag));
    members.insert("attachment".into(), json!(attachment));
    members.insert(
        "endpoints".into(),
        json!({
            "sharedInbox": state.uris.shared_inbox_uri().map_err(anyhow::Error::from)?,
        }),
    );
    if has_avatar {
        members.insert(
            "icon".into(),
            image(
                crate::api::mastodon::convert::account_avatar_url_for(&state.urls, account),
                account.avatar_content_type.as_deref(),
                &account.avatar_description,
            ),
        );
    }
    if has_header {
        members.insert(
            "image".into(),
            image(
                crate::api::mastodon::convert::account_header_url_for(&state.urls, account),
                account.header_content_type.as_deref(),
                &account.header_description,
            ),
        );
    }
    // FEP-521a: the Ed25519 key this account signs integrity proofs with,
    // published as a Multikey so a peer can resolve a proof's
    // `verificationMethod`. Absent until the account first signs something.
    if let Some(assertion_method) = assertion_method {
        members.insert("assertionMethod".into(), assertion_method);
    }
    Ok(actor)
}
