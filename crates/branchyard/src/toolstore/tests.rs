use std::os::unix::fs::PermissionsExt;

use branchyard_testkit::Scratch;

use super::*;

/// A packager that serves the bundles it is given and writes a package
/// of two files for each.
#[derive(Debug)]
struct Fake {
    bundles: Vec<Bundle>,
}

impl Packager for Fake {
    fn served(&self) -> Result<Vec<Bundle>, String> {
        Ok(self.bundles.clone())
    }

    fn package(&self, bundle: &Bundle, out: &Path) -> Result<(), String> {
        fs::create_dir_all(out.join("bin")).map_err(|e| e.to_string())?;
        fs::write(
            out.join("SKILL.md"),
            format!("---\nname: {}\ndescription: A fake.\n---\n", bundle.id),
        )
        .map_err(|e| e.to_string())?;
        fs::write(out.join("bin").join(&bundle.id), "#!/bin/sh\n").map_err(|e| e.to_string())
    }

    fn index(&self, _: &Path, _: &[Bundle], _: &Path) -> Result<(), String> {
        Ok(())
    }
}

fn fake(ids: &[&str]) -> Fake {
    Fake {
        bundles: ids
            .iter()
            .map(|id| Bundle {
                id: (*id).to_owned(),
                path: PathBuf::from(format!("/bundles/{id}")),
                hash: blake3::hash(id.as_bytes()).to_hex().to_string(),
            })
            .collect(),
    }
}

/// A script that records its working directory and arguments in `log`,
/// then does `body`.
fn script(dir: &Path, name: &str, log: &Path, body: &str) -> String {
    let path = dir.join(name);
    fs::write(
        &path,
        format!(
            "#!/bin/sh\necho \"{name} $PWD $*\" >> '{}'\n{body}\n",
            log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path.display().to_string()
}

/// `gcloud` and `oras` over directories that stand for a bucket and a
/// registry. `gs://b/p` is `<bucket>/b/p`, and a reference is a directory
/// named by it.
fn programs(scratch: &Scratch) -> (Programs, PathBuf, PathBuf, PathBuf) {
    let bin = scratch.join("fakebin");
    fs::create_dir_all(&bin).unwrap();
    let (log, bucket, registry) = (
        scratch.join("calls.log"),
        scratch.join("bucket"),
        scratch.join("registry"),
    );
    let gcloud = script(
        &bin,
        "gcloud",
        &log,
        &format!(
            r#"src="$4"; dst="$5"
case "$src" in gs://*) src="{b}/${{src#gs://}}";; esac
case "$dst" in gs://*) dst="{b}/${{dst#gs://}}";; esac
mkdir -p "$dst"
cp -R "$src"/. "$dst"/"#,
            b = bucket.display()
        ),
    );
    let oras = script(
        &bin,
        "oras",
        &log,
        &format!(
            r#"cmd="$1"; shift
key() {{ printf %s "$1" | tr '/:' '__'; }}
case "$cmd" in
push)
  ref="$3"; shift 3
  mkdir -p "{r}/$(key "$ref")"
  for f in "$@"; do cp -R "${{f%%:*}}" "{r}/$(key "$ref")/"; done ;;
pull)
  ref="$1"; out="$3"
  [ -d "{r}/$(key "$ref")" ] || {{ echo "$ref: not found" >&2; exit 1; }}
  mkdir -p "$out"; cp -R "{r}/$(key "$ref")"/. "$out"/ ;;
esac"#,
            r = registry.display()
        ),
    );
    (Programs { gcloud, oras }, log, bucket, registry)
}

