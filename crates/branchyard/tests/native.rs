//! Hermetic engine turns through native Docker/kubectl command boundaries.
//! Guest shells are local stand-ins: these checks prove transport, copy-back
//! and cleanup wiring, not container or GKE isolation. No model is called.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // tests: a panic is the failure report
mod common;

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use branchyard::services::{self, Outcome, ServiceState, ServiceStore as _};
use branchyard::{
    Activity, After, AgentSandboxOptions, AgentSandboxRuntime, BranchStatus, ContainerOptions,
    Envelope, Error, Event, GraphEdit, Network, NetworkEnforce, Policy, Provider, Provisioning,
    SandboxEvent, SandboxOrigin, SpawnSpec, TaskOptions, Yard,
};
use branchyard_support::LockExt as _;
use branchyard_testkit::wait;
use common::Fixture;
use serde_json::{json, Value};

#[derive(Clone, Copy)]
enum Native {
    Container,
    Gke(AgentSandboxRuntime),
}

fn fixture(kind: Native) -> (MutexGuard<'static, ()>, Fixture, PathBuf, TaskOptions) {
    // PATH is the worker's process-global transport selection. Serialize this
    // test binary rather than introduce a task-supplied executable override.
    static SERIAL: Mutex<()> = Mutex::new(());
    let guard = SERIAL.lock_recovering("native engine CLI tests");
    let f = Fixture::new();
    let bin = f.dir.join("native-cli");
    fs::create_dir(&bin).unwrap();
    for (command, source) in [("docker", "native-docker"), ("kubectl", "native-kubectl")] {
        std::os::unix::fs::symlink(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(source),
            bin.join(command),
        )
        .unwrap();
    }
    let mut paths = vec![bin.clone()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
    std::env::set_var(
        "BRANCHYARD_REGISTRY",
        f.root.join(".branchyard/registry.db"),
    );
    let image = format!("example.test/runtime@sha256:{}", "a".repeat(64));
    let provider = match kind {
        Native::Container => Provider::container(ContainerOptions {
            image,
            ..ContainerOptions::default()
        }),
        Native::Gke(runtime) => {
            let mode = match runtime {
                AgentSandboxRuntime::Gvisor => "gvisor",
                AgentSandboxRuntime::Microvm => "microvm",
            };
            fs::write(bin.join("state.json"),serde_json::to_vec(&json!({"objects":{},"runtime":mode,"counter":0,"extra_policies":[],"namespace_uid":"namespace-uid"})).unwrap()).unwrap();
            Provider::agent_sandbox(AgentSandboxOptions {
                context: "test-context".into(),
                namespace: format!("branchyard-{mode}"),
                image,
                runtime,
                ..AgentSandboxOptions::default()
            })
        }
    };
    let options = TaskOptions {
        provider: Some(provider),
        policy: Policy::allow_all(),
        ..f.options()
    };
    (guard, f, bin, options)
}

fn calls(bin: &std::path::Path) -> Vec<Value> {
    let text = match fs::read_to_string(bin.join("calls.jsonl")) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => panic!("read fake native CLI calls: {error}"),
    };
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn server_admission(yard: &Yard) {
    yard.use_provider_admission(Arc::new(|actual, options| {
        if matches!(
            actual,
            Some(Provider::Container(_) | Provider::AgentSandbox(_))
        ) && actual != options.provider.as_ref()
        {
            return Err(Error::Denied(
                "native provider needs approved options".into(),
            ));
        }
        Ok(())
    }));
}

fn denied<T: std::fmt::Debug>(result: Result<T, Error>) {
    assert!(
        matches!(&result, Err(Error::Denied(why)) if why == "native provider needs approved options"),
        "expected provider admission denial, got {result:?}"
    );
}

fn delegating(options: TaskOptions) -> TaskOptions {
    TaskOptions {
        delegation: Some(Envelope::default()),
        delegation_server: Some(vec![common::fake_agent().display().to_string()]),
        ..options
    }
}

#[test]
fn native_admission_is_shared_by_clones_and_guards_inherited_turns_before_allocation() {
    let (_serial, f, bin, options) = fixture(Native::Container);
    let root = f
        .yard
        .task("WRITE root.txt=first")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    // The SDK has no embedding restriction until one is installed.
    let root = root.send("WRITE sdk.txt=allowed", f.options()).unwrap();
    assert_eq!(allocations(&root).len(), 2);
    let Some(Provider::Container(container)) = options.provider.as_ref() else {
        panic!("container test provider");
    };
    f.yard
        .configure(
            &serde_json::from_value(json!({
                "defaults": {"provider": "container"},
                "container": {"image": container.image}
            }))
            .unwrap(),
        )
        .unwrap();
    let clone = f.yard.clone();
    server_admission(&clone);
    let before = calls(&bin);
    let branches = f.yard.branches().unwrap().len();
    denied(root.send("WRITE denied.txt=default", TaskOptions::default()));
    denied(root.send("WRITE denied.txt=send", f.options()));
    denied(root.fork("WRITE denied.txt=fork", true, f.options()));
    denied(root.reincarnate(f.options()));
    assert_eq!(f.yard.branches().unwrap().len(), branches);
    assert_eq!(calls(&bin), before, "denial precedes native CLI calls");
    assert_eq!(f.yard.branch("root").unwrap().info().turns, 2);
    // Server-supplied options replace configured defaults. Completing the task
    // must not repopulate their provider and manufacture admission evidence.
    let local = f
        .yard
        .task("WRITE local.txt=local")
        .options(f.options())
        .name("local")
        .run()
        .unwrap();
    assert_eq!(local.provider().unwrap(), None);
    assert_eq!(
        calls(&bin),
        before,
        "config did not restore a native provider"
    );
    let approved = root.send("WRITE approved.txt=yes", options).unwrap();
    assert_eq!(approved.info().status, BranchStatus::Ready);
    assert_eq!(allocations(&approved).len(), 3);
    assert_deleted(&bin, Native::Container);
}

#[test]
fn native_graph_recovery_requires_approved_options_and_preserves_cached_admission() {
    let (_serial, f, bin, options) = fixture(Native::Container);
    let options = delegating(options);
    server_admission(&f.yard);
    let root = f
        .yard
        .task("WRITE root.txt=root")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    root.delegate(options.clone())
        .unwrap()
        .apply_graph(
            [
                ("first", Vec::new(), After::Settled),
                ("cached", vec!["first".into()], After::Integrated),
                ("recovered", vec!["first".into()], After::Integrated),
            ]
            .into_iter()
            .map(|(name, depends_on, after)| {
                GraphEdit::Spawn(SpawnSpec {
                    prompt: format!("WRITE {name}.txt=done"),
                    name: Some(name.into()),
                    depends_on,
                    after,
                    ..SpawnSpec::default()
                })
            })
            .collect(),
            0,
        )
        .unwrap();
    root.wait_subtree().unwrap();
    let db = rusqlite::Connection::open(f.root.join(".branchyard/state.db")).unwrap();
    db.execute(
        "UPDATE graph_edges SET after = 'settled' WHERE dependent = 'cached'",
        [],
    )
    .unwrap();
    let waiting = f.yard.branch("cached").unwrap().info().clone();
    assert_eq!(waiting.status, BranchStatus::Waiting);
    assert!(!waiting.worktree.exists());
    let before = calls(&bin);
    // A fresh hub has no cached trusted options. Opening alone starts nothing.
    let recovered = Yard::open(&f.root).unwrap();
    server_admission(&recovered);
    denied(recovered.resume_graph(&TaskOptions::default()));
    assert_eq!(
        recovered.branch("cached").unwrap().info().status,
        BranchStatus::Waiting
    );
    assert!(
        !waiting.worktree.exists(),
        "denial precedes materialization"
    );
    assert_eq!(calls(&bin), before, "denial precedes native CLI calls");
    // The original hub retained the approved graph proposal's exact provider.
    assert_eq!(
        f.yard.resume_graph(&TaskOptions::default()).unwrap(),
        ["cached"]
    );
    root.wait_subtree().unwrap();
    assert_eq!(
        f.yard.branch("cached").unwrap().info().status,
        BranchStatus::Ready
    );
    db.execute(
        "UPDATE graph_edges SET after = 'settled' WHERE dependent = 'recovered'",
        [],
    )
    .unwrap();
    assert_eq!(recovered.resume_graph(&options).unwrap(), ["recovered"]);
    recovered.branch("root").unwrap().wait_subtree().unwrap();
    assert_eq!(
        recovered.branch("recovered").unwrap().info().status,
        BranchStatus::Ready
    );
    assert_deleted(&bin, Native::Container);
}

#[test]
fn native_parked_recovery_denial_keeps_the_turn_for_an_approved_wake() {
    let kind = Native::Gke(AgentSandboxRuntime::Microvm);
    let (_serial, f, bin, options) = fixture(kind);
    let options = delegating(options);
    let first = f
        .yard
        .task("WRITE root.txt=before-wake")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    assert_eq!(
        first.info().status,
        BranchStatus::Ready,
        "initial native turn"
    );
    // A stopped engine's parked turn after its last child was removed.
    common::edit_record(&f.root, "root", |record| {
        record["info"]["status"] = json!({"state": "waiting_on_children"});
        record["parked"] = json!({
            "since_ms": 0, "ended": {"state": "ready"}, "on": [], "budget": {}
        });
    });
    let before_record = common::stored_record(&f.root, "root");
    let before = calls(&bin);
    let yard = Yard::open(&f.root).unwrap();
    server_admission(&yard);
    denied(yard.resume_graph(&TaskOptions::default()));
    let mut wrong = options.clone();
    let Some(Provider::AgentSandbox(provider)) = wrong.provider.as_mut() else {
        panic!("GKE test provider");
    };
    provider.context = "another-context".into();
    denied(yard.resume_graph(&wrong));
    let parked = common::stored_record(&f.root, "root");
    assert_eq!(parked["info"]["status"]["state"], "waiting_on_children");
    assert_eq!(parked["info"]["turns"], 1);
    assert_eq!(parked, before_record, "denial preserves the parked turn");
    assert_eq!(calls(&bin), before, "denial precedes native CLI calls");
    assert_eq!(yard.resume_graph(&options).unwrap(), ["root"]);
    yard.branch("root").unwrap().wait_subtree().unwrap();
    let root = yard.branch("root").unwrap();
    assert_eq!(root.info().turns, 2);
    assert_eq!(root.info().status, BranchStatus::Ready);
    assert_eq!(root.provider().unwrap(), options.provider);
    assert_deleted(&bin, kind);
}

fn assert_deleted(bin: &std::path::Path, kind: Native) {
    match kind {
        Native::Container => {
            let state: Value =
                serde_json::from_slice(&fs::read(bin.join("docker.json")).unwrap()).unwrap();
            assert!(state.get("container").is_none());
            assert_eq!(state["volumes"], json!([]));
            assert!(calls(bin)
                .iter()
                .any(|call| call[2] == "container" && call[3] == "rm"));
        }
        Native::Gke(_) => {
            let state: Value =
                serde_json::from_slice(&fs::read(bin.join("state.json")).unwrap()).unwrap();
            assert!(state["objects"].as_object().unwrap().is_empty());
            assert!(calls(bin).iter().any(|call| call["operation"] == "delete"));
        }
    }
}

fn allocations(branch: &branchyard::Branch) -> Vec<String> {
    branch
        .events()
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.activity {
            Activity::Sandbox(event) => match *event {
                SandboxEvent::Started {
                    sandbox, origin, ..
                } => {
                    assert!(matches!(origin, SandboxOrigin::Fresh { .. }));
                    Some(sandbox)
                }
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[test]
fn native_turns_copy_back_work_and_private_home_then_use_a_fresh_allocation() {
    for kind in [
        Native::Container,
        Native::Gke(AgentSandboxRuntime::Gvisor),
        Native::Gke(AgentSandboxRuntime::Microvm),
    ] {
        let (_serial, f, bin, options) = fixture(kind);
        let first = f
            .yard
            .task("SH printf 'first\\n' > result.txt; printf saved > \"$HOME/saved.txt\"; printf %s \"$BRANCHYARD_TOOLS\" > tools-env.txt")
            .options(options.clone())
            .name("native")
            .run()
            .unwrap();
        assert_eq!(
            first.info().status,
            BranchStatus::Ready,
            "native initial turn"
        );
        assert_eq!(
            fs::read_to_string(first.info().worktree.join("result.txt")).unwrap(),
            "first\n"
        );
        // The guest is told where the tool tree is, and the tree was copied to that root after the home.
        // The fake CLIs map guest paths into their private directory, `/tools` included.
        assert_eq!(
            fs::read_to_string(first.info().worktree.join("tools-env.txt")).unwrap(),
            bin.join("tools").display().to_string()
        );
        assert!(bin.join("tools/INDEX.md").is_file());
        assert!(bin.join("tools/bin/tools").is_file());
        assert_deleted(&bin, kind);
        assert!(f
            .yard
            .services()
            .unwrap()
            .all()
            .unwrap()
            .iter()
            .filter(|service| service.kind == services::KIND_SANDBOX)
            .all(|service| service.state == ServiceState::Left));
        let second = first
            .send(
                "SH test \"$(cat \"$HOME/saved.txt\")\" = saved && printf 'second\\n' > next.txt",
                options,
            )
            .unwrap();
        assert_eq!(
            second.info().status,
            BranchStatus::Ready,
            "native follow-up turn"
        );
        assert_eq!(
            fs::read_to_string(second.info().worktree.join("next.txt")).unwrap(),
            "second\n"
        );
        let names = allocations(&second);
        assert_eq!(names.len(), 2);
        assert_ne!(names[0], names[1]);
        assert_deleted(&bin, kind);
    }
}

#[test]
fn native_failed_cleanup_keeps_its_owned_reclaim_intent_through_every_setup_phase() {
    for (phase, kind) in [
        ("release", Native::Container),
        ("acquire", Native::Container),
        ("push", Native::Container),
        ("release", Native::Gke(AgentSandboxRuntime::Gvisor)),
    ] {
        let (_serial, f, bin, options) = fixture(kind);
        fs::write(bin.join("fail-destroy"), b"retry cleanup").unwrap();
        if matches!(kind, Native::Container) {
            fs::write(bin.join("require-registration"), b"before create").unwrap();
        }
        if phase != "release" {
            fs::write(bin.join(format!("fail-{phase}")), b"setup failure").unwrap();
        }
        let branch = f
            .yard
            .task("WRITE returned.txt=work")
            .options(options)
            .name("cleanup")
            .run()
            .unwrap();
        if phase == "release" {
            assert_eq!(branch.info().status, BranchStatus::Ready);
        } else {
            assert!(matches!(branch.info().status, BranchStatus::Failed { .. }));
        }
        let registry = f.yard.services().unwrap();
        let owned: Vec<_> = registry
            .all()
            .unwrap()
            .into_iter()
            .filter(|service| {
                service.kind == services::KIND_SANDBOX
                    && service.state == ServiceState::Live
                    && service.owner.branch.as_deref() == Some("cleanup")
            })
            .collect();
        assert_eq!(
            owned.len(),
            1,
            "{phase}: failed cleanup must remain discoverable"
        );
        let intent = &owned[0];
        assert!(matches!(
            intent.reclaim,
            Some(services::Reclaim::Sandbox { .. })
        ));
        registry
            .transition(
                &intent.id,
                intent.seq,
                ServiceState::Expired,
                Some("simulate abandoned lease expiry".into()),
                branchyard_support::time::now_ms(),
            )
            .unwrap()
            .unwrap();
        let attempted = f.yard.reclaim_services().unwrap();
        assert!(
            attempted
                .iter()
                .any(|item| item.service.id == intent.id
                    && matches!(item.outcome, Outcome::Failed(_))),
            "{phase}: {attempted:?}"
        );
        let retained = registry.get(&intent.id).unwrap().unwrap();
        assert_eq!(retained.state, ServiceState::Expired);
        assert_eq!(retained.reclaim, intent.reclaim);
        fs::remove_file(bin.join("fail-destroy")).unwrap();
        if phase != "release" {
            fs::remove_file(bin.join(format!("fail-{phase}"))).unwrap();
        }
        let completed = f.yard.reclaim_services().unwrap();
        assert!(
            completed.iter().any(
                |item| item.service.id == intent.id && matches!(item.outcome, Outcome::Done(_))
            ),
            "{phase}: {completed:?}"
        );
        assert_eq!(
            registry.get(&intent.id).unwrap().unwrap().state,
            ServiceState::Reclaimed
        );
        assert_deleted(&bin, kind);
    }
}

#[test]
fn native_setup_failure_deregisters_ownership_when_cleanup_succeeds() {
    for phase in ["acquire", "push"] {
        let (_serial, f, bin, options) = fixture(Native::Container);
        fs::write(bin.join("require-registration"), b"before create").unwrap();
        fs::write(bin.join(format!("fail-{phase}")), b"setup failure").unwrap();
        let failed = f
            .yard
            .task("never starts")
            .options(options)
            .name("setup-failed")
            .run()
            .unwrap();
        assert!(matches!(failed.info().status, BranchStatus::Failed { .. }));
        assert_deleted(&bin, Native::Container);
        let rows: Vec<_> = f
            .yard
            .services()
            .unwrap()
            .all()
            .unwrap()
            .into_iter()
            .filter(|service| service.kind == services::KIND_SANDBOX)
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, ServiceState::Left, "{phase}");
    }
}

#[test]
fn native_cancellation_and_capture_failure_both_destroy_the_allocation() {
    for kind in [Native::Container, Native::Gke(AgentSandboxRuntime::Gvisor)] {
        let (_serial, f, bin, options) = fixture(kind);
        let yard = f.yard.clone();
        let run_options = options.clone();
        let running = std::thread::spawn(move || {
            yard.task("HANG")
                .options(run_options)
                .name("cancelled")
                .run()
        });
        wait::until("native harness session", || {
            f.yard.branch("cancelled").ok().filter(|branch| {
                if let BranchStatus::Failed { reason } = &branch.info().status {
                    panic!("native harness failed to start: {reason}");
                }
                branch.events().unwrap().iter().any(|event| {
                    matches!(
                        event.activity,
                        Activity::Harness(Event::SessionStarted { .. })
                    )
                })
            })
        });
        fs::write(
            bin.join("fail-exec"),
            b"simulate interrupted return transport",
        )
        .unwrap();
        f.yard.cancel("cancelled").unwrap();
        let cancelled = running.join().unwrap().unwrap();
        assert_eq!(cancelled.info().status, BranchStatus::Interrupted);
        assert_deleted(&bin, kind);
        fs::remove_file(bin.join("fail-exec")).unwrap();
        let marker = branchyard_recipe::quote(&bin.join("fail-exec").display().to_string());
        let failed = f
            .yard
            .task(format!("SH printf changed > output.txt; touch {marker}"))
            .options(options)
            .name("failed-capture")
            .run()
            .unwrap();
        assert!(matches!(failed.info().status, BranchStatus::Failed { .. }));
        assert_deleted(&bin, kind);
    }
}

#[test]
fn native_required_egress_and_invalid_specs_are_refused_before_any_cli_or_branch() {
    for kind in [Native::Container, Native::Gke(AgentSandboxRuntime::Gvisor)] {
        let (_serial, f, bin, mut options) = fixture(kind);
        options.provision = Some(Provisioning {
            network: Some(Network::none().with_enforce(NetworkEnforce::Required)),
            ..Provisioning::default()
        });
        assert!(matches!(
            f.yard.task("not started").options(options).run(),
            Err(Error::Unsupported(_))
        ));
        assert!(f.yard.branches().unwrap().is_empty());
        assert!(calls(&bin).is_empty());
        let invalid = Provider::container(ContainerOptions {
            image: "mutable:latest".into(),
            ..ContainerOptions::default()
        });
        assert!(f.yard.task("not started").provider(invalid).run().is_err());
        assert!(f.yard.branches().unwrap().is_empty());
        assert!(calls(&bin).is_empty());
    }
}
