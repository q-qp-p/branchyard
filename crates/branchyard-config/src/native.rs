//! Saved defaults for the native container and Kubernetes sandbox providers.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ConfigError;

/// The runtime required for a worker-local container.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContainerRuntime {
    /// gVisor's runsc runtime; unavailable runtimes fail closed.
    #[default]
    Gvisor,
    /// Ordinary Linux namespaces sharing the worker's kernel.
    Runc,
}

/// The runtime required for a Kubernetes Agent Sandbox.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentSandboxRuntime {
    /// gVisor's runsc runtime.
    #[default]
    Gvisor,
    /// The operator's Kata and Cloud Hypervisor runtime.
    Microvm,
}

/// Allocation-wide networking for native sandbox providers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SandboxNetwork {
    /// Disable external networking, including cloud metadata access.
    #[default]
    Deny,
    /// Use the provider's ordinary network boundary; Container permits cloud metadata.
    Unrestricted,
}

/// `[container]`: worker-local Docker allocation defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Container {
    /// Locally preloaded image, pinned by registry SHA-256 digest.
    pub image: String,
    /// Positive virtual CPU limit; omitted uses the provider default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub cpus: Option<u8>,
    /// Positive memory limit in MiB; also written `memory`.
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "memory")]
    #[schemars(range(min = 1))]
    pub memory_mib: Option<u32>,
    /// Required runtime; defaults to gVisor without fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<ContainerRuntime>,
    /// Allocation-wide networking; defaults to deny.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<SandboxNetwork>,
    /// Environment variable names copied into the allocation, never their values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pass_env: Vec<String>,
}

/// The most sandboxes `[agent_sandbox] warm_pool` may keep booted.
pub const MAX_WARM_POOL: u32 = 100;

/// `[agent_sandbox]`: Kubernetes Agent Sandbox allocation defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentSandbox {
    /// An existing operator-controlled kubeconfig context.
    pub context: String,
    /// The operator's sandbox namespace.
    pub namespace: String,
    /// Image with the harness installed, pinned by registry SHA-256 digest.
    pub image: String,
    /// Positive virtual CPU limit; omitted uses the provider default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub cpus: Option<u8>,
    /// Positive memory limit in MiB; also written `memory`.
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "memory")]
    #[schemars(range(min = 1))]
    pub memory_mib: Option<u32>,
    /// Required runtime; defaults to gVisor without fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<AgentSandboxRuntime>,
    /// Allocation-wide networking; defaults to deny.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<SandboxNetwork>,
    /// Environment variable names copied into the allocation, never their values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pass_env: Vec<String>,
    /// Hosts a `deny` allocation may reach by name, on port 443, besides the
    /// ones Branchyard adds itself (`[network] mirrors`, the gateway's host,
    /// Cloud Storage with a tool store): the model endpoint. Lower-case DNS
    /// names or `*.suffix`. Needs GKE's FQDN network policy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub egress_hosts: Vec<String>,
    /// The connector gateway, when it runs in the cluster: a `deny`
    /// allocation may reach its pods on its port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_service: Option<AgentSandboxGateway>,
    /// Sandboxes to keep booted ahead of time (a `SandboxWarmPool` of this
    /// many replicas for the allocation's `SandboxTemplate`), claimed by a
    /// turn when one is ready; omitted, or 0, keeps none and every turn
    /// creates its own. See `docs/agent-sandbox-gke.md#warm-pool`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 100))]
    pub warm_pool: Option<u32>,
}

/// An in-cluster Service an Agent Sandbox allocation may reach.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentSandboxGateway {
    /// The Service's namespace.
    pub namespace: String,
    /// The Service's name; its pods carry `app.kubernetes.io/name` with it.
    pub name: String,
    /// The TCP port the pods listen on.
    #[schemars(range(min = 1))]
    pub port: u16,
}

