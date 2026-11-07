//! W3C trace-context propagation, through the OpenTelemetry propagator.
//!
//! A `traceparent` is what Branchyard stores with an operation (so another
//! worker continues its trace), sends to harnesses (`TRACEPARENT`) and to
//! webhook receivers (a `traceparent` header), and reads from a client's
//! request or the process's own environment. The parsing and formatting are
//! the propagator's, not ours: a malformed header, all-zero ids and an
//! unknown version all read as "no parent", and the caller starts a new
//! trace.

use opentelemetry::propagation::{Extractor, Injector, TextMapPropagator};
use opentelemetry::trace::TraceContextExt as _;
use opentelemetry::Context;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use std::collections::HashMap;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

/// The header and variable names, as the propagator writes them.
pub const TRACEPARENT_HEADER: &str = "traceparent";
/// The environment variable harnesses and child processes inherit.
pub const TRACEPARENT_ENV: &str = "TRACEPARENT";

struct One<'a>(&'a str);

impl Extractor for One<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        (key == TRACEPARENT_HEADER).then_some(self.0)
    }

    fn keys(&self) -> Vec<&str> {
        vec![TRACEPARENT_HEADER]
    }
}

/// The remote context a `traceparent` header value names, or `None` when
/// it is malformed or not a valid span context.
pub fn extract(traceparent: &str) -> Option<Context> {
    let cx = TraceContextPropagator::new().extract(&One(traceparent.trim()));
    cx.span().span_context().is_valid().then_some(cx)
}

/// Whether `traceparent` is a valid header value.
pub fn is_valid(traceparent: &str) -> bool {
    extract(traceparent).is_some()
}

/// Write `cx`'s span context into `injector` as a `traceparent` (and
/// `tracestate`, when it has one). Nothing is written for a context with no
/// valid span.
pub fn inject(cx: &Context, injector: &mut dyn Injector) {
    TraceContextPropagator::new().inject_context(cx, injector);
}

/// The `traceparent` header value of `cx`'s span, if it has a valid one.
pub fn traceparent_of(cx: &Context) -> Option<String> {
    let mut carrier: HashMap<String, String> = HashMap::new();
    inject(cx, &mut carrier);
    carrier.remove(TRACEPARENT_HEADER)
}

/// The `traceparent` of the OpenTelemetry span behind `span`. `None` when
/// no OpenTelemetry layer is installed (telemetry is off), the span was
/// filtered out, or it is not a span at all.
pub fn span_traceparent(span: &tracing::Span) -> Option<String> {
    traceparent_of(&span.context())
}

/// The `traceparent` of the span in scope on this thread.
pub fn current_traceparent() -> Option<String> {
    span_traceparent(&tracing::Span::current())
}

/// Make `traceparent`'s span the parent of `span`, which must not have
/// been entered yet to be the root of its trace. Returns whether it was
/// valid and applied.
pub fn adopt(span: &tracing::Span, traceparent: &str) -> bool {
    match extract(traceparent) {
        Some(cx) => span.set_parent(cx).is_ok(),
        None => false,
    }
}

/// Mark the OpenTelemetry span behind `span` as failed, with `message`.
/// A no-op when telemetry is off.
pub fn fail(span: &tracing::Span, message: &str) {
    span.set_status(opentelemetry::trace::Status::error(message.to_owned()));
}

/// Set an attribute on the OpenTelemetry span behind `span` after it was
/// made. A no-op when telemetry is off.
pub fn attr(span: &tracing::Span, key: &'static str, value: impl Into<opentelemetry::Value>) {
    span.set_attribute(key, value);
}

/// The `traceparent` the process was started with (`TRACEPARENT`), when it
/// is valid: a harness or a CI job that runs `by` continues its trace.
pub fn from_env() -> Option<String> {
    std::env::var(TRACEPARENT_ENV)
        .ok()
        .filter(|v| is_valid(v))
        .map(|v| v.trim().to_owned())
}

/// The `traceparent` to pass on: `span`'s own when telemetry is on, else
/// `fallback` (an incoming value passed through unchanged, so a trace
/// started upstream is not lost where nothing here records spans).
pub fn pass_on(span: &tracing::Span, fallback: Option<&str>) -> Option<String> {
    span_traceparent(span).or_else(|| fallback.filter(|f| is_valid(f)).map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::TraceId;

    const SAMPLE: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

    #[test]
    fn a_traceparent_round_trips_through_the_propagator() {
        let cx = extract(SAMPLE).expect("a valid header");
        let span_context = cx.span().span_context().clone();
        assert!(span_context.is_remote());
        assert_eq!(
            span_context.trace_id(),
            TraceId::from_hex("0af7651916cd43dd8448eb211c80319c").expect("hex")
        );
        assert_eq!(traceparent_of(&cx).as_deref(), Some(SAMPLE));
        assert_eq!(extract(&format!("  {SAMPLE}\n")).map(|_| ()), Some(()));
    }

    #[test]
    fn injection_fills_a_header_map_and_is_empty_without_a_span() {
        let cx = extract(SAMPLE).expect("a valid header");
        let mut headers: HashMap<String, String> = HashMap::new();
        inject(&cx, &mut headers);
        assert_eq!(headers.get("traceparent").map(String::as_str), Some(SAMPLE));
        let mut none: HashMap<String, String> = HashMap::new();
        inject(&Context::new(), &mut none);
        assert!(none.is_empty());
        assert_eq!(traceparent_of(&Context::new()), None);
    }

    #[test]
    fn malformed_headers_are_no_parent() {
        for bad in [
            "",
            "nonsense",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331",
            "00-00000000000000000000000000000000-b7ad6b7169203331-01",
            "00-0af7651916cd43dd8448eb211c80319c-0000000000000000-01",
            "ff-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
            "00-0AF7651916CD43DD8448EB211C80319C-b7ad6b7169203331-01",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01-extra",
        ] {
            assert!(!is_valid(bad), "{bad:?} should not parse");
            assert_eq!(pass_on(&tracing::Span::none(), Some(bad)), None);
        }
    }

    #[test]
    fn with_no_layer_an_incoming_value_is_passed_on_unchanged() {
        let span = tracing::Span::none();
        assert_eq!(span_traceparent(&span), None);
        assert_eq!(pass_on(&span, Some(SAMPLE)).as_deref(), Some(SAMPLE));
        assert_eq!(pass_on(&span, None), None);
    }
}
