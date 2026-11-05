use serde_json::{json, Value};

use crate::config::{Network, GOOGLE_API_BLOCKS};
use crate::owned::Receipt;

/// GKE's FQDN network policy API: `apiVersion`, `kind`, the plural
/// `kubectl` names it by, and its REST collection.
pub(crate) const FQDN_API_VERSION: &str = "networking.gke.io/v1alpha1";
pub(crate) const FQDN_KIND: &str = "FQDNNetworkPolicy";
pub(crate) const FQDN_RESOURCE: &str = "fqdnnetworkpolicies.networking.gke.io";
pub(crate) const FQDN_COLLECTION: &str = "/apis/networking.gke.io/v1alpha1/namespaces";
/// The guest root the tool tree is also at (`docs/tools-model.md`), and the
/// volume that holds it: the engine copies the tree in after the home.
pub(crate) const TOOLS_VOLUME: &str = "tools";
pub(crate) const TOOLS_PATH: &str = "/tools";

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

pub(crate) fn sandbox(receipt: &Receipt) -> Value {
    let runtime = receipt.target.runtime.name();
    let mut mounts = vec![
        json!({"name":"workspace","mountPath":"/workspace"}),
        json!({"name":"home","mountPath":"/branchyard/home"}),
        json!({"name":"tmp","mountPath":"/tmp"}),
        json!({"name":"identity","mountPath":IDENTITY,"readOnly":true}),
        json!({"name":TOOLS_VOLUME,"mountPath":TOOLS_PATH}),
    ];
    let mut volumes = vec![
        json!({"name":"workspace","emptyDir":{}}),
        json!({"name":"home","emptyDir":{}}),
        json!({"name":"tmp","emptyDir":{"sizeLimit":"256Mi"}}),
        json!({"name":"identity","downwardAPI":{"defaultMode":292,"items":[
            {"path":"pod-uid","fieldRef":{"apiVersion":"v1","fieldPath":"metadata.uid"}}
        ]}}),
        json!({"name":TOOLS_VOLUME,"emptyDir":{"sizeLimit":"64Mi"}}),
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
                    "containers":[{
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
                    }],
                    "volumes":volumes
                }
            }
        }
    })
}

/// Cluster DNS, which every allocation policy lets through: the resolver for
/// the names an `FQDNNetworkPolicy` matches.
fn dns_rule() -> Value {
    json!({"to":[{
        "namespaceSelector":{"matchLabels":{"kubernetes.io/metadata.name":"kube-system"}},
        "podSelector":{"matchExpressions":[{"key":"k8s-app","operator":"In","values":["kube-dns","node-local-dns"]}]}
    }],"ports":[{"protocol":"UDP","port":53},{"protocol":"TCP","port":53}]})
}

fn owned_by_sandbox(receipt: &Receipt, sandbox_uid: &str) -> Value {
    json!([{"apiVersion":"agents.x-k8s.io/v1beta1","kind":"Sandbox",
        "name":receipt.resource_name(),"uid":sandbox_uid,"controller":true,"blockOwnerDeletion":true}])
}

/// The egress rules of the allocation's `NetworkPolicy`.
///
/// Under `unrestricted`: the IPv4 Internet except the GKE metadata addresses,
/// and cluster DNS. Under `deny`, only what the turn needs and nothing else:
/// cluster DNS; the gateway's Service pods, when it runs in the cluster; and,
/// when the allocation mounts the tool store, the Private Google Access
/// ranges on 443, because the Cloud Storage FUSE sidecar runs in the pod's
/// network namespace and has no other route to the bucket. Hosts by name are
/// the `FQDNNetworkPolicy`'s ([`fqdn_policy`]).
pub(crate) fn egress_rules(receipt: &Receipt) -> Vec<Value> {
    let target = &receipt.target;
    if target.network == Network::Unrestricted {
        return vec![
            json!({"to":[{"ipBlock":{"cidr":"0.0.0.0/0","except":["169.254.169.254/32","169.254.169.252/32"]}}]}),
            dns_rule(),
        ];
    }
    let mut rules = vec![dns_rule()];
    if let Some(gateway) = &target.egress.gateway {
        rules.push(json!({"to":[{
            "namespaceSelector":{"matchLabels":{"kubernetes.io/metadata.name":gateway.namespace}},
            "podSelector":{"matchLabels":{"app.kubernetes.io/name":gateway.name}}
        }],"ports":[{"protocol":"TCP","port":gateway.port}]}));
    }
    if target.store.is_some() {
        let blocks: Vec<Value> = GOOGLE_API_BLOCKS
            .iter()
            .map(|cidr| json!({"ipBlock":{"cidr":cidr}}))
            .collect();
        rules.push(json!({"to":blocks,"ports":[{"protocol":"TCP","port":443}]}));
    }
    rules
}

pub(crate) fn policy(receipt: &Receipt, sandbox_uid: &str) -> Value {
    json!({
        "apiVersion":"networking.k8s.io/v1", "kind":"NetworkPolicy",
        "metadata":{
            "name":receipt.resource_name(),"namespace":receipt.target.namespace,
            "labels":{OWNER:receipt.owner},"annotations":{SPEC:receipt.digest},
            "ownerReferences":owned_by_sandbox(receipt, sandbox_uid)
        },
        "spec":{
            "podSelector":{"matchLabels":{OWNER:receipt.owner}},"policyTypes":["Egress"],
            "egress":egress_rules(receipt)
        }
    })
}

/// The allocation's GKE `FQDNNetworkPolicy`: the hosts it may reach by name,
/// on port 443. GKE adds it to the `NetworkPolicy`; both select the pod by
/// the same owner label.
pub(crate) fn fqdn_policy(receipt: &Receipt, sandbox_uid: &str) -> Value {
    let matches: Vec<Value> = receipt
        .target
        .fqdn_hosts()
        .iter()
        .map(|host| match host.strip_prefix("*.") {
            Some(_) => json!({"pattern":host}),
            None => json!({"name":host}),
        })
        .collect();
    json!({
        "apiVersion":FQDN_API_VERSION, "kind":FQDN_KIND,
        "metadata":{
            "name":receipt.resource_name(),"namespace":receipt.target.namespace,
            "labels":{OWNER:receipt.owner},"annotations":{SPEC:receipt.digest},
            "ownerReferences":owned_by_sandbox(receipt, sandbox_uid)
        },
        "spec":{
            "podSelector":{"matchLabels":{OWNER:receipt.owner}},
            "egress":[{"matches":matches,"ports":[{"protocol":"TCP","port":443}]}]
        }
    })
}
