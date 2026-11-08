//! `by trace`: a branch's trajectory as an OpenTelemetry trace, rebuilt from
//! its event log, so a run that is over can be loaded into Jaeger or Tempo.
//! The spans are `branchyard::trace_export`'s; this module reads the logs,
//! writes the OTLP/JSON document and prints, saves or posts it. See
//! `docs/observability.md#by-trace`.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use branchyard::trace_export::{self, BranchLog, Span, Value};
use branchyard::Yard;
use branchyard_client::http;
use serde_json::{json, Value as Json};

use crate::commands::{open, print, Env, Outcome, Target};
use crate::errors::Report;

/// What `by trace` was asked.
pub struct Args<'a> {
    /// The branch whose trajectory to export.
    pub branch: &'a str,
    /// Include the branches delegated under it, recursively.
    pub subtree: bool,
    /// Print the OTLP/JSON document instead of the tree.
    pub json: bool,
    /// Write the OTLP/JSON document to this file.
    pub otlp: Option<&'a Path>,
    /// POST the OTLP/JSON document to this collector.
    pub endpoint: Option<&'a str>,
}

/// `by trace BRANCH [--subtree] [--json | --otlp FILE | --endpoint URL]`.
pub fn main(_env: &Env, target: &Target, args: &Args<'_>) -> Outcome {
    if let Target::Remote(_) = target {
        return Err(
            Report::usage("a trace is rebuilt from this repository's own event log")
                .next("run it without --remote, in the repository")
                .into(),
        );
    }
    let yard = open()?;
    let logs = logs(&yard, args.branch, args.subtree)?;
    let spans = trace_export::spans(&logs);
    let Some(root) = spans.first() else {
        return Err(Report::new(
            "not_found",
            format!("branch {} has recorded no events", args.branch),
        )
        .next(format!(
            "run a turn on it first, then `by trace {}`",
            args.branch
        ))
        .into());
    };
    let document = otlp_json(&spans, &service_name());
    if let Some(path) = args.otlp {
        let text = encode(&document)?;
        std::fs::write(path, text).map_err(|e| {
            Report::new("io", format!("could not write {}: {e}", path.display()))
                .next("choose a path the current user can write")
        })?;
        return print(&format!(
            "wrote {} spans of trace {} to {}\n",
            spans.len(),
            root.trace_id,
            path.display()
        ));
    }
    if let Some(url) = args.endpoint {
        let body = encode(&document)?;
        let headers_sent = post(url, body.as_bytes(), &Collector::from_env())?;
        let note = match headers_sent {
            Sent::WithHeaders => " with the configured collector headers",
            Sent::WithoutHeaders { configured: true } => {
                " without the configured collector headers (the URL is not \
                 OTEL_EXPORTER_OTLP_ENDPOINT)"
            }
            Sent::WithoutHeaders { configured: false } => "",
        };
        return print(&format!(
            "sent {} spans of trace {} to {url}{note}\n",
            spans.len(),
            root.trace_id
        ));
    }
    if args.json {
        return print(&crate::json::text(&document));
    }
    print(&render(&spans))
}

/// The root branch's log, and with `subtree` the logs of every branch under
/// it, parents before children.
fn logs(
    yard: &Yard,
    root: &str,
    subtree: bool,
) -> Result<Vec<BranchLog>, crate::commands::Failure> {
    let all = yard.branches()?;
    let mut names = vec![root.to_owned()];
    // The branch must exist: an unknown name is the engine's usual error.
    let first = yard.branch(root)?;
    if subtree {
        let mut seen: BTreeSet<String> = names.iter().cloned().collect();
        let mut at = 0;
        while at < names.len() {
            let parent = names[at].clone();
            at += 1;
            for info in all.iter().filter(|i| i.parent.as_deref() == Some(&parent)) {
                if seen.insert(info.name.clone()) {
                    names.push(info.name.clone());
                }
            }
        }
    }
    let mut out = Vec::new();
    for name in names {
        let branch = if name == root {
            first.clone()
        } else {
            yard.branch(&name)?
        };
        let info = branch.info();
        out.push(BranchLog {
            name: name.clone(),
            parent: info
                .parent
                .clone()
                .filter(|p| out.iter().any(|l: &BranchLog| &l.name == p)),
            harness: info.harness.clone(),
            events: branch.events()?,
        });
    }
    Ok(out)
}

fn service_name() -> String {
    std::env::var("OTEL_SERVICE_NAME")
        .ok()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "by".to_owned())
}

fn attribute(key: &str, value: &Value) -> Json {
    let value = match value {
        Value::Str(text) => json!({ "stringValue": text }),
        // OTLP/JSON writes 64-bit integers as strings.
        Value::Int(n) => json!({ "intValue": n.to_string() }),
        Value::Bool(flag) => json!({ "boolValue": flag }),
    };
    json!({ "key": key, "value": value })
}

