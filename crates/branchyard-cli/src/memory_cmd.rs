//! `by memory`: memory as a reviewed skill (docs/memory.md).
//!
//! Two kinds of thing answer to it. **Files** (section 1): a memory is one
//! Markdown file in the bundle of its topic,
//! `memory-<topic>/references/<slug>.md`, beside a `SKILL.md` and a
//! `memory.json` the writer rewrites deterministically; `add`, `list`,
//! `show SLUG`, `edit SLUG`, `remove` and `export` work on them, under
//! `.agents/skills/` of this checkout (commit them and open the pull
//! request; turns find them once `by skills trust` trusts the set) or with
//! `--mine` in your skills folder, approved by you at once. **Rows**
//! (sections 3 to 5): what consolidation proposed from the observations of
//! many turns, by number; `list --status`, `show ID`, `review`, `approve`,
//! `reject`, `edit ID`, `promote`, `forget`, `consolidate`, `runs` work on
//! them, locally or with `--remote`, and `approve` writes the row into its
//! bundle through the same writer.
//!
//! A harness on a branch (`BRANCHYARD_BRANCH` set) cannot add, edit,
//! remove, approve, reject, promote or forget: a harness proposes nothing.
//! Locally the refusal is advisory (the variable can be unset); on a server
//! it is enforced, because a turn holds no `run` token. `--remote` is
//! refused for the file commands: memory files are in a worktree or a
//! person's folder.

use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use branchyard::memory::observe::LOCAL_TENANT;
use branchyard::memory::{self, Audience, Library, Memory, MemoryEdit, MemoryKind, NewMemory};
use branchyard::{
    Consolidated, MemoryFilter, MemoryRecord, MemoryRecordEdit, MemoryRun, MemoryScope,
    MemoryStatus, Observation, TaskKind, Yard,
};
use branchyard_client::memory_api::{MemoryEditRequest, SCOPE_MINE, SCOPE_REPO};
use serde_json::{json, Value};

use crate::args::MemoryAction;
use crate::commands::{self, print, Failure, Outcome, Target};
use crate::errors::Report;
use crate::plan_cmd::{edit_text, person};
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

/// A row's number in `12` or `#12`, when `text` is one and not a slug.
fn row_id(text: &str) -> Option<u64> {
    text.strip_prefix('#')
        .unwrap_or(text)
        .parse::<u64>()
        .ok()
        .filter(|_| text.starts_with('#') || text.chars().all(|c| c.is_ascii_digit()))
}

fn run(target: &Target, action: &MemoryAction, json: bool) -> Outcome {
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
            files_only(target)?;
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
        MemoryAction::List { mine, status } => match status {
            Some(status) => list_rows(&Rows::open(target)?, *mine, Some(*status), json),
            None => {
                files_only(target)?;
                list(*mine, json)
            }
        },
        MemoryAction::Show {
            slug,
            mine,
            evidence,
        } => match row_id(slug) {
            Some(id) => show_row(&Rows::open(target)?, id, *evidence, json),
            None => {
                files_only(target)?;
                show(slug, *mine, json)
            }
        },
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
            if let Some(id) = row_id(slug) {
                let rows = Rows::open(target)?;
                let edited = rows.edit(
                    id,
                    &MemoryEditRequest {
                        title: title.clone(),
                        body,
                        kind: *kind,
                        paths: given(paths),
                        task_kinds: (!task_kinds.is_empty()).then(|| task_kinds.clone()),
                    },
                )?;
                return emit(json, &edited, || {
                    format!("edited #{}: {}\n", edited.id, row_line(&edited))
                });
            }
            files_only(target)?;
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
            files_only(target)?;
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
        MemoryAction::Export { out, mine } => {
            files_only(target)?;
            export(out.as_deref(), *mine, json)
        }
        MemoryAction::Review { mine, editor } => {
            refuse_in_harness("approve")?;
            review(&Rows::open(target)?, *mine, editor.as_deref(), json)
        }
        MemoryAction::Approve { ids } => {
            refuse_in_harness("approve")?;
            let rows = Rows::open(target)?;
            let mut done = Vec::new();
            for id in ids {
                done.push(rows.approve(*id)?);
            }
            emit(json, &done, || {
                done.iter()
                    .map(|m| format!("approved #{}: {}\n", m.id, approved_where(m)))
                    .collect()
            })
        }
        MemoryAction::Reject { ids, reason } => {
            refuse_in_harness("reject")?;
            let rows = Rows::open(target)?;
            let mut done = Vec::new();
            for id in ids {
                done.push(rows.reject(*id, reason.as_deref())?);
            }
            emit(json, &done, || {
                done.iter()
                    .map(|m| format!("rejected #{}: {}\n", m.id, m.title))
                    .collect()
            })
        }
        MemoryAction::Promote { id } => {
            refuse_in_harness("promote")?;
            let promoted = Rows::open(target)?.promote(*id)?;
            emit(json, &promoted, || {
                format!(
                    "promoted #{id} as #{} for the team, proposed; by memory approve {} makes it \
                     theirs\n",
                    promoted.id, promoted.id
                )
            })
        }
        MemoryAction::Forget { id } => {
            refuse_in_harness("forget")?;
            let forgotten = Rows::open(target)?.forget(*id)?;
            emit(json, &forgotten, || {
                format!("forgot #{}: {}\n", forgotten.id, forgotten.title)
            })
        }
        MemoryAction::Consolidate { mine } => {
            let consolidated = Rows::open(target)?.consolidate(*mine)?;
            emit(json, &consolidated, || consolidated_text(&consolidated))
        }
        MemoryAction::Runs { mine, limit } => {
            let runs = Rows::open(target)?.runs(*mine, *limit)?;
            emit(json, &runs, || runs_text(&runs))
        }
        MemoryAction::Observations { mine, since, limit } => {
            let observations = Rows::open(target)?.observations(*mine, *since, *limit)?;
            emit(json, &observations, || match observations.is_empty() {
                true => "no observations\n".to_owned(),
                false => observations.iter().map(observation_line).collect(),
            })
        }
    }
}

