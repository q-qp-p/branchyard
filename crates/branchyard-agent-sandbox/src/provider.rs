use std::path::PathBuf;
use std::sync::Arc;

use branchyard_sandbox::exec::{self, ExecGroup};
use branchyard_sandbox::{
    Capabilities, ExecSpec, Process, ProviderError, SandboxInfo, SandboxProvider, SandboxSpec,
    SandboxState,
};

use crate::config::{runtime_error, Config, Network};
use crate::control::Control;
use crate::manifest;
use crate::owned::{Locked, Receipt, Store};
use crate::transport::Transport;
use crate::verify;

/// A receipt-owned Agent Sandbox provider using operator-authenticated kubectl.
/// Allocations use copied workspace/home placement, piped exec, and destroy-only
/// engine lifecycle. This adapter offers no pause, snapshots, host mounts,
/// write scopes, or Branchyard proxy-based required-egress confinement.
pub struct AgentSandboxProvider {
    pub(crate) control: Control,
    store: Store,
}

impl AgentSandboxProvider {
    /// Validate configuration without cloud calls or filesystem creation.
    /// `state_dir` holds private durable ownership intents and UID receipts.
    pub fn new(config: Config, state_dir: impl Into<PathBuf>) -> Result<Self, ProviderError> {
        config.validate()?;
        Ok(Self {
            control: Control {
                config,
                binary: PathBuf::from("kubectl"),
                python: PathBuf::from("python3"),
            },
            store: Store::new(state_dir.into()),
        })
    }

    fn load(&self, locked: &Locked, name: &str) -> Result<Option<Receipt>, ProviderError> {
        let receipt = locked.read()?;
        if let Some(receipt) = &receipt {
            receipt.validate_target(&self.control.config, name)?;
        }
        Ok(receipt)
    }

    fn target(&self, receipt: &Receipt) -> Result<(), ProviderError> {
        let target = verify::namespace(&self.control)?;
        verify::check(
            target.namespace_uid == receipt.namespace_uid
                && target.runtime_class_uid == receipt.runtime_class_uid,
            "namespace or RuntimeClass UID was replaced; refusing allocation access",
        )
    }

    // Persist immutable identity before returning or performing the next mutation.
    fn reconcile(&self, locked: &Locked, receipt: &mut Receipt) -> Result<(), ProviderError> {
        let name = receipt.resource_name();
        let sandbox = self
            .control
            .get_here("sandboxes.agents.x-k8s.io", &name)?
            .ok_or_else(|| runtime_error("Sandbox disappeared during allocation"))?;
        let uid = verify::sandbox(&sandbox, receipt)?;
        if receipt.sandbox_uid.is_none() {
            receipt.sandbox_uid = Some(uid.clone());
            locked.write(receipt)?;
        }
        if receipt.target.network == Network::Unrestricted {
            if self.control.get_here("networkpolicy", &name)?.is_none() {
                if receipt.policy_uid.is_some() {
                    return Err(runtime_error(
                        "recorded allocation network policy disappeared",
                    ));
                }
                if let Err(error) = self.control.create(&manifest::policy(receipt, &uid)) {
                    if self.control.get_here("networkpolicy", &name)?.is_none() {
                        return Err(error);
                    }
                }
            }
            let policy = self
                .control
                .get_here("networkpolicy", &name)?
                .ok_or_else(|| runtime_error("allocation network policy disappeared"))?;
            receipt.policy_uid = Some(verify::policy(&policy, receipt, &uid)?);
            locked.write(receipt)?;
        }
        verify::network(&self.control, receipt, &uid)?;
        Ok(())
    }