fn lines(log: &Path) -> Vec<String> {
    fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_skill_is_named_by_the_hash_of_its_files() {
    let scratch = Scratch::new("toolstore-skill");
    let index = publish(scratch.path(), None).unwrap();
    assert!(index.entries.iter().all(|e| e.kind == Kind::Skill));
    let find = index.entries.iter().find(|e| e.id == "find-tools").unwrap();
    let skill = crate::capabilities::catalogued()
        .unwrap()
        .into_iter()
        .find(|s| s.name == "find-tools")
        .unwrap();
    assert_eq!(find.digest, skill_digest(skill));
    assert_eq!(find.path, format!("skills/find-tools@{}", find.digest));
    let dir = scratch.join(&find.path);
    assert_eq!(packager::hash_dir(&dir).unwrap(), find.digest);
    assert_eq!(size_of(&dir).unwrap(), find.size);
    let store = ToolStore::new(scratch.path());
    assert_eq!(store.skill(skill), Some(dir));
    assert_eq!(store.connector("github", "nothing"), None);
}

#[test]
fn publishing_builds_connector_entries_by_bundle_hash_and_twice_changes_nothing() {
    let scratch = Scratch::new("toolstore-publish");
    let (store, cache) = (scratch.join("store"), scratch.join("cache"));
    let packager = fake(&["linear", "github"]);
    let first = publish(&store, Some((&packager, &cache))).unwrap();
    let connectors: Vec<_> = first
        .entries
        .iter()
        .filter(|e| e.kind == Kind::Connector)
        .collect();
    let ids: Vec<&str> = connectors.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["github", "linear"]);
    let github = &packager.bundles[1];
    assert_eq!(
        connectors[0].path,
        format!("connectors/github@{}", github.hash)
    );
    assert!(store.join(&connectors[0].path).join("bin/github").is_file());
    assert_eq!(
        ToolStore::new(&store).connector("github", &github.hash),
        Some(store.join(&connectors[0].path))
    );
    let before = fs::read_to_string(store.join(INDEX_FILE)).unwrap();
    let second = publish(&store, Some((&packager, &cache))).unwrap();
    assert_eq!(first, second);
    assert_eq!(fs::read_to_string(store.join(INDEX_FILE)).unwrap(), before);
    assert_eq!(read_index(&store).unwrap(), first);
    verify(&store, &first).unwrap();
}

#[test]
fn verify_names_the_entry_that_does_not_match_and_an_index_cannot_leave_the_store() {
    let scratch = Scratch::new("toolstore-verify");
    let packager = fake(&["github"]);
    let cache = scratch.join("cache");
    let index = publish(scratch.path(), Some((&packager, &cache))).unwrap();
    let github = index.entries.iter().find(|e| e.id == "github").unwrap();
    fs::write(scratch.join(&github.path).join("SKILL.md"), "tampered").unwrap();
    let error = verify(scratch.path(), &index).unwrap_err();
    assert!(error.contains(&github.path), "{error}");
    assert!(error.contains("digest"), "{error}");
    fs::remove_dir_all(scratch.join(&github.path)).unwrap();
    assert!(verify(scratch.path(), &index)
        .unwrap_err()
        .contains("missing"));
    assert_eq!(missing(scratch.path(), &index), vec![github.clone()]);

    for path in [
        "skills/../../etc@x",
        "skills/a@b/c",
        "/abs@x",
        "connectors/a@.hidden",
    ] {
        let hostile = Index {
            version: 1,
            entries: vec![Entry {
                id: "a".into(),
                kind: Kind::Skill,
                digest: "d".into(),
                size: 0,
                path: path.into(),
            }],
        };
        write_index(scratch.path(), &hostile).unwrap();
        let error = read_index(scratch.path()).unwrap_err();
        assert!(error.contains("not a store entry"), "{path}: {error}");
    }
}