/// The file commands have no server counterpart.
fn files_only(target: &Target) -> Result<(), Failure> {
    match target {
        Target::Remote(_) => Err(Report::usage(
            "by memory writes files in a worktree or in your own skills folder; a server has \
             neither",
        )
        .next("run by memory without --remote")
        .next("the row commands (list --status, show ID, review, approve, reject, consolidate, runs) work with --remote")
        .into()),
        Target::Local => Ok(()),
    }
}

fn emit<T: serde::Serialize>(json: bool, value: &T, text: impl FnOnce() -> String) -> Outcome {
    match json {
        true => print(&format!("{}\n", commands::to_json(value))),
        false => print(&text()),
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

/// `by memory` refuses to decide inside a harness: a harness proposes
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
        .next("by memory list shows every slug and row number")
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
    // The proposed rows wait for a decision: say so under the files.
    let yard = commands::open()?;
    let rows = Rows::Local(yard);
    let proposed = rows.list(mine_only, Some(MemoryStatus::Proposed))?;
    if json {
        let mut value = json!({ "mine": yours, "mine_dir": own.dir(), "proposed": proposed });
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
    if !proposed.is_empty() {
        text.push_str(&format!(
            "proposed ({}), waiting for a decision (by memory review):\n",
            proposed.len()
        ));
        for row in &proposed {
            text.push_str(&format!("  {}\n", row_line(row)));
        }
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
// Rows: what consolidation proposed, locally or on a server.

/// The rows' store: the local yard, or a server's repository.
enum Rows<'a> {
    Local(Yard),
    Remote(&'a branchyard_client::Repo),
}

impl<'a> Rows<'a> {
    fn open(target: &'a Target) -> Result<Rows<'a>, Failure> {
        Ok(match target {
            Target::Local => Rows::Local(commands::open()?),
            Target::Remote(remote) => Rows::Remote(&remote.repo),
        })
    }

    /// The local scope: the team's, or with `mine` the person's own.
    fn scope(yard: &Yard, mine: bool) -> MemoryScope {
        match mine {
            true => yard.person_memory_scope(LOCAL_TENANT, &person()),
            false => yard.memory_scope(LOCAL_TENANT),
        }
    }

    fn remote_scope(mine: bool) -> &'static str {
        match mine {
            true => SCOPE_MINE,
            false => SCOPE_REPO,
        }
    }

    /// The person's skills folder, where their approved rows are written.
    fn person_dir() -> PathBuf {
        user_skills_dir()
    }

    /// Rows of the team and, unless `mine`, also the person's own... with
    /// `mine`, only the person's own.
    fn list(&self, mine: bool, status: Option<MemoryStatus>) -> Result<Vec<MemoryRecord>, Failure> {
        Ok(match self {
            Rows::Local(yard) => {
                let mut rows = Vec::new();
                if !mine {
                    rows.extend(yard.memory_records(&MemoryFilter {
                        scope: Some(Rows::scope(yard, false)),
                        status,
                        ..MemoryFilter::default()
                    })?);
                }
                rows.extend(yard.memory_records(&MemoryFilter {
                    scope: Some(Rows::scope(yard, true)),
                    status,
                    ..MemoryFilter::default()
                })?);
                rows.sort_by_key(|m| m.id);
                rows
            }
            Rows::Remote(repo) => {
                let mut rows = Vec::new();
                if !mine {
                    rows.extend(repo.memories(Some(SCOPE_REPO), status)?);
                }
                rows.extend(repo.memories(Some(SCOPE_MINE), status)?);
                rows.sort_by_key(|m| m.id);
                rows
            }
        })
    }

    fn get(&self, id: u64) -> Result<MemoryRecord, Failure> {
        Ok(match self {
            Rows::Local(yard) => yard.memory_record(id)?,
            Rows::Remote(repo) => repo.memory(id)?,
        })
    }

    fn evidence(&self, id: u64) -> Result<Vec<Observation>, Failure> {
        Ok(match self {
            Rows::Local(yard) => yard.memory_evidence(id)?,
            Rows::Remote(repo) => repo.memory_evidence(id)?.observations,
        })
    }

    fn approve(&self, id: u64) -> Result<MemoryRecord, Failure> {
        Ok(match self {
            Rows::Local(yard) => {
                yard.approve_memory(id, &person(), Some(&Rows::person_dir()))?
                    .record
            }
            Rows::Remote(repo) => repo.approve_memory(id)?,
        })
    }

    fn reject(&self, id: u64, reason: Option<&str>) -> Result<MemoryRecord, Failure> {
        Ok(match self {
            Rows::Local(yard) => yard.reject_memory(id, &person(), reason)?,
            Rows::Remote(repo) => repo.reject_memory(id, reason)?,
        })
    }

    fn edit(&self, id: u64, change: &MemoryEditRequest) -> Result<MemoryRecord, Failure> {
        Ok(match self {
            Rows::Local(yard) => yard.edit_memory_record(
                id,
                &MemoryRecordEdit {
                    title: change.title.clone(),
                    body: change.body.clone(),
                    kind: change.kind,
                    paths: change.paths.clone(),
                    task_kinds: change.task_kinds.clone(),
                },
                &person(),
                Some(&Rows::person_dir()),
            )?,
            Rows::Remote(repo) => repo.edit_memory(id, change)?,
        })
    }

    fn promote(&self, id: u64) -> Result<MemoryRecord, Failure> {
        Ok(match self {
            Rows::Local(yard) => yard.promote_memory(id, &person(), Rows::scope(yard, false))?,
            Rows::Remote(repo) => repo.promote_memory(id)?,
        })
    }

    fn forget(&self, id: u64) -> Result<MemoryRecord, Failure> {
        Ok(match self {
            Rows::Local(yard) => yard.forget_memory(id, &person(), Some(&Rows::person_dir()))?,
            Rows::Remote(repo) => repo.forget_memory(id)?,
        })
    }

    fn consolidate(&self, mine: bool) -> Result<Consolidated, Failure> {
        Ok(match self {
            Rows::Local(yard) => {
                yard.consolidate_memory(&Rows::scope(yard, mine), &person(), "asked")?
            }
            Rows::Remote(repo) => repo.consolidate_memory(Some(Rows::remote_scope(mine)))?,
        })
    }

    fn runs(&self, mine: bool, limit: usize) -> Result<Vec<MemoryRun>, Failure> {
        Ok(match self {
            Rows::Local(yard) => yard.memory_runs(Some(&Rows::scope(yard, mine)), limit)?,
            Rows::Remote(repo) => {
                let mut runs = repo.memory_runs(Some(Rows::remote_scope(mine)))?;
                runs.truncate(limit);
                runs
            }
        })
    }

    fn observations(
        &self,
        mine: bool,
        since: u64,
        limit: usize,
    ) -> Result<Vec<Observation>, Failure> {
        match self {
            Rows::Local(yard) => Ok(yard.observations(&Rows::scope(yard, mine), since, limit)?),
            Rows::Remote(_) => Err(Report::usage(
                "observations are read where they are stored: on the server's host",
            )
            .next("run by memory observations without --remote, or by memory show ID --evidence")
            .into()),
        }
    }
}

/// One row's line: `#12  proposed  rule   notes.txt  Title (seen 2)`.
fn row_line(row: &MemoryRecord) -> String {
    let paths = match row.paths.is_empty() {
        true => "repository".to_owned(),
        false => row.paths.join(","),
    };
    let whose = match &row.scope {
        MemoryScope::Repo { .. } => "team",
        MemoryScope::Person { .. } => "mine",
    };
    format!(
        "#{:<4} {:<9} {:<9} {:<5} {}  {} (seen {})",
        row.id,
        row.status.as_str(),
        row.kind.as_str(),
        whose,
        paths,
        row.title,
        row.seen
    )
}

/// Where an approved row was written.
fn approved_where(row: &MemoryRecord) -> String {
    match &row.published {
        Some(published) => format!(
            "{} written to {}/{}",
            row.title, published.skill, published.path
        ),
        None => row.title.clone(),
    }
}

/// A row, whole.
fn row_text(row: &MemoryRecord) -> String {
    let mut text = format!(
        "#{} {} ({}, {}, {})\n",
        row.id,
        row.title,
        row.status.as_str(),
        row.kind.as_str(),
        row.scope.describe()
    );
    text.push_str(&format!(
        "paths {}; task kinds {}; seen {}; first {}, last {}\n",
        match row.paths.is_empty() {
            true => "the whole repository".to_owned(),
            false => row.paths.join(", "),
        },
        match row.task_kinds.is_empty() {
            true => "every kind".to_owned(),
            false => row
                .task_kinds
                .iter()
                .map(|k| k.to_string())
                .collect::<Vec<_>>()
                .join(", "),
        },
        row.seen,
        branchyard_support::time::rfc3339_secs(row.first_seen_ms),
        branchyard_support::time::rfc3339_secs(row.last_seen_ms)
    ));
    if let Some(by) = &row.decided_by {
        text.push_str(&format!("decided by {by}"));
        if let Some(at) = row.decided_ms {
            text.push_str(&format!(
                " at {}",
                branchyard_support::time::rfc3339_secs(at)
            ));
        }
        text.push('\n');
    }
    if let Some(note) = &row.note {
        text.push_str(&format!("note: {note}\n"));
    }
    if let Some(published) = &row.published {
        text.push_str(&format!("in {}/{}\n", published.skill, published.path));
    }
    text.push_str(&format!("\n{}\n", row.body.trim_end()));
    text
}

/// One observation's line, for evidence and `observations`.
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

fn show_row(rows: &Rows<'_>, id: u64, evidence: bool, json: bool) -> Outcome {
    let row = rows.get(id)?;
    let observations = match evidence {
        true => rows.evidence(id)?,
        false => Vec::new(),
    };
    if json {
        return print(&to_json(&json!({
            "memory": row,
            "evidence": observations,
        })));
    }
    let mut text = row_text(&row);
    if evidence {
        text.push_str(&format!("\nevidence ({}):\n", observations.len()));
        for observation in &observations {
            text.push_str(&observation_line(observation));
        }
    }
    print(&text)
}

fn list_rows(rows: &Rows<'_>, mine: bool, status: Option<MemoryStatus>, json: bool) -> Outcome {
    let listed = rows.list(mine, status)?;
    emit(json, &listed, || match listed.is_empty() {
        true => "no memory rows\n".to_owned(),
        false => listed
            .iter()
            .map(|r| format!("{}\n", row_line(r)))
            .collect(),
    })
}

fn consolidated_text(consolidated: &Consolidated) -> String {
    if consolidated.lost_race {
        return format!(
            "run {} lost the race: another consolidation of {} moved the watermark first; \
             nothing was written\n",
            consolidated.run.id, consolidated.run.scope
        );
    }
    let counts = &consolidated.run.counts;
    let mut text = format!(
        "run {} ({}): {} observation(s) in {} cluster(s); {} proposed, {} as evidence, {} to \
         knowledge, {} expired\n",
        consolidated.run.id,
        consolidated.run.scope,
        counts.observations,
        counts.clusters,
        counts.proposed,
        counts.merged,
        counts.knowledge,
        counts.expired
    );
    for row in &consolidated.proposed {
        text.push_str(&format!("  {}\n", row_line(row)));
    }
    if !consolidated.proposed.is_empty() {
        text.push_str("review them with: by memory review\n");
    }
    if !consolidated.knowledge.is_empty() {
        text.push_str(&format!(
            "proposed knowledge {}: by knowledge review\n",
            consolidated
                .knowledge
                .iter()
                .map(|id| format!("k{id}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    text
}

fn runs_text(runs: &[MemoryRun]) -> String {
    if runs.is_empty() {
        return "no consolidation runs yet (by memory consolidate)\n".into();
    }
    let mut text = String::new();
    for run in runs {
        text.push_str(&format!(
            "{:<5} {}  {}  {}  by {} ({}): {} obs, {} clusters, {} proposed, {} evidence, {} \
             knowledge, {} expired\n",
            run.id,
            branchyard_support::time::rfc3339_secs(run.started_ms),
            run.scope,
            run.outcome.as_deref().unwrap_or("running"),
            run.by,
            run.trigger,
            run.counts.observations,
            run.counts.clusters,
            run.counts.proposed,
            run.counts.merged,
            run.counts.knowledge,
            run.counts.expired
        ));
    }
    text
}

/// Walk the proposed rows: approve, reject, edit then approve, promote or
/// skip each, reading one answer a line from stdin.
fn review(rows: &Rows<'_>, mine: bool, editor: Option<&str>, json: bool) -> Outcome {
    let proposed = rows.list(mine, Some(MemoryStatus::Proposed))?;
    if proposed.is_empty() {
        return emit(json, &json!({"decisions": []}), || {
            "nothing to review: no proposed memory\n".into()
        });
    }
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let mut decisions = Vec::new();
    let total = proposed.len();
    for (n, row) in proposed.into_iter().enumerate() {
        let mut err = io::stderr();
        let evidence = rows.evidence(row.id)?;
        let mut shown = row_text(&row);
        shown.push_str(&format!(
            "\nevidence ({}, from {}):\n",
            evidence.len(),
            branches_of(&evidence)
        ));
        for observation in &evidence {
            shown.push_str(&observation_line(observation));
        }
        let promote = match row.scope {
            MemoryScope::Person { .. } => ", [p]romote to the team",
            MemoryScope::Repo { .. } => "",
        };
        branchyard_support::best_effort(
            "write the review prompt",
            write!(
                err,
                "\n[{}/{total}]\n{shown}[a]pprove, [r]eject, [e]dit then approve{promote}, [s]kip, [q]uit? ",
                n + 1
            ),
        );
        branchyard_support::best_effort("flush the review prompt", err.flush());
        let Some(answer) = lines.next().transpose()? else {
            break;
        };
        let answer = answer.trim().to_lowercase();
        let decision = match answer.as_str() {
            "a" | "approve" | "y" | "yes" => Some(("approved", rows.approve(row.id)?)),
            "r" | "reject" | "n" | "no" => {
                branchyard_support::best_effort(
                    "write the reason prompt",
                    write!(err, "reason (optional): "),
                );
                branchyard_support::best_effort("flush the reason prompt", err.flush());
                let reason = lines.next().transpose()?.unwrap_or_default();
                let reason = Some(reason.trim()).filter(|r| !r.is_empty());
                Some(("rejected", rows.reject(row.id, reason)?))
            }
            "e" | "edit" => {
                let body = edit_text(&row.body, editor, &format!("memory-{}", row.id))?;
                rows.edit(
                    row.id,
                    &MemoryEditRequest {
                        body: Some(body),
                        ..MemoryEditRequest::default()
                    },
                )?;
                Some(("edited and approved", rows.approve(row.id)?))
            }
            "p" | "promote" if matches!(row.scope, MemoryScope::Person { .. }) => {
                Some(("promoted", rows.promote(row.id)?))
            }
            "q" | "quit" => break,
            _ => None,
        };
        if let Some((what, row)) = decision {
            if !json {
                eprintln!("{what} #{}", row.id);
            }
            decisions.push(json!({"decision": what, "memory": row}));
        }
    }
    emit(json, &json!({"decisions": decisions}), || {
        format!("{} decision(s) made\n", decisions.len())
    })
}

/// The branches `observations` came from, deduplicated and joined.
fn branches_of(observations: &[Observation]) -> String {
    let mut branches: Vec<&str> = observations.iter().map(|o| o.branch.as_str()).collect();
    branches.sort_unstable();
    branches.dedup();
    match branches.is_empty() {
        true => "no branch".to_owned(),
        false => branches.join(", "),
    }
}
