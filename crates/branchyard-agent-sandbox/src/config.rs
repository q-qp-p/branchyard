use branchyard_sandbox::{ProviderError, SandboxSpec};
use serde::{Deserialize, Serialize};

/// Managed GKE runtime required for the Sandbox and its backing node.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum Runtime {
    /// GKE's gVisor user-space kernel.
    #[default]
    Gvisor,
    /// GKE's managed Kata Containers and Cloud Hypervisor microVM.
    Microvm,
}

impl Runtime {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Gvisor => "gvisor",
            Self::Microvm => "microvm",
        }
    }

    pub(crate) fn handler(self) -> &'static str {
        match self {
            Self::Gvisor => "gvisor",
            Self::Microvm => "kata-clh",
        }
    }
}

/// Network boundary applied by the setup-verified GKE Dataplane V2.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum Network {
    /// Deny workload ingress and egress, including DNS and metadata.
    #[default]
    Deny,
    /// Allow IPv4 Internet egress except GKE metadata, plus cluster DNS.
    /// Ingress and IPv6 Internet egress remain denied.
    Unrestricted,
}

/// The Cloud Storage API host a Cloud Storage FUSE sidecar reads the tool
/// store through. Always allowed when the allocation mounts a store, even
/// under [`Network::Deny`]: the sidecar runs in the pod's network namespace.
pub const STORAGE_HOST: &str = "storage.googleapis.com";

/// The Private Google Access ranges (`restricted.googleapis.com` and
/// `private.googleapis.com`), allowed as IP blocks on port 443 with the
/// store, for a subnet that routes Google APIs to them.
pub const GOOGLE_API_BLOCKS: [&str; 2] = ["199.36.153.4/30", "199.36.153.8/30"];

/// An in-cluster Service an allocation may reach: the connector gateway.
/// The `NetworkPolicy` selects the Service's pods by the label
/// `app.kubernetes.io/name=NAME` in `namespace`, on `port`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GatewayService {
    /// The namespace the Service is in.
    pub namespace: String,
    /// The Service's name, also the value of its pods' `app.kubernetes.io/name` label.
    pub name: String,
    /// The TCP port the pods listen on.
    pub port: u16,
}

/// What a [`Network::Deny`] allocation may reach besides DNS, and nothing
/// else: the gateway, and hosts by name (the model endpoint, package
/// mirrors, the gateway's own host when it is not in the cluster). Hosts
/// become a GKE `FQDNNetworkPolicy` (`networking.gke.io/v1alpha1`) on port
/// 443. With a [`StoreVolume`] the Cloud Storage API is allowed too, whatever
/// is listed here.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Egress {
    /// The connector gateway, when it runs in this cluster.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<GatewayService>,
    /// DNS names, or `*.suffix` patterns, reached on port 443.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
}

impl Egress {
    /// Whether it allows nothing.
    pub fn is_empty(&self) -> bool {
        self.gateway.is_none() && self.hosts.is_empty()
    }
}

/// A host name an `FQDNNetworkPolicy` can match: lower-case DNS labels with
/// at least one dot, or such a name led by `*.`.
fn fqdn(host: &str) -> bool {
    let name = host.strip_prefix("*.").unwrap_or(host);
    name.len() <= 253
        && name.contains('.')
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
        && !name.bytes().all(|b| b.is_ascii_digit() || b == b'.')
}

/// A Cloud Storage bucket that holds Branchyard's shared tool store, mounted
/// read-only into the allocation with the Cloud Storage FUSE CSI driver
/// (`docs/agent-sandbox-gke.md`).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct StoreVolume {
    /// The bucket, `BUCKET` or `BUCKET/PREFIX` where the store sits below a
    /// prefix.
    pub bucket: String,
    /// The absolute path the store is mounted at: the store's root on the
    /// host, so the links a turn makes into it resolve here.
    pub path: String,
}

impl StoreVolume {
    /// The bucket's name, without any prefix.
    pub(crate) fn bucket_name(&self) -> &str {
        self.bucket.split('/').next().unwrap_or_default()
    }

    /// The prefix below the bucket, if there is one.
    pub(crate) fn prefix(&self) -> Option<&str> {
        self.bucket
            .split_once('/')
            .map(|(_, prefix)| prefix.trim_matches('/'))
            .filter(|prefix| !prefix.is_empty())
    }

    /// The driver's mount options: the cache the shared tools are read
    /// through, and the prefix when the store sits below one.
    pub(crate) fn mount_options(&self) -> String {
        // A bucket keeps no file modes: give everything 755 so connector
        // launchers (`bin/<id>`) stay runnable; the mount is read-only.
        let mut options = String::from(
            "implicit-dirs,file-mode=755,dir-mode=755,file-cache:enable-parallel-downloads:true",
        );
        if let Some(prefix) = self.prefix() {
            options.push_str(",only-dir=");
            options.push_str(prefix);
        }
        options
    }

    fn validate(&self) -> Result<(), ProviderError> {
        let plain = |s: &str| {
            !s.is_empty()
                && !s.bytes().any(|b| {
                    b.is_ascii_whitespace() || b.is_ascii_control() || b == b',' || b == b'"'
                })
        };
        if !plain(self.bucket_name())
            || self.prefix().is_some_and(|p| !plain(p))
            || self.bucket.starts_with("gs://")
        {
            return Err(invalid(
                "the tool store bucket must be BUCKET or BUCKET/PREFIX",
            ));
        }
        if !self.path.starts_with('/') || self.path.len() < 2 || !plain(&self.path) {
            return Err(invalid("the tool store mount path must be absolute"));
        }
        Ok(())
    }
}

