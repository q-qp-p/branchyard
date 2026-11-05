use branchyard_sandbox::ProviderError;
use serde_json::{json, Value};

use crate::config::{runtime_error, Network, Runtime};
use crate::control::Control;
use crate::manifest;
use crate::owned::{NodeEvidence, Receipt};

pub(crate) struct Target {
    pub namespace_uid: String,
    pub runtime_class_uid: String,
}

pub(crate) struct PodIdentity {
    pub uid: String,
    pub node: NodeEvidence,
}

pub(crate) fn text<'a>(object: &'a Value, pointer: &str) -> Result<&'a str, ProviderError> {
    object
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| runtime_error(format!("Kubernetes object is missing {pointer}")))
}

pub(crate) fn uid(object: &Value) -> Result<&str, ProviderError> {
    text(object, "/metadata/uid")
}

pub(crate) fn namespace(control: &Control) -> Result<Target, ProviderError> {
    let namespace = control
        .get("namespace", &control.config.namespace, None)?
        .ok_or_else(|| runtime_error("the configured namespace does not exist"))?;
    check(
        namespace.pointer("/metadata/deletionTimestamp").is_none(),
        "namespace is being deleted",
    )?;
    check(
        text(&namespace, "/status/phase")? == "Active",
        "namespace is not active",
    )?;
    check(
        text(&namespace, "/metadata/labels/branchyard-runtime")? == control.config.runtime.name(),
        "namespace runtime differs from configuration",
    )?;
    check(
        text(&namespace, "/metadata/labels/branchyard-network-policy")? == "enforced-v2",
        "namespace lacks setup-verified Dataplane V2 enforcement",
    )?;
    let deployment = text(&namespace, "/metadata/labels/branchyard-deployment")?;
    let fingerprint = text(&namespace, "/metadata/labels/branchyard-config")?;
    let policy = control
        .get_here("networkpolicy", "branchyard-default-deny")?
        .ok_or_else(|| runtime_error("the setup-owned default deny policy is missing"))?;
    check(
        text(&policy, "/metadata/labels/branchyard-deployment")? == deployment
            && text(&policy, "/metadata/labels/branchyard-config")? == fingerprint,
        "default deny policy has foreign ownership",
    )?;
    check(
        policy.get("spec") == Some(&json!({"podSelector":{},"policyTypes":["Ingress","Egress"]})),
        "default deny policy has drifted",
    )?;
    let anetd = control
        .get("daemonset", "anetd", Some("kube-system"))?
        .ok_or_else(|| runtime_error("GKE Dataplane V2 anetd is missing"))?;
    let desired = anetd
        .pointer("/status/desiredNumberScheduled")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    check(
        desired > 0
            && anetd.pointer("/status/numberReady").and_then(Value::as_u64) == Some(desired),
        "GKE Dataplane V2 anetd is not ready on every selected node",
    )?;
    let runtime = control
        .get("runtimeclass", control.config.runtime.name(), None)?
        .ok_or_else(|| runtime_error("the managed RuntimeClass is missing"))?;
    let handler = text(&runtime, "/handler")?;
    check(
        handler == control.config.runtime.handler(),
        "RuntimeClass handler differs from the managed GKE runtime",
    )?;
    check(
        text(&runtime, "/scheduling/nodeSelector/sandbox.gke.io~1runtime")?
            == control.config.runtime.name(),
        "RuntimeClass scheduling differs from the managed GKE runtime",
    )?;
    check(
        runtime
            .pointer("/scheduling/tolerations")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item["key"] == "sandbox.gke.io/runtime"
                        && item["value"] == control.config.runtime.name()
                        && item["operator"] == "Equal"
                        && item["effect"] == "NoSchedule"
                })
            }),
        "RuntimeClass lacks the matching managed runtime toleration",
    )?;
    Ok(Target {
        namespace_uid: uid(&namespace)?.to_owned(),
        runtime_class_uid: uid(&runtime)?.to_owned(),
    })
}

pub(crate) fn namespace_uid(control: &Control) -> Result<String, ProviderError> {
    let namespace = control
        .get("namespace", &control.config.namespace, None)?
        .ok_or_else(|| runtime_error("the configured namespace does not exist"))?;
    Ok(uid(&namespace)?.to_owned())
}

