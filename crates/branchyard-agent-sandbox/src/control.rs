use std::io::{self, Write};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use branchyard_sandbox::ProviderError;
use serde_json::Value;

use crate::config::{runtime_error, Config};

#[derive(Clone)]
pub(crate) struct Control {
    pub config: Config,
    // Only tests replace this field, never wire/config input.
    pub binary: std::path::PathBuf,
    pub python: std::path::PathBuf,
}

impl Control {
    pub fn check_host(&self) -> Result<(), ProviderError> {
        let mut command = Command::new(&self.python);
        command.args([
            "-c",
            "import subprocess,sys; assert sys.version_info >= (3, 8)",
        ]);
        run(command, None, Duration::from_secs(10)).map_err(|_| runtime_error(
            "Python 3.8 or newer is required on the operator host before allocating an Agent Sandbox",
        ))?;
        Ok(())
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        command.args(["--context", &self.config.context, "--request-timeout=30s"]);
        command
    }

    pub fn namespaced(&self) -> Command {
        let mut command = self.command();
        command.args(["--namespace", &self.config.namespace]);
        command
    }

    pub fn get(
        &self,
        kind: &str,
        name: &str,
        namespace: Option<&str>,
    ) -> Result<Option<Value>, ProviderError> {
        let mut command = self.command();
        if let Some(namespace) = namespace {
            command.args(["--namespace", namespace]);
        }
        command.args(["get", kind]);
        if !name.is_empty() {
            command.arg(name);
        }
        command.args(["--ignore-not-found", "-o", "json"]);
        let output = run(command, None, Duration::from_secs(45))?;
        if output.stdout.is_empty() {
            return Ok(None);
        }
        serde_json::from_slice(&output.stdout)
            .map(Some)
            .map_err(|_| runtime_error("kubectl returned invalid resource JSON"))
    }

    pub fn get_here(&self, kind: &str, name: &str) -> Result<Option<Value>, ProviderError> {
        self.get(kind, name, Some(&self.config.namespace))
    }

    pub fn create(&self, value: &Value) -> Result<(), ProviderError> {
        let mut command = self.namespaced();
        command.args(["create", "-f", "-", "-o", "json"]);
        let body = serde_json::to_vec(value).map_err(|e| runtime_error(e.to_string()))?;
        run(command, Some(&body), Duration::from_secs(45))?;
        Ok(())
    }

    /// Set `key=value` on an object's labels (or annotations), without
    /// `--overwrite`: kubectl refuses when the key already holds another
    /// value, so an object that someone else marked is never taken.
    pub fn mark(
        &self,
        verb: &str,
        kind: &str,
        name: &str,
        key: &str,
        value: &str,
    ) -> Result<(), ProviderError> {
        let mut command = self.namespaced();
        command.args([verb, kind, name, &format!("{key}={value}")]);
        run(command, None, Duration::from_secs(45))?;
        Ok(())
    }

    /// Merge-patch an object.
    pub fn patch(&self, kind: &str, name: &str, patch: &Value) -> Result<(), ProviderError> {
        let mut command = self.namespaced();
        command.args(["patch", kind, name, "--type", "merge", "-p"]);
        command.arg(serde_json::to_string(patch).map_err(|e| runtime_error(e.to_string()))?);
        run(command, None, Duration::from_secs(45))?;
        Ok(())
    }

    pub fn delete_uid(&self, api_path: &str, uid: &str) -> Result<(), ProviderError> {
        let mut command = self.command();
        command.args(["delete", "--raw", api_path, "-f", "-"]);
        let body = serde_json::to_vec(&serde_json::json!({
            "apiVersion":"v1", "kind":"DeleteOptions",
            "preconditions":{"uid":uid}, "propagationPolicy":"Foreground"
        }))
        .map_err(|e| runtime_error(e.to_string()))?;
        run(command, Some(&body), Duration::from_secs(45))?;
        Ok(())
    }

    pub fn wait(&self, kind: &str, name: &str, condition: &str) -> Result<(), ProviderError> {
        let mut command = self.namespaced();
        command.args(["wait", kind, name, condition, "--timeout=300s"]);
        run(command, None, Duration::from_secs(315))?;
        Ok(())
    }
}

// A failed thread creation must also kill and reap the already-spawned child.
struct Guard(Option<Child>);
impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            branchyard_support::kill_group(child.id());
            branchyard_support::best_effort("reap kubectl", child.wait());
        }
    }
}

fn run(
    mut command: Command,
    input: Option<&[u8]>,
    timeout: Duration,
) -> Result<Output, ProviderError> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut guard = Guard(Some(command.spawn()?));
    let child = guard
        .0
        .as_mut()
        .ok_or_else(|| runtime_error("kubectl child was lost"))?;
    let pid = child.id();
    if let Some(input) = input {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| runtime_error("kubectl stdin was not piped"))?;
        stdin.write_all(input)?;
    }
    let (sender, receiver) = mpsc::channel();
    let thread = branchyard_support::spawn_named(
        "agent-sandbox-kubectl",
        |_| {},
        move || {
            let result = match guard.0.take() {
                Some(child) => child.wait_with_output(),
                None => Err(io::Error::other("kubectl child was lost")),
            };
            branchyard_support::best_effort("return kubectl output", sender.send(result));
        },
    )?;
    let result = receiver.recv_timeout(timeout);
    if result.is_err() {
        branchyard_support::kill_group(pid);
    }
    branchyard_support::join_reporting("kubectl output", thread);
    let output =
        result.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "kubectl timed out"))??;
    if output.status.success() {
        Ok(output)
    } else {
        // Credential plugin stderr may contain secrets. Keep it out of public diagnostics.
        Err(runtime_error(format!(
            "kubectl control operation failed ({})",
            output.status
        )))
    }
}
