//! Where observations, memory rows, watermarks and consolidation runs live:
//! one trait, `MemoryBackend`, on SQLite and PostgreSQL with one
//! conformance case each (`docs/memory.md`, section 3). Every row carries
//! a tenant: a served repository can be on several tenants' allowlists, and
//! a scope without one would make tenant A's corrections tenant B's memory.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::observe::{NewObservation, Observation};
use super::MemoryKind;
use crate::{Error, KnowledgeEntry, TaskKind};

/// The owner, and the audience, of a memory.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum Scope {
    /// The team memory of one repository, on one tenant.
    Repo {
        /// The tenant whose sessions fed it.
        tenant: String,
        /// The repository, as its store labels it.
        repo: String,
    },
    /// One person's memory: read only by that person's turns and `by
    /// memory` run as them.
    Person {
        /// The tenant the person belongs to.
        tenant: String,
        /// The person.
        subject: String,
        /// A repository the memory is about; `None` for all of them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
    },
}

impl Scope {
    /// The scope as the `memory_watermarks` and `memory_runs` tables key
    /// it: `repo:<tenant>:<repo>` or `person:<tenant>:<subject>[:<repo>]`.
    /// A component holding `:` or `%` is written with `%3A` and `%25`, so
    /// two scopes never share a key (a tenant `a:b` with subject `c` and a
    /// tenant `a` with subject `b:c` once did).
    pub fn key(&self) -> String {
        match self {
            Scope::Repo { tenant, repo } => {
                format!("repo:{}:{}", escape(tenant), escape(repo))
            }
            Scope::Person {
                tenant,
                subject,
                repo,
            } => match repo {
                Some(repo) => format!(
                    "person:{}:{}:{}",
                    escape(tenant),
                    escape(subject),
                    escape(repo)
                ),
                None => format!("person:{}:{}", escape(tenant), escape(subject)),
            },
        }
    }

    /// The scope a key names; see [`Scope::key`].
    pub fn parse_key(key: &str) -> Result<Scope, Error> {
        let bad = || Error::State(format!("{key:?} is not a memory scope"));
        let (kind, rest) = key.split_once(':').ok_or_else(bad)?;
        match kind {
            "repo" => {
                let (tenant, repo) = rest.split_once(':').ok_or_else(bad)?;
                if repo.contains(':') {
                    return Err(bad());
                }
                Ok(Scope::Repo {
                    tenant: unescape(tenant).ok_or_else(bad)?,
                    repo: unescape(repo).ok_or_else(bad)?,
                })
            }
            "person" => {
                let mut parts = rest.split(':');
                let tenant = parts.next().ok_or_else(bad)?;
                let subject = parts.next().ok_or_else(bad)?;
                let repo = parts.next();
                if parts.next().is_some() {
                    return Err(bad());
                }
                Ok(Scope::Person {
                    tenant: unescape(tenant).ok_or_else(bad)?,
                    subject: unescape(subject).ok_or_else(bad)?,
                    repo: repo
                        .map(|repo| unescape(repo).ok_or_else(bad))
                        .transpose()?,
                })
            }
            _ => Err(bad()),
        }
    }

    /// The tenant the scope belongs to.
    pub fn tenant(&self) -> &str {
        match self {
            Scope::Repo { tenant, .. } | Scope::Person { tenant, .. } => tenant,
        }
    }

    /// The person a `Person` scope belongs to.
    pub fn subject(&self) -> Option<&str> {
        match self {
            Scope::Repo { .. } => None,
            Scope::Person { subject, .. } => Some(subject),
        }
    }

    /// `repo` or `person`.
    pub fn kind(&self) -> &'static str {
        match self {
            Scope::Repo { .. } => "repo",
            Scope::Person { .. } => "person",
        }
    }

    /// One line for people: `the team's memory of app` or `ana's memory`.
    pub fn describe(&self) -> String {
        match self {
            Scope::Repo { repo, .. } => format!("the team's memory of {repo}"),
            Scope::Person { subject, .. } => format!("{subject}'s memory"),
        }
    }
}

/// `component` with `%` and `:` escaped, for a scope key.
fn escape(component: &str) -> String {
    component.replace('%', "%25").replace(':', "%3A")
}

