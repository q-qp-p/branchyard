//! `by tools publish` and `by tools pull` end to end, with a stand-in for
//! Anvil's packager and for `gcloud` and `oras`: shell scripts on `PATH`
//! that record their arguments and move files between directories that
//! stand for a bucket and a registry. No network, no cloud, no real Anvil.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::Value;

struct Kit {
    repo: branchyard_testkit::Repo,
    store: PathBuf,
    log: PathBuf,
    bucket: PathBuf,
    registry: PathBuf,
}

/// The Anvil mock, the one `tests/test_anvil_mock.py` tests against the contract.
fn anvil_mock() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/anvil/anvil")
}

fn script(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

impl Kit {
    /// A repository whose `branchyard.toml` names two bundles, the fake
    /// Anvil and a store, with `gcloud` and `oras` first on `PATH`. With
    /// `with_oras` false there is no `oras`.
    fn new(with_oras: bool) -> Kit {
        let mut repo = branchyard_testkit::repo!();
        let (bin, system) = (repo.dir.join("fakebin"), repo.dir.join("system"));
        fs::create_dir_all(&bin).unwrap();
        branchyard_testkit::system_tools(&system).unwrap();
        let (log, bucket, registry, store) = (
            repo.dir.join("calls.log"),
            repo.dir.join("bucket"),
            repo.dir.join("registry"),
            repo.dir.join("store"),
        );
        // Anvil is the contract's mock (docs/anvil-contract-v2.md), which logs each call.
        script(
            &bin,
            "anvil",
            &format!(
                "ANVIL_MOCK_LOG='{log}' exec python3 '{mock}' \"$@\"",
                log = log.display(),
                mock = anvil_mock().display()
            ),
        );
        script(
            &bin,
            "gcloud",
            &format!(
                r#"echo "gcloud $*" >> '{log}'
src="$4"; dst="$5"
case "$src" in gs://*) src="{b}/${{src#gs://}}";; esac
case "$dst" in gs://*) dst="{b}/${{dst#gs://}}";; esac
mkdir -p "$dst"; cp -R "$src"/. "$dst"/"#,
                log = log.display(),
                b = bucket.display()
            ),
        );
        if with_oras {
            script(
                &bin,
                "oras",
                &format!(
                    r#"echo "oras $PWD $*" >> '{log}'
cmd="$1"; shift
key() {{ printf %s "$1" | tr '/:' '__'; }}
case "$cmd" in
push) ref="$3"; shift 3; mkdir -p "{r}/$(key "$ref")"
  for f in "$@"; do cp -R "${{f%%:*}}" "{r}/$(key "$ref")/"; done ;;
pull) ref="$1"; out="$3"; mkdir -p "$out"; cp -R "{r}/$(key "$ref")"/. "$out"/ ;;
esac"#,
                    log = log.display(),
                    r = registry.display()
                ),
            );
        }
        let bundles = repo.dir.join("bundles");
        for id in ["github", "linear"] {
            fs::create_dir_all(bundles.join(id)).unwrap();
            fs::copy(
                anvil_mock()
                    .with_file_name("bundles")
                    .join(id)
                    .join("air.json"),
                bundles.join(id).join("air.json"),
            )
            .unwrap();
        }
        fs::write(
            repo.root.join("branchyard.toml"),
            format!(
                "[connectors]\nbundles = \"{}\"\nanvil = \"{}\"\n\n[tools]\nstore = \"{}\"\n",
                bundles.display(),
                bin.join("anvil").display(),
                store.display()
            ),
        )
        .unwrap();
        let path = if with_oras {
            format!("{}:{}", bin.display(), system.display())
        } else {
            let no_oras = repo.dir.join("no-oras");
            fs::create_dir_all(&no_oras).unwrap();
            for tool in ["gcloud", "anvil"] {
                std::os::unix::fs::symlink(bin.join(tool), no_oras.join(tool)).unwrap();
            }
            format!("{}:{}", no_oras.display(), system.display())
        };
        repo.set_env("PATH", &path);
        Kit {
            repo,
            store,
            log,
            bucket,
            registry,
        }
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn publish_builds_skills_and_connector_packages_and_an_index() {
    let kit = Kit::new(true);
    let said = kit.repo.json(&["tools", "publish", "--json"]);
    assert_eq!(said["connectors"], 2, "{said}");
    assert!(said["skills"].as_u64().unwrap() > 5, "{said}");
    assert_eq!(said["store"], kit.store.display().to_string());
    let index: Value =
        serde_json::from_str(&fs::read_to_string(kit.store.join("store.json")).unwrap()).unwrap();
    let entries = index["entries"].as_array().unwrap();
    let github = entries.iter().find(|e| e["id"] == "github").unwrap();
    assert_eq!(github["kind"], "connector");
    let path = github["path"].as_str().unwrap();
    assert!(path.starts_with("connectors/github@"), "{path}");
    assert!(kit.store.join(path).join("SKILL.md").is_file());
    // The package is the contract's: the search index travels into the store with it.
    let search: Value = serde_json::from_str(
        &fs::read_to_string(kit.store.join(path).join("search.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(search["schema"], "anvil.search/v1");
    assert_eq!(search["connector"], "github");
    assert!(kit.store.join(path).join("harness.json").is_file());
    assert!(kit
        .store
        .join(path)
        .join("reference/operations.md")
        .is_file());
    assert!(github["size"].as_u64().unwrap() > 0);
    assert_eq!(github["digest"].as_str().unwrap().len(), 64);
    let skill = entries.iter().find(|e| e["id"] == "find-tools").unwrap();
    assert!(skill["path"]
        .as_str()
        .unwrap()
        .starts_with("skills/find-tools@"));
    // Anvil packaged each bundle once; a second publish packages nothing.
    let packaged = kit
        .calls()
        .iter()
        .filter(|c| c.starts_with("anvil"))
        .count();
    assert_eq!(packaged, 2);
    let text = kit.repo.ok(&["tools", "publish"]);
    let text = String::from_utf8_lossy(&text.stdout).into_owned();
    assert!(text.contains("2 connector packages"), "{text}");
    assert_eq!(
        kit.calls()
            .iter()
            .filter(|c| c.starts_with("anvil"))
            .count(),
        2
    );
}

#[test]
fn publish_mirrors_to_a_bucket_and_pushes_to_a_registry_and_pull_fills_another_store() {
    let kit = Kit::new(true);
    let said = kit.repo.json(&[
        "tools",
        "publish",
        "--gcs",
        "gs://acme-tools/store",
        "--oci",
        "us-docker.pkg.dev/acme/tools/store",
        "--json",
    ]);
    assert_eq!(said["gcs"], "gs://acme-tools/store");
    assert_eq!(said["oci"], "us-docker.pkg.dev/acme/tools/store");
    let calls = kit.calls();
    assert!(
        calls.contains(&format!(
            "gcloud storage rsync --recursive {} gs://acme-tools/store",
            kit.store.display()
        )),
        "{calls:?}"
    );
    let pushes: Vec<_> = calls.iter().filter(|c| c.starts_with("oras")).collect();
    let entries = said["entries"].as_array().unwrap().len();
    assert_eq!(pushes.len(), entries + 1, "each entry, then the index");
    assert!(pushes
        .iter()
        .any(|c| c.contains("us-docker.pkg.dev/acme/tools/store/connectors/github:")));
    assert!(pushes.last().unwrap().ends_with(
        "push --artifact-type application/vnd.branchyard.tool-index.v1+json \
         us-docker.pkg.dev/acme/tools/store/index:latest store.json:application/json"
    ));

    // A second host pulls from the bucket, then from the registry.
    let (from_gcs, from_oci) = (kit.repo.dir.join("host-a"), kit.repo.dir.join("host-b"));
    let pulled = kit.repo.json(&[
        "tools",
        "pull",
        "--from",
        "gs://acme-tools/store",
        "--store",
        from_gcs.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(pulled["connectors"], 2, "{pulled}");
    assert_eq!(pulled["entries"], said["entries"]);
    assert!(kit.bucket.join("acme-tools/store/store.json").is_file());
    let pulled = kit.repo.json(&[
        "tools",
        "pull",
        "--oci",
        "us-docker.pkg.dev/acme/tools/store",
        "--store",
        from_oci.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(pulled["entries"], said["entries"]);
    for store in [&from_gcs, &from_oci] {
        for entry in said["entries"].as_array().unwrap() {
            let path = store.join(entry["path"].as_str().unwrap());
            assert!(path.join("SKILL.md").is_file(), "{}", path.display());
        }
    }
    assert!(fs::read_dir(&kit.registry).unwrap().count() > 0);
}

#[test]
fn pull_refuses_an_entry_that_does_not_match_the_index() {
    let kit = Kit::new(true);
    kit.repo
        .ok(&["tools", "publish", "--gcs", "gs://acme-tools/store"]);
    let github = fs::read_dir(kit.bucket.join("acme-tools/store/connectors"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(github.join("SKILL.md"), "changed in the bucket").unwrap();
    let target = kit.repo.dir.join("host");
    let out = kit.repo.by(&[
        "tools",
        "pull",
        "--from",
        "gs://acme-tools/store",
        "--store",
        target.to_str().unwrap(),
    ]);
    assert!(!out.status.success());
    let error = stderr(&out);
    assert!(error.contains("does not match its digest"), "{error}");
    assert!(error.contains("connectors/"), "{error}");
}

#[test]
fn mistakes_say_what_to_do_next() {
    let kit = Kit::new(false);
    // No oras on PATH.
    let out = kit
        .repo
        .by(&["tools", "publish", "--oci", "r.example/acme/store"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("oras is not on PATH"),
        "{}",
        stderr(&out)
    );
    // A bad destination is refused before anything is built.
    let fresh = kit.repo.dir.join("fresh");
    let out = kit.repo.by(&[
        "tools",
        "publish",
        "--store",
        fresh.to_str().unwrap(),
        "--gcs",
        "acme-tools",
    ]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("gs://BUCKET/PREFIX"),
        "{}",
        stderr(&out)
    );
    assert!(!fresh.exists());
    // A pull needs one source.
    let out = kit.repo.by(&["tools", "pull"]);
    assert!(!out.status.success());
    // No store named anywhere.
    fs::remove_file(kit.repo.root.join("branchyard.toml")).unwrap();
    let out = kit.repo.by(&["tools", "publish"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--store DIR"), "{}", stderr(&out));
}
