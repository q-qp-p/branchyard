#!/usr/bin/env python3
"""GKE preparation is ownership-fenced and resumable, using fake provider tools."""

import copy
import hashlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from dataclasses import replace
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
import agent_sandbox_gke as gke

ROOT = Path(__file__).resolve().parents[1]
ASSET = b"hermetic controller fixture\n"
FAKE = r"""
import copy,json,os,sys
from pathlib import Path
tool=Path(sys.argv[0]).name
args=sys.argv[1:]
path=Path(os.environ['FAKE_STATE'])
state=json.loads(path.read_text())
with Path(os.environ['FAKE_LOG']).open('a') as log:
 log.write(json.dumps({'tool':tool,'args':args,'kubeconfig':os.environ.get('KUBECONFIG')})+'\n')
def done(value=''):
 path.write_text(json.dumps(state))
 print(json.dumps(value) if isinstance(value,(dict,list)) else value)
 sys.exit(0)
def fail(message='private-token-must-not-be-logged'):
 path.write_text(json.dumps(state))
 print(message,file=sys.stderr)
 sys.exit(1)
if tool=='gcloud':
 if args[0]=='version':done({'Google Cloud SDK':state.get('gcloud_version','590.0.1')})
 if args[:3]==['iam','service-accounts','describe']:done(state['account'])
 if args[:4]==['compute','networks','subnets','list']:done(state['subnets'])
 if args[:3]==['compute','routers','list']:done(state['routers'])
 if args[:3]==['compute','routes','list']:done(state['routes'])
 if args[:3]==['container','clusters','list']:
  done(state['other_clusters']+([state['cluster']] if state['cluster'] else []))
 if args[:3]==['container','clusters','describe']:
  if state.get('describe_error'):fail(state['describe_error'])
  if not state['cluster']:fail('NOT_FOUND')
  done(state['cluster'])
 if args[:3]==['container','clusters','create']:
  state['cluster']=copy.deepcopy(state['desired_cluster'])
  state['cluster']['nodePools']=[state['pools']['default-pool']]
  state['mutations'].append('cluster-create')
  done()
 if args[:3]==['container','node-pools','create']:
  name=args[3]
  if state.get('fail_pool')==name:
   del state['fail_pool']
   fail()
  state['cluster']['nodePools'].append(state['pools'][name])
  state['mutations'].append('pool-create:'+name)
  done()
 if args[:3]==['container','node-pools','describe']:
  done(next(pool for pool in state['cluster']['nodePools'] if pool['name']==args[3]))
 if args[:3]==['container','clusters','get-credentials']:
  Path(os.environ['KUBECONFIG']).write_text('private kubeconfig')
  done()
 fail('unexpected fake gcloud command')
if tool=='curl':
 output=Path(args[args.index('-o')+1])
 output.write_bytes(b'wrong asset' if state.get('corrupt_asset') else b'hermetic controller fixture\n')
 done()
if tool=='kubectl':
 args=[arg for arg in args if not arg.startswith(('--context=','--kubeconfig='))]
 if args[0]=='create' and '--dry-run=client' in args:
  if state.get('controller_json_stream'):
   done('\n'.join(json.dumps(item) for item in state['controller']['items'])+state.get('controller_stream_suffix',''))
  done(state['controller'])
 if args[0]=='get':
  kind,name=args[1:3]
  if kind=='runtimeclass':
   default=state.get('runtime_handler',{'gvisor':'gvisor','microvm':'kata-clh'}[name])
   done({'handler':state.get('runtime_handlers',{}).get(name,default)})
  ns=args[args.index('--namespace')+1] if '--namespace' in args else ''
  done(state['kube'].get(kind+'|'+ns+'|'+name,''))
 if args[0]=='apply':
  items=json.loads(Path(args[args.index('-f')+1]).read_text())['items']
  for item in items:
   m=item['metadata']
   state['kube'][item['kind']+'|'+m.get('namespace','')+'|'+m['name']]=item
  state['mutations'].append('kube-apply')
  done()
 if args[:2]==['rollout','status']:done('deployment rolled out')
 fail('unexpected fake kubectl command')
fail('unexpected tool')
"""


