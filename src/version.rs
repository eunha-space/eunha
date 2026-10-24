//! Eunha's version, and the Mastodon release it implements.
//!
//! Both halves come from `Cargo.toml`'s `version`, which carries the target
//! Mastodon release as semver build metadata (`0.2.0+mastodon.4.7.0`).
//! `build.rs` splits them apart and checks the Mastodon half against
//! `mastodon.toml`, so these constants cannot drift from the manifest.
//!
//! Mastodon: lib/mastodon/version.rb

/// Eunha's own version, without the `+mastodon.x.y.z` build metadata.
pub const EUNHA: &str = env!("EUNHA_VERSION");

/// The full version, as releases are tagged: `0.2.0+mastodon.4.7.0`.
pub const EUNHA_FULL: &str = env!("CARGO_PKG_VERSION");

/// The Mastodon release whose schema and API this build implements.
pub const MASTODON: &str = env!("EUNHA_MASTODON_VERSION");

/// Newest Mastodon migration covered by `migrations/`, matching the
/// `define(version:)` of that release's `db/schema.rb`.
pub const MASTODON_SCHEMA: &str = env!("EUNHA_MASTODON_SCHEMA_VERSION");

/// The version string clients see from `/api/v1/instance`, `/api/v2/instance`
/// and NodeInfo.
///
/// Mastodon clients feature-detect by parsing a trailing `(compatible; Mastodon
/// x.y.z)` out of this field, so the Mastodon version has to be stated in the
/// shape they expect rather than left as build metadata.
pub fn compatible_string() -> String {
    format!("{EUNHA} (compatible; Mastodon {MASTODON})")
}

/// `User-Agent` for outgoing federation requests.
pub const USER_AGENT: &str = concat!("eunha/", env!("EUNHA_VERSION"), " (ActivityPub)");
