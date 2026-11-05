//! Kubernetes SIGs Agent Sandboxes reached through copied/exec placement.

use std::sync::Arc;

use branchyard_sandbox::SandboxProvider;
use serde_json::json;

use super::{native, HarnessLocation, ProviderKind};
use crate::placement::{sandbox_name, Placement, SandboxPlan, HOME, WORKSPACE};
use crate::snapshots::Lifecycle;
use crate::state::{Fence, Record};
use crate::{AgentSandboxOptions, AgentSandboxRuntime, Error, Yard};

/// The tool store to mount into an allocation: the yard's store root, from
/// its bucket, when it has both.
fn store_volume(yard: &Yard) -> Option<branchyard_agent_sandbox::StoreVolume> {
    let store = yard.tool_store()?;
    Some(branchyard_agent_sandbox::StoreVolume {
        bucket: store.bucket.clone()?,
        path: store.root.display().to_string(),
    })
}

impl ProviderKind for AgentSandboxOptions {
    fn harness_location(&self) -> HarnessLocation<'_> {
        HarnessLocation::Image
    }
    fn validate_options(&self) -> Result<(), Error> {
        self.validate()
    }
    fn name(&self) -> &'static str {
        "agent_sandbox"
    }
    fn key(&self) -> String {
        native::key(self.name(), json!(self))
    }
    fn banner(&self, _isolated: bool, _server: bool) -> (&'static str, String) {
        let runtime = match self.runtime {
            AgentSandboxRuntime::Gvisor => "gVisor",
            AgentSandboxRuntime::Microvm => "managed Kata microVM",
        };
        ("sandbox",format!("harnesses run in GKE Agent Sandboxes with {runtime}; workspace and home are copied in and back; each allocation is destroyed after capture"))
    }
    fn lifecycle(&self) -> Option<Lifecycle> {
        native::lifecycle()
    }
    fn copies_worktree(&self) -> bool {
        true
    }
    fn check(&self, _yard: &Yard) -> Result<(), Error> {
        self.validate()
    }
    /// A bucket-backed store is mounted at the store's own path
    /// ([`store_volume`]); without a bucket the home is all it gets.
    fn sees_store(&self, yard: &Yard) -> bool {
        store_volume(yard).is_some()
    }
    fn guest_paths(&self, _record: &Record) -> (String, String) {
        (WORKSPACE.into(), HOME.into())
    }
    fn prepare(
        &self,
        yard: &Yard,
        record: &Record,
        fence: &Fence,
        plan: &SandboxPlan,
    ) -> Result<Placement, String> {
        let spec = self.spec(&sandbox_name(&record.info.name)?);
        Placement::exec_copied(
            yard,
            record,
            fence,
            plan,
            self.open(yard)?,
            spec,
            &self.pass_env,
        )
    }
    fn recover(&self, yard: &Yard, record: &Record, sandbox: &str) -> Option<String> {
        Some(native::recover(self, yard, record, sandbox))
    }
    fn open(&self, yard: &Yard) -> Result<Arc<dyn SandboxProvider>, String> {
        branchyard_agent_sandbox::AgentSandboxProvider::new(
            self.config_with(store_volume(yard)),
            native::state_dir(yard, &self.key()),
        )
        .map(|provider| Arc::new(provider) as Arc<dyn SandboxProvider>)
        .map_err(|error| error.to_string())
    }
    fn destroy(&self, yard: &Yard, sandbox: &str) -> Result<String, String> {
        native::destroy(self, yard, sandbox)
    }
}
