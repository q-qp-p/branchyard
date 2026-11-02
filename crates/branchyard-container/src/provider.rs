use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use branchyard_sandbox::exec::{self, ExecGroup, ExecTransport};
use branchyard_sandbox::{
    Capabilities, ExecSpec, Process, ProviderError, SandboxInfo, SandboxProvider, SandboxSpec,
    SandboxState,
};
use serde_json::{json, Value};

use crate::docker::{Docker, TIMEOUT};
use crate::receipt::{Locked, Receipt};
use crate::verify::{digest_image, hex_id, object, refused, verify, verify_identity};
use crate::Config;

/// Check image pinning and reject guarantees this copied-files provider cannot honor.
/// Unspecified limits use one CPU and 1024 MiB; both explicit limits must be positive.
pub fn validate_spec(spec: &SandboxSpec) -> Result<(), ProviderError> {
    spec.validate()?;
    ExecGroup::new(&spec.name)?;
    if !spec.image.as_deref().is_some_and(digest_image) {
        return Err(ProviderError::Invalid(
            "container image must be pinned with @sha256 and a lowercase SHA-256 digest".into(),
        ));
    }
    if !spec.mounts.is_empty() || spec.persist {
        return Err(ProviderError::Invalid(
            "container sandboxes copy files and do not support host mounts or retained allocations"
                .into(),
        ));
    }
    if spec.resources.cpus == Some(0) || spec.resources.memory_mib == Some(0) {
        return Err(ProviderError::Invalid(
            "container CPU and memory limits must be positive".into(),
        ));
    }
    Ok(())
}

/// Owned, per-turn containers on the worker's local Docker daemon.
/// Receipts bind names to daemon identity, immutable container IDs and private volumes.
pub struct ContainerProvider {
    config: Config,
    state_dir: PathBuf,
    docker: Docker,
}