class AgentSandboxGke(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        fake = self.bin / "fake"
        fake.write_text(f"#!{sys.executable}\n" + FAKE)
        fake.chmod(0o755)
        for name in ("gcloud", "kubectl", "curl"):
            (self.bin / name).symlink_to(fake)
        self.state_file = self.root / "fake-state.json"
        self.log = self.root / "fake-log.jsonl"
        self.env = {"PATH": str(self.bin), "FAKE_STATE": str(self.state_file), "FAKE_LOG": str(self.log)}
        self.addCleanup(patch.stopall)
        patch.dict(os.environ, self.env).start()
        patch.object(gke, "CONTROLLER_SHA256", hashlib.sha256(ASSET).hexdigest()).start()
        self.config = gke.Config(
            project="test-project",
            zone="us-central1-a",
            cluster="test-sandbox",
            version="1.37.1-gke.3020000",
            service_account="12345-compute@developer.gserviceaccount.com",
            master_cidr="172.30.80.0/28",
            state_dir=self.root / "state",
        )
        pools = {}
        for pool in self.config.pools:
            pools[pool.name] = {
                "name": pool.name,
                "version": self.config.version,
                "status": "RUNNING",
                "initialNodeCount": pool.nodes,
                "management": {"autoUpgrade": False},
                "config": {
                    "machineType": pool.machine_type,
                    "diskSizeGb": pool.disk_gb,
                    "imageType": "COS_CONTAINERD",
                    "serviceAccount": self.config.service_account,
                    "labels": gke.pool_labels(self.config, pool),
                    "workloadMetadataConfig": {"mode": "GKE_METADATA"},
                    "oauthScopes": list(gke.NODE_SCOPES),
                    "sandboxConfig": {"type": pool.runtime.upper() if pool.runtime != "system" else "UNSPECIFIED"},
                    "advancedMachineFeatures": {"enableNestedVirtualization": pool.runtime == "microvm"},
                },
            }
        cluster = {
            "name": self.config.cluster,
            "location": self.config.zone,
            "status": "RUNNING",
            "resourceLabels": self.config.labels,
            "currentMasterVersion": self.config.version,
            "network": "default",
            "subnetwork": "default",
            "releaseChannel": {"channel": "UNSPECIFIED"},
            "workloadIdentityConfig": {"workloadPool": "test-project.svc.id.goog"},
            "networkConfig": {"datapathProvider": "ADVANCED_DATAPATH"},
            "privateClusterConfig": {
                "enablePrivateNodes": True,
                "enablePrivateEndpoint": True,
                "masterIpv4CidrBlock": self.config.master_cidr,
            },
            "nodePools": [],
        }
        self.state = {
            "account": {"email": self.config.service_account},
            "cluster": None,
            "desired_cluster": cluster,
            "pools": pools,
            "other_clusters": [{"name": "unrelated", "network": "other"}],
            "subnets": [
                {
                    "name": "default",
                    "region": "us-central1",
                    "network": "default",
                    "privateIpGoogleAccess": True,
                    "ipCidrRange": "10.128.0.0/20",
                    "secondaryIpRanges": [{"ipCidrRange": "10.80.0.0/14"}],
                }
            ],
            "routers": [
                {
                    "network": "default",
                    "region": "us-central1",
                    "nats": [{"sourceSubnetworkIpRangesToNat": "ALL_SUBNETWORKS_ALL_IP_RANGES"}],
                }
            ],
            "routes": [{"network": "default", "destRange": "0.0.0.0/0"}],
            "kube": {},
            "mutations": [],
            "controller": {
                "apiVersion": "v1",
                "kind": "List",
                "items": [
                    {"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "agent-sandbox-system"}},
                    {
                        "apiVersion": "apps/v1",
                        "kind": "Deployment",
                        "metadata": {"name": "agent-sandbox-controller", "namespace": "agent-sandbox-system"},
                        "spec": {
                            "template": {
                                "spec": {"containers": [{"name": "agent-sandbox-controller", "image": "tag-to-pin"}]}
                            }
                        },
                    },
                ],
            },
        }
        self.save()

    def save(self):
        self.state_file.write_text(json.dumps(self.state))

    def read_state(self):
        return json.loads(self.state_file.read_text())

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def apply(self):
        return gke.apply(self.config)

    def existing_cluster(self):
        self.state["cluster"] = {
            **copy.deepcopy(self.state["desired_cluster"]),
            "nodePools": list(copy.deepcopy(self.state["pools"]).values()),
        }

    def test_plan_is_offline_and_quantifies_the_complete_footprint(self):
        with patch.object(gke.subprocess, "run", side_effect=AssertionError("offline plan called a subprocess")):
            result = gke.plan(self.config)
        self.assertFalse(self.config.state_dir.exists())
        self.assertEqual(self.calls(), [])
        self.assertEqual(result["footprint"]["nodes"], 3)
        self.assertEqual(result["footprint"]["vcpus"], 10)
        self.assertEqual(result["footprint"]["boot_disk_gb"], 300)
        self.assertFalse(result["creates_iam_grants"])
        self.assertTrue(result["network"]["private_endpoint"])
        self.assertEqual(result["network"]["datapath_provider"], "ADVANCED_DATAPATH")
        self.assertEqual(result["network"]["metadata_egress"]["default"], "denied")
        self.assertTrue(result["controller"]["url"].endswith("/v1.0.9/sandbox-with-extensions.yaml"))
        self.assertNotIn("latest", result["controller"]["url"])
        commands = result["creation_commands"]
        self.assertIn("--enable-private-nodes", commands[0])
        self.assertIn("--enable-dataplane-v2", commands[0])
        self.assertIn("--sandbox=type=gvisor", commands[1])
        self.assertIn("--sandbox=type=microvm", commands[2])
        self.assertIn("--enable-nested-virtualization", commands[2])
        self.assertNotIn("--enable-nested-virtualization", commands[1])
        self.assertNotIn("cloud-platform", " ".join(commands[0]))
        self.assertEqual(self.config.fingerprint, replace(self.config, state_dir=self.root / "other").fingerprint)
        self.assertNotEqual(self.config.fingerprint, replace(self.config, master_cidr="172.30.90.0/28").fingerprint)

    def test_invalid_inputs_are_refused_before_any_tool_runs(self):
        for changes in (
            {"cluster": "bad name"},
            {"zone": "us-central1"},
            {"version": "1.36.1-gke.1"},
            {"version": "1.37.0-gke.4712999"},
            {"service_account": "not-an-account"},
            {"master_cidr": "172.30.80.1/28"},
            {"master_cidr": "127.0.0.0/28"},
            {"master_cidr": "192.0.2.0/28"},
            {"master_cidr": "::/28"},
            {"state_dir": Path("relative")},
            {"rollout_timeout": 0},
        ):
            with self.subTest(changes=changes), self.assertRaises(gke.SetupError):
                gke.plan(replace(self.config, **changes))
        self.assertEqual(self.calls(), [])

    def test_apply_is_pinned_private_and_reuses_owned_cloud_resources(self):
        result = self.apply()
        self.assertEqual(result["mode"], "applied")
        self.assertFalse(result["workloads_run"])
        self.assertEqual(self.config.state_dir.stat().st_mode & 0o777, 0o700)
        self.assertEqual(self.config.kubeconfig.stat().st_mode & 0o777, 0o600)
        state = self.read_state()
        controller = state["kube"]["Deployment|agent-sandbox-system|agent-sandbox-controller"]
        self.assertEqual(controller["spec"]["template"]["spec"]["containers"][0]["image"], gke.CONTROLLER_IMAGE)
        self.assertEqual(state["mutations"].count("cluster-create"), 1)
        self.assertEqual(len([x for x in state["mutations"] if x.startswith("pool-create")]), 2)
        before = copy.deepcopy(state["other_clusters"])
        self.apply()
        state = self.read_state()
        self.assertEqual(state["other_clusters"], before)
        self.assertEqual(state["mutations"].count("cluster-create"), 1)
        self.assertEqual(state["mutations"].count("kube-apply"), 2)
        for call in self.calls():
            if call["tool"] == "kubectl":
                self.assertIn(f"--context={self.config.context}", call["args"])
                self.assertIn(f"--kubeconfig={self.config.kubeconfig}", call["args"])
            self.assertEqual(call["kubeconfig"], str(self.config.kubeconfig))
        self.assertEqual(sum(call["tool"] == "curl" for call in self.calls()), 1)
        self.assertTrue(any("daemonset/anetd" in call["args"] for call in self.calls()))

    def test_controller_render_accepts_the_real_kubectl_json_stream(self):
        self.state["controller_json_stream"] = True
        self.save()
        self.assertEqual(self.apply()["mode"], "applied")
        controller = self.read_state()["kube"]["Deployment|agent-sandbox-system|agent-sandbox-controller"]
        self.assertEqual(controller["spec"]["template"]["spec"]["containers"][0]["image"], gke.CONTROLLER_IMAGE)

    def test_a_malformed_controller_stream_never_applies_kubernetes_objects(self):
        self.state["controller_json_stream"] = True
        self.state["controller_stream_suffix"] = "\nnot-json"
        self.save()
        with self.assertRaises(gke.SetupError):
            self.apply()
        self.assertNotIn("kube-apply", self.read_state()["mutations"])

    def test_managed_microvm_runtime_selects_the_kata_cloud_hypervisor_handler(self):
        self.state["runtime_handlers"] = {"microvm": "kata-clh"}
        self.save()
        self.assertEqual(self.apply()["mode"], "applied")

    def test_policies_bind_each_runtime_and_cover_all_containers(self):
        self.apply()
        kube = self.read_state()["kube"]
        for runtime in ("gvisor", "microvm"):
            namespace = kube[f"Namespace||branchyard-{runtime}"]
            self.assertEqual(namespace["metadata"]["labels"][gke.RUNTIME_LABEL], runtime)
            self.assertEqual(namespace["metadata"]["labels"][gke.NETWORK_POLICY_LABEL], "enforced-v2")
            network = kube[f"NetworkPolicy|branchyard-{runtime}|branchyard-default-deny"]["spec"]
            self.assertEqual(network, {"podSelector": {}, "policyTypes": ["Ingress", "Egress"]})
            for kind in ("sandbox", "pod"):
                name = f"{self.config.cluster}-{runtime}-{kind}"
                policy = kube[f"ValidatingAdmissionPolicy||{name}"]["spec"]
                self.assertEqual(policy["failurePolicy"], "Fail")
                self.assertEqual(
                    policy["matchConstraints"]["namespaceSelector"]["matchLabels"][gke.RUNTIME_LABEL], runtime
                )
                expressions = "\n".join(v["expression"] for v in policy["validations"])
                for required in (
                    "runtimeClassName",
                    f"== '{runtime}'",
                    "automountServiceAccountToken",
                    "runAsNonRoot",
                    "runAsUser",
                    "runAsGroup",
                    "hostNetwork",
                    "hostPath",
                    "serviceAccountToken",
                    "podCertificate",
                    "allowPrivilegeEscalation",
                    "'ALL'",
                    "resources.limits['cpu']",
                    "resources.limits['memory']",
                    "hostPort",
                    "downwardAPI",
                    "metadata.uid",
                    "m.readOnly",
                ):
                    self.assertIn(required, expressions)
                containers = policy["variables"][1]["expression"]
                self.assertIn("initContainers", containers)
                self.assertIn("ephemeralContainers", containers)
                binding = kube[f"ValidatingAdmissionPolicyBinding||{name}"]["spec"]
                self.assertEqual(binding["policyName"], name)
                self.assertEqual(binding["validationActions"], ["Deny"])

    def test_foreign_cluster_and_pool_are_never_adopted(self):
        for foreign in ("cluster", "pool"):
            with self.subTest(foreign=foreign):
                self.existing_cluster()
                if foreign == "cluster":
                    self.state["cluster"]["resourceLabels"] = {}
                else:
                    self.state["cluster"]["nodePools"][1]["config"]["labels"] = {}
                self.save()
                with self.assertRaisesRegex(gke.SetupError, "foreign"):
                    self.apply()
                self.assertEqual(self.read_state()["mutations"], [])

    def test_incompatible_cluster_and_pool_are_refused_before_mutation(self):
        for target, key, value in (
            ("cluster", "currentMasterVersion", "1.37.2-gke.1"),
            ("cluster", "autopilot", {"enabled": True}),
            ("cluster", "networkConfig", {"datapathProvider": "LEGACY_DATAPATH"}),
            ("pool", "sandboxConfig", {"type": "GVISOR"}),
            ("pool", "advancedMachineFeatures", {"enableNestedVirtualization": False}),
            ("pool", "oauthScopes", [*gke.NODE_SCOPES, "cloud-platform"]),
            ("pool", "diskSizeGb", 200),
        ):
            with self.subTest(target=target, key=key):
                self.existing_cluster()
                value_target = (
                    self.state["cluster"] if target == "cluster" else self.state["cluster"]["nodePools"][2]["config"]
                )
                value_target[key] = value
                self.save()
                with self.assertRaises(gke.SetupError):
                    self.apply()
                self.assertEqual(self.read_state()["mutations"], [])

    def test_cidr_overlap_is_refused_for_subnets_secondary_routes_and_other_clusters(self):
        baseline = copy.deepcopy(self.state)
        for source in ("primary", "secondary", "route", "cluster"):
            with self.subTest(source=source):
                self.state = copy.deepcopy(baseline)
                if source == "primary":
                    self.state["subnets"][0]["ipCidrRange"] = "172.30.0.0/16"
                elif source == "secondary":
                    self.state["subnets"][0]["secondaryIpRanges"] = [{"ipCidrRange": "172.30.80.0/24"}]
                elif source == "route":
                    self.state["routes"].append({"network": "default", "destRange": "172.16.0.0/12"})
                else:
                    self.state["other_clusters"].append(
                        {
                            "name": "foreign",
                            "network": "default",
                            "privateClusterConfig": {"masterIpv4CidrBlock": "172.30.80.0/28"},
                        }
                    )
                self.save()
                with self.assertRaisesRegex(gke.SetupError, "overlaps"):
                    self.apply()
                self.assertEqual(self.read_state()["mutations"], [])

    def test_preconditions_and_permission_failures_never_create_resources(self):
        baseline = copy.deepcopy(self.state)
        for broken in ("nat", "google-access", "account", "old-sdk", "permission"):
            with self.subTest(broken=broken):
                self.state = copy.deepcopy(baseline)
                if broken == "nat":
                    self.state["routers"] = []
                elif broken == "google-access":
                    self.state["subnets"][0]["privateIpGoogleAccess"] = False
                elif broken == "account":
                    self.state["account"]["disabled"] = True
                elif broken == "old-sdk":
                    self.state["gcloud_version"] = "580.0.0"
                else:
                    self.state["describe_error"] = "PERMISSION_DENIED private-token-must-not-be-logged"
                self.save()
                with self.assertRaises(gke.SetupError) as result:
                    self.apply()
                self.assertNotIn("private-token", str(result.exception))
                self.assertEqual(self.read_state()["mutations"], [])
        self.assertNotIn("private-token", (self.config.state_dir / "commands.jsonl").read_text())

    def test_corrupt_controller_is_refused_before_cloud_creation(self):
        self.state["corrupt_asset"] = True
        self.save()
        with self.assertRaisesRegex(gke.SetupError, "checksum"):
            self.apply()
        self.assertEqual(self.read_state()["mutations"], [])

    def test_partial_pool_creation_resumes_without_recreating_the_cluster(self):
        self.state["fail_pool"] = "branchyard-microvm"
        self.save()
        with self.assertRaises(gke.SetupError):
            self.apply()
        result = self.apply()
        self.assertEqual(result["mode"], "applied")
        state = self.read_state()
        self.assertEqual(state["mutations"].count("cluster-create"), 1)
        self.assertEqual(state["mutations"].count("pool-create:branchyard-gvisor"), 1)
        self.assertEqual(state["mutations"].count("pool-create:branchyard-microvm"), 1)

    def test_an_owned_master_route_is_reused_but_an_unowned_overlap_is_not(self):
        self.existing_cluster()
        self.state["routes"].append({"network": "default", "destRange": self.config.master_cidr})
        self.state["routes"].append({"network": "default", "destRange": "::/0"})
        self.save()
        self.apply()
        self.assertEqual(self.read_state()["mutations"], ["kube-apply"])

    def test_the_verified_clusters_managed_control_plane_subnet_is_reused(self):
        self.existing_cluster()
        self.state["subnets"].append(
            {
                "name": f"gke-{self.config.cluster}-5ef17c73-pe-subnet",
                "network": "default",
                "region": self.config.region,
                "ipCidrRange": self.config.master_cidr,
            }
        )
        self.save()
        self.apply()
        self.assertEqual(self.read_state()["mutations"], ["kube-apply"])

    def test_a_cached_controller_checksum_is_verified_on_retry(self):
        self.apply()
        (self.config.state_dir / "controller.yaml").write_bytes(b"tampered")
        self.state = self.read_state()
        self.state["mutations"] = []
        self.save()
        with self.assertRaisesRegex(gke.SetupError, "cached controller checksum"):
            self.apply()
        self.assertEqual(self.read_state()["mutations"], [])

    def test_foreign_controller_namespace_blocks_all_kubernetes_changes(self):
        self.existing_cluster()
        self.state["kube"]["Namespace||agent-sandbox-system"] = {"metadata": {"labels": {"other": "owner"}}}
        self.save()
        with self.assertRaisesRegex(gke.SetupError, "foreign"):
            self.apply()
        self.assertEqual(self.read_state()["mutations"], [])

    def test_cli_prints_plan_or_a_safe_failure(self):
        args = [
            f"--{key.replace('_', '-')}={value}"
            for key, value in {
                "project": self.config.project,
                "zone": self.config.zone,
                "cluster": self.config.cluster,
                "version": self.config.version,
                "service_account": self.config.service_account,
                "master_cidr": self.config.master_cidr,
                "state_dir": self.config.state_dir,
            }.items()
        ]
        stdout, stderr = io.StringIO(), io.StringIO()
        with redirect_stdout(stdout), redirect_stderr(stderr):
            self.assertEqual(gke.main(args), 0)
        self.assertEqual(json.loads(stdout.getvalue())["mode"], "plan")
        self.assertEqual(self.calls(), [])
        self.state["describe_error"] = "PERMISSION_DENIED private-token-must-not-be-logged"
        self.save()
        with redirect_stdout(stdout), redirect_stderr(stderr):
            self.assertEqual(gke.main([*args, "--apply"]), 1)
        self.assertNotIn("private-token", stderr.getvalue())

    def test_runner_rejects_bad_json_missing_tools_and_timeouts(self):
        self.config.state_dir.mkdir()
        runner = gke.Runner(self.config)
        with (
            patch.object(runner, "call", return_value="not json"),
            self.assertRaisesRegex(gke.SetupError, "invalid JSON"),
        ):
            runner.json(["gcloud"])
        with self.assertRaisesRegex(gke.SetupError, "FileNotFoundError"):
            runner.call(["missing-tool"])
        with (
            patch.object(gke.subprocess, "run", side_effect=subprocess.TimeoutExpired("fake", 1)),
            self.assertRaisesRegex(gke.SetupError, "TimeoutExpired"),
        ):
            runner.call(["gcloud"])


if __name__ == "__main__":
    unittest.main()
