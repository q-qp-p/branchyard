//! Approvals and the effect ledger on a server, over real HTTP, against a
//! mock gateway on loopback (`mock_gateway`): the administrator's locked
//! policy holds over a principal's own, an ask is answered through the API
//! as the caller, the ledger and an undo plan are read and an undo is
//! performed through the API, and `/metrics` counts effects and waiting
//! approvals. A shell script stands in for Anvil's packaging. The ground
//! plan reads the open branches' diffs, and the decisions follow each
//! branch's stage, a root's question to the person among them.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;
#[path = "../../branchyard/tests/mock_gateway/mod.rs"]
mod mock_gateway;

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use branchyard::connectors::GrantEntry;
use branchyard::effects::{Approval, ApprovalPolicy, ApprovalSettings, EffectState, Layer};
use branchyard::stage::Lane;
use branchyard::{BranchStatus, Envelope, Provisioning};
use branchyard_client::api::{
    MergeRequest, OperationState, PolicySpec, ProblemKind, SendRequest, SpawnRequest, TaskRequest,
};
use branchyard_client::effects_api::{ApprovalAnswerRequest, DecisionKind, UndoRequest, Waits};
use branchyard_client::new_key;
use branchyard_server::config::{ConnectorsConfig, MetricsConfig, Principal, Token};
use branchyard_testkit::wait;
use common::{await_operation, get, raw, run, task, Fixture, Server};
use mock_gateway::MockGateway;

const FAKE_ANVIL: &str = r##"#!/bin/sh
case "$1 $2" in
"package harness") mkdir -p "$5" && echo "# $(basename "$3")" > "$5/SKILL.md" ;;
"connectors index") echo "# Connectors" > "$6" ;;
*) echo "fake anvil: $*" >&2; exit 2 ;;
esac
"##;

const METRICS_TOKEN: &str = "metrics-token-0123456789";

fn policy(rules: &[(&str, Approval)]) -> ApprovalPolicy {
    ApprovalPolicy {
        rules: rules.iter().map(|(p, a)| ((*p).to_owned(), *a)).collect(),
        ..ApprovalPolicy::default()
    }
}

fn granted(f: &Fixture, prompt: &str, name: &str) -> TaskRequest {
    let script = f.dir.join("call.py");
    fs::write(&script, mock_gateway::CALL_PY).unwrap();
    TaskRequest {
        isolated: true,
        provision: Some(Provisioning {
            connectors: ["slack:write", "github:write"]
                .iter()
                .map(|g| GrantEntry::parse(g).unwrap())
                .collect(),
            ..Provisioning::default()
        }),
        ..task(
            &prompt.replace("call.py", &script.display().to_string()),
            name,
        )
    }
}

/// Connectors through `mock`, with Slack and GitHub bundles packaged by
/// the fake Anvil.
fn connectors(f: &Fixture, mock: &MockGateway) -> ConnectorsConfig {
    let bundles = f.dir.join("bundles");
    for id in ["slack", "github"] {
        fs::create_dir_all(bundles.join(id)).unwrap();
        fs::write(
            bundles.join(id).join("air.yaml"),
            format!("service: {id}\n"),
        )
        .unwrap();
    }
    let anvil = f.dir.join("fake-anvil");
    fs::write(&anvil, FAKE_ANVIL).unwrap();
    fs::set_permissions(&anvil, fs::Permissions::from_mode(0o755)).unwrap();
    ConnectorsConfig {
        gateway: mock.url.clone(),
        sandbox_gateway: None,
        issuer: Some("https://by.example".into()),
        signing_key: None,
        audit_file: None,
        bundles,
        anvil: vec![anvil.display().to_string()],
        run_gateway: false,
        listen: None,
        vault_key: None,
        effects_proxy: None,
        effects_listen: None,
        effects_sandbox_host: None,
    }
}

