use serde_json::{json, Value};

use crate::owned::Receipt;

pub(crate) const OWNER: &str = "branchyard.io/owner";
pub(crate) const SPEC: &str = "branchyard.io/spec";
pub(crate) const IDENTITY: &str = "/var/run/branchyard-identity";

pub(crate) fn sandbox(receipt: &Receipt) -> Value {
    let runtime = receipt.target.runtime.name();
    json!({
        "apiVersion":"agents.x-k8s.io/v1beta1", "kind":"Sandbox",
        "metadata": {
            "name":receipt.resource_name(), "namespace":receipt.target.namespace,
            "labels":{OWNER:receipt.owner}, "annotations":{SPEC:receipt.digest}
        },
        "spec": {
            "service":false, "failedPodPolicy":"Retain", "operatingMode":"Running",
            "podTemplate": {
                "metadata":{"labels":{OWNER:receipt.owner}, "annotations":{SPEC:receipt.digest}},
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
                        "volumeMounts":[
                            {"name":"workspace","mountPath":"/workspace"},
                            {"name":"home","mountPath":"/branchyard/home"},
                            {"name":"tmp","mountPath":"/tmp"},
                            {"name":"identity","mountPath":IDENTITY,"readOnly":true}
                        ]
                    }],
                    "volumes":[
                        {"name":"workspace","emptyDir":{}}, {"name":"home","emptyDir":{}},
                        {"name":"tmp","emptyDir":{"sizeLimit":"256Mi"}},
                        {"name":"identity","downwardAPI":{"defaultMode":292,"items":[
                            {"path":"pod-uid","fieldRef":{"apiVersion":"v1","fieldPath":"metadata.uid"}}
                        ]}}
                    ]
                }
            }
        }
    })
}

pub(crate) fn policy(receipt: &Receipt, sandbox_uid: &str) -> Value {
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
                {"to":[{"ipBlock":{"cidr":"0.0.0.0/0","except":["169.254.169.254/32","169.254.169.252/32"]}}]},
                {"to":[{
                    "namespaceSelector":{"matchLabels":{"kubernetes.io/metadata.name":"kube-system"}},
                    "podSelector":{"matchExpressions":[{"key":"k8s-app","operator":"In","values":["kube-dns","node-local-dns"]}]}
                }],"ports":[{"protocol":"UDP","port":53},{"protocol":"TCP","port":53}]}
            ]
        }
    })
}