#[test]
fn a_mirror_runs_gcloud_rsync_both_ways_and_checks_what_it_pulled() {
    let scratch = Scratch::new("toolstore-gcs");
    let (programs, log, bucket, _) = programs(&scratch);
    let (source, target) = (scratch.join("source"), scratch.join("target"));
    let packager = fake(&["github"]);
    publish(&source, Some((&packager, &scratch.join("cache")))).unwrap();

    mirror_to_gcs(&programs, &source, "gs://acme-tools/store").unwrap();
    assert!(bucket.join("acme-tools/store/store.json").is_file());
    let pulled = pull_gcs(&programs, "gs://acme-tools/store", &target).unwrap();
    assert_eq!(pulled, read_index(&source).unwrap());
    verify(&target, &pulled).unwrap();

    let calls = lines(&log);
    let gcloud = programs.gcloud.rsplit('/').next().unwrap();
    assert_eq!(
        calls[0],
        format!(
            "{gcloud} {} storage rsync --recursive {} gs://acme-tools/store",
            std::env::current_dir().unwrap().display(),
            source.display()
        )
    );
    assert_eq!(
        calls[1],
        format!(
            "{gcloud} {} storage rsync --recursive gs://acme-tools/store {}",
            std::env::current_dir().unwrap().display(),
            target.display()
        )
    );

    // A bucket whose files changed after the index was written is refused.
    let entry = &pulled.entries[0];
    let broken = bucket.join("acme-tools/store").join(&entry.path);
    fs::write(broken.join("SKILL.md"), "changed in the bucket").unwrap();
    let error = pull_gcs(&programs, "gs://acme-tools/store", &scratch.join("again")).unwrap_err();
    assert!(error.contains(&entry.path), "{error}");

    for bad in ["acme-tools/store", "gs://", "https://x/y"] {
        assert!(mirror_to_gcs(&programs, &source, bad).is_err(), "{bad}");
    }
}

#[test]
fn a_registry_push_sends_each_entry_then_the_index_and_a_pull_fetches_only_what_is_missing() {
    let scratch = Scratch::new("toolstore-oci");
    let (programs, log, _, registry) = programs(&scratch);
    let (source, target) = (scratch.join("source"), scratch.join("target"));
    let packager = fake(&["github"]);
    let index = publish(&source, Some((&packager, &scratch.join("cache")))).unwrap();
    let base = "us-docker.pkg.dev/acme/tools/store";

    push_oci(&programs, &source, base).unwrap();
    let calls = lines(&log);
    assert_eq!(calls.len(), index.entries.len() + 1);
    let github = index.entries.iter().find(|e| e.id == "github").unwrap();
    let push = calls
        .iter()
        .find(|c| c.contains(":github") || c.contains("/connectors/github:"))
        .unwrap();
    assert_eq!(
        *push,
        format!(
            "oras {}/connectors push --artifact-type {OCI_ENTRY_TYPE} {base}/connectors/github:{} github@{}",
            source.display(),
            github.key(),
            github.key()
        )
    );
    let last = calls.last().unwrap();
    assert_eq!(
        *last,
        format!(
            "oras {} push --artifact-type {OCI_INDEX_TYPE} {base}/index:latest store.json:application/json",
            source.display()
        )
    );
    assert!(fs::read_dir(&registry).unwrap().count() > index.entries.len());

    let pulled = pull_oci(&programs, base, &target).unwrap();
    assert_eq!(pulled, index);
    verify(&target, &pulled).unwrap();
    let after = lines(&log);
    let pulls = after.iter().filter(|c| c.contains(" pull ")).count();
    assert_eq!(pulls, 1 + index.entries.len(), "the index, then each entry");
    // A second pull finds everything and fetches only the index.
    pull_oci(&programs, base, &target).unwrap();
    let again = lines(&log);
    assert_eq!(
        again.iter().filter(|c| c.contains(" pull ")).count(),
        pulls + 1
    );

    for bad in ["no-slash", "https://r/x", "r/x:tag", "r/x@sha256:1"] {
        assert!(push_oci(&programs, &source, bad).is_err(), "{bad}");
    }
}

#[test]
fn a_missing_cli_says_what_to_install() {
    let scratch = Scratch::new("toolstore-missing");
    let programs = Programs {
        gcloud: scratch.join("no-gcloud").display().to_string(),
        oras: scratch.join("no-oras").display().to_string(),
    };
    publish(scratch.path(), None).unwrap();
    let error = mirror_to_gcs(&programs, scratch.path(), "gs://b/p").unwrap_err();
    assert!(
        error.contains("is not on PATH") && error.contains("Google Cloud CLI"),
        "{error}"
    );
    let error = push_oci(&programs, scratch.path(), "r.example/x").unwrap_err();
    assert!(error.contains("ORAS"), "{error}");
}
