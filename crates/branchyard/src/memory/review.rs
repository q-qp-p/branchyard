//! Review: what a person does with a proposed memory row
//! (`docs/memory.md`, section 5). Approve writes the memory into its
//! scope's bundle through the [`Library`] of M1 (the team's
//! `.agents/skills/`, or the person's skills folder) and marks the row;
//! reject keeps the row so the rejection sticks against new evidence; edit
//! rewrites the row (and the file, once published); promote offers a
//! person's memory to the team as a new proposal; forget deletes the row
//! and the file and records it, without the text, on the branches whose
//! observations it named.
//!
//! A `Person` memory is decided only by its subject. Who may approve a
//! `Repo` memory is the caller's rule: locally whoever runs `by memory`,
//! on a server an `admin` or one of `[memory] approvers`
//! (`branchyard-server`'s memory routes).

use std::path::Path;

use super::observe::Observation;
use super::store::{
    Decision, Forgotten, MemoryFilter, MemoryRecord, MemoryStatus, NewMemoryRecord, Published,
    Scope,
};
use super::{Library, MemoryActivity, MemoryEdit, MemoryKind, NewMemory, Written};
use crate::{Activity, Error, TaskKind, Yard};
use branchyard_support::time::now_ms;

/// A change to a memory row; `None` keeps the field.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryRecordEdit {
    /// A new title.
    pub title: Option<String>,
    /// A new body.
    pub body: Option<String>,
    /// A new kind.
    pub kind: Option<MemoryKind>,
    /// An empty list clears the paths.
    pub paths: Option<Vec<String>>,
    /// The task kinds; an empty list clears them.
    pub task_kinds: Option<Vec<TaskKind>>,
}

/// What an approval wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Approved {
    /// The row, now approved and published.
    pub record: MemoryRecord,
    /// The bundle files written.
    pub written: Written,
}

pub(crate) fn list(yard: &Yard, filter: &MemoryFilter) -> Result<Vec<MemoryRecord>, Error> {
    yard.store().memory().memories(filter)
}

pub(crate) fn get(yard: &Yard, id: u64) -> Result<MemoryRecord, Error> {
    yard.store()
        .memory()
        .memory(id)?
        .ok_or_else(|| Error::UnknownMemory(format!("#{id}")))
}

/// The observations a row's provenance names, in id order.
pub(crate) fn evidence(yard: &Yard, id: u64) -> Result<Vec<Observation>, Error> {
    let record = get(yard, id)?;
    yard.store().memory().observations(&record.provenance)
}

/// A `Person` memory is its subject's alone.
fn check_decider(record: &MemoryRecord, by: &str) -> Result<(), Error> {
    if let Scope::Person { subject, .. } = &record.scope {
        if subject != by {
            return Err(Error::Denied(format!(
                "memory #{} is {subject}'s; only they decide it",
                record.id
            )));
        }
    }
    Ok(())
}

/// The library a scope's approved memories are written to.
fn library_for(yard: &Yard, scope: &Scope, person_dir: Option<&Path>) -> Result<Library, Error> {
    match scope {
        Scope::Repo { .. } => Ok(Library::team(yard.root())),
        Scope::Person { .. } => person_dir.map(Library::person).ok_or_else(|| {
            Error::Unsupported(
                "a person's memory is written to their skills folder, which this call did not \
                 name"
                    .into(),
            )
        }),
    }
}

/// `<branch> turn <n> (<kind>)` for each observation, for the footer.
fn evidence_lines(observations: &[Observation]) -> Vec<String> {
    observations
        .iter()
        .map(|o| format!("{} turn {} ({})", o.branch, o.turn, o.kind.as_str()))
        .collect()
}

/// The slug of a published memory, from its reference path.
fn published_slug(published: &Published) -> Option<String> {
    published
        .path
        .strip_prefix("references/")?
        .strip_suffix(".md")
        .map(str::to_owned)
}

