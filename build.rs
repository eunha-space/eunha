//! Keeps `Cargo.toml`'s version metadata and `mastodon.toml` in agreement, and
//! exports both halves of the version to the crate.
//!
//! Eunha's releases are tagged `v<semver>+mastodon.<mastodon version>`, e.g.
//! `v0.2.0+mastodon.4.7.0`. The build metadata after `+` names the Mastodon
//! release whose schema and API this build implements; it is not part of the
//! semver ordering, so eunha's own version bumps freely between Mastodon
//! releases. Tags mirror `Cargo.toml`'s `version`, which mirrors this file's
//! check against `mastodon.toml`.

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=mastodon.toml");
    println!("cargo:rerun-if-changed=build.rs");
    // `sqlx::migrate!` reads `migrations/` at compile time, but on stable Rust
    // it cannot tell cargo about the directory, so a new migration would not
    // reach the binary until something else forced a rebuild. Watching the
    // directory here reruns this script, and so recompiles the crate, whenever
    // a migration is added, edited or removed.
    println!("cargo:rerun-if-changed=migrations");

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let manifest_path = Path::new(&manifest_dir).join("mastodon.toml");
    let manifest = std::fs::read_to_string(&manifest_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", manifest_path.display()));

    let mastodon_version = field(&manifest, "version");
    let schema_version = field(&manifest, "schema_version");

    // `0.2.0+mastodon.4.7.0` -> ("0.2.0", "mastodon.4.7.0")
    let full = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let (semver, metadata) = full.split_once('+').unwrap_or_else(|| {
        panic!(
            "Cargo.toml version `{full}` is missing its `+mastodon.<version>` build metadata; \
             expected `{full}+mastodon.{mastodon_version}`"
        )
    });

    let expected = format!("mastodon.{mastodon_version}");
    assert_eq!(
        metadata, expected,
        "Cargo.toml version metadata `+{metadata}` disagrees with mastodon.toml \
         (`version = \"{mastodon_version}\"`); expected `+{expected}`"
    );

    println!("cargo:rustc-env=EUNHA_VERSION={semver}");
    println!("cargo:rustc-env=EUNHA_MASTODON_VERSION={mastodon_version}");
    println!("cargo:rustc-env=EUNHA_MASTODON_SCHEMA_VERSION={schema_version}");
}

/// Pull a bare `key = "value"` out of `mastodon.toml`.
///
/// The manifest is deliberately small and flat so that the build script needs
/// no TOML dependency.
fn field(manifest: &str, key: &str) -> String {
    manifest
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .find_map(|line| {
            let (found, value) = line.split_once('=')?;
            (found.trim() == key).then(|| value.trim().trim_matches('"').to_string())
        })
        .unwrap_or_else(|| panic!("mastodon.toml is missing `{key}`"))
}
