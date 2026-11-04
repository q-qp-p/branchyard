//! Claude Code's managed settings are the administrator's: a sandboxed
//! Claude Code turn writes nothing under `/etc/claude-code`, whatever its
//! policy, and records no managed settings. Turns before 3 November 2026
//! wrote the branch's denials there, replacing whatever an image's
//! administrator had put in the file (issue #62); the policy now binds
//! through the session itself (the PreToolUse hook and `--permission-mode
//! default`, `crates/branchyard-harness/src/claude_code.rs`). The fake
//! provider runs processes on this host and logs every exec, so the test
//! sees each command the engine ran in the sandbox. Hermetic; nothing here
//! is evidence about a real sandbox's `/etc`.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};

use branchyard::{
    Activity, Branch, Policy, PolicyPreset, Provider, Provisioning, RecordedEvent, SandboxOptions,
    TaskOptions, Telemetry,
};
use branchyard_runtime::LocalProvider;
use branchyard_sandbox::fake::FakeProvider;
use branchyard_sandbox::{
    Capabilities, ExecSpec, Process, ProviderError, SandboxInfo, SandboxProvider, SandboxSpec,
};
use branchyard_support::LockExt as _;
use common::{fake_agent, Fixture};

/// The directory of Claude Code's managed settings.
const MANAGED_DIR: &str = "/etc/claude-code";

/// A fake provider that logs every exec in order.
struct Logging {
    inner: Arc<FakeProvider>,
    log: Mutex<Vec<Vec<String>>>,
}

impl Logging {
    fn execs(&self) -> Vec<Vec<String>> {
        self.log.lock_recovering("exec log").clone()
    }
}

impl SandboxProvider for Logging {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn ensure(&self, spec: &SandboxSpec) -> Result<SandboxInfo, ProviderError> {
        self.inner.ensure(spec)
    }

    fn inspect(&self, name: &str) -> Result<Option<SandboxInfo>, ProviderError> {
        self.inner.inspect(name)
    }

    fn exec(&self, name: &str, spec: &ExecSpec) -> Result<Box<dyn Process>, ProviderError> {
        self.log.lock_recovering("exec log").push(spec.argv.clone());
        self.inner.exec(name, spec)
    }

    fn stop(&self, name: &str) -> Result<(), ProviderError> {
        self.inner.stop(name)
    }

    fn destroy(&self, name: &str) -> Result<(), ProviderError> {
        self.inner.destroy(name)
    }
}

/// Install the logging provider for the fixture's yard.
fn install(f: &Fixture) -> Arc<Logging> {
    let inner = Arc::new(FakeProvider::plain(
        Box::new(LocalProvider::new()),
        f.dir.join("fake-provider"),
    ));
    let logging = Arc::new(Logging {
        inner,
        log: Mutex::new(Vec::new()),
    });
    f.yard.use_sandbox_provider(logging.clone());
    logging
}

/// Options for a Claude Code stream-json turn in a sandbox, the fake agent
/// as its harness; `telemetry` as the task asks it.
fn sandboxed(f: &Fixture, record: &Path, telemetry: Option<Telemetry>) -> TaskOptions {
    TaskOptions {
        harness: Some("claude-code-stream-json".into()),
        command: Some(vec![
            fake_agent().display().to_string(),
            "--record-launch".into(),
            record.display().to_string(),
        ]),
        provider: Some(Provider::Microsandbox(SandboxOptions {
            image: "registry.example/harness:1".into(),
            ..SandboxOptions::default()
        })),
        provision: telemetry.map(|telemetry| Provisioning {
            telemetry: Some(telemetry),
            ..Provisioning::default()
        }),
        ..f.options()
    }
}

/// Run one turn with `options` under `policy`.
fn turn(f: &Fixture, name: &str, options: TaskOptions, policy: Policy) -> Branch {
    f.task("hello")
        .options(options)
        .name(name)
        .policy(policy)
        .run()
        .unwrap()
}

/// Whether the turn's `Provisioned` activity records managed settings.
fn records_managed(events: &[RecordedEvent]) -> bool {
    events.iter().any(|e| {
        matches!(
            &e.activity,
            Activity::Provisioned {
                managed: Some(_),
                ..
            }
        )
    })
}

/// The command line the harness was started with, from the fake agent's
/// launch record.
fn launched(record: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(record).unwrap();
    let launch: serde_json::Value = serde_json::from_str(&text).unwrap();
    serde_json::from_value(launch["cmdline"].clone()).unwrap()
}

/// Under each restricting preset, with telemetry on and off, a sandboxed
/// turn starts the harness in the default permission mode and runs no
/// command that touches the managed-settings directory.
#[test]
fn a_sandboxed_turn_leaves_the_administrators_settings_alone() {
    for (name, policy, telemetry) in [
        ("read-only", PolicyPreset::ReadOnly.policy(), None),
        (
            "edit",
            PolicyPreset::EditWorktree.policy(),
            Some(Telemetry::parse("off").unwrap()),
        ),
    ] {
        let f = Fixture::new();
        let logging = install(&f);
        let record = f.dir.join("launch.json");
        let branch = turn(&f, name, sandboxed(&f, &record, telemetry), policy);
        // The recording agent exits once it has recorded its launch, so the
        // turn ends in a failure the driver reports; only the launch and
        // what ran before it matter here.
        let events = branch.events().unwrap();
        assert!(record.exists(), "{name}: the harness started: {events:?}");
        let execs = logging.execs();
        assert!(
            execs
                .iter()
                .all(|argv| !argv.iter().any(|a| a.contains(MANAGED_DIR))),
            "{name}: {execs:?}"
        );
        assert!(!records_managed(&events), "{name}: {events:?}");
        let cmdline = launched(&record);
        let mode = cmdline.iter().position(|a| a == "--permission-mode");
        assert_eq!(
            mode.and_then(|at| cmdline.get(at + 1)).map(String::as_str),
            Some("default"),
            "{name}: {cmdline:?}"
        );
    }
}