/// Approve `id` as `by`: the memory is written into its scope's bundle and
/// the row marked approved. A `Person` memory needs `person_dir`, the
/// subject's skills folder.
pub(crate) fn approve(
    yard: &Yard,
    id: u64,
    by: &str,
    person_dir: Option<&Path>,
) -> Result<Approved, Error> {
    let record = get(yard, id)?;
    if record.status != MemoryStatus::Proposed {
        return Err(Error::Denied(format!(
            "memory #{id} is {}, not proposed; only a proposed memory is approved",
            record.status
        )));
    }
    check_decider(&record, by)?;
    let library = library_for(yard, &record.scope, person_dir)?;
    let observations = yard.store().memory().observations(&record.provenance)?;
    let now = now_ms();
    let written = library.add(NewMemory {
        title: record.title.clone(),
        body: record.body.clone(),
        kind: record.kind,
        paths: record.paths.clone(),
        task_kinds: record.task_kinds.clone(),
        topic: None,
        by: by.to_owned(),
        now_ms: now,
        evidence: evidence_lines(&observations),
    })?;
    let skill = format!("{}{}", super::PREFIX, written.memory.topic);
    let digest = library
        .export(None)?
        .into_iter()
        .find(|b| {
            b.files
                .iter()
                .any(|(p, _)| p == &format!("references/{}.md", written.memory.id))
        })
        .map(|b| b.digest)
        .unwrap_or_default();
    let published = Published {
        skill,
        digest,
        path: format!("references/{}.md", written.memory.id),
    };
    let decided = yard.store().memory().decide(
        id,
        MemoryStatus::Proposed,
        &Decision {
            status: MemoryStatus::Approved,
            by: by.to_owned(),
            now_ms: now,
            note: None,
            published: Some(published),
        },
    )?;
    if !decided {
        branchyard_support::best_effort(
            "take back the bundle file of a memory decided meanwhile",
            library.remove(&written.memory.id),
        );
        return Err(Error::State(format!(
            "memory #{id} was decided by someone else meanwhile; read it again"
        )));
    }
    Ok(Approved {
        record: get(yard, id)?,
        written,
    })
}

/// Reject `id` as `by`, with `reason`: the row stays, so new evidence only
/// records itself.
pub(crate) fn reject(
    yard: &Yard,
    id: u64,
    by: &str,
    reason: Option<&str>,
) -> Result<MemoryRecord, Error> {
    let record = get(yard, id)?;
    check_decider(&record, by)?;
    if record.status == MemoryStatus::Approved {
        return Err(Error::Denied(format!(
            "memory #{id} is approved and in its bundle; by memory forget removes it"
        )));
    }
    let decided = yard.store().memory().decide(
        id,
        record.status,
        &Decision {
            status: MemoryStatus::Rejected,
            by: by.to_owned(),
            now_ms: now_ms(),
            note: reason
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .map(str::to_owned),
            published: None,
        },
    )?;
    if !decided {
        return Err(Error::State(format!(
            "memory #{id} was decided by someone else meanwhile; read it again"
        )));
    }
    get(yard, id)
}

/// Change `id` as `edit` says, keeping its status; an approved memory's
/// file is rewritten too (needs `person_dir` for a `Person` memory).
pub(crate) fn edit(
    yard: &Yard,
    id: u64,
    edit: &MemoryRecordEdit,
    by: &str,
    person_dir: Option<&Path>,
) -> Result<MemoryRecord, Error> {
    let current = get(yard, id)?;
    check_decider(&current, by)?;
    let mut record = current.clone();
    if let Some(title) = &edit.title {
        record.title = title.trim().to_owned();
    }
    if let Some(body) = &edit.body {
        record.body = body.trim().to_owned();
    }
    if let Some(kind) = edit.kind {
        record.kind = kind;
    }
    if let Some(paths) = &edit.paths {
        record.paths = paths.clone();
    }
    if let Some(task_kinds) = &edit.task_kinds {
        record.task_kinds = task_kinds.clone();
    }
    check_record(&record)?;
    if let Some(published) = &current.published {
        if let Some(slug) = published_slug(published) {
            let library = library_for(yard, &record.scope, person_dir)?;
            let written = library.edit(
                &slug,
                MemoryEdit {
                    title: edit.title.clone(),
                    body: edit.body.clone(),
                    kind: edit.kind,
                    paths: edit.paths.clone(),
                    task_kinds: edit.task_kinds.clone(),
                    by: by.to_owned(),
                    now_ms: now_ms(),
                },
            )?;
            record.published = Some(Published {
                path: format!("references/{}.md", written.memory.id),
                ..published.clone()
            });
            record.decided_by = Some(by.to_owned());
            record.decided_ms = Some(now_ms());
        }
    }
    match yard.store().memory().put_memory(&record, current.status)? {
        true => get(yard, id),
        false => Err(Error::State(format!(
            "memory #{id} changed while it was being edited; try again"
        ))),
    }
}

