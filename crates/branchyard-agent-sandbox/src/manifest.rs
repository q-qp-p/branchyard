use serde_json::{json, Value};

use crate::owned::Receipt;

pub(crate) const OWNER: &str = "branchyard.io/owner";
pub(crate) const SPEC: &str = "branchyard.io/spec";
pub(crate) const IDENTITY: &str = "/var/run/branchyard-identity";
/// The Cloud Storage FUSE CSI driver, the tool store volume's name, the pod
/// annotation that has GKE inject the driver's sidecar, that sidecar's name
/// and the prefix of the volumes GKE adds for it.
pub(crate) const GCSFUSE_DRIVER: &str = "gcsfuse.csi.storage.gke.io";
pub(crate) const STORE_VOLUME: &str = "tool-store";
pub(crate) const GCSFUSE_ANNOTATION: &str = "gke-gcsfuse/volumes";
pub(crate) const GCSFUSE_SIDECAR: &str = "gke-gcsfuse-sidecar";
pub(crate) const GCSFUSE_VOLUME_PREFIX: &str = "gke-gcsfuse-";

/// The credential proxy sidecar's container name, and where in the pod it
/// finds its settings (the home the engine copies in; the same file as
/// `branchyard::connectors::credential::PROXY_CONFIG_FILE`).
pub(crate) const SIDECAR: &str = "credential-proxy";
pub(crate) const SIDECAR_CONFIG: &str = "/branchyard/home/.branchyard/credential-proxy.json";
/// The GKE metadata server's address: where the sidecar's Workload Identity
/// ID token comes from.
const METADATA_SERVER: &str = "169.254.169.254";

/// The sidecar container: `by credential-proxy` on the pod's loopback,
/// reading its settings from the home (mounted read-only) once the engine
/// has copied them in.
fn sidecar(receipt: &Receipt, sidecar: &crate::config::CredentialSidecar) -> Value {
    let image = sidecar.image.as_deref().unwrap_or(&receipt.image);
    json!({
        "name":SIDECAR, "image":image, "imagePullPolicy":"IfNotPresent",
        "command":["by","credential-proxy","--config",SIDECAR_CONFIG,"--wait"],
        "env":[
            {"name":"PATH","value":"/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"},
            {"name":"GCE_METADATA_HOST","value":METADATA_SERVER}
        ],
        "resources":{
            "requests":{"cpu":"50m","memory":"64Mi"},
            "limits":{"cpu":"250m","memory":"128Mi"}
        },
        "securityContext":{
            "allowPrivilegeEscalation":false,"readOnlyRootFilesystem":true,
            "capabilities":{"drop":["ALL"]}
        },
        "volumeMounts":[{"name":"home","mountPath":"/branchyard/home","readOnly":true}]
    })
}

