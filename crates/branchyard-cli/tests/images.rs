//! Toolchain images through the built `by` (docs/images.md): the build
//! context and its labels, pinned and unpinned harnesses, a build through a
//! fake `docker` that records its argv and answers `image inspect` with the
//! labels it was built with, the records, `by images pin`, the provider
//! fallback to `[images]`, and `by harnesses --image` reading labels from the
//! records, the docker daemon and a mock registry. Nothing is built, pulled or
//! pushed, no network is used beyond the mock registry on loopback, and no
//! model runs. Requires `git`, `sh` and `python3`.

#![allow(clippy::panic, clippy::unwrap_in_result, clippy::unwrap_used)] // tests: a panic is the failure report
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use branchyard_testkit::{MockHttp, Response};
use serde_json::{json, Value};

const BY: &str = env!("CARGO_BIN_EXE_by");
const FAKE_DOCKER: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake-docker");
const ANVIL_MOCK: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/anvil/anvil"
);

/// A repository, a `PATH` of temporary fakes and a private home.
struct World {
    _dir: tempfile::TempDir,
    base: PathBuf,
    bin: PathBuf,
    home: PathBuf,
    repo: PathBuf,
    docker: PathBuf,
}

fn write_exec(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

impl World {
    fn new() -> World {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let (bin, home, repo, docker) = (
            base.join("bin"),
            base.join("home"),
            base.join("repo"),
            base.join("docker-state"),
        );
        for d in [&bin, &home, &repo] {
            fs::create_dir_all(d).unwrap();
        }
        branchyard_testkit::system_tools(&base.join("tools")).unwrap();
        let world = World {
            _dir: dir,
            base,
            bin,
            home,
            repo,
            docker,
        };
        world.git(&["init", "-q", "-b", "main"]);
        world.git(&["config", "user.name", "Test"]);
        world.git(&["config", "user.email", "test@localhost"]);
        fs::write(world.repo.join("a.txt"), "one\n").unwrap();
        fs::write(
            world.repo.join(".gitignore"),
            "branchyard.toml\n.branchyard\n",
        )
        .unwrap();
        world.git(&["add", "."]);
        world.git(&["commit", "-q", "-m", "initial"]);
        world
    }

    /// `docker` on `PATH`: the fake.
    fn with_docker(&self) {
        write_exec(
            &self.bin.join("docker"),
            &format!(
                "#!/bin/sh\nFAKE_DOCKER_DIR='{}' exec python3 '{FAKE_DOCKER}' \"$@\"\n",
                self.docker.display()
            ),
        );
    }

    /// `anvil` on `PATH`: the contract's mock.
    fn with_anvil(&self) {
        write_exec(
            &self.bin.join("anvil"),
            &format!("#!/bin/sh\nexec python3 '{ANVIL_MOCK}' \"$@\"\n"),
        );
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        branchyard_testkit::hermetic(command.env_clear())
            .current_dir(&self.repo)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.bin.display(),
                    self.base.join("tools").display()
                ),
            )
            .env("HOME", &self.home)
            .env("BRANCHYARD_USER_CONFIG", self.base.join("user/config.toml"))
            .env("BRANCHYARD_TRUST_FILE", self.base.join("trusted.json"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("NO_COLOR", "1");
        command
    }

    fn git(&self, args: &[&str]) {
        let out = self.command("git").args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", text(&out.stderr));
    }

    fn by(&self, args: &[&str]) -> Output {
        self.command(BY)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> Output {
        let out = self.by(args);
        assert!(
            out.status.success(),
            "by {args:?}\nstdout:\n{}\nstderr:\n{}",
            text(&out.stdout),
            text(&out.stderr)
        );
        out
    }

    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_slice(&self.ok(args).stdout).unwrap()
    }

    /// The code and stderr of a command that must fail.
    fn fails(&self, args: &[&str]) -> (i32, String) {
        let out = self.by(args);
        assert!(
            !out.status.success(),
            "by {args:?} should fail\nstdout:\n{}",
            text(&out.stdout)
        );
        (out.status.code().unwrap(), text(&out.stderr))
    }

    /// A stand-in for the Linux `by` binary that goes into an image.
    fn fake_by(&self) -> String {
        let path = self.base.join("by-linux");
        fs::write(&path, "#!/bin/sh\necho by\n").unwrap();
        path.display().to_string()
    }

    fn project_config(&self, body: &str) {
        fs::write(self.repo.join("branchyard.toml"), body).unwrap();
    }

    /// The argv of every docker call, oldest first.
    fn docker_calls(&self) -> Vec<Vec<String>> {
        fs::read_to_string(self.docker.join("calls.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn records(&self) -> Value {
        serde_json::from_slice(&fs::read(self.repo.join(".branchyard/images.json")).unwrap())
            .unwrap()
    }
}

fn digest_of(reference: &str) -> &str {
    reference.rsplit_once('@').unwrap().1
}

#[test]
fn a_build_writes_a_deterministic_context_labels_it_and_records_the_digest() {
    let w = World::new();
    w.with_docker();
    let by = w.fake_by();
    let out = w.base.join("context");
    let args = [
        "images",
        "build",
        "--harness",
        "pi",
        "--harness",
        "codex",
        "--tag",
        "registry.test/team/tc:1",
        "--push",
        "--by-binary",
        &by,
        "--out",
        out.to_str().unwrap(),
        "--json",
    ];
    let built = w.json(&args);
    let reference = built["reference"].as_str().unwrap().to_owned();
    let prefix = "registry.test/team/tc:1@sha256:";
    assert!(
        reference.starts_with(prefix) && reference.len() == prefix.len() + 64,
        "{reference}"
    );
    assert_eq!(built["provider"], "container");
    assert_eq!(built["pushed"], true);
    assert_eq!(built["digest"].as_str().unwrap(), digest_of(&reference));

    // The label set: each harness at the version its install was pinned to, `by`, and the hash.
    let labels = built["labels"].as_object().unwrap();
    let codex = branchyard::inventory::checked_version("codex").unwrap();
    assert_eq!(labels["dev.branchyard.harness.codex"], codex);
    assert!(labels.contains_key("dev.branchyard.harness.pi"));
    // No qualified pin here: both versions come from the catalog.
    assert_eq!(labels["dev.branchyard.harness.codex.evidence"], "catalog");
    assert_eq!(labels["dev.branchyard.harness.pi.evidence"], "catalog");
    assert_eq!(labels["dev.branchyard.by"], env!("CARGO_PKG_VERSION"));
    let toolchain = labels["dev.branchyard.toolchain"].as_str().unwrap();
    assert!(
        toolchain.starts_with("sha256:") && toolchain.len() == 71,
        "{toolchain}"
    );

    // The context: the Dockerfile and the `by` binary, nothing secret.
    let dockerfile = fs::read_to_string(out.join("Dockerfile")).unwrap();
    assert!(dockerfile.contains("hold no credentials, tokens or keys"));
    assert!(dockerfile.contains(&format!("RUN npm install -g @openai/codex@{codex}\n")));
    assert!(dockerfile.contains(&format!("LABEL dev.branchyard.harness.codex=\"{codex}\"\n")));
    assert!(dockerfile.contains(&format!("LABEL dev.branchyard.toolchain=\"{toolchain}\"\n")));
    assert!(out.join("by").is_file());
    for secret in ["KEY", "TOKEN", "SECRET", "PASSWORD"] {
        assert!(
            !dockerfile
                .replace("no credentials, tokens or keys", "")
                .contains(secret),
            "{secret} in the Dockerfile"
        );
    }
    // The harnesses are in sorted order whatever order they were named in.
    let at = |needle: &str| dockerfile.find(needle).unwrap();
    assert!(at("# codex ") < at("# pi "));

    // buildx was asked once, to push, with the context.
    let calls = w.docker_calls();
    let build: Vec<&Vec<String>> = calls.iter().filter(|c| c[0] == "buildx").collect();
    assert_eq!(build.len(), 1, "{calls:?}");
    assert_eq!(
        &build[0][..4],
        ["buildx", "build", "--tag", "registry.test/team/tc:1"]
    );
    assert!(build[0].contains(&"--push".to_owned()));
    assert_eq!(build[0].last().unwrap(), out.to_str().unwrap());

    // The record, and the same build again: the same context, one record.
    assert_eq!(w.records()["images"][0]["reference"], reference);
    assert_eq!(w.records()["images"][0]["labels"], built["labels"]);
    let again = w.json(&args);
    assert_eq!(again["reference"], reference);
    assert_eq!(
        fs::read_to_string(out.join("Dockerfile")).unwrap(),
        dockerfile
    );
    assert_eq!(w.records()["images"].as_array().unwrap().len(), 1);

    // list and show.
    let listed = text(&w.ok(&["images", "list"]).stdout);
    // The table cuts a long reference short; its tag and the start of its digest stay.
    assert!(
        listed.contains(&reference[..60]) && listed.contains("container"),
        "{listed}"
    );
    assert!(listed.contains(&format!("codex {codex}")), "{listed}");
    let shown = w.json(&["images", "show", "registry.test/team/tc:1", "--json"]);
    assert_eq!(shown["reference"], reference);
    let shown_text = text(&w.ok(&["images", "show", digest_of(&reference)]).stdout);
    assert!(
        shown_text.contains(&format!("holds codex {codex}")),
        "{shown_text}"
    );
    let (code, message) = w.fails(&["images", "show", "nope"]);
    assert_eq!(code, 2, "{message}");
}

#[test]
fn a_qualified_pin_wins_over_the_catalog_pin_and_its_evidence_is_labelled() {
    let w = World::new();
    w.with_docker();
    let by = w.fake_by();
    let pin = |id: &str, package: &str, version: &str, live: Option<&str>| {
        branchyard::qualified::QualifiedPin {
            id: id.into(),
            version: version.into(),
            package: package.into(),
            profile: format!("{id}-profile"),
            integrity: None,
            qualified_at: "2026-11-01T00:00:00Z".into(),
            live_record: live.map(str::to_owned),
            checks: Vec::new(),
        }
    };
    let file = w.base.join("user/qualified.toml");
    branchyard::qualified::store_pin(&file, &pin("codex", "@openai/codex", "99.1.0", None))
        .unwrap();
    branchyard::qualified::store_pin(
        &file,
        &pin(
            "pi",
            "@mariozechner/pi-coding-agent",
            "98.2.0",
            Some("docs/qualification/pi.json"),
        ),
    )
    .unwrap();
    let out = w.base.join("context");
    let built = w.json(&[
        "images",
        "build",
        "--harness",
        "codex",
        "--harness",
        "pi",
        "--by-binary",
        &by,
        "--out",
        out.to_str().unwrap(),
        "--json",
    ]);
    let labels = built["labels"].as_object().unwrap();
    assert_eq!(labels["dev.branchyard.harness.codex"], "99.1.0");
    assert_eq!(labels["dev.branchyard.harness.codex.evidence"], "hermetic");
    assert_eq!(labels["dev.branchyard.harness.pi"], "98.2.0");
    assert_eq!(labels["dev.branchyard.harness.pi.evidence"], "live");
    let dockerfile = fs::read_to_string(out.join("Dockerfile")).unwrap();
    assert!(
        dockerfile.contains("RUN npm install -g @openai/codex@99.1.0\n"),
        "{dockerfile}"
    );
    assert!(dockerfile.contains("LABEL dev.branchyard.harness.codex.evidence=\"hermetic\"\n"));
    // The catalog's version is not what was installed.
    let catalog = branchyard::inventory::checked_version("codex").unwrap();
    assert!(
        !dockerfile.contains(&format!("codex@{catalog}")),
        "{dockerfile}"
    );
    let listed = text(&w.ok(&["images", "list"]).stdout);
    assert!(listed.contains("codex 99.1.0"), "{listed}");
}

#[test]
fn an_unpinned_harness_is_refused_unless_allowed_and_then_labelled_unpinned() {
    let w = World::new();
    w.with_docker();
    let by = w.fake_by();
    let out = w.base.join("context");
    let base = [
        "images",
        "build",
        "--harness",
        "claude-code",
        "--by-binary",
        &by,
        "--out",
        out.to_str().unwrap(),
    ];
    // Claude Code installs only by a piped script, which takes no version.
    let (code, message) = w.fails(&base);
    assert_eq!(code, 4, "{message}");
    assert!(
        message.contains("claude-code cannot be pinned"),
        "{message}"
    );
    assert!(message.contains("--allow-unpinned"), "{message}");
    assert!(!out.exists(), "nothing is written for a refused build");
    assert!(w.docker_calls().is_empty());

    let mut allowed = base.to_vec();
    allowed.extend(["--allow-unpinned", "--json"]);
    let built = w.json(&allowed);
    assert_eq!(
        built["labels"]["dev.branchyard.harness.claude-code"],
        "unpinned"
    );
    assert_eq!(built["harnesses"][0]["version"], "unpinned");
    assert!(built["harnesses"][0]["unpinned"].is_string());
    let dockerfile = fs::read_to_string(out.join("Dockerfile")).unwrap();
    assert!(dockerfile.contains("curl -fsSL https://claude.ai/install.sh | bash"));
    assert!(
        dockerfile.contains("apt-get install -y --no-install-recommends bash ca-certificates curl")
    );
    // Built locally, never pushed: no registry digest, so nothing to pin yet.
    assert_eq!(built["pushed"], false);
    assert!(built["digest"].is_null());
    let human = text(
        &w.ok(&{
            let mut a = base.to_vec();
            a.push("--allow-unpinned");
            a
        })
        .stdout,
    );
    assert!(human.contains("no registry digest yet"), "{human}");
}

#[test]
fn without_docker_the_context_is_left_and_the_command_printed() {
    let w = World::new();
    let by = w.fake_by();
    let out = w.base.join("context dir");
    let built = w.json(&[
        "images",
        "build",
        "--harness",
        "codex",
        "--tag",
        "t:1",
        "--by-binary",
        &by,
        "--out",
        out.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(built["built"], false);
    let command = built["command"].as_str().unwrap();
    assert!(
        command.starts_with("docker buildx build --tag t:1 --load "),
        "{command}"
    );
    assert!(
        command.contains("'"),
        "a path with a space is quoted: {command}"
    );
    assert!(out.join("Dockerfile").is_file());
    assert!(
        !w.repo.join(".branchyard/images.json").exists(),
        "nothing was built to record"
    );
    let human = text(
        &w.ok(&[
            "images",
            "build",
            "--harness",
            "codex",
            "--by-binary",
            &by,
            "--out",
            out.to_str().unwrap(),
        ])
        .stdout,
    );
    assert!(
        human.contains("docker is not on PATH, so nothing was built"),
        "{human}"
    );
}

#[test]
fn a_failed_build_is_a_failure_and_records_nothing() {
    let w = World::new();
    w.with_docker();
    let by = w.fake_by();
    let out = w
        .command(BY)
        .args([
            "images",
            "build",
            "--harness",
            "codex",
            "--by-binary",
            &by,
            "--out",
        ])
        .arg(w.base.join("context"))
        .env("FAKE_DOCKER_FAIL_BUILD", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("docker buildx build failed") && stderr.contains("could not be pulled"),
        "{stderr}"
    );
    assert!(!w.repo.join(".branchyard/images.json").exists());
}

#[test]
fn anvil_goes_into_the_image_when_configured_and_is_skipped_when_absent() {
    let w = World::new();
    w.with_docker();
    let by = w.fake_by();
    w.project_config("[connectors]\nanvil = \"anvil\"\n");
    let out = w.base.join("context");
    let args = [
        "images",
        "build",
        "--harness",
        "codex",
        "--by-binary",
        &by,
        "--out",
        out.to_str().unwrap(),
        "--json",
    ];
    // Configured, but no `anvil` on PATH: said, and the image is built without it.
    let skipped = w.by(&args);
    assert!(skipped.status.success(), "{}", text(&skipped.stderr));
    assert!(
        text(&skipped.stderr).contains("Anvil's toolchain is not in the image"),
        "{}",
        text(&skipped.stderr)
    );
    let report: Value = serde_json::from_slice(&skipped.stdout).unwrap();
    assert_eq!(report["anvil"]["state"], "skipped");
    assert!(report["labels"].get("dev.branchyard.anvil").is_none());
    assert!(!fs::read_to_string(out.join("Dockerfile"))
        .unwrap()
        .contains("/opt/anvil"));
    assert!(!out.join("anvil").exists());

    // With an Anvil that can install: its toolchain is copied in and its digest labelled.
    w.with_anvil();
    let with = w.json(&args);
    assert_eq!(with["anvil"]["state"], "installed");
    let digest = with["anvil"]["digest"].as_str().unwrap();
    assert_eq!(with["labels"]["dev.branchyard.anvil"], digest);
    let dockerfile = fs::read_to_string(out.join("Dockerfile")).unwrap();
    assert!(dockerfile.contains("COPY anvil /opt/anvil\n"));
    assert!(out.join("anvil/current").is_symlink());
    assert!(out.join("anvil/versions").join(digest).is_dir());
    assert!(!w.repo.join(".branchyard/images/.staging").exists());
    // The toolchain hash covers it: another label set, another hash.
    assert_ne!(
        with["labels"]["dev.branchyard.toolchain"],
        report["labels"]["dev.branchyard.toolchain"]
    );
}

#[test]
fn a_pin_refuses_a_tag_and_writes_the_key_the_provider_reads() {
    let w = World::new();
    let digest = format!("sha256:{}", "d".repeat(64));
    let pinned = format!("registry.test/team/tc:1@{digest}");

    let (code, message) = w.fails(&[
        "images",
        "pin",
        "registry.test/team/tc:1",
        "--provider",
        "container",
    ]);
    assert_eq!(code, 2, "{message}");
    assert!(message.contains("is a tag, not a digest"), "{message}");
    assert!(!w.repo.join("branchyard.toml").exists());
    let (code, message) = w.fails(&[
        "images",
        "pin",
        "registry.test/tc@sha256:abc",
        "--provider",
        "container",
    ]);
    assert_eq!(code, 2, "{message}");

    // No `[container]` table: a new `[images]` key, which `by config validate` accepts.
    let done = w.json(&[
        "images",
        "pin",
        &pinned,
        "--provider",
        "container",
        "--json",
    ]);
    assert_eq!(done["key"], "[images] container");
    let toml = fs::read_to_string(w.repo.join("branchyard.toml")).unwrap();
    assert!(
        toml.contains("[images]") && toml.contains(&format!("container = \"{pinned}\"")),
        "{toml}"
    );
    w.ok(&["config", "validate"]);
    let shown = text(&w.ok(&["config", "show"]).stdout);
    assert!(shown.contains("images.container"), "{shown}");

    // With the provider's own table in the project file, its `image` key is the one written.
    let other = format!("registry.test/team/tc:2@sha256:{}", "e".repeat(64));
    w.project_config(&format!(
        "[container]\nimage = \"old@sha256:{}\"\ncpus = 2\n",
        "a".repeat(64)
    ));
    let done = w.json(&["images", "pin", &other, "--provider", "container", "--json"]);
    assert_eq!(done["key"], "[container] image");
    let toml = fs::read_to_string(w.repo.join("branchyard.toml")).unwrap();
    assert!(
        toml.contains(&other) && toml.contains("cpus = 2") && !toml.contains("[images]"),
        "{toml}"
    );
    w.ok(&["config", "validate"]);

    // A provider that is not given and cannot be told.
    let (code, message) = w.fails(&["images", "pin", &other]);
    assert_eq!(code, 2, "{message}");
    assert!(message.contains("which provider"), "{message}");
}

#[test]
fn a_pin_reads_the_images_labels_so_a_turn_can_record_its_toolchain() {
    let w = World::new();
    w.with_docker();
    let by = w.fake_by();
    let built = w.json(&[
        "images",
        "build",
        "--harness",
        "codex",
        "--tag",
        "registry.test/team/tc:1",
        "--push",
        "--by-binary",
        &by,
        "--out",
        w.base.join("context").to_str().unwrap(),
        "--json",
    ]);
    let reference = built["reference"].as_str().unwrap();
    // Pinning what was built here keeps its record (its provider too).
    let done = w.json(&["images", "pin", reference, "--json"]);
    assert_eq!(done["provider"], "container");
    assert_eq!(done["labels_recorded"], true);

    // An image built elsewhere, known only to the docker daemon: pinned, its labels are adopted.
    let theirs = format!("elsewhere.test/team/tc:9@sha256:{}", "7".repeat(64));
    let labels = json!({
        "dev.branchyard.harness.codex": "0.157.1",
        "dev.branchyard.by": "0.4.0",
    });
    // Its toolchain label is the hash of the others; get it from the SDK's own rule.
    let mut raw: std::collections::BTreeMap<String, String> = labels
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
        .collect();
    let toolchain = branchyard::images::ImageLabels::from_map(&raw)
        .unwrap()
        .unwrap()
        .toolchain();
    raw.insert("dev.branchyard.toolchain".into(), toolchain.clone());
    fs::create_dir_all(&w.docker).unwrap();
    fs::write(
        w.docker.join("images.json"),
        json!({"elsewhere.test/team/tc:9": {"labels": raw, "digest": format!("sha256:{}", "7".repeat(64))}}).to_string(),
    )
    .unwrap();
    let done = w.json(&[
        "images",
        "pin",
        &theirs,
        "--provider",
        "agent-sandbox",
        "--json",
    ]);
    assert_eq!(done["labels_recorded"], true);
    assert_eq!(done["key"], "[images] agent_sandbox");
    let shown = w.json(&["images", "show", &theirs, "--json"]);
    assert_eq!(shown["labels"]["dev.branchyard.toolchain"], toolchain);
    assert_eq!(shown["provider"], "agent_sandbox");
}

#[test]
fn a_provider_named_without_an_image_takes_the_pinned_one_or_is_refused() {
    let w = World::new();
    for provider in ["container", "agent-sandbox", "microsandbox"] {
        let mut line = vec!["run", "fix it", "--provider", provider];
        if provider == "agent-sandbox" {
            line.extend(["--kube-context", "c", "--kube-namespace", "n"]);
        }
        let (code, message) = w.fails(&line);
        assert!(
            message.contains(&format!("--provider {provider} needs --image"))
                && message.contains("by images pin"),
            "{provider}: {message}"
        );
        assert_eq!(code, 2, "{message}");
    }
    // Pinned, the refusal is gone: the run goes on to the sandbox, which the
    // fake docker cannot be (it answers nothing but images), and fails there.
    let pinned = format!("registry.test/team/tc:1@sha256:{}", "d".repeat(64));
    w.with_docker();
    w.ok(&["images", "pin", &pinned, "--provider", "container"]);
    let out = w.by(&["run", "fix it", "--provider", "container", "--yes"]);
    let said = format!("{}{}", text(&out.stdout), text(&out.stderr));
    assert!(!said.contains("needs --image"), "{said}");
    let calls = w.docker_calls();
    assert!(
        calls.iter().any(|c| c.iter().any(|a| a == &pinned)),
        "the pinned image reached the provider: {calls:?}"
    );
    // An explicit --image still wins: the pinned image is not asked about again, and the
    // run is not refused for want of one (the fake docker cannot create a sandbox, so it
    // fails there, after settling its image).
    let before = w
        .docker_calls()
        .iter()
        .filter(|c| c.iter().any(|a| a == &pinned))
        .count();
    let flag = format!("registry.test/team/other:1@sha256:{}", "c".repeat(64));
    let again = w.by(&[
        "run",
        "fix it again",
        "--provider",
        "container",
        "--image",
        &flag,
        "--yes",
    ]);
    let said = format!("{}{}", text(&again.stdout), text(&again.stderr));
    assert!(!said.contains("needs --image"), "{said}");
    assert!(said.contains("could not create sandbox"), "{said}");
    let after = w
        .docker_calls()
        .iter()
        .filter(|c| c.iter().any(|a| a == &pinned))
        .count();
    assert_eq!(
        before,
        after,
        "the second run used the pinned image: {:?}",
        w.docker_calls()
    );
}

#[test]
fn harnesses_image_reads_the_labels_from_the_records_the_daemon_and_the_registry() {
    let w = World::new();
    w.with_docker();
    let by = w.fake_by();

    // From the record `by images build` wrote: docker is not asked.
    let built = w.json(&[
        "images",
        "build",
        "--harness",
        "codex",
        "--tag",
        "registry.test/team/tc:1",
        "--push",
        "--by-binary",
        &by,
        "--out",
        w.base.join("context").to_str().unwrap(),
        "--json",
    ]);
    let reference = built["reference"].as_str().unwrap();
    let before = w.docker_calls().len();
    let held = w.json(&["harnesses", "--image", reference, "--json"]);
    assert_eq!(held["labelled"], true);
    assert_eq!(
        held["harnesses"]["codex"],
        built["labels"]["dev.branchyard.harness.codex"]
    );
    assert_eq!(
        held["toolchain"],
        built["labels"]["dev.branchyard.toolchain"]
    );
    assert_eq!(
        w.docker_calls().len(),
        before,
        "a recorded image is read from the record"
    );
    let human = text(&w.ok(&["harnesses", "--image", reference]).stdout);
    assert!(human.contains("holds codex "), "{human}");

    // From the daemon: `image inspect`, never a run or a start.
    let mut daemon: Value =
        serde_json::from_slice(&fs::read(w.docker.join("images.json")).unwrap()).unwrap();
    daemon["daemon.test/plain:1"] =
        json!({"labels": {"maintainer": "me"}, "digest": format!("sha256:{}", "1".repeat(64))});
    fs::write(w.docker.join("images.json"), daemon.to_string()).unwrap();
    let plain = w.json(&["harnesses", "--image", "daemon.test/plain:1", "--json"]);
    assert_eq!(plain["labelled"], false);
    let human = text(
        &w.ok(&["harnesses", "--image", "daemon.test/plain:1"])
            .stdout,
    );
    assert!(
        human.contains("carries no dev.branchyard labels"),
        "{human}"
    );
    assert!(
        w.docker_calls()
            .iter()
            .all(|c| c[0] == "buildx" || c[..2] == ["image", "inspect"]),
        "{:?}",
        w.docker_calls()
    );
}

/// A registry that holds one image: a manifest (or an index of one) and its
/// configuration, behind an optional bearer-token handshake.
fn registry(labels: Value, index: bool, token: bool) -> MockHttp {
    let manifest_digest = format!("sha256:{}", "b".repeat(64));
    let config_digest = format!("sha256:{}", "c".repeat(64));
    MockHttp::start(move |request| {
        let path = request.path.split('?').next().unwrap().to_owned();
        if path == "/token" {
            assert!(
                request.path.contains("service=registry.test"),
                "{}",
                request.path
            );
            assert!(
                request.path.contains("scope=repository%3Ateam%2Ftc%3Apull"),
                "{}",
                request.path
            );
            return Response::json(200, &json!({"token": "t0k3n"}));
        }
        if path == "/cdn/config" {
            assert!(
                request.header("authorization").is_none(),
                "no credential goes to the CDN"
            );
            return Response::json(200, &json!({"config": {"Labels": labels}}));
        }
        if token && request.header("authorization") != Some("Bearer t0k3n") {
            let host = request.header("host").unwrap().to_owned();
            return Response::new(401, "").header(
                "WWW-Authenticate",
                &format!(
                    "Bearer realm=\"http://{host}/token\",service=\"registry.test\",scope=\"repository:team/tc:pull\""
                ),
            );
        }
        match path.as_str() {
            "/v2/team/tc/manifests/latest" | "/v2/team/tc/manifests/1" if index => Response::json(
                200,
                &json!({"manifests": [
                    {"digest": "sha256:ffff", "platform": {"os": "windows"}},
                    {"digest": manifest_digest, "platform": {"os": "linux"}},
                ]}),
            ),
            "/v2/team/tc/manifests/latest" | "/v2/team/tc/manifests/1" => {
                Response::json(200, &json!({"config": {"digest": config_digest}}))
            }
            p if p == format!("/v2/team/tc/manifests/{manifest_digest}") => {
                Response::json(200, &json!({"config": {"digest": config_digest}}))
            }
            p if p == format!("/v2/team/tc/blobs/{config_digest}") => {
                // A blob is often behind a redirect.
                Response::new(307, "").header(
                    "Location",
                    &format!("http://{}/cdn/config", request.header("host").unwrap()),
                )
            }
            _ => Response::new(404, ""),
        }
    })
}

#[test]
fn harnesses_image_reads_the_oci_config_from_a_registry_without_pulling() {
    let w = World::new();
    let mut raw = std::collections::BTreeMap::from([
        (
            "dev.branchyard.harness.codex".to_owned(),
            "0.157.1".to_owned(),
        ),
        ("dev.branchyard.harness.pi".to_owned(), "0.65.0".to_owned()),
        (
            "dev.branchyard.harness.codex.evidence".to_owned(),
            "hermetic".to_owned(),
        ),
        ("dev.branchyard.by".to_owned(), "0.4.0".to_owned()),
    ]);
    let toolchain = branchyard::images::ImageLabels::from_map(&raw)
        .unwrap()
        .unwrap()
        .toolchain();
    raw.insert("dev.branchyard.toolchain".into(), toolchain.clone());
    for (index, token) in [(false, false), (true, false), (true, true)] {
        let mock = registry(json!(raw), index, token);
        let reference = format!("{}/team/tc:1", mock.url().trim_start_matches("http://"));
        let held = w.json(&["harnesses", "--image", &reference, "--json"]);
        assert_eq!(held["labelled"], true, "index {index}, token {token}");
        assert_eq!(held["harnesses"]["codex"], "0.157.1");
        assert_eq!(held["harnesses"]["pi"], "0.65.0");
        assert_eq!(held["toolchain"], toolchain.as_str());
        let requests = mock.requests();
        // The manifest is asked for as OCI or Docker, and nothing is pulled: only
        // the manifest, perhaps its platform's, and the configuration blob.
        assert!(
            requests[0].path.ends_with("/manifests/1"),
            "{:?}",
            requests[0].path
        );
        let accept = requests[0].header("accept").unwrap();
        assert!(
            accept.contains("application/vnd.oci.image.manifest.v1+json"),
            "{accept}"
        );
        assert!(requests.iter().all(|r| r.method == "GET"));
        assert!(requests
            .iter()
            .all(|r| !r.path.contains("/layers/") && !r.path.contains("sha256:ffff")));
        assert!(requests.iter().any(|r| r.path.contains("/blobs/")));
        mock.finish();
    }
    // A registry that does not know the image.
    let mock = MockHttp::start(|_| Response::new(404, ""));
    let missing = format!("{}/team/none:1", mock.url().trim_start_matches("http://"));
    let (code, message) = w.fails(&["harnesses", "--image", &missing]);
    assert_eq!(code, 2, "{message}");
    assert!(message.contains("no label source holds"), "{message}");
}

#[test]
fn the_image_flag_names_no_machine() {
    let w = World::new();
    let (code, message) = w.fails(&["harnesses", "--image", "x", "--on", "ssh://h"]);
    assert_eq!(code, 2, "{message}");
}
