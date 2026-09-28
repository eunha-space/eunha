//! WebFinger (RFC 7033) lookup for ActivityPub actor discovery, over eunha's
//! outbound client.

/// Resolve a fediverse handle to an ActivityPub actor URL.
///
/// Performs a WebFinger lookup for `acct:{user}@{domain}` and returns the
/// `href` of the `self` link whose type contains `activity+json` or `ld+json`.
pub async fn resolve(client: &reqwest::Client, user: &str, domain: &str) -> anyhow::Result<String> {
    let url = format!(
        "https://{}/.well-known/webfinger?resource=acct:{}@{}",
        domain, user, domain
    );

    let jrd: serde_json::Value = client
        .get(&url)
        .header("Accept", "application/jrd+json, application/json")
        .send()
        .await?
        .json()
        .await?;

    jrd.get("links")
        .and_then(|l| l.as_array())
        .and_then(|arr| {
            arr.iter().find(|link| {
                link.get("rel").and_then(|r| r.as_str()) == Some("self")
                    && link
                        .get("type")
                        .and_then(|t| t.as_str())
                        .is_some_and(|t| t.contains("activity+json") || t.contains("ld+json"))
            })
        })
        .and_then(|link| link.get("href"))
        .and_then(|h| h.as_str())
        .map(str::to_owned)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no ActivityPub self link in WebFinger response for {}@{}",
                user,
                domain
            )
        })
}
