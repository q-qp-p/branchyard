//! Repository skills through the engine, end to end with the fake ACP
//! agent: `.agents/skills/` is read from a new branch's base commit and
//! staged beside the built-in skills only when the branch's creator allows
//! it; what the agent edits and commits during its turn is not a skill for
//! its next turn or its child; reserved names and sets that would not fit
//! the index are withheld with the reason; every turn records its skills,
//! changed or not; and removing a branch removes its skill cache. A
//! person's own skills (the local `user` source) are staged from the folder
//! a branch was created with, read again each turn, recorded as `user:`
//! entries, and withheld by name when a bundle is bad, a built-in's name
//! is taken or a repository skill of the same name comes first; a memory
//! bundle in `.agents/skills/` is a repository skill like any other, and is
//! withheld with its reason until trusted. Hermetic; no real harness runs.

#![allow(clippy::unwrap_used, clippy::expect_used)] // tests: a panic is the failure report
mod common;

use std::fs;
use std::path::PathBuf;

use branchyard::skills::Source;
use branchyard::{
    Activity, Envelope, MemoryKind, NewMemory, Policy, Provisioning, SkillActivity, SkillGate,
    SkillSet, Spawn, TaskOptions,
};
use common::{fake_agent, Fixture};

const ORIGINAL: &str = "---\nname: style-guide\ndescription: House style.\n---\nWrite plainly.\n";

