//! The memory bundle writer and library (`branchyard::memory`,
//! docs/memory.md section 1), hermetically: the same approved set written
//! twice gives the same files and digest; a topic over the limits is split
//! with the note, every `SKILL.md` under 4,096 characters; a library adds,
//! edits, removes and exports, and `Yard::add_memory` writes into the
//! checkout's `.agents/skills/`.

#![allow(clippy::unwrap_used, clippy::expect_used)] // tests: a panic is the failure report
mod common;

use std::fs;

use branchyard::memory::{bundles, Audience, Library, Memory, MemoryEdit, SKILL_MD_MAX};
use branchyard::{Error, MemoryKind, NewMemory, TaskKind};
use branchyard_provision::skillset;
use common::Fixture;

fn approved(topic: &str, n: usize, kind: MemoryKind) -> Memory {
    Memory {
        topic: topic.into(),
        id: format!("memory-{n:03}"),
        title: format!("Memory number {n} about {topic}, written to be found"),
        kind,
        body: format!("The body of memory {n}.\n\nIt says what was learned about {topic}."),
        paths: vec![format!("crates/{topic}/**")],
        task_kinds: vec![TaskKind::Tests],
        approved_by: "ann".into(),
        approved_ms: 1_762_500_000_000 + n as u64,
        evidence: Vec::new(),
    }
}

fn new_memory(title: &str, body: &str, kind: MemoryKind, paths: &[&str]) -> NewMemory {
    NewMemory {
        title: title.into(),
        body: body.into(),
        kind,
        paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        task_kinds: Vec::new(),
        topic: None,
        by: "ann".into(),
        now_ms: 1_762_500_000_000,
        evidence: Vec::new(),
    }
}

#[test]
fn the_same_approved_set_written_twice_gives_the_same_digest() {
    let set: Vec<Memory> = (0..5)
        .map(|n| approved("parser", n, MemoryKind::ALL[n % 4]))
        .collect();
    let first = bundles("parser", &set, Audience::Team).unwrap();
    let mut shuffled = set.clone();
    shuffled.reverse();
    let second = bundles("parser", &shuffled, Audience::Team).unwrap();
    assert_eq!(first, second, "order of the input is not a fact of the set");
    assert_eq!(first.len(), 1);
    assert_eq!(
        first[0].digest,
        skillset::digest(
            first[0]
                .files
                .iter()
                .map(|(p, c)| (p.as_str(), c.as_bytes()))
        )
    );
    // Nothing volatile: a later run with another clock writes the same.
    let again = bundles("parser", &set, Audience::Team).unwrap();
    assert_eq!(again[0].digest, first[0].digest);
    // One approved field changed is another set.
    let mut changed = set.clone();
    changed[2].approved_by = "bob".into();
    assert_ne!(
        bundles("parser", &changed, Audience::Team).unwrap()[0].digest,
        first[0].digest
    );
    let scratch = tempfile::tempdir().unwrap();
    let dir = scratch.path().join("memory-parser");
    fs::create_dir_all(&dir).unwrap();
    for (path, content) in &first[0].files {
        let file = dir.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, content).unwrap();
    }
    let read = branchyard::skills::read_directory("memory-parser", &dir).unwrap();
    assert_eq!(
        read.digest, first[0].digest,
        "the files on disk are the bundle"
    );
    assert_eq!(
        read.description,
        "What this team learned about parser: 2 rules, 1 procedure, 1 fact, 1 pitfall; found \
         by search, approved by people."
    );
}

