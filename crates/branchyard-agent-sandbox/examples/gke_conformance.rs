//! Explicit live GKE qualification. This example calls no model and never runs in default tests.

use std::error::Error;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use branchyard_agent_sandbox::{AgentSandboxProvider, Config, Network, Runtime};
use branchyard_sandbox::{ExecSpec, Process, Resources, SandboxProvider, SandboxSpec};
use serde_json::{json, Value};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn require(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(io::Error::other(message).into())
    }
}

fn exec_spec(argv: &[&str]) -> ExecSpec {
    ExecSpec {
        argv: argv.iter().map(|s| (*s).into()).collect(),
        cwd: "/workspace".into(),
        ..ExecSpec::default()
    }
}

fn finish(mut process: Box<dyn Process>, input: &[u8]) -> Result<(Option<i32>, Vec<u8>, Vec<u8>)> {
    if let Some(mut stdin) = process.take_stdin() {
        stdin.write_all(input)?;
    }
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut pipe) = process.take_stdout() {
        pipe.read_to_end(&mut stdout)?;
    }
    if let Some(mut pipe) = process.take_stderr() {
        pipe.read_to_end(&mut stderr)?;
    }
    let status = process.wait()?;
    process.teardown();
    Ok((status.code, stdout, stderr))
}

fn run(
    provider: &AgentSandboxProvider,
    name: &str,
    argv: &[&str],
    input: &[u8],
) -> Result<Vec<u8>> {
    let (code, stdout, stderr) = finish(provider.exec(name, &exec_spec(argv))?, input)?;
    require(
        code == Some(0),
        &format!(
            "guest command failed: {code:?}: {}",
            String::from_utf8_lossy(&stderr)
        ),
    )?;
    Ok(stdout)
}

fn kubectl(config: &Config, args: &[&str]) -> Result<Value> {
    let output = Command::new("kubectl")
        .args([
            "--context",
            &config.context,
            "--namespace",
            &config.namespace,
            "--request-timeout=30s",
        ])
        .args(args)
        .args(["-o", "json"])
        .output()?;
    require(
        output.status.success(),
        "read-only kubectl evidence collection failed",
    )?;
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn spawned(provider: &AgentSandboxProvider, name: &str) -> Result<(Box<dyn Process>, u32)> {
    let mut process =
        provider.exec(name, &exec_spec(&["sh", "-c", "sleep 300 & echo $!; wait"]))?;
    let stdout = process
        .take_stdout()
        .ok_or_else(|| io::Error::other("missing child PID pipe"))?;
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line)?;
    let pid = line.trim().parse()?;
    Ok((process, pid))
}

