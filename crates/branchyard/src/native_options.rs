//! Native per-turn allocations use an immutable image, explicit runtime and
//! networking, copied worktree/home, and whole-allocation destruction after
//! capture or cancellation. They expose no host mounts, transport credentials,
//! pause, retention or checkpoint options.

use branchyard_sandbox::{Resources, SandboxSpec};
use serde::{Deserialize, Serialize};

use crate::Error;

/// GKE allocation networking, independent of required proxy egress.
pub use branchyard_agent_sandbox::Network as AgentSandboxNetwork;
/// Managed GKE runtime required by an Agent Sandbox allocation.
pub use branchyard_agent_sandbox::Runtime as AgentSandboxRuntime;
/// Container-wide networking, independent of Branchyard's required proxy egress.
pub use branchyard_container::Network as ContainerNetwork;
/// Docker runtime required by a native container allocation.
pub use branchyard_container::Runtime as ContainerRuntime;

/// An owned native container per turn, with copied files and a private home.
/// The image must provide the harness and the tools in `docs/container-sandbox.md`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerOptions {
    /// Immutable OCI image reference with a lowercase SHA-256 digest.
    pub image: String,
    /// CPU limit; unset selects one CPU.
    #[serde(default)]
    pub cpus: Option<u8>,
    /// Memory limit in MiB; unset selects 1024 MiB.
    #[serde(default)]
    pub memory_mib: Option<u32>,
    /// Required runtime; unset selects gVisor, with no fallback.
    #[serde(default)]
    pub runtime: ContainerRuntime,
    /// Allocation network boundary; unset denies external network access.
    #[serde(default)]
    pub network: ContainerNetwork,
    /// Host environment variables to copy by name, such as model API keys.
    /// Operator transport credentials are refused.
    #[serde(default)]
    pub pass_env: Vec<String>,
}

/// An owned Kubernetes SIGs Agent Sandbox per turn on a managed GKE runtime.
/// Operator kubeconfig stays on the worker. The image contract and namespace
/// preparation are described in `docs/agent-sandbox.md`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSandboxOptions {
    /// Explicit kubeconfig context authenticated by the operator's worker.
    pub context: String,
    /// Namespace prepared for this runtime and Dataplane V2 network enforcement.
    pub namespace: String,
    /// Immutable OCI image reference with a lowercase SHA-256 digest.
    pub image: String,
    /// CPU limit; unset selects one CPU.
    #[serde(default)]
    pub cpus: Option<u8>,
    /// Memory limit in MiB; unset selects 1024 MiB.
    #[serde(default)]
    pub memory_mib: Option<u32>,
    /// Required managed runtime; unset selects gVisor, with no fallback.
    #[serde(default)]
    pub runtime: AgentSandboxRuntime,
    /// Allocation network boundary; unset denies ingress and egress.
    #[serde(default)]
    pub network: AgentSandboxNetwork,
    /// Host environment variables to copy by name, such as model API keys.
    /// Operator Kubernetes and transport credentials are refused.
    #[serde(default)]
    pub pass_env: Vec<String>,
}

impl ContainerOptions {
    /// Check options without reading credentials, invoking Docker or creating files.
    pub fn validate(&self) -> Result<(), Error> {
        validate_env(&self.pass_env)?;
        branchyard_container::validate_spec(&self.spec("native-validation"))
            .map_err(|error| Error::Unsupported(error.to_string()))
    }

    pub(crate) fn spec(&self, name: &str) -> SandboxSpec {
        allocation(name, &self.image, self.cpus, self.memory_mib)
    }
}

impl AgentSandboxOptions {
    /// Check options without reading kubeconfig, invoking kubectl or creating files.
    pub fn validate(&self) -> Result<(), Error> {
        validate_env(&self.pass_env)?;
        self.config()
            .validate_spec(&self.spec("native-validation"))
            .map_err(|error| Error::Unsupported(error.to_string()))
    }

    pub(crate) fn config(&self) -> branchyard_agent_sandbox::Config {
        self.config_with(
            None,
            branchyard_agent_sandbox::Egress::default(),
            None,
            None,
        )
    }