/// Validate a native provider's registry image digest without accepting a mutable tag.
pub fn validate_sandbox_image(image: &str) -> Result<(), String> {
    if image
        .rsplit_once("@sha256:")
        .is_some_and(|(repository, digest)| {
            !repository.is_empty()
                && !repository.starts_with('-')
                && !repository.chars().any(char::is_whitespace)
                && digest.len() == 64
                && digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    {
        Ok(())
    } else {
        Err("must be pinned by @sha256: followed by a lowercase SHA-256 digest".into())
    }
}

fn check(
    section: &str,
    image: &str,
    cpus: Option<u8>,
    memory: Option<u32>,
    pass_env: &[String],
) -> Result<(), ConfigError> {
    validate_sandbox_image(image).map_err(|why| ConfigError(format!("{section}.image: {why}")))?;
    if cpus == Some(0) || memory == Some(0) {
        return Err(ConfigError(format!(
            "{section}: CPU and memory limits must be positive"
        )));
    }
    for name in pass_env {
        branchyard_provision::check_variable_name(name)
            .map_err(|why| ConfigError(format!("{section}.pass_env: {why}")))?;
    }
    Ok(())
}

impl Container {
    pub(crate) fn check(&self) -> Result<(), ConfigError> {
        check(
            "container",
            &self.image,
            self.cpus,
            self.memory_mib,
            &self.pass_env,
        )
    }
}

impl AgentSandbox {
    pub(crate) fn check(&self) -> Result<(), ConfigError> {
        check(
            "agent_sandbox",
            &self.image,
            self.cpus,
            self.memory_mib,
            &self.pass_env,
        )?;
        if self.context.trim().is_empty() || self.namespace.trim().is_empty() {
            return Err(ConfigError(
                "agent_sandbox.context and namespace must not be empty".into(),
            ));
        }
        if self.warm_pool.is_some_and(|size| size > MAX_WARM_POOL) {
            return Err(ConfigError(format!(
                "agent_sandbox.warm_pool must be at most {MAX_WARM_POOL}"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse, render, ProviderKind};

    #[test]
    fn native_sections_round_trip_and_apply_safe_defaults() {
        let image = format!("example/worker@sha256:{}", "a".repeat(64));
        let text = format!("[defaults]\nprovider = 'agent_sandbox'\n[container]\nimage = '{image}'\n[agent_sandbox]\ncontext = 'validation'\nnamespace = 'by-test'\nimage = '{image}'\nmemory = 128\npass_env = ['LANG']\n");
        let config = parse(&text).unwrap();
        assert_eq!(config.defaults.provider, Some(ProviderKind::AgentSandbox));
        let container = config.container.as_ref().unwrap();
        assert_eq!(
            container.runtime.unwrap_or_default(),
            ContainerRuntime::Gvisor
        );
        assert_eq!(container.network.unwrap_or_default(), SandboxNetwork::Deny);
        assert_eq!(config.agent_sandbox.as_ref().unwrap().memory_mib, Some(128));
        let mut expected = config.clone();
        expected.version = Some(crate::VERSION);
        assert_eq!(
            parse(&render(&config, "Native defaults")).unwrap(),
            expected
        );
    }

    #[test]
    fn native_configuration_refuses_mutable_images_wrong_runtimes_and_unsupported_settings() {
        let image = format!("example/worker@sha256:{}", "a".repeat(64));
        for table in [
            "[defaults]\nprovider = 'container'".to_owned(),
            "[defaults]\nprovider = 'agent_sandbox'".to_owned(),
            "[container]\nimage = 'worker:latest'".to_owned(),
            format!("[container]\nimage = '{image}'\nruntime = 'microvm'"),
            format!("[container]\nimage = '{image}'\ncpus = 0"),
            format!("[container]\nimage = '{image}'\npass_env = ['TOKEN=value']"),
            format!("[container]\nimage = '{image}'\nkeep = 'pause'"),
            format!("[agent_sandbox]\nimage = '{image}'\ncontext = ''\nnamespace = 'test'"),
            format!("[agent_sandbox]\nimage = '{image}'\ncontext = 'test'\nnamespace = 'test'\nruntime = 'runc'"),
            format!("[agent_sandbox]\nimage = '{image}'\ncontext = 'test'\nnamespace = 'test'\nnetwork = 'allowlist'"),
        ] {
            assert!(parse(&table).is_err(), "{table}");
        }
    }

    #[test]
    fn partial_project_sections_preserve_user_runtime_and_network_choices() {
        let image = format!("example/worker@sha256:{}", "a".repeat(64));
        let user = parse(&format!(
            "[container]\nimage = '{image}'\nruntime = 'runc'\nnetwork = 'unrestricted'\n"
        ))
        .unwrap();
        let project = parse(&format!("[container]\nimage = '{image}'\ncpus = 2\n")).unwrap();
        let merged = crate::merge(
            &[
                (
                    crate::Source::User {
                        path: "user.toml".into(),
                    },
                    user,
                ),
                (
                    crate::Source::Project {
                        path: "branchyard.toml".into(),
                    },
                    project,
                ),
            ],
            |_| None,
        )
        .unwrap();
        let container = merged.config.container.unwrap();
        assert_eq!(container.runtime, Some(ContainerRuntime::Runc));
        assert_eq!(container.network, Some(SandboxNetwork::Unrestricted));
        assert_eq!(container.cpus, Some(2));
    }
}
