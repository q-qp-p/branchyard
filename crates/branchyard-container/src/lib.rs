//! Container sandboxes whose runtime, identity and limits are verified before use.
//!
//! A gVisor runtime is a distinct boundary from an ordinary container's shared
//! host kernel. Configuration names that choice explicitly; absence of the
//! selected runtime never permits a fallback to another runtime.
#![warn(missing_docs)]

mod config;

pub use config::{Config, Network, Runtime};
