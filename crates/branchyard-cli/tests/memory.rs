//! `by memory` through the built `by`, against temporary repositories and
//! the fake ACP agent (docs/memory.md, section 1): `add` writing a bundle
//! into `.agents/skills/` and, with `--mine`, into the person's own skills
//! folder; `list`, `show`, `edit`, `remove` and `export`; `add` refused
//! inside a harness and with `--remote`; a long correction becoming a
//! memory draft at `by knowledge distill` and added with `--from`; and a
//! personal memory staged for a branch the person starts, as its
//! `provisioned` record says. Hermetic; requires `git` and `sh`.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::fs;
use std::path::PathBuf;
use std::process::Output;

use serde_json::Value;

/// The kit's repository, plus what this file adds.
struct Repo(branchyard_testkit::Repo);

impl std::ops::Deref for Repo {
    type Target = branchyard_testkit::Repo;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Repo {
    fn new() -> Repo {
        let mut kit =
            branchyard_testkit::repo!(&[("a.txt", "one\n"), (".gitignore", ".branchyard/\n")]);
        kit.set_env("USER", "ana");
        Repo(kit)
    }

    /// Where `--mine` writes: the skills folder beside the test's user
    /// configuration file.
    fn mine_dir(&self) -> PathBuf {
        self.dir.join("user/skills")
    }

    fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.dir.join(name);
        fs::write(&path, text).unwrap();
        path
    }

    /// `by <args>` as a harness on `branch` would run it.
    fn in_harness(&self, branch: &str, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_by"))
            .args(args)
            .env("BRANCHYARD_BRANCH", branch)
            .output()
            .unwrap()
    }

    /// The `provisioned` activity of `branch`'s first turn.
    fn provisioned(&self, branch: &str) -> Value {
        let log = self.json(&["log", branch, "--json"]);
        log.as_array()
            .unwrap()
            .iter()
            .find(|e| e["activity"] == "provisioned")
            .cloned()
            .unwrap_or_else(|| panic!("{branch} recorded no provisioned activity: {log}"))
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn envelope(out: &Output) -> Value {
    let value: Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}):\n{}", stdout(out)));
    value["error"].clone()
}

const BENCH: &str = "The small fixture fits the cache, so its numbers say nothing.\n\nRun the \
                     benches with `bench/large.json` and compare against main.\n";