#[test]
fn a_topic_over_the_limits_is_split_with_the_note_and_every_skill_md_fits() {
    let set: Vec<Memory> = (0..150)
        .map(|n| approved("releases", n, MemoryKind::ALL[n % 4]))
        .collect();
    let produced = bundles("releases", &set, Audience::Team).unwrap();
    assert!(produced.len() >= 3, "{} bundles", produced.len());
    let names: Vec<&str> = produced.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names[0], "memory-releases");
    assert_eq!(names[1], "memory-releases--2");
    assert_eq!(names[2], "memory-releases--3");
    let mut listed = 0;
    for (at, bundle) in produced.iter().enumerate() {
        let md = &bundle.files[0].1;
        assert!(
            md.chars().count() <= SKILL_MD_MAX,
            "{}: {} characters",
            bundle.name,
            md.chars().count()
        );
        assert!(bundle.files.len() <= skillset::MAX_SKILL_FILES);
        let references = bundle.files.len() - 2;
        listed += references;
        if at + 1 < produced.len() {
            assert!(
                md.contains(&format!(
                    "Continued in memory-releases--{} ({} more).",
                    at + 2,
                    set.len() - listed
                )),
                "{}: {md}",
                bundle.name
            );
        } else {
            assert!(!md.contains("Continued in"));
        }
        let front = skillset::frontmatter(md).unwrap();
        assert_eq!(front.name, bundle.name);
        assert!(front
            .description
            .starts_with("What this team learned about releases:"));
    }
    assert_eq!(listed, set.len(), "every memory is in exactly one bundle");
    assert_eq!(
        bundles("releases", &set, Audience::Team).unwrap(),
        produced,
        "split or not, the same set gives the same bundles"
    );
    // The writer's own guard: a title too long for one line is refused
    // before anything is written, naming the limit.
    let mut long = set[..1].to_vec();
    long[0].title = "t".repeat(81);
    assert!(matches!(
        bundles("releases", &long, Audience::Team),
        Err(Error::InvalidMemory(why)) if why.contains("at most 80")
    ));
}

#[test]
fn a_library_adds_edits_removes_and_exports_what_the_writer_produces() {
    let scratch = tempfile::tempdir().unwrap();
    let dir = scratch.path().join("skills");
    let library = Library::person(&dir);
    assert_eq!(library.read().unwrap(), Vec::<Memory>::new());

    let first = library
        .add(new_memory(
            "Run the parser benches with the 1 MB fixture",
            "The small fixture fits the cache.\n\nUse `bench/large.json`.",
            MemoryKind::Procedure,
            &["crates/parser/**"],
        ))
        .unwrap();
    assert_eq!(
        first.memory.topic, "parser",
        "the topic comes from the path"
    );
    assert_eq!(
        first.memory.id,
        "run-the-parser-benches-with-the-1-mb-fixture"
    );
    assert_eq!(first.bundles, [dir.join("memory-parser")]);
    assert!(dir
        .join("memory-parser/references/run-the-parser-benches-with-the-1-mb-fixture.md")
        .is_file());
    let md = fs::read_to_string(dir.join("memory-parser/SKILL.md")).unwrap();
    assert!(md
        .contains("What you learned about parser: 1 procedure; found by search, approved by you."));
    assert!(md.contains("- Run the parser benches with the 1 MB fixture (references/run-the-parser-benches-with-the-1-mb-fixture.md)"));

    // The same title again gets the next slug; no topic and no path is general.
    let second = library
        .add(new_memory(
            "Run the parser benches with the 1 MB fixture",
            "Another body.",
            MemoryKind::Pitfall,
            &[],
        ))
        .unwrap();
    assert_eq!(
        second.memory.id,
        "run-the-parser-benches-with-the-1-mb-fixture-2"
    );
    assert_eq!(second.memory.topic, "general");
    let mut third = new_memory(
        "Releases go out on Thursdays",
        "Not on Fridays.",
        MemoryKind::Fact,
        &[],
    );
    third.topic = Some("releases".into());
    third.task_kinds = vec![TaskKind::Docs, TaskKind::Review];
    library.add(third).unwrap();
    let all = library.read().unwrap();
    assert_eq!(
        all.iter()
            .map(|m| (m.topic.as_str(), m.id.as_str()))
            .collect::<Vec<_>>(),
        [
            ("general", "run-the-parser-benches-with-the-1-mb-fixture-2"),
            ("parser", "run-the-parser-benches-with-the-1-mb-fixture"),
            ("releases", "releases-go-out-on-thursdays"),
        ]
    );
    assert_eq!(all[2].task_kinds, [TaskKind::Docs, TaskKind::Review]);
    assert_eq!(
        all[1].body,
        "The small fixture fits the cache.\n\nUse `bench/large.json`."
    );

    // Export reproduces the files on disk byte for byte.
    let out = scratch.path().join("export");
    let exported = library.export(Some(&out)).unwrap();
    assert_eq!(
        exported.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
        ["memory-general", "memory-parser", "memory-releases"]
    );
    for bundle in &exported {
        for (path, content) in &bundle.files {
            assert_eq!(
                fs::read_to_string(dir.join(&bundle.name).join(path)).unwrap(),
                *content,
                "{}/{path}",
                bundle.name
            );
            assert_eq!(
                fs::read_to_string(out.join(&bundle.name).join(path)).unwrap(),
                *content
            );
        }
        let on_disk =
            branchyard::skills::read_directory(&bundle.name, &dir.join(&bundle.name)).unwrap();
        assert_eq!(on_disk.digest, bundle.digest);
    }

    // An edit is a new approval and rewrites the bundle; a stale file goes.
    let edited = library
        .edit(
            "releases-go-out-on-thursdays",
            MemoryEdit {
                title: Some("Releases go out on Thursdays, never Fridays".into()),
                kind: Some(MemoryKind::Rule),
                paths: Some(Vec::new()),
                by: "bob".into(),
                now_ms: 1_762_600_000_000,
                ..MemoryEdit::default()
            },
        )
        .unwrap();
    assert_eq!(edited.memory.approved_by, "bob");
    assert_eq!(
        edited.memory.id, "releases-go-out-on-thursdays",
        "the slug stays"
    );
    let md = fs::read_to_string(dir.join("memory-releases/SKILL.md")).unwrap();
    assert!(md.contains("## Rules\n\n- Releases go out on Thursdays, never Fridays"));
    assert!(!md.contains("## Facts"));
    let reference =
        fs::read_to_string(dir.join("memory-releases/references/releases-go-out-on-thursdays.md"))
            .unwrap();
    assert!(reference.ends_with("Approved by bob on 2025-11-08.\n"));

    assert!(matches!(
        library.edit("nope", MemoryEdit::default()),
        Err(Error::UnknownMemory(id)) if id == "nope"
    ));
    assert!(matches!(library.get("nope"), Err(Error::UnknownMemory(_))));
    assert!(matches!(
        library.remove("nope"),
        Err(Error::UnknownMemory(_))
    ));

    // Removing the last memory of a topic removes its bundle.
    let removed = library.remove("releases-go-out-on-thursdays").unwrap();
    assert_eq!(removed.approved_by, "bob");
    assert!(!dir.join("memory-releases").exists());
    assert_eq!(library.read().unwrap().len(), 2);
    assert_eq!(library.export(None).unwrap().len(), 2);

    // A bundle that does not read refuses the library, naming the file.
    fs::write(dir.join("memory-parser/memory.json"), "{").unwrap();
    assert!(matches!(
        library.read(),
        Err(Error::InvalidMemory(why)) if why.contains("memory.json does not read")
    ));
}