/// [`escape`] undone; `None` for a component with a stray `%`.
fn unescape(component: &str) -> Option<String> {
    let mut out = String::with_capacity(component.len());
    let mut rest = component;
    while let Some(at) = rest.find('%') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        if let Some(after) = tail.strip_prefix("%25") {
            out.push('%');
            rest = after;
        } else if let Some(after) = tail.strip_prefix("%3A") {
            out.push(':');
            rest = after;
        } else {
            return None;
        }
    }
    out.push_str(rest);
    Some(out)
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.key())
    }
}

/// Where a memory row stands.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    /// Waiting for a person; never published, never given to a turn.
    Proposed,
    /// A person approved it: it is in its scope's bundle.
    Approved,
    /// A person rejected it; new evidence only records itself.
    Rejected,
    /// Deleted; kept as a status for the conformance case, never stored.
    Forgotten,
}

impl MemoryStatus {
    /// Every status a row can hold.
    pub const ALL: [MemoryStatus; 4] = [
        MemoryStatus::Proposed,
        MemoryStatus::Approved,
        MemoryStatus::Rejected,
        MemoryStatus::Forgotten,
    ];

    /// The status as serialized: `proposed`, `approved`, ...
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryStatus::Proposed => "proposed",
            MemoryStatus::Approved => "approved",
            MemoryStatus::Rejected => "rejected",
            MemoryStatus::Forgotten => "forgotten",
        }
    }
}

impl fmt::Display for MemoryStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for MemoryStatus {
    type Err = Error;
    fn from_str(text: &str) -> Result<MemoryStatus, Error> {
        MemoryStatus::ALL
            .into_iter()
            .find(|s| s.as_str() == text)
            .ok_or_else(|| {
                Error::Unsupported(format!(
                    "{text:?} is not a memory status; use proposed, approved or rejected"
                ))
            })
    }
}

/// Where an approved memory was written.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Published {
    /// The bundle, `memory-<topic>`.
    pub skill: String,
    /// The bundle's digest when it was written.
    pub digest: String,
    /// The reference's path in the bundle.
    pub path: String,
}

/// A memory as a row: proposed by a consolidation or a person, decided by
/// a person, published as a file of a bundle (`docs/memory.md`, section
/// 3). The file form is [`super::Memory`].
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRecord {
    /// Assigned by the store, from 1.
    pub id: u64,
    /// The owner and audience.
    pub scope: Scope,
    /// Rule, procedure, fact or pitfall.
    pub kind: MemoryKind,
    /// One line, at most [`super::TITLE_MAX`] characters.
    pub title: String,
    /// Markdown, at most [`super::BODY_MAX`] characters.
    pub body: String,
    /// Globs it applies to; empty for the whole repository.
    #[serde(default)]
    pub paths: Vec<String>,
    /// The task kinds it applies to; empty for every kind.
    #[serde(default)]
    pub task_kinds: Vec<TaskKind>,
    /// Where the row stands.
    pub status: MemoryStatus,
    /// Observation ids; empty for one a person wrote.
    #[serde(default)]
    pub provenance: Vec<u64>,
    /// How many consolidations found evidence for it.
    pub seen: u32,
    /// When it was first proposed, in milliseconds since the Unix epoch.
    pub first_seen_ms: u64,
    /// When evidence was last found for it.
    pub last_seen_ms: u64,
    /// Who decided it, once decided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<String>,
    /// When it was decided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_ms: Option<u64>,
    /// Why it was proposed (`deterministic`), or why it was rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Where it is, once approved and written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<Published>,
}

/// A row before the store assigns its id: proposed.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewMemoryRecord {
    /// The owner and audience.
    pub scope: Scope,
    /// Rule, procedure, fact or pitfall.
    pub kind: MemoryKind,
    /// One line, at most [`super::TITLE_MAX`] characters.
    pub title: String,
    /// Markdown, at most [`super::BODY_MAX`] characters.
    pub body: String,
    /// Globs it applies to; empty for the whole repository.
    #[serde(default)]
    pub paths: Vec<String>,
    /// The task kinds it applies to; empty for every kind.
    #[serde(default)]
    pub task_kinds: Vec<TaskKind>,
    /// Observation ids; empty for one a person wrote.
    #[serde(default)]
    pub provenance: Vec<u64>,
    /// Why it was proposed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// `first_seen_ms` and `last_seen_ms`.
    pub now_ms: u64,
}

/// What `MemoryBackend::memories` returns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryFilter {
    /// Only this scope; `None` for every scope of `tenant`.
    pub scope: Option<Scope>,
    /// Every scope of this tenant, when `scope` is `None`; `None` for all.
    pub tenant: Option<String>,
    /// Only rows of this status; `None` for every status.
    pub status: Option<MemoryStatus>,
}

