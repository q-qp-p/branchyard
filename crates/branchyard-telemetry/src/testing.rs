//! Capture spans in memory, for tests of the crates that emit them.
//!
//! [`capture`] installs, once per process, a global subscriber with the
//! OpenTelemetry layer over a tracer provider whose exporter keeps every
//! finished span (synchronously, with no thread). Tests share it, so a test
//! finds its own spans by trace id or by an attribute it made unique, and
//! never counts spans in total.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use opentelemetry::trace::{Status, TracerProvider as _};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::Layer as _;

/// A finished span, in plain types.
#[derive(Clone, Debug, PartialEq)]
pub struct CapturedSpan {
    /// The span's name.
    pub name: String,
    /// Its trace id, as 32 hex digits.
    pub trace_id: String,
    /// Its own id, as 16 hex digits.
    pub span_id: String,
    /// Its parent's id, when it has one.
    pub parent_id: Option<String>,
    /// `internal`, `server` or `client`, among others.
    pub kind: String,
    /// Its attributes, each value as text.
    pub attributes: BTreeMap<String, String>,
    /// The description of an error status, when it ended in error.
    pub error: Option<String>,
    /// When it began, in milliseconds since the Unix epoch.
    pub start_ms: u64,
    /// When it ended, in milliseconds since the Unix epoch.
    pub end_ms: u64,
}

/// The spans captured so far in this process.
pub struct Capture {
    exporter: InMemorySpanExporter,
    // Keeps the provider alive for the process.
    _provider: SdkTracerProvider,
}

static CAPTURE: OnceLock<Result<Capture, String>> = OnceLock::new();

/// Install the capture if it is not installed, and return it. An error says
/// another global subscriber was installed first (a test must call this
/// before anything that starts logging).
pub fn capture() -> Result<&'static Capture, String> {
    CAPTURE.get_or_init(install).as_ref().map_err(Clone::clone)
}

fn install() -> Result<Capture, String> {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let layer = tracing_opentelemetry::layer()
        .with_tracer(provider.tracer("branchyard"))
        .with_location(false)
        .with_threads(false)
        .with_target(false)
        .with_tracked_inactivity(false)
        .with_filter(tracing_subscriber::filter::filter_fn(crate::bridged));
    tracing::subscriber::set_global_default(tracing_subscriber::Registry::default().with(layer))
        .map_err(|e| format!("another global tracing subscriber is installed: {e}"))?;
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );
    opentelemetry::global::set_tracer_provider(provider.clone());
    crate::set_spans_enabled(true);
    Ok(Capture {
        exporter,
        _provider: provider,
    })
}

fn ms(time: std::time::SystemTime) -> u64 {
    branchyard_support::time::system_time_ms(time).unwrap_or(0)
}

impl Capture {
    /// Every finished span so far.
    pub fn spans(&self) -> Vec<CapturedSpan> {
        let finished = branchyard_support::best_effort(
            "read the captured spans",
            self.exporter.get_finished_spans(),
        )
        .unwrap_or_default();
        finished
            .into_iter()
            .map(|span| CapturedSpan {
                name: span.name.to_string(),
                trace_id: span.span_context.trace_id().to_string(),
                span_id: span.span_context.span_id().to_string(),
                parent_id: (span.parent_span_id != opentelemetry::trace::SpanId::INVALID)
                    .then(|| span.parent_span_id.to_string()),
                kind: format!("{:?}", span.span_kind).to_lowercase(),
                attributes: span
                    .attributes
                    .iter()
                    .map(|kv| (kv.key.to_string(), kv.value.as_str().into_owned()))
                    .collect(),
                error: match span.status {
                    Status::Error { description } => Some(description.to_string()),
                    _ => None,
                },
                start_ms: ms(span.start_time),
                end_ms: ms(span.end_time),
            })
            .collect()
    }

    /// The spans of the trace `trace_id` (32 hex digits).
    pub fn trace(&self, trace_id: &str) -> Vec<CapturedSpan> {
        self.spans()
            .into_iter()
            .filter(|s| s.trace_id == trace_id)
            .collect()
    }
}

/// The trace id of a `traceparent` header value, as 32 hex digits.
pub fn trace_id_of(traceparent: &str) -> Option<String> {
    traceparent.split('-').nth(1).map(str::to_owned)
}

/// The span id of a `traceparent` header value, as 16 hex digits.
pub fn span_id_of(traceparent: &str) -> Option<String> {
    traceparent.split('-').nth(2).map(str::to_owned)
}