#[test]
fn a_locked_policy_holds_an_ask_is_answered_through_the_api_and_undo_runs_there() {
    let f = Fixture::new();
    let mock = MockGateway::start();
    let mut config = f.config();
    config.connectors = Some(connectors(&f, &mock));
    // The administrator asks before any GitHub call and blocks Slack
    // deletions; the principal would allow both.
    config.approvals = ApprovalSettings {
        admin: Some(policy(&[
            ("github:*", Approval::Ask),
            ("slack:chat_delete", Approval::Block),
        ])),
        person: None,
        people: [(
            "tester".to_owned(),
            policy(&[("github:*", Approval::Allow), ("slack:*", Approval::Allow)]),
        )]
        .into(),
    };
    config.metrics = Some(MetricsConfig {
        listen: None,
        token_sha256: Some(branchyard_server::config::sha256_hex(
            METRICS_TOKEN.as_bytes(),
        )),
    });
    let server = Server::start(config);
    let client = server.client();
    let repo = client.repo("app");

    let op = repo
        .submit_task(
            &granted(
                &f,
                "SH python3 call.py github__issues_create '{\"title\": \"t\"}'\n\
                 SH python3 call.py slack__chat_delete '{\"ts\": \"1\"}'",
                "asker",
            ),
            "k-asker",
        )
        .unwrap();
    // The ask waits; the turn with it.
    let mut waiting = Vec::new();
    wait::until("an ask", || {
        waiting = repo.approvals(false).unwrap();
        !waiting.is_empty()
    });
    let ask = &waiting[0];
    assert_eq!(ask.branch, "asker");
    assert_eq!(ask.subject, "tester");
    assert_eq!(ask.resolved.as_ref().unwrap().layer, Layer::Admin);
    // The branch says it is parked on the ask, and the decisions put it
    // first, waiting on the person for the agent.
    let parked = repo.branch("asker").unwrap().awaiting.unwrap();
    assert_eq!(
        (parked.id.as_str(), parked.since_ms),
        (ask.id.as_str(), ask.created_ms)
    );
    let decisions = repo.decisions().unwrap();
    let first = &decisions[0];
    assert_eq!(
        (first.kind, first.waits, first.branch.as_str()),
        (DecisionKind::Approval, Waits::Agent, "asker")
    );
    assert_eq!(first.approval.as_ref().unwrap().id, ask.id);
    assert_eq!(first.since_ms, Some(ask.created_ms));
    // `/metrics` counts it.
    let (status, _, metrics) = raw(server.addr, &get("/metrics", Some(METRICS_TOKEN)));
    assert_eq!(status, 200, "{metrics}");
    assert!(
        metrics.contains("branchyard_approvals_pending{repo=\"app\"} 1"),
        "{metrics}"
    );
    // A surface that is not one is refused.
    assert!(repo
        .answer_approval(
            &ask.id,
            true,
            &ApprovalAnswerRequest {
                reason: None,
                surface: Some("fax".into()),
            },
        )
        .is_err());
    let answered = repo
        .answer_approval(
            &ask.id,
            true,
            &ApprovalAnswerRequest {
                reason: Some("ok".into()),
                surface: Some("companion".into()),
            },
        )
        .unwrap();
    let answer = answered.answer.unwrap();
    assert_eq!(
        (answer.by.as_str(), answer.surface.as_str()),
        ("tester", "companion")
    );
    let done = await_operation(&client, &op.id);
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    // Answered, the branch is parked no more and the decision is gone.
    assert!(repo.branch("asker").unwrap().awaiting.is_none());
    assert!(repo
        .decisions()
        .unwrap()
        .iter()
        .all(|d| d.kind != DecisionKind::Approval));

    // The ledger: the issue made, approved by the caller; the deletion
    // blocked by the administrator, never called.
    let effects = repo.effects(Some("asker")).unwrap();
    assert_eq!(effects.len(), 1, "{effects:?}");
    let issue = &effects[0];
    assert_eq!(issue.state, EffectState::Confirmed);
    assert_eq!(issue.approval.as_ref().unwrap().by, "tester");
    assert!(mock.calls_to("slack__chat_delete").is_empty());
    let detail = repo.effect(&issue.id[issue.id.len() - 8..]).unwrap();
    assert_eq!(detail.events.len(), 2);
    let (_, _, metrics) = raw(server.addr, &get("/metrics", Some(METRICS_TOKEN)));
    assert!(
        metrics.contains(
            "branchyard_effects{repo=\"app\",class=\"compensable\",state=\"confirmed\"} 1"
        ),
        "{metrics}"
    );
    assert!(
        metrics.contains("branchyard_approvals_pending{repo=\"app\"} 0"),
        "{metrics}"
    );

    // Undo through the API: the plan, then the compensation.
    let plan = repo.undo_plan("asker", 0).unwrap();
    assert_eq!(plan.compensable.len(), 1);
    let report = repo
        .undo(
            "asker",
            &UndoRequest {
                to: 0,
                only: vec![issue.id.clone()],
            },
        )
        .unwrap();
    assert_eq!(report.outcomes[0].state, EffectState::Compensated);
    assert_eq!(mock.calls_to("github__issues_close").len(), 1);
    let closed = repo.effect(&issue.id).unwrap().entry;
    assert_eq!(closed.undo_approval.unwrap().by, "tester");
    // Reconciling finds nothing to settle.
    assert!(repo.reconcile_effects().unwrap().settled.is_empty());
}

