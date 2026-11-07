//! Priority, metrics and traces over real HTTP with the fake ACP agent:
//! see `docs/server.md#scheduling` and `docs/observability.md`.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use branchyard_client::api::{OperationState, PolicySpec, SpawnRequest, TaskRequest};
use branchyard_client::new_key;
use branchyard_server::config::{MetricsConfig, Principal, TenantPolicy, Token};
use branchyard_telemetry::testing::capture;
use common::{await_operation, get, post, raw, run, task, Fixture, Server, TOKEN};

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body:?}"))
}

const METRICS_TOKEN: &str = "metrics-token-0123456789";
const READER_TOKEN: &str = "reader-token-0123456789";

/// A request's priority is checked (-10 to 10), capped at its tenant's
/// `max_priority`, reported on the operation, and inherited by a spawned
/// child that names none.
#[test]
fn priority_is_checked_capped_reported_and_inherited() {
    let f = Fixture::new();
    let mut config = f.config();
    config.allow_delegation = true;
    config.by_path = Some("/bin/true".into());
    config.tenants.insert(
        "default".into(),
        TenantPolicy {
            max_priority: Some(6),
            ..TenantPolicy::default()
        },
    );
    let server = Server::start(config);
    let client = server.client();
    let repo = client.repo("app");

    let refused = repo
        .submit_task(
            &TaskRequest {
                priority: Some(11),
                ..task("say hi", "too-high")
            },
            &new_key(),
        )
        .unwrap_err();
    assert!(refused.to_string().contains("priority 11"), "{refused}");

    let capped = run(
        &client,
        &TaskRequest {
            priority: Some(9),
            delegation: Some(branchyard::Envelope::depth(2)),
            ..task("say hi", "parent")
        },
    );
    assert_eq!(capped.state, OperationState::Succeeded, "{capped:?}");
    assert_eq!(capped.priority, 6, "capped at the tenant's max_priority");
    let low = run(
        &client,
        &TaskRequest {
            priority: Some(-4),
            ..task("say hi", "low")
        },
    );
    assert_eq!(low.priority, -4);
    // The wire form leaves out priority 0.
    let plain = run(&client, &task("say hi", "plain"));
    assert_eq!(plain.priority, 0);

    let child = |name: &str, priority: Option<i32>| SpawnRequest {
        prompt: "say hi".into(),
        name: Some(name.into()),
        policy: PolicySpec::allow_all(),
        priority,
        ..SpawnRequest::default()
    };
    let inherited = repo
        .spawn("parent", &child("inherits", None), &new_key())
        .unwrap();
    assert_eq!(inherited.priority, 6, "a child inherits its parent's");
    assert_eq!(
        await_operation(&client, &inherited.id).state,
        OperationState::Succeeded
    );
    let own = repo
        .spawn("parent", &child("own", Some(-2)), &new_key())
        .unwrap();
    assert_eq!(own.priority, -2, "unless it names its own");
    assert_eq!(
        await_operation(&client, &own.id).state,
        OperationState::Succeeded
    );
    server.stop();
}

fn metrics_config(f: &Fixture, listen: bool) -> branchyard_server::Config {
    let mut config = f.config();
    config.tokens.push(Token {
        name: "reader".into(),
        secret: READER_TOKEN.into(),
    });
    config.principals.insert(
        "reader".into(),
        Principal {
            origin: Default::default(),
            name: "reader".into(),
            tenant: "default".into(),
            scopes: ["read", "run"].into_iter().map(String::from).collect(),
            repos: None,
            groups: Default::default(),
        },
    );
    config.metrics = Some(MetricsConfig {
        listen: listen.then(|| "127.0.0.1:0".parse().unwrap()),
        token_sha256: Some(branchyard_server::config::sha256_hex(
            METRICS_TOKEN.as_bytes(),
        )),
    });
    config
}

/// The value of the sample `name{labels}` in `text`, labels in the order
/// written.
fn sample(text: &str, name_and_labels: &str) -> Option<f64> {
    text.lines()
        .find_map(|line| line.strip_prefix(&format!("{name_and_labels} ")))
        .map(|v| v.parse().unwrap())
}