/// A decision on a row: compare-and-swap from its current status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    /// The status the row moves to.
    pub status: MemoryStatus,
    /// Who decided.
    pub by: String,
    /// When, in milliseconds since the Unix epoch.
    pub now_ms: u64,
    /// A new note (a rejection's reason); `None` keeps the row's.
    pub note: Option<String>,
    /// Where it was written, for an approval; `None` clears it.
    pub published: Option<Published>,
}

/// What `MemoryBackend::forget` deleted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Forgotten {
    /// The row as it was.
    pub memory: MemoryRecord,
}

/// A consolidation run in progress: a row of `memory_runs` with a fence
/// (`fenced_until_ms`) renewed while it works. The fence bounds how long a
/// run may hold its scope: a renewal after it ran out fails, and the next
/// run of the scope closes the dead run's row. Correctness does not rest
/// on it; the watermark's compare-and-swap does.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    /// The `memory_runs` row.
    pub id: u64,
    /// The scope the run consolidates.
    pub scope: Scope,
    /// The scope's watermark when the run started: the observation id it
    /// reads from.
    pub watermark_read: u64,
    /// When it started, in milliseconds since the Unix epoch.
    pub started_ms: u64,
    /// Until when the run holds its scope; renewed by `MemoryBackend::run_renew`.
    pub fenced_until_ms: u64,
    /// Who started it: a person, or the dispatcher.
    pub by: String,
    /// `asked`, `schedule`, or a trigger's name.
    pub trigger: String,
}

/// How a run went.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCounts {
    /// Observations read.
    pub observations: u32,
    /// Clusters they formed.
    pub clusters: u32,
    /// Memory rows proposed.
    pub proposed: u32,
    /// Clusters recorded as evidence for existing rows.
    pub merged: u32,
    /// Knowledge entries proposed.
    pub knowledge: u32,
    /// Observations deleted as expired.
    pub expired: u32,
}

/// What a finished run writes, in one transaction with its watermark's
/// compare-and-swap: the run whose swap changes no row writes none of it
/// and records `lost_race`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunOutcome {
    /// The new watermark: the last observation id the run consumed.
    pub watermark_written: u64,
    /// The rows to propose.
    pub proposals: Vec<NewMemoryRecord>,
    /// Evidence for existing rows: `(memory id, observation ids)`.
    pub evidence: Vec<(u64, Vec<u64>)>,
    /// Short repository-wide candidates, proposed to the knowledge store.
    pub knowledge: Vec<KnowledgeEntry>,
    /// Delete observations recorded before this time, unless a memory's
    /// provenance names them.
    pub expire_before_ms: Option<u64>,
    /// What the run did, for its row.
    pub counts: RunCounts,
    /// When the run finished, in milliseconds since the Unix epoch.
    pub now_ms: u64,
}

/// A run as `by memory runs` lists it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRow {
    /// The `memory_runs` row.
    pub id: u64,
    /// The scope the run consolidated.
    pub scope: Scope,
    /// The watermark it read from.
    pub watermark_read: u64,
    /// The watermark it wrote, once finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watermark_written: Option<u64>,
    /// When it started, in milliseconds since the Unix epoch.
    pub started_ms: u64,
    /// When it finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_ms: Option<u64>,
    /// `proposed`, `nothing_new`, `lost_race`, or `None` while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// Who started it.
    pub by: String,
    /// `asked`, `schedule`, or a trigger's name.
    pub trigger: String,
    /// What it did.
    #[serde(default)]
    pub counts: RunCounts,
    /// The ids the run proposed.
    #[serde(default)]
    pub proposed: Vec<u64>,
}

/// The outcome a run records when another run moved the watermark first.
pub const LOST_RACE: &str = "lost_race";

/// The outcome of a run that never finished and whose fence ran out: a
/// later run of the scope closed its row.
pub const ABANDONED: &str = "abandoned";

/// The outcome of a run that proposed or merged something.
pub const PROPOSED: &str = "proposed";

/// The outcome of a run that found nothing new.
pub const NOTHING_NEW: &str = "nothing_new";

// ---------------------------------------------------------------------------
// What both stores share: rows are kept as JSON bodies beside the columns
// they are filtered by, and these turn one into the other.