/// The ground plan: two ready branches that both wrote `shared.txt` hold
/// it together, a third holds `only.txt` alone, and a branch merged lets
/// go of what it held.
#[test]
fn the_ground_names_every_open_branch_holding_a_file_and_drops_a_merged_one() {
    let f = Fixture::new();
    let server = Server::start(f.config());
    let client = server.client();
    let repo = client.repo("app");
    for (name, prompt) in [
        ("app-1", "WRITE shared.txt=one"),
        ("app-2", "WRITE shared.txt=two"),
        ("lib", "WRITE only.txt=lib"),
    ] {
        let done = run(&client, &task(prompt, name));
        assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    }
    let held = |ground: &branchyard_client::effects_api::Ground| -> Vec<(String, Vec<String>)> {
        ground
            .files
            .iter()
            .map(|f| {
                let names = f.branches.iter().map(|b| b.name.clone()).collect();
                (f.path.clone(), names)
            })
            .collect()
    };
    let ground = repo.ground().unwrap();
    assert_eq!(
        held(&ground),
        [
            ("only.txt".to_owned(), vec!["lib".to_owned()]),
            (
                "shared.txt".to_owned(),
                vec!["app-1".to_owned(), "app-2".to_owned()]
            ),
        ]
    );
    assert!(ground.files[1]
        .branches
        .iter()
        .all(|b| b.status == BranchStatus::Ready));
    assert!(ground.skipped.is_empty(), "{:?}", ground.skipped);

    // Merged, a branch holds nothing.
    let merge = repo
        .merge("app-1", &MergeRequest::default(), &new_key())
        .unwrap();
    let merged = await_operation(&client, &merge.id);
    assert_eq!(merged.state, OperationState::Succeeded, "{merged:?}");
    assert_eq!(
        held(&repo.ground().unwrap()),
        [
            ("only.txt".to_owned(), vec!["lib".to_owned()]),
            ("shared.txt".to_owned(), vec!["app-2".to_owned()]),
        ]
    );
}

