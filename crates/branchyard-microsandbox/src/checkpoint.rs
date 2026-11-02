//! Verify the reviewed runtime and bind opaque references to its memory layout.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use branchyard_sandbox::ProviderError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const RECOVERY: &str = "use the original runtime to read or export the old checkpoint, or create a fresh checkpoint with the installed runtime; the saved checkpoint is unchanged";

/// The same reviewed source, patch, payload and layout pins used by the runtime builder.
pub(crate) const REVIEWED_RUNTIME: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tools/microsandbox-runtime-build.json"
));

#[derive(Deserialize, Eq, PartialEq)]
struct ReviewedRuntime {
    format: u32,
    runtime_version: String,
    revision: String,
    platform: String,
    upstream_repository: String,
    upstream_revision: String,
    architecture_revision: String,
    architecture_tree_sha256: String,
    firmware_member: String,
    upstream_archive_sha256: String,
    agentd_sha256: String,
    inputs: BTreeMap<String, String>,
    zstd_tree_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Identity {
    runtime_sha256: String,
    firmware_sha256: String,
    architecture_tree_sha256: String,
}

#[derive(Deserialize)]
struct Provenance {
    #[serde(flatten)]
    reviewed: ReviewedRuntime,
    runtime_sha256: String,
    firmware_sha256: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reference {
    format: u8,
    artifact: String,
    runtime: Identity,
}

fn invalid(reason: &str) -> ProviderError {
    ProviderError::Invalid(format!("Microsandbox checkpoint {reason}; {RECOVERY}"))
}

fn runtime_invalid(reason: &str) -> ProviderError {
    ProviderError::Invalid(format!(
        "Microsandbox runtime {reason}; install the reviewed Branchyard runtime with matching provenance; see docs/microsandbox-runtime.md"
    ))
}

fn digest(path: &Path) -> Result<String, ProviderError> {
    let mut input = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 32 * 1024];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            return Ok(hex::encode(digest.finalize()));
        }
        digest.update(&buffer[..read]);
    }
}

fn identify(binary: &Path, firmware: &Path) -> Result<Identity, ProviderError> {
    let binary = binary.canonicalize()?;
    let parent = binary
        .parent()
        .ok_or_else(|| runtime_invalid("has no parent directory"))?;
    let provenance = std::fs::read(parent.join("runtime-provenance.json")).map_err(|_| {
        runtime_invalid("requires runtime-provenance.json beside the resolved msb binary")
    })?;
    let provenance: Provenance = serde_json::from_slice(&provenance)
        .map_err(|_| runtime_invalid("provenance is malformed"))?;
    let reviewed: ReviewedRuntime = serde_json::from_str(REVIEWED_RUNTIME)
        .map_err(|_| runtime_invalid("reviewed build plan is malformed"))?;
    if provenance.reviewed != reviewed || reviewed.runtime_version != crate::SDK_VERSION {
        return Err(runtime_invalid(
            "does not match the reviewed patched layout and source pins",
        ));
    }
    let runtime_sha256 = digest(&binary)?;
    let firmware_sha256 = digest(firmware)?;
    if provenance.runtime_sha256 != runtime_sha256 || provenance.firmware_sha256 != firmware_sha256
    {
        return Err(runtime_invalid(
            "binary or firmware differs from its provenance",
        ));
    }
    Ok(Identity {
        runtime_sha256,
        firmware_sha256,
        architecture_tree_sha256: reviewed.architecture_tree_sha256,
    })
}

pub(crate) fn current() -> Result<Identity, ProviderError> {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return Err(runtime_invalid("is reviewed for Linux x86_64 only"));
    }
    let config = microsandbox::config::config().map_err(crate::provider::error)?;
    let runtime = microsandbox::setup::resolve_runtime(&config).map_err(crate::provider::error)?;
    identify(&runtime.msb_path, &runtime.libkrunfw_path)
}

impl Reference {
    pub(crate) fn capture(artifact: String, runtime: Identity) -> Self {
        Self {
            format: 1,
            artifact,
            runtime,
        }
    }

    pub(crate) fn parse(reference: &str) -> Result<Self, ProviderError> {
        let reference: Self = serde_json::from_str(reference).map_err(|_| {
            invalid("reference is legacy or malformed and has no verified runtime identity")
        })?;
        if reference.format != 1 || !Path::new(&reference.artifact).is_absolute() {
            return Err(invalid(
                "reference has an unsupported format or artifact path",
            ));
        }
        Ok(reference)
    }

    pub(crate) fn encode(&self) -> Result<String, ProviderError> {
        serde_json::to_string(self).map_err(|error| ProviderError::Invalid(error.to_string()))
    }

    pub(crate) fn compatible_artifact(&self) -> Result<&str, ProviderError> {
        self.verify(&current()?)
    }

