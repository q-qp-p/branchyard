//! `by memory add|list|show|edit|remove|export`: memory as a reviewed skill
//! (docs/memory.md, section 1); and, hidden, `observations`: what each
//! turn's end derived (section 2).
//!
//! A memory is one Markdown file in the bundle of its topic,
//! `memory-<topic>/references/<slug>.md`, beside a `SKILL.md` and a
//! `memory.json` the writer rewrites deterministically. Without `--mine`
//! the bundles are the repository's, under `.agents/skills/` of this
//! checkout: commit them and open the pull request, and turns find them
//! once `by skills trust` trusts the set. With `--mine` they are yours, in
//! your skills folder (`~/.config/branchyard/skills/`), approved by you at
//! once and staged for the sessions you start.
//!
//! A harness on a branch (`BRANCHYARD_BRANCH` set) cannot add, edit or
//! remove: a harness proposes nothing. Locally the refusal is advisory
//! (the variable can be unset); on a server it is enforced, because a turn
//! holds no `run` token and the repository's files reach a turn only
//! through a commit someone reviews. `--remote` is refused: memory is files
//! in a worktree or a person's folder.

use std::fs;
use std::path::{Path, PathBuf};

use branchyard::memory::observe::LOCAL_TENANT;
use branchyard::memory::{self, Audience, Library, Memory, MemoryEdit, MemoryKind, NewMemory};
use branchyard::{Observation, TaskKind};
use serde_json::{json, Value};

use crate::args::MemoryAction;
use crate::commands::{self, print, Failure, Outcome, Target};
use crate::errors::Report;
use crate::plan_cmd::person;
use crate::skills_cmd::user_skills_dir;
use crate::workspace_cmd::in_harness;

/// `by memory ACTION`.
pub fn main(target: &Target, action: &MemoryAction, json: bool) -> Outcome {
    run(target, action, json).map_err(|failure| match failure {
        // A symbolic link in a bundle is refused by the engine; say what to do.
        Failure::Sdk(branchyard::Error::InvalidMemory(message))
            if message.contains("is a symbolic link") =>
        {
            Report::new("invalid_memory", message)
                .next("remove the symbolic link from the bundle, then run by memory again")
                .into()
        }
        other => other,
    })
}

fn run(target: &Target, action: &MemoryAction, json: bool) -> Outcome {
    if let Target::Remote(_) = target {
        return Err(Report::usage(
            "by memory writes files in a worktree or in your own skills folder; a server has \
             neither",
        )
        .next("run by memory without --remote")
        .into());
    }
    match action {
        MemoryAction::Add {
            title,
            body_file,
            from,
            kind,
            paths,
            task_kinds,
            topic,
            mine,
        } => {
            refuse_in_harness("add")?;
            add(
                AddArgs {
                    title: title.as_deref(),
                    body_file: body_file.as_deref(),
                    from: from.as_deref(),
                    kind: *kind,
                    paths,
                    task_kinds,
                    topic: topic.as_deref(),
                    mine: *mine,
                },
                json,
            )
        }
        MemoryAction::List { mine } => list(*mine, json),
        MemoryAction::Show { slug, mine } => show(slug, *mine, json),
        MemoryAction::Edit {
            slug,
            title,
            body_file,
            kind,
            paths,
            task_kinds,
            mine,
        } => {
            refuse_in_harness("edit")?;
            let body = match body_file {
                Some(file) => Some(read(file)?),
                None => None,
            };
            let (library, where_) = library_of(slug, *mine)?;
            let written = library.edit(
                slug,
                MemoryEdit {
                    title: title.clone(),
                    body,
                    kind: *kind,
                    paths: given(paths),
                    task_kinds: (!task_kinds.is_empty()).then(|| task_kinds.clone()),
                    by: person(),
                    now_ms: branchyard_support::time::now_ms(),
                },
            )?;
            if json {
                return print(&to_json(&json!({
                    "memory": written.memory,
                    "bundles": written.bundles,
                    "dir": library.dir(),
                    "mine": where_ == Where::Mine,
                })));
            }
            print(&format!(
                "edited {} in memory-{} ({}); {}\n",
                written.memory.id,
                written.memory.topic,
                written.memory.kind.as_str(),
                next_step(&library, &written.memory.topic)
            ))
        }
        MemoryAction::Remove { slug, mine } => {
            refuse_in_harness("remove")?;
            let (library, where_) = library_of(slug, *mine)?;
            let removed = library.remove(slug)?;
            if json {
                return print(&to_json(&json!({
                    "memory": removed,
                    "dir": library.dir(),
                    "mine": where_ == Where::Mine,
                })));
            }
            print(&format!(
                "removed {} from memory-{}; {}\n",
                removed.id,
                removed.topic,
                next_step(&library, &removed.topic)
            ))
        }
        MemoryAction::Export { out, mine } => export(out.as_deref(), *mine, json),
        MemoryAction::Observations { mine, since, limit } => {
            observations(*mine, *since, *limit, json)
        }
    }
}

