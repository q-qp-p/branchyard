//! A turn is traced as it runs: the `turn` span, and under it the tool
//! calls, permission waits, steers and stalls, with the GenAI attributes;
//! a delegated child's turn joins the delegation that started it; and the
//! store, the sandbox provider and git add spans of their own. Against the
//! fake ACP agent, with spans captured in memory. Hermetic; no model.
//!
//! The capture is one subscriber for the whole binary, so each test names
//! its branches uniquely and reads only the trace it started.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests: a panic is the failure report
mod common;

use std::sync::Arc;
use std::time::Duration;

use branchyard::{
    BranchStatus, Budget, Envelope, Policy, Provider, SandboxKeep, SandboxOptions, Spawn,
    StallAction, TaskOptions,
};
use branchyard_runtime::LocalProvider;
use branchyard_sandbox::fake::FakeProvider;
use branchyard_telemetry::propagation::span_traceparent;
use branchyard_telemetry::testing::{capture, trace_id_of, CapturedSpan};
use branchyard_telemetry::TRACE_TARGET;
use branchyard_testkit::wait;
use common::{fake_agent, Fixture};

/// A root span for a test, and the options that put the engine's spans
/// under it.
fn rooted(f: &Fixture, name: &str) -> (tracing::Span, String, TaskOptions) {
    capture().expect("the span capture is installed first in this binary");
    let root = tracing::info_span!(target: TRACE_TARGET, "test root", "by.test" = name);
    let traceparent = span_traceparent(&root).expect("a traced root span");
    let options = TaskOptions {
        policy: Policy::allow_all(),
        trace_parent: Some(traceparent.clone()),
        ..f.options()
    };
    (root, traceparent, options)
}

fn trace(traceparent: &str) -> Vec<CapturedSpan> {
    capture()
        .unwrap()
        .trace(&trace_id_of(traceparent).expect("a trace id"))
}

fn named<'a>(spans: &'a [CapturedSpan], name: &str) -> Vec<&'a CapturedSpan> {
    spans.iter().filter(|s| s.name == name).collect()
}

fn attr<'a>(span: &'a CapturedSpan, key: &str) -> &'a str {
    span.attributes
        .get(key)
        .map_or("", std::string::String::as_str)
}

#[test]
fn a_turn_is_a_live_span_with_genai_attributes_and_its_tool_calls_beneath() {
    let f = Fixture::new();
    let (root, traceparent, options) = rooted(&f, "live-turn");
    let branch = f
        .yard
        .task("SH echo hi --token=hunter2secret")
        .options(options)
        .name("live-turn")
        .run()
        .unwrap();
    assert_eq!(branch.info().status, BranchStatus::NoChanges);
    let record = branch;
    drop(root);

    let spans = trace(&traceparent);
    let turns = named(&spans, "turn");
    assert_eq!(turns.len(), 1, "{spans:#?}");
    let turn = turns[0];
    let root_span = named(&spans, "test root")[0];
    assert_eq!(turn.parent_id.as_deref(), Some(root_span.span_id.as_str()));
    assert_eq!(attr(turn, "by.live"), "true");
    assert_eq!(attr(turn, "gen_ai.operation.name"), "invoke_agent");
    assert_eq!(attr(turn, "gen_ai.agent.name"), record.info().harness);
    assert_eq!(attr(turn, "by.harness"), record.info().harness);
    assert_eq!(attr(turn, "by.branch"), "live-turn");
    assert_eq!(attr(turn, "by.outcome"), "completed");
    assert_eq!(turn.error, None);
    assert!(turn.end_ms >= turn.start_ms);

    // The shell call is a child of the turn, named for its kind, with the
    // scrubbed target and nothing of its input.
    let tools: Vec<_> = spans
        .iter()
        .filter(|s| s.name.starts_with("tool "))
        .collect();
    assert_eq!(tools.len(), 1, "{spans:#?}");
    let tool = tools[0];
    assert_eq!(tool.parent_id.as_deref(), Some(turn.span_id.as_str()));
    assert_eq!(attr(tool, "gen_ai.operation.name"), "execute_tool");
    assert_eq!(attr(tool, "by.tool.target.kind"), "command");
    assert!(attr(tool, "by.tool.target").contains("echo hi"), "{tool:?}");
    assert!(
        attr(tool, "by.tool.target").contains("[redacted]"),
        "{tool:?}"
    );
    assert!(tool.end_ms >= tool.start_ms);
    for span in &spans {
        for (key, value) in &span.attributes {
            assert!(
                !value.contains("hunter2secret"),
                "{} carries a secret in {key}",
                span.name
            );
        }
    }
}