    /// The target with the shared tool store mounted from `store`, when
    /// the yard has a store and a bucket for it, and `egress`: what a
    /// `deny` allocation may reach besides DNS, which is the worker's to
    /// say (`[agent_sandbox]` and `[network] mirrors`), not the branch's.
    pub(crate) fn config_with(
        &self,
        store: Option<branchyard_agent_sandbox::StoreVolume>,
        egress: branchyard_agent_sandbox::Egress,
        credential_proxy: Option<branchyard_agent_sandbox::CredentialSidecar>,
        warm_pool: Option<u32>,
    ) -> branchyard_agent_sandbox::Config {
        branchyard_agent_sandbox::Config {
            context: self.context.clone(),
            namespace: self.namespace.clone(),
            runtime: self.runtime,
            network: self.network,
            store,
            egress,
            credential_proxy,
            warm_pool,
        }
    }

    pub(crate) fn spec(&self, name: &str) -> SandboxSpec {
        allocation(name, &self.image, self.cpus, self.memory_mib)
    }
}

fn allocation(name: &str, image: &str, cpus: Option<u8>, memory_mib: Option<u32>) -> SandboxSpec {
    SandboxSpec::new(name)
        .image(image)
        .resources(Resources { cpus, memory_mib })
}

fn validate_env(names: &[String]) -> Result<(), Error> {
    for name in names {
        let upper = name.to_ascii_uppercase();
        if name.is_empty()
            || !name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(Error::Unsupported(
                "native sandbox pass_env names must be environment identifiers".into(),
            ));
        }
        if upper == branchyard_support::MANAGER_TOKEN_ENV
            || upper.starts_with("KUBE")
            || upper.starts_with("DOCKER_")
            || upper.starts_with("CLOUDSDK_AUTH_")
            || matches!(
                upper.as_str(),
                "GOOGLE_APPLICATION_CREDENTIALS" | "GOOGLE_OAUTH_ACCESS_TOKEN" | "SSH_AUTH_SOCK"
            )
        {
            return Err(Error::Unsupported(
                "operator transport credentials cannot be passed to a native sandbox".into(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn image() -> String {
        format!("example.test/runtime@sha256:{}", "a".repeat(64))
    }

    #[test]
    fn native_options_have_explicit_safe_defaults_and_round_trip() {
        let container: ContainerOptions = serde_json::from_value(json!({"image":image()})).unwrap();
        let kube: AgentSandboxOptions = serde_json::from_value(
            json!({"image":image(),"context":"test","namespace":"branchyard-gvisor"}),
        )
        .unwrap();
        assert_eq!(container.runtime, ContainerRuntime::Gvisor);
        assert_eq!(container.network, ContainerNetwork::Deny);
        assert_eq!(kube.runtime, AgentSandboxRuntime::Gvisor);
        assert_eq!(kube.network, AgentSandboxNetwork::Deny);
        container.validate().unwrap();
        kube.validate().unwrap();
        assert_eq!(
            serde_json::from_value::<ContainerOptions>(serde_json::to_value(&container).unwrap())
                .unwrap(),
            container
        );
        assert_eq!(
            serde_json::from_value::<AgentSandboxOptions>(serde_json::to_value(&kube).unwrap())
                .unwrap(),
            kube
        );
        assert!(!container.spec("test").persist);
        assert!(container.spec("test").mounts.is_empty());
        assert!(kube.spec("test").mounts.is_empty());
    }

    #[test]
    fn native_options_refuse_extra_controls_invalid_images_and_host_credentials() {
        for extra in [
            "keep",
            "snapshots",
            "privileged",
            "kubeconfig",
            "command",
            "mounts",
        ] {
            let mut value = json!({"image":image()});
            value[extra] = json!(true);
            assert!(serde_json::from_value::<ContainerOptions>(value.clone()).is_err());
            value["context"] = json!("test");
            value["namespace"] = json!("test");
            assert!(serde_json::from_value::<AgentSandboxOptions>(value).is_err());
        }
        let mut options = ContainerOptions {
            image: image(),
            ..ContainerOptions::default()
        };
        for name in [
            "KUBECONFIG",
            "DOCKER_AUTH_CONFIG",
            "GOOGLE_OAUTH_ACCESS_TOKEN",
            branchyard_support::MANAGER_TOKEN_ENV,
            "BAD=ENV",
        ] {
            options.pass_env = vec![name.into()];
            assert!(options.validate().is_err(), "{name}");
        }
        options.pass_env = vec!["OPENAI_API_KEY".into()];
        options.validate().unwrap();
        options.image = "image:latest".into();
        assert!(options.validate().is_err());
        assert!(AgentSandboxOptions::default().validate().is_err());
    }
}
