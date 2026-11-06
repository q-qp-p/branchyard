//! Helpers the in-process tests of the command modules share: a terminal
//! that is none, a server that is never reached, the report inside a
//! failure, and a `PATH` with a fake program first.

use std::ffi::OsString;
use std::path::Path;
use std::sync::Mutex;

use branchyard_support::LockExt as _;

use crate::commands::{Env, Failure, Target};
use crate::errors::Report;

/// A process with no terminal and no color.
pub(crate) fn env() -> Env {
    Env {
        stdin_tty: false,
        stdout_tty: false,
        stderr_tty: false,
        color: false,
        notify: crate::notify::Settings::default(),
    }
}

/// `--remote`: a server on a port nothing listens on. A command that reaches
/// it fails with a transport error, so a test that expects a refusal first
/// shows it did not.
pub(crate) fn remote() -> Target {
    let client = branchyard_client::Client::new("http://127.0.0.1:1", "token")
        .expect("a loopback URL and a token make a client");
    let repo = client.repo("app");
    Target::Remote(Box::new(crate::remote::Remote {
        client,
        repo,
        args: Vec::new(),
    }))
}

/// The report a command refused with.
pub(crate) fn report_of(failure: Failure) -> Report {
    match failure {
        Failure::Report(report) => *report,
        other => panic!("expected a report, got {other:?}"),
    }
}

/// One test at a time changes the process's environment.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Run `body` with the variables `vars` names set, then put them back.
/// `vars` runs once the lock is held, so it may read the environment as no
/// other test is changing it. Programs other tests start in the meantime
/// still find what they found before, so a `PATH` is only ever added to.
fn with_vars<T>(
    vars: impl FnOnce() -> Vec<(&'static str, OsString)>,
    body: impl FnOnce() -> T,
) -> T {
    let _held = ENV_LOCK.lock_recovering("environment lock");
    struct Restore(Vec<(&'static str, Option<OsString>)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            for (name, value) in &self.0 {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
    let mut restore = Restore(Vec::new());
    for (name, value) in vars() {
        restore.0.push((name, std::env::var_os(name)));
        std::env::set_var(name, value);
    }
    body()
}

/// Run `body` with `variable` set to `value`.
pub(crate) fn with_env<T>(variable: &'static str, value: &Path, body: impl FnOnce() -> T) -> T {
    with_vars(|| vec![(variable, value.as_os_str().to_owned())], body)
}

/// Run `body` with `dir` in front of the `PATH` it would otherwise have.
pub(crate) fn with_path<T>(dir: &Path, body: impl FnOnce() -> T) -> T {
    with_vars(
        || {
            let mut parts = vec![dir.to_path_buf()];
            if let Some(path) = std::env::var_os("PATH") {
                parts.extend(std::env::split_paths(&path));
            }
            let joined = std::env::join_paths(parts).expect("directories without a colon");
            vec![("PATH", joined)]
        },
        body,
    )
}
