use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;

use branchyard_sandbox::{ExecSpec, Mount, Resources, SandboxProvider, SandboxSpec};
use branchyard_testkit::Scratch;
use serde_json::{json, Value};

use super::{
    AgentSandboxProvider, Config, CredentialSidecar, Egress, GatewayService, Network, Runtime,
    StoreVolume, GOOGLE_API_BLOCKS,
};

fn config() -> Config {
    Config {
        context: "test-context".into(),
        namespace: "branchyard-gvisor".into(),
        runtime: Runtime::Gvisor,
        network: Network::Deny,
        store: None,
        egress: Egress::default(),
        credential_proxy: None,
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

    fn with_egress(network: Network, store: Option<StoreVolume>, egress: Egress) -> Self {
        let mut fixture = Self::build(network, store);
        let mut config = fixture.provider.control.config.clone();
        config.egress = egress;
        fixture.provider.control.config = config;
        fixture
    }

    fn build(network: Network, store: Option<StoreVolume>) -> Self {
        Self::build_full(network, store, None)
    }

    fn build_full(
        network: Network,
        store: Option<StoreVolume>,
        sidecar: Option<CredentialSidecar>,
    ) -> Self {
        let scratch = Scratch::new("agent-sandbox");
        let mut config = config();
        config.network = network;
        config.store = store;
        config.credential_proxy = sidecar;
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
        mounts[5],
        json!({"name":"tool-store","mountPath":"/opt/branchyard/store","readOnly":true})
    );
    // The first five mounts are the ones every allocation has.
    assert_eq!(mounts.as_array().unwrap().len(), 6);
    true_in(&fixture).unwrap();

    // A store below a prefix in the bucket is mounted from there.
    let prefixed = Fixture::with_store(store("acme-tools/store/v1"));
    prefixed.provider.ensure(&spec()).unwrap();
    let state = prefixed.state();
    let options = &state["objects"][format!("sandbox/{}", prefixed.name())]["spec"]["podTemplate"]
        ["spec"]["volumes"][5]["csi"]["volumeAttributes"];
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
    assert_eq!(template["spec"]["volumes"].as_array().unwrap().len(), 5);
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

fn gateway() -> GatewayService {
    GatewayService {
        namespace: "branchyard-system".into(),
        name: "anvil-gateway".into(),
        port: 8931,
    }
}

fn egress_hosts() -> Egress {
    Egress {
        gateway: Some(gateway()),
        hosts: vec![
            "pypi.org".into(),
            "api.anthropic.com".into(),
            "*.pythonhosted.org".into(),
            "gateway.example.com".into(),
        ],
    }
}

fn object(fixture: &Fixture, kind: &str) -> Value {
    fixture.state()["objects"][format!("{kind}/{}", fixture.name())].clone()
}

#[test]
fn a_deny_allocation_with_a_store_still_reaches_cloud_storage_and_dns_and_nothing_else() {
    let fixture = Fixture::with_store(store("acme-tools"));
    fixture.provider.ensure(&spec()).unwrap();
    let policy = object(&fixture, "networkpolicy");
    assert_eq!(policy["spec"]["policyTypes"], json!(["Egress"]));
    let egress = policy["spec"]["egress"].as_array().unwrap();
    // Cluster DNS, and the Private Google Access ranges on 443: no 0.0.0.0/0, no gateway.
    assert_eq!(egress.len(), 2, "{egress:?}");
    assert_eq!(egress[0]["ports"][0]["port"], 53);
    assert_eq!(
        egress[1],
        json!({"to":[
            {"ipBlock":{"cidr":"199.36.153.4/30"}},
            {"ipBlock":{"cidr":"199.36.153.8/30"}}
        ],"ports":[{"protocol":"TCP","port":443}]})
    );
    assert_eq!(GOOGLE_API_BLOCKS, ["199.36.153.4/30", "199.36.153.8/30"]);
    assert!(!policy.to_string().contains("0.0.0.0/0"));
    // The FQDN policy names the Cloud Storage API and nothing else.
    let fqdn = object(&fixture, "fqdnnetworkpolicy");
    assert_eq!(fqdn["apiVersion"], "networking.gke.io/v1alpha1");
    assert_eq!(fqdn["kind"], "FQDNNetworkPolicy");
    assert_eq!(
        fqdn["spec"]["egress"],
        json!([{"matches":[{"name":"storage.googleapis.com"}],"ports":[{"protocol":"TCP","port":443}]}])
    );
    assert_eq!(
        fqdn["spec"]["podSelector"],
        json!({"matchLabels":{"branchyard.io/owner": policy["spec"]["podSelector"]["matchLabels"]["branchyard.io/owner"]}})
    );
    // Both belong to the Sandbox, so deleting it removes them; and the live pod passes the checks.
    for object in [&policy, &fqdn] {
        assert_eq!(object["metadata"]["ownerReferences"][0]["kind"], "Sandbox");
        assert_eq!(object["metadata"]["ownerReferences"][0]["controller"], true);
    }
    true_in(&fixture).unwrap();
    let state = fixture.state();
    assert!(
        state["objects"][format!("networkpolicy/{}", fixture.name())]["metadata"]["uid"]
            .is_string()
    );
    fixture.provider.destroy("allocation").unwrap();
    assert!(fixture.state()["objects"].as_object().unwrap().is_empty());
}

#[test]
fn an_orphaned_fqdn_policy_is_deleted_by_its_recorded_uid() {
    let fixture = Fixture::with_store(store("acme-tools"));
    fixture.provider.ensure(&spec()).unwrap();
    let recorded: Value = serde_json::from_slice(&fs::read(fixture.receipt()).unwrap()).unwrap();
    let fqdn_uid = recorded["fqdn_policy_uid"].as_str().unwrap().to_owned();
    fixture.edit(|state| {
        state["objects"]
            .as_object_mut()
            .unwrap()
            .remove(&format!("sandbox/{}", fixture.name()));
    });
    fixture.provider.destroy("allocation").unwrap();
    assert!(fixture.state()["objects"].as_object().unwrap().is_empty());
    let calls = fixture.calls();
    let deleted: Vec<_> = calls
        .iter()
        .filter(|call| call["operation"] == "delete")
        .map(|call| {
            call["body"]["preconditions"]["uid"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert!(deleted.contains(&fqdn_uid), "{deleted:?}");
    assert!(calls.iter().any(|call| call["args"].to_string().contains(
        "/apis/networking.gke.io/v1alpha1/namespaces/branchyard-gvisor/fqdnnetworkpolicies/"
    )));
}

#[test]
fn a_deny_allocation_lists_the_gateway_and_the_hosts_it_may_reach() {
    let fixture = Fixture::with_egress(Network::Deny, Some(store("acme-tools")), egress_hosts());
    fixture.provider.ensure(&spec()).unwrap();
    let policy = object(&fixture, "networkpolicy");
    let egress = policy["spec"]["egress"].as_array().unwrap();
    assert_eq!(egress.len(), 3, "DNS, the gateway Service, Cloud Storage");
    assert_eq!(
        egress[1],
        json!({"to":[{
            "namespaceSelector":{"matchLabels":{"kubernetes.io/metadata.name":"branchyard-system"}},
            "podSelector":{"matchLabels":{"app.kubernetes.io/name":"anvil-gateway"}}
        }],"ports":[{"protocol":"TCP","port":8931}]})
    );
    let fqdn = object(&fixture, "fqdnnetworkpolicy");
    assert_eq!(
        fqdn["spec"]["egress"][0]["matches"],
        json!([
            {"pattern":"*.pythonhosted.org"},
            {"name":"api.anthropic.com"},
            {"name":"gateway.example.com"},
            {"name":"pypi.org"},
            {"name":"storage.googleapis.com"}
        ])
    );
    true_in(&fixture).unwrap();
}

#[test]
fn a_deny_allocation_without_a_store_a_gateway_or_hosts_gets_no_policy_of_its_own() {
    let fixture = Fixture::new(Network::Deny);
    fixture.provider.ensure(&spec()).unwrap();
    assert!(fixture.state()["objects"]
        .as_object()
        .unwrap()
        .keys()
        .all(|key| !key.contains("policy")));
    // Only hosts: DNS and the names, no gateway rule, no Cloud Storage.
    let hosts = Egress {
        gateway: None,
        hosts: vec!["pypi.org".into()],
    };
    let named = Fixture::with_egress(Network::Deny, None, hosts);
    named.provider.ensure(&spec()).unwrap();
    assert_eq!(
        object(&named, "networkpolicy")["spec"]["egress"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        object(&named, "fqdnnetworkpolicy")["spec"]["egress"][0]["matches"],
        json!([{"name":"pypi.org"}])
    );
}

#[test]
fn unrestricted_keeps_its_open_policy_and_needs_no_name_list() {
    let fixture = Fixture::with_egress(
        Network::Unrestricted,
        Some(store("acme-tools")),
        egress_hosts(),
    );
    fixture.provider.ensure(&spec()).unwrap();
    let policy = object(&fixture, "networkpolicy");
    assert_eq!(
        policy["spec"]["egress"][0]["to"][0]["ipBlock"]["cidr"],
        "0.0.0.0/0"
    );
    assert!(object(&fixture, "fqdnnetworkpolicy").is_null());
}

#[test]
fn a_drifted_or_foreign_egress_policy_is_refused_before_exec() {
    for mutation in [
        "fqdn_hosts",
        "fqdn_extra",
        "policy_ports",
        "foreign_fqdn",
        "fqdn_gone",
    ] {
        let fixture = Fixture::with_store(store("acme-tools"));
        fixture.provider.ensure(&spec()).unwrap();
        let name = fixture.name();
        fixture.edit(|state| match mutation {
            "fqdn_hosts" => state["objects"][format!("fqdnnetworkpolicy/{name}")]["spec"]["egress"]
                [0]["matches"]
                .as_array_mut()
                .unwrap()
                .push(json!({"name":"evil.example.com"})),
            "fqdn_extra" => state["objects"][format!("fqdnnetworkpolicy/{name}")]["spec"]["egress"]
                .as_array_mut()
                .unwrap()
                .push(json!({"matches":[{"name":"evil.example.com"}]})),
            "policy_ports" => {
                state["objects"][format!("networkpolicy/{name}")]["spec"]["egress"][1]["ports"][0]
                    ["port"] = 80.into()
            }
            "foreign_fqdn" => {
                state["extra_fqdn_policies"] =
                    json!([{"metadata":{"name":"foreign"},"spec":{"podSelector":{}}}])
            }
            "fqdn_gone" => {
                state["objects"]
                    .as_object_mut()
                    .unwrap()
                    .remove(&format!("fqdnnetworkpolicy/{name}"));
            }
            _ => unreachable!(),
        });
        assert!(true_in(&fixture).is_err(), "{mutation}");
        assert!(
            !fixture
                .calls()
                .iter()
                .any(|call| call["operation"] == "exec"),
            "{mutation}"
        );
    }
}

fn with_sidecar(sidecar: CredentialSidecar) -> Fixture {
    Fixture::build_full(Network::Unrestricted, None, Some(sidecar))
}

#[test]
fn a_credential_proxy_sidecar_runs_beside_the_harness_with_the_home_read_only() {
    let image = format!("example.test/by@sha256:{}", "b".repeat(64));
    for (sidecar, expected) in [
        (CredentialSidecar::default(), spec().image.unwrap()),
        (
            CredentialSidecar {
                image: Some(image.clone()),
            },
            image,
        ),
    ] {
        let fixture = with_sidecar(sidecar);
        fixture.provider.ensure(&spec()).unwrap();
        let name = fixture.name();
        let state = fixture.state();
        let pod = &state["objects"][format!("sandbox/{name}")]["spec"]["podTemplate"]["spec"];
        let containers = pod["containers"].as_array().unwrap();
        assert_eq!(containers.len(), 2, "{containers:?}");
        assert_eq!(containers[0]["name"], "runtime");
        let sidecar = &containers[1];
        assert_eq!(sidecar["name"], "credential-proxy");
        assert_eq!(sidecar["image"], expected.as_str());
        // It reads its settings from the home the engine copies in, and
        // can neither change that home nor escalate.
        assert_eq!(
            sidecar["command"],
            json!([
                "by",
                "credential-proxy",
                "--config",
                "/branchyard/home/.branchyard/credential-proxy.json",
                "--wait"
            ])
        );
        assert_eq!(
            sidecar["volumeMounts"],
            json!([{"name":"home","mountPath":"/branchyard/home","readOnly":true}])
        );
        assert_eq!(
            sidecar["securityContext"]["allowPrivilegeEscalation"],
            false
        );
        assert_eq!(sidecar["securityContext"]["readOnlyRootFilesystem"], true);
        // The pod's identity is its Workload Identity, from the GKE
        // metadata server, which the pod's network policy leaves open.
        assert!(sidecar["env"]
            .as_array()
            .unwrap()
            .contains(&json!({"name":"GCE_METADATA_HOST","value":"169.254.169.254"})));
        let policy = &state["objects"][format!("networkpolicy/{name}")]["spec"]["egress"][0]["to"]
            [0]["ipBlock"]["except"];
        assert_eq!(*policy, json!(["169.254.169.252/32"]));
        // The pod with both containers passes the allocation's checks.
        true_in(&fixture).unwrap();
    }
}

#[test]
fn egress_hosts_and_the_gateway_service_are_validated_before_anything_is_created() {
    for hosts in [
        vec!["https://pypi.org"],
        vec!["pypi.org:443"],
        vec!["PyPI.org"],
        vec!["localhost"],
        vec!["10.0.0.1"],
        vec!["*.com."],
        vec!["a_b.example.com"],
        vec!["-x.example.com"],
        vec!["*"],
    ] {
        let mut target = config();
        target.egress.hosts = hosts.iter().map(|h| (*h).to_owned()).collect();
        assert!(target.validate().is_err(), "{hosts:?}");
    }
    let mut many = config();
    many.egress.hosts = (0..33).map(|n| format!("h{n}.example.com")).collect();
    assert!(many.validate().is_err());
    for bad in [
        GatewayService {
            namespace: "Bad".into(),
            ..gateway()
        },
        GatewayService {
            name: String::new(),
            ..gateway()
        },
        GatewayService {
            port: 0,
            ..gateway()
        },
    ] {
        let mut target = config();
        target.egress.gateway = Some(bad);
        assert!(target.validate().is_err());
    }
    let mut good = config();
    good.egress = egress_hosts();
    good.validate().unwrap();
    // An older receipt, written before `egress` existed, still matches a config without any.
    assert!(serde_json::to_value(config())
        .unwrap()
        .get("egress")
        .is_none());
}

#[test]
fn the_tool_tree_has_a_volume_of_its_own_at_the_guest_root() {
    let fixture = Fixture::new(Network::Deny);
    fixture.provider.ensure(&spec()).unwrap();
    let state = fixture.state();
    let template =
        &state["objects"][format!("sandbox/{}", fixture.name())]["spec"]["podTemplate"]["spec"];
    assert!(template["volumes"]
        .as_array()
        .unwrap()
        .contains(&json!({"name":"tools","emptyDir":{"sizeLimit":"64Mi"}})));
    assert!(template["containers"][0]["volumeMounts"]
        .as_array()
        .unwrap()
        .contains(&json!({"name":"tools","mountPath":"/tools"})));
    true_in(&fixture).unwrap();
}

#[test]
fn a_deny_allocation_with_a_sidecar_and_a_tool_store_keeps_its_rules_and_opens_only_the_metadata_server() {
    let fixture = Fixture::build_full(
        Network::Deny,
        Some(store("acme-tools")),
        Some(CredentialSidecar::default()),
    );
    fixture.provider.ensure(&spec()).unwrap();
    let name = fixture.name();
    let state = fixture.state();
    let pod = &state["objects"][format!("sandbox/{name}")]["spec"]["podTemplate"]["spec"];
    // The pod has the harness, the credential proxy and the tool store's mount.
    assert_eq!(pod["containers"].as_array().unwrap().len(), 2);
    assert!(pod["containers"][0]["volumeMounts"]
        .as_array()
        .unwrap()
        .contains(&json!({"name":"tools","mountPath":"/tools"})));
    let policy = &state["objects"][format!("networkpolicy/{name}")];
    let egress = policy["spec"]["egress"].as_array().unwrap();
    // DNS, the metadata server on port 80, and the Private Google Access
    // ranges; no 0.0.0.0/0.
    assert_eq!(egress.len(), 3, "{egress:?}");
    assert_eq!(egress[0]["ports"][0]["port"], 53);
    assert_eq!(
        egress[1],
        json!({"to":[{"ipBlock":{"cidr":"169.254.169.254/32"}}],"ports":[{"protocol":"TCP","port":80}]})
    );
    assert_eq!(egress[2]["ports"], json!([{"protocol":"TCP","port":443}]));
    assert!(!policy.to_string().contains("0.0.0.0/0"));
    true_in(&fixture).unwrap();
}

#[test]
fn without_a_sidecar_the_pod_has_one_container_and_the_metadata_server_stays_closed() {
    let fixture = Fixture::new(Network::Unrestricted);
    fixture.provider.ensure(&spec()).unwrap();
    let name = fixture.name();
    let state = fixture.state();
    let pod = &state["objects"][format!("sandbox/{name}")]["spec"]["podTemplate"]["spec"];
    assert_eq!(pod["containers"].as_array().unwrap().len(), 1);
    assert_eq!(
        state["objects"][format!("networkpolicy/{name}")]["spec"]["egress"][0]["to"][0]["ipBlock"]
            ["except"],
        json!(["169.254.169.254/32", "169.254.169.252/32"])
    );
}

#[test]
fn a_sidecar_needs_a_pinned_image_and_a_pod_with_a_stranger_in_it_fails() {
    let mut target = config();
    target.credential_proxy = Some(CredentialSidecar::default());
    target.validate().unwrap();
    target.credential_proxy = Some(CredentialSidecar {
        image: Some("example.test/by:latest".into()),
    });
    assert!(target.validate().is_err(), "an unpinned sidecar image");

    let fixture = with_sidecar(CredentialSidecar::default());
    fixture.provider.ensure(&spec()).unwrap();
    let name = fixture.name();
    fixture.edit(|state| {
        let pod = &mut state["objects"][format!("pod/{name}")];
        pod["spec"]["containers"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"miner"}));
        pod["status"]["containerStatuses"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"miner","ready":true,"state":{"running":{}}}));
    });
    assert!(true_in(&fixture).is_err());
}