/// A proposed row from what a consolidation or a person gives.
pub(crate) fn proposed_record(new: &NewMemoryRecord) -> MemoryRecord {
    MemoryRecord {
        id: 0,
        scope: new.scope.clone(),
        kind: new.kind,
        title: new.title.clone(),
        body: new.body.clone(),
        paths: new.paths.clone(),
        task_kinds: new.task_kinds.clone(),
        status: MemoryStatus::Proposed,
        provenance: new.provenance.clone(),
        seen: 1,
        first_seen_ms: new.now_ms,
        last_seen_ms: new.now_ms,
        decided_by: None,
        decided_ms: None,
        note: new.note.clone(),
        published: None,
    }
}

/// `record` after `decision`.
pub(crate) fn decided(record: &MemoryRecord, decision: &Decision) -> MemoryRecord {
    MemoryRecord {
        status: decision.status,
        decided_by: Some(decision.by.clone()),
        decided_ms: Some(decision.now_ms),
        note: decision.note.clone().or_else(|| record.note.clone()),
        published: decision.published.clone(),
        ..record.clone()
    }
}

/// `record` with `observations` as new evidence, seen once more now.
pub(crate) fn with_evidence(
    record: &MemoryRecord,
    observations: &[u64],
    now_ms: u64,
) -> MemoryRecord {
    let mut provenance = record.provenance.clone();
    provenance.extend(observations.iter().copied());
    provenance.sort_unstable();
    provenance.dedup();
    MemoryRecord {
        provenance,
        seen: record.seen.saturating_add(1),
        last_seen_ms: record.last_seen_ms.max(now_ms),
        ..record.clone()
    }
}

/// `into` with `from` folded in: provenance union, `seen` summed, the
/// earlier first and the later last seen.
pub(crate) fn merged(into: &MemoryRecord, from: &MemoryRecord) -> MemoryRecord {
    let mut provenance = into.provenance.clone();
    provenance.extend(from.provenance.iter().copied());
    provenance.sort_unstable();
    provenance.dedup();
    MemoryRecord {
        provenance,
        seen: into.seen.saturating_add(from.seen),
        first_seen_ms: into.first_seen_ms.min(from.first_seen_ms),
        last_seen_ms: into.last_seen_ms.max(from.last_seen_ms),
        ..into.clone()
    }
}

/// The outcome label a finished run records.
pub(crate) fn outcome_label(outcome: &RunOutcome) -> &'static str {
    match outcome.proposals.is_empty()
        && outcome.evidence.is_empty()
        && outcome.knowledge.is_empty()
    {
        true => NOTHING_NEW,
        false => PROPOSED,
    }
}

/// The `(tenant, subject)` filter of a scope's observations: a `Repo`
/// scope reads shared observations (no subject), a `Person` scope the
/// subject's.
pub(crate) fn scope_filter(scope: &Scope) -> (&str, Option<&str>, Option<&str>) {
    match scope {
        Scope::Repo { tenant, repo } => (tenant, None, Some(repo)),
        Scope::Person {
            tenant,
            subject,
            repo,
        } => (tenant, Some(subject), repo.as_deref()),
    }
}