#[test]
fn a_split_topic_is_rewritten_as_a_whole_and_continuations_go_when_it_shrinks() {
    let scratch = tempfile::tempdir().unwrap();
    let library = Library::team(scratch.path());
    let dir = scratch.path().join(".agents/skills");
    for n in 0..120 {
        let mut new = new_memory(
            &format!("Memory number {n} about releases, written to be found"),
            &format!("Body {n}."),
            MemoryKind::ALL[n % 4],
            &["docs/releases/**"],
        );
        new.title = format!("Memory number {n:03} about releases, written to be found");
        library.add(new).unwrap();
    }
    assert!(dir.join("memory-releases--2/SKILL.md").is_file());
    let all = library.read().unwrap();
    assert_eq!(all.len(), 120);
    assert!(all.iter().all(|m| m.topic == "releases"));
    for memory in all.iter().take(100) {
        library.remove(&memory.id).unwrap();
    }
    assert_eq!(library.read().unwrap().len(), 20);
    assert!(dir.join("memory-releases/SKILL.md").is_file());
    assert!(
        !dir.join("memory-releases--2").exists(),
        "a continuation no longer produced is removed"
    );
}

fn add_to(library: &Library, topic: &str, title: &str) -> String {
    let mut new = new_memory(title, "Body.", MemoryKind::Fact, &[]);
    new.topic = Some(topic.into());
    library.add(new).unwrap().memory.id
}

