//! `tracing` spans become OpenTelemetry spans: parent and child, attributes,
//! errors, an incoming `traceparent`, and spans written after the fact.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // tests: a panic is the failure report

use branchyard_telemetry::propagation::{adopt, attr, fail, pass_on, span_traceparent};
use branchyard_telemetry::spans::{self, Finished, Kind};
use branchyard_telemetry::testing::{capture, span_id_of, trace_id_of};
use branchyard_telemetry::TRACE_TARGET;
use opentelemetry::KeyValue;

fn trace() -> String {
    // A trace id no other test in this binary has.
    let n = branchyard_support::time::now_nanos();
    format!("{n:032x}")
}

#[test]
fn nested_spans_are_parent_and_child_with_their_attributes() {
    let capture = capture().unwrap();
    let trace = trace();
    let incoming = format!("00-{trace}-00f067aa0ba902b7-01");
    let parent = tracing::info_span!(
        target: TRACE_TARGET,
        "admission",
        otel.kind = "server",
        "by.operation" = "op_1",
        "by.priority" = 3_i64,
    );
    assert!(adopt(&parent, &incoming));
    {
        let _in = parent.enter();
        let child = tracing::info_span!(
            target: TRACE_TARGET,
            "operation",
            otel.name = "operation task",
            "by.fence" = 7_i64,
        );
        let _in_child = child.enter();
        attr(&child, "by.took_over", true);
        fail(&child, "boom");
        // A span of another target is not bridged.
        let log_only = tracing::info_span!("request", path = "/x");
        assert_eq!(span_traceparent(&log_only), None);
    }
    // The span's own context is a valid traceparent in the same trace.
    let tp = span_traceparent(&parent).expect("a traceparent");
    assert_eq!(trace_id_of(&tp).as_deref(), Some(trace.as_str()));
    assert_eq!(
        pass_on(&parent, Some("garbage")).as_deref(),
        Some(tp.as_str())
    );
    // A span is exported when it closes.
    drop(parent);
    let spans = capture.trace(&trace);
    let admission = spans.iter().find(|s| s.name == "admission").unwrap();
    let operation = spans.iter().find(|s| s.name == "operation task").unwrap();
    assert_eq!(admission.parent_id.as_deref(), Some("00f067aa0ba902b7"));
    assert_eq!(admission.kind, "server");
    assert_eq!(admission.attributes["by.operation"], "op_1");
    assert_eq!(admission.attributes["by.priority"], "3");
    assert_eq!(operation.parent_id.as_ref(), Some(&admission.span_id));
    assert_eq!(operation.attributes["by.took_over"], "true");
    assert_eq!(operation.error.as_deref(), Some("boom"));
    assert!(admission.error.is_none());
    assert!(
        !operation
            .attributes
            .keys()
            .any(|k| k.starts_with("code.") || k.starts_with("thread.")),
        "noise attributes are off: {:?}",
        operation.attributes
    );
    assert!(!spans.iter().any(|s| s.name == "request"));
    assert_eq!(span_id_of(&tp).as_deref(), Some(admission.span_id.as_str()));
}

#[test]
fn a_malformed_or_missing_parent_starts_a_trace_of_its_own() {
    let capture = capture().unwrap();
    let span = tracing::info_span!(target: TRACE_TARGET, "admission", "by.operation" = "op_lone");
    assert!(!adopt(&span, "nonsense"));
    assert!(!adopt(
        &span,
        "00-00000000000000000000000000000000-00f067aa0ba902b7-01"
    ));
    drop(span);
    let lone = capture
        .spans()
        .into_iter()
        .find(|s| s.attributes.get("by.operation").map(String::as_str) == Some("op_lone"))
        .unwrap();
    assert_eq!(lone.parent_id, None);
    assert_eq!(lone.trace_id.len(), 32);
}

#[test]
fn a_span_written_after_the_fact_keeps_its_times_and_children_join_it() {
    let capture = capture().unwrap();
    assert!(branchyard_telemetry::spans_enabled());
    let trace = trace();
    let parent = format!("00-{trace}-00f067aa0ba902b7-01");
    let turn = spans::record(Finished {
        name: "turn".into(),
        parent: Some(parent),
        kind: Kind::Internal,
        start_ms: 1_000_000,
        end_ms: 1_004_000,
        attributes: vec![KeyValue::new("by.harness", "codex")],
        error: Some("failed".into()),
    })
    .expect("a traceparent for the children");
    let child = spans::record(Finished {
        name: "tool Bash".into(),
        parent: Some(turn.clone()),
        kind: Kind::Client,
        start_ms: 1_001_000,
        end_ms: 1_002_000,
        attributes: vec![],
        error: None,
    });
    assert!(child.is_some());
    // A child that ends before it starts is clamped, not negative.
    spans::record(Finished {
        name: "backwards".into(),
        parent: Some(turn.clone()),
        kind: Kind::Internal,
        start_ms: 1_003_000,
        end_ms: 1_000_000,
        attributes: vec![],
        error: None,
    });
    let spans = capture.trace(&trace);
    let turn_span = spans.iter().find(|s| s.name == "turn").unwrap();
    assert_eq!(
        (turn_span.start_ms, turn_span.end_ms),
        (1_000_000, 1_004_000)
    );
    assert_eq!(turn_span.parent_id.as_deref(), Some("00f067aa0ba902b7"));
    assert_eq!(turn_span.error.as_deref(), Some("failed"));
    let tool = spans.iter().find(|s| s.name == "tool Bash").unwrap();
    assert_eq!(tool.parent_id.as_ref(), Some(&turn_span.span_id));
    assert_eq!(tool.kind, "client");
    let backwards = spans.iter().find(|s| s.name == "backwards").unwrap();
    assert_eq!(backwards.start_ms, backwards.end_ms);
}