/// Which library a memory is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Where {
    Repository,
    Mine,
}

impl Where {
    fn as_str(self) -> &'static str {
        match self {
            Where::Repository => "repository",
            Where::Mine => "mine",
        }
    }
}

/// `by memory` refuses to write inside a harness: a harness proposes
/// nothing. Advisory locally; a server enforces it, since a turn holds no
/// `run` token.
fn refuse_in_harness(what: &str) -> Result<(), Failure> {
    match in_harness() {
        Some(_) => Err(Report::new(
            "denied",
            format!(
                "a harness running on a branch cannot {what} memory; what a team or a person \
                 remembers is a person's decision, made outside any branch"
            ),
        )
        .next(format!(
            "say what you learned in your report; a person runs by memory {what} in their own \
             terminal"
        ))
        .into()),
        None => Ok(()),
    }
}

fn to_json(value: &Value) -> String {
    format!("{value:#}\n")
}

/// The team's library, under the repository's `.agents/skills/`.
fn team() -> Result<Library, Failure> {
    Ok(Library::team(commands::open()?.root()))
}

/// Your own library, under your skills folder.
fn mine() -> Library {
    Library::person(&user_skills_dir())
}

/// The library holding `slug`: the repository's first, then yours; only
/// yours with `mine`.
fn library_of(slug: &str, mine_only: bool) -> Result<(Library, Where), Failure> {
    if mine_only {
        return Ok((mine(), Where::Mine));
    }
    let team = team()?;
    if team.read()?.iter().any(|m| m.id == slug) {
        return Ok((team, Where::Repository));
    }
    let own = mine();
    if own.read()?.iter().any(|m| m.id == slug) {
        return Ok((own, Where::Mine));
    }
    Err(Report::new("unknown_memory", format!("no memory {slug}"))
        .next("by memory list shows every slug")
        .into())
}

fn read(path: &Path) -> Result<String, Failure> {
    fs::read_to_string(path).map_err(|e| {
        Report::new("io", format!("could not read {}: {e}", path.display()))
            .next("pass a file that exists")
            .into()
    })
}

/// `--path` flags as an edit: none given keeps the paths; `""` alone
/// clears them.
fn given(paths: &[String]) -> Option<Vec<String>> {
    match paths.is_empty() {
        true => None,
        false => Some(
            paths
                .iter()
                .filter(|p| !p.trim().is_empty())
                .cloned()
                .collect(),
        ),
    }
}

/// What to do once a topic's bundle was rewritten.
fn next_step(library: &Library, topic: &str) -> String {
    match library.audience() {
        Audience::Team => format!(
            "commit .agents/skills/memory-{topic} and open the pull request; new branches stage \
             it after by skills trust"
        ),
        Audience::Person => "your new branches stage it".into(),
    }
}

/// `by memory add`'s flags.
struct AddArgs<'a> {
    title: Option<&'a str>,
    body_file: Option<&'a Path>,
    from: Option<&'a Path>,
    kind: Option<MemoryKind>,
    paths: &'a [String],
    task_kinds: &'a [TaskKind],
    topic: Option<&'a str>,
    mine: bool,
}