fn bundle_dirs(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn topics_parser_and_parser_2_coexist_through_add_edit_and_remove() {
    let scratch = tempfile::tempdir().unwrap();
    let library = Library::team(scratch.path());
    let dir = scratch.path().join(".agents/skills");
    let other = add_to(&library, "parser-2", "Belongs to parser two");
    let first = add_to(&library, "parser", "Belongs to parser");
    assert_eq!(bundle_dirs(&dir), ["memory-parser", "memory-parser-2"]);
    let before = fs::read(dir.join("memory-parser-2/memory.json")).unwrap();
    add_to(&library, "parser", "Another for parser");
    library
        .edit(
            &first,
            MemoryEdit {
                body: Some("Edited.".into()),
                by: "bob".into(),
                now_ms: 5,
                ..MemoryEdit::default()
            },
        )
        .unwrap();
    library.remove(&first).unwrap();
    assert_eq!(
        fs::read(dir.join("memory-parser-2/memory.json")).unwrap(),
        before,
        "the other topic's bundle is untouched"
    );
    let remaining = library.read().unwrap();
    assert!(remaining
        .iter()
        .any(|m| m.id == other && m.topic == "parser-2"));
    // Emptying `parser` removes only its own directory.
    for memory in remaining.iter().filter(|m| m.topic == "parser") {
        library.remove(&memory.id).unwrap();
    }
    assert_eq!(bundle_dirs(&dir), ["memory-parser-2"]);
    // And the other way round.
    library.remove(&other).unwrap();
    assert!(bundle_dirs(&dir).is_empty());
}

#[test]
fn an_overflowing_parser_never_touches_parser_2() {
    let scratch = tempfile::tempdir().unwrap();
    let library = Library::team(scratch.path());
    let dir = scratch.path().join(".agents/skills");
    add_to(&library, "parser-2", "Belongs to parser two");
    let before = fs::read(dir.join("memory-parser-2/memory.json")).unwrap();
    for n in 0..120 {
        let mut new = new_memory(
            &format!("Memory number {n:03} about parsing, written to be found"),
            &format!("Body {n}."),
            MemoryKind::ALL[n % 4],
            &[],
        );
        new.topic = Some("parser".into());
        library.add(new).unwrap();
    }
    let names = bundle_dirs(&dir);
    assert!(names.contains(&"memory-parser--2".to_owned()), "{names:?}");
    assert_eq!(
        fs::read(dir.join("memory-parser-2/memory.json")).unwrap(),
        before
    );
    let all = library.read().unwrap();
    assert_eq!(all.iter().filter(|m| m.topic == "parser-2").count(), 1);
    assert_eq!(all.iter().filter(|m| m.topic == "parser").count(), 120);
}

#[test]
fn a_topic_cannot_hold_the_continuation_separator() {
    let scratch = tempfile::tempdir().unwrap();
    let library = Library::team(scratch.path());
    for topic in ["a--2", "-a", "a-"] {
        let mut new = new_memory("Title", "Body.", MemoryKind::Fact, &[]);
        new.topic = Some(topic.into());
        assert!(
            matches!(library.add(new), Err(Error::InvalidMemory(_))),
            "{topic}"
        );
    }
}

#[test]
fn the_yard_writes_team_memory_into_the_checkouts_skills_directory() {
    let f = Fixture::new();
    assert_eq!(f.yard.memories().unwrap(), Vec::<Memory>::new());
    let memory = f
        .yard
        .add_memory(new_memory(
            "Run cargo fmt before you finish",
            "The CI's first step is `cargo fmt --check`.",
            MemoryKind::Rule,
            &[],
        ))
        .unwrap();
    assert_eq!(memory.topic, "general");
    let skill = f.root.join(".agents/skills/memory-general/SKILL.md");
    assert!(skill.is_file());
    assert_eq!(f.yard.memories().unwrap(), vec![memory]);
    // Nothing a turn can find until it is committed and trusted: the
    // worktree's files are not a repository skill.
    assert_eq!(
        branchyard::skills::read(&f.root, "HEAD").unwrap(),
        branchyard::skills::Found::Nothing
    );
}

#[cfg(unix)]
mod symlinks {
    use super::*;
    use std::os::unix::fs::symlink;

    fn sentinel_repo() -> (branchyard_testkit::Scratch, std::path::PathBuf) {
        let scratch = branchyard_testkit::Scratch::new("memory-links");
        let root = scratch.path().join("repo");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(root.join("sentinel.txt"), "keep\n").unwrap();
        fs::create_dir_all(root.join(".agents/skills")).unwrap();
        (scratch, root)
    }

    #[test]
    fn a_symlinked_bundle_directory_is_refused_and_nothing_outside_is_touched() {
        let (_scratch, root) = sentinel_repo();
        symlink("../..", root.join(".agents/skills/memory-general")).unwrap();
        let error = Library::team(&root)
            .add(new_memory("T", "B", MemoryKind::Rule, &[]))
            .unwrap_err();
        assert!(matches!(&error, Error::InvalidMemory(m) if m.contains("symbolic link")));
        assert_eq!(
            fs::read_to_string(root.join("sentinel.txt")).unwrap(),
            "keep\n"
        );
        assert!(root.join(".git/HEAD").is_file());
        assert!(!root.join("SKILL.md").exists());
        assert!(!root.join("memory.json").exists());
        assert!(!root.join("references").exists());
    }

    #[test]
    fn a_symlinked_references_directory_is_refused_on_add_and_export() {
        let (scratch, root) = sentinel_repo();
        let library = Library::team(&root);
        library
            .add(new_memory("First", "Body.", MemoryKind::Rule, &[]))
            .unwrap();
        let bundle = root.join(".agents/skills/memory-general");
        let outside = scratch.path().join("outside");
        fs::rename(bundle.join("references"), &outside).unwrap();
        fs::write(outside.join("victim.md"), "keep\n").unwrap();
        symlink(&outside, bundle.join("references")).unwrap();
        let error = library
            .add(new_memory("Second", "Body.", MemoryKind::Rule, &[]))
            .unwrap_err();
        assert!(matches!(error, Error::InvalidMemory(_)));
        assert_eq!(
            fs::read_to_string(outside.join("victim.md")).unwrap(),
            "keep\n"
        );
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 2);
        // Export into a directory whose bundle has a link is refused too.
        let out = scratch.path().join("out");
        fs::create_dir_all(out.join("memory-general")).unwrap();
        symlink(&outside, out.join("memory-general/references")).unwrap();
        let error = library.export(Some(&out)).unwrap_err();
        assert!(matches!(error, Error::InvalidMemory(_)));
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 2);
    }

    #[test]
    fn a_symlinked_skills_root_is_refused() {
        let (scratch, root) = sentinel_repo();
        let real = scratch.path().join("elsewhere");
        fs::create_dir_all(&real).unwrap();
        fs::remove_dir(root.join(".agents/skills")).unwrap();
        symlink(&real, root.join(".agents/skills")).unwrap();
        let error = Library::team(&root)
            .add(new_memory("T", "B", MemoryKind::Rule, &[]))
            .unwrap_err();
        assert!(matches!(error, Error::InvalidMemory(_)));
        assert_eq!(fs::read_dir(&real).unwrap().count(), 0);
    }

    #[test]
    fn a_manifest_id_that_is_a_path_is_refused_on_read() {
        let (_scratch, root) = sentinel_repo();
        let library = Library::team(&root);
        library
            .add(new_memory("First", "Body.", MemoryKind::Rule, &[]))
            .unwrap();
        let manifest = root.join(".agents/skills/memory-general/memory.json");
        let text = fs::read_to_string(&manifest).unwrap();
        assert!(text.contains("\"first\""));
        fs::write(
            &manifest,
            text.replace("\"first\"", "\"../../../sentinel\""),
        )
        .unwrap();
        let error = library.read().unwrap_err();
        assert!(
            matches!(&error, Error::InvalidMemory(m) if m.contains("not a slug")),
            "{error}"
        );
    }
}