#[test]
fn a_permission_wait_is_a_span_under_the_turn_with_its_answer() {
    let f = Fixture::new();
    let (root, traceparent, options) = rooted(&f, "live-perm");
    f.yard
        .task("PERMISSION WRITE p.txt=1")
        .options(options)
        .name("live-perm")
        .run()
        .unwrap();
    drop(root);
    let spans = trace(&traceparent);
    let turn = named(&spans, "turn")[0];
    let waits = named(&spans, "permission wait");
    assert_eq!(waits.len(), 1, "{spans:#?}");
    assert_eq!(waits[0].parent_id.as_deref(), Some(turn.span_id.as_str()));
    assert_eq!(attr(waits[0], "by.allowed"), "true");
    assert!(!attr(waits[0], "by.source").is_empty());
}

#[test]
fn a_stalled_turn_has_a_stall_span_and_an_error_status() {
    let f = Fixture::new();
    let (root, traceparent, options) = rooted(&f, "live-stall");
    let branch = f
        .yard
        .task("HANG WRITE partial.txt=1")
        .options(options)
        .name("live-stall")
        .budget(
            Budget::default()
                .stall_after(Duration::from_millis(150))
                .stall_action(StallAction::Interrupt),
        )
        .run()
        .unwrap();
    assert_eq!(branch.info().status, BranchStatus::Interrupted);
    drop(root);
    let spans = trace(&traceparent);
    let turn = named(&spans, "turn")[0];
    assert_eq!(attr(turn, "by.outcome"), "interrupted");
    assert_eq!(attr(turn, "error.type"), "interrupted");
    assert!(turn.error.is_some(), "{turn:?}");
    let stalls = named(&spans, "stall");
    assert_eq!(stalls.len(), 1, "{spans:#?}");
    assert_eq!(stalls[0].parent_id.as_deref(), Some(turn.span_id.as_str()));
}

#[test]
fn a_steer_is_a_span_from_its_write_to_the_harness_taking_it() {
    let f = Fixture::new();
    let (root, traceparent, options) = rooted(&f, "live-steer");
    let task = f
        .yard
        .task("AWAIT_STEER")
        .options(options)
        .name("live-steer");
    let turn = std::thread::spawn(move || task.run());
    wait::until("the turn to wait for steering", || {
        f.yard
            .branch("live-steer")
            .is_ok_and(|b| b.info().status == BranchStatus::Running)
    });
    f.yard.branch("live-steer").unwrap().steer("go on").unwrap();
    turn.join().unwrap().unwrap();
    drop(root);
    let spans = trace(&traceparent);
    let turn = named(&spans, "turn")[0];
    let steers = named(&spans, "steer");
    assert_eq!(steers.len(), 1, "{spans:#?}");
    assert_eq!(steers[0].parent_id.as_deref(), Some(turn.span_id.as_str()));
    assert_eq!(attr(steers[0], "by.branch"), "live-steer");
    assert!(
        !steers[0].attributes.values().any(|v| v.contains("go on")),
        "a steer's text is not an attribute: {:?}",
        steers[0]
    );
}