    fn running(
        &self,
        locked: &Locked,
        receipt: &mut Receipt,
    ) -> Result<SandboxInfo, ProviderError> {
        let name = receipt.resource_name();
        let sandbox = self
            .control
            .get_here("sandboxes.agents.x-k8s.io", &name)?
            .ok_or_else(|| runtime_error("recorded Sandbox disappeared"))?;
        let sandbox_uid = verify::sandbox(&sandbox, receipt)?;
        if receipt.sandbox_uid.is_none() {
            receipt.sandbox_uid = Some(sandbox_uid.clone());
            locked.write(receipt)?;
        }
        verify::check(verify::ready(&sandbox), "Sandbox is not ready")?;
        verify::network(&self.control, receipt, &sandbox_uid)?;
        // v1.0.9 cold-created Sandboxes give their Pod the Sandbox name. We do
        // not adopt warm-pool Pods or follow client-controlled pod-name hints.
        let pod = self
            .control
            .get_here("pod", &name)?
            .ok_or_else(|| runtime_error("the Sandbox's owned Pod is missing"))?;
        let pod = verify::pod(&self.control, &pod, receipt, &sandbox_uid)?;
        if receipt.pod_uid.is_none() || receipt.node.as_ref() != Some(&pod.node) {
            receipt.pod_uid = Some(pod.uid);
            receipt.node = Some(pod.node);
            locked.write(receipt)?;
        }
        Ok(SandboxInfo {
            name: receipt.name.clone(),
            state: SandboxState::Running,
        })
    }

    fn remove(&self, name: &str) -> Result<(), ProviderError> {
        let locked = self.store.lock(name)?;
        let Some(mut receipt) = self.load(&locked, name)? else {
            return Ok(());
        };
        verify::check(
            verify::namespace_uid(&self.control)? == receipt.namespace_uid,
            "namespace UID was replaced; refusing allocation deletion",
        )?;
        let resource_name = receipt.resource_name();
        if let Some(sandbox) = self
            .control
            .get_here("sandboxes.agents.x-k8s.io", &resource_name)?
        {
            let uid = verify::owned(&sandbox, &receipt, receipt.sandbox_uid.as_deref())?;
            if receipt.sandbox_uid.is_none() {
                // Recover the create-before-receipt gap using nonce/spec ownership.
                receipt.sandbox_uid = Some(uid.clone());
                locked.write(&receipt)?;
            }
            if let Some(pod) = self.control.get_here("pod", &resource_name)? {
                let pod_uid = verify::owned(&pod, &receipt, receipt.pod_uid.as_deref())?;
                verify::check(
                    pod.pointer("/metadata/ownerReferences")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|refs| {
                            refs.iter().any(|r| {
                                r["uid"] == uid && r["kind"] == "Sandbox" && r["controller"] == true
                            })
                        }),
                    "Pod controller ownership changed; refusing deletion",
                )?;
                if receipt.pod_uid.is_none() {
                    receipt.pod_uid = Some(pod_uid);
                    locked.write(&receipt)?;
                }
            }
            if let Some(policy) = self.control.get_here("networkpolicy", &resource_name)? {
                verify::check(
                    receipt.target.network == Network::Unrestricted,
                    "unexpected allocation policy in deny mode; refusing cascading deletion",
                )?;
                let policy_uid = verify::policy(&policy, &receipt, &uid)?;
                if receipt.policy_uid.is_none() {
                    receipt.policy_uid = Some(policy_uid);
                    locked.write(&receipt)?;
                }
            }
            let path = format!(
                "/apis/agents.x-k8s.io/v1beta1/namespaces/{}/sandboxes/{resource_name}",
                receipt.target.namespace
            );
            self.control.delete_uid(&path, &uid)?;
            // No force deletion: a missing API object alone must not be used
            // to bypass normal kubelet termination on an unreachable node.
            self.control
                .wait("sandboxes.agents.x-k8s.io", &resource_name, "--for=delete")?;
        }
        // Foreground GC removes the controller-owned Pod and egress policy.
        // Verify their immutable identities before any explicit cleanup retry.
        if let Some(pod) = self.control.get_here("pod", &resource_name)? {
            let pod_uid = verify::owned(&pod, &receipt, receipt.pod_uid.as_deref())?;
            let sandbox_uid = receipt
                .sandbox_uid
                .as_deref()
                .ok_or_else(|| runtime_error("cannot clean Pod without a recorded Sandbox UID"))?;
            verify::check(
                pod.pointer("/metadata/ownerReferences")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|refs| {
                        refs.iter().any(|r| {
                            r["uid"] == sandbox_uid
                                && r["kind"] == "Sandbox"
                                && r["controller"] == true
                        })
                    }),
                "orphan Pod has foreign controller ownership",
            )?;
            let path = format!(
                "/api/v1/namespaces/{}/pods/{resource_name}",
                receipt.target.namespace
            );
            self.control.delete_uid(&path, &pod_uid)?;
            self.control.wait("pod", &resource_name, "--for=delete")?;
        }
        if let Some(policy) = self.control.get_here("networkpolicy", &resource_name)? {
            let sandbox_uid = receipt.sandbox_uid.as_deref().ok_or_else(|| {
                runtime_error("cannot clean policy without a recorded Sandbox UID")
            })?;
            let policy_uid = verify::policy(&policy, &receipt, sandbox_uid)?;
            let path = format!(
                "/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/{resource_name}",
                receipt.target.namespace
            );
            self.control.delete_uid(&path, &policy_uid)?;
            self.control
                .wait("networkpolicy", &resource_name, "--for=delete")?;
        }
        locked.remove()
    }
}