fn add(args: AddArgs<'_>, json: bool) -> Outcome {
    let AddArgs {
        title,
        body_file,
        from,
        kind,
        paths,
        task_kinds,
        topic,
        mine: mine_only,
    } = args;
    let mut paths: Vec<String> = paths.to_vec();
    let (title, body, kind) = match from {
        Some(draft) => {
            let parsed = memory::parse_draft(&read(draft)?)?;
            let Some(title) = title.map(str::to_owned).or(parsed.title) else {
                return Err(Report::usage(format!(
                    "{} has no title in its frontmatter or first heading",
                    draft.display()
                ))
                .next("pass --title TEXT")
                .into());
            };
            if parsed.body.trim().is_empty() {
                return Err(Report::usage(format!("{} has no body", draft.display()))
                    .next("write the memory's body under its frontmatter")
                    .into());
            }
            if paths.is_empty() {
                paths = parsed.paths;
            }
            (
                title,
                parsed.body,
                kind.or(parsed.kind).unwrap_or(MemoryKind::Rule),
            )
        }
        None => {
            let Some(title) = title else {
                return Err(Report::usage("a memory needs --title TEXT")
                    .next("by memory add --title TEXT --body-file FILE")
                    .next("by memory add --from DRAFT")
                    .into());
            };
            let Some(file) = body_file else {
                return Err(Report::usage("a memory's body comes from a file")
                    .next("by memory add --title TEXT --body-file FILE")
                    .next("by memory add --from DRAFT")
                    .into());
            };
            (
                title.to_owned(),
                read(file)?,
                kind.unwrap_or(MemoryKind::Rule),
            )
        }
    };
    let library = match mine_only {
        true => mine(),
        false => team()?,
    };
    let written = library.add(NewMemory {
        title,
        body,
        kind,
        paths,
        task_kinds: task_kinds.to_vec(),
        topic: topic.map(str::to_owned),
        by: person(),
        now_ms: branchyard_support::time::now_ms(),
        evidence: Vec::new(),
    })?;
    let mut removed_draft = None;
    if let Some(draft) = from {
        fs::remove_file(draft).map_err(|e| {
            Report::new(
                "io",
                format!(
                    "added, but could not remove the draft {}: {e}",
                    draft.display()
                ),
            )
            .next("remove it by hand")
        })?;
        removed_draft = Some(draft.to_path_buf());
    }
    if json {
        return print(&to_json(&json!({
            "memory": written.memory,
            "bundles": written.bundles,
            "dir": library.dir(),
            "mine": mine_only,
            "draft_removed": removed_draft,
        })));
    }
    let mut text = format!(
        "added {} to memory-{} ({}) under {}\n{}\n",
        written.memory.id,
        written.memory.topic,
        written.memory.kind.as_str(),
        library.dir().display(),
        next_step(&library, &written.memory.topic)
    );
    if let Some(draft) = removed_draft {
        text.push_str(&format!("removed the draft {}\n", draft.display()));
    }
    print(&text)
}

/// One memory's line in a listing.
fn line(memory: &Memory, width: usize) -> String {
    format!(
        "  {:<width$}  {:<9}  {}\n",
        memory.id,
        memory.kind.as_str(),
        memory.title
    )
}

/// The text of one library's memories, grouped by topic.
fn listing(memories: &[Memory], where_: Where) -> String {
    let mut text = String::new();
    let width = memories.iter().map(|m| m.id.len()).max().unwrap_or(0);
    let mut topic: Option<&str> = None;
    for memory in memories {
        if topic != Some(memory.topic.as_str()) {
            let count = memories.iter().filter(|m| m.topic == memory.topic).count();
            text.push_str(&format!(
                "memory-{} ({}, {count})\n",
                memory.topic,
                where_.as_str()
            ));
            topic = Some(memory.topic.as_str());
        }
        text.push_str(&line(memory, width));
    }
    text
}

fn list(mine_only: bool, json: bool) -> Outcome {
    let own = mine();
    let yours = own.read()?;
    let team = match mine_only {
        true => None,
        false => Some(team()?.read()?),
    };
    if json {
        let mut value = json!({ "mine": yours, "mine_dir": own.dir() });
        if let Some(team) = &team {
            value["repository"] = json!(team);
        }
        return print(&to_json(&value));
    }
    let mut text = String::new();
    if let Some(team) = &team {
        match team.is_empty() {
            true => text.push_str(
                "repository: no memory bundles under .agents/skills (by memory add --title ... \
                 --body-file ...)\n",
            ),
            false => text.push_str(&listing(team, Where::Repository)),
        }
    }
    match yours.is_empty() {
        true => text.push_str(&format!(
            "mine: no memory bundles in {} (by memory add --mine ...)\n",
            own.dir().display()
        )),
        false => text.push_str(&listing(&yours, Where::Mine)),
    }
    print(&text)
}