pub(crate) fn owned(
    object: &Value,
    receipt: &Receipt,
    expected_uid: Option<&str>,
) -> Result<String, ProviderError> {
    check(
        text(object, "/metadata/name")? == receipt.resource_name(),
        "resource name differs from receipt",
    )?;
    check(
        text(object, "/metadata/namespace")? == receipt.target.namespace,
        "resource namespace differs from receipt",
    )?;
    check(
        text(object, "/metadata/labels/branchyard.io~1owner")? == receipt.owner,
        "resource has foreign ownership; refusing adoption or deletion",
    )?;
    check(
        text(object, "/metadata/annotations/branchyard.io~1spec")? == receipt.digest,
        "resource specification identity differs from receipt",
    )?;
    let actual = uid(object)?;
    if let Some(expected) = expected_uid {
        check(
            actual == expected,
            "resource UID was replaced; refusing adoption or deletion",
        )?;
    }
    Ok(actual.to_owned())
}

pub(crate) fn sandbox(object: &Value, receipt: &Receipt) -> Result<String, ProviderError> {
    let uid = owned(object, receipt, receipt.sandbox_uid.as_deref())?;
    check(
        object.pointer("/metadata/deletionTimestamp").is_none(),
        "Sandbox is being deleted",
    )?;
    let expected = manifest::sandbox(receipt);
    subset(&expected["spec"], &object["spec"], "Sandbox.spec")?;
    Ok(uid)
}

pub(crate) fn policy(
    object: &Value,
    receipt: &Receipt,
    sandbox_uid: &str,
) -> Result<String, ProviderError> {
    let uid = owned(object, receipt, receipt.policy_uid.as_deref())?;
    let expected = manifest::policy(receipt, sandbox_uid);
    check(
        object.get("spec") == expected.get("spec"),
        "allocation network policy has drifted",
    )?;
    check(
        object.pointer("/metadata/ownerReferences")
            == expected.pointer("/metadata/ownerReferences"),
        "allocation policy owner UID has drifted",
    )?;
    Ok(uid)
}

// NetworkPolicy is additive. A correctly named deny policy is insufficient if
// another selector also grants this Pod access. Permit only the one exact
// receipt-bound egress exception, or policies selecting a different owner.
pub(crate) fn network(
    control: &Control,
    receipt: &Receipt,
    sandbox_uid: &str,
) -> Result<(), ProviderError> {
    let list = control
        .get_here("networkpolicies", "")?
        .ok_or_else(|| runtime_error("cannot list namespace network policies"))?;
    let items = list
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| runtime_error("invalid network policy list"))?;
    let mut own_allow_seen = false;
    for item in items {
        if text(item, "/metadata/name")? == "branchyard-default-deny" {
            continue;
        }
        let selector = item.pointer("/spec/podSelector");
        let other_owner = selector
            .and_then(|v| v.pointer("/matchLabels/branchyard.io~1owner"))
            .and_then(Value::as_str);
        if other_owner.is_some_and(|owner| owner != receipt.owner)
            && selector
                .and_then(|v| v.get("matchExpressions"))
                .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty))
        {
            continue;
        }
        if receipt.target.network == Network::Unrestricted
            && item.pointer("/metadata/name").and_then(Value::as_str)
                == Some(&receipt.resource_name())
        {
            policy(item, receipt, sandbox_uid)?;
            own_allow_seen = true;
        } else {
            return Err(runtime_error(
                "an additional NetworkPolicy could override this allocation's network boundary",
            ));
        }
    }
    check(
        receipt.target.network != Network::Unrestricted || own_allow_seen,
        "the allocation's IPv4 egress policy is missing",
    )
}

/// `pod` without what GKE injects for the Cloud Storage FUSE CSI driver
/// when the allocation mounts the tool store: the driver's sidecar (a
/// container or an init container, by cluster version), its status, and the
/// volumes named for it. Only those exact names go; anything else GKE or an
/// admission webhook adds still fails the checks below.
fn without_gcsfuse(pod: &Value) -> Value {
    let mut pod = pod.clone();
    let named = |item: &Value, prefix: bool, name: &str| {
        item["name"]
            .as_str()
            .is_some_and(|n| n == name || (prefix && n.starts_with(name)))
    };
    for (path, name, prefix) in [
        ("/spec/containers", manifest::GCSFUSE_SIDECAR, false),
        ("/spec/initContainers", manifest::GCSFUSE_SIDECAR, false),
        ("/spec/volumes", manifest::GCSFUSE_VOLUME_PREFIX, true),
        (
            "/status/containerStatuses",
            manifest::GCSFUSE_SIDECAR,
            false,
        ),
        (
            "/status/initContainerStatuses",
            manifest::GCSFUSE_SIDECAR,
            false,
        ),
    ] {
        if let Some(items) = pod.pointer_mut(path).and_then(Value::as_array_mut) {
            items.retain(|item| !named(item, prefix, name));
        }
    }
    pod
}

