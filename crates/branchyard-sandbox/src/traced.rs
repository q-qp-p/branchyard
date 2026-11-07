//! A provider that spans and times what it is asked to do.
//!
//! [`traced`] wraps any [`SandboxProvider`] so each lifecycle action
//! (`ensure`, `exec`, `checkpoint`, `restore`, `branch`, `pause`, `resume`,
//! `stop`, `destroy`) is a span named `sandbox <action>` with the
//! `by.provider` and `by.sandbox` attributes and an error status when it
//! failed, and is observed in
//! `branchyard_sandbox_action_duration_seconds` by provider, action and
//! result. The span is built only when the process exports spans
//! ([`branchyard_support::trace::enabled`]); the timer is a clock read.
//! `exec` covers starting the process, not its lifetime: the process's own
//! work is the turn's.

use std::sync::Arc;

use branchyard_support::trace::{self, Timer};
use branchyard_support::trace_span;

use crate::{
    Capabilities, Checkpoint, ExecSpec, Launch, Mount, Process, ProviderError, SandboxInfo,
    SandboxProvider, SandboxSpec, SnapshotGuarantee,
};

/// The metric family the actions are observed in. The server declares it
/// (`branchyard_server::metrics::SANDBOX_ACTION`); a test there checks the
/// two names are the same.
pub const ACTION_FAMILY: &str = "branchyard_sandbox_action_duration_seconds";

/// `inner`, spanning and timing its actions as the provider named
/// `provider` (a stable kind such as `microsandbox`, never a recipe's or a
/// sandbox's own name).
pub fn traced(provider: &str, inner: Arc<dyn SandboxProvider>) -> Arc<dyn SandboxProvider> {
    Arc::new(Traced {
        provider: provider.to_owned(),
        inner,
    })
}

struct Traced {
    provider: String,
    inner: Arc<dyn SandboxProvider>,
}

impl Traced {
    fn act<T>(
        &self,
        action: &'static str,
        sandbox: &str,
        run: impl FnOnce() -> Result<T, ProviderError>,
    ) -> Result<T, ProviderError> {
        let timer = Timer::start(
            ACTION_FAMILY,
            vec![
                ("provider", self.provider.clone()),
                ("action", action.to_owned()),
            ],
        );
        let span = trace_span!(
            "sandbox",
            "otel.name" = format!("sandbox {action}"),
            "by.provider" = self.provider.as_str(),
            "by.action" = action,
            "by.sandbox" = sandbox,
            "otel.status_description" = trace::tracing::field::Empty,
        );
        let result = span.in_scope(run);
        if let Err(error) = &result {
            let why: String = error.to_string().chars().take(200).collect();
            span.record("otel.status_description", why.as_str());
        }
        timer.finish(result.is_ok());
        result
    }
}

impl SandboxProvider for Traced {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn ensure(&self, spec: &SandboxSpec) -> Result<SandboxInfo, ProviderError> {
        self.act("ensure", &spec.name, || self.inner.ensure(spec))
    }

    fn inspect(&self, name: &str) -> Result<Option<SandboxInfo>, ProviderError> {
        self.inner.inspect(name)
    }

    fn launch(&self, name: &str) -> Option<Launch> {
        self.inner.launch(name)
    }

    fn image_receipt(&self, name: &str) -> Result<Option<String>, ProviderError> {
        self.inner.image_receipt(name)
    }

    fn exec(&self, name: &str, spec: &ExecSpec) -> Result<Box<dyn Process>, ProviderError> {
        self.act("exec", name, || self.inner.exec(name, spec))
    }

    fn stop(&self, name: &str) -> Result<(), ProviderError> {
        self.act("stop", name, || self.inner.stop(name))
    }

    fn destroy(&self, name: &str) -> Result<(), ProviderError> {
        self.act("destroy", name, || self.inner.destroy(name))
    }

    fn checkpoint(
        &self,
        name: &str,
        required: &SnapshotGuarantee,
    ) -> Result<Checkpoint, ProviderError> {
        self.act("checkpoint", name, || self.inner.checkpoint(name, required))
    }

    fn restore(&self, name: &str, checkpoint: &Checkpoint) -> Result<(), ProviderError> {
        self.act("restore", name, || self.inner.restore(name, checkpoint))
    }

    fn branch(
        &self,
        checkpoint: &Checkpoint,
        spec: &SandboxSpec,
    ) -> Result<SandboxInfo, ProviderError> {
        self.act("branch", &spec.name, || self.inner.branch(checkpoint, spec))
    }

    fn share(&self, name: &str, mount: &Mount) -> Result<(), ProviderError> {
        self.inner.share(name, mount)
    }

    fn pause(&self, name: &str) -> Result<(), ProviderError> {
        self.act("pause", name, || self.inner.pause(name))
    }

    fn resume(&self, name: &str) -> Result<SandboxInfo, ProviderError> {
        self.act("resume", name, || self.inner.resume(name))
    }

    fn branch_live(
        &self,
        source: &str,
        children: &[SandboxSpec],
    ) -> Vec<Result<SandboxInfo, ProviderError>> {
        let timer = Timer::start(
            ACTION_FAMILY,
            vec![
                ("provider", self.provider.clone()),
                ("action", "branch".to_owned()),
            ],
        );
        let span = trace_span!(
            "sandbox",
            "otel.name" = "sandbox branch_live",
            "by.provider" = self.provider.as_str(),
            "by.action" = "branch_live",
            "by.sandbox" = source,
            "by.children" = i64::try_from(children.len()).unwrap_or(i64::MAX),
        );
        let results = span.in_scope(|| self.inner.branch_live(source, children));
        timer.finish(results.iter().all(Result::is_ok));
        results
    }

    fn exec_confined(
        &self,
        name: &str,
        spec: &ExecSpec,
        port: u16,
    ) -> Result<(Box<dyn Process>, std::net::TcpListener), ProviderError> {
        self.act("exec", name, || self.inner.exec_confined(name, spec, port))
    }

    fn release_checkpoint(&self, checkpoint: &Checkpoint) -> Result<(), ProviderError> {
        self.inner.release_checkpoint(checkpoint)
    }
}
