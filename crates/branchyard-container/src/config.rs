use serde::{Deserialize, Serialize};

/// The container runtime the operator requires.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    /// gVisor's `runsc` runtime, installed and configured by the operator.
    #[default]
    Gvisor,
    /// Ordinary Linux container namespaces, sharing the host kernel.
    Runc,
}

impl Runtime {
    /// The exact runtime name the Docker daemon must report.
    pub fn docker_name(self) -> &'static str {
        match self {
            Runtime::Gvisor => "runsc",
            Runtime::Runc => "runc",
        }
    }
}

/// The allocation's network boundary.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Network {
    /// No external network interface. This does not implement a host allowlist.
    #[default]
    Deny,
    /// An ordinary container bridge, selected explicitly by the operator.
    Unrestricted,
}

impl Network {
    /// Docker's network mode for this boundary.
    pub fn docker_name(self) -> &'static str {
        match self {
            Network::Deny => "none",
            Network::Unrestricted => "bridge",
        }
    }
}

/// Operator-selected runtime and network settings for an allocation.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Required runtime; the provider never substitutes another runtime.
    #[serde(default)]
    pub runtime: Runtime,
    /// Allocation-wide networking, independent of an exec's egress requirements.
    #[serde(default)]
    pub network: Network,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_boundary_requires_gvisor_without_network() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(config, Config::default());
        assert_eq!(config.runtime.docker_name(), "runsc");
        assert_eq!(config.network.docker_name(), "none");
    }

    #[test]
    fn an_operator_can_choose_the_shared_kernel_boundary_explicitly() {
        let config: Config =
            serde_json::from_str(r#"{"runtime":"runc","network":"unrestricted"}"#).unwrap();
        assert_eq!(config.runtime.docker_name(), "runc");
        assert_eq!(config.network.docker_name(), "bridge");
        assert_eq!(
            serde_json::to_value(config).unwrap(),
            serde_json::json!({"runtime":"runc","network":"unrestricted"})
        );
    }

    #[test]
    fn unknown_runtimes_networks_and_daemon_overrides_are_refused() {
        for json in [
            r#"{"runtime":"default"}"#,
            r#"{"network":"host"}"#,
            r#"{"docker_host":"tcp://another-daemon:2375"}"#,
            r#"{"privileged":true}"#,
        ] {
            assert!(serde_json::from_str::<Config>(json).is_err(), "{json}");
        }
    }
}