/// Operator-selected target, authenticated by the host's kubeconfig.
/// No workload-supplied PodSpec, command, kubeconfig or credential is accepted.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Config {
    /// Explicit kubeconfig context; the current context is never implicit.
    pub context: String,
    /// Namespace prepared by `tools/agent_sandbox_gke.py`.
    pub namespace: String,
    /// Required managed runtime; defaults to gVisor.
    #[serde(default)]
    pub runtime: Runtime,
    /// Enforced workload network boundary; defaults to deny.
    #[serde(default)]
    pub network: Network,
    /// The shared tool store to mount read-only, when the cluster has the
    /// Cloud Storage FUSE CSI driver and the namespace's workload identity
    /// can read the bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<StoreVolume>,
    /// What a `deny` allocation may reach besides DNS (and Cloud Storage,
    /// with a `store`); ignored under `unrestricted`.
    #[serde(default, skip_serializing_if = "Egress::is_empty")]
    pub egress: Egress,
}

impl Config {
    /// The hosts the allocation's `FQDNNetworkPolicy` lists: `egress.hosts`
    /// and, with a store, [`STORAGE_HOST`]; sorted, once each. Empty for
    /// `unrestricted`, which needs no list.
    pub fn fqdn_hosts(&self) -> Vec<String> {
        if self.network != Network::Deny {
            return Vec::new();
        }
        let mut hosts: Vec<String> = self.egress.hosts.clone();
        if self.store.is_some() {
            hosts.push(STORAGE_HOST.to_owned());
        }
        hosts.sort();
        hosts.dedup();
        hosts
    }

    /// Whether the allocation has a `NetworkPolicy` of its own: the open
    /// one under `unrestricted`, and under `deny` the one that lets through
    /// DNS, the gateway and Cloud Storage, when there is anything to let through.
    pub(crate) fn has_policy(&self) -> bool {
        self.network == Network::Unrestricted
            || self.egress.gateway.is_some()
            || self.has_fqdn_policy()
    }

    /// Whether the allocation has an `FQDNNetworkPolicy`.
    pub(crate) fn has_fqdn_policy(&self) -> bool {
        !self.fqdn_hosts().is_empty()
    }

    /// Validate the target without invoking kubectl or touching local state.
    pub fn validate(&self) -> Result<(), ProviderError> {
        if self.context.is_empty()
            || self.context.len() > 1024
            || self.context.bytes().any(|b| b.is_ascii_control())
        {
            return Err(invalid("an explicit kubeconfig context is required"));
        }
        if !dns_label(&self.namespace) {
            return Err(invalid("the namespace must be a DNS label"));
        }
        if let Some(store) = &self.store {
            store.validate()?;
        }
        if let Some(gateway) = &self.egress.gateway {
            if !dns_label(&gateway.namespace) || !dns_label(&gateway.name) || gateway.port == 0 {
                return Err(invalid(
                    "the gateway Service needs a namespace, a name (DNS labels) and a port",
                ));
            }
        }
        if self.egress.hosts.len() > 32 {
            return Err(invalid("at most 32 egress hosts are allowed"));
        }
        if let Some(bad) = self.egress.hosts.iter().find(|host| !fqdn(host)) {
            return Err(invalid(&format!(
                "egress host {bad:?} must be a lower-case DNS name or *.suffix, without a scheme, port or path"
            )));
        }
        Ok(())
    }

    /// Refuse an unsupported allocation before creating runtime resources.
    /// The image must be an immutable OCI `repository@sha256:digest` reference.
    /// Unset limits select one CPU and 1024 MiB, with requests equal to limits.
    pub fn validate_spec(&self, spec: &SandboxSpec) -> Result<(), ProviderError> {
        self.validate()?;
        spec.validate()?;
        if !spec.mounts.is_empty() {
            return Err(invalid(
                "Agent Sandbox does not accept host mounts; use copied placement",
            ));
        }
        let image = spec
            .image
            .as_deref()
            .ok_or_else(|| invalid("a digest-pinned image is required"))?;
        let Some((repository, digest)) = image.rsplit_once("@sha256:") else {
            return Err(invalid(
                "the image must use repository@sha256:<64 hex digits>",
            ));
        };
        if repository.is_empty()
            || repository.starts_with('-')
            || repository
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
            || digest.len() != 64
            || !digest
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(invalid("invalid digest-pinned image reference"));
        }
        if spec.resources.cpus == Some(0) || spec.resources.memory_mib == Some(0) {
            return Err(invalid("CPU and memory limits must be positive"));
        }
        Ok(())
    }
}

pub(crate) fn dns_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value.as_bytes()[0].is_ascii_lowercase()
        && value.as_bytes()[value.len() - 1].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub(crate) fn invalid(reason: &str) -> ProviderError {
    ProviderError::Invalid(reason.into())
}

pub(crate) fn runtime_error(reason: impl Into<String>) -> ProviderError {
    ProviderError::Runtime(reason.into())
}
