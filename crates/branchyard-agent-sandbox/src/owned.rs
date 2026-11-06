use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use branchyard_sandbox::{ProviderError, SandboxSpec};
use serde::{Deserialize, Serialize};

use crate::config::{invalid, Config};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Receipt {
    pub version: u8,
    pub name: String,
    pub target: Config,
    pub namespace_uid: String,
    pub runtime_class_uid: String,
    pub owner: String,
    pub digest: String,
    pub image: String,
    pub cpus: u8,
    pub memory_mib: u32,
    pub sandbox_uid: Option<String>,
    pub pod_uid: Option<String>,
    pub policy_uid: Option<String>,
    #[serde(default)]
    pub fqdn_policy_uid: Option<String>,
    pub node: Option<NodeEvidence>,
    /// Set when the allocation was claimed from the warm pool instead of
    /// created (`docs/agent-sandbox-gke.md#warm-pool`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pooled: Option<Pooled>,
}

/// What a claim on the warm pool made of an allocation: the objects it
/// went through, and the names of the Sandbox and Pod it ended up with,
/// which the pool, not Branchyard, named.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Pooled {
    /// The `SandboxTemplate` the claim named.
    pub template: String,
    /// The claim's UID, once it was read back.
    pub claim_uid: Option<String>,
    /// The Sandbox the claim resolved to (`status.sandbox.name`), when it
    /// is not named like the claim.
    pub sandbox: Option<String>,
    /// The warm Pod the Sandbox adopted.
    pub pod: Option<String>,
    /// The Sandbox and Pod were verified and marked as this allocation's.
    pub bound: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct NodeEvidence {
    pub name: String,
    pub uid: String,
    pub kubelet_version: String,
}

impl Receipt {
    pub fn intent(
        config: &Config,
        spec: &SandboxSpec,
        namespace_uid: String,
        runtime_class_uid: String,
    ) -> Result<Self, ProviderError> {
        config.validate_spec(spec)?;
        let image = spec
            .image
            .clone()
            .ok_or_else(|| invalid("an image is required"))?;
        let cpus = spec.resources.cpus.unwrap_or(1);
        let memory_mib = spec.resources.memory_mib.unwrap_or(1024);
        // The pool's size is not part of what an allocation is: resizing a
        // pool must not make a new template, nor tell a fresh Sandbox from
        // a claimed one.
        let identity = Config {
            warm_pool: None,
            ..config.clone()
        };
        let body = serde_json::to_vec(&(&identity, &image, cpus, memory_mib))
            .map_err(|e| invalid(&e.to_string()))?;
        let mut nonce = [0u8; 16];
        branchyard_support::rng::fill_random(&mut nonce).map_err(io::Error::from)?;
        Ok(Self {
            version: 1,
            name: spec.name.clone(),
            target: config.clone(),
            namespace_uid,
            runtime_class_uid,
            owner: hex::encode(nonce),
            digest: blake3::hash(&body).to_hex().to_string(),
            image,
            cpus,
            memory_mib,
            sandbox_uid: None,
            pod_uid: None,
            policy_uid: None,
            fqdn_policy_uid: None,
            node: None,
            pooled: None,
        })
    }

    /// The name of the allocation's own objects: its claim, its network
    /// policies, and (unless claimed from the pool) its Sandbox and Pod.
    pub fn resource_name(&self) -> String {
        resource_name(&self.name)
    }

    /// The allocation's Sandbox: named by Branchyard, or by the pool when
    /// claimed.
    pub fn sandbox_name(&self) -> String {
        match self.pooled.as_ref().and_then(|p| p.sandbox.clone()) {
            Some(name) => name,
            None => self.resource_name(),
        }
    }

    /// The allocation's Pod: named like the Sandbox, or by the pool when
    /// claimed.
    pub fn pod_name(&self) -> String {
        match self.pooled.as_ref().and_then(|p| p.pod.clone()) {
            Some(name) => name,
            None => self.resource_name(),
        }
    }

    /// The template and pool a claim for this allocation goes through:
    /// named by the specification digest, so every allocation of one
    /// specification shares one pool.
    pub fn pool_id(&self) -> &str {
        &self.digest[..32]
    }

    pub fn validate_target(&self, config: &Config, name: &str) -> Result<(), ProviderError> {
        if self.version != 1
            || self.name != name
            || self.target != *config
            || self.owner.len() != 32
            || !self.owner.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(invalid(
                "receipt target or ownership does not match this provider",
            ));
        }
        Ok(())
    }

    pub fn validate_spec(&self, spec: &SandboxSpec) -> Result<(), ProviderError> {
        self.target.validate_spec(spec)?;
        if self.image != spec.image.as_deref().unwrap_or_default()
            || self.cpus != spec.resources.cpus.unwrap_or(1)
            || self.memory_mib != spec.resources.memory_mib.unwrap_or(1024)
        {
            return Err(invalid(
                "an existing Agent Sandbox has a different image or resources",
            ));
        }
        Ok(())
    }
}

pub(crate) fn resource_name(name: &str) -> String {
    format!("by-{}", &blake3::hash(name.as_bytes()).to_hex()[..40])
}

pub(crate) struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn lock(&self, name: &str) -> Result<Locked, ProviderError> {
        match fs::symlink_metadata(&self.dir) {
            Ok(metadata) => {
                if !metadata.is_dir() {
                    return Err(invalid("the receipt directory is not a real directory"));
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if metadata.permissions().mode() & 0o077 != 0 {
                        return Err(invalid("the receipt directory must be private (mode 0700)"));
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut builder = fs::DirBuilder::new();
                builder.recursive(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                builder.create(&self.dir)?;
            }
            Err(error) => return Err(error.into()),
        }
        let base = resource_name(name);
        let lock_path = self.dir.join(format!("{base}.lock"));
        no_symlink(&lock_path)?;
        let file = private_options()
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        file.lock()?;
        Ok(Locked {
            file,
            path: self.dir.join(format!("{base}.json")),
        })
    }
}

pub(crate) struct Locked {
    // File locks are released by drop and are close-on-exec.
    file: File,
    path: PathBuf,
}

impl Locked {
    pub fn read(&self) -> Result<Option<Receipt>, ProviderError> {
        no_symlink(&self.path)?;
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| invalid("the private Agent Sandbox receipt is corrupt")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn write(&self, receipt: &Receipt) -> Result<(), ProviderError> {
        let bytes = serde_json::to_vec(receipt).map_err(|e| invalid(&e.to_string()))?;
        let temporary = self.path.with_extension("pending");
        no_symlink(&temporary)?;
        let mut file = private_options()
            .create(true)
            .truncate(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, &self.path)?;
        self.sync_dir()?;
        Ok(())
    }

    pub fn remove(&self) -> Result<(), ProviderError> {
        match fs::remove_file(&self.path) {
            Ok(()) => self.sync_dir(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn sync_dir(&self) -> Result<(), ProviderError> {
        if let Some(parent) = self.path.parent() {
            File::open(parent)?.sync_all()?;
        }
        Ok(())
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        branchyard_support::best_effort("unlock Agent Sandbox receipt", self.file.unlock());
    }
}

fn no_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "receipt path is not a regular file",
            ))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}
