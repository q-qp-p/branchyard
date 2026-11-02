//! Container sandboxes whose runtime, identity and limits are verified before use.
//!
//! A gVisor runtime is a distinct boundary from an ordinary container's shared
//! host kernel. Configuration names that choice explicitly; absence of the
//! selected runtime never permits a fallback to another runtime.
#![warn(missing_docs)]

mod config;
mod docker;
mod provider;
mod receipt;
mod verify;

pub use config::{Config, Network, Runtime};
pub use provider::{validate_spec, ContainerProvider};

#[cfg(test)]
mod tests;