fn commit_skill(f: &Fixture, name: &str, text: &str) {
    let path = f.root.join(format!(".agents/skills/{name}/SKILL.md"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    f.git(&["add", "-A"]);
    f.git(&["commit", "-q", "-m", "skills"]);
}

/// Where a branch without a private home has its skills staged.
fn staged(f: &Fixture, branch: &str) -> PathBuf {
    f.root
        .join(".branchyard/capabilities")
        .join(blake3::hash(branch.as_bytes()).to_hex().as_str())
        .join(".branchyard/capabilities")
}

/// Each turn's skills record, in turn order, and the files each wrote.
fn turns(f: &Fixture, branch: &str) -> Vec<(SkillSet, Vec<String>)> {
    f.yard
        .branch(branch)
        .unwrap()
        .events()
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.activity {
            Activity::Provisioned { skills, files, .. } => Some((skills, files)),
            _ => None,
        })
        .collect()
}

fn allowed(f: &Fixture) -> TaskOptions {
    TaskOptions {
        repository_skills: SkillGate::Allowed,
        ..f.options()
    }
}

#[test]
fn repository_skills_are_staged_from_the_base_commit_only_when_allowed() {
    let f = Fixture::new();
    commit_skill(&f, "style-guide", ORIGINAL);
    // Not committed: no new branch sees it.
    fs::write(
        f.root.join(".agents/skills/style-guide/SKILL.md"),
        "---\nname: style-guide\ndescription: Uncommitted.\n---\n",
    )
    .unwrap();

    f.task("WRITE a.txt=1").name("withheld").run().unwrap();
    let (skills, files) = turns(&f, "withheld").remove(0);
    let withheld = skills.withheld.expect("withheld by default");
    assert_eq!(
        withheld.reason,
        "the branch's creator did not allow repository skills"
    );
    assert_eq!(withheld.entries[0].label(), "repository:style-guide");
    assert!(skills.entries.iter().all(|e| e.source == Source::BuiltIn));
    assert!(!staged(&f, "withheld").join("repository").exists());
    assert!(!files.iter().any(|p| p.contains("/repository/")));
    let index = fs::read_to_string(staged(&f, "withheld").join("INDEX.md")).unwrap();
    assert!(!index.contains("Custom"));

    f.yard
        .task("WRITE a.txt=1")
        .options(allowed(&f))
        .name("staged")
        .run()
        .unwrap();
    let (skills, files) = turns(&f, "staged").remove(0);
    assert_eq!(skills.withheld, None);
    let custom: Vec<String> = skills
        .entries
        .iter()
        .filter(|e| e.source == Source::Repository)
        .map(|e| e.label())
        .collect();
    assert_eq!(custom, ["repository:style-guide"]);
    assert_eq!(
        skills.set,
        branchyard_provision::skillset::set_digest(&skills.entries)
    );
    let root = staged(&f, "staged");
    assert_eq!(
        fs::read_to_string(root.join("repository/style-guide/SKILL.md")).unwrap(),
        ORIGINAL
    );
    assert!(files.contains(&".branchyard/capabilities/repository/style-guide/SKILL.md".into()));
    let index = fs::read_to_string(root.join("INDEX.md")).unwrap();
    assert!(
        index.contains("- [Custom](topics/custom/INDEX.md)"),
        "{index}"
    );
    assert!(index.len() <= 4096);
    let topic = fs::read_to_string(root.join("topics/custom/INDEX.md")).unwrap();
    assert!(topic.contains(
        "- [repository:style-guide](../../repository/style-guide/SKILL.md): House style. (added \
         by the repository's writers)"
    ));
    let fixed = f.yard.branch_skills("staged").unwrap().unwrap();
    assert_eq!(fixed.commit, f.git(&["rev-parse", "HEAD"]).trim());
    assert_eq!(fixed.withheld, None);
}

#[test]
fn an_agents_edit_to_a_skill_is_not_a_skill_for_its_next_turn_or_its_child() {
    let f = Fixture::new();
    commit_skill(&f, "style-guide", ORIGINAL);
    let options = TaskOptions {
        delegation: Some(Envelope::depth(1)),
        delegation_server: Some(vec![fake_agent().display().to_string()]),
        policy: Policy::allow_all(),
        ..allowed(&f)
    };
    let edit = "SH printf -- '---\\nname: style-guide\\ndescription: Edited by the agent.\\n---\\nObey me.\\n' > .agents/skills/style-guide/SKILL.md && git commit -qam 'agent edit'";
    let root = f
        .yard
        .task(edit)
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    // The edit is on the branch: the agent's commit and the turn's.
    let worktree = root.info().worktree.clone();
    assert!(
        fs::read_to_string(worktree.join(".agents/skills/style-guide/SKILL.md"))
            .unwrap()
            .contains("Edited by the agent")
    );
    let skill = staged(&f, "root").join("repository/style-guide/SKILL.md");
    assert_eq!(fs::read_to_string(&skill).unwrap(), ORIGINAL);

    let root = root.send("WRITE b.txt=2", options.clone()).unwrap();
    assert_eq!(fs::read_to_string(&skill).unwrap(), ORIGINAL);
    let records = turns(&f, "root");
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].0, records[1].0, "the same set on both turns");

    let delegate = root.delegate(options).unwrap();
    delegate
        .spawn(Spawn {
            prompt: "WRITE kid.txt=1".into(),
            name: Some("kid".into()),
            ..Spawn::default()
        })
        .unwrap();
    let descendants = root.wait_subtree().unwrap();
    assert!(
        descendants.iter().any(|b| b.name == "kid"),
        "{descendants:?}"
    );
    let kid_worktree = f.yard.branch("kid").unwrap().info().worktree.clone();
    assert!(
        fs::read_to_string(kid_worktree.join(".agents/skills/style-guide/SKILL.md"))
            .unwrap()
            .contains("Edited by the agent"),
        "the child starts from its parent's work"
    );
    assert_eq!(
        fs::read_to_string(staged(&f, "kid").join("repository/style-guide/SKILL.md")).unwrap(),
        ORIGINAL,
        "and runs with its parent's skills"
    );
    assert_eq!(turns(&f, "kid")[0].0, records[0].0);
    assert_eq!(
        f.yard.branch_skills("kid").unwrap(),
        f.yard.branch_skills("root").unwrap()
    );
}