impl SandboxProvider for AgentSandboxProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            exec: true,
            ..Capabilities::default()
        }
    }

    fn ensure(&self, spec: &SandboxSpec) -> Result<SandboxInfo, ProviderError> {
        self.control.config.validate_spec(spec)?;
        self.control.check_host()?;
        let locked = self.store.lock(&spec.name)?;
        let mut receipt = match self.load(&locked, &spec.name)? {
            Some(receipt) => {
                receipt.validate_spec(spec)?;
                self.target(&receipt)?;
                receipt
            }
            None => {
                let target = verify::namespace(&self.control)?;
                let receipt = Receipt::intent(
                    &self.control.config,
                    spec,
                    target.namespace_uid,
                    target.runtime_class_uid,
                )?;
                // fsync intent before a create can possibly reach Kubernetes.
                locked.write(&receipt)?;
                receipt
            }
        };
        let name = receipt.resource_name();
        if self
            .control
            .get_here("sandboxes.agents.x-k8s.io", &name)?
            .is_none()
        {
            if receipt.sandbox_uid.is_some() {
                return Err(runtime_error("recorded Sandbox disappeared; destroy its receipt before starting a new allocation"));
            }
            // A lost response is ambiguous. Recover only our own nonce/spec;
            // reconcile validates ownership before accepting any existing UID.
            if let Err(error) = self.control.create(&manifest::sandbox(&receipt)) {
                if self
                    .control
                    .get_here("sandboxes.agents.x-k8s.io", &name)?
                    .is_none()
                {
                    return Err(error);
                }
            }
        }
        self.reconcile(&locked, &mut receipt)?;
        self.control
            .wait("sandboxes.agents.x-k8s.io", &name, "--for=condition=Ready")?;
        self.target(&receipt)?;
        self.running(&locked, &mut receipt)
    }

    fn inspect(&self, name: &str) -> Result<Option<SandboxInfo>, ProviderError> {
        let locked = self.store.lock(name)?;
        let Some(mut receipt) = self.load(&locked, name)? else {
            return Ok(None);
        };
        self.target(&receipt)?;
        let Some(sandbox) = self
            .control
            .get_here("sandboxes.agents.x-k8s.io", &receipt.resource_name())?
        else {
            return Ok(None);
        };
        let sandbox_uid = verify::sandbox(&sandbox, &receipt)?;
        if receipt.sandbox_uid.is_none() {
            receipt.sandbox_uid = Some(sandbox_uid);
            locked.write(&receipt)?;
        }
        if !verify::ready(&sandbox) {
            return Ok(Some(SandboxInfo {
                name: name.into(),
                state: SandboxState::Starting,
            }));
        }
        self.running(&locked, &mut receipt).map(Some)
    }

    fn exec(&self, name: &str, spec: &ExecSpec) -> Result<Box<dyn Process>, ProviderError> {
        spec.refuse_write_scope()?;
        let locked = self.store.lock(name)?;
        let mut receipt = self
            .load(&locked, name)?
            .ok_or_else(|| ProviderError::NotFound(name.into()))?;
        self.target(&receipt)?;
        self.running(&locked, &mut receipt)?;
        let pod_uid = receipt
            .pod_uid
            .clone()
            .ok_or_else(|| runtime_error("ready Pod has no recorded UID"))?;
        let transport = Arc::new(Transport {
            control: self.control.clone(),
            pod_name: receipt.resource_name(),
            pod_uid,
        });
        exec::spawn(transport, &ExecGroup::new(&receipt.owner)?, spec)
    }

    fn stop(&self, name: &str) -> Result<(), ProviderError> {
        self.remove(name)
    }

    fn destroy(&self, name: &str) -> Result<(), ProviderError> {
        self.remove(name)
    }
}