    fn verify(&self, current: &Identity) -> Result<&str, ProviderError> {
        if &self.runtime != current {
            return Err(invalid(
                "runtime or memory layout differs from the saved reference",
            ));
        }
        Ok(&self.artifact)
    }
}

#[allow(clippy::unwrap_used)] // tests: a panic is the failure report
#[cfg(test)]
mod tests {
    use super::*;
    use branchyard_testkit::Scratch;

    fn fixture() -> (Scratch, Identity) {
        let scratch = Scratch::new("msb-checkpoint-runtime");
        std::fs::write(scratch.join("msb"), b"runtime executable").unwrap();
        std::fs::write(scratch.join("firmware"), b"firmware").unwrap();
        let mut provenance: serde_json::Value = serde_json::from_str(REVIEWED_RUNTIME).unwrap();
        provenance["runtime_sha256"] = digest(&scratch.join("msb")).unwrap().into();
        provenance["firmware_sha256"] = digest(&scratch.join("firmware")).unwrap().into();
        std::fs::write(
            scratch.join("runtime-provenance.json"),
            provenance.to_string(),
        )
        .unwrap();
        let identity = identify(&scratch.join("msb"), &scratch.join("firmware")).unwrap();
        (scratch, identity)
    }

    #[test]
    fn opaque_reference_round_trip_retains_the_actual_artifact() {
        let (_scratch, runtime) = fixture();
        let reference = Reference::capture("/snapshots/disk".into(), runtime.clone());
        let parsed = Reference::parse(&reference.encode().unwrap()).unwrap();
        assert_eq!(parsed.verify(&runtime).unwrap(), "/snapshots/disk");
    }

    #[test]
    fn every_changed_runtime_component_refuses_without_changing_the_saved_artifact() {
        let (scratch, runtime) = fixture();
        let artifact = scratch.join("checkpoint");
        std::fs::write(&artifact, b"keep this saved checkpoint").unwrap();
        let reference = Reference {
            format: 1,
            artifact: artifact.to_string_lossy().into(),
            runtime: runtime.clone(),
        };
        for component in [0, 1, 2] {
            let mut changed = runtime.clone();
            match component {
                0 => changed.runtime_sha256 = "different binary".into(),
                1 => changed.firmware_sha256 = "different firmware".into(),
                _ => changed.architecture_tree_sha256 = "different memory layout".into(),
            }
            let error = reference.verify(&changed).unwrap_err().to_string();
            assert!(error.contains("original runtime"), "{error}");
            assert_eq!(
                std::fs::read(&artifact).unwrap(),
                b"keep this saved checkpoint"
            );
        }
    }

    #[test]
    fn legacy_and_unknown_formats_are_never_passed_to_the_sdk() {
        for value in ["/old/home/absolute-checkpoint", "old-label", "{}"] {
            let error = Reference::parse(value).unwrap_err().to_string();
            assert!(error.contains("legacy or malformed"), "{error}");
        }
        let (_scratch, runtime) = fixture();
        for (format, artifact) in [(2, "/absolute"), (1, "relative")] {
            let reference = Reference {
                format,
                artifact: artifact.into(),
                runtime: runtime.clone(),
            };
            assert!(Reference::parse(&reference.encode().unwrap()).is_err());
        }
    }

    #[test]
    fn missing_or_unbound_provenance_cannot_stamp_a_checkpoint() {
        let (scratch, _) = fixture();
        std::fs::write(scratch.join("msb"), b"different runtime executable").unwrap();
        assert!(identify(&scratch.join("msb"), &scratch.join("firmware")).is_err());
        std::fs::remove_file(scratch.join("runtime-provenance.json")).unwrap();
        let error = identify(&scratch.join("msb"), &scratch.join("firmware"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("runtime-provenance.json"), "{error}");
    }

    #[test]
    fn unreviewed_layout_or_changed_firmware_is_not_an_approved_runtime() {
        let (scratch, _) = fixture();
        let path = scratch.join("runtime-provenance.json");
        let original = std::fs::read(&path).unwrap();
        let mut provenance: serde_json::Value = serde_json::from_slice(&original).unwrap();
        provenance["architecture_tree_sha256"] = "b".repeat(64).into();
        std::fs::write(&path, provenance.to_string()).unwrap();
        let error = identify(&scratch.join("msb"), &scratch.join("firmware")).unwrap_err();
        assert!(
            error.to_string().contains("reviewed patched layout"),
            "{error}"
        );
        std::fs::write(&path, original).unwrap();
        std::fs::write(scratch.join("firmware"), b"changed firmware").unwrap();
        let error = identify(&scratch.join("msb"), &scratch.join("firmware")).unwrap_err();
        assert!(error.to_string().contains("firmware differs"), "{error}");
    }
}