fn nanos(ms: u64) -> String {
    (u128::from(ms) * 1_000_000).to_string()
}

/// The spans as an OTLP/JSON `ExportTraceServiceRequest`.
pub fn otlp_json(spans: &[Span], service: &str) -> Json {
    let encoded: Vec<Json> = spans
        .iter()
        .map(|span| {
            let mut out = json!({
                "traceId": span.trace_id,
                "spanId": span.span_id,
                "name": span.name,
                // SPAN_KIND_INTERNAL = 1, SPAN_KIND_CLIENT = 3.
                "kind": if span.kind == "client" { 3 } else { 1 },
                "startTimeUnixNano": nanos(span.start_ms),
                "endTimeUnixNano": nanos(span.end_ms),
                "attributes": span.attributes.iter().map(|(k, v)| attribute(k, v)).collect::<Vec<_>>(),
            });
            if let Some(parent) = &span.parent_span_id {
                out["parentSpanId"] = json!(parent);
            }
            if let Some(error) = &span.error {
                // STATUS_CODE_ERROR = 2.
                out["status"] = json!({ "code": 2, "message": error });
            }
            out
        })
        .collect();
    json!({
        "resourceSpans": [{
            "resource": { "attributes": [
                attribute("service.name", &Value::Str(service.to_owned())),
                attribute("service.version", &Value::Str(env!("CARGO_PKG_VERSION").to_owned())),
                attribute("by.role", &Value::Str("cli".into())),
                attribute("by.trace.source", &Value::Str("event-log".into())),
            ]},
            "scopeSpans": [{
                "scope": { "name": "branchyard" },
                "spans": encoded,
            }],
        }],
    })
}

/// The document as JSON text. An encoding failure is an error: an empty
/// document is never written or sent.
fn encode(document: &Json) -> Result<String, crate::commands::Failure> {
    serde_json::to_string(document).map_err(|e| {
        Report::new("internal", format!("could not encode the trace: {e}"))
            .next("report this with the branch's event log")
            .into()
    })
}

/// The collector the environment configures: its endpoint
/// (`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, `OTEL_EXPORTER_OTLP_ENDPOINT`) and
/// the headers meant for it (`OTEL_EXPORTER_OTLP_TRACES_HEADERS`, else
/// `OTEL_EXPORTER_OTLP_HEADERS`: `key=value,key2=value2`, percent-decoded).
#[derive(Debug, Default)]
struct Collector {
    endpoints: Vec<String>,
    headers: Vec<(String, String)>,
}

impl Collector {
    fn from_env() -> Collector {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let raw = var("OTEL_EXPORTER_OTLP_TRACES_HEADERS")
            .or_else(|| var("OTEL_EXPORTER_OTLP_HEADERS"))
            .unwrap_or_default();
        Collector {
            endpoints: [
                "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                "OTEL_EXPORTER_OTLP_ENDPOINT",
            ]
            .iter()
            .filter_map(|name| var(name))
            .collect(),
            headers: raw
                .split(',')
                .filter_map(|pair| pair.split_once('='))
                .map(|(k, v)| (k.trim().to_owned(), http::decode(v.trim())))
                .filter(|(k, _)| !k.is_empty())
                .collect(),
        }
    }

    /// Whether `url` is the endpoint the environment configures, with or
    /// without the `/v1/traces` path and a trailing slash.
    fn is_configured(&self, url: &str) -> bool {
        let bare = |u: &str| {
            let u = u.trim().trim_end_matches('/');
            u.strip_suffix("/v1/traces").unwrap_or(u).to_owned()
        };
        let url = bare(url);
        self.endpoints.iter().any(|e| bare(e) == url)
    }
}

/// What `post` sent.
#[derive(Debug, PartialEq, Eq)]
enum Sent {
    /// The configured collector headers went with it.
    WithHeaders,
    /// None did; `configured` says headers were set but are for another URL.
    WithoutHeaders { configured: bool },
}

/// The headers to send to `endpoint` (`url` as the user gave it): the
/// configured ones only when `url` is the configured collector, never over
/// plain http to a host that is not this machine (they are credentials).
fn collector_headers(
    collector: &Collector,
    url: &str,
    endpoint: &http::Endpoint,
) -> Result<Vec<(String, String)>, Box<Report>> {
    if collector.headers.is_empty() || !collector.is_configured(url) {
        return Ok(Vec::new());
    }
    let host = endpoint.host.trim_matches(['[', ']']);
    let local = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if !endpoint.tls && !local {
        return Err(Box::new(
            Report::new(
                "denied",
                format!("not sending the collector headers to {url} over plain http"),
            )
            .line(
                "why",
                "OTEL_EXPORTER_OTLP_HEADERS is credentials, and http would send them in the clear",
            )
            .next("use an https:// URL, or unset OTEL_EXPORTER_OTLP_HEADERS and send without them"),
        ));
    }
    Ok(collector.headers.clone())
}