impl ContainerProvider {
    /// Open private ownership state without contacting Docker or creating a container.
    pub fn new(config: Config, state_dir: impl Into<PathBuf>) -> Result<Self, ProviderError> {
        let state_dir = state_dir.into();
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&state_dir)?;
        Ok(Self {
            config,
            state_dir,
            docker: Docker {
                binary: "docker".into(),
            },
        })
    }

    /// Choose an operator-controlled Docker executable, including a fake in tests.
    /// This setting is deliberately absent from serialized task configuration.
    pub fn with_binary(mut self, binary: impl Into<PathBuf>) -> Self {
        self.docker.binary = binary.into();
        self
    }

    fn daemon(&self) -> Result<String, ProviderError> {
        let info: Value = self.docker.json(&["info", "--format", "{{json .}}"])?;
        if info["OSType"] != "linux"
            || info["MemoryLimit"] != true
            || info["CpuCfsQuota"] != true
            || !info["Runtimes"]
                .get(self.config.runtime.docker_name())
                .is_some_and(Value::is_object)
        {
            return Err(refused(
                "daemon lacks the selected runtime or enforced resource limits",
            ));
        }
        info["ID"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| refused("daemon has no identity"))
    }

    fn receipt(
        &self,
        locked: &Locked,
        name: &str,
        daemon: &str,
    ) -> Result<Option<Receipt>, ProviderError> {
        let receipt = locked.read()?;
        if let Some(receipt) = &receipt {
            if receipt.version != 1
                || receipt.name != name
                || receipt.daemon != daemon
                || receipt.config != self.config
            {
                return Err(refused(
                    "saved allocation belongs to another name, daemon or configuration",
                ));
            }
        }
        Ok(receipt)
    }

    fn lookup(&self, receipt: &Receipt) -> Result<Option<Value>, ProviderError> {
        // Exact generated name finds a creation that finished before its ID was persisted.
        let filter = format!("name=^/{}$", receipt.container_name());
        let ids = self.docker.run(&[
            "container",
            "ls",
            "--all",
            "--quiet",
            "--no-trunc",
            "--filter",
            &filter,
        ])?;
        let ids: Vec<_> = ids.split_whitespace().collect();
        match ids.as_slice() {
            [] => Ok(None),
            [id] if hex_id(id) => {
                let found = object(self.docker.json(&["container", "inspect", id])?)?;
                verify_identity(&found, receipt)?;
                Ok(Some(found))
            }
            _ => Err(refused("container lookup returned an ambiguous identity")),
        }
    }

    fn intent(&self, spec: &SandboxSpec, daemon: String) -> Result<Receipt, ProviderError> {
        let image = spec
            .image
            .as_deref()
            .ok_or_else(|| refused("missing image"))?;
        let actual = object(self.docker.json(&["image", "inspect", image])?)?;
        let image_id = actual["Id"]
            .as_str()
            .filter(|id| id.strip_prefix("sha256:").is_some_and(hex_id))
            .ok_or_else(|| refused("image has no immutable identity"))?;
        let digest = image
            .rsplit_once('@')
            .map(|(_, digest)| digest)
            .ok_or_else(|| refused("unpinned image"))?;
        if !actual["RepoDigests"].as_array().is_some_and(|digests| {
            digests.iter().any(|value| {
                value.as_str().is_some_and(|value| {
                    value
                        .rsplit_once('@')
                        .is_some_and(|(_, found)| found == digest)
                })
            })
        }) {
            return Err(refused("image does not have the requested registry digest"));
        }
        let cpus = spec.resources.cpus.unwrap_or(1);
        let memory = spec.resources.memory_mib.unwrap_or(1024);
        let bytes = serde_json::to_vec(&json!({"image":image,"image_id":image_id,"cpus":cpus,"memory":memory,"config":self.config}))
            .map_err(io::Error::other)?;
        Ok(Receipt {
            version: 1,
            name: spec.name.clone(),
            owner: branchyard_support::new_ulid().map_err(io::Error::from)?,
            daemon,
            digest: blake3::hash(&bytes).to_hex().to_string(),
            image: image.into(),
            image_id: image_id.into(),
            cpus,
            memory,
            config: self.config.clone(),
            id: None,
            volumes: Vec::new(),
        })
    }

    fn create(&self, receipt: &Receipt) -> Result<(), ProviderError> {
        let memory = (u64::from(receipt.memory) * 1024 * 1024).to_string();
        self.docker.run(&[
            "container",
            "create",
            "--name",
            &receipt.container_name(),
            "--label",
            &format!("io.branchyard.owner={}", receipt.owner),
            "--label",
            &format!("io.branchyard.spec={}", receipt.digest),
            "--runtime",
            self.config.runtime.docker_name(),
            "--network",
            self.config.network.docker_name(),
            "--user",
            "1000:1000",
            "--read-only",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges:true",
            "--ipc",
            "private",
            "--cgroupns",
            "private",
            "--pids-limit",
            "256",
            "--cpus",
            &receipt.cpus.to_string(),
            "--memory",
            &memory,
            "--memory-swap",
            &memory,
            "--mount",
            "type=volume,destination=/workspace",
            "--mount",
            "type=volume,destination=/branchyard/home",
            "--tmpfs",
            "/tmp:rw,nosuid,nodev,noexec,size=67108864,mode=1777",
            "--env",
            "HOME=/branchyard/home",
            "--workdir",
            "/workspace",
            "--pull",
            "never",
            "--entrypoint",
            "sh",
            &receipt.image,
            "-c",
            "exec tail -f /dev/null",
        ])?;
        Ok(())
    }

    fn ready(&self, receipt: &Receipt) -> Result<(), ProviderError> {
        let id = receipt
            .id
            .as_deref()
            .ok_or_else(|| refused("allocation has no receipt"))?;
        self.docker.run(&["container", "exec", "--user", "1000:1000", "--env", "HOME=/branchyard/home", id,
            "sh", "-c", "test -w /workspace && test -w /branchyard/home && test \"$(id -u)\" = 1000 && command -v tar >/dev/null && command -v setsid >/dev/null"])?;
        Ok(())
    }

    fn volumes_gone(&self, receipt: &Receipt) -> Result<(), ProviderError> {
        for volume in &receipt.volumes {
            let filter = format!("name=^{volume}$");
            if !self
                .docker
                .run(&["volume", "ls", "--quiet", "--filter", &filter])?
                .trim()
                .is_empty()
            {
                return Err(refused(
                    "owned volume cleanup is incomplete; receipt retained",
                ));
            }
        }
        Ok(())
    }
}

fn info(name: &str, value: &Value) -> SandboxInfo {
    SandboxInfo {
        name: name.into(),
        state: if value["State"]["Running"] == true {
            SandboxState::Running
        } else {
            SandboxState::Stopped
        },
    }
}

