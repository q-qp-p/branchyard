//! Spans and metrics reach an OTLP/HTTP collector, configured only by the
//! standard `OTEL_*` variables. The collector is a stand-in on loopback:
//! nothing leaves the machine.
//!
//! One test, because `init` installs process-wide providers and the
//! variables it reads are the process's.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // tests: a panic is the failure report

use branchyard_telemetry::{init, Config, Meters, Role, TRACE_TARGET};
use branchyard_testkit::wait;
use branchyard_testkit::{MockHttp, Response};
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{any_value::Value, KeyValue};
use prost::Message as _;

fn text(attributes: &[KeyValue], key: &str) -> Option<String> {
    attributes
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.as_ref())
        .and_then(|v| match v.value.as_ref()? {
            Value::StringValue(s) => Some(s.clone()),
            _ => None,
        })
}

#[test]
fn spans_and_metrics_reach_a_collector_over_otlp_http() {
    let collector = MockHttp::start(|_| {
        Response::new(200, Vec::new()).header("content-type", "application/x-protobuf")
    });
    // Set before anything else runs: nothing else in this binary reads the
    // environment concurrently.
    std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", collector.url());
    std::env::set_var("OTEL_EXPORTER_OTLP_HEADERS", "x-api-key=s3cret");
    std::env::set_var("OTEL_SERVICE_NAME", "by-test");
    let guard = init(Config {
        role: Role::Worker,
        service_name: "fallback-name".into(),
        service_version: "9.9.9".into(),
        layers: Vec::new(),
    });
    assert!(
        guard.exporting_traces() && guard.exporting_metrics(),
        "{guard:?}"
    );
    assert!(branchyard_telemetry::spans_enabled());

    {
        let admission = tracing::info_span!(
            target: TRACE_TARGET,
            "admission",
            otel.kind = "server",
            "by.operation" = "op_a",
        );
        let _in = admission.enter();
        let operation = tracing::info_span!(target: TRACE_TARGET, "operation", "by.fence" = 1_i64);
        let _in_operation = operation.enter();
    }
    let meters = Meters::process().expect("init installed the process's meters");
    let jobs = meters.meter("test").u64_counter("jobs_total").build();
    jobs.add(5, &[opentelemetry::KeyValue::new("kind", "task")]);
    guard.flush();

    let both = || {
        let seen = collector.requests();
        seen.iter().any(|r| r.path.ends_with("/v1/traces"))
            && seen.iter().any(|r| r.path.ends_with("/v1/metrics"))
    };
    wait::until("the collector to receive spans and metrics", both);
    let requests = collector.requests();
    let traces = requests
        .iter()
        .find(|r| r.path.ends_with("/v1/traces"))
        .unwrap();
    assert_eq!(traces.method, "POST");
    assert_eq!(traces.header("x-api-key"), Some("s3cret"));
    assert_eq!(
        traces.header("content-type"),
        Some("application/x-protobuf")
    );
    let decoded = ExportTraceServiceRequest::decode(traces.body.as_slice()).unwrap();
    let resource = decoded.resource_spans[0].resource.as_ref().unwrap();
    assert_eq!(
        text(&resource.attributes, "service.name").as_deref(),
        Some("by-test")
    );
    assert_eq!(
        text(&resource.attributes, "service.version").as_deref(),
        Some("9.9.9")
    );
    assert_eq!(
        text(&resource.attributes, "by.role").as_deref(),
        Some("worker")
    );
    let spans: Vec<_> = decoded
        .resource_spans
        .iter()
        .flat_map(|r| &r.scope_spans)
        .flat_map(|s| &s.spans)
        .collect();
    let admission = spans.iter().find(|s| s.name == "admission").unwrap();
    let operation = spans.iter().find(|s| s.name == "operation").unwrap();
    assert_eq!(operation.parent_span_id, admission.span_id);
    assert_eq!(operation.trace_id, admission.trace_id);
    assert_eq!(
        text(&admission.attributes, "by.operation").as_deref(),
        Some("op_a")
    );

    let metrics = requests
        .iter()
        .find(|r| r.path.ends_with("/v1/metrics"))
        .unwrap();
    assert_eq!(metrics.header("x-api-key"), Some("s3cret"));
    let decoded = ExportMetricsServiceRequest::decode(metrics.body.as_slice()).unwrap();
    let names: Vec<&str> = decoded
        .resource_metrics
        .iter()
        .flat_map(|r| &r.scope_metrics)
        .flat_map(|s| &s.metrics)
        .map(|m| m.name.as_str())
        .collect();
    assert!(names.contains(&"jobs_total"), "{names:?}");
    drop(guard);
    collector.finish();
}