/// Observations, memory rows, watermarks and runs, per repository store.
pub(crate) trait MemoryBackend: Send + Sync + fmt::Debug {
    /// Store each observation once, by its key; the ids, in order (an
    /// observation already stored gives its existing id).
    fn observe(&self, obs: &[NewObservation]) -> Result<Vec<u64>, Error>;
    /// The observations of `scope` with an id above `watermark`, oldest
    /// first, at most `limit`.
    fn observations_since(
        &self,
        scope: &Scope,
        watermark: u64,
        limit: usize,
    ) -> Result<Vec<Observation>, Error>;
    /// The observations with these ids, in id order; missing ones skipped.
    fn observations(&self, ids: &[u64]) -> Result<Vec<Observation>, Error>;
    fn memories(&self, filter: &MemoryFilter) -> Result<Vec<MemoryRecord>, Error>;
    fn memory(&self, id: u64) -> Result<Option<MemoryRecord>, Error>;
    /// Store a proposed row; its id.
    fn propose(&self, m: &NewMemoryRecord) -> Result<u64, Error>;
    /// Move the row from `from` to `to.status`, if it is still `from`.
    fn decide(&self, id: u64, from: MemoryStatus, to: &Decision) -> Result<bool, Error>;
    /// Replace the row's fields (title, body, kind, paths, task kinds, note,
    /// published), if its status is still `expected`.
    fn put_memory(&self, m: &MemoryRecord, expected: MemoryStatus) -> Result<bool, Error>;
    /// Fold `from` into `into`: provenance union, `seen` summed, the earlier
    /// first and the later last seen; `from` is deleted.
    fn merge(&self, into: u64, from: u64) -> Result<(), Error>;
    /// Hard delete.
    fn forget(&self, id: u64) -> Result<Forgotten, Error>;
    /// The scope's watermark: the last observation id consolidation
    /// consumed, 0 before any.
    fn watermark(&self, scope: &Scope) -> Result<u64, Error>;
    /// Record a run starting at `watermark`, fenced until `fenced_until_ms`.
    /// An unfinished run of the scope whose fence ran out before `now_ms`
    /// died: its row is closed as [`ABANDONED`]. A run whose fence has not
    /// run out is left alone: two runs of a scope may overlap, and the
    /// watermark's compare-and-swap lets only one write.
    fn run_started(
        &self,
        scope: &Scope,
        watermark: u64,
        by: &str,
        trigger: &str,
        now_ms: u64,
        fenced_until_ms: u64,
    ) -> Result<Run, Error>;
    /// Move the run's fence to `fenced_until_ms`; false when the run is
    /// finished, abandoned or gone, or its fence ran out before `now_ms`
    /// (a run that held its scope past its hold may have been taken over).
    fn run_renew(&self, run: &Run, now_ms: u64, fenced_until_ms: u64) -> Result<bool, Error>;
    /// Finish the run: in one transaction, swap the scope's watermark from
    /// `run.watermark_read` to `outcome.watermark_written`, write the
    /// proposals, the evidence, the knowledge entries and the expiry, and
    /// finish the run row. When the swap changes no row (another run moved
    /// the watermark), nothing else is written, the row records
    /// [`LOST_RACE`], and `Ok(None)`. Otherwise the ids proposed.
    fn run_finished(&self, run: &Run, outcome: &RunOutcome) -> Result<Option<Vec<u64>>, Error>;
    /// Runs, newest first, at most `limit`; of `scope` when given.
    fn runs(&self, scope: Option<&Scope>, limit: usize) -> Result<Vec<RunRow>, Error>;
    /// Every scope with observations above its watermark: the repository's
    /// scope of each tenant seen, and each person's.
    fn scopes_with_observations(&self) -> Result<Vec<Scope>, Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_keys_round_trip() {
        let repo = Scope::Repo {
            tenant: "acme".into(),
            repo: "app".into(),
        };
        assert_eq!(repo.key(), "repo:acme:app");
        assert_eq!(Scope::parse_key("repo:acme:app").unwrap(), repo);
        let person = Scope::Person {
            tenant: "local".into(),
            subject: "ana".into(),
            repo: Some("app".into()),
        };
        assert_eq!(person.key(), "person:local:ana:app");
        assert_eq!(Scope::parse_key(&person.key()).unwrap(), person);
        let bare = Scope::Person {
            tenant: "local".into(),
            subject: "ana".into(),
            repo: None,
        };
        assert_eq!(Scope::parse_key("person:local:ana").unwrap(), bare);
        // A component with a colon or a percent sign keeps its scope's key
        // its own: these two once shared `person:a:b:c`.
        let first = Scope::Person {
            tenant: "a:b".into(),
            subject: "c".into(),
            repo: None,
        };
        let second = Scope::Person {
            tenant: "a".into(),
            subject: "b".into(),
            repo: Some("c".into()),
        };
        assert_ne!(first.key(), second.key());
        let odd = Scope::Person {
            tenant: "a:b%c".into(),
            subject: "s%3A".into(),
            repo: Some("r:1".into()),
        };
        assert_eq!(odd.key(), "person:a%3Ab%25c:s%253A:r%3A1");
        for scope in [&first, &second, &odd] {
            assert_eq!(&Scope::parse_key(&scope.key()).unwrap(), scope);
        }
        assert!(Scope::parse_key("repo:a:b:c").is_err());
        assert!(Scope::parse_key("repo:a%:b").is_err());
        assert!(Scope::parse_key("team:x").is_err());
        assert!(Scope::parse_key("repo:acme").is_err());
        assert_eq!(repo.describe(), "the team's memory of app");
        assert_eq!(bare.describe(), "ana's memory");
        assert_eq!(
            "approved".parse::<MemoryStatus>().unwrap(),
            MemoryStatus::Approved
        );
        assert!("adopted".parse::<MemoryStatus>().is_err());
    }
}