pub(crate) fn sandbox(receipt: &Receipt) -> Value {
    let runtime = receipt.target.runtime.name();
    let mut mounts = vec![
        json!({"name":"workspace","mountPath":"/workspace"}),
        json!({"name":"home","mountPath":"/branchyard/home"}),
        json!({"name":"tmp","mountPath":"/tmp"}),
        json!({"name":"identity","mountPath":IDENTITY,"readOnly":true}),
    ];
    let mut volumes = vec![
        json!({"name":"workspace","emptyDir":{}}),
        json!({"name":"home","emptyDir":{}}),
        json!({"name":"tmp","emptyDir":{"sizeLimit":"256Mi"}}),
        json!({"name":"identity","downwardAPI":{"defaultMode":292,"items":[
            {"path":"pod-uid","fieldRef":{"apiVersion":"v1","fieldPath":"metadata.uid"}}
        ]}}),
    ];
    let mut pod_annotations = json!({SPEC:receipt.digest});
    if let Some(store) = &receipt.target.store {
        mounts.push(json!({"name":STORE_VOLUME,"mountPath":store.path,"readOnly":true}));
        volumes.push(json!({"name":STORE_VOLUME,"csi":{
            "driver":GCSFUSE_DRIVER,"readOnly":true,
            "volumeAttributes":{"bucketName":store.bucket_name(),"mountOptions":store.mount_options()}
        }}));
        pod_annotations[GCSFUSE_ANNOTATION] = json!("true");
    }
    let mut containers = vec![json!({
        "name":"runtime", "image":receipt.image, "imagePullPolicy":"IfNotPresent",
        "command":["setpriv","--no-new-privs","--","sh","-c","exec tail -f /dev/null"],
        "workingDir":"/workspace", "env":[
            {"name":"HOME","value":"/branchyard/home"},
            {"name":"PATH","value":"/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"}
        ],
        "resources":{
            "requests":{"cpu":receipt.cpus.to_string(), "memory":format!("{}Mi",receipt.memory_mib)},
            "limits":{"cpu":receipt.cpus.to_string(), "memory":format!("{}Mi",receipt.memory_mib)}
        },
        "securityContext":{
            "allowPrivilegeEscalation":false,"readOnlyRootFilesystem":true,
            "capabilities":{"drop":["ALL"]}
        },
        "volumeMounts":mounts
    })];
    if let Some(proxy) = &receipt.target.credential_proxy {
        containers.push(sidecar(receipt, proxy));
    }
    json!({
        "apiVersion":"agents.x-k8s.io/v1beta1", "kind":"Sandbox",
        "metadata": {
            "name":receipt.resource_name(), "namespace":receipt.target.namespace,
            "labels":{OWNER:receipt.owner}, "annotations":{SPEC:receipt.digest}
        },
        "spec": {
            "service":false, "failedPodPolicy":"Retain", "operatingMode":"Running",
            "podTemplate": {
                "metadata":{"labels":{OWNER:receipt.owner}, "annotations":pod_annotations},
                "spec": {
                    "runtimeClassName":runtime,
                    "nodeSelector":{"sandbox.gke.io/runtime":runtime},
                    "tolerations":[{"key":"sandbox.gke.io/runtime", "operator":"Equal", "value":runtime,"effect":"NoSchedule"}],
                    "automountServiceAccountToken":false,
                    "enableServiceLinks":false, "restartPolicy":"Never",
                    "terminationGracePeriodSeconds":10,
                    "securityContext":{
                        "runAsNonRoot":true, "runAsUser":1000, "runAsGroup":1000,
                        "fsGroup":1000, "seccompProfile":{"type":"RuntimeDefault"}
                    },
                    "containers":containers,
                    "volumes":volumes
                }
            }
        }
    })
}

pub(crate) fn policy(receipt: &Receipt, sandbox_uid: &str) -> Value {
    // A pod's containers share one network policy: with a credential proxy
    // sidecar the GKE metadata server (Workload Identity) is reachable by
    // the whole pod, the harness included; without one it stays closed.
    let except: &[&str] = match receipt.target.credential_proxy {
        Some(_) => &["169.254.169.252/32"],
        None => &["169.254.169.254/32", "169.254.169.252/32"],
    };
    json!({
        "apiVersion":"networking.k8s.io/v1", "kind":"NetworkPolicy",
        "metadata":{
            "name":receipt.resource_name(),"namespace":receipt.target.namespace,
            "labels":{OWNER:receipt.owner},"annotations":{SPEC:receipt.digest},
            "ownerReferences":[{"apiVersion":"agents.x-k8s.io/v1beta1","kind":"Sandbox",
                "name":receipt.resource_name(),"uid":sandbox_uid,"controller":true,"blockOwnerDeletion":true}]
        },
        "spec":{
            "podSelector":{"matchLabels":{OWNER:receipt.owner}},"policyTypes":["Egress"],
            "egress":[
                {"to":[{"ipBlock":{"cidr":"0.0.0.0/0","except":except}}]},
                {"to":[{
                    "namespaceSelector":{"matchLabels":{"kubernetes.io/metadata.name":"kube-system"}},
                    "podSelector":{"matchExpressions":[{"key":"k8s-app","operator":"In","values":["kube-dns","node-local-dns"]}]}
                }],"ports":[{"protocol":"UDP","port":53},{"protocol":"TCP","port":53}]}
            ]
        }
    })
}