/// The decisions are the branches whose stage puts them in Needs you or
/// Ready, one each: a plan waiting for approval is one; a settled child
/// whose parent runs again is not, nor is the question it sent after its
/// turn ended.
#[test]
fn the_decisions_are_the_branches_that_need_you_or_are_ready_once_each() {
    let f = Fixture::new();
    let mut config = f.config();
    config.allow_delegation = true;
    config.by_path = Some("/bin/true".into());
    let server = Server::start(config);
    let client = server.client();
    let repo = client.repo("app");

    let planned = run(
        &client,
        &TaskRequest {
            plan: true,
            ..task("Mark it PERMISSION WRITE marker.txt=x", "planned")
        },
    );
    assert_eq!(planned.state, OperationState::Succeeded, "{planned:?}");
    let done = run(&client, &task("WRITE done.txt=1", "done"));
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    run(&client, &task("EXIT", "broken"));
    let root = run(
        &client,
        &TaskRequest {
            delegation: Some(Envelope::depth(2)),
            ..task("WRITE root.txt=1", "root")
        },
    );
    assert_eq!(root.state, OperationState::Succeeded, "{root:?}");
    let kid = repo
        .spawn(
            "root",
            &SpawnRequest {
                prompt: "WRITE kid.txt=k".into(),
                name: Some("kid".into()),
                policy: PolicySpec::allow_all(),
                ..SpawnRequest::default()
            },
            &new_key(),
        )
        .unwrap();
    let kid = await_operation(&client, &kid.id);
    assert_eq!(kid.state, OperationState::Succeeded, "{kid:?}");
    repo.ask("kid", "should I rename it?", None).unwrap();
    let again = repo
        .send(
            "root",
            &SendRequest {
                prompt: "HANG".into(),
                policy: PolicySpec::allow_all(),
                ..SendRequest::default()
            },
            &new_key(),
        )
        .unwrap();
    wait::until("root to run again", || {
        repo.branch("root").unwrap().status == BranchStatus::Running
    });

    let branches = repo.branches().unwrap();
    let wanted: BTreeSet<String> = branches
        .iter()
        .filter(|b| matches!(b.stage.map(|s| s.lane), Some(Lane::NeedsYou | Lane::Ready)))
        .map(|b| b.name.clone())
        .collect();
    assert_eq!(
        wanted,
        ["broken", "done", "planned"].map(String::from).into(),
        "{branches:#?}"
    );
    let decisions = repo.decisions().unwrap();
    let listed: Vec<&str> = decisions.iter().map(|d| d.branch.as_str()).collect();
    let once: BTreeSet<String> = listed.iter().map(|b| (*b).to_owned()).collect();
    assert_eq!(
        once.len(),
        listed.len(),
        "a branch decided twice: {listed:?}"
    );
    assert_eq!(once, wanted, "{decisions:#?}");
    let kinds: Vec<(&str, DecisionKind, Waits)> = decisions
        .iter()
        .map(|d| (d.branch.as_str(), d.kind, d.waits))
        .collect();
    assert_eq!(
        kinds,
        [
            ("planned", DecisionKind::Plan, Waits::Agent),
            ("done", DecisionKind::Merge, Waits::Delivery),
            ("broken", DecisionKind::Stop, Waits::Nobody),
        ]
    );

    repo.cancel("root").unwrap();
    await_operation(&client, &again.id);
}

/// A root started with delegation asks the person through the server: the
/// decisions list its question after its turn ended, a token with only the
/// `read` scope sees it in the person's inbox but may not answer it, and an
/// answer with the `run` scope reaches the root's inbox, for its next turn,
/// and takes the question out of the decisions.
#[test]
fn the_person_answers_a_roots_question_through_the_server() {
    const READER: &str = "reader-token-0123456789";
    let f = Fixture::new();
    let mut config = f.config();
    config.allow_delegation = true;
    config.by_path = Some("/bin/true".into());
    config.tokens.push(Token {
        name: "reader".into(),
        secret: READER.into(),
    });
    config.principals.insert(
        "reader".into(),
        Principal {
            origin: Default::default(),
            name: "reader".into(),
            tenant: "default".into(),
            scopes: ["read".to_owned()].into(),
            repos: None,
            groups: Default::default(),
        },
    );
    let server = Server::start(config);
    let client = server.client();
    let repo = client.repo("app");
    let root = run(
        &client,
        &TaskRequest {
            delegation: Some(Envelope::depth(1)),
            ..task("say hi", "root")
        },
    );
    assert_eq!(root.state, OperationState::Succeeded, "{root:?}");
    let asked = repo.ask("root", "which database?", None).unwrap().message;
    assert_eq!(
        (asked.from.as_str(), asked.to.as_str()),
        ("root", branchyard::PERSON)
    );
    let question = |repo: &branchyard_client::Repo| {
        repo.decisions()
            .unwrap()
            .into_iter()
            .find(|d| d.branch == "root")
            .map(|d| (d.kind, d.waits, d.question.map(|m| m.id)))
    };
    assert_eq!(
        question(&repo),
        Some((DecisionKind::Question, Waits::Nobody, Some(asked.id)))
    );

    let reader = branchyard_client::Client::new(&server.url(), READER)
        .unwrap()
        .repo("app");
    let inbox = reader.person_inbox().unwrap();
    let listed: Vec<u64> = inbox.messages.iter().map(|m| m.id).collect();
    assert_eq!(
        (inbox.branch.as_str(), listed),
        (branchyard::PERSON, vec![asked.id])
    );
    let refused = reader.answer_message(asked.id, "postgres").unwrap_err();
    assert_eq!(refused.code(), Some("scope_required"), "{refused}");
    let blank = repo.answer_message(asked.id, " ").unwrap_err();
    assert_eq!(blank.code(), Some("invalid_request"), "{blank}");

    let answer = repo.answer_message(asked.id, "postgres").unwrap();
    assert_eq!(
        (answer.from.as_str(), answer.to.as_str(), answer.in_reply_to),
        (branchyard::PERSON, "root", Some(asked.id))
    );
    assert_eq!(question(&repo), None);
    let waiting = repo.inbox("root").unwrap();
    assert!(
        waiting
            .messages
            .iter()
            .any(|m| m.id == answer.id && !m.delivered),
        "{waiting:?}"
    );
    let again = repo.answer_message(asked.id, "mysql").unwrap_err();
    assert_eq!(again.code(), Some("denied"), "{again}");
}

