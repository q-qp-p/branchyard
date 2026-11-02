use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use branchyard_sandbox::ProviderError;
use serde::de::DeserializeOwned;

pub(crate) const TIMEOUT: Duration = Duration::from_secs(30);
const OUTPUT_LIMIT: u64 = 1024 * 1024;

#[derive(Clone)]
pub(crate) struct Docker {
    pub binary: PathBuf,
}

impl Docker {
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        // Bind to the worker's local daemon, never the caller's current context.
        command.args(["--host", "unix:///var/run/docker.sock"]);
        for name in [
            "DOCKER_HOST",
            "DOCKER_CONTEXT",
            "DOCKER_TLS_VERIFY",
            "DOCKER_CERT_PATH",
        ] {
            command.env_remove(name);
        }
        command
    }

    pub fn run(&self, args: &[&str]) -> Result<String, ProviderError> {
        bounded(self.command().args(args), TIMEOUT)
    }

    pub fn json<T: DeserializeOwned>(&self, args: &[&str]) -> Result<T, ProviderError> {
        serde_json::from_str(&self.run(args)?)
            .map_err(|_| ProviderError::Runtime("Docker returned invalid inspection JSON".into()))
    }
}

pub(crate) fn bounded(command: &mut Command, timeout: Duration) -> Result<String, ProviderError> {
    use std::os::unix::process::CommandExt;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Docker diagnostics may contain operator credentials or registry URLs.
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    let pipe = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing Docker stdout"))?;
    let (send, receive) = mpsc::sync_channel(1);
    let reader = std::thread::Builder::new()
        .name("container-inspect".into())
        .spawn(move || {
            let mut bytes = Vec::new();
            let result = pipe
                .take(OUTPUT_LIMIT + 1)
                .read_to_end(&mut bytes)
                .map(|_| bytes);
            // A timed-out caller no longer needs the inspection output.
            branchyard_support::best_effort("deliver Docker output", send.send(result));
        });
    let reader = match reader {
        Ok(reader) => reader,
        Err(error) => {
            branchyard_support::kill_group(child.id());
            branchyard_support::best_effort("reap Docker command", child.wait());
            return Err(error.into());
        }
    };
    let deadline = Instant::now() + timeout;
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                break match receive.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                {
                    Ok(Ok(bytes)) if status.success() && bytes.len() <= OUTPUT_LIMIT as usize => {
                        String::from_utf8(bytes).map_err(|_| {
                            ProviderError::Runtime("Docker returned non-UTF-8 output".into())
                        })
                    }
                    Ok(_) => Err(ProviderError::Runtime(
                        "Docker command failed or exceeded its output limit".into(),
                    )),
                    Err(_) => Err(ProviderError::Runtime(
                        "Docker command output timed out".into(),
                    )),
                };
            }
            Err(error) => break Err(error.into()),
            Ok(None) if Instant::now() >= deadline => {
                break Err(ProviderError::Runtime("Docker command timed out".into()))
            }
            Ok(None) => std::thread::park_timeout(Duration::from_millis(10)),
        }
    };
    if result.is_err() {
        branchyard_support::kill_group(child.id());
        branchyard_support::best_effort("reap failed Docker command", child.wait());
    }
    if reader.is_finished() {
        branchyard_support::join_reporting("Docker output", reader);
    }
    result
}