#[test]
fn a_reserved_name_or_a_set_too_large_for_the_index_is_withheld_with_the_reason() {
    let f = Fixture::new();
    commit_skill(
        &f,
        "docx",
        "---\nname: docx\ndescription: Not the built-in one.\n---\n",
    );
    f.yard
        .task("WRITE a.txt=1")
        .options(allowed(&f))
        .name("reserved")
        .run()
        .unwrap();
    let (skills, _) = turns(&f, "reserved").remove(0);
    let reason = skills.withheld.expect("withheld").reason;
    assert!(
        reason.contains("repository skill docx has a built-in skill's name"),
        "{reason}"
    );
    assert!(!staged(&f, "reserved").join("repository").exists());

    f.git(&["rm", "-rq", ".agents"]);
    for i in 0..30 {
        let name = format!("s{i:02}");
        let text = format!(
            "---\nname: {name}\ndescription: {}\n---\n",
            "word ".repeat(if i == 7 { 300 } else { 40 })
        );
        let path = f.root.join(format!(".agents/skills/{name}/SKILL.md"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    f.git(&["add", "-A"]);
    f.git(&["commit", "-q", "-m", "many skills"]);
    f.yard
        .task("WRITE a.txt=1")
        .options(allowed(&f))
        .name("crowded")
        .run()
        .unwrap();
    let (skills, _) = turns(&f, "crowded").remove(0);
    let reason = skills.withheld.expect("withheld").reason;
    assert!(reason.contains("over the 8192 allowed"), "{reason}");
    assert!(
        reason.contains("the largest entries are repository:s07 ("),
        "{reason}"
    );
}

#[test]
fn every_turn_records_its_skills_whether_or_not_anything_changed() {
    let f = Fixture::new();
    let branch = f.task("WRITE a.txt=1").name("plain").run().unwrap();
    branch.send("WRITE b.txt=2", f.options()).unwrap();
    let records = turns(&f, "plain");
    assert_eq!(records.len(), 2, "one record per turn");
    let (first, first_files) = &records[0];
    let (second, second_files) = &records[1];
    assert!(!first_files.is_empty(), "the first turn staged the skills");
    assert!(second_files.is_empty(), "{second_files:?}");
    assert_eq!(first, second);
    assert_eq!(first.withheld, None);
    let names: Vec<&str> = first.entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"docx") && names.contains(&"artifact-tools"));
    assert!(first.entries.iter().all(|e| e.source == Source::BuiltIn));
    assert_eq!(f.yard.branch_skills("plain").unwrap(), None);
}

#[test]
fn removing_a_branch_removes_its_skill_cache() {
    let f = Fixture::new();
    f.task("WRITE a.txt=1").name("cached").run().unwrap();
    let cache = staged(&f, "cached");
    assert!(cache.join("INDEX.md").is_file());
    let kept = staged(&f, "other");
    fs::create_dir_all(&kept).unwrap();
    f.yard.remove("cached").unwrap();
    assert!(!cache.parent().unwrap().parent().unwrap().exists());
    assert!(kept.is_dir(), "another branch's cache stays");
}

/// Write a skill `name` into the person's folder `dir`.
fn user_skill(dir: &std::path::Path, name: &str, description: &str) {
    let path = dir.join(name).join("SKILL.md");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        format!("---\nname: {name}\ndescription: {description}\n---\nMine.\n"),
    )
    .unwrap();
}

