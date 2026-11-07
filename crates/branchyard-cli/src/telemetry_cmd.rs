//! OpenTelemetry for a command run locally.
//!
//! Off unless `OTEL_EXPORTER_OTLP_ENDPOINT` (or a per-signal endpoint) is
//! set, as for a server. When it is, a command runs under a `by <command>`
//! span, a child of the `TRACEPARENT` it inherited when that is valid (a CI
//! job or a harness that runs `by` continues its own trace), and once it
//! ends, the turns, tool calls and connector calls the repository recorded
//! while it ran become the same spans and metrics a server's operation
//! makes (`docs/observability.md`): the engine records events, and one
//! recorder turns them into telemetry for both, so a local `by run` and a
//! served task are measured alike.

use std::collections::BTreeSet;

use branchyard::{Activity, Yard};
use branchyard_client::api::FeedEntry;
use branchyard_server::observe::{record_events, record_starts, Observability};
use branchyard_telemetry::propagation;

/// Feed events read per page.
const PAGE: usize = 500;

/// A command being traced.
pub struct Traced {
    span: tracing::Span,
    yard: Option<Yard>,
    /// The repository feed's position when the command began.
    cursor: u64,
    started_ms: u64,
    /// The branches that existed when the command began.
    before: BTreeSet<String>,
}

/// The feed's position now: what a command recorded is after it.
fn end_of_feed(yard: &Yard) -> u64 {
    let mut cursor = 0;
    while let Ok(page) = yard.events_since(cursor, PAGE) {
        if page.events.is_empty() {
            break;
        }
        cursor = page.next_cursor;
    }
    cursor
}

/// Start tracing a command named `name` (`run`, `send`, ...), when
/// telemetry is on for this process: `exporting` says a guard is pushing
/// something. Returns `None` otherwise, which costs nothing.
pub fn begin(name: &str, exporting: bool) -> Option<Traced> {
    if !exporting {
        return None;
    }
    let span = tracing::info_span!(
        target: branchyard_telemetry::TRACE_TARGET,
        "by",
        otel.name = format!("by {name}").as_str(),
        "by.command" = name,
    );
    if let Some(inherited) = propagation::from_env() {
        propagation::adopt(&span, &inherited);
    }
    let yard = crate::commands::open().ok();
    let cursor = yard.as_ref().map_or(0, end_of_feed);
    let before = yard
        .as_ref()
        .and_then(|y| y.branches().ok())
        .map(|all| all.into_iter().map(|b| b.name).collect())
        .unwrap_or_default();
    Some(Traced {
        span,
        yard,
        cursor,
        started_ms: branchyard_support::time::now_ms(),
        before,
    })
}

impl Traced {
    /// The command's span, to enter while it runs.
    pub fn span(&self) -> &tracing::Span {
        &self.span
    }

    /// The command ended: mark the span failed if it did, and record what
    /// the repository's feed holds from the command's start.
    pub fn finish(self, failed: Option<&str>) {
        if let Some(message) = failed {
            propagation::fail(&self.span, message);
        }
        if let Some(yard) = &self.yard {
            record_since(yard, &self, &self.span);
        }
    }
}

/// Turn the feed after `cursor` into metrics and spans under `parent`.
fn record_since(yard: &Yard, run: &Traced, parent: &tracing::Span) {
    let mut entries: Vec<FeedEntry> = Vec::new();
    let mut at = run.cursor;
    while let Ok(page) = yard.events_since(at, PAGE) {
        if page.events.is_empty() {
            break;
        }
        at = page.next_cursor;
        entries.extend(page.events.into_iter().map(|e| FeedEntry {
            seq: e.position,
            branch: e.branch,
            at_ms: e.event.at_ms,
            activity: e.event.activity,
        }));
    }
    if entries.is_empty() {
        return;
    }
    let branches: BTreeSet<String> = entries.iter().map(|e| e.branch.clone()).collect();
    let harness: std::collections::BTreeMap<String, String> = yard
        .branches()
        .map(|all| all.into_iter().map(|b| (b.name, b.harness)).collect())
        .unwrap_or_default();
    let observability = Observability::from_env();
    let traceparent = propagation::span_traceparent(parent);
    record_events(
        &observability,
        &branches,
        &|b| harness.get(b).cloned().unwrap_or_else(|| "unknown".into()),
        &entries,
        traceparent.as_deref(),
    );
    // A branch the command made started during it.
    let created: BTreeSet<String> = entries
        .iter()
        .filter(|e| matches!(e.activity, Activity::Prompt(_)))
        .filter(|e| !run.before.contains(&e.branch))
        .map(|e| e.branch.clone())
        .collect();
    record_starts(
        &observability.metrics,
        "local",
        run.started_ms,
        &created,
        &entries,
    );
}
