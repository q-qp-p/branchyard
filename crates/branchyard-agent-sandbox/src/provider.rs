use std::path::PathBuf;
use std::sync::Arc;

use branchyard_sandbox::exec::{self, ExecGroup};
use branchyard_sandbox::{
    Capabilities, ExecSpec, Launch, Process, ProviderError, SandboxInfo, SandboxProvider,
    SandboxSpec, SandboxState,
};

use crate::config::{runtime_error, Config};
use crate::control::Control;
use crate::manifest;
use crate::owned::{Locked, Pooled, Receipt, Store};
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
            .get_here("sandboxes.agents.x-k8s.io", &receipt.sandbox_name())?
            .ok_or_else(|| runtime_error("Sandbox disappeared during allocation"))?;
        let uid = verify::sandbox(&sandbox, receipt)?;
        if receipt.sandbox_uid.is_none() {
            receipt.sandbox_uid = Some(uid.clone());
            locked.write(receipt)?;
        }
        if receipt.target.has_policy() {
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
        if receipt.target.has_fqdn_policy() {
            if self
                .control
                .get_here(manifest::FQDN_RESOURCE, &name)?
                .is_none()
            {
                if receipt.fqdn_policy_uid.is_some() {
                    return Err(runtime_error(
                        "recorded allocation FQDN network policy disappeared",
                    ));
                }
                if let Err(error) = self.control.create(&manifest::fqdn_policy(receipt, &uid)) {
                    if self
                        .control
                        .get_here(manifest::FQDN_RESOURCE, &name)?
                        .is_none()
                    {
                        return Err(error);
                    }
                }
            }
            let policy = self
                .control
                .get_here(manifest::FQDN_RESOURCE, &name)?
                .ok_or_else(|| runtime_error("allocation FQDN network policy disappeared"))?;
            receipt.fqdn_policy_uid = Some(verify::fqdn_policy(&policy, receipt, &uid)?);
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
        let sandbox = self
            .control
            .get_here("sandboxes.agents.x-k8s.io", &receipt.sandbox_name())?
            .ok_or_else(|| runtime_error("recorded Sandbox disappeared"))?;
        let sandbox_uid = verify::sandbox(&sandbox, receipt)?;
        if receipt.sandbox_uid.is_none() {
            receipt.sandbox_uid = Some(sandbox_uid.clone());
            locked.write(receipt)?;
        }
        verify::check(verify::ready(&sandbox), "Sandbox is not ready")?;
        verify::network(&self.control, receipt, &sandbox_uid)?;
        // v1.0.9 cold-created Sandboxes give their Pod the Sandbox name. A
        // pod-name hint on the Sandbox is never followed: a claimed
        // allocation's Pod is the one the claim step found controlled by its
        // Sandbox's UID and recorded in the receipt, verified like any other.
        let pod = self
            .control
            .get_here("pod", &receipt.pod_name())?
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

    /// Give back a pooled allocation's claim, which is how the pool learns
    /// the sandbox is no longer wanted. The claim is deleted by UID once it
    /// is shown to be this allocation's.
    fn release_claim(&self, locked: &Locked, receipt: &mut Receipt) -> Result<(), ProviderError> {
        let resource_name = receipt.resource_name();
        let Some(claim) = self
            .control
            .get_here(manifest::CLAIM_RESOURCE, &resource_name)?
        else {
            return Ok(());
        };
        let uid = verify::claim(&claim, receipt)?;
        if let Some(pooled) = receipt.pooled.as_mut() {
            if pooled.claim_uid.is_none() {
                pooled.claim_uid = Some(uid.clone());
                locked.write(receipt)?;
            }
        }
        let path = format!(
            "{}/{}/sandboxclaims/{resource_name}",
            manifest::EXTENSIONS_COLLECTION,
            receipt.target.namespace
        );
        self.control.delete_uid(&path, &uid)?;
        self.control
            .wait(manifest::CLAIM_RESOURCE, &resource_name, "--for=delete")
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
        // A claimed Sandbox that was never marked as this allocation's
        // belongs to the pool until its claim is gone: only the claim is
        // ours to delete.
        let unmarked = receipt.pooled.as_ref().is_some_and(|p| !p.bound);
        if receipt.pooled.is_some() {
            self.release_claim(&locked, &mut receipt)?;
        }
        let sandbox_name = receipt.sandbox_name();
        let pod_name = receipt.pod_name();
        if let Some(sandbox) = self
            .control
            .get_here("sandboxes.agents.x-k8s.io", &sandbox_name)?
            .filter(|_| !unmarked)
        {
            let uid = verify::owned_named(
                &sandbox,
                &receipt,
                &sandbox_name,
                receipt.sandbox_uid.as_deref(),
            )?;
            if receipt.sandbox_uid.is_none() {
                // Recover the create-before-receipt gap using nonce/spec ownership.
                receipt.sandbox_uid = Some(uid.clone());
                locked.write(&receipt)?;
            }
            if let Some(pod) = self.control.get_here("pod", &pod_name)? {
                let pod_uid =
                    verify::owned_named(&pod, &receipt, &pod_name, receipt.pod_uid.as_deref())?;
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
                    receipt.target.has_policy(),
                    "unexpected allocation policy in deny mode; refusing cascading deletion",
                )?;
                let policy_uid = verify::policy(&policy, &receipt, &uid)?;
                if receipt.policy_uid.is_none() {
                    receipt.policy_uid = Some(policy_uid);
                    locked.write(&receipt)?;
                }
            }
            if receipt.target.has_fqdn_policy() {
                if let Some(policy) = self
                    .control
                    .get_here(manifest::FQDN_RESOURCE, &resource_name)?
                {
                    let policy_uid = verify::fqdn_policy(&policy, &receipt, &uid)?;
                    if receipt.fqdn_policy_uid.is_none() {
                        receipt.fqdn_policy_uid = Some(policy_uid);
                        locked.write(&receipt)?;
                    }
                }
            }
            let path = format!(
                "/apis/agents.x-k8s.io/v1beta1/namespaces/{}/sandboxes/{sandbox_name}",
                receipt.target.namespace
            );
            self.control.delete_uid(&path, &uid)?;
            // No force deletion: a missing API object alone must not be used
            // to bypass normal kubelet termination on an unreachable node.
            self.control
                .wait("sandboxes.agents.x-k8s.io", &sandbox_name, "--for=delete")?;
        }
        // Foreground GC removes the controller-owned Pod and egress policy.
        // Verify their immutable identities before any explicit cleanup retry.
        if let Some(pod) = self
            .control
            .get_here("pod", &pod_name)?
            .filter(|_| !unmarked)
        {
            let pod_uid =
                verify::owned_named(&pod, &receipt, &pod_name, receipt.pod_uid.as_deref())?;
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
                "/api/v1/namespaces/{}/pods/{pod_name}",
                receipt.target.namespace
            );
            self.control.delete_uid(&path, &pod_uid)?;
            self.control.wait("pod", &pod_name, "--for=delete")?;
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
        if receipt.target.has_fqdn_policy() {
            if let Some(policy) = self
                .control
                .get_here(manifest::FQDN_RESOURCE, &resource_name)?
            {
                let sandbox_uid = receipt.sandbox_uid.as_deref().ok_or_else(|| {
                    runtime_error("cannot clean FQDN policy without a recorded Sandbox UID")
                })?;
                let policy_uid = verify::fqdn_policy(&policy, &receipt, sandbox_uid)?;
                let path = format!(
                    "{}/{}/fqdnnetworkpolicies/{resource_name}",
                    manifest::FQDN_COLLECTION,
                    receipt.target.namespace
                );
                self.control.delete_uid(&path, &policy_uid)?;
                self.control
                    .wait(manifest::FQDN_RESOURCE, &resource_name, "--for=delete")?;
            }
        }
        locked.remove()
    }

    /// Make sure the warm pool for this allocation's specification exists
    /// and has the configured size, and say whether it has a ready sandbox
    /// to claim. The template and pool are created once, by the first
    /// allocation of a specification, and shared by every later one; they
    /// are never deleted here (`docs/agent-sandbox-gke.md#warm-pool`).
    fn pool_ready(&self, receipt: &Receipt, size: u32) -> Result<bool, ProviderError> {
        let template_name = manifest::template_name(receipt);
        let template = match self
            .control
            .get_here(manifest::TEMPLATE_RESOURCE, &template_name)?
        {
            Some(template) => template,
            None => {
                let created = self.control.create(&manifest::template(receipt));
                self.control
                    .get_here(manifest::TEMPLATE_RESOURCE, &template_name)?
                    .ok_or_else(|| match created {
                        Err(error) => error,
                        Ok(()) => runtime_error("the SandboxTemplate disappeared after creation"),
                    })?
            }
        };
        verify::template(&template, receipt)?;
        let pool_name = manifest::pool_name(receipt);
        let pool = match self.control.get_here(manifest::POOL_RESOURCE, &pool_name)? {
            Some(pool) => pool,
            None => {
                let created = self.control.create(&manifest::warm_pool(receipt, size));
                self.control
                    .get_here(manifest::POOL_RESOURCE, &pool_name)?
                    .ok_or_else(|| match created {
                        Err(error) => error,
                        Ok(()) => runtime_error("the SandboxWarmPool disappeared after creation"),
                    })?
            }
        };
        verify::warm_pool(&pool, receipt)?;
        if pool
            .pointer("/spec/replicas")
            .and_then(serde_json::Value::as_u64)
            != Some(u64::from(size))
        {
            self.control.patch(
                manifest::POOL_RESOURCE,
                &pool_name,
                &serde_json::json!({"spec":{"replicas":size}}),
            )?;
        }
        Ok(pool
            .pointer("/status/readyReplicas")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            > 0)
    }

    /// Claim a ready sandbox of the pool for the allocation and make it the
    /// allocation's. The claim is created (its intent already durable in the
    /// receipt), waited for, and read back; the Sandbox it resolved to and
    /// the warm Pod that Sandbox adopted are each shown to be made from
    /// this claim and unmarked, then marked with the allocation's owner.
    /// From then on both verify as a freshly created allocation's do
    /// ([`verify::sandbox`], [`verify::pod`]). Safe to run again after a
    /// stop at any point.
    fn bind(&self, locked: &Locked, receipt: &mut Receipt) -> Result<(), ProviderError> {
        let name = receipt.resource_name();
        if self
            .control
            .get_here(manifest::CLAIM_RESOURCE, &name)?
            .is_none()
        {
            if let Err(error) = self.control.create(&manifest::claim(receipt)) {
                if self
                    .control
                    .get_here(manifest::CLAIM_RESOURCE, &name)?
                    .is_none()
                {
                    return Err(error);
                }
            }
        }
        self.control
            .wait(manifest::CLAIM_RESOURCE, &name, "--for=condition=Ready")?;
        let claim = self
            .control
            .get_here(manifest::CLAIM_RESOURCE, &name)?
            .ok_or_else(|| runtime_error("the SandboxClaim disappeared"))?;
        let claim_uid = verify::claim(&claim, receipt)?;
        // The claim says which Sandbox it resolved to; one named like the
        // claim is the default.
        let sandbox_name = claim
            .pointer("/status/sandbox/name")
            .and_then(serde_json::Value::as_str)
            .filter(|n| !n.is_empty())
            .map_or_else(|| name.clone(), str::to_owned);
        if let Some(pooled) = receipt.pooled.as_mut() {
            pooled.claim_uid = Some(claim_uid.clone());
            pooled.sandbox = Some(sandbox_name.clone());
        }
        locked.write(receipt)?;
        let sandbox = self
            .control
            .get_here("sandboxes.agents.x-k8s.io", &sandbox_name)?
            .ok_or_else(|| runtime_error("the claimed Sandbox is missing"))?;
        let sandbox_uid = verify::claimed_sandbox(&sandbox, receipt, &sandbox_name, &claim_uid)?;
        let pod_name = self.adopted_pod(receipt, &sandbox_uid)?;
        // Marked only now, after both were shown to be unclaimed by anyone
        // else; `kubectl label` and `annotate` refuse to replace a value.
        self.control.mark(
            "label",
            "sandboxes.agents.x-k8s.io",
            &sandbox_name,
            manifest::OWNER,
            &receipt.owner,
        )?;
        self.control.mark(
            "annotate",
            "sandboxes.agents.x-k8s.io",
            &sandbox_name,
            manifest::SPEC,
            &receipt.digest,
        )?;
        self.control
            .mark("label", "pod", &pod_name, manifest::OWNER, &receipt.owner)?;
        if let Some(pooled) = receipt.pooled.as_mut() {
            pooled.pod = Some(pod_name);
            pooled.bound = true;
        }
        locked.write(receipt)
    }

    /// The one Pod the claimed Sandbox controls, by owner reference.
    fn adopted_pod(&self, receipt: &Receipt, sandbox_uid: &str) -> Result<String, ProviderError> {
        let list = self
            .control
            .get_here("pods", "")?
            .ok_or_else(|| runtime_error("cannot list namespace pods"))?;
        let mut owned = list
            .get("items")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| runtime_error("invalid pod list"))?
            .iter()
            .filter(|pod| {
                pod.pointer("/metadata/ownerReferences")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|refs| {
                        refs.iter()
                            .any(|r| r["uid"] == sandbox_uid && r["controller"] == true)
                    })
            });
        let (Some(pod), None) = (owned.next(), owned.next()) else {
            return Err(runtime_error(
                "the claimed Sandbox does not control exactly one Pod",
            ));
        };
        verify::claimed_pod(pod, receipt, sandbox_uid)?;
        verify::text(pod, "/metadata/name").map(str::to_owned)
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
        if receipt.pooled.is_none()
            && self
                .control
                .get_here("sandboxes.agents.x-k8s.io", &name)?
                .is_none()
        {
            if receipt.sandbox_uid.is_some() {
                return Err(runtime_error("recorded Sandbox disappeared; destroy its receipt before starting a new allocation"));
            }
            // With a warm pool configured, a ready sandbox of the pool is
            // claimed; none ready (the pool is new, or being refilled) and
            // the allocation is created as ever. The intent to claim is
            // durable before the claim can exist.
            let claimable = match self.control.config.pool_size() {
                Some(size) => self.pool_ready(&receipt, size)?,
                None => false,
            };
            if claimable {
                receipt.pooled = Some(Pooled {
                    template: manifest::template_name(&receipt),
                    ..Pooled::default()
                });
                locked.write(&receipt)?;
            } else {
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
        }
        if receipt.pooled.as_ref().is_some_and(|p| !p.bound) {
            self.bind(&locked, &mut receipt)?;
        }
        self.reconcile(&locked, &mut receipt)?;
        self.control.wait(
            "sandboxes.agents.x-k8s.io",
            &receipt.sandbox_name(),
            "--for=condition=Ready",
        )?;
        self.target(&receipt)?;
        self.running(&locked, &mut receipt)
    }

    /// Whether the allocation was claimed from the warm pool. Nothing else
    /// about its launch is measured here.
    fn launch(&self, name: &str) -> Option<Launch> {
        // A receipt that cannot be read reports no launch: measurement is
        // advisory and must not fail the turn.
        let Ok(locked) = self.store.lock(name) else {
            return None;
        };
        let Ok(Some(receipt)) = self.load(&locked, name) else {
            return None;
        };
        Some(Launch {
            pooled: receipt.pooled.is_some(),
            ..Launch::default()
        })
    }

    fn inspect(&self, name: &str) -> Result<Option<SandboxInfo>, ProviderError> {
        let locked = self.store.lock(name)?;
        let Some(mut receipt) = self.load(&locked, name)? else {
            return Ok(None);
        };
        self.target(&receipt)?;
        let Some(sandbox) = self
            .control
            .get_here("sandboxes.agents.x-k8s.io", &receipt.sandbox_name())?
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

    fn image_receipt(&self, name: &str) -> Result<Option<String>, ProviderError> {
        // The receipt alone: kubectl is not asked.
        let locked = self.store.lock(name)?;
        Ok(self.load(&locked, name)?.map(|receipt| receipt.image))
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
            pod_name: receipt.pod_name(),
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