#[test]
fn a_persons_skills_are_staged_from_their_folder_after_the_repositorys_and_recorded() {
    let f = Fixture::new();
    commit_skill(&f, "style-guide", ORIGINAL);
    let folder = f.dir.join("home/.config/branchyard/skills");
    user_skill(
        &folder,
        "memory-parser",
        "What you learned about the parser.",
    );
    user_skill(&folder, "style-guide", "Shadowed by the repository's.");
    user_skill(&folder, "docx", "A built-in's name.");
    user_skill(&folder, "find-tools", "The search skill's name.");
    fs::create_dir_all(folder.join("bad")).unwrap();
    fs::write(
        folder.join("bad/SKILL.md"),
        "---\nname: other\ndescription: d\n---\n",
    )
    .unwrap();
    let options = TaskOptions {
        user_skills: Some(folder.clone()),
        ..allowed(&f)
    };
    f.yard
        .task("WRITE a.txt=1")
        .options(options.clone())
        .name("mine")
        .run()
        .unwrap();
    let (skills, files) = turns(&f, "mine").remove(0);
    let custom: Vec<String> = skills
        .entries
        .iter()
        .filter(|e| e.source != Source::BuiltIn)
        .map(|e| e.label())
        .collect();
    assert_eq!(custom, ["repository:style-guide", "user:memory-parser"]);
    assert_eq!(
        skills.set,
        branchyard_provision::skillset::set_digest(&skills.entries)
    );
    let withheld: Vec<(&str, &str)> = skills
        .user_withheld
        .iter()
        .map(|w| (w.name.as_str(), w.reason.as_str()))
        .collect();
    assert_eq!(withheld.len(), 4, "{withheld:?}");
    assert_eq!(withheld[0].0, "bad");
    assert!(withheld[0].1.contains("the name must be its directory's"));
    assert_eq!(withheld[1].0, "docx");
    assert!(withheld[1]
        .1
        .contains("built-in skill's name, which is reserved"));
    assert_eq!(withheld[2].0, "find-tools");
    assert_eq!(
        withheld[3],
        (
            "style-guide",
            "shadowed by repository:style-guide: a repository's skill comes before yours"
        )
    );
    let root = staged(&f, "mine");
    assert!(root.join("user/memory-parser/SKILL.md").is_file());
    assert!(!root.join("user/style-guide").exists());
    assert!(!root.join("user/docx").exists());
    assert!(files.contains(&".branchyard/capabilities/user/memory-parser/SKILL.md".into()));
    let topic = fs::read_to_string(root.join("topics/custom/INDEX.md")).unwrap();
    assert!(topic.contains("[repository:style-guide]"));
    assert!(topic.contains(
        "- [user:memory-parser](../../user/memory-parser/SKILL.md): What you learned about the \
         parser. (added by you, from your own skills folder)"
    ));
    let index = fs::read_to_string(root.join("INDEX.md")).unwrap();
    assert!(index.contains("[Custom](topics/custom/INDEX.md)"));
    let record = f.yard.branch("mine").unwrap().events().unwrap();
    let provisioned = record
        .iter()
        .find(|e| matches!(e.activity, Activity::Provisioned { .. }))
        .unwrap();
    let json = serde_json::to_value(&provisioned.activity).unwrap();
    let skills_json = &json["provisioned"]["skills"];
    assert_eq!(
        skills_json["entries"].as_array().unwrap().last().unwrap()["layer"],
        "user",
        "{skills_json}"
    );
    assert_eq!(skills_json["user_withheld"][0]["name"], "bad");

    // Pinned by nothing: the next turn reads the folder again, and the
    // record carries the new digest.
    user_skill(&folder, "memory-parser", "Changed since the first turn.");
    fs::remove_dir_all(folder.join("bad")).unwrap();
    f.yard
        .branch("mine")
        .unwrap()
        .send("WRITE b.txt=2", options.clone())
        .unwrap();
    let records = turns(&f, "mine");
    assert_eq!(records.len(), 2);
    let digest_of = |set: &SkillSet| {
        set.entries
            .iter()
            .find(|e| e.name == "memory-parser")
            .unwrap()
            .digest
            .clone()
    };
    assert_ne!(digest_of(&records[0].0), digest_of(&records[1].0));
    assert_eq!(records[1].0.user_withheld.len(), 3);
    assert!(fs::read_to_string(root.join("user/memory-parser/SKILL.md"))
        .unwrap()
        .contains("Changed since"));

    // A branch created without a folder stages none, and says nothing.
    f.yard
        .task("WRITE a.txt=1")
        .options(allowed(&f))
        .name("without")
        .run()
        .unwrap();
    let (skills, _) = turns(&f, "without").remove(0);
    assert!(skills.entries.iter().all(|e| e.source != Source::User));
    assert!(skills.user_withheld.is_empty());
    assert!(!staged(&f, "without").join("user").exists());
}

