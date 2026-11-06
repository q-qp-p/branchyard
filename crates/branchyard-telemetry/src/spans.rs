//! Spans written after the fact.
//!
//! Most spans are `tracing` spans, opened and closed as the work happens.
//! A turn, a tool call or a connector call in a served operation is known
//! only from the events it recorded, with their own timestamps, once the
//! operation has finished; [`record`] writes such a span with explicit start
//! and end times through the OpenTelemetry API, as a child of a
//! `traceparent`, so it lands in the same trace as the live spans.

use std::time::SystemTime;

use opentelemetry::trace::{
    Span as _, SpanKind as OtelKind, Status, TraceContextExt as _, Tracer as _,
};
use opentelemetry::{Context, KeyValue};

use crate::propagation::{extract, traceparent_of};

/// What a span measures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Work inside the process.
    Internal,
    /// A call made to another service.
    Client,
    /// A request served.
    Server,
}

/// A finished span to write.
#[derive(Clone, Debug)]
pub struct Finished {
    /// The span's name.
    pub name: String,
    /// The parent's `traceparent`; with none (or an invalid one) the span
    /// starts a trace of its own.
    pub parent: Option<String>,
    /// What it measures.
    pub kind: Kind,
    /// When it began, in milliseconds since the Unix epoch.
    pub start_ms: u64,
    /// When it ended, in milliseconds since the Unix epoch.
    pub end_ms: u64,
    /// Its attributes.
    pub attributes: Vec<KeyValue>,
    /// Set when it ended in error, with what went wrong.
    pub error: Option<String>,
}

/// The last millisecond `jiff` can name: the end of the year 9999.
const LATEST_MS: i64 = 253_402_300_799_000;

fn at(ms: u64) -> SystemTime {
    let ms = i64::try_from(ms).unwrap_or(LATEST_MS).min(LATEST_MS);
    jiff::Timestamp::from_millisecond(ms)
        .map_or_else(|_| SystemTime::from(jiff::Timestamp::MAX), SystemTime::from)
}

/// Write `span` through the process's tracer. Returns the new span's
/// `traceparent`, for its children; `None` when telemetry is off (nothing
/// is written then), so callers can skip building spans at all with
/// [`crate::spans_enabled`].
pub fn record(span: Finished) -> Option<String> {
    if !crate::spans_enabled() {
        return None;
    }
    let tracer = opentelemetry::global::tracer("branchyard");
    let parent = span.parent.as_deref().and_then(extract).unwrap_or_default();
    let builder = tracer
        .span_builder(span.name)
        .with_kind(match span.kind {
            Kind::Internal => OtelKind::Internal,
            Kind::Client => OtelKind::Client,
            Kind::Server => OtelKind::Server,
        })
        .with_start_time(at(span.start_ms))
        .with_attributes(span.attributes);
    let mut started = tracer.build_with_context(builder, &parent);
    if let Some(error) = span.error {
        started.set_status(Status::error(error));
    }
    let context = started.span_context().clone();
    started.end_with_timestamp(at(span.end_ms.max(span.start_ms)));
    traceparent_of(&Context::new().with_remote_span_context(context))
}