#[test]
fn add_list_show_edit_remove_and_export_write_the_bundles_of_a_topic() {
    let repo = Repo::new();
    let text = stdout(&repo.ok(&["memory", "list"]));
    assert!(
        text.starts_with("repository: no memory bundles under .agents/skills"),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "mine: no memory bundles in {}",
            repo.mine_dir().display()
        )),
        "{text}"
    );

    let body = repo.write("bench.md", BENCH);
    let added = repo.json(&[
        "memory",
        "add",
        "--title",
        "Run the parser benches with the 1 MB fixture",
        "--body-file",
        body.to_str().unwrap(),
        "--kind",
        "procedure",
        "--path",
        "crates/parser/**",
        "--task-kind",
        "tests",
        "--json",
    ]);
    let memory = &added["memory"];
    assert_eq!(memory["topic"], "parser", "{added}");
    assert_eq!(memory["id"], "run-the-parser-benches-with-the-1-mb-fixture");
    assert_eq!(memory["kind"], "procedure");
    assert_eq!(memory["approved_by"], "ana");
    assert_eq!(memory["task_kinds"], serde_json::json!(["tests"]));
    assert_eq!(added["mine"], false);
    let bundle = repo.root.join(".agents/skills/memory-parser");
    assert_eq!(
        added["bundles"],
        serde_json::json!([bundle.display().to_string()])
    );
    assert!(bundle.join("SKILL.md").is_file());
    assert!(bundle.join("memory.json").is_file());
    let reference = fs::read_to_string(
        bundle.join("references/run-the-parser-benches-with-the-1-mb-fixture.md"),
    )
    .unwrap();
    assert!(reference
        .starts_with("# Run the parser benches with the 1 MB fixture\n\nThe small fixture"));
    assert!(reference.contains("\nApproved by ana on 20"), "{reference}");

    // Text form: the next step is the commit and the pull request.
    let second = repo.write("second.md", "Releases ship on Thursdays.\n");
    let out = repo.ok(&[
        "memory",
        "add",
        "--title",
        "Releases ship on Thursdays",
        "--body-file",
        second.to_str().unwrap(),
        "--kind",
        "fact",
        "--topic",
        "releases",
    ]);
    let text = stdout(&out);
    assert!(
        text.starts_with("added releases-ship-on-thursdays to memory-releases (fact) under"),
        "{text}"
    );
    assert!(
        text.contains("commit .agents/skills/memory-releases and open the pull request; new branches stage it after by skills trust"),
        "{text}"
    );

    // --mine: the person's own folder, approved at once.
    let out = repo.ok(&[
        "memory",
        "add",
        "--mine",
        "--title",
        "Where the staging credentials live",
        "--body-file",
        second.to_str().unwrap(),
    ]);
    let text = stdout(&out);
    assert!(
        text.contains(&format!(
            "to memory-general (rule) under {}\nyour new branches stage it\n",
            repo.mine_dir().display()
        )),
        "{text}"
    );
    assert!(repo
        .mine_dir()
        .join("memory-general/references/where-the-staging-credentials-live.md")
        .is_file());
    assert!(!repo.root.join(".agents/skills/memory-general").exists());

    let listed = repo.json(&["memory", "list", "--json"]);
    assert_eq!(
        listed["repository"].as_array().unwrap().len(),
        2,
        "{listed}"
    );
    assert_eq!(listed["mine"].as_array().unwrap().len(), 1);
    assert_eq!(
        listed["mine"][0]["id"],
        "where-the-staging-credentials-live"
    );
    assert_eq!(listed["mine_dir"], repo.mine_dir().display().to_string());
    let only_mine = repo.json(&["memory", "list", "--mine", "--json"]);
    assert!(only_mine.get("repository").is_none(), "{only_mine}");
    let text = stdout(&repo.ok(&["memory", "list"]));
    assert!(text.contains("memory-parser (repository, 1)\n  run-the-parser-benches-with-the-1-mb-fixture  procedure  Run the parser benches with the 1 MB fixture\n"), "{text}");
    assert!(text.contains("memory-general (mine, 1)\n"), "{text}");

    let shown = repo.json(&[
        "memory",
        "show",
        "where-the-staging-credentials-live",
        "--json",
    ]);
    assert_eq!(shown["where"], "mine");
    assert_eq!(shown["memory"]["body"], "Releases ship on Thursdays.");
    let text = stdout(&repo.ok(&[
        "memory",
        "show",
        "run-the-parser-benches-with-the-1-mb-fixture",
    ]));
    assert!(
        text.starts_with("# Run the parser benches with the 1 MB fixture\n\nThe small fixture"),
        "{text}"
    );
    assert!(
        text.contains("\ntopic parser (repository), kind procedure, paths crates/parser/**, task kinds tests\n"),
        "{text}"
    );

    let edited = repo.json(&[
        "memory",
        "edit",
        "releases-ship-on-thursdays",
        "--title",
        "Releases ship on Thursdays, never Fridays",
        "--kind",
        "rule",
        "--json",
    ]);
    assert_eq!(
        edited["memory"]["title"],
        "Releases ship on Thursdays, never Fridays"
    );
    assert_eq!(edited["memory"]["kind"], "rule");
    assert_eq!(edited["memory"]["id"], "releases-ship-on-thursdays");
    let md = fs::read_to_string(repo.root.join(".agents/skills/memory-releases/SKILL.md")).unwrap();
    assert!(md.contains("## Rules\n\n- Releases ship on Thursdays, never Fridays (references/releases-ship-on-thursdays.md)"), "{md}");

    let out_dir = repo.dir.join("export");
    let exported = repo.json(&[
        "memory",
        "export",
        "--out",
        out_dir.to_str().unwrap(),
        "--json",
    ]);
    let names: Vec<&str> = exported["bundles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["memory-parser", "memory-releases"]);
    assert_eq!(
        fs::read_to_string(out_dir.join("memory-parser/SKILL.md")).unwrap(),
        fs::read_to_string(bundle.join("SKILL.md")).unwrap(),
        "export reproduces what add wrote"
    );
    assert_eq!(exported["bundles"][0]["digest"].as_str().unwrap().len(), 64);
    let text = stdout(&repo.ok(&["memory", "export"]));
    assert!(text.starts_with("memory-parser  "), "{text}");
    assert!(text.contains("  3 files, "), "{text}");

    let out = repo.ok(&["memory", "remove", "releases-ship-on-thursdays"]);
    assert!(
        stdout(&out).starts_with("removed releases-ship-on-thursdays from memory-releases; commit"),
        "{}",
        stdout(&out)
    );
    assert!(!repo.root.join(".agents/skills/memory-releases").exists());
    let missing = repo.by(&["memory", "show", "releases-ship-on-thursdays", "--json"]);
    assert_eq!(missing.status.code(), Some(2));
    let error = envelope(&missing);
    assert_eq!(error["kind"], "unknown_memory");
    assert_eq!(
        error["next"],
        serde_json::json!(["by memory list shows every slug"])
    );
    let removed = repo.json(&[
        "memory",
        "remove",
        "where-the-staging-credentials-live",
        "--json",
    ]);
    assert_eq!(removed["mine"], true);
    assert!(!repo.mine_dir().join("memory-general").exists());
}