/// `/metrics` is off by default; when on, it needs the `admin` scope or the
/// metrics token, which reads nothing else; it reports the queue, claims,
/// finished operations and turns, also on a listener of its own.
#[test]
fn metrics_need_an_operator_or_the_metrics_token_and_count_the_work() {
    let f = Fixture::new();
    let server = Server::start(f.config());
    let (status, _, _) = raw(server.addr, &get("/metrics", Some(TOKEN)));
    assert_eq!(status, 404, "off by default");
    server.stop();

    let f = Fixture::new();
    let server = Server::start(metrics_config(&f, true));
    let client = server.client();
    let op = run(&client, &task("WRITE a.txt=1", "one"));
    assert_eq!(op.state, OperationState::Succeeded, "{op:?}");

    let (status, _, _) = raw(server.addr, &get("/metrics", None));
    assert_eq!(status, 401);
    let (status, _, body) = raw(server.addr, &get("/metrics", Some(READER_TOKEN)));
    assert_eq!(status, 403, "{body}");
    assert_eq!(json(&body)["error"]["detail"]["scope"], "admin");
    // The metrics token reads metrics, and nothing else.
    let (status, _, _) = raw(server.addr, &get("/v1/repos", Some(METRICS_TOKEN)));
    assert_eq!(status, 401);
    let (status, head, by_token) = raw(server.addr, &get("/metrics", Some(METRICS_TOKEN)));
    assert_eq!(status, 200, "{by_token}");
    assert!(
        head.to_lowercase().contains("text/plain; version=0.0.4"),
        "{head}"
    );
    let (status, _, by_admin) = raw(server.addr, &get("/metrics", Some(TOKEN)));
    assert_eq!(status, 200);
    for text in [&by_token, &by_admin] {
        assert_eq!(
            sample(
                text,
                r#"branchyard_operations_admitted_total{kind="task",tenant="default"}"#
            ),
            Some(1.0),
            "{text}"
        );
        assert_eq!(
            sample(
                text,
                r#"branchyard_operations_finished_total{kind="task",state="succeeded"}"#
            ),
            Some(1.0)
        );
        assert_eq!(
            sample(
                text,
                r#"branchyard_claims_total{tenant="default",priority="0"}"#
            ),
            Some(1.0)
        );
        assert_eq!(
            sample(
                text,
                r#"branchyard_turns_started_total{harness="gemini-cli"}"#
            ),
            Some(1.0)
        );
        assert_eq!(
            sample(
                text,
                r#"branchyard_turns_ended_total{harness="gemini-cli",outcome="completed"}"#
            ),
            Some(1.0)
        );
        assert_eq!(
            sample(
                text,
                r#"branchyard_turn_duration_seconds_count{harness="gemini-cli"}"#
            ),
            Some(1.0)
        );
        assert_eq!(
            sample(text, r#"branchyard_operations{state="queued"}"#),
            Some(0.0)
        );
        assert_eq!(sample(text, "branchyard_workers_live"), Some(1.0));
    }

    // The separate listener: /metrics only, with the metrics token.
    let url = server.running.as_ref().unwrap().metrics_url().unwrap();
    let addr: std::net::SocketAddr = url
        .trim_start_matches("http://")
        .trim_end_matches("/metrics")
        .parse()
        .unwrap();
    let (status, _, _) = raw(addr, &get("/metrics", None));
    assert_eq!(status, 401);
    let (status, _, _) = raw(addr, &get("/metrics", Some(TOKEN)));
    assert_eq!(status, 401, "the API's tokens are not the metrics token");
    let (status, _, text) = raw(addr, &get("/metrics", Some(METRICS_TOKEN)));
    assert_eq!(status, 200);
    assert!(text.contains("# TYPE branchyard_claims_total counter"));
    let (status, _, _) = raw(addr, &get("/v1/repos", Some(METRICS_TOKEN)));
    assert_eq!(status, 404);
    server.stop();
}

/// An operation is traced from its admission (continuing the request's
/// `traceparent`) through its claim and run to each turn, and the turn's
/// harness gets the operation's span as `TRACEPARENT`.
#[test]
fn an_operation_is_traced_from_admission_to_its_turns() {
    let capture = capture().unwrap();
    let f = Fixture::new();
    let server = Server::start(f.config());
    let client = server.client();
    let trace = new_key();
    let incoming = format!("00-{trace}-00f067aa0ba902b7-01");
    let body = serde_json::to_string(&TaskRequest {
        priority: Some(3),
        ..task("ENV TRACEPARENT", "traced")
    })
    .unwrap();
    let (status, _, answer) = raw(
        server.addr,
        &post(
            "/v1/repos/app/tasks",
            Some(TOKEN),
            &format!("traceparent: {incoming}\r\n"),
            &body,
        ),
    );
    assert_eq!(status, 202, "{answer}");
    let id = json(&answer)["id"].as_str().unwrap().to_owned();
    assert_eq!(
        await_operation(&client, &id).state,
        OperationState::Succeeded
    );
    let spans = capture.trace(&trace);
    let named = |name: &str| {
        spans
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no {name} span in {spans:#?}"))
    };
    let admission = named("admission");
    assert_eq!(admission.parent_id.as_deref(), Some("00f067aa0ba902b7"));
    assert_eq!(admission.kind, "server");
    assert_eq!(admission.attributes["by.priority"], "3");
    assert_eq!(admission.attributes["by.operation"], id);
    assert_eq!(admission.attributes["by.kind"], "task");
    let claim = named("claim");
    assert_eq!(claim.parent_id.as_ref(), Some(&admission.span_id));
    assert_eq!(claim.attributes["by.operation"], id);
    let operation = named("operation task");
    assert_eq!(operation.parent_id.as_ref(), Some(&admission.span_id));
    assert_eq!(operation.attributes["by.repo"], "app");
    assert_eq!(operation.attributes["by.branches"], "traced");
    let turn = named("turn");
    assert_eq!(turn.parent_id.as_ref(), Some(&operation.span_id));
    assert_eq!(turn.attributes["by.outcome"], "completed");
    assert!(turn.start_ms <= turn.end_ms);
    // The harness saw the operation's span as its parent.
    let said: String = client
        .repo("app")
        .events("traced", 0)
        .unwrap()
        .events
        .iter()
        .filter_map(|e| match &e.activity {
            branchyard::Activity::Harness(branchyard::Event::MessageDelta { text, .. }) => {
                Some(text.clone())
            }
            _ => None,
        })
        .collect();
    assert!(
        said.contains(&format!("TRACEPARENT=00-{trace}-{}-01", operation.span_id)),
        "{said}"
    );
    // A malformed traceparent starts a trace of its own.
    let (status, _, answer) = raw(
        server.addr,
        &post(
            "/v1/repos/app/tasks",
            Some(TOKEN),
            "traceparent: nonsense\r\n",
            &serde_json::to_string(&task("say hi", "fresh")).unwrap(),
        ),
    );
    assert_eq!(status, 202, "{answer}");
    let fresh_id = json(&answer)["id"].as_str().unwrap().to_owned();
    assert_eq!(
        await_operation(&client, &fresh_id).state,
        OperationState::Succeeded
    );
    let fresh = capture
        .spans()
        .into_iter()
        .find(|s| s.name == "admission" && s.attributes.get("by.operation") == Some(&fresh_id))
        .unwrap();
    assert_eq!(fresh.parent_id, None);
    assert_ne!(fresh.trace_id, trace);
    server.stop();
}

/// The golden metrics the OpenTelemetry rebuild adds, over real HTTP: the
/// request rate by route template, refused requests by error code, an
/// operation's duration, and the turn as a GenAI client operation. Label
/// sets stay bounded: a route is a template, never a URL with an id.
#[test]
fn the_golden_metrics_count_requests_errors_operations_and_turns() {
    let f = Fixture::new();
    let server = Server::start(metrics_config(&f, false));
    let client = server.client();
    let op = run(&client, &task("WRITE a.txt=1", "golden"));
    assert_eq!(op.state, OperationState::Succeeded, "{op:?}");
    // A refused request: an unknown branch, by its id in the URL.
    let (status, _, _) = raw(
        server.addr,
        &get("/v1/repos/app/branches/no-such-branch-91", Some(TOKEN)),
    );
    assert_eq!(status, 404);
    let (status, _, _) = raw(server.addr, &get("/no/such/route/77", Some(TOKEN)));
    assert_eq!(status, 404);
    let (status, _, text) = raw(server.addr, &get("/metrics", Some(TOKEN)));
    assert_eq!(status, 200, "{text}");
    // Requests, by the route's template.
    assert!(
        text.lines().any(|l| l.starts_with(
            "http_server_request_duration_seconds_count{http_route=\"/v1/repos/{repo}/tasks\",http_request_method=\"POST\",status_class=\"2xx\"}"
        )),
        "{text}"
    );
    assert!(
        !text.contains("no-such-branch-91"),
        "an id became a label:\n{text}"
    );
    assert!(
        !text.contains("/no/such/route/77"),
        "a URL became a label:\n{text}"
    );
    assert!(
        text.contains("http_route=\"unmatched\""),
        "a request no route took is `unmatched`:\n{text}"
    );
    assert!(sample(
        &text,
        "http_server_active_requests{http_request_method=\"GET\"}"
    )
    .is_some());
    // Errors, by code.
    assert!(
        sample(
            &text,
            "branchyard_api_errors_total{code=\"unknown_branch\"}"
        )
        .is_some_and(|n| n >= 1.0),
        "{text}"
    );
    assert!(
        sample(&text, "branchyard_api_errors_total{code=\"not_found\"}").is_some_and(|n| n >= 1.0),
        "{text}"
    );
    // The operation's duration, and its turn as a GenAI client operation.
    assert_eq!(
        sample(
            &text,
            r#"branchyard_operation_duration_seconds_count{kind="task",state="succeeded"}"#
        ),
        Some(1.0),
        "{text}"
    );
    assert_eq!(
        sample(
            &text,
            r#"gen_ai_client_operation_duration_seconds_count{gen_ai_operation_name="invoke_agent",gen_ai_agent_name="gemini-cli",by_turn_outcome="completed"}"#
        ),
        Some(1.0),
        "{text}"
    );
    // Every series of the golden families has a bounded label set: no
    // value looks like an id.
    for line in text
        .lines()
        .filter(|l| l.starts_with("http_server_") || l.starts_with("branchyard_api_errors_total"))
    {
        assert!(!line.contains("op_"), "{line}");
        assert!(!line.contains("req_"), "{line}");
    }
    server.stop();
}
