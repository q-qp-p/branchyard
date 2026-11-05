//! `by credential-proxy`: the credential proxy a sandbox runs beside its
//! harness (`branchyard::connectors::sandbox_proxy`). It needs no repository
//! and reads no configuration: everything comes from its command line, a
//! settings file (which holds the turn's secret, for a sidecar), and
//! `BRANCHYARD_CREDENTIAL_SECRET`.

use std::path::Path;
use std::time::Duration;

use branchyard::connectors::credential::{parse_config, PROXY_CONFIG_FILE};
use branchyard::connectors::sandbox_proxy::{self, IdentityKind, ProxyConfig, ENV_SECRET};

use crate::args::Command;
use crate::commands::{Failure, Outcome};
use crate::errors::Report;

/// How often `--wait` looks for the settings file.
const LOOK: Duration = Duration::from_millis(250);

/// Run until killed, or fail with the reason the proxy could not start.
pub(crate) fn run(command: &Command) -> Outcome {
    let Command::CredentialProxy {
        config,
        wait,
        listen,
        server,
        upstream,
        manifest,
        branch,
        turn,
        identity,
        audience,
    } = command
    else {
        return Err(Report::new("internal", "not a credential-proxy command")
            .next("this is a bug in by; please report it")
            .into());
    };
    let settings = match config {
        Some(path) => from_file(Path::new(path), *wait)?,
        None => ProxyConfig {
            listen: listen.clone().unwrap_or_default(),
            server: server.clone().unwrap_or_default(),
            upstream: upstream.clone().unwrap_or_default(),
            manifest: manifest.clone().unwrap_or_default(),
            branch: branch.clone().unwrap_or_default(),
            turn: turn.unwrap_or_default(),
            identity: match (identity.as_deref(), audience) {
                (Some("workload"), Some(audience)) => IdentityKind::Workload {
                    audience: audience.clone(),
                },
                _ => IdentityKind::Secret,
            },
            secret: None,
        },
    };
    let secret = std::env::var(ENV_SECRET).ok();
    // A sidecar waits out a server that is not ready; a guest the engine
    // started fails at once, so the turn says why.
    sandbox_proxy::run(&settings, secret, !*wait).map_err(|why| {
        Report::new("config", format!("the credential proxy could not run: {why}"))
            .next("check the server URL, the turn's manifest and the sandbox's identity (docs/connectors.md#the-credential-proxy-in-the-sandbox)")
            .into()
    })
}

/// The settings file at `path`, waiting for it with `wait`.
fn from_file(path: &Path, wait: bool) -> Result<ProxyConfig, Failure> {
    let read = || std::fs::read_to_string(path);
    let text = loop {
        match read() {
            Ok(text) => break text,
            Err(error) if wait && error.kind() == std::io::ErrorKind::NotFound => {
                std::thread::sleep(LOOK);
            }
            Err(error) => {
                return Err(Report::new(
                    "config",
                    format!("could not read {}: {}", path.display(), crate::errors::plain_io(&error)),
                )
                .next(format!(
                    "the engine writes {PROXY_CONFIG_FILE} in the sandbox's home; pass --wait to wait for it"
                ))
                .into())
            }
        }
    };
    parse_config(&text).map_err(|why| Report::new("config", why).into())
}