#[test]
fn a_childs_turn_joins_the_delegation_that_started_it() {
    let f = Fixture::new();
    let (root_span, traceparent, options) = rooted(&f, "live-delegation");
    let options = TaskOptions {
        delegation: Some(Envelope::default()),
        delegation_server: Some(vec![fake_agent().display().to_string()]),
        ..options
    };
    let lead = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("live-lead")
        .run()
        .unwrap();
    let delegate = lead.delegate(options).unwrap();
    delegate
        .spawn(Spawn {
            prompt: "say kid".into(),
            name: Some("live-kid".into()),
            ..Spawn::default()
        })
        .unwrap();
    lead.wait_subtree().unwrap();
    drop(root_span);

    let spans = trace(&traceparent);
    let delegations = named(&spans, "delegation spawn");
    assert_eq!(delegations.len(), 1, "{spans:#?}");
    let delegation = delegations[0];
    assert_eq!(attr(delegation, "by.child"), "live-kid");
    assert_eq!(attr(delegation, "by.refused"), "false");
    let kid_turns: Vec<_> = spans
        .iter()
        .filter(|s| s.name == "turn" && attr(s, "by.branch") == "live-kid")
        .collect();
    assert_eq!(kid_turns.len(), 1, "{spans:#?}");
    assert_eq!(
        kid_turns[0].parent_id.as_deref(),
        Some(delegation.span_id.as_str()),
        "the child's turn is under the delegation, in the same trace"
    );
}

#[test]
fn a_sandbox_provider_action_is_a_span_per_action_with_the_provider() {
    let f = Fixture::new();
    let fake = Arc::new(FakeProvider::live(
        Box::new(LocalProvider::new()),
        f.dir.join("fake-provider"),
    ));
    f.yard.use_sandbox_provider(fake);
    let (root, traceparent, options) = rooted(&f, "live-sandbox");
    let options = TaskOptions {
        provider: Some(Provider::Microsandbox(SandboxOptions {
            image: "registry.example/harness:1".into(),
            keep: SandboxKeep::Pause,
            ..SandboxOptions::default()
        })),
        ..options
    };
    // Entered, so the provider's spans (which have no parent of their own
    // to adopt) are in the root's trace.
    let entered = root.enter();
    f.yard
        .task("say hi")
        .options(options)
        .name("live-sandbox")
        .run()
        .unwrap();
    drop(entered);
    drop(root);
    let spans = trace(&traceparent);
    let actions: Vec<&CapturedSpan> = spans
        .iter()
        .filter(|s| s.name.starts_with("sandbox "))
        .collect();
    let seen: Vec<&str> = actions.iter().map(|s| attr(s, "by.action")).collect();
    for expected in ["ensure", "exec"] {
        assert!(seen.contains(&expected), "{expected} in {seen:?}");
    }
    assert!(actions
        .iter()
        .all(|s| attr(s, "by.provider") == "microsandbox"));
    // A kept sandbox is paused, not destroyed, at the turn's end.
    assert!(seen.contains(&"pause"), "{seen:?}");
}

#[test]
fn hot_git_operations_are_spans_that_name_the_operation_and_nothing_else() {
    let f = Fixture::new();
    let (root, traceparent, options) = rooted(&f, "live-git");
    let entered = root.enter();
    f.yard
        .task("WRITE g.txt=1")
        .options(options)
        .name("live-git")
        .run()
        .unwrap();
    drop(entered);
    drop(root);
    let spans = trace(&traceparent);
    let git: Vec<&CapturedSpan> = spans
        .iter()
        .filter(|s| s.name.starts_with("git "))
        .collect();
    let operations: Vec<&str> = git.iter().map(|s| attr(s, "by.git.operation")).collect();
    assert!(operations.contains(&"worktree_add"), "{operations:?}");
    assert!(operations.contains(&"commit"), "{operations:?}");
    for span in &git {
        assert_eq!(span.attributes.len(), 1, "only the operation: {span:?}");
    }
}