impl SandboxProvider for ContainerProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            exec: true,
            ..Capabilities::default()
        }
    }

    fn ensure(&self, spec: &SandboxSpec) -> Result<SandboxInfo, ProviderError> {
        validate_spec(spec)?;
        let locked = Locked::open(&self.state_dir, &spec.name)?;
        let daemon = self.daemon()?;
        let mut receipt = match self.receipt(&locked, &spec.name, &daemon)? {
            Some(receipt) => {
                if receipt.image != spec.image.as_deref().unwrap_or_default()
                    || receipt.cpus != spec.resources.cpus.unwrap_or(1)
                    || receipt.memory != spec.resources.memory_mib.unwrap_or(1024)
                {
                    return Err(refused("existing allocation has a different spec"));
                }
                receipt
            }
            None => {
                let receipt = self.intent(spec, daemon)?;
                locked.write(&receipt)?;
                receipt
            }
        };
        let found = match self.lookup(&receipt)? {
            Some(found) => found,
            None if receipt.id.is_some() => {
                return Err(refused(
                    "saved container disappeared; destroy its receipt before recreating",
                ))
            }
            None => {
                self.create(&receipt)?;
                self.lookup(&receipt)?
                    .ok_or_else(|| refused("created container was not found"))?
            }
        };
        receipt.volumes = verify(&found, &receipt)?;
        receipt.id = Some(verify_identity(&found, &receipt)?);
        locked.write(&receipt)?;
        let id = receipt
            .id
            .as_deref()
            .ok_or_else(|| refused("missing receipt ID"))?;
        if found["State"]["Running"] != true {
            self.docker.run(&["container", "start", id])?;
        }
        let running = self
            .lookup(&receipt)?
            .ok_or_else(|| refused("container disappeared after start"))?;
        verify(&running, &receipt)?;
        if running["State"]["Running"] != true {
            return Err(refused("container did not remain running"));
        }
        self.ready(&receipt)?;
        Ok(info(&spec.name, &running))
    }

    fn inspect(&self, name: &str) -> Result<Option<SandboxInfo>, ProviderError> {
        let locked = Locked::open(&self.state_dir, name)?;
        let Some(receipt) = self.receipt(&locked, name, &self.daemon()?)? else {
            return Ok(None);
        };
        self.lookup(&receipt)?
            .map(|value| {
                verify(&value, &receipt)?;
                Ok(info(name, &value))
            })
            .transpose()
    }

    fn exec(&self, name: &str, spec: &ExecSpec) -> Result<Box<dyn Process>, ProviderError> {
        spec.refuse_write_scope()?;
        let locked = Locked::open(&self.state_dir, name)?;
        let Some(receipt) = self.receipt(&locked, name, &self.daemon()?)? else {
            return Err(ProviderError::NotFound(name.into()));
        };
        let found = self
            .lookup(&receipt)?
            .ok_or_else(|| ProviderError::NotFound(name.into()))?;
        verify(&found, &receipt)?;
        if found["State"]["Running"] != true {
            return Err(refused("container is stopped"));
        }
        let id = receipt
            .id
            .ok_or_else(|| refused("creation receipt has not been committed"))?;
        let target = Arc::new(Target {
            docker: self.docker.clone(),
            id,
        });
        exec::spawn(target, &ExecGroup::new(name)?, spec)
    }

    fn stop(&self, name: &str) -> Result<(), ProviderError> {
        let locked = Locked::open(&self.state_dir, name)?;
        let Some(receipt) = self.receipt(&locked, name, &self.daemon()?)? else {
            return Ok(());
        };
        if let Some(found) = self.lookup(&receipt)? {
            let id = verify_identity(&found, &receipt)?;
            if found["State"]["Running"] == true {
                self.docker
                    .run(&["container", "stop", "--time", "0", &id])?;
            }
            if self
                .lookup(&receipt)?
                .is_some_and(|found| found["State"]["Running"] == true)
            {
                return Err(refused("container remains running after stop"));
            }
        }
        Ok(())
    }

    fn destroy(&self, name: &str) -> Result<(), ProviderError> {
        let locked = Locked::open(&self.state_dir, name)?;
        let Some(mut receipt) = self.receipt(&locked, name, &self.daemon()?)? else {
            return Ok(());
        };
        if let Some(found) = self.lookup(&receipt)? {
            let id = verify_identity(&found, &receipt)?;
            // A crash before receipt commit may have left an allocation to reclaim.
            receipt.volumes = verify(&found, &receipt)?;
            receipt.id = Some(id.clone());
            locked.write(&receipt)?;
            self.docker
                .run(&["container", "rm", "--force", "--volumes", &id])?;
            if self.lookup(&receipt)?.is_some() {
                return Err(refused("container remains after deletion"));
            }
        }
        self.volumes_gone(&receipt)?;
        locked.remove()
    }
}

struct Target {
    docker: Docker,
    id: String,
}

impl ExecTransport for Target {
    fn command(&self, script: &str, args: &[&str]) -> Command {
        let mut command = self.docker.command();
        command
            .args([
                "container",
                "exec",
                "--interactive",
                "--user",
                "1000:1000",
                "--env",
                "HOME=/branchyard/home",
                &self.id,
                "sh",
                "-c",
                script,
                "sh",
            ])
            .args(args);
        command
    }
    fn describe(&self) -> String {
        "owned container".into()
    }
    fn start_timeout(&self) -> std::time::Duration {
        TIMEOUT
    }
}
