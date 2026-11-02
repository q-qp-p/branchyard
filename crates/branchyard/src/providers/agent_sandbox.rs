//! Kubernetes SIGs Agent Sandboxes reached through copied/exec placement.

use std::sync::Arc;

use branchyard_sandbox::SandboxProvider;
use serde_json::json;

use super::{native, ProviderKind};
use crate::placement::{sandbox_name, Placement, SandboxPlan, HOME, WORKSPACE};
use crate::snapshots::Lifecycle;
use crate::state::{Fence, Record};
use crate::{AgentSandboxOptions, AgentSandboxRuntime, Error, Yard};

impl ProviderKind for AgentSandboxOptions {
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
            self.config(),
            native::state_dir(yard, &self.key()),
        )
        .map(|provider| Arc::new(provider) as Arc<dyn SandboxProvider>)
        .map_err(|error| error.to_string())
    }
    fn destroy(&self, yard: &Yard, sandbox: &str) -> Result<String, String> {
        native::destroy(self, yard, sandbox)
    }
}