#[test]
fn add_needs_a_title_and_a_body_and_is_refused_in_a_harness_and_remotely() {
    let repo = Repo::new();
    let out = repo.by(&["memory", "add", "--body-file", "x.md"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("a memory needs --title TEXT"),
        "{}",
        stderr(&out)
    );
    let out = repo.by(&["memory", "add", "--title", "T"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains(
            "next: by memory add --title TEXT --body-file FILE, or by memory add --from DRAFT"
        ),
        "{}",
        stderr(&out)
    );
    let body = repo.write("long.md", &"x".repeat(8_001));
    let out = repo.by(&[
        "memory",
        "add",
        "--title",
        "T",
        "--body-file",
        body.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let error = envelope(&out);
    assert_eq!(error["kind"], "invalid_memory");
    assert!(
        error["message"].as_str().unwrap().contains("at most 8000"),
        "{error}"
    );
    let out = repo.by(&[
        "memory",
        "add",
        "--title",
        "T",
        "--body-file",
        body.to_str().unwrap(),
        "--kind",
        "habit",
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("is not a memory kind"),
        "{}",
        stderr(&out)
    );

    let body = repo.write("ok.md", "Body.\n");
    for args in [
        vec![
            "memory",
            "add",
            "--title",
            "T",
            "--body-file",
            body.to_str().unwrap(),
        ],
        vec!["memory", "edit", "t", "--title", "U"],
        vec!["memory", "remove", "t"],
    ] {
        let out = repo.in_harness("some-branch", &args);
        assert_eq!(out.status.code(), Some(4), "{args:?}: {}", stderr(&out));
        let text = stderr(&out);
        assert!(
            text.starts_with(&format!(
                "by: a harness running on a branch cannot {} memory;",
                args[1]
            )),
            "{text}"
        );
        assert!(
            text.contains("next: say what you learned in your report; a person runs by memory"),
            "{text}"
        );
    }
    let out = repo.in_harness(
        "some-branch",
        &[
            "memory",
            "add",
            "--title",
            "T",
            "--body-file",
            body.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(envelope(&out)["kind"], "denied");
    assert!(!repo.root.join(".agents/skills").exists());
    // Reading is allowed inside a harness.
    assert!(repo
        .in_harness("some-branch", &["memory", "list"])
        .status
        .success());

    let token = repo.dir.join("token");
    fs::write(&token, "secret\n").unwrap();
    let out = repo.by(&[
        "--remote",
        "http://127.0.0.1:1",
        "--token-file",
        token.to_str().unwrap(),
        "--repo",
        "app",
        "memory",
        "list",
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let error = envelope(&out);
    assert_eq!(error["kind"], "usage");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("writes files in a worktree or in your own skills folder"),
        "{error}"
    );
}

#[test]
fn a_long_correction_becomes_a_draft_that_add_takes_from() {
    let repo = Repo::new();
    let out = repo.by_agent(&["run", "Add notes WRITE notes.txt=1", "--name", "notes"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let long = format!(
        "Always run the parser benches with the 1 MB fixture before you report numbers: {} WRITE notes.txt=2",
        "the small fixture fits the cache and says nothing about real inputs. ".repeat(10)
    );
    assert!(long.chars().count() > 600);
    let out = repo.by_agent(&["send", "notes", &long]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = repo.by_agent(&[
        "send",
        "notes",
        "Short correction, kept whole WRITE notes.txt=3",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));

    let distilled = repo.json(&["knowledge", "distill", "notes", "--deterministic", "--json"]);
    assert_eq!(
        distilled["proposed"].as_array().unwrap().len(),
        2,
        "{distilled}"
    );
    let drafts = distilled["drafts"].as_array().unwrap();
    assert_eq!(drafts.len(), 1, "{distilled}");
    assert_eq!(
        drafts[0]["slug"],
        "always-run-the-parser-benches-with-the-1-mb"
    );
    assert_eq!(drafts[0]["turn"], 2);
    assert_eq!(drafts[0]["via"], "send");
    assert_eq!(drafts[0]["text"], long.trim());
    let files = distilled["draft_files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    let draft = PathBuf::from(files[0].as_str().unwrap());
    assert_eq!(
        draft,
        repo.root
            .join(".branchyard/memory-drafts/always-run-the-parser-benches-with-the-1-mb.md")
    );
    let text = fs::read_to_string(&draft).unwrap();
    assert!(text.starts_with("---\ntitle: Always run the parser benches with the 1 MB fixture before you report numbers\nkind: rule\nsource: notes turn 2 (send)\n---\n\nAlways run"), "{text}");
    // The proposed entry holds the cut text; the draft the whole.
    let cut = distilled["proposed"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["text"].as_str().unwrap().starts_with("Always run"))
        .unwrap();
    assert!(cut["text"].as_str().unwrap().ends_with("..."));
    assert!(cut["text"].as_str().unwrap().chars().count() <= 600);

    // Distilling again writes the same draft once more, not a second file.
    let again = repo.json(&["knowledge", "distill", "notes", "--deterministic", "--json"]);
    assert_eq!(again["draft_files"], distilled["draft_files"]);
    let text_form = stdout(&repo.ok(&["knowledge", "distill", "notes", "--deterministic"]));
    assert!(
        text_form.contains("memory draft always-run-the-parser-benches-with-the-1-mb ("),
        "{text_form}"
    );
    assert!(
        text_form.contains("by memory add --from .branchyard/memory-drafts/always-run-the-parser-benches-with-the-1-mb.md"),
        "{text_form}"
    );

    let added = repo.json(&[
        "memory",
        "add",
        "--from",
        draft.to_str().unwrap(),
        "--kind",
        "procedure",
        "--path",
        "crates/parser/**",
        "--json",
    ]);
    assert_eq!(
        added["memory"]["title"],
        "Always run the parser benches with the 1 MB fixture before you report numbers"
    );
    assert_eq!(
        added["memory"]["kind"], "procedure",
        "the flag wins over the draft's kind"
    );
    assert_eq!(added["memory"]["topic"], "parser");
    assert_eq!(added["memory"]["body"], long.trim());
    assert_eq!(added["draft_removed"], draft.display().to_string());
    assert!(!draft.exists(), "the draft is a file until it is added");
    assert!(repo
        .root
        .join(".agents/skills/memory-parser/SKILL.md")
        .is_file());
}

#[test]
fn a_personal_memory_is_staged_for_the_branches_the_person_starts() {
    let repo = Repo::new();
    let body = repo.write("creds.md", "In the ops vault, under staging.\n");
    repo.ok(&[
        "memory",
        "add",
        "--mine",
        "--title",
        "Where the staging credentials live",
        "--body-file",
        body.to_str().unwrap(),
        "--kind",
        "fact",
    ]);
    let out = repo.by_agent(&["run", "WRITE f.txt=1", "--name", "mine"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let provisioned = repo.provisioned("mine");
    let entries = provisioned["skills"]["entries"].as_array().unwrap();
    let own = entries
        .iter()
        .find(|e| e["layer"] == "user")
        .unwrap_or_else(|| panic!("no user entry: {provisioned}"));
    assert_eq!(own["name"], "memory-general");
    assert!(
        provisioned["skills"].get("user_withheld").is_none(),
        "{provisioned}"
    );
    assert!(
        provisioned["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == ".branchyard/capabilities/user/memory-general/SKILL.md"),
        "{provisioned}"
    );
    let log = stdout(&repo.ok(&["log", "mine"]));
    assert!(log.contains("user:memory-general"), "{log}");
    let shown = repo.json(&["skills", "ls", "--json"]);
    assert_eq!(
        shown["user"]["skills"][0]["label"], "user:memory-general",
        "{shown}"
    );
    assert_eq!(shown["user"]["dir"], repo.mine_dir().display().to_string());

    // A harness on a branch starts children that inherit, and a run it
    // starts itself reads no folder of the person's.
    let out = repo.in_harness("mine", &["skills", "ls", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
}

/// The review correction two branches are sent after their task.
const REVIEW: &str = "File: notes.txt\nLine: 1\nUser comment: \"Always end notes with a summary \
                      line, and date it\"";

#[test]
fn turns_leave_scrubbed_observations_that_the_hidden_listing_shows() {
    let repo = Repo::new();
    for name in ["one", "two"] {
        let out = repo.by_agent(&["run", "Add notes WRITE notes.txt=1", "--name", name]);
        assert!(out.status.success(), "{}", stderr(&out));
        let out = repo.by_agent(&["send", name, REVIEW]);
        assert!(out.status.success(), "{}", stderr(&out));
    }
    let observations = repo.json(&["memory", "observations", "--json"]);
    let listed = observations.as_array().unwrap();
    assert_eq!(listed.len(), 2, "{observations}");
    assert_eq!(listed[0]["kind"], "correction");
    assert_eq!(listed[0]["branch"], "one");
    assert_eq!(listed[0]["turn"], 2);
    assert_eq!(listed[0]["visibility"], "shared");
    assert_eq!(listed[0]["paths"], serde_json::json!(["notes.txt"]));
    assert!(listed[0]["text"]
        .as_str()
        .unwrap()
        .starts_with("[review] Always end notes"));
    let text = stdout(&repo.ok(&["memory", "observations"]));
    assert!(text.contains("one turn 2 correction:"), "{text}");
    assert!(text.contains("two turn 2 correction:"), "{text}");
    let since = listed[0]["id"].as_u64().unwrap().to_string();
    let rest = repo.json(&["memory", "observations", "--since", &since, "--json"]);
    assert_eq!(rest.as_array().unwrap().len(), 1);
    // A private scope holds nothing: no session was private.
    assert_eq!(
        stdout(&repo.ok(&["memory", "observations", "--mine"])),
        "no observations\n"
    );
}

#[cfg(unix)]
#[test]
fn add_refuses_a_symlinked_bundle_directory_and_touches_nothing_outside_it() {
    let repo = Repo::new();
    fs::create_dir_all(repo.root.join(".agents/skills")).unwrap();
    std::os::unix::fs::symlink("../..", repo.root.join(".agents/skills/memory-general")).unwrap();
    let body = repo.write("body.md", "A body.\n");
    let out = repo.by(&[
        "memory",
        "add",
        "--title",
        "T",
        "--body-file",
        body.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let error = envelope(&out);
    assert_eq!(error["kind"], "invalid_memory");
    assert!(
        error["message"].as_str().unwrap().contains("symbolic link"),
        "{error}"
    );
    assert!(error["next"][0].as_str().is_some(), "{error}");
    assert!(repo.root.join(".git/HEAD").is_file());
    assert!(repo.root.join("a.txt").is_file());
    assert!(!repo.root.join("SKILL.md").exists());
    assert!(!repo.root.join("memory.json").exists());
}
