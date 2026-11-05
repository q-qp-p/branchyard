use branchyard_sandbox::ProviderError;
use serde_json::{json, Value};

use crate::receipt::Receipt;

/// The three private volumes of an allocation: the worktree, the home, and
/// the tool tree the engine copies in after it (docs/tools-model.md).
const VOLUMES: [&str; 3] = ["/workspace", "/branchyard/home", "/tools"];

pub(crate) fn refused(what: &str) -> ProviderError {
    ProviderError::Runtime(format!("container verification refused: {what}"))
}

pub(crate) fn hex_id(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

pub(crate) fn digest_image(image: &str) -> bool {
    image
        .rsplit_once("@sha256:")
        .is_some_and(|(repository, digest)| {
            !repository.is_empty()
                && !repository.starts_with('-')
                && !repository.chars().any(char::is_whitespace)
                && hex_id(digest)
        })
}

pub(crate) fn object(value: Value) -> Result<Value, ProviderError> {
    match value {
        Value::Array(mut values) if values.len() == 1 => Ok(values.remove(0)),
        _ => Err(refused("inspection must return exactly one object")),
    }
}

pub(crate) fn verify_identity(value: &Value, receipt: &Receipt) -> Result<String, ProviderError> {
    let id = value["Id"]
        .as_str()
        .filter(|id| hex_id(id))
        .ok_or_else(|| refused("invalid immutable container ID"))?;
    if receipt.id.as_deref().is_some_and(|saved| saved != id)
        || value["Name"] != format!("/{}", receipt.container_name())
        || value["Config"]["Labels"]["io.branchyard.owner"] != receipt.owner
        || value["Config"]["Labels"]["io.branchyard.spec"] != receipt.digest
    {
        return Err(refused("container identity or ownership changed"));
    }
    Ok(id.to_owned())
}

pub(crate) fn verify(value: &Value, receipt: &Receipt) -> Result<Vec<String>, ProviderError> {
    verify_identity(value, receipt)?;
    let host = &value["HostConfig"];
    let memory = u64::from(receipt.memory) * 1024 * 1024;
    for (path, expected) in [
        ("/Image", json!(receipt.image_id)),
        ("/Config/Image", json!(receipt.image)),
        ("/Config/User", json!("1000:1000")),
        ("/Config/WorkingDir", json!("/workspace")),
        (
            "/HostConfig/Runtime",
            json!(receipt.config.runtime.docker_name()),
        ),
        (
            "/HostConfig/NetworkMode",
            json!(receipt.config.network.docker_name()),
        ),
        ("/HostConfig/Privileged", json!(false)),
        ("/HostConfig/ReadonlyRootfs", json!(true)),
        ("/HostConfig/PidMode", json!("")),
        ("/HostConfig/IpcMode", json!("private")),
        ("/HostConfig/UTSMode", json!("")),
        ("/HostConfig/CgroupnsMode", json!("private")),
        (
            "/HostConfig/NanoCpus",
            json!(u64::from(receipt.cpus) * 1_000_000_000),
        ),
        ("/HostConfig/Memory", json!(memory)),
        ("/HostConfig/MemorySwap", json!(memory)),
        ("/HostConfig/PidsLimit", json!(256)),
        ("/HostConfig/CapDrop", json!(["ALL"])),
    ] {
        if value.pointer(path) != Some(&expected) {
            return Err(refused(&format!(
                "inspected {path} differs from the requested boundary"
            )));
        }
    }
    for key in [
        "Binds",
        "CapAdd",
        "Devices",
        "DeviceRequests",
        "VolumesFrom",
    ] {
        if !host[key].is_null() && host[key] != json!([]) {
            return Err(refused("unexpected host access"));
        }
    }
    let security = host["SecurityOpt"]
        .as_array()
        .ok_or_else(|| refused("missing security options"))?;
    if security.len() != 1
        || !matches!(
            security[0].as_str(),
            Some("no-new-privileges:true" | "no-new-privileges=true")
        )
    {
        return Err(refused("no-new-privileges is not enforced"));
    }
    if host["Tmpfs"] != json!({"/tmp":"rw,nosuid,nodev,noexec,size=67108864,mode=1777"}) {
        return Err(refused("unexpected temporary filesystem"));
    }
    let mounts = host["Mounts"]
        .as_array()
        .ok_or_else(|| refused("missing volume creation settings"))?;
    if mounts.len() != VOLUMES.len() {
        return Err(refused("unexpected volume creation settings"));
    }
    for destination in VOLUMES {
        let mount = mounts
            .iter()
            .find(|mount| mount["Target"] == destination)
            .ok_or_else(|| refused("missing private volume"))?;
        if mount["Type"] != "volume"
            || mount["Source"].as_str().is_some_and(|s| !s.is_empty())
            || mount["ReadOnly"] == true
            || mount["VolumeOptions"]["NoCopy"] == true
        {
            return Err(refused(
                "volume is not anonymous and initialized from the image",
            ));
        }
    }
    let mounts = value["Mounts"]
        .as_array()
        .ok_or_else(|| refused("missing actual mounts"))?;
    let mut volumes = Vec::new();
    let mut destinations = Vec::new();
    for mount in mounts {
        if mount["Type"] == "tmpfs" && mount["Destination"] == "/tmp" {
            continue;
        }
        let name = mount["Name"]
            .as_str()
            .filter(|name| hex_id(name))
            .ok_or_else(|| refused("mount is not an anonymous volume"))?;
        if mount["Type"] != "volume"
            || mount["Driver"] != "local"
            || mount["RW"] != true
            || !matches!(
                mount["Destination"].as_str(),
                Some("/workspace" | "/branchyard/home" | "/tools")
            )
        {
            return Err(refused("unexpected actual mount"));
        }
        volumes.push(name.to_owned());
        destinations.push(mount["Destination"].as_str());
    }
    volumes.sort();
    destinations.sort();
    destinations.dedup();
    let distinct = volumes.windows(2).all(|pair| pair[0] != pair[1]);
    if volumes.len() != VOLUMES.len()
        || !distinct
        || destinations.len() != VOLUMES.len()
        || (!receipt.volumes.is_empty() && receipt.volumes != volumes)
    {
        return Err(refused("private volume identities changed"));
    }
    if value["State"]["Running"] == true {
        let networks = value["NetworkSettings"]["Networks"]
            .as_object()
            .ok_or_else(|| refused("running container has no inspected network"))?;
        if networks.len() != 1 || !networks.contains_key(receipt.config.network.docker_name()) {
            return Err(refused("active network differs from the selected boundary"));
        }
    }
    Ok(volumes)
}