/// POST `body` to a collector's OTLP/HTTP traces endpoint: `url` as given
/// when its path ends `/v1/traces`, else with that appended. The
/// configured collector headers go only to the configured collector.
fn post(url: &str, body: &[u8], collector: &Collector) -> Result<Sent, crate::commands::Failure> {
    let failed = |why: String| {
        Report::new(
            "remote_unreachable",
            format!("could not send the trace to {url}: {why}"),
        )
        .next("check the URL, or write the trace with --otlp FILE and load it from there")
    };
    let full = match url.trim_end_matches('/').ends_with("/v1/traces") {
        true => url.trim_end_matches('/').to_owned(),
        false => format!("{}/v1/traces", url.trim_end_matches('/')),
    };
    let endpoint = http::Endpoint::parse(&full).map_err(failed)?;
    let tls = match endpoint.tls {
        true => Some(http::tls_config(None).map_err(failed)?),
        false => None,
    };
    let extra = collector_headers(collector, url, &endpoint).map_err(|report| *report)?;
    let mut headers = vec![("Content-Type", "application/json".to_owned())];
    headers.extend(extra.iter().map(|(k, v)| (k.as_str(), v.clone())));
    let target = match endpoint.prefix.is_empty() {
        true => "/".to_owned(),
        false => endpoint.prefix.clone(),
    };
    let stream = http::connect(&endpoint, tls.as_ref(), Duration::from_secs(30))
        .map_err(|e| failed(e.to_string()))?;
    let response = http::send(
        stream,
        &endpoint,
        &http::Request {
            method: "POST",
            target: &target,
            headers: &headers,
            body: Some(body),
        },
    )
    .map_err(|e| failed(e.to_string()))?;
    match response.status {
        200..=299 if extra.is_empty() => Ok(Sent::WithoutHeaders {
            configured: !collector.headers.is_empty(),
        }),
        200..=299 => Ok(Sent::WithHeaders),
        status => Err(failed(format!("the collector answered {status}")).into()),
    }
}

/// The spans as an indented tree, one line each.
fn render(spans: &[Span]) -> String {
    let mut out = String::new();
    if let Some(root) = spans.first() {
        out.push_str(&format!("trace {}\n", root.trace_id));
    }
    let roots: Vec<&Span> = spans
        .iter()
        .filter(|s| {
            s.parent_span_id
                .as_ref()
                .is_none_or(|p| !spans.iter().any(|o| &o.span_id == p))
        })
        .collect();
    for root in roots {
        line(&mut out, spans, root, 0);
    }
    out
}

