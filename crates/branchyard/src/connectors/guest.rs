//! The credential proxy as a process in a sandbox the engine started.
//!
//! For a provider whose [`super::CredentialSite`] is `Guest` the engine runs
//! `by credential-proxy` in the sandbox, before the harness, with the
//! per-turn secret in its environment (never its arguments, which a process
//! listing shows). The proxy prints `listening <address>` once it holds its
//! first token; a proxy that cannot get one exits with the reason on its
//! error output, and the turn fails with it instead of the harness's first
//! call.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use branchyard_sandbox::{ExecSpec, Process, SandboxProvider};

use super::sandbox_proxy::{IdentityKind, ProxyConfig, ENV_SECRET, READY};

/// How long a guest's proxy may take to be ready.
const READY_WITHIN: Duration = Duration::from_secs(60);

/// A proxy running in a sandbox; killed when dropped.
pub(crate) struct GuestProxy {
    process: Box<dyn Process>,
}

impl Drop for GuestProxy {
    fn drop(&mut self) {
        branchyard_support::best_effort("stop the guest's credential proxy", self.process.kill());
        self.process.teardown();
    }
}

/// The command line that starts the proxy for `config`.
pub(crate) fn argv(command: &str, config: &ProxyConfig) -> Vec<String> {
    let mut argv = vec![
        command.to_owned(),
        "credential-proxy".to_owned(),
        "--listen".to_owned(),
        config.listen.clone(),
        "--server".to_owned(),
        config.server.clone(),
        "--upstream".to_owned(),
        config.upstream.clone(),
        "--manifest".to_owned(),
        config.manifest.clone(),
        "--branch".to_owned(),
        config.branch.clone(),
        "--turn".to_owned(),
        config.turn.to_string(),
    ];
    match &config.identity {
        IdentityKind::Secret => argv.extend(["--identity".to_owned(), "secret".to_owned()]),
        IdentityKind::Workload { audience } => argv.extend([
            "--identity".to_owned(),
            "workload".to_owned(),
            "--audience".to_owned(),
            audience.clone(),
        ]),
    }
    argv
}

/// Start the proxy for `config` in sandbox `name` of `provider`, wait until
/// it is ready, and return it. `secret` is given to it in [`ENV_SECRET`].
pub(crate) fn start(
    provider: &dyn SandboxProvider,
    name: &str,
    cwd: &str,
    command: &str,
    config: &ProxyConfig,
    secret: Option<&str>,
) -> Result<GuestProxy, String> {
    let mut env: BTreeMap<OsString, OsString> = BTreeMap::new();
    if let Some(secret) = secret {
        env.insert(ENV_SECRET.into(), secret.into());
    }
    let spec = ExecSpec {
        argv: argv(command, config),
        cwd: PathBuf::from(cwd),
        env,
        writable: None,
    };
    let mut process = provider
        .exec(name, &spec)
        .map_err(|e| format!("could not start `{command} credential-proxy` in the sandbox: {e}"))?;
    let stdout = process.take_stdout();
    let stderr = process.take_stderr();
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::Builder::new()
        .name("by-credential-ready".into())
        .spawn(move || {
            let mut line = String::new();
            let first = stdout.map(|out| BufReader::new(out).read_line(&mut line));
            let mut errors = String::new();
            if !line.starts_with(READY) {
                if let Some(mut stderr) = stderr {
                    branchyard_support::best_effort(
                        "read the proxy's error output",
                        stderr.by_ref().take(4096).read_to_string(&mut errors),
                    );
                }
            }
            branchyard_support::best_effort(
                "report the proxy's first line",
                tx.send((first, line, errors)),
            );
        })
        .map_err(|e| e.to_string())?;
    let result = rx.recv_timeout(READY_WITHIN);
    let ready = match result {
        Ok((_, line, _)) if line.starts_with(READY) => Ok(()),
        Ok((_, line, errors)) => Err(format!(
            "the credential proxy in the sandbox did not start: {}",
            first_words(&errors, &line)
        )),
        Err(_) => Err(format!(
            "the credential proxy in the sandbox was not ready in {}s",
            READY_WITHIN.as_secs()
        )),
    };
    match ready {
        Ok(()) => {
            branchyard_support::join_reporting("credential proxy readiness", reader);
            Ok(GuestProxy { process })
        }
        Err(reason) => {
            branchyard_support::best_effort("stop the guest's credential proxy", process.kill());
            process.teardown();
            Err(reason)
        }
    }
}

/// What a failed start said: its error output, else its first line.
fn first_words(errors: &str, line: &str) -> String {
    let said = if errors.trim().is_empty() {
        line
    } else {
        errors
    };
    match said.trim() {
        "" => "it exited without saying why".to_owned(),
        text => text.lines().next().unwrap_or(text).to_owned(),
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // tests: a panic is the failure report
#[cfg(test)]
mod tests;
