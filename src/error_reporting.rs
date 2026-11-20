//! Errors for whoever runs the process, the hosting provider when there is
//! one, rather than only for whoever reads its log.
//!
//! Everything goes through `tracing`, which already carries each event's
//! tenant in the `tenant{domain=…}` span ([`crate::tenants::span`]). What is
//! reported:
//!
//!  -  every `ERROR` event;
//!  -  a `WARN` event that carries a `report` field, for something an operator
//!     has to act on although no request failed — a TOTP secret the
//!     configured keys cannot open, say.
//!
//! An event may carry a `kind` field naming what went wrong, from [`KINDS`].
//!
//! Each reported event counts in `eunha_errors_total{tenant, level, kind}` on
//! the private metrics listener ([`crate::telemetry`]), and, when the process
//! is given a DSN, goes to a server that speaks Sentry's protocol (Sentry
//! itself, self-hosted Sentry, GlitchTip), tagged with its tenant. Nothing is
//! sent anywhere by default.
//!
//! Mastodon: none

use std::fmt;

use tracing::{
    field::{Field, Visit},
    span, Event, Level, Metadata, Subscriber,
};
use tracing_subscriber::{layer::Context, registry::LookupSpan, Layer};

/// The `kind`s an event may report, so that the metric's labels stay
/// bounded. Anything else counts as `other`.
pub const KINDS: &[&str] = &[
    // A request failed in a way that is eunha's fault: a 500.
    "internal",
    // A database error surfaced as a 500.
    "database",
    // A stored secret the configured ActiveRecord encryption keys cannot open.
    "secret_undecryptable",
    // The configured ActiveRecord encryption keys do not open what the
    // database holds, found by checking a sample of it.
    "encryption_keys",
    // A tenant could not start, and its host answers 503.
    "tenant_not_started",
];

/// The label for an event without a `kind`.
const UNCLASSIFIED: &str = "unclassified";

/// The label for an event outside every tenant, such as the process starting.
const NO_TENANT: &str = "none";

/// The domain of the tenant a span belongs to, kept in the span's extensions
/// by [`Layer`] so that every event inside it can find it.
struct TenantDomain(String);

/// Whether an event is reported, and at which level.
fn reported_level(metadata: &Metadata<'_>) -> Option<&'static str> {
    match *metadata.level() {
        Level::ERROR => Some("error"),
        Level::WARN if metadata.fields().field("report").is_some() => Some("warning"),
        _ => None,
    }
}

/// The tenant an event happened in: its own `tenant` field, as an event
/// about a tenant from outside its span has, or its `tenant` span, or `none`.
fn tenant_of<S>(event: &Event<'_>, ctx: &Context<'_, S>) -> String
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let mut named = FieldValue::of("tenant");
    event.record(&mut named);
    if let Some(tenant) = named.value {
        return tenant;
    }
    ctx.event_scope(event)
        .and_then(|scope| {
            scope
                .from_root()
                .find_map(|span| span.extensions().get::<TenantDomain>().map(|d| d.0.clone()))
        })
        .unwrap_or_else(|| NO_TENANT.to_string())
}

/// The bounded label for an event's `kind`.
fn kind_label(kind: Option<&str>) -> &'static str {
    match kind {
        None => UNCLASSIFIED,
        Some(kind) => KINDS
            .iter()
            .find(|k| **k == kind)
            .copied()
            .unwrap_or("other"),
    }
}

/// Reads one field's value as a string.
struct FieldValue {
    name: &'static str,
    value: Option<String>,
}

impl FieldValue {
    fn of(name: &'static str) -> Self {
        Self { name, value: None }
    }
}

impl Visit for FieldValue {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == self.name {
            self.value = Some(value.to_string());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        // `%domain` arrives here as a `DisplayValue`, whose `Debug` is the
        // value's `Display`.
        if field.name() == self.name {
            self.value = Some(format!("{value:?}"));
        }
    }
}

/// Remembers each tenant span's domain, and counts reported events in
/// `eunha_errors_total`.
///
/// It goes before [`sentry_layer`], which reads the domain it keeps.
#[derive(Default)]
pub struct ErrorLayer;

impl<S> Layer<S> for ErrorLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        if attrs.metadata().name() != "tenant" {
            return;
        }
        let mut domain = FieldValue::of("domain");
        attrs.record(&mut domain);
        if let (Some(domain), Some(span)) = (domain.value, ctx.span(id)) {
            span.extensions_mut().insert(TenantDomain(domain));
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let Some(level) = reported_level(event.metadata()) else {
            return;
        };
        let mut kind = FieldValue::of("kind");
        event.record(&mut kind);
        metrics::counter!(
            "eunha_errors_total",
            "tenant" => tenant_of(event, &ctx),
            "level" => level,
            "kind" => kind_label(kind.value.as_deref()),
        )
        .increment(1);
    }
}

/// Sends each reported event to Sentry as an error event, tagged with its
/// `tenant` and `kind`, with its fields as context and an error field
/// recorded as `&dyn Error` as the exception. Inert until [`init`] has set a
/// client up.
///
/// No breadcrumbs: every tenant shares the process's Sentry scope, so the
/// lines leading up to one tenant's error would include other tenants'.
pub struct SentryLayer {
    inner: sentry::integrations::tracing::ErrorLayer,
}

/// The [`SentryLayer`] for the process.
pub fn sentry_layer() -> SentryLayer {
    SentryLayer {
        inner: sentry::integrations::tracing::ErrorLayer::new(),
    }
}

