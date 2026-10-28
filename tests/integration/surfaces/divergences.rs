//! The registry of deliberate differences from Mastodon.
//!
//! Eunha aims for behavioural parity, so a difference is either a bug or a
//! decision. `divergences.toml` records the decisions; these tests hold it to
//! the code, because a register nothing checks is one that quietly stops being
//! true.

use std::collections::HashSet;

use eunha::divergence;

/// Every divergence has been weighed against the release Eunha now tracks.
///
/// This is the test that makes the registry worth keeping. Adopting a new
/// Mastodon release fails here until each entry is looked at again: upstream may
/// have adopted the same idea, changed what is being diverged from, or ruled it
/// out, and none of those show up on their own.
#[tokio::test]
async fn test_every_divergence_was_reviewed_for_the_tracked_release() {
    let stale = divergence::needing_review();
    assert!(
        stale.is_empty(),
        "eunha tracks Mastodon {}, but {} divergence(s) were last judged against \
         an earlier release. Re-examine each against {} — has upstream adopted \
         it, changed it, or ruled it out? — then move `reviewed_for` forward:\n{}",
        eunha::version::MASTODON,
        stale.len(),
        eunha::version::MASTODON,
        stale
            .iter()
            .map(|d| format!("  {} (reviewed for {})", d.id, d.reviewed_for))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Each entry names a test that would fail if the divergence stopped being
/// true, and that test exists. Without this the registry could describe
/// behaviour nothing exercises.
#[tokio::test]
async fn test_every_divergence_points_at_a_test_that_exists() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for d in divergence::all() {
        let path = root.join(&d.evidence);
        assert!(
            path.is_file(),
            "divergence `{}` cites {} as evidence, which does not exist",
            d.id,
            d.evidence
        );
    }
}

/// Entries are complete and distinct: an id that repeats, or a field left
/// empty, makes the registry harder to trust than no registry.
#[tokio::test]
async fn test_entries_are_distinct_and_filled_in() {
    let divergences = divergence::all();
    assert!(
        !divergences.is_empty(),
        "the registry is empty; if Eunha has stopped diverging, say so here \
         rather than deleting the file"
    );

    let mut seen = HashSet::new();
    for d in &divergences {
        assert!(
            seen.insert(d.id.clone()),
            "duplicate divergence id `{}`",
            d.id
        );
        assert!(
            d.id.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "divergence id `{}` should be kebab-case",
            d.id
        );
        for (field, value) in [
            ("summary", &d.summary),
            ("mastodon", &d.mastodon),
            ("eunha", &d.eunha),
            ("why", &d.why),
            ("since", &d.since),
        ] {
            assert!(
                !value.trim().is_empty(),
                "divergence `{}` leaves `{field}` empty",
                d.id
            );
        }
        // `why` should give a reason rather than restate what was done.
        assert!(
            d.why.trim() != d.eunha.trim(),
            "divergence `{}` restates itself instead of giving a reason",
            d.id
        );
    }
}

/// A divergence cannot claim to predate the release it was reviewed for.
#[tokio::test]
async fn test_versions_are_coherent() {
    for d in divergence::all() {
        let parse = |v: &str| -> Vec<u32> { v.split('.').filter_map(|p| p.parse().ok()).collect() };
        assert!(
            parse(&d.since) <= parse(&d.reviewed_for),
            "divergence `{}` was introduced against {} but reviewed for {}",
            d.id,
            d.since,
            d.reviewed_for
        );
    }
}