#[test]
fn store_queries_are_spans_only_when_asked_and_only_inside_a_trace() {
    let f = Fixture::new();
    let (root, traceparent, options) = rooted(&f, "live-store");
    branchyard_support::trace::set_store_spans(true);
    let entered = root.enter();
    f.yard
        .task("say hi")
        .options(options)
        .name("live-store")
        .run()
        .unwrap();
    drop(entered);
    drop(root);
    // A query on a thread with no span in scope would start a trace of its
    // own for every call: it makes none.
    f.yard.branch("live-store").unwrap().events().unwrap();
    branchyard_support::trace::set_store_spans(false);

    let spans = trace(&traceparent);
    let queries: Vec<&CapturedSpan> = spans
        .iter()
        .filter(|s| s.name.starts_with("store "))
        .collect();
    assert!(!queries.is_empty(), "{spans:#?}");
    for query in &queries {
        assert_eq!(attr(query, "by.store.backend"), "sqlite");
        assert_eq!(
            query.name,
            format!("store {}", attr(query, "by.store.method"))
        );
    }
    let methods: Vec<&str> = queries.iter().map(|s| attr(s, "by.store.method")).collect();
    assert!(methods.contains(&"append"), "{methods:?}");
    let orphans: Vec<_> = capture()
        .unwrap()
        .spans()
        .into_iter()
        .filter(|s| s.name.starts_with("store ") && s.parent_id.is_none())
        .collect();
    assert!(orphans.is_empty(), "{orphans:#?}");
}

#[test]
fn the_messaging_observe_stage_is_a_span_in_the_senders_trace() {
    let f = Fixture::new();
    let (root_span, traceparent, options) = rooted(&f, "live-message");
    let options = TaskOptions {
        delegation: Some(Envelope::depth(1)),
        delegation_server: Some(vec![fake_agent().display().to_string()]),
        observer: Some(Arc::new(|_| {})),
        ..options
    };
    let lead = f
        .yard
        .task("WRITE lead.txt=1")
        .options(options.clone())
        .name("live-msg-lead")
        .run()
        .unwrap();
    let lead_delegate = lead.delegate(options.clone()).unwrap();
    for name in ["live-msg-a", "live-msg-b"] {
        lead_delegate
            .spawn(Spawn {
                prompt: format!("WRITE {name}.txt=1"),
                name: Some(name.into()),
                ..Spawn::default()
            })
            .unwrap();
        lead_delegate.wait(name, Duration::from_secs(120)).unwrap();
    }
    let sender = f
        .yard
        .branch("live-msg-a")
        .unwrap()
        .delegate(options)
        .unwrap();
    let sent = sender
        .message("live-msg-b", "hello", None, None, None, None)
        .unwrap();
    drop(root_span);

    let spans = trace(&traceparent);
    let observed: Vec<&CapturedSpan> = spans
        .iter()
        .filter(|s| s.name == "message observe" && attr(s, "by.message") == sent.id.to_string())
        .collect();
    assert_eq!(observed.len(), 1, "{spans:#?}");
    let root = named(&spans, "test root")[0];
    assert_eq!(
        observed[0].parent_id.as_deref(),
        Some(root.span_id.as_str()),
        "the stage is a child of the span the message carried"
    );
    assert_eq!(attr(observed[0], "by.from"), "live-msg-a");
    assert_eq!(attr(observed[0], "by.to"), "live-msg-b");
    assert!(
        !observed[0].attributes.values().any(|v| v == "hello"),
        "the message's text is not an attribute"
    );
}

#[test]
fn a_connector_token_request_is_a_span_with_the_refusal_code_and_no_secret() {
    use branchyard::connectors::{Proof, TokenRequest};
    let f = Fixture::new();
    let (root, traceparent, _) = rooted(&f, "live-token");
    let entered = root.enter();
    // This repository has no gateway: the request is refused as
    // unavailable, and the span says so by code.
    let refused = f
        .yard
        .issue_turn_token(
            "no-such-branch",
            &TokenRequest {
                manifest: "sha256:abc".into(),
                branch: "app/no-such-branch".into(),
                turn: 1,
            },
            Proof {
                secret: "turn-secret-value",
                identity: None,
            },
        )
        .unwrap_err();
    drop(entered);
    drop(root);
    let spans = trace(&traceparent);
    let tokens = named(&spans, "connector token");
    assert_eq!(tokens.len(), 1, "{spans:#?}");
    assert_eq!(attr(tokens[0], "by.refusal"), refused.kind.code());
    assert_eq!(attr(tokens[0], "by.branch"), "no-such-branch");
    assert!(tokens[0].error.is_some());
    for (key, value) in &tokens[0].attributes {
        assert!(
            !value.contains("turn-secret-value") && !value.contains("sha256:abc"),
            "{key} carries the request"
        );
    }
}
