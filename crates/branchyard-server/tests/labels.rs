//! Worker labels over real HTTP: an operation that requires a label no
//! live worker carries stays queued and, after `unclaimable_after`, says
//! why in `GET /v1/operations/{id}` and `GET /v1/repos/{repo}/operations`;
//! a server whose worker carries the labels runs it after a restart on
//! the same data directory; work requiring nothing runs anywhere; a
//! malformed label is refused at admission; such an operation is listed in
//! `GET /v1/problems` for its tenant and `admin` until a worker claims it.

mod common;

use std::time::Duration;

use branchyard_client::api::{OperationState, ProblemKind};
use branchyard_client::{new_key, Client};
use branchyard_support::time::now_ms;
use branchyard_testkit::wait;
use common::{await_operation, get, raw, run, task, Fixture, Server};

#[test]
fn labeled_work_waits_for_a_worker_that_carries_its_labels_and_says_why() {
    let f = Fixture::new();
    let mut config = f.config();
    config.labels = vec!["linux".into()];
    config.unclaimable_after = Duration::ZERO;
    let server = Server::start(config);
    let client = server.client();

    let mut gpu = task("WRITE gpu.txt=1", "needs-gpu");
    gpu.require_labels = vec!["gpu".into(), "linux".into(), "gpu".into()];
    let op = client.repo("app").submit_task(&gpu, &new_key()).unwrap();
    assert_eq!(op.requires, ["gpu", "linux"]);
    // Work that requires nothing still runs while it waits.
    let plain = run(&client, &task("WRITE plain.txt=1", "plain"));
    assert_eq!(plain.state, OperationState::Succeeded);
    wait::until("the operation to say why it waits", || {
        client.operation(&op.id).unwrap().waiting.is_some()
    });
    let waiting = client.operation(&op.id).unwrap();
    assert_eq!(waiting.state, OperationState::Queued);
    let reason = waiting.waiting.unwrap();
    assert!(reason.contains("gpu, linux"), "{reason}");
    let host = branchyard::process_identity().0;
    assert!(
        reason.contains(&format!("[linux, worker-host:{host}]")),
        "{reason}"
    );
    let listed = client.repo("app").operations(Some("needs-gpu")).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, op.id);
    assert!(listed[0].waiting.is_some());
    assert!(client
        .repo("app")
        .operations(Some("plain"))
        .unwrap()
        .is_empty());

    // A malformed label is refused before anything is admitted.
    let mut bad = task("WRITE x.txt=1", "bad-label");
    bad.require_labels = vec!["GPU box".into()];
    let error = client
        .repo("app")
        .submit_task(&bad, &new_key())
        .unwrap_err();
    assert_eq!(error.code(), Some("invalid_request"), "{error}");

    // A worker with the labels takes it over.
    server.stop();
    let mut config = f.config();
    config.labels = vec!["linux".into(), "gpu".into()];
    let server = Server::start(config);
    let client = server.client();
    let done = await_operation(&client, &op.id);
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    assert_eq!(done.waiting, None);
}

#[test]
fn an_operation_no_live_worker_can_claim_is_a_problem_until_one_claims_it() {
    const UNCLAIMABLE_AFTER: Duration = Duration::from_millis(200);
    let f = Fixture::new();
    let appb = f.extra_repo("appb");
    let config = |labels: &[&str]| {
        let mut config = f.config();
        common::two_tenants(&mut config, appb.clone(), None);
        common::one_admin(&mut config);
        config.labels = labels.iter().map(|l| (*l).to_owned()).collect();
        config.unclaimable_after = UNCLAIMABLE_AFTER;
        config
    };
    let server = Server::start(config(&["linux"]));
    let acme = Client::new(&server.url(), common::ACME_TOKEN).unwrap();
    let mut gpu = task("WRITE gpu.txt=1", "needs-gpu");
    gpu.require_labels = vec!["gpu".into()];
    let op = acme.repo("app").submit_task(&gpu, &new_key()).unwrap();

    let listed = wait::until("the operation to be listed as a problem", || {
        let problems = acme.problems().unwrap().problems;
        match problems.is_empty() {
            true => Err("nothing listed yet"),
            false => Ok(problems),
        }
    });
    assert_eq!(listed.len(), 1, "{listed:?}");
    let problem = &listed[0];
    assert_eq!(
        (problem.kind, problem.key.as_str(), problem.failures),
        (ProblemKind::Operation, op.id.as_str(), None)
    );
    assert_eq!(problem.what, format!("operation {} on app", op.id));
    assert_eq!(problem.since_ms, op.created_at_ms);
    assert!(now_ms() - problem.since_ms >= UNCLAIMABLE_AFTER.as_millis() as u64);
    assert_eq!(
        Some(problem.error.as_str()),
        acme.operation(&op.id).unwrap().waiting.as_deref()
    );
    assert!(problem.error.contains("(gpu)"), "{problem:?}");
    assert_eq!(
        problem.next,
        "start `by worker` or `by serve` for app with `--label gpu`; it claims the operation"
    );
    let globex = Client::new(&server.url(), common::GLOBEX_TOKEN).unwrap();
    assert_eq!(globex.problems().unwrap().problems, [], "another tenant's");
    let admin = Client::new(&server.url(), common::GLOBEX_ADMIN_TOKEN).unwrap();
    assert_eq!(
        admin.problems().unwrap().problems,
        [],
        "admin does not cross tenants"
    );
    let (status, _, body) = raw(server.addr, &get("/healthz", None));
    assert_eq!(
        (status, body.as_str()),
        (200, "ok\n"),
        "a problem is not ill health"
    );

    server.stop();
    let server = Server::start(config(&["linux", "gpu"]));
    let acme = Client::new(&server.url(), common::ACME_TOKEN).unwrap();
    let done = await_operation(&acme, &op.id);
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    assert_eq!(acme.problems().unwrap().problems, []);
}

/// A credential configured without `scopes` holds every scope, `admin`
/// included (`config.rs`, a `tokens` or `credentials` entry with none), so
/// each tenant's default token is an admin. A problem still stays inside
/// its tenant: `admin` never crosses tenants.
#[test]
fn a_tenants_problem_is_hidden_from_another_tenants_default_scoped_admin_token() {
    let f = Fixture::new();
    let appb = f.extra_repo("appb");
    let mut config = f.config();
    common::two_tenants(&mut config, appb, None);
    assert!(
        ["acme", "globex"]
            .iter()
            .all(|name| config.principals[*name].allows("admin")),
        "both tenants' default principals hold admin"
    );
    config.labels = vec!["linux".into()];
    config.unclaimable_after = Duration::ZERO;
    let server = Server::start(config);
    let acme = Client::new(&server.url(), common::ACME_TOKEN).unwrap();
    let globex = Client::new(&server.url(), common::GLOBEX_TOKEN).unwrap();
    let mut gpu = task("WRITE gpu.txt=1", "needs-gpu");
    gpu.require_labels = vec!["gpu".into()];
    let op = acme.repo("app").submit_task(&gpu, &new_key()).unwrap();

    let listed = wait::until("acme's operation listed for acme", || {
        let problems = acme.problems().unwrap().problems;
        (!problems.is_empty()).then_some(problems)
    });
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].key, op.id);
    assert_eq!(
        globex.problems().unwrap().problems,
        [],
        "globex's admin token must not see acme's problem"
    );
}