fn qualify(
    provider: &AgentSandboxProvider,
    config: &Config,
    name: &str,
    image: &str,
    evidence: &Path,
    row: &mut Value,
) -> Result<()> {
    eprintln!("{name}: ensure");
    let spec = SandboxSpec::new(name).image(image).resources(Resources {
        cpus: Some(1),
        memory_mib: Some(512),
    });
    provider.ensure(&spec)?;
    provider.ensure(&spec)?;
    require(
        provider.inspect(name)?.is_some(),
        "inspect lost the running allocation",
    )?;
    let resource = format!("by-{}", &blake3::hash(name.as_bytes()).to_hex()[..40]);
    let pod = kubectl(config, &["get", "pod", &resource])?;
    let sandbox = kubectl(config, &["get", "sandboxes.agents.x-k8s.io", &resource])?;
    fs::write(evidence.join("pod.json"), serde_json::to_vec_pretty(&pod)?)?;
    fs::write(
        evidence.join("sandbox.json"),
        serde_json::to_vec_pretty(&sandbox)?,
    )?;
    let node_name = pod["spec"]["nodeName"]
        .as_str()
        .ok_or_else(|| io::Error::other("missing node name"))?;
    let node = kubectl(config, &["get", "node", node_name])?;
    row["identity"] = json!({"sandbox_uid":sandbox["metadata"]["uid"],"pod_uid":pod["metadata"]["uid"],
        "node_name":node_name,"node_uid":node["metadata"]["uid"],"node_runtime":node["metadata"]["labels"]["sandbox.gke.io/runtime"],
        "kubelet_version":node["status"]["nodeInfo"]["kubeletVersion"],"node_kernel":node["status"]["nodeInfo"]["kernelVersion"],
        "runtime_class":pod["spec"]["runtimeClassName"],"image_id":pod["status"]["containerStatuses"][0]["imageID"],
        "resources":pod["spec"]["containers"][0]["resources"]});
    let network = if config.network == Network::Deny {
        "deny"
    } else {
        "unrestricted"
    };
    eprintln!("{name}: guest/network probe");
    let guest: Value = serde_json::from_slice(&run(
        provider,
        name,
        &["python3", "-", network],
        include_bytes!("gke_probe.py"),
    )?)?;
    row["guest"] = guest.clone();
    require(
        guest["uid"] == 1000 && guest["gid"] == 1000 && guest["root_readonly"] == true,
        "guest UID/root boundary differs",
    )?;
    require(
        guest["pod_uid"] == pod["metadata"]["uid"],
        "DownwardAPI Pod UID differs",
    )?;
    require(
        guest["process_status"]["CapEff"] == "0000000000000000"
            && guest["process_status"]["NoNewPrivs"] == "1"
            && guest["pid_one_status"]["CapEff"] == "0000000000000000"
            && guest["pid_one_status"]["NoNewPrivs"] == "1",
        "guest privilege boundary differs",
    )?;
    for key in [
        "docker_socket_visible",
        "kvm_visible",
        "service_account_token_visible",
    ] {
        require(
            guest[key] == false,
            "guest gained host or credential access",
        )?;
    }
    for path in ["/workspace", "/branchyard/home", "/tmp"] {
        require(
            guest["writable"][path] == true,
            "guest writable directory failed",
        )?;
    }
    for address in ["169.254.169.254", "169.254.169.252"] {
        require(
            guest["metadata"][address]["reachable"] == false,
            "metadata endpoint was reachable",
        )?;
    }
    require(
        guest["external_tcp_reachable"] == (config.network == Network::Unrestricted),
        "external network policy differs",
    )?;
    if config.network == Network::Unrestricted {
        require(
            guest["dns_resolved"] == true && guest["external_https_status"] == 200,
            "DNS/HTTPS egress failed",
        )?;
    }
    eprintln!("{name}: binary I/O and transfer");
    let binary: Vec<u8> = (0..=255).cycle().take(1024).collect();
    let (code, stdout, stderr) = finish(provider.exec(name, &exec_spec(&["python3", "-c", "import sys; data=sys.stdin.buffer.read(); sys.stdout.buffer.write(data[::-1]); sys.stderr.buffer.write(b'error\\x00\\xff'); sys.exit(7)"]))?, &binary)?;
    row["binary_observation"] =
        json!({"exit_code":code,"stdout_bytes":stdout.len(),"stderr_hex":hex::encode(&stderr)});
    require(
        code == Some(7)
            && stdout == binary.iter().rev().copied().collect::<Vec<_>>()
            && stderr == b"error\0\xff",
        "binary exec changed bytes or status",
    )?;
    fs::write(evidence.join("transfer.bin"), &binary)?;
    let archive = Command::new("tar")
        .args(["-cf", "-", "-C"])
        .arg(evidence)
        .arg("transfer.bin")
        .output()?;
    require(archive.status.success(), "local tar failed")?;
    run(
        provider,
        name,
        &["tar", "-xf", "-", "-C", "/workspace"],
        &archive.stdout,
    )?;
    let received = run(provider, name, &["cat", "/workspace/transfer.bin"], b"")?;
    require(received == binary, "uploaded binary file differs")?;
    let returned = run(
        provider,
        name,
        &["tar", "-cf", "-", "-C", "/workspace", "transfer.bin"],
        b"",
    )?;
    fs::write(evidence.join("returned.tar"), &returned)?;
    let restored = Command::new("tar")
        .args(["-xOf"])
        .arg(evidence.join("returned.tar"))
        .arg("transfer.bin")
        .output()?;
    require(
        restored.status.success() && restored.stdout == binary,
        "returned archive differs",
    )?;
    row["binary_exec_and_transfer"] =
        json!({"bytes":1024,"exit_code":7,"blake3":blake3::hash(&binary).to_hex().to_string()});
    require(
        provider
            .exec(name, &exec_spec(&["/nonexistent/branchyard-live-program"]))
            .is_err(),
        "missing program was accepted",
    )?;
    row["missing_program_refused"] = json!(true);
    eprintln!("{name}: cancellation and sibling isolation");
    let (mut first, first_pid) = spawned(provider, name)?;
    let (mut sibling, sibling_pid) = spawned(provider, name)?;
    first.kill()?;
    require(!first.wait()?.success(), "killed exec succeeded")?;
    first.teardown();
    let script = format!("import os,time\nfrom pathlib import Path\npid={first_pid}\nuntil=time.monotonic()+5\nwhile True:\n p=Path('/proc')/str(pid)/'status'\n dead=not p.exists() or '\\nState:\\tZ' in p.read_text()\n if dead:break\n if time.monotonic()>until:raise RuntimeError('cancelled descendant survived')\n time.sleep(0.05)\nos.kill({sibling_pid},0)\nprint('sibling_alive')\n");
    run(provider, name, &["python3", "-c", &script], b"")?;
    sibling.kill()?;
    sibling.wait()?;
    sibling.teardown();
    row["cancellation"] = json!({"first_descendant":first_pid,"sibling_descendant":sibling_pid,"sibling_survived_first_cancel":true});
    if config.network == Network::Deny {
        eprintln!("{name}: CPU ceiling and touched-memory probe");
        let cpu: Value = serde_json::from_slice(&run(
            provider,
            name,
            &["python3", "-", "cpu"],
            include_bytes!("gke_resources.py"),
        )?)?;
        let wall = cpu["wall_seconds"]
            .as_f64()
            .ok_or_else(|| io::Error::other("CPU wall time missing"))?;
        let used = cpu["cpu_seconds"]
            .as_f64()
            .ok_or_else(|| io::Error::other("CPU usage missing"))?;
        row["cpu_probe"] = cpu;
        require(
            used <= wall * 1.35,
            "four CPU workers exceeded the one-CPU ceiling",
        )?;
        let (code, stdout, stderr) = finish(
            provider.exec(name, &exec_spec(&["python3", "-", "memory"]))?,
            include_bytes!("gke_resources.py"),
        )?;
        row["memory_probe"] = json!({"exit_code":code,"stdout":String::from_utf8_lossy(&stdout),"stderr":String::from_utf8_lossy(&stderr),
            "pod_status_after":kubectl(config, &["get", "pod", &resource])?["status"]});
        require(
            code != Some(0)
                && !String::from_utf8_lossy(&stdout).contains("unexpected_one_gib_allocation"),
            "one GiB touched allocation escaped the 512 MiB limit",
        )?;
        if let Ok(observation) = run(
            provider,
            name,
            &["python3", "-", "observe"],
            include_bytes!("gke_resources.py"),
        ) {
            row["memory_probe"]["cgroups_after"] = serde_json::from_slice(&observation)?;
        }
    } else {
        // Stop must remove a child that deliberately escapes its exec group.
        let escaped = run(
            provider,
            name,
            &[
                "sh",
                "-c",
                "setsid sleep 300 </dev/null >/dev/null 2>&1 & echo $!",
            ],
            b"",
        )?;
        row["escaped_descendant_pid"] = json!(String::from_utf8_lossy(&escaped).trim());
        provider.stop(name)?;
        require(
            provider.inspect(name)?.is_none(),
            "stop retained its allocation",
        )?;
        row["stop_removed_allocation"] = json!(true);
    }
    eprintln!("{name}: complete; final owned cleanup follows");
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    require(args.len() == 7, "usage: gke_conformance CONTEXT NAMESPACE gvisor|microvm deny|unrestricted STATE_DIR EVIDENCE_DIR IMAGE@sha256:DIGEST")?;
    let runtime = match args[2].as_str() {
        "gvisor" => Runtime::Gvisor,
        "microvm" => Runtime::Microvm,
        _ => return Err(io::Error::other("invalid runtime").into()),
    };
    let network = match args[3].as_str() {
        "deny" => Network::Deny,
        "unrestricted" => Network::Unrestricted,
        _ => return Err(io::Error::other("invalid network").into()),
    };
    let state = PathBuf::from(&args[4]);
    let evidence = PathBuf::from(&args[5]);
    require(
        state.is_absolute()
            && evidence.is_absolute()
            && !state.starts_with(&evidence)
            && !evidence.starts_with(&state),
        "state/evidence must be distinct absolute directories",
    )?;
    fs::create_dir_all(&evidence)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&evidence, fs::Permissions::from_mode(0o700))?;
    }
    let config = Config {
        context: args[0].clone(),
        namespace: args[1].clone(),
        runtime,
        network,
        store: None,
        credential_proxy: None,
    };
    let provider = AgentSandboxProvider::new(config.clone(), &state)?;
    let name = format!("gke-live-{}-{}", args[2], args[3]);
    let mut row = json!({"started_at":branchyard_support::time::rfc3339(branchyard_support::time::now_ms()),"config":config,"image":args[6],"name":name});
    let result = qualify(&provider, &config, &name, &args[6], &evidence, &mut row);
    if let Err(error) = &result {
        row["failure"] = json!(error.to_string());
    }
    eprintln!("{name}: owned cleanup");
    let cleanup = provider.destroy(&name);
    row["cleanup"] = json!({"destroy_succeeded":cleanup.is_ok(),"error":cleanup.as_ref().err().map(ToString::to_string)});
    if cleanup.is_ok() {
        row["cleanup"]["inspect_absent"] = json!(provider.inspect(&name)?.is_none());
        row["cleanup"]["remaining_allocations"] =
            kubectl(&config, &["get", "sandboxes.agents.x-k8s.io,pods"])?["items"].clone();
        row["cleanup"]["remaining_allocation_policies"] = kubectl(
            &config,
            &["get", "networkpolicies", "-l", "branchyard.io/owner"],
        )?["items"]
            .clone();
    }
    row["passed"] = json!(result.is_ok() && cleanup.is_ok());
    fs::write(
        evidence.join("verification.json"),
        serde_json::to_vec_pretty(&row)?,
    )?;
    println!("{}", serde_json::to_string_pretty(&row)?);
    cleanup?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_and_requirement_boundaries_are_explicit() {
        assert_eq!(
            exec_spec(&["printf", "%s", "literal"]).argv,
            ["printf", "%s", "literal"]
        );
        assert_eq!(exec_spec(&["true"]).cwd, Path::new("/workspace"));
        assert!(require(true, "ok").is_ok());
        assert!(require(false, "refused").is_err());
    }
}
