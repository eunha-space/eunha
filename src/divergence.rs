//! Where Eunha deliberately differs from the Mastodon release it implements.
//!
//! Eunha aims for behavioural parity, so a difference is either a bug or a
//! decision. `divergences.toml` is where the decisions are recorded, and this
//! module reads it so that tests can hold it to the code — prose that nothing
//! checks is prose that stops being true.
//!
//! The part that earns its keep over time is [`Divergence::reviewed_for`]:
//! adopting a newer Mastodon release fails the suite until every entry has been
//! re-examined against it. A divergence that was right against one release is
//! not automatically right against the next, and upstream may have adopted the
//! same idea, changed the thing being diverged from, or ruled it out.
//!
//! Mastodon: none

use serde::Deserialize;

/// What kind of difference an entry describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Eunha does something Mastodon does not.
    Addition,
    /// Mastodon does something Eunha does not.
    Omission,
    /// Both do it, differently.
    Behaviour,
}

impl Kind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Addition => "addition",
            Self::Omission => "omission",
            Self::Behaviour => "behaviour",
        }
    }
}

/// One recorded decision to differ.
#[derive(Debug, Clone, Deserialize)]
pub struct Divergence {
    /// Stable kebab-case identifier, referenced from commits and issues.
    pub id: String,
    pub kind: Kind,
    pub summary: String,
    /// What the tracked Mastodon release does.
    pub mastodon: String,
    /// What Eunha does instead.
    pub eunha: String,
    /// Why — the reason, not a restatement of `eunha`.
    pub why: String,
    /// A test that would fail if this stopped being true.
    pub evidence: String,
    /// The Mastodon release this was introduced against.
    pub since: String,
    /// The Mastodon release it was last judged correct for.
    pub reviewed_for: String,
}

#[derive(Debug, Deserialize)]
struct Registry {
    #[serde(default, rename = "divergence")]
    divergences: Vec<Divergence>,
}

/// Every recorded divergence.
///
/// # Panics
/// Panics if `divergences.toml` is not valid, which would mean the file was
/// edited into a state no test could read.
#[must_use]
pub fn all() -> Vec<Divergence> {
    let registry: Registry = toml::from_str(include_str!("../divergences.toml"))
        .expect("divergences.toml is not a valid registry");
    registry.divergences
}

/// Divergences last judged against an older Mastodon release than the one being
/// tracked now, and so due another look.
#[must_use]
pub fn needing_review() -> Vec<Divergence> {
    all()
        .into_iter()
        .filter(|d| d.reviewed_for != crate::version::MASTODON)
        .collect()
}
