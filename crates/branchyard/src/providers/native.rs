//! Shared policy for copied, destroy-only native allocations.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::ProviderKind;
use crate::placement::{pull_staged, said_back, staging, HOME, WORKSPACE};
use crate::snapshots::Lifecycle;
use crate::state::Record;
use crate::Yard;

pub(super) fn key(kind: &str, value: Value) -> String {
    format!(
        "{kind}:{}",
        blake3::hash(value.to_string().as_bytes()).to_hex()
    )
}

pub(super) fn state_dir(yard: &Yard, key: &str) -> PathBuf {
    yard.store()
        .dir()
        .join("native")
        .join(blake3::hash(key.as_bytes()).to_hex().as_str())
}

pub(super) fn lifecycle() -> Option<Lifecycle> {
    Some(Lifecycle {
        keep: false,
        snapshots: 0,
        max_paused: 0,
        max_kept_secs: None,
    })
}

pub(super) fn destroy(
    kind: &dyn ProviderKind,
    yard: &Yard,
    sandbox: &str,
) -> Result<String, String> {
    kind.open(yard)?.destroy(sandbox).map_err(|error| {
        format!(
            "could not destroy {} sandbox {sandbox}: {error}",
            kind.name()
        )
    })?;
    Ok(format!("destroyed {} sandbox {sandbox}", kind.name()))
}

// Copy-back uses the transfer's staged base to protect concurrent host edits.
// A crashed engine cannot promise application-consistent guest files; regardless
// of pull failure, whole-allocation deletion is attempted before returning.
pub(super) fn recover(
    kind: &dyn ProviderKind,
    yard: &Yard,
    record: &Record,
    sandbox: &str,
) -> String {
    let provider = match kind.open(yard) {
        Ok(provider) => provider,
        Err(error) => {
            return format!(
                "could not open {} sandbox {sandbox} for recovery: {error}",
                kind.name()
            )
        }
    };
    let stage = (!sandbox.is_empty() && !sandbox.contains('/') && !sandbox.starts_with('.'))
        .then(|| staging(yard, sandbox));
    let mut said = Vec::new();
    match provider.inspect(sandbox) {
        Ok(Some(_)) => {
            let pulled = (|| {
                let stage = stage
                    .as_deref()
                    .ok_or("sandbox name cannot name a staging directory")?;
                let endpoint =
                    branchyard_substrate::transfer::Exec::new(provider.as_ref(), sandbox);
                pull_staged(
                    &endpoint,
                    record,
                    (Path::new(WORKSPACE), Path::new(HOME)),
                    stage,
                )
            })();
            said.push(said_back(pulled, "sandbox", sandbox));
        }
        Ok(None) => {}
        Err(error) => said.push(format!(
            "could not inspect {} sandbox {sandbox}: {error}",
            kind.name()
        )),
    }
    match provider.destroy(sandbox) {
        Ok(()) => {
            if let Some(stage) = stage {
                branchyard_support::cleanup_dir(stage);
            }
            said.push(format!("destroyed its {} sandbox {sandbox}", kind.name()));
        }
        Err(error) => said.push(format!(
            "could not destroy its {} sandbox {sandbox}: {error}",
            kind.name()
        )),
    }
    said.join("; ")
}