fn line(out: &mut String, spans: &[Span], span: &Span, depth: usize) {
    let took = span.end_ms.saturating_sub(span.start_ms);
    let outcome = span
        .attributes
        .iter()
        .find_map(|(k, v)| match v {
            Value::Str(s) if k == "by.outcome" => Some(format!("  {s}")),
            _ => None,
        })
        .unwrap_or_default();
    let error = span
        .error
        .as_ref()
        .map(|e| format!("  error: {e}"))
        .unwrap_or_default();
    out.push_str(&format!(
        "{}{}  {}{outcome}{error}\n",
        "  ".repeat(depth),
        span.name,
        branchyard_support::time::human_duration(Duration::from_millis(took)),
    ));
    let mut children: Vec<&Span> = spans
        .iter()
        .filter(|s| s.parent_span_id.as_ref() == Some(&span.span_id))
        .collect();
    children.sort_by_key(|s| (s.start_ms, s.end_ms));
    for child in children {
        line(out, spans, child, depth + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(name: &str, id: &str, parent: Option<&str>, start: u64, end: u64) -> Span {
        Span {
            name: name.into(),
            trace_id: "0af7651916cd43dd8448eb211c80319c".into(),
            span_id: id.into(),
            parent_span_id: parent.map(str::to_owned),
            kind: "internal",
            start_ms: start,
            end_ms: end,
            attributes: vec![("by.outcome".into(), Value::Str("completed".into()))],
            error: None,
        }
    }

    #[test]
    fn the_document_is_otlp_json_with_hex_ids_and_nanosecond_strings() {
        let mut child = span(
            "turn",
            "00f067aa0ba902b7",
            Some("b7ad6b7169203331"),
            1_000,
            2_500,
        );
        child.error = Some("failed".into());
        child.attributes.push(("by.turn".into(), Value::Int(3)));
        child
            .attributes
            .push(("by.live".into(), Value::Bool(false)));
        let document = otlp_json(
            &[
                span("branch a", "b7ad6b7169203331", None, 1_000, 3_000),
                child,
            ],
            "by-test",
        );
        let spans = &document["resourceSpans"][0]["scopeSpans"][0]["spans"];
        assert_eq!(spans[0]["traceId"], "0af7651916cd43dd8448eb211c80319c");
        assert!(spans[0].get("parentSpanId").is_none());
        assert_eq!(spans[1]["parentSpanId"], "b7ad6b7169203331");
        assert_eq!(spans[1]["startTimeUnixNano"], "1000000000");
        assert_eq!(spans[1]["endTimeUnixNano"], "2500000000");
        assert_eq!(spans[1]["status"]["code"], 2);
        assert_eq!(spans[1]["attributes"][1]["value"]["intValue"], "3");
        assert_eq!(spans[1]["attributes"][2]["value"]["boolValue"], false);
        let resource = &document["resourceSpans"][0]["resource"]["attributes"];
        assert_eq!(resource[0]["value"]["stringValue"], "by-test");
    }

    fn collector(endpoint: &str) -> Collector {
        Collector {
            endpoints: vec![endpoint.to_owned()],
            headers: vec![("Authorization".into(), "Bearer secret-token".into())],
        }
    }

    #[test]
    fn collector_headers_go_only_to_the_configured_collector() {
        use branchyard_testkit::{MockHttp, Response};
        let configured = MockHttp::start(|_| Response::new(200, "{}"));
        let other = MockHttp::start(|_| Response::new(200, "{}"));
        let collector = collector(&configured.url());
        let body = b"{}";

        // The configured collector gets them, with the path appended or not.
        assert_eq!(
            post(&configured.url(), body, &collector).unwrap(),
            Sent::WithHeaders
        );
        assert_eq!(
            post(&format!("{}/v1/traces", configured.url()), body, &collector).unwrap(),
            Sent::WithHeaders
        );
        let seen = configured.await_requests(2);
        assert!(seen
            .iter()
            .all(|r| r.header("authorization") == Some("Bearer secret-token")));

        // Any other URL is sent the trace and none of the credentials.
        assert_eq!(
            post(&other.url(), body, &collector).unwrap(),
            Sent::WithoutHeaders { configured: true }
        );
        let seen = other.await_requests(1);
        assert!(
            seen.iter().all(|r| r.header("authorization").is_none()),
            "{seen:?}"
        );
        configured.finish();
        other.finish();
    }

    #[test]
    fn collector_headers_are_refused_over_plain_http_to_another_machine() {
        let collector = collector("http://collector.example.com:4318");
        let endpoint =
            http::Endpoint::parse("http://collector.example.com:4318/v1/traces").unwrap();
        let refused = collector_headers(&collector, "http://collector.example.com:4318", &endpoint)
            .unwrap_err();
        assert_eq!(refused.kind, "denied");
        assert!(refused.message.contains("plain http"), "{refused:?}");
        // Over https, or to this machine, they go; to a URL that is not the
        // configured one they never do, whatever the scheme.
        let https = http::Endpoint::parse("https://collector.example.com/v1/traces").unwrap();
        let secure = super::Collector {
            endpoints: vec!["https://collector.example.com".into()],
            ..collector_with_headers()
        };
        assert_eq!(
            collector_headers(&secure, "https://collector.example.com/v1/traces", &https)
                .unwrap()
                .len(),
            1
        );
        let elsewhere =
            collector_headers(&collector, "http://evil.example.com", &endpoint).unwrap();
        assert!(elsewhere.is_empty());
    }

    fn collector_with_headers() -> Collector {
        collector("")
    }

    #[test]
    fn a_trace_that_cannot_be_encoded_is_an_error_not_an_empty_document() {
        // A JSON value always encodes, so this pins the success path and the
        // error kind the failure maps to.
        let text = encode(&json!({"a": 1})).unwrap();
        assert_eq!(text, r#"{"a":1}"#);
    }

    #[test]
    fn the_tree_indents_children_under_their_parents_in_time_order() {
        let text = render(&[
            span("branch a", "aa", None, 0, 3_000),
            span("turn", "bb", Some("aa"), 100, 2_000),
            span("tool Bash", "cc", Some("bb"), 200, 900),
            span("turn", "dd", Some("aa"), 2_100, 2_900),
        ]);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("trace "), "{text}");
        assert!(lines[1].starts_with("branch a"), "{text}");
        assert!(lines[2].starts_with("  turn"), "{text}");
        assert!(lines[3].starts_with("    tool Bash"), "{text}");
        assert!(lines[4].starts_with("  turn"), "{text}");
    }
}
