//! The documents local actors and their statuses are served as; feder serves
//! them (`super::serving`).

use serde_json::{json, Value};

use crate::{
    error::{AppError, AppResult},
    state::AppState,
};

pub const ACTIVITY_STREAMS: &str = "application/activity+json";

/// The instance actor, served at `/actor`: an Application actor whose public key
/// remote servers fetch to verify our signed authorized-fetch GET requests.
pub async fn instance_actor_json(state: &AppState) -> AppResult<Value> {
    let instance = &state.instance;
    let public_key = crate::federation::instance_actor::public_key(state)
        .await
        .map_err(AppError::Internal)?;
    let actor_url = crate::federation::instance_actor::actor_url(&instance.domain);

    let actor = json!({
        "@context": [
            "https://www.w3.org/ns/activitystreams",
            "https://w3id.org/security/v1",
        ],
        "id": actor_url,
        "type": "Application",
        "preferredUsername": instance.domain,
        "inbox": format!("https://{}/inbox", instance.domain),
        "url": actor_url,
        "manuallyApprovesFollowers": true,
        "publicKey": {
            "id": format!("{actor_url}#main-key"),
            "owner": actor_url,
            "publicKeyPem": public_key,
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
                "SELECT * FROM accounts WHERE username = $1 AND domain IS NULL",
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

/// Load a status bundle, enforcing that it belongs to the addressed account and
/// is publicly dereferenceable (public or unlisted). Private/direct posts are
/// not served over unauthenticated AP GET.
pub(crate) async fn status_bundle(
    state: &AppState,
    domain: &str,
    who: AccountRef<'_>,
    id: i64,
) -> AppResult<super::note::NoteBundle> {
    let account = load_local_account(state, who).await?;
    // An unavailable account's objects are not dereferenceable, matching
    // `StatusPolicy#show?` on the REST side.
    if account.is_unavailable() {
        return Err(AppError::NotFound);
    }
    let owner_ok = sqlx::query_scalar!(
        r#"SELECT EXISTS(
             SELECT 1 FROM statuses s
             WHERE s.id = $1 AND s.account_id = $2
               AND s.deleted_at IS NULL AND s.visibility IN (0, 1)
           )"#,
        id,
        account.id,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(false);
    if !owner_ok {
        return Err(AppError::NotFound);
    }
    super::note::build_note(state, domain, id)
        .await?
        .ok_or(AppError::NotFound)
}

pub async fn actor_json(
    state: &AppState,
    domain: &str,
    account: &crate::db::models::Account,
) -> AppResult<Value> {
    let base = format!("https://{}", domain);
    let actor_url = crate::federation::tag::account_uri_of(domain, account);

    // Account migration metadata: aliases (alsoKnownAs) + movedTo target URI.
    let mut also_known_as: Vec<String> = sqlx::query_scalar!(
        "SELECT uri FROM account_aliases WHERE account_id = $1 ORDER BY created_at",
        account.id,
    )
    .fetch_all(&state.db)
    .await?;
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

    let has_avatar = account
        .avatar_file_name
        .as_ref()
        .is_some_and(|s| !s.is_empty())
        || account
            .avatar_remote_url
            .as_ref()
            .is_some_and(|s| !s.is_empty());
    let has_header = account
        .header_file_name
        .as_ref()
        .is_some_and(|s| !s.is_empty())
        || !account.header_remote_url.is_empty();
    let avatar_url = crate::api::mastodon::convert::account_avatar_url_for(&state.urls, account);
    let header_url = crate::api::mastodon::convert::account_header_url_for(&state.urls, account);

    // Profile metadata fields, serialized as `PropertyValue` attachments so
    // remote servers show the account's fields (Mastodon's `virtual_attachments`).
    let attachment: Vec<Value> = account
        .fields
        .as_ref()
        .and_then(|f| f.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|f| {
                    let name = f.get("name")?.as_str()?;
                    let value = f.get("value")?.as_str()?;
                    Some(json!({
                        "type": "PropertyValue",
                        "name": name,
                        "value": crate::api::mastodon::formatting::format_field_value(value),
                    }))
                })
                .collect()
        })
        .unwrap_or_default();

    // Custom emoji used in the display name / bio, serialized as `Emoji` tags so
    // remote servers can render them (Mastodon's `virtual_tags`, emojis part).
    let tag =
        crate::api::ap::note::emoji_tags_for(state, &account.display_name, &account.note).await?;

    // The `note` column holds the raw bio; the AP actor `summary` must be HTML,
    // so render it on the fly (Mastodon's `account_bio_format`).
    let summary = crate::api::mastodon::formatting::render_content(
        &account.note,
        domain,
        &std::collections::HashMap::new(),
    );

    // Local accounts store an empty `url` column; the human profile URL is
    // `/@username` (matching the Mastodon API serializer and Mastodon core).
    let profile_url = format!("https://{}/@{}", domain, account.username);
    // Bots federate as `Service`; everyone else as `Person`.
    let actor_type = account.actor_type.as_deref().unwrap_or("Person");

    let actor = json!({
        "@context": [
            "https://www.w3.org/ns/activitystreams",
            "https://w3id.org/security/v1",
            // Defines `assertionMethod` and `Multikey` (FEP-521a).
            "https://w3id.org/security/multikey/v1",
            {
                "manuallyApprovesFollowers": "as:manuallyApprovesFollowers",
                "alsoKnownAs": { "@id": "as:alsoKnownAs", "@type": "@id" },
                "movedTo": { "@id": "as:movedTo", "@type": "@id" },
                "schema": "http://schema.org#",
                "PropertyValue": "schema:PropertyValue",
                "value": "schema:value",
                "toot": "http://joinmastodon.org/ns#",
                "Emoji": "toot:Emoji",
                "featured": { "@id": "toot:featured", "@type": "@id" },
                "featuredCollections": { "@id": "toot:featuredCollections", "@type": "@id" },
                "discoverable": "toot:discoverable",
                "indexable": "toot:indexable",
                "fep": "https://w3id.org/fep/044f#",
                "quote": { "@id": "fep:quote", "@type": "@id" },
                "quoteUrl": { "@id": "fep:quote", "@type": "@id" },
            }
        ],
        "id": actor_url,
        "type": actor_type,
        "following": format!("{}/following", actor_url),
        "followers": format!("{}/followers", actor_url),
        "inbox": format!("{}/inbox", actor_url),
        "outbox": format!("{}/outbox", actor_url),
        "featured": format!("{}/collections/featured", actor_url),
        "featuredCollections": format!("{}/collections", actor_url),
        "preferredUsername": account.username,
        "name": account.display_name,
        "summary": summary,
        "url": profile_url,
        "attachment": attachment,
        "tag": tag,
        "manuallyApprovesFollowers": account.locked,
        "discoverable": account.discoverable,
        "indexable": account.indexable,
        "published": account.created_at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "icon": if has_avatar { Some(json!({ "type": "Image", "url": avatar_url })) } else { None },
        "image": if has_header { Some(json!({ "type": "Image", "url": header_url })) } else { None },
        "publicKey": {
            "id": format!("{}#main-key", actor_url),
            "owner": actor_url,
            "publicKeyPem": public_key,
        },
        "endpoints": {
            "sharedInbox": format!("{}/inbox", base),
        },
        // FEP-521a: the Ed25519 key this account signs integrity proofs with,
        // published as a Multikey so a peer can resolve a proof's
        // `verificationMethod`. Absent until the account first signs something.
        "assertionMethod": assertion_method,
        "alsoKnownAs": also_known_as,
        "movedTo": moved_to,
    });

    Ok(actor)
}
