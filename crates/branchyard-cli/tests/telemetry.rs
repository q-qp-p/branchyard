//! A local `by run` exports its spans and metrics over OTLP when an
//! endpoint is set, and nothing otherwise (docs/observability.md). The
//! built `by`, the fake ACP agent and a stand-in collector on loopback:
//! hermetic, no model and no network beyond loopback.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report

use branchyard_testkit::{MockHttp, Response};
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{any_value::Value, KeyValue};
use prost::Message as _;

fn collector() -> MockHttp {
    MockHttp::start(|_| {
        Response::new(200, Vec::new()).header("content-type", "application/x-protobuf")
    })
}

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
fn a_local_run_exports_its_command_turn_and_metrics() {
    let collector = collector();
    let mut repo = branchyard_testkit::repo!();
    repo.set_env("OTEL_EXPORTER_OTLP_ENDPOINT", &collector.url());
    repo.set_env("OTEL_SERVICE_NAME", "by-local-test");
    let out = repo.by_agent(&["run", "WRITE hello.txt=hi", "--name", "hello"]);
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // `by` flushed when it ended, so what it sent is here already.
    let requests = collector.requests();
    let traces = requests
        .iter()
        .filter(|r| r.path.ends_with("/v1/traces"))
        .map(|r| ExportTraceServiceRequest::decode(r.body.as_slice()).unwrap())
        .collect::<Vec<_>>();
    assert!(
        !traces.is_empty(),
        "no traces among {} requests",
        requests.len()
    );
    let resource = traces[0].resource_spans[0].resource.as_ref().unwrap();
    assert_eq!(
        text(&resource.attributes, "service.name").as_deref(),
        Some("by-local-test")
    );
    assert_eq!(
        text(&resource.attributes, "by.role").as_deref(),
        Some("cli")
    );
    let spans: Vec<_> = traces
        .iter()
        .flat_map(|t| &t.resource_spans)
        .flat_map(|r| &r.scope_spans)
        .flat_map(|s| &s.spans)
        .collect();
    let command = spans
        .iter()
        .find(|s| s.name == "by run")
        .unwrap_or_else(|| {
            panic!(
                "no `by run` span in {:?}",
                spans.iter().map(|s| &s.name).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        text(&command.attributes, "by.command").as_deref(),
        Some("run")
    );
    let turn = spans
        .iter()
        .find(|s| s.name == "turn")
        .expect("a turn span");
    assert_eq!(turn.parent_span_id, command.span_id);
    assert_eq!(turn.trace_id, command.trace_id);
    assert_eq!(
        text(&turn.attributes, "by.harness").as_deref(),
        Some("gemini-cli")
    );
    assert_eq!(
        text(&turn.attributes, "by.outcome").as_deref(),
        Some("completed")
    );

    let names: Vec<String> = requests
        .iter()
        .filter(|r| r.path.ends_with("/v1/metrics"))
        .map(|r| ExportMetricsServiceRequest::decode(r.body.as_slice()).unwrap())
        .flat_map(|m| m.resource_metrics)
        .flat_map(|r| r.scope_metrics)
        .flat_map(|s| s.metrics)
        .map(|m| m.name)
        .collect();
    for expected in [
        "branchyard_turns_started_total",
        "branchyard_turns_ended_total",
        "branchyard_turn_duration_seconds",
        "gen_ai.client.operation.duration",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "{expected} in {names:?}"
        );
    }
    collector.finish();
}

#[test]
fn nothing_is_sent_when_the_sdk_is_disabled_or_no_endpoint_is_set() {
    let collector = collector();
    let mut repo = branchyard_testkit::repo!();
    // An endpoint, and the kill switch.
    repo.set_env("OTEL_EXPORTER_OTLP_ENDPOINT", &collector.url());
    repo.set_env("OTEL_SDK_DISABLED", "true");
    let out = repo.by_agent(&["run", "WRITE a.txt=1", "--name", "quiet"]);
    assert!(out.status.success());
    assert!(
        collector.requests().is_empty(),
        "{:?}",
        collector.requests()
    );
    // No endpoint at all.
    let quiet = branchyard_testkit::repo!();
    let out = quiet.by_agent(&["run", "WRITE a.txt=1", "--name", "quiet"]);
    assert!(out.status.success());
    assert!(collector.requests().is_empty());
    collector.finish();
}
