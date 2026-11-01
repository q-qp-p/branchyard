//! Approvals and the effect ledger on a server, over real HTTP, against a
//! mock gateway on loopback (`mock_gateway`): the administrator's locked
//! policy holds over a principal's own, an ask is answered through the API
//! as the caller, the ledger and an undo plan are read and an undo is
//! performed through the API, and `/metrics` counts effects and waiting
//! approvals. A shell script stands in for Anvil's packaging. The ground
//! plan reads the open branches' diffs.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;
#[path = "../../branchyard/tests/mock_gateway/mod.rs"]
mod mock_gateway;

use std::fs;
use std::os::unix::fs::PermissionsExt;

use branchyard::connectors::GrantEntry;
use branchyard::effects::{Approval, ApprovalPolicy, ApprovalSettings, EffectState, Layer};
use branchyard::{BranchStatus, Provisioning};
use branchyard_client::api::{MergeRequest, OperationState, TaskRequest};
use branchyard_client::effects_api::{ApprovalAnswerRequest, DecisionKind, UndoRequest, Waits};
use branchyard_client::new_key;
use branchyard_server::config::{ConnectorsConfig, MetricsConfig};
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

#[test]
fn a_locked_policy_holds_an_ask_is_answered_through_the_api_and_undo_runs_there() {
    let f = Fixture::new();
    let mock = MockGateway::start();
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
    let mut config = f.config();
    config.connectors = Some(ConnectorsConfig {
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
    });
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