/// Observations through real turns of the fake agent (docs/memory.md,
/// section 2): a correction and a repeated command observed at the turn's
/// end and stored once per turn, capture off, a recreated branch a new
/// incarnation, a private yard feeding one person's scope.
mod engine {
    use branchyard::memory::Visibility;
    use branchyard::{MemorySettings, ObservationKind};

    use crate::common::Fixture;

    const REVIEW: &str = "File: notes.txt\nLine: 1\nUser comment: \"Always end notes with a \
                          summary line, and date it\"";

    fn kinds(f: &Fixture, since: u64) -> Vec<(ObservationKind, String, String, u32)> {
        f.yard
            .observations(&f.yard.memory_scope("local"), since, 100)
            .unwrap()
            .into_iter()
            .map(|o| (o.kind, o.text.clone(), o.branch.clone(), o.turn))
            .collect()
    }

    #[test]
    fn a_turns_end_derives_its_observations_once_and_a_recreated_branch_is_new() {
        let f = Fixture::new();
        let branch = f.task("Add notes WRITE notes.txt=1").run().unwrap();
        assert!(
            kinds(&f, 0).is_empty(),
            "the first turn is the task: {:?}",
            kinds(&f, 0)
        );
        let sent = branch
            .send(&format!("{REVIEW}\nSH false\nSH false"), f.options())
            .unwrap();
        let observed = kinds(&f, 0);
        assert_eq!(
            observed,
            [
                (
                    ObservationKind::Correction,
                    "[review] Always end notes with a summary line, and date it".to_owned(),
                    sent.info().name.clone(),
                    2
                ),
                (
                    ObservationKind::RepeatedCommand,
                    "false (2 times)".to_owned(),
                    sent.info().name.clone(),
                    2
                ),
            ],
            "{observed:?}"
        );
        let first = f
            .yard
            .observations(&f.yard.memory_scope("local"), 0, 10)
            .unwrap();
        assert_eq!(first[0].paths, ["notes.txt"]);
        assert_eq!(first[0].harness, "gemini-cli");
        assert_eq!(first[0].incarnation, first[0].events[0].incarnation);
        assert!(first[0].author.is_none(), "a review comment has no author");
        // The same turn derived again (a settle replayed) yields the same
        // set.
        let again = f.yard.observe_turn(&sent.info().name, 2, &[]).unwrap();
        assert_eq!(
            again,
            first.iter().map(|o| o.id).collect::<Vec<_>>(),
            "idempotent per turn"
        );
        // Capture off: a turn derives nothing.
        f.yard.use_memory(MemorySettings {
            capture: false,
            ..MemorySettings::default()
        });
        branch
            .send("Keep every note short WRITE notes.txt=3", f.options())
            .unwrap();
        assert_eq!(kinds(&f, 0).len(), 2);
        f.yard.use_memory(MemorySettings::default());
        // A branch removed and recreated is a new incarnation: a new source
        // for the same correction, and the old observations keep their text.
        let name = sent.info().name.clone();
        f.yard.remove(&name).unwrap();
        let recreated = f
            .task("Add notes WRITE notes.txt=1")
            .name(&name)
            .run()
            .unwrap();
        recreated.send(REVIEW, f.options()).unwrap();
        let all = f
            .yard
            .observations(&f.yard.memory_scope("local"), 0, 10)
            .unwrap();
        assert_eq!(all.len(), 3, "{all:?}");
        assert_ne!(all[0].incarnation, all[2].incarnation);
        assert_eq!(all[0].text, all[2].text);
    }

    #[test]
    fn a_private_session_feeds_one_persons_scope() {
        let f = Fixture::new();
        f.yard.use_memory(MemorySettings {
            visibility: Visibility::Private {
                subject: "ana".into(),
            },
            ..MemorySettings::default()
        });
        for name in ["one", "two"] {
            let branch = f
                .task("Add notes WRITE notes.txt=1")
                .name(name)
                .run()
                .unwrap();
            branch.send(REVIEW, f.options()).unwrap();
        }
        let team = f.yard.memory_scope("local");
        let mine = f.yard.person_memory_scope("local", "ana");
        assert!(f.yard.observations(&team, 0, 10).unwrap().is_empty());
        let private = f.yard.observations(&mine, 0, 10).unwrap();
        assert_eq!(private.len(), 2);
        assert_eq!(private[0].visibility.subject(), Some("ana"));
        assert!(private.iter().all(|o| o.tenant == "local"));
    }
}
