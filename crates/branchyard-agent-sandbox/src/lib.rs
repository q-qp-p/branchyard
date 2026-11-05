//! Native Kubernetes SIGs Agent Sandbox execution on managed GKE runtimes.
//!
//! Operator kubeconfig authentication stays in the host's `kubectl`; the guest
//! receives no Kubernetes credentials. Private durable receipts and protected
//! DownwardAPI identity checks bind every execution to the recorded allocation.
//! Stop and destroy delete the whole allocation. Capability declarations are
//! not evidence of a live runtime qualification; see `docs/agent-sandbox.md`.

mod config;
mod control;
mod manifest;
mod owned;
mod provider;
mod transport;
mod verify;

pub use config::{Config, CredentialSidecar, Network, Runtime, StoreVolume};
pub use provider::AgentSandboxProvider;

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // tests: a panic is the failure report
#[cfg(test)]
mod tests;