#[test]
fn an_untrusted_memory_bundle_is_withheld_with_its_reason_until_trusted() {
    let f = Fixture::new();
    f.yard
        .add_memory(NewMemory {
            title: "Run the parser benches with the 1 MB fixture".into(),
            body: "The small fixture fits the cache and says nothing.".into(),
            kind: MemoryKind::Procedure,
            paths: vec!["crates/parser/**".into()],
            task_kinds: Vec::new(),
            topic: None,
            by: "ann".into(),
            now_ms: 1_762_500_000_000,
            evidence: Vec::new(),
        })
        .unwrap();
    f.git(&["add", "-A"]);
    f.git(&["commit", "-q", "-m", "team memory"]);
    f.task("WRITE a.txt=1").name("untrusted").run().unwrap();
    let (skills, _) = turns(&f, "untrusted").remove(0);
    let withheld = skills.withheld.expect("withheld by default");
    assert_eq!(
        withheld.reason,
        "the branch's creator did not allow repository skills"
    );
    assert_eq!(withheld.entries[0].label(), "repository:memory-parser");
    assert!(!staged(&f, "untrusted").join("repository").exists());

    let set = match branchyard::skills::read(&f.root, "HEAD").unwrap() {
        branchyard::skills::Found::Skills(set) => set,
        other => panic!("{other:?}"),
    };
    f.yard
        .task("WRITE a.txt=1")
        .options(TaskOptions {
            repository_skills: SkillGate::Trusted(set.digest),
            ..f.options()
        })
        .name("trusted")
        .run()
        .unwrap();
    let (skills, _) = turns(&f, "trusted").remove(0);
    assert_eq!(skills.withheld, None);
    let reference = staged(&f, "trusted").join(
        "repository/memory-parser/references/run-the-parser-benches-with-the-1-mb-fixture.md",
    );
    assert!(reference.is_file(), "{}", reference.display());
    let topic = fs::read_to_string(staged(&f, "trusted").join("topics/custom/INDEX.md")).unwrap();
    assert!(topic.contains(
        "- [repository:memory-parser](../../repository/memory-parser/SKILL.md): What this team \
         learned about parser: 1 procedure; found by search, approved by people."
    ));
}

/// The skills a turn mounts follow the policy as it stands for that turn:
/// a branch admitted with no limit mounts every built-in skill, and the next
/// send, which carries the narrowed set a server computed from the current
/// policy, mounts only that, in the staged tree and in the manifest.
#[test]
fn a_policy_narrowed_after_admission_mounts_fewer_skills_the_next_turn() {
    let f = Fixture::new();
    let ls = "SH ls \"$HOME/.branchyard/capabilities\" > caps.txt\nWRITE a.txt=1";
    let options = TaskOptions {
        isolated: true,
        provision: Some(Provisioning {
            skills: Some(vec!["docx".into()]),
            ..Provisioning::default()
        }),
        ..allowed(&f)
    };
    let branch = f
        .yard
        .task(ls)
        .options(options.clone())
        .name("narrowed")
        .run()
        .unwrap();
    let worktree = branch.info().worktree.clone();
    let caps = fs::read_to_string(worktree.join("caps.txt")).unwrap();
    assert!(caps.lines().any(|l| l == "pptx"), "{caps}");
    let first = branch.manifest().unwrap().unwrap();
    assert!(first.mounted_skill("pptx").is_some());

    let narrowed = TaskOptions {
        allowed_skills: Some(vec!["docx".into()]),
        ..options
    };
    let again = branch
        .send(
            "SH ls \"$HOME/.branchyard/capabilities/pptx\" > pptx2.txt 2>&1\nWRITE b.txt=2",
            narrowed,
        )
        .unwrap();
    let second = again.manifest().unwrap().unwrap();
    assert!(
        second.mounted_skill("pptx").is_none(),
        "{:?}",
        second
            .mounted
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>()
    );
    assert!(second.mounted_skill("docx").is_some());
    // Nothing of the dropped skill is left in the tree.
    let pptx = fs::read_to_string(worktree.join("pptx2.txt")).unwrap();
    assert!(!pptx.contains("SKILL.md"), "{pptx}");
}