pub(crate) fn pod(
    control: &Control,
    live: &Value,
    receipt: &Receipt,
    sandbox_uid: &str,
) -> Result<PodIdentity, ProviderError> {
    let cleaned;
    let object = match receipt.target.store {
        Some(_) => {
            cleaned = without_gcsfuse(live);
            &cleaned
        }
        None => live,
    };
    let pod_uid = owned(object, receipt, receipt.pod_uid.as_deref())?;
    check(
        object.pointer("/metadata/deletionTimestamp").is_none(),
        "Pod is being deleted",
    )?;
    let references = object
        .pointer("/metadata/ownerReferences")
        .and_then(Value::as_array)
        .ok_or_else(|| runtime_error("Pod has no controller ownership"))?;
    check(
        references.iter().any(|owner| {
            owner["uid"] == sandbox_uid && owner["kind"] == "Sandbox" && owner["controller"] == true
        }),
        "Pod is not controlled by the recorded Sandbox UID",
    )?;
    let expected = manifest::sandbox(receipt);
    let expected = &expected["spec"]["podTemplate"]["spec"];
    subset(expected, &object["spec"], "Pod.spec")?;
    let spec = &object["spec"];
    for key in ["hostNetwork", "hostPID", "hostIPC", "shareProcessNamespace"] {
        check(
            spec.get(key).is_none_or(|v| v == false),
            "Pod gained a host or shared process namespace",
        )?;
    }
    check(
        spec.pointer("/securityContext/sysctls")
            .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty)),
        "Pod gained custom sysctls",
    )?;
    for key in ["initContainers", "ephemeralContainers", "imagePullSecrets"] {
        check(
            spec.get(key)
                .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty)),
            "Pod gained extra containers or image credentials",
        )?;
    }
    let container = &spec["containers"][0];
    let security = &container["securityContext"];
    check(
        security.get("privileged").is_none_or(|v| v == false),
        "container became privileged",
    )?;
    check(
        security.get("procMount").is_none_or(|v| v == "Default"),
        "container gained unmasked proc mounts",
    )?;
    check(
        security
            .get("seccompProfile")
            .is_none_or(|v| v["type"] == "RuntimeDefault"),
        "container overrides seccomp",
    )?;
    check(
        container
            .get("args")
            .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty)),
        "container gained command arguments",
    )?;
    check(
        container.get("lifecycle").is_none(),
        "container gained lifecycle commands",
    )?;
    let mounts = container
        .get("volumeMounts")
        .and_then(Value::as_array)
        .ok_or_else(|| runtime_error("container mounts are missing"))?;
    check(
        mounts.iter().all(|mount| {
            mount.get("subPath").is_none()
                && mount.get("subPathExpr").is_none()
                && mount.get("mountPropagation").is_none_or(|v| v == "None")
        }),
        "container mount gained a subpath or propagation",
    )?;
    for key in ["runAsUser", "runAsGroup"] {
        check(
            security.get(key).is_none_or(|v| v == 1000),
            "container overrides the non-root identity",
        )?;
    }
    check(
        security.get("runAsNonRoot").is_none_or(|v| v == true),
        "container disables non-root enforcement",
    )?;
    check(
        security
            .pointer("/capabilities/add")
            .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty)),
        "container gained capabilities",
    )?;
    check(
        container
            .get("envFrom")
            .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty)),
        "container gained environment credentials",
    )?;
    check(
        container
            .get("ports")
            .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty)),
        "container gained ports outside the exec-only contract",
    )?;
    check(
        text(object, "/status/phase")? == "Running" && ready(object),
        "Pod is not running and ready",
    )?;
    let statuses = object
        .pointer("/status/containerStatuses")
        .and_then(Value::as_array)
        .ok_or_else(|| runtime_error("Pod has no container status"))?;
    check(
        statuses.len() == 1
            && statuses[0]["ready"] == true
            && statuses[0]["state"].get("running").is_some(),
        "runtime container is not ready",
    )?;
    let digest = receipt
        .image
        .rsplit_once('@')
        .map(|(_, digest)| digest)
        .ok_or_else(|| runtime_error("receipt image is not immutable"))?;
    let image_id = text(&statuses[0], "/imageID")?;
    check(
        image_id
            .rsplit_once('@')
            .map_or(image_id, |(_, digest)| digest)
            == digest,
        "running container image digest differs from the requested image",
    )?;
    let node_name = text(object, "/spec/nodeName")?;
    let node = control
        .get("node", node_name, None)?
        .ok_or_else(|| runtime_error("Pod node is missing"))?;
    check(
        text(&node, "/metadata/labels/sandbox.gke.io~1runtime")? == receipt.target.runtime.name(),
        "actual node is not configured for the required managed runtime",
    )?;
    text(&node, "/metadata/labels/cloud.google.com~1gke-nodepool")?;
    check(
        text(&node, "/spec/providerID")?.starts_with("gce://"),
        "node is not a GCE managed GKE node",
    )?;
    check(
        text(&node, "/status/nodeInfo/containerRuntimeVersion")?.starts_with("containerd://"),
        "node does not use containerd",
    )?;
    check(ready(&node), "Pod node is not ready")?;
    let node_uid = uid(&node)?;
    check(
        receipt
            .node
            .as_ref()
            .is_none_or(|recorded| recorded.uid == node_uid && recorded.name == node_name),
        "recorded Pod node identity was replaced",
    )?;
    check(
        text(&node, "/status/nodeInfo/operatingSystem")? == "linux",
        "managed sandbox node is not Linux",
    )?;
    let version = text(&node, "/status/nodeInfo/kubeletVersion")?;
    if receipt.target.runtime == Runtime::Microvm {
        check(
            microvm_version(version),
            "managed microVM requires GKE 1.37.0-gke.4713000 or later",
        )?;
    }
    Ok(PodIdentity {
        uid: pod_uid,
        node: NodeEvidence {
            name: node_name.into(),
            uid: node_uid.into(),
            kubelet_version: version.into(),
        },
    })
}

