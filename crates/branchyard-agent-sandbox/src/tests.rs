use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;

use branchyard_sandbox::{ExecSpec, Mount, Resources, SandboxProvider, SandboxSpec};
use branchyard_testkit::Scratch;
use serde_json::{json, Value};

use super::{AgentSandboxProvider, Config, Network, Runtime, StoreVolume};

fn config() -> Config {
    Config {
        context: "test-context".into(),
        namespace: "branchyard-gvisor".into(),
        runtime: Runtime::Gvisor,
        network: Network::Deny,
        store: None,
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
        Self::build(network, None)
    }

    fn with_store(store: StoreVolume) -> Self {
        Self::build(Network::Deny, Some(store))
    }

    fn build(network: Network, store: Option<StoreVolume>) -> Self {
        let scratch = Scratch::new("agent-sandbox");
        let mut config = config();
        config.network = network;
        config.store = store;
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
        let text = match fs::read_to_string(self.scratch.join("calls.jsonl")) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => panic!("read fake kubectl calls: {error}"),
        };
        text.lines()
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
        policy["spec"]["egress"][1]["to"][0]["podSelector"],
        json!({"matchExpressions":[{"key":"k8s-app","operator":"In","values":["kube-dns","node-local-dns"]}]})
    );
    assert_eq!(
        policy["spec"]["egress"][1]["ports"],
        json!([{"protocol":"UDP","port":53},{"protocol":"TCP","port":53}])
    );
    assert_eq!(
        policy["spec"]["egress"][1]["to"][0]["namespaceSelector"],
        json!({"matchLabels":{"kubernetes.io/metadata.name":"kube-system"}})
    );
    assert_eq!(
        fixture.state()["objects"][format!("pod/{}", fixture.name())]["spec"]["containers"][0]
            ["command"],
        json!([
            "setpriv",
            "--no-new-privs",
            "--",
            "sh",
            "-c",
            "exec tail -f /dev/null"
        ])
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
fn replaced_allocation_policy_blocks_foreground_cascade() {
    let fixture = Fixture::new(Network::Unrestricted);
    fixture.provider.ensure(&spec()).unwrap();
    fixture.edit(|state| {
        state["objects"][format!("networkpolicy/{}", fixture.name())]["metadata"]["uid"] =
            "replacement-policy".into()
    });
    assert!(fixture
        .provider
        .destroy("allocation")
        .unwrap_err()
        .to_string()
        .contains("UID was replaced"));
    assert!(fixture.receipt().exists());
    assert!(!fixture
        .calls()
        .iter()
        .any(|call| call["operation"] == "delete"));
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
fn unavailable_host_python_refuses_before_cloud_calls_or_receipts() {
    let mut fixture = Fixture::new(Network::Deny);
    fixture.provider.control.python = fixture.scratch.join("missing-python3");
    assert!(fixture
        .provider
        .ensure(&spec())
        .unwrap_err()
        .to_string()
        .contains("Python"));
    assert!(fixture.calls().is_empty());
    assert!(!fixture.scratch.join("receipts").exists());
}

#[test]
fn nonzero_exec_keeps_binary_stderr_and_guest_trailer_lookalikes() {
    let fixture = Fixture::new(Network::Deny);
    fixture.provider.ensure(&spec()).unwrap();
    for code in [0, 7, 255] {
        let payload = format!(
            "printf 'guest\\000bytes\\377command terminated with exit code 7\\ncommand terminated with exit code 7\\n' >&2; exit {code}"
        );
        let mut process = fixture
            .provider
            .exec(
                "allocation",
                &ExecSpec {
                    argv: vec!["sh".into(), "-c".into(), payload],
                    cwd: fixture.scratch.path().to_path_buf(),
                    ..ExecSpec::default()
                },
            )
            .unwrap();
        drop(process.take_stdin());
        let mut stderr = Vec::new();
        process
            .take_stderr()
            .unwrap()
            .read_to_end(&mut stderr)
            .unwrap();
        assert_eq!(process.wait().unwrap().code, Some(code));
        assert_eq!(
            stderr,
            b"guest\0bytes\xffcommand terminated with exit code 7\ncommand terminated with exit code 7\n"
        );
        assert!(process.teardown().is_empty());
    }
}

#[test]
fn relay_startup_failure_and_timeout_end_local_kubectl_and_plugin_children() {
    use branchyard_sandbox::exec::{self, ExecGroup, ExecTransport};
    use branchyard_testkit::wait;
    use std::process::Command;
    use std::sync::Arc;
    use std::time::Duration;

    struct RelayTransport {
        root: PathBuf,
        fail: bool,
    }
    impl ExecTransport for RelayTransport {
        fn command(&self, _: &str, _: &[&str]) -> Command {
            let mut command = Command::new("python3");
            command.args(["-c", include_str!("kubectl_exec.py"), "python3", "-c", r#"
import os,subprocess,sys
from pathlib import Path
child=subprocess.Popen([sys.executable,'-c','import sys; sys.stdin.buffer.read()'],stdin=subprocess.PIPE)
Path(sys.argv[1],'kubectl.pid').write_text(str(os.getpid()))
Path(sys.argv[1],'plugin.pid').write_text(str(child.pid))
if sys.argv[2]=='failure':
 sys.stderr.buffer.write(b'by-sandbox-missing\n'); sys.stderr.buffer.flush()
sys.stdin.buffer.read()
"#]).arg(&self.root).arg(if self.fail { "failure" } else { "timeout" });
            command
        }
        fn describe(&self) -> String {
            "owned local relay fixture".into()
        }
        fn start_timeout(&self) -> Duration {
            Duration::from_secs(5)
        }
    }
    for fail in [true, false] {
        let root = Scratch::new("relay-startup-group");
        let result = exec::spawn(
            Arc::new(RelayTransport {
                root: root.path().to_path_buf(),
                fail,
            }),
            &ExecGroup::new("relay-startup").unwrap(),
            &ExecSpec {
                argv: vec!["true".into()],
                ..ExecSpec::default()
            },
        );
        assert!(result.is_err());
        for name in ["kubectl.pid", "plugin.pid"] {
            let pid: u32 = fs::read_to_string(root.join(name))
                .unwrap()
                .parse()
                .unwrap();
            wait::gone(pid);
        }
    }
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

fn store(bucket: &str) -> StoreVolume {
    StoreVolume {
        bucket: bucket.into(),
        path: "/opt/branchyard/store".into(),
    }
}

fn true_in(fixture: &Fixture) -> Result<(), branchyard_sandbox::ProviderError> {
    fixture
        .provider
        .exec(
            "allocation",
            &ExecSpec {
                argv: vec!["true".into()],
                cwd: fixture.scratch.path().to_path_buf(),
                ..ExecSpec::default()
            },
        )
        .map(|_| ())
}

#[test]
fn the_tool_store_is_a_read_only_cloud_storage_fuse_volume_at_the_hosts_path() {
    let fixture = Fixture::with_store(store("acme-tools"));
    fixture.provider.ensure(&spec()).unwrap();
    let name = fixture.name();
    let state = fixture.state();
    let sandbox = &state["objects"][format!("sandbox/{name}")]["spec"]["podTemplate"];
    assert_eq!(
        sandbox["metadata"]["annotations"]["gke-gcsfuse/volumes"],
        "true"
    );
    let volume = sandbox["spec"]["volumes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "tool-store")
        .unwrap();
    assert_eq!(
        *volume,
        json!({"name":"tool-store","csi":{
            "driver":"gcsfuse.csi.storage.gke.io","readOnly":true,
            "volumeAttributes":{"bucketName":"acme-tools",
                "mountOptions":"implicit-dirs,file-mode=755,dir-mode=755,file-cache:enable-parallel-downloads:true"}}})
    );
    let mounts = &sandbox["spec"]["containers"][0]["volumeMounts"];
    assert_eq!(
        mounts[4],
        json!({"name":"tool-store","mountPath":"/opt/branchyard/store","readOnly":true})
    );
    // The first four mounts are the ones every allocation has.
    assert_eq!(mounts.as_array().unwrap().len(), 5);
    true_in(&fixture).unwrap();

    // A store below a prefix in the bucket is mounted from there.
    let prefixed = Fixture::with_store(store("acme-tools/store/v1"));
    prefixed.provider.ensure(&spec()).unwrap();
    let state = prefixed.state();
    let options = &state["objects"][format!("sandbox/{}", prefixed.name())]["spec"]["podTemplate"]
        ["spec"]["volumes"][4]["csi"]["volumeAttributes"];
    assert_eq!(options["bucketName"], "acme-tools");
    assert_eq!(
        options["mountOptions"],
        "implicit-dirs,file-mode=755,dir-mode=755,file-cache:enable-parallel-downloads:true,only-dir=store/v1"
    );

    // Without a store the pod is what it was: no volume, no annotation.
    let plain = Fixture::new(Network::Deny);
    plain.provider.ensure(&spec()).unwrap();
    let state = plain.state();
    let template = &state["objects"][format!("sandbox/{}", plain.name())]["spec"]["podTemplate"];
    assert_eq!(template["spec"]["volumes"].as_array().unwrap().len(), 4);
    assert!(template["metadata"]["annotations"]
        .get("gke-gcsfuse/volumes")
        .is_none());
}

#[test]
fn a_bad_store_volume_is_refused_before_anything_is_created() {
    for (bucket, path) in [
        ("", "/opt/branchyard/store"),
        ("gs://acme", "/opt/branchyard/store"),
        ("acme tools", "/opt/branchyard/store"),
        ("acme,x", "/opt/branchyard/store"),
        ("acme", "relative/store"),
        ("acme", "/"),
    ] {
        let mut target = config();
        target.store = Some(StoreVolume {
            bucket: bucket.into(),
            path: path.into(),
        });
        assert!(target.validate().is_err(), "{bucket} {path}");
    }
    let mut target = config();
    target.store = Some(store("acme-tools/store"));
    target.validate().unwrap();
}

#[test]
fn the_sidecar_gke_injects_for_the_store_is_accepted_and_nothing_else_is() {
    let inject = |state: &mut Value, name: &str, extra: Option<(&str, Value)>| {
        let pod = &mut state["objects"][format!("pod/{name}")];
        pod["spec"]["initContainers"] =
            json!([{"name":"gke-gcsfuse-sidecar","image":"gcs-fuse-csi-driver-sidecar-mounter"}]);
        pod["spec"]["volumes"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"gke-gcsfuse-tmp","emptyDir":{}}));
        pod["status"]["initContainerStatuses"] =
            json!([{"name":"gke-gcsfuse-sidecar","ready":true,"state":{"running":{}}}]);
        if let Some((key, value)) = extra {
            match key {
                "container" => pod["spec"]["containers"]
                    .as_array_mut()
                    .unwrap()
                    .push(value),
                "volume" => pod["spec"]["volumes"].as_array_mut().unwrap().push(value),
                "init" => pod["spec"]["initContainers"]
                    .as_array_mut()
                    .unwrap()
                    .push(value),
                _ => unreachable!(),
            }
        }
    };
    let fixture = Fixture::with_store(store("acme-tools"));
    fixture.provider.ensure(&spec()).unwrap();
    let name = fixture.name();
    fixture.edit(|state| inject(state, &name, None));
    true_in(&fixture).unwrap();
    // As a regular container, as older clusters inject it.
    fixture.edit(|state| {
        let pod = &mut state["objects"][format!("pod/{name}")];
        pod["spec"]["initContainers"] = json!([]);
        pod["spec"]["containers"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"gke-gcsfuse-sidecar"}));
        pod["status"]["containerStatuses"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"gke-gcsfuse-sidecar","ready":true,"state":{"running":{}}}));
    });
    true_in(&fixture).unwrap();

    for (what, extra) in [
        ("container", json!({"name":"miner"})),
        ("init", json!({"name":"miner"})),
        ("volume", json!({"name":"hostpath","hostPath":{"path":"/"}})),
    ] {
        let fixture = Fixture::with_store(store("acme-tools"));
        fixture.provider.ensure(&spec()).unwrap();
        let name = fixture.name();
        fixture.edit(|state| inject(state, &name, Some((what, extra.clone()))));
        assert!(true_in(&fixture).is_err(), "{what}");
    }

    // An allocation with no store accepts no sidecar at all.
    let plain = Fixture::new(Network::Deny);
    plain.provider.ensure(&spec()).unwrap();
    let name = plain.name();
    plain.edit(|state| inject(state, &name, None));
    assert!(true_in(&plain).is_err());
}
