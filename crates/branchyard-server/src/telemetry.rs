//! Traces of Branchyard's own work: admission, claim, the operation a worker
//! runs, each turn it ran, the tool calls and connector calls inside them.
//! See `docs/observability.md`.
//!
//! Spans are [`tracing`] spans bridged to OpenTelemetry by
//! `branchyard-telemetry`; this module names them and fixes their
//! attributes, so the span tree is defined in one place.
//!
//! * `admission` and `operation <kind>` are live spans, entered on the
//!   thread that does the work, so the log lines written meanwhile (with
//!   `--log-format json`) can be joined to the trace.
//! * `claim` covers the time an operation waited queued; it is written
//!   when the claim is made, from the admission time ([`claim`]).
//! * `turn`, `tool <name>` and `connector <operation>` happen in the engine
//!   and the harness, so they are written after the operation that ran them
//!   finishes, from the events it recorded, with those events' timestamps
//!   (see [`crate::observe`]).
//!
//! The admission's `traceparent` is stored with the operation
//! (`StoredOperation::trace`), so the worker that claims it, on any server
//! sharing the database, continues the same trace. With telemetry off, an
//! incoming `traceparent` is stored and passed on unchanged.

use branchyard_support::LockExt as _;
use branchyard_telemetry::propagation;
use branchyard_telemetry::spans;
use opentelemetry::KeyValue;
use std::collections::HashMap;
use std::sync::Mutex;
use tracing::Span;

/// Branches whose latest trace is remembered for webhook deliveries.
const REMEMBERED: usize = 4096;

/// The trace each branch's latest operation ran in, by `repo/branch`, for
/// the `traceparent` of webhook deliveries.
#[derive(Debug, Default)]
pub struct BranchTraces {
    branches: Mutex<HashMap<String, String>>,
}

impl BranchTraces {
    /// Remember that `repo`'s `branch` is being worked on in `traceparent`'s
    /// trace, for webhook deliveries of its activity.
    pub fn note(&self, repo: &str, branch: &str, traceparent: &str) {
        let mut branches = self.branches.lock_recovering("branch traces");
        if branches.len() >= REMEMBERED {
            branches.clear();
        }
        branches.insert(format!("{repo}/{branch}"), traceparent.to_owned());
    }

    /// The trace `repo`'s `branch` was last worked on in, here.
    pub fn get(&self, repo: &str, branch: &str) -> Option<String> {
        self.branches
            .lock_recovering("branch traces")
            .get(&format!("{repo}/{branch}"))
            .cloned()
    }
}

/// What an operation's admission knows.
pub struct Admission<'a> {
    /// The operation's id.
    pub operation: &'a str,
    /// The repository.
    pub repo: &'a str,
    /// The operation's kind (`task`, `merge`, ...).
    pub kind: &'a str,
    /// The tenant that admitted it.
    pub tenant: &'a str,
    /// Its scheduling priority.
    pub priority: i32,
}

/// The `admission` span (a server span), a child of `incoming` when that is
/// a valid `traceparent`. Enter it for the duration of the admission.
pub fn admission(admission: &Admission<'_>, incoming: Option<&str>) -> Span {
    let span = tracing::info_span!(
        target: branchyard_telemetry::TRACE_TARGET,
        "admission",
        otel.kind = "server",
        "by.operation" = admission.operation,
        "by.repo" = admission.repo,
        "by.kind" = admission.kind,
        "by.tenant" = admission.tenant,
        "by.priority" = i64::from(admission.priority),
    );
    if let Some(incoming) = incoming {
        propagation::adopt(&span, incoming);
    }
    span
}

/// What a claim knows.
pub struct Claimed<'a> {
    /// The operation's id.
    pub operation: &'a str,
    /// The claiming worker.
    pub worker: &'a str,
    /// The claim's fence.
    pub fence: i64,
    /// Whether the claim took over another worker's lapsed one.
    pub took_over: bool,
    /// The operation's scheduling priority.
    pub priority: i32,
    /// When the operation was admitted, in milliseconds since the epoch.
    pub admitted_ms: u64,
    /// Now, in milliseconds since the epoch.
    pub claimed_ms: u64,
}

/// Write the `claim` span: the operation's wait in the queue, from its
/// admission to this claim, a child of `admission` (the stored trace).
pub fn claim(claimed: &Claimed<'_>, admission: &str) {
    if !branchyard_telemetry::spans_enabled() {
        return;
    }
    spans::record(spans::Finished {
        name: "claim".into(),
        parent: Some(admission.to_owned()),
        kind: spans::Kind::Internal,
        start_ms: claimed.admitted_ms,
        end_ms: claimed.claimed_ms,
        attributes: vec![
            KeyValue::new("by.operation", claimed.operation.to_owned()),
            KeyValue::new("by.worker", claimed.worker.to_owned()),
            KeyValue::new("by.fence", claimed.fence),
            KeyValue::new("by.took_over", claimed.took_over),
            KeyValue::new(
                "by.waited_ms",
                i64::try_from(claimed.claimed_ms.saturating_sub(claimed.admitted_ms))
                    .unwrap_or(i64::MAX),
            ),
            KeyValue::new("by.priority", i64::from(claimed.priority)),
        ],
        error: None,
    });
}

/// What a worker's run of an operation knows.
pub struct Running<'a> {
    /// The operation's id.
    pub operation: &'a str,
    /// The repository.
    pub repo: &'a str,
    /// The operation's kind.
    pub kind: &'a str,
    /// The tenant that admitted it.
    pub tenant: &'a str,
    /// The branches it works on, comma-joined.
    pub branches: &'a str,
    /// The claim's fence.
    pub fence: i64,
}

/// The `operation <kind>` span, a child of `admission` (the stored trace).
/// Enter it around the executor, so the engine's own spans and log lines
/// are inside it.
pub fn operation(running: &Running<'_>, admission: Option<&str>) -> Span {
    let name = format!("operation {}", running.kind);
    let span = tracing::info_span!(
        target: branchyard_telemetry::TRACE_TARGET,
        "operation",
        otel.name = name.as_str(),
        "by.operation" = running.operation,
        "by.repo" = running.repo,
        "by.kind" = running.kind,
        "by.tenant" = running.tenant,
        "by.branches" = running.branches,
        "by.fence" = running.fence,
    );
    if let Some(admission) = admission {
        propagation::adopt(&span, admission);
    }
    span
}

/// Mark `span` failed, with `message`.
pub fn fail(span: &Span, message: &str) {
    branchyard_telemetry::propagation::fail(span, message);
}

/// Set an attribute on `span` after it was made.
pub fn attr(span: &Span, key: &'static str, value: impl Into<opentelemetry::Value>) {
    branchyard_telemetry::propagation::attr(span, key, value);
}

/// The `traceparent` to store or pass on for `span`: its own, or `incoming`
/// unchanged when telemetry is off.
pub fn traceparent(span: &Span, incoming: Option<&str>) -> Option<String> {
    propagation::pass_on(span, incoming)
}