pub(crate) fn ready(object: &Value) -> bool {
    object
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .is_some_and(|conditions| {
            conditions
                .iter()
                .any(|condition| condition["type"] == "Ready" && condition["status"] == "True")
        })
}

fn microvm_version(version: &str) -> bool {
    let Some((version, build)) = version.trim_start_matches('v').split_once("-gke.") else {
        return false;
    };
    let mut parts = version.split('.').map(str::parse::<u32>);
    let (Some(Ok(major)), Some(Ok(minor)), Some(Ok(patch))) =
        (parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let Ok(build) = build.parse::<u64>() else {
        return false;
    };
    (major, minor, patch, build) >= (1, 37, 0, 4_713_000)
}

// API defaults may add object keys. Required scalars and complete arrays stay
// exact, so admission cannot inject a sidecar, mount, env, or command argument.
fn subset(expected: &Value, actual: &Value, path: &str) -> Result<(), ProviderError> {
    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => {
            for (key, value) in expected {
                let observed = actual
                    .get(key)
                    .ok_or_else(|| runtime_error(format!("{path}.{key} is missing")))?;
                subset(value, observed, &format!("{path}.{key}"))?;
            }
            Ok(())
        }
        (Value::Array(expected), Value::Array(actual)) if expected.len() == actual.len() => {
            for (index, (expected, actual)) in expected.iter().zip(actual).enumerate() {
                subset(expected, actual, &format!("{path}[{index}]"))?;
            }
            Ok(())
        }
        (Value::Array(expected), Value::Array(actual)) if path.ends_with(".tolerations") => check(
            expected.iter().all(|item| actual.contains(item))
                && actual.iter().all(|item| {
                    expected.contains(item)
                        || ((item["key"] == "node.kubernetes.io/not-ready"
                            || item["key"] == "node.kubernetes.io/unreachable")
                            && item["operator"] == "Exists"
                            && item["effect"] == "NoExecute")
                }),
            "Pod gained an unexpected runtime toleration",
        ),
        // Kubernetes quantities canonicalize, e.g. 1024Mi to 1Gi.
        (Value::String(expected), Value::String(actual)) if path.ends_with(".memory") => check(
            memory(expected).is_some() && memory(expected) == memory(actual),
            &format!("{path} differs from the requested memory"),
        ),
        (Value::String(expected), Value::String(actual)) if path.ends_with(".cpu") => check(
            cpu(expected).is_some() && cpu(expected) == cpu(actual),
            &format!("{path} differs from the requested CPU"),
        ),
        _ => check(
            expected == actual,
            &format!("{path} differs from the recorded specification"),
        ),
    }
}

fn memory(value: &str) -> Option<u64> {
    for (suffix, multiplier) in [
        ("Gi", 1u64 << 30),
        ("Mi", 1u64 << 20),
        ("Ki", 1u64 << 10),
        ("", 1),
    ] {
        if let Some(number) = value
            .strip_suffix(suffix)
            .and_then(|v| v.parse::<u64>().ok())
        {
            return number.checked_mul(multiplier);
        }
    }
    None
}

fn cpu(value: &str) -> Option<u64> {
    if let Some(millis) = value.strip_suffix('m') {
        millis.parse().ok()
    } else {
        value.parse::<u64>().ok()?.checked_mul(1000)
    }
}

pub(crate) fn check(condition: bool, message: &str) -> Result<(), ProviderError> {
    if condition {
        Ok(())
    } else {
        Err(runtime_error(message))
    }
}