/// What every row must satisfy.
fn check_record(record: &MemoryRecord) -> Result<(), Error> {
    if record.title.trim().is_empty() || record.title.chars().count() > super::TITLE_MAX {
        return Err(Error::InvalidMemory(format!(
            "a title is one line of 1 to {} characters",
            super::TITLE_MAX
        )));
    }
    if record.body.trim().is_empty() || record.body.chars().count() > super::BODY_MAX {
        return Err(Error::InvalidMemory(format!(
            "a body is 1 to {} characters",
            super::BODY_MAX
        )));
    }
    Ok(())
}

/// Offer `id`, a person's memory, to the team: a new proposed row in
/// `repo`, the `Repo` scope, with the same content and provenance; the
/// original stays as it was.
pub(crate) fn promote(yard: &Yard, id: u64, by: &str, repo: Scope) -> Result<MemoryRecord, Error> {
    let record = get(yard, id)?;
    let Scope::Person { .. } = &record.scope else {
        return Err(Error::Denied(format!(
            "memory #{id} is already the team's; promote offers a person's memory to the team"
        )));
    };
    check_decider(&record, by)?;
    if !matches!(repo, Scope::Repo { .. }) {
        return Err(Error::Unsupported(
            "a memory is promoted to a repository scope".into(),
        ));
    }
    let new_id = yard.store().memory().propose(&NewMemoryRecord {
        scope: repo,
        kind: record.kind,
        title: record.title.clone(),
        body: record.body.clone(),
        paths: record.paths.clone(),
        task_kinds: record.task_kinds.clone(),
        provenance: record.provenance.clone(),
        note: Some(format!("promoted from #{id} by {by}")),
        now_ms: now_ms(),
    })?;
    get(yard, new_id)
}

/// Delete `id` and its file, as `by`, recording it without the text on the
/// branches whose observations it named.
pub(crate) fn forget(
    yard: &Yard,
    id: u64,
    by: &str,
    person_dir: Option<&Path>,
) -> Result<Forgotten, Error> {
    let record = get(yard, id)?;
    check_decider(&record, by)?;
    if let Some(slug) = record.published.as_ref().and_then(published_slug) {
        let library = library_for(yard, &record.scope, person_dir)?;
        match library.remove(&slug) {
            Ok(_) | Err(Error::UnknownMemory(_)) => {}
            Err(error) => return Err(error),
        }
    }
    let store = yard.store();
    let observations = store.memory().observations(&record.provenance)?;
    let forgotten = store.memory().forget(id)?;
    let mut branches: Vec<String> = observations.iter().map(|o| o.branch.clone()).collect();
    branches.sort();
    branches.dedup();
    for branch in branches {
        if store.read(&branch).is_err() {
            continue;
        }
        if let Ok(mut recorder) = crate::record::Recorder::open(&store, &branch, None) {
            branchyard_support::best_effort(
                "record the forgotten memory on the branch",
                recorder.record(Activity::Memory(Box::new(MemoryActivity::Forgotten {
                    memory: id,
                    scope: record.scope.clone(),
                    by: by.to_owned(),
                }))),
            );
        }
    }
    Ok(forgotten)
}