/// A call whose answer the gateway never sent is `unknown`; with no lookup
/// to ask, the server's recovery pass leaves it so, and `GET /v1/problems`
/// lists it with the pass's reason until a later pass finds it settled
/// (here by the gateway's audit log).
#[test]
fn an_effect_a_recovery_pass_leaves_unknown_is_a_problem_until_it_settles() {
    let f = Fixture::new();
    let mock = MockGateway::start();
    let mut config = f.config();
    config.connectors = Some(connectors(&f, &mock));
    config.approvals = ApprovalSettings {
        admin: None,
        person: None,
        people: [("tester".to_owned(), policy(&[("slack:*", Approval::Allow)]))].into(),
    };
    config.recover_interval = Duration::from_millis(100);
    let audit = config.data_dir.join("gateway/audit.jsonl");
    let server = Server::start(config);
    let client = server.client();
    let repo = client.repo("app");

    mock.hold("slack__chat_post");
    let op = repo
        .submit_task(
            &granted(
                &f,
                "SH python3 call.py slack__chat_post '{\"text\": \"hi\"}'",
                "poster",
            ),
            "k-poster",
        )
        .unwrap();
    mock.wait_held();
    mock.release();
    await_operation(&client, &op.id);
    let entry = repo.effects(Some("poster")).unwrap().remove(0);
    assert_eq!(entry.state, EffectState::Unknown, "{entry:?}");
    assert!(entry.lookup.is_none(), "{entry:?}");

    let problem = wait::until("the unknown effect listed", || {
        let listed = client.problems().unwrap().problems;
        listed.into_iter().find(|p| p.kind == ProblemKind::Effect)
    });
    let short = &entry.id[entry.id.len() - 8..];
    assert_eq!(problem.key, format!("app/{}", entry.id));
    assert_eq!(
        problem.what,
        format!("effect {short} (slack: chat_post) of app/poster")
    );
    assert_eq!(
        problem.error,
        "its operation declares no lookup; check upstream and decide"
    );
    assert_eq!(
        (problem.since_ms, problem.failures),
        (entry.updated_ms, None)
    );
    assert!(
        problem.next.contains(&format!("`by effects show {short}`")),
        "{problem:?}"
    );

    let line = serde_json::json!({"time": branchyard_support::time::rfc3339(entry.created_ms + 1_000),
        "by_branch": "app/poster", "by_turn": "1", "sub": "local:me", "connector": "slack",
        "operation": "slack.chat.post", "decision": "allowed", "upstream_status": 200,
        "error_code": null, "effect_class": "reversible", "ledger_id": entry.id,
        "staged_for": null});
    fs::write(&audit, format!("{line}\n")).unwrap();
    wait::until("the settled effect no longer listed", || {
        client.problems().unwrap().problems.is_empty()
    });
    assert_eq!(
        repo.effect(&entry.id).unwrap().entry.state,
        EffectState::Confirmed
    );
}