fn show(slug: &str, mine_only: bool, json: bool) -> Outcome {
    let (library, where_) = library_of(slug, mine_only)?;
    let memory = library.get(slug)?;
    if json {
        return print(&to_json(&json!({
            "memory": memory,
            "where": where_.as_str(),
            "path": library.dir().join(format!("memory-{}", memory.topic)).join("references").join(format!("{slug}.md")),
        })));
    }
    let mut text = memory::reference(&memory);
    text.push_str(&format!(
        "\ntopic {} ({}), kind {}, paths {}, task kinds {}\n",
        memory.topic,
        where_.as_str(),
        memory.kind.as_str(),
        match memory.paths.is_empty() {
            true => "the whole repository".to_owned(),
            false => memory.paths.join(", "),
        },
        match memory.task_kinds.is_empty() {
            true => "every kind".to_owned(),
            false => memory
                .task_kinds
                .iter()
                .map(|k| k.to_string())
                .collect::<Vec<_>>()
                .join(", "),
        }
    ));
    print(&text)
}

fn export(out: Option<&Path>, mine_only: bool, json: bool) -> Outcome {
    let library = match mine_only {
        true => mine(),
        false => team()?,
    };
    let bundles = library.export(out)?;
    if json {
        let listed: Vec<Value> = bundles
            .iter()
            .map(|b| {
                json!({
                    "name": b.name,
                    "digest": b.digest,
                    "files": b.files.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>(),
                    "bytes": b.files.iter().map(|(_, c)| c.len()).sum::<usize>(),
                })
            })
            .collect();
        return print(&to_json(&json!({
            "dir": library.dir(),
            "out": out.map(Path::to_path_buf),
            "bundles": listed,
        })));
    }
    if bundles.is_empty() {
        return print(&format!(
            "no memory bundles under {}\n",
            library.dir().display()
        ));
    }
    let mut text = String::new();
    for bundle in &bundles {
        text.push_str(&format!(
            "{}  {}  {} files, {} bytes\n",
            bundle.name,
            bundle.digest.get(..12).unwrap_or(&bundle.digest),
            bundle.files.len(),
            bundle.files.iter().map(|(_, c)| c.len()).sum::<usize>()
        ));
    }
    if let Some(out) = out {
        text.push_str(&format!("written under {}\n", out.display()));
    }
    print(&text)
}

/// The path a draft of `slug` is written to under `root`, not taken by a
/// different draft.
pub(crate) fn draft_path(root: &Path, slug: &str, text: &str) -> PathBuf {
    let dir = root.join(memory::DRAFTS);
    let mut n = 1;
    loop {
        let name = match n {
            1 => format!("{slug}.md"),
            n => format!("{slug}-{n}.md"),
        };
        let path = dir.join(name);
        match fs::read_to_string(&path) {
            Ok(existing) if existing != text => n += 1,
            _ => return path,
        }
    }
}

// ---------------------------------------------------------------------------
// Observations: what each turn's end derived (docs/memory.md, section 2),
// read where they are stored.

/// `by memory observations`: the scope's observations, oldest first.
fn observations(mine: bool, since: u64, limit: usize, json: bool) -> Outcome {
    let yard = commands::open()?;
    let scope = match mine {
        true => yard.person_memory_scope(LOCAL_TENANT, &person()),
        false => yard.memory_scope(LOCAL_TENANT),
    };
    let observations = yard.observations(&scope, since, limit)?;
    if json {
        return print(&format!("{}\n", commands::to_json(&observations)));
    }
    match observations.is_empty() {
        true => print("no observations\n"),
        false => print(
            &observations
                .iter()
                .map(observation_line)
                .collect::<String>(),
        ),
    }
}

/// One observation's line.
fn observation_line(observation: &Observation) -> String {
    let author = match &observation.author {
        Some(author) => format!(
            " by {} ({})",
            author.name,
            match author.association {
                branchyard::memory::Association::Member => "member",
                branchyard::memory::Association::External => "external",
            }
        ),
        None => String::new(),
    };
    let text: String = observation
        .text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let text = match text.chars().count() > 120 {
        true => format!("{}…", text.chars().take(119).collect::<String>()),
        false => text,
    };
    format!(
        "  o{:<5} {} turn {} {}{}: {}\n",
        observation.id,
        observation.branch,
        observation.turn,
        observation.kind.as_str(),
        author,
        text
    )
}
