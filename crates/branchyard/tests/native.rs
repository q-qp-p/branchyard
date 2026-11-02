//! Hermetic engine turns through native Docker/kubectl command boundaries.
//! Guest shells are local stand-ins: these checks prove transport, copy-back
//! and cleanup wiring, not container or GKE isolation. No model is called.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod common;

use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use branchyard::services::{self, Outcome, ServiceState, ServiceStore as _};
use branchyard::{
    Activity, AgentSandboxOptions, AgentSandboxRuntime, BranchStatus, ContainerOptions, Error,
    Event, Network, NetworkEnforce, Policy, Provider, Provisioning, SandboxEvent, SandboxOrigin,
    TaskOptions,
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
    fs::read_to_string(bin.join("calls.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
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
            .task("SH printf 'first\\n' > result.txt; printf saved > \"$HOME/saved.txt\"")
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
