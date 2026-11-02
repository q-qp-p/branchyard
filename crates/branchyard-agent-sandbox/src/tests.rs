#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;

use branchyard_sandbox::{ExecSpec, Mount, Resources, SandboxProvider, SandboxSpec};
use branchyard_testkit::Scratch;
use serde_json::{json, Value};

use super::{AgentSandboxProvider, Config, Network, Runtime};

fn config() -> Config {
    Config {
        context: "test-context".into(),
        namespace: "branchyard-gvisor".into(),
        runtime: Runtime::Gvisor,
        network: Network::Deny,
    }
}

fn spec() -> SandboxSpec {
    SandboxSpec::new("allocation").image(format!("example.test/runtime@sha256:{}", "a".repeat(64)))
}

struct Fixture {
    scratch: Scratch,
    provider: AgentSandboxProvider,
}

impl Fixture {
    fn new(network: Network) -> Self {
        let scratch = Scratch::new("agent-sandbox");
        let mut config = config();
        config.network = network;
        let mut provider = AgentSandboxProvider::new(config, scratch.join("receipts")).unwrap();
        provider.control.binary = scratch.join("kubectl");
        // Share the immutable fixture inode. Writing executable scripts while
        // parallel tests fork can leave inherited writer FDs and cause ETXTBSY.
        std::os::unix::fs::symlink(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake_kubectl.py"),
            &provider.control.binary,
        )
        .unwrap();
        fs::create_dir(scratch.join("home")).unwrap();
        fs::write(scratch.join("state.json"), serde_json::to_vec(&json!({
            "objects":{}, "runtime":"gvisor", "counter":0, "extra_policies":[], "namespace_uid":"namespace-uid"
        })).unwrap()).unwrap();
        Self { scratch, provider }
    }

    fn state(&self) -> Value {
        serde_json::from_slice(&fs::read(self.scratch.join("state.json")).unwrap()).unwrap()
    }

