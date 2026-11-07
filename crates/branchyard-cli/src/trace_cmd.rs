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
        let text = serde_json::to_string(&document).unwrap_or_default();
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
        post(url, &document)?;
        return print(&format!(
            "sent {} spans of trace {} to {url}\n",
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

/// The `Name: value` headers of `OTEL_EXPORTER_OTLP_TRACES_HEADERS` (else
/// `OTEL_EXPORTER_OTLP_HEADERS`): `key=value,key2=value2`, percent-decoded.
fn configured_headers() -> Vec<(String, String)> {
    let raw = std::env::var("OTEL_EXPORTER_OTLP_TRACES_HEADERS")
        .or_else(|_| std::env::var("OTEL_EXPORTER_OTLP_HEADERS"))
        .unwrap_or_default();
    raw.split(',')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), http::decode(v.trim())))
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

/// POST `document` to a collector's OTLP/HTTP traces endpoint: `url` as given
/// when its path ends `/v1/traces`, else with that appended.
fn post(url: &str, document: &Json) -> Result<(), crate::commands::Failure> {
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
    let body = serde_json::to_vec(document).unwrap_or_default();
    let mut headers = vec![("Content-Type", "application/json".to_owned())];
    let extra = configured_headers();
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
            body: Some(&body),
        },
    )
    .map_err(|e| failed(e.to_string()))?;
    match response.status {
        200..=299 => Ok(()),
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