impl<S> Layer<S> for SentryLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        if reported_level(event.metadata()).is_none() || sentry::Hub::current().client().is_none() {
            return;
        }
        let tenant = tenant_of(event, &ctx);
        let mut kind = FieldValue::of("kind");
        event.record(&mut kind);
        let kind = kind_label(kind.value.as_deref());
        sentry::with_scope(
            |scope| {
                scope.set_tag("tenant", tenant);
                scope.set_tag("kind", kind);
            },
            || self.inner.on_event(event, ctx),
        );
    }
}

/// Set up the Sentry client when a DSN is given, as `--sentry-dsn` or
/// `SENTRY_DSN`. Keep the guard for the life of the process: dropping it
/// sends what is still queued.
///
/// Nothing that identifies a person is attached (`send_default_pii` stays
/// off): no IP addresses, cookies, headers or request bodies, only what the
/// event's own fields say, which name users by id.
pub fn init(dsn: Option<&str>) -> anyhow::Result<Option<sentry::ClientInitGuard>> {
    let Some(dsn) = dsn.map(str::trim).filter(|dsn| !dsn.is_empty()) else {
        return Ok(None);
    };
    // Parsed here, where `ClientOptions::dsn` would panic.
    dsn.parse::<sentry::types::Dsn>()
        .map_err(|error| anyhow::anyhow!("--sentry-dsn is not a Sentry DSN: {error}"))?;
    let guard = sentry::init(
        sentry::ClientOptions::new()
            .dsn(dsn)
            .release(env!("EUNHA_VERSION"))
            .send_default_pii(false),
    );
    Ok(Some(guard))
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use tracing_subscriber::layer::SubscriberExt as _;

    /// What `eunha_errors_total` counted, as `(tenant, level, kind, count)`.
    fn counted(run: impl FnOnce()) -> Vec<(String, String, String, u64)> {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let subscriber = tracing_subscriber::registry().with(ErrorLayer);
        metrics::with_local_recorder(&recorder, || {
            tracing::subscriber::with_default(subscriber, run);
        });
        let mut counts: Vec<_> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(key, ..)| key.key().name() == "eunha_errors_total")
            .map(|(key, _, _, value)| {
                let label = |name: &str| {
                    key.key()
                        .labels()
                        .find(|l| l.key() == name)
                        .map(|l| l.value().to_string())
                        .unwrap_or_default()
                };
                let DebugValue::Counter(count) = value else {
                    panic!("eunha_errors_total is a counter");
                };
                (label("tenant"), label("level"), label("kind"), count)
            })
            .collect();
        counts.sort();
        counts
    }

    #[test]
    fn counts_errors_and_reported_warnings_by_tenant_and_kind() {
        let counts = counted(|| {
            crate::tenants::span("seoul.earth").in_scope(|| {
                tracing::error!(kind = "internal", "internal error");
                tracing::error!(kind = "internal", "internal error");
                tracing::warn!(report = true, kind = "secret_undecryptable", "cannot open");
                // Neither of these is reported.
                tracing::warn!("a warning nobody has to act on");
                tracing::info!(kind = "internal", "an info line");
                tracing::info_span!("request").in_scope(|| {
                    tracing::error!(kind = "made-up kind", "nested in a request");
                });
            });
            tracing::error!("outside every tenant");
            tracing::error!(tenant = %"furaxy.example", kind = "tenant_not_started", "not started");
        });
        assert_eq!(
            counts,
            vec![
                (
                    "furaxy.example".into(),
                    "error".into(),
                    "tenant_not_started".into(),
                    1
                ),
                ("none".into(), "error".into(), "unclassified".into(), 1),
                ("seoul.earth".into(), "error".into(), "internal".into(), 2),
                ("seoul.earth".into(), "error".into(), "other".into(), 1),
                (
                    "seoul.earth".into(),
                    "warning".into(),
                    "secret_undecryptable".into(),
                    1
                ),
            ]
        );
    }

    #[test]
    fn sends_reported_events_to_sentry_tagged_with_their_tenant() {
        let subscriber = tracing_subscriber::registry()
            .with(ErrorLayer)
            .with(sentry_layer());
        let events = sentry::test::with_captured_events(|| {
            tracing::subscriber::with_default(subscriber, || {
                crate::tenants::span("seoul.earth").in_scope(|| {
                    let error = std::io::Error::other("no configured key opened it");
                    tracing::error!(
                        kind = "internal",
                        error = &error as &dyn std::error::Error,
                        "internal error"
                    );
                    tracing::warn!(
                        report = true,
                        kind = "secret_undecryptable",
                        user_id = 21,
                        "cannot open"
                    );
                    tracing::warn!("not reported");
                    tracing::info!("not reported either");
                });
            });
        });
        assert_eq!(events.len(), 2);
        let tags = |i: usize| {
            let tags = &events[i].tags;
            (tags.get("tenant").cloned(), tags.get("kind").cloned())
        };
        assert_eq!(
            tags(0),
            (Some("seoul.earth".into()), Some("internal".into()))
        );
        assert_eq!(events[0].level, sentry::Level::Error);
        assert_eq!(
            events[0].exception.values[0].value.as_deref(),
            Some("no configured key opened it")
        );
        assert!(events[0].breadcrumbs.is_empty());
        assert_eq!(
            tags(1),
            (
                Some("seoul.earth".into()),
                Some("secret_undecryptable".into())
            )
        );
        assert_eq!(events[1].level, sentry::Level::Warning);
    }

    #[test]
    fn no_dsn_sends_nothing() {
        assert!(init(None).unwrap().is_none());
        assert!(init(Some("  ")).unwrap().is_none());
        assert!(init(Some("not a dsn")).is_err());
    }
}