    fn edit(&self, change: impl FnOnce(&mut Value)) {
        let mut state = self.state();
        change(&mut state);
        fs::write(
            self.scratch.join("state.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
    }

    fn name(&self) -> String {
        super::owned::resource_name("allocation")
    }

    fn calls(&self) -> Vec<Value> {
        fs::read_to_string(self.scratch.join("calls.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn receipt(&self) -> PathBuf {
        self.scratch.join(format!("receipts/{}.json", self.name()))
    }
}

#[test]
fn validation_is_pure_and_refuses_weakened_specs() {
    let scratch = Scratch::new("agent-validation");
    let path = scratch.join("does-not-exist");
    let provider = AgentSandboxProvider::new(config(), &path).unwrap();
    assert!(!path.exists());
    let mounts = spec().mount(Mount::writable("/tmp", "/workspace"));
    let tagged = SandboxSpec::new("allocation").image("image:latest");
    let zero = spec().resources(Resources {
        cpus: Some(0),
        memory_mib: Some(1),
    });
    for bad in [mounts, tagged, zero] {
        assert!(provider.ensure(&bad).is_err());
    }
    assert!(!path.exists());
    let mut target = config();
    target.context.clear();
    assert!(target.validate().is_err());
    target = config();
    target.namespace = "../other".into();
    assert!(target.validate().is_err());
    assert!(!provider.capabilities().egress);
    assert!(!provider.capabilities().pause);
}

#[test]
fn ensure_recovers_intent_and_destroy_uses_uid_preconditions() {
    let fixture = Fixture::new(Network::Unrestricted);
    fixture.edit(|state| state["lose_create_response"] = true.into());
    fixture.provider.ensure(&spec()).unwrap();
    let first: Value = serde_json::from_slice(&fs::read(fixture.receipt()).unwrap()).unwrap();
    // Simulate a process dying after create but before any UID was durable.
    let mut intent = first.clone();
    for key in ["sandbox_uid", "pod_uid", "policy_uid"] {
        intent[key] = Value::Null;
    }
    fs::write(fixture.receipt(), serde_json::to_vec(&intent).unwrap()).unwrap();
    fixture.provider.ensure(&spec()).unwrap();
    assert_eq!(first["namespace_uid"], "namespace-uid");
    let runtime_classes: Value =
        serde_json::from_str(include_str!("../tests/gke_runtimeclasses.json")).unwrap();
    assert_eq!(
        first["runtime_class_uid"],
        runtime_classes["items"][0]["metadata"]["uid"]
    );
    assert_eq!(first["node"]["uid"], "node-uid");
    assert_eq!(first["node"]["name"], "test-node");
    assert!(first["sandbox_uid"].is_string());
    assert!(first["pod_uid"].is_string());
    assert!(first["policy_uid"].is_string());
    fixture.provider.ensure(&spec()).unwrap();
    assert_eq!(fixture.state()["counter"], 2); // Sandbox + explicit egress policy.
    let name = fixture.name();
    let state = fixture.state();
    let pod = &state["objects"][format!("pod/{name}")];
    let policy = &state["objects"][format!("networkpolicy/{name}")];
    assert_eq!(
        policy["spec"]["egress"][0]["to"][0]["ipBlock"]["except"],
        json!(["169.254.169.254/32", "169.254.169.252/32"])
    );
    assert_eq!(
        policy["spec"]["egress"][1]["to"][0]["podSelector"]["matchLabels"]["k8s-app"],
        "kube-dns"
    );
    assert_eq!(pod["spec"]["automountServiceAccountToken"], false);
    assert_eq!(
        pod["spec"]["containers"][0]["securityContext"]["readOnlyRootFilesystem"],
        true
    );
    assert_eq!(
        pod["spec"]["containers"][0]["volumeMounts"][1]["mountPath"],
        "/branchyard/home"
    );
    fixture.provider.destroy("allocation").unwrap();
    assert!(!fixture.receipt().exists());
    assert!(fixture.state()["objects"].as_object().unwrap().is_empty());
    let delete = fixture
        .calls()
        .into_iter()
        .find(|call| call["operation"] == "delete")
        .unwrap();
    assert_eq!(delete["body"]["preconditions"]["uid"], first["sandbox_uid"]);
    assert_eq!(delete["body"]["propagationPolicy"], "Foreground");
    fixture.provider.destroy("allocation").unwrap();
}

#[test]
fn replacement_resource_and_namespace_are_never_adopted_or_deleted() {
    let fixture = Fixture::new(Network::Deny);
    fixture.provider.ensure(&spec()).unwrap();
    let name = fixture.name();
    fixture.edit(|state| {
        state["objects"][format!("sandbox/{name}")]["metadata"]["uid"] = "replacement".into()
    });
    assert!(fixture.provider.ensure(&spec()).is_err());
    assert!(fixture.provider.inspect("allocation").is_err());
    assert!(fixture.provider.destroy("allocation").is_err());
    assert!(!fixture
        .calls()
        .iter()
        .any(|call| call["operation"] == "delete"));
    fixture.edit(|state| state["namespace_uid"] = "new-namespace".into());
    assert!(fixture
        .provider
        .destroy("allocation")
        .unwrap_err()
        .to_string()
        .contains("namespace UID"));
    assert!(fixture.receipt().exists());
}

#[test]
fn foreign_name_collision_preserves_intent_and_foreign_object() {
    let fixture = Fixture::new(Network::Deny);
    fixture.edit(|state| state["objects"][format!("sandbox/{}",fixture.name())] = json!({
        "metadata":{"name":fixture.name(),"namespace":"branchyard-gvisor","uid":"foreign",
        "labels":{"branchyard.io/owner":"someone-else"},"annotations":{"branchyard.io/spec":"foreign"}}
    }));
    assert!(fixture
        .provider
        .ensure(&spec())
        .unwrap_err()
        .to_string()
        .contains("foreign ownership"));
    assert!(fixture.receipt().exists());
    assert!(fixture.provider.destroy("allocation").is_err());
    assert_eq!(fixture.state()["counter"], 0);
}

#[test]
fn uid_delete_race_is_refused_and_receipt_survives() {
    let fixture = Fixture::new(Network::Deny);
    fixture.provider.ensure(&spec()).unwrap();
    fixture.edit(|state| state["replace_on_delete"] = true.into());
    assert!(fixture.provider.destroy("allocation").is_err());
    assert!(fixture.receipt().exists());
    assert_eq!(
        fixture.state()["objects"][format!("sandbox/{}", fixture.name())]["metadata"]["uid"],
        "raced-replacement"
    );
}

#[test]
fn effective_pod_runtime_image_security_and_network_drift_are_refused() {
    for mutation in [
        "image",
        "runtime",
        "security",
        "resources",
        "pod_uid",
        "node",
        "policy",
        "dataplane",
        "handler",
    ] {
        let fixture = Fixture::new(Network::Deny);
        fixture.provider.ensure(&spec()).unwrap();
        let name = fixture.name();
        fixture.edit(|state| match mutation {
            "image" => state["objects"][format!("pod/{name}")]["status"]["containerStatuses"][0]["imageID"] = "sha256:wrong".into(),
            "runtime" => state["objects"][format!("pod/{name}")]["spec"]["runtimeClassName"] = "runc".into(),
            "security" => state["objects"][format!("pod/{name}")]["spec"]["containers"][0]["securityContext"]["privileged"] = true.into(),
            "resources" => state["objects"][format!("pod/{name}")]["spec"]["containers"][0]["resources"]["limits"]["cpu"] = "2".into(),
            "pod_uid" => state["objects"][format!("pod/{name}")]["metadata"]["uid"] = "replacement-pod".into(),
            "node" => state["wrong_node"] = true.into(),
            "policy" => state["extra_policies"] = json!([{"metadata":{"name":"foreign-allow"},"spec":{"podSelector":{},"egress":[{}]}}]),
            "dataplane" => state["dataplane_unready"] = true.into(),
            "handler" => state["wrong_handler"] = true.into(),
            _ => unreachable!(),
        });
        let error = fixture.provider.exec(
            "allocation",
            &ExecSpec {
                argv: vec!["true".into()],
                cwd: fixture.scratch.path().to_path_buf(),
                ..ExecSpec::default()
            },
        );
        assert!(error.is_err(), "{mutation}");
        assert!(
            !fixture
                .calls()
                .iter()
                .any(|call| call["operation"] == "exec"),
            "{mutation}"
        );
    }
}

#[test]
fn delete_remains_available_when_network_enforcement_is_unready() {
    let fixture = Fixture::new(Network::Deny);
    fixture.provider.ensure(&spec()).unwrap();
    fixture.edit(|state| state["dataplane_unready"] = true.into());
    assert!(fixture.provider.inspect("allocation").is_err());
    fixture.provider.stop("allocation").unwrap();
    assert!(fixture.state()["objects"].as_object().unwrap().is_empty());
}

#[test]
fn defaulted_pod_quantities_and_tolerations_preserve_the_requested_boundary() {
    let fixture = Fixture::new(Network::Deny);
    fixture.provider.ensure(&spec()).unwrap();
    let name = fixture.name();
    fixture.edit(|state| {
        let pod = &mut state["objects"][format!("pod/{name}")];
        let container = &mut pod["spec"]["containers"][0];
        for boundary in ["requests", "limits"] {
            container["resources"][boundary]["cpu"] = "1000m".into();
            container["resources"][boundary]["memory"] = "1Gi".into();
        }
        pod["spec"]["tolerations"].as_array_mut().unwrap().push(json!({
            "key":"node.kubernetes.io/not-ready","operator":"Exists","effect":"NoExecute","tolerationSeconds":300
        }));
    });
    fixture.provider.inspect("allocation").unwrap().unwrap();
}

#[test]
fn piped_exec_preserves_bytes_and_refuses_replacement_before_payload() {
    let fixture = Fixture::new(Network::Deny);
    fixture.provider.ensure(&spec()).unwrap();
    let mut process = fixture
        .provider
        .exec(
            "allocation",
            &ExecSpec {
                argv: vec![
                    "sh".into(),
                    "-c".into(),
                    "cat; printf 'err\\000bytes' >&2".into(),
                ],
                cwd: fixture.scratch.path().to_path_buf(),
                ..ExecSpec::default()
            },
        )
        .unwrap();
    let mut stdout = process.take_stdout().unwrap();
    let mut stderr = process.take_stderr().unwrap();
    let mut stdin = process.take_stdin().unwrap();
    stdin.write_all(b"hello\0\xffbytes").unwrap();
    drop(stdin);
    let mut out = Vec::new();
    stdout.read_to_end(&mut out).unwrap();
    let mut err = Vec::new();
    stderr.read_to_end(&mut err).unwrap();
    assert!(process.wait().unwrap().success());
    assert_eq!(out, b"hello\0\xffbytes");
    assert_eq!(err, b"err\0bytes");
    assert!(process.teardown().is_empty());
    fixture.edit(|state| state["exec_replacement_uid"] = "new-pod".into());
    let marker = fixture.scratch.join("must-not-exist");
    let result = fixture.provider.exec(
        "allocation",
        &ExecSpec {
            argv: vec!["touch".into(), marker.display().to_string()],
            cwd: fixture.scratch.path().to_path_buf(),
            ..ExecSpec::default()
        },
    );
    assert!(result.is_err());
    assert!(!marker.exists());
}

#[test]
fn changed_spec_and_required_write_scope_fail_before_exec() {
    let fixture = Fixture::new(Network::Deny);
    fixture.provider.ensure(&spec()).unwrap();
    assert!(fixture
        .provider
        .ensure(&spec().resources(Resources {
            cpus: Some(2),
            memory_mib: None
        }))
        .is_err());
    let count = fixture.calls().len();
    assert!(fixture
        .provider
        .exec(
            "allocation",
            &ExecSpec {
                writable: Some(vec!["/workspace".into()]),
                ..ExecSpec::default()
            }
        )
        .is_err());
    assert_eq!(fixture.calls().len(), count);
}

#[test]
fn credential_plugin_diagnostics_are_redacted_before_startup() {
    let fixture = Fixture::new(Network::Deny);
    fixture.provider.ensure(&spec()).unwrap();
    fixture.edit(|state| state["diagnostic"] = "test-credential-must-stay-host-side".into());
    let request = ExecSpec {
        argv: vec!["true".into()],
        cwd: fixture.scratch.path().to_path_buf(),
        ..ExecSpec::default()
    };
    let mut process = fixture.provider.exec("allocation", &request).unwrap();
    let mut stderr = String::new();
    process
        .take_stderr()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(process.wait().unwrap().success());
    assert!(!stderr.contains("test-credential"));
    let missing = ExecSpec {
        argv: vec!["branchyard-missing-program-test".into()],
        ..request.clone()
    };
    let error = fixture
        .provider
        .exec("allocation", &missing)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("guest program was not found"));
    let bad_cwd = ExecSpec {
        cwd: fixture.scratch.join("missing-cwd"),
        ..request.clone()
    };
    let error = fixture
        .provider
        .exec("allocation", &bad_cwd)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("guest exec directory could not be entered"));
    fixture.edit(|state| state["exec_replacement_uid"] = "replacement-pod".into());
    let teardown = process.teardown();
    assert!(teardown
        .iter()
        .any(|line| line.contains("exec control failed")));
    assert!(teardown
        .iter()
        .all(|line| !line.contains("test-credential")));
    let error = fixture
        .provider
        .exec("allocation", &request)
        .err()
        .unwrap()
        .to_string();
    assert!(!error.contains("test-credential"));
}

#[test]
fn orphaned_owned_pod_and_policy_are_deleted_by_their_recorded_uids() {
    let fixture = Fixture::new(Network::Unrestricted);
    fixture.provider.ensure(&spec()).unwrap();
    fixture.edit(|state| {
        state["objects"]
            .as_object_mut()
            .unwrap()
            .remove(&format!("sandbox/{}", fixture.name()));
    });
    fixture.provider.destroy("allocation").unwrap();
    assert!(fixture.state()["objects"].as_object().unwrap().is_empty());
    assert!(!fixture.receipt().exists());
    let calls = fixture.calls();
    let deletes: Vec<_> = calls
        .iter()
        .filter(|call| call["operation"] == "delete")
        .collect();
    assert_eq!(deletes.len(), 2);
    assert_eq!(deletes[0]["body"]["preconditions"]["uid"], "pod-uid");
}

#[test]
fn managed_microvm_requires_supported_gke_node_version() {
    let fixture = Fixture::new(Network::Deny);
    let mut config = config();
    config.runtime = Runtime::Microvm;
    let mut provider =
        AgentSandboxProvider::new(config, fixture.scratch.join("microvm-receipts")).unwrap();
    provider.control.binary = fixture.provider.control.binary.clone();
    fixture.edit(|state| {
        state["runtime"] = "microvm".into();
        state["version"] = "v1.36.5-gke.9999999".into();
    });
    assert!(provider
        .ensure(&spec())
        .unwrap_err()
        .to_string()
        .contains("1.37.0"));
    fixture.edit(|state| state["version"] = "v1.37.0-gke.4713000".into());
    provider.ensure(&spec()).unwrap();
    provider.stop("allocation").unwrap();
    assert!(provider.inspect("allocation").unwrap().is_none());
}
