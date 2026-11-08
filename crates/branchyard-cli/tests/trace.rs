//! `by trace`: a finished run's trajectory as an OTLP trace, from the event
//! log alone. The built `by`, the fake ACP agent and, for `--endpoint`, a
//! stand-in collector on loopback: hermetic, no model.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report

use branchyard_testkit::{MockHttp, Response};
use serde_json::Value;

fn spans(document: &Value) -> Vec<Value> {
    document["resourceSpans"][0]["scopeSpans"][0]["spans"]
        .as_array()
        .unwrap_or_else(|| panic!("no spans in {document}"))
        .clone()
}

fn name_of(span: &Value) -> &str {
    span["name"].as_str().unwrap()
}

fn attr<'a>(span: &'a Value, key: &str) -> Option<&'a str> {
    span["attributes"]
        .as_array()?
        .iter()
        .find(|kv| kv["key"] == key)?["value"]["stringValue"]
        .as_str()
}

#[test]
fn a_finished_run_is_exported_as_a_deterministic_otlp_trace() {
    let repo = branchyard_testkit::repo!();
    let out = repo.by_agent(&[
        "run",
        "SH echo hi --token=hunter2secret",
        "--name",
        "traced",
    ]);
    assert!(out.status.success(), "{out:?}");

    let first = repo.json(&["trace", "traced", "--json"]);
    let second = repo.json(&["trace", "traced", "--json"]);
    assert_eq!(first, second, "the same log exports the same trace");

    let all = spans(&first);
    let branch = all.iter().find(|s| name_of(s) == "branch traced").unwrap();
    let turn = all.iter().find(|s| name_of(s) == "turn").unwrap();
    let tool = all
        .iter()
        .find(|s| name_of(s).starts_with("tool "))
        .unwrap();
    assert_eq!(turn["parentSpanId"], branch["spanId"]);
    assert_eq!(tool["parentSpanId"], turn["spanId"]);
    for span in &all {
        assert_eq!(span["traceId"].as_str().unwrap().len(), 32);
        assert_eq!(span["spanId"].as_str().unwrap().len(), 16);
        assert_eq!(span["traceId"], branch["traceId"]);
    }
    assert_eq!(attr(turn, "gen_ai.operation.name"), Some("invoke_agent"));
    assert_eq!(attr(turn, "by.outcome"), Some("completed"));
    assert_eq!(attr(turn, "by.source"), Some("event-log"));
    assert_eq!(attr(tool, "by.tool.target.kind"), Some("command"));
    let text = first.to_string();
    assert!(!text.contains("hunter2secret"), "{text}");
    assert!(text.contains("[redacted]"), "{text}");

    // Another branch is another trace.
    let out = repo.by_agent(&["run", "say hi", "--name", "other"]);
    assert!(out.status.success());
    let other = repo.json(&["trace", "other", "--json"]);
    assert_ne!(
        spans(&other)[0]["traceId"],
        branch["traceId"],
        "a trace id comes from the branch's own log"
    );
}

#[test]
fn the_default_output_is_a_tree_and_otlp_writes_the_same_document_to_a_file() {
    let repo = branchyard_testkit::repo!();
    assert!(repo
        .by_agent(&["run", "SH echo hi", "--name", "tree"])
        .status
        .success());
    let tree = String::from_utf8(repo.ok(&["trace", "tree"]).stdout).unwrap();
    let lines: Vec<&str> = tree.lines().collect();
    assert!(lines[0].starts_with("trace "), "{tree}");
    assert!(lines[1].starts_with("branch tree"), "{tree}");
    assert!(lines[2].starts_with("  turn"), "{tree}");
    assert!(lines[3].starts_with("    tool "), "{tree}");

    let file = repo.dir.join("trace.json");
    let said = String::from_utf8(
        repo.ok(&["trace", "tree", "--otlp", file.to_str().unwrap()])
            .stdout,
    )
    .unwrap();
    assert!(said.contains("wrote 3 spans"), "{said}");
    let written: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(written, repo.json(&["trace", "tree", "--json"]));
}

#[test]
fn an_endpoint_receives_the_document_over_otlp_http() {
    let collector = MockHttp::start(|_| Response::new(200, b"{}".to_vec()));
    let mut repo = branchyard_testkit::repo!();
    repo.set_env(
        "OTEL_EXPORTER_OTLP_HEADERS",
        "x-api-key=s3cret%2Bkey,x-tenant=acme",
    );
    // The headers are credentials for the configured collector: they go
    // only to that endpoint.
    repo.set_env("OTEL_EXPORTER_OTLP_ENDPOINT", &collector.url());
    // That endpoint would also receive the process's own live export.
    repo.set_env("OTEL_SDK_DISABLED", "true");
    assert!(repo
        .by_agent(&["run", "say hi", "--name", "sent"])
        .status
        .success());
    let said = String::from_utf8(
        repo.ok(&["trace", "sent", "--endpoint", &collector.url()])
            .stdout,
    )
    .unwrap();
    assert!(said.contains("sent 2 spans"), "{said}");
    let requests = collector.await_requests(1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert!(request.path.ends_with("/v1/traces"), "{}", request.path);
    assert_eq!(request.header("content-type"), Some("application/json"));
    assert_eq!(request.header("x-api-key"), Some("s3cret+key"));
    assert_eq!(request.header("x-tenant"), Some("acme"));
    let posted: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(posted, repo.json(&["trace", "sent", "--json"]));
    collector.finish();
}

#[test]
fn an_endpoint_that_is_not_the_configured_collector_gets_no_headers() {
    let collector = MockHttp::start(|_| Response::new(200, b"{}".to_vec()));
    let mut repo = branchyard_testkit::repo!();
    repo.set_env("OTEL_EXPORTER_OTLP_HEADERS", "x-api-key=s3cret");
    repo.set_env("OTEL_EXPORTER_OTLP_ENDPOINT", "https://collector.invalid");
    repo.set_env("OTEL_SDK_DISABLED", "true");
    assert!(repo
        .by_agent(&["run", "say hi", "--name", "other"])
        .status
        .success());
    repo.ok(&["trace", "other", "--endpoint", &collector.url()]);
    let requests = collector.await_requests(1);
    assert_eq!(requests[0].header("x-api-key"), None);
    collector.finish();
}

#[test]
fn a_branch_with_no_events_or_no_existence_is_a_report_with_a_next_step() {
    let repo = branchyard_testkit::repo!();
    let out = repo.by(&["trace", "nowhere", "--json"]);
    assert!(!out.status.success());
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("nowhere"), "{text}");
    // The flags that choose where it goes exclude each other.
    let both = repo.by(&["trace", "x", "--json", "--otlp", "f.json"]);
    assert!(!both.status.success());
}
