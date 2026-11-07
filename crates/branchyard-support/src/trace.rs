//! The seam between code that has spans to open and the telemetry setup
//! that decides where they go.
//!
//! Crates below the binaries (the engine, the sandbox providers, the
//! workspace) must not depend on OpenTelemetry. They open `tracing` spans
//! through `trace_span!`, which builds nothing unless [`enabled`] (the
//! process exports spans), and ask the installed [`Hooks`] for the three
//! things only the OpenTelemetry SDK can do: make a `traceparent` the parent
//! of a span, read a span's `traceparent`, and write a span whose times are
//! already known. `branchyard-telemetry` sets the switch and the hooks when
//! it installs an exporter; with none installed every function here is a
//! cheap no-op.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// The `target` of the spans that belong in a trace.
pub const TARGET: &str = "branchyard_trace";

/// The `target` of the spans of store queries, which are exported only
/// when [`set_store_spans`] turned them on: there are thousands per
/// operation.
pub const STORE_TARGET: &str = "branchyard_trace_store";

pub use tracing;

static ENABLED: AtomicBool = AtomicBool::new(false);
static STORE_SPANS: AtomicBool = AtomicBool::new(false);
static HOOKS: OnceLock<Hooks> = OnceLock::new();

/// Whether finished spans go anywhere in this process.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Turn span building on or off; called by the telemetry setup.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// Whether a span is built for each store query: spans are on and the
/// operator asked for them (`BRANCHYARD_TRACE_STORE`).
pub fn store_spans() -> bool {
    enabled() && STORE_SPANS.load(Ordering::Relaxed)
}

/// Turn the per-query store spans on or off.
pub fn set_store_spans(on: bool) {
    STORE_SPANS.store(on, Ordering::Relaxed);
}

/// The value of an attribute of a [`Finished`] span.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// Text.
    Str(String),
    /// An integer.
    Int(i64),
    /// A flag.
    Bool(bool),
}

impl From<&str> for Value {
    fn from(v: &str) -> Value {
        Value::Str(v.to_owned())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Value {
        Value::Str(v)
    }
}
impl From<i64> for Value {
    fn from(v: i64) -> Value {
        Value::Int(v)
    }
}
impl From<u64> for Value {
    fn from(v: u64) -> Value {
        Value::Int(i64::try_from(v).unwrap_or(i64::MAX))
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Value {
        Value::Bool(v)
    }
}

/// A span whose start and end are already known.
#[derive(Clone, Debug)]
pub struct Finished {
    /// The span's name.
    pub name: String,
    /// The parent's `traceparent`.
    pub parent: Option<String>,
    /// `internal`, `client` or `server`.
    pub kind: &'static str,
    /// Start, in milliseconds since the Unix epoch.
    pub start_ms: u64,
    /// End, in milliseconds since the Unix epoch.
    pub end_ms: u64,
    /// Its attributes.
    pub attributes: Vec<(&'static str, Value)>,
    /// Set when it ended in error.
    pub error: Option<String>,
}

/// What only the OpenTelemetry SDK can do.
#[derive(Clone, Copy)]
pub struct Hooks {
    /// Make a `traceparent` the parent of a span; whether it applied.
    pub adopt: fn(&tracing::Span, &str) -> bool,
    /// A span's own `traceparent`.
    pub traceparent: fn(&tracing::Span) -> Option<String>,
    /// Write a finished span; its `traceparent`.
    pub record: fn(&Finished) -> Option<String>,
}

/// Install the hooks (the first call wins).
pub fn set_hooks(hooks: Hooks) {
    // A second install is the same SDK: nothing to report.
    let _installed = HOOKS.set(hooks).is_ok();
}

/// Make `traceparent` the parent of `span`. False when it did not apply
/// (no SDK, or a malformed header).
pub fn adopt(span: &tracing::Span, traceparent: &str) -> bool {
    HOOKS.get().is_some_and(|h| (h.adopt)(span, traceparent))
}

/// The `traceparent` of `span`, when the SDK has one for it.
pub fn traceparent(span: &tracing::Span) -> Option<String> {
    HOOKS.get().and_then(|h| (h.traceparent)(span))
}

/// Write `span`; its `traceparent`. `None` when spans are off.
pub fn record(span: &Finished) -> Option<String> {
    if !enabled() {
        return None;
    }
    HOOKS.get().and_then(|h| (h.record)(span))
}

/// Where [`Timer`] sends a duration: the metrics registry of the process
/// (family name, labels, seconds), installed by the binary that has one.
pub type DurationSink = fn(&'static str, &[(&'static str, &str)], f64);

static SINK: OnceLock<DurationSink> = OnceLock::new();

/// Install the sink durations go to (the first call wins).
pub fn set_duration_sink(sink: DurationSink) {
    // A second install is the same registry: nothing to report.
    let _installed = SINK.set(sink).is_ok();
}

/// Send one duration to the installed sink; nothing without one.
pub fn observe_duration(family: &'static str, labels: &[(&'static str, &str)], seconds: f64) {
    if let Some(sink) = SINK.get() {
        sink(family, labels, seconds);
    }
}

/// Times one action and reports it, with a `result` label, to the
/// [`DurationSink`]. Dropped without [`Timer::finish`] it reports an
/// `error`: the action unwound.
#[derive(Debug)]
pub struct Timer {
    family: &'static str,
    labels: Vec<(&'static str, String)>,
    started: std::time::Instant,
    reported: bool,
}

impl Timer {
    /// Start timing an action of `family`, with `labels` (the `result`
    /// label is added when it ends).
    pub fn start(family: &'static str, labels: Vec<(&'static str, String)>) -> Timer {
        Timer {
            family,
            labels,
            started: std::time::Instant::now(),
            reported: false,
        }
    }

    /// Report the action as ended, `ok` or not.
    pub fn finish(mut self, ok: bool) {
        self.report(if ok { "ok" } else { "error" });
    }

    fn report(&mut self, result: &str) {
        if std::mem::replace(&mut self.reported, true) {
            return;
        }
        let mut pairs: Vec<(&'static str, &str)> =
            self.labels.iter().map(|(k, v)| (*k, v.as_str())).collect();
        pairs.push(("result", result));
        observe_duration(self.family, &pairs, self.started.elapsed().as_secs_f64());
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        self.report("error");
    }
}

/// Open a span of the trace, or [`tracing::Span::none`] when spans are off:
/// `trace_span!("name", "by.key" = value, ...)`.
#[macro_export]
macro_rules! trace_span {
    ($name:expr $(, $($fields:tt)*)?) => {
        if $crate::trace::enabled() {
            $crate::trace::tracing::info_span!(target: "branchyard_trace", $name $(, $($fields)*)?)
        } else {
            $crate::trace::tracing::Span::none()
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_are_none_unless_enabled_and_record_is_a_no_op() {
        // This test binary never installs an exporter.
        assert!(!enabled());
        let span = trace_span!("x", "by.k" = 1_i64);
        assert!(span.is_none());
        assert_eq!(traceparent(&span), None);
        assert!(!adopt(
            &span,
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"
        ));
        let finished = Finished {
            name: "n".into(),
            parent: None,
            kind: "internal",
            start_ms: 1,
            end_ms: 2,
            attributes: vec![("a", 1_u64.into())],
            error: None,
        };
        assert_eq!(record(&finished), None);
        assert!(!store_spans());
    }
}