/// R2: a local turn mounts every ready skill and names only the chosen
/// ones. With `docx` chosen, the tree stages every built-in skill, the
/// trusted repository set and the person's folder; `INDEX.md` names `docx`
/// and what it needs and counts the rest; the manifest's `mounted` lists
/// every entry with its source and digest; and what the fake agent did with
/// `bin/tools` comes back as `skill` events with the manifest's digests,
/// after which the guest's log is gone. A person's edit to their own skill
/// resolves the manifest again; a stored manifest naming another digest for
/// the repository's skill fails the turn before the harness.
#[test]
fn a_turn_mounts_every_ready_skill_names_the_chosen_ones_and_records_what_it_searched() {
    let f = Fixture::new();
    commit_skill(&f, "style-guide", ORIGINAL);
    let folder = f.dir.join("home/.config/branchyard/skills");
    user_skill(
        &folder,
        "memory-parser",
        "What you learned about the parser.",
    );
    let tools = "python3 \"$HOME/.branchyard/tools/bin/tools\"";
    let prompt = format!(
        "SH {tools} search word document\n\
         SH {tools} show pptx\n\
         SH {tools} show repository:style-guide\n\
         SH {tools} show user:memory-parser\n\
         SH printf %s \"$(cat \"$HOME/.branchyard/tools/INDEX.md\")\" > index.txt\n\
         SH ls \"$HOME/.branchyard/capabilities\" > caps.txt\n\
         WRITE a.txt=1"
    );
    let options = TaskOptions {
        isolated: true,
        user_skills: Some(folder.clone()),
        provision: Some(Provisioning {
            skills: Some(vec!["docx".into()]),
            ..Provisioning::default()
        }),
        ..allowed(&f)
    };
    let branch = f
        .yard
        .task(&prompt)
        .options(options.clone())
        .name("mounted")
        .run()
        .unwrap();
    assert!(
        matches!(branch.info().status, branchyard::BranchStatus::Ready),
        "{:?}",
        branch.info().status
    );
    let worktree = branch.info().worktree.clone();
    let index = fs::read_to_string(worktree.join("index.txt")).unwrap();
    assert!(index.contains("- docx:"), "{index}");
    assert!(
        index.contains("- artifact-tools:"),
        "docx needs it: {index}"
    );
    assert!(!index.contains("- pptx"), "mounted, not named: {index}");
    assert!(!index.contains("repository:style-guide"), "{index}");
    let caps = fs::read_to_string(worktree.join("caps.txt")).unwrap();
    for staged in ["docx", "pptx", "find-tools", "repository", "user"] {
        assert!(caps.lines().any(|l| l == staged), "{staged} in {caps}");
    }
    let mounted_count = caps
        .lines()
        .filter(|l| {
            ![
                "docx",
                "artifact-tools",
                "INDEX.md",
                "topics",
                "upstream.lock.json",
                "repository",
                "user",
            ]
            .contains(l)
        })
        .count()
        + 2;
    assert!(
        index.contains(&format!(
            "- ...{mounted_count} more: `bin/tools search WORDS`"
        )),
        "{mounted_count} mounted: {index}"
    );

    // The manifest names every mounted skill with its source and digest.
    let manifest = branch.manifest().unwrap().expect("a manifest");
    assert_eq!(
        manifest.skills,
        branchyard::manifest::ManifestSkills::Chosen(vec!["docx".into()])
    );
    let pptx = manifest.mounted_skill("pptx").expect("pptx is mounted");
    assert_eq!(pptx.source, Source::BuiltIn);
    let style = manifest
        .mounted_skill("repository:style-guide")
        .expect("the repository's skill is mounted");
    assert_eq!(style.source, Source::Repository);
    let mine = manifest
        .mounted_skill("user:memory-parser")
        .expect("the person's skill is mounted");
    assert_eq!(mine.source, Source::User);
    let (skills, _) = turns(&f, "mounted").remove(0);
    assert_eq!(skills.entries.len(), manifest.mounted.len());
    for entry in &skills.entries {
        let recorded = manifest.mounted_skill(&entry.label()).unwrap();
        assert_eq!(recorded.source, entry.source, "{}", entry.label());
        // A built-in skill's manifest digest is the tool store's framing of
        // its files; the record's is the skill set's. The other sources
        // record one digest in both places.
        match entry.source {
            Source::BuiltIn => assert_eq!(recorded.digest.len(), 64),
            _ => assert_eq!(recorded.digest, entry.digest, "{}", entry.label()),
        }
    }

    // What the agent did with the tree, as events, with the manifest's digests.
    let events = branch.events().unwrap();
    let skill_events: Vec<&SkillActivity> = events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Skill(activity) => Some(activity.as_ref()),
            _ => None,
        })
        .collect();
    assert_eq!(skill_events.len(), 4, "{skill_events:?}");
    match skill_events[0] {
        SkillActivity::Searched {
            terms,
            results,
            kind,
            plain,
        } => {
            assert_eq!(*terms, 2);
            assert_eq!(results[0], "skill:docx", "{results:?}");
            assert_eq!((kind.as_deref(), *plain), (None, false));
        }
        other => panic!("{other:?}"),
    }
    let opened: Vec<(&str, &str, Option<Source>, u8)> = skill_events[1..]
        .iter()
        .map(|a| match a {
            SkillActivity::Opened {
                id,
                digest,
                source,
                level,
                ..
            } => (id.as_str(), digest.as_str(), *source, *level),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        opened,
        [
            ("pptx", pptx.digest.as_str(), Some(Source::BuiltIn), 2),
            (
                "repository:style-guide",
                style.digest.as_str(),
                Some(Source::Repository),
                2
            ),
            (
                "user:memory-parser",
                mine.digest.as_str(),
                Some(Source::User),
                2
            ),
        ]
    );
    let home = PathBuf::from(
        common::stored_record(&f.root, "mounted")["home"]
            .as_str()
            .unwrap(),
    );
    assert!(
        !home.join(".branchyard/turn/skills.jsonl").exists(),
        "read once"
    );
    assert!(
        !home.join(".claude").exists(),
        "no native directory for this harness"
    );
    assert!(
        skill_events[0]
            .describe()
            .starts_with("skill search (2 terms) → docx"),
        "{}",
        skill_events[0].describe()
    );
    assert!(
        skill_events[1]
            .describe()
            .starts_with("skill opened pptx (level 2, "),
        "{}",
        skill_events[1].describe()
    );

    // The person edits their own skill: other tools, so a new manifest, and the turn runs.
    user_skill(
        &folder,
        "memory-parser",
        "What you learned about the parser, revised.",
    );
    let again = branch.send("WRITE b.txt=2", options.clone()).unwrap();
    assert!(
        matches!(again.info().status, branchyard::BranchStatus::Ready),
        "{:?}",
        again.info().status
    );
    let revised = again.manifest().unwrap().unwrap();
    assert_ne!(revised.digest(), manifest.digest());
    assert_ne!(
        revised.mounted_skill("user:memory-parser").unwrap().digest,
        mine.digest
    );
    assert!(!revised.same_tools(&manifest));
    // A stored manifest that names another digest for the repository's
    // skill than the commit holds is a stored record of other tools: the
    // next turn resolves the manifest again from the commit and runs; the
    // mismatch check holds a turn to the manifest resolved for it
    // (`bundles::verify_custom`), never to a stale one.
    common::edit_record(&f.root, "mounted", |record| {
        for skill in record["manifest"]["mounted"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .filter(|s| s["id"] == "repository:style-guide")
        {
            skill["digest"] = "feedface".into();
        }
    });
    let third = again.send("WRITE c.txt=3", options).unwrap();
    assert!(
        matches!(third.info().status, branchyard::BranchStatus::Ready),
        "{:?}",
        third.info().status
    );
    let resolved = third.manifest().unwrap().unwrap();
    assert_eq!(
        resolved
            .mounted_skill("repository:style-guide")
            .unwrap()
            .digest,
        style.digest,
        "the commit's digest, not the edited record's"
    );
}
