//! Consolidation, deterministic (`docs/memory.md`, section 4): an internal
//! job, no worktree and no harness turn, that reads a scope's observations
//! since its watermark, clusters them, matches clusters to existing
//! memories as evidence, proposes the rest verbatim, and finishes in one
//! transaction with a compare-and-swap on the watermark, so two
//! consolidations of one scope can run at once and neither duplicates a
//! row: the one whose swap changes nothing records `lost_race`.
//!
//! Clustering normalizes text (case, whitespace, numbers to `#`), groups by
//! `(kind, normalized text)`, then merges groups whose token sets (text and
//! paths) overlap enough ([`SIMILAR`]). `branchyard::rank`, the ranker the
//! registry's R3 ports from `tools.py`, does not exist in this tree yet;
//! this is a token-overlap stand-in with the same contract (a score over
//! text and paths, a threshold), to be replaced when it lands.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::observe::{Observation, ObservationKind};
use super::store::{
    MemoryFilter, MemoryRecord, NewMemoryRecord, RunCounts, RunOutcome, RunRow, Scope, LOST_RACE,
};
use super::{MemoryKind, BODY_MAX, TITLE_MAX};
use crate::knowledge::{KnowledgeEntry, KnowledgeScope, KnowledgeSource, KnowledgeStatus};
use crate::{Activity, Error, TaskKind, Yard};
use branchyard_support::time::now_ms;

/// The most observations one run reads per scope; older ones wait for the
/// next run, so a backlog is worked off in order.
pub const GATHER_MAX: usize = 500;

/// How long a run holds its scope before another dispatcher may assume it
/// died: the trigger dispatcher's hold.
pub const HOLD_MS: u64 = 11 * 60 * 1_000;

/// The least token overlap (Jaccard) that puts two groups in one cluster,
/// or makes a cluster evidence for an existing memory.
pub const SIMILAR: f64 = 0.5;

/// Observations of a cluster become one candidate from this many.
pub const CLUSTER_MIN: usize = 2;

/// Memory activity on a branch, recorded as [`Activity::Memory`].
/// Serialized as an object tagged by `type`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MemoryActivity {
    /// A consolidation consumed this branch's observations.
    Consolidated {
        /// The `memory_runs` row.
        run: u64,
        /// The scope the run consolidated.
        scope: Scope,
        /// How many of this branch's observations the run read.
        observations: u32,
        /// Clusters the run's observations formed, across the whole run.
        clusters: u32,
        /// The memory rows proposed, across the whole run.
        proposed: Vec<u64>,
        /// Clusters that became evidence for existing rows.
        merged: u32,
        /// Who started the run.
        by: String,
        /// `asked`, `schedule`, or a trigger's name.
        trigger: String,
    },
    /// A memory whose provenance named this branch's observations was
    /// forgotten; no text is kept.
    Forgotten {
        /// The row's id, as it was.
        memory: u64,
        /// The scope it belonged to.
        scope: Scope,
        /// Who forgot it.
        by: String,
    },
}

impl MemoryActivity {
    /// One line for logs.
    pub fn describe(&self) -> String {
        match self {
            MemoryActivity::Consolidated {
                run,
                scope,
                observations,
                clusters,
                proposed,
                merged,
                by,
                trigger,
            } => {
                let proposed = match proposed.is_empty() {
                    true => "nothing new proposed".to_owned(),
                    false => format!(
                        "proposed memory {}",
                        proposed
                            .iter()
                            .map(|id| format!("#{id}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                };
                format!(
                    "consolidated run {run} ({scope}, {trigger}, by {by}): {observations} \
                     observation(s) in {clusters} cluster(s), {proposed}, {merged} as evidence"
                )
            }
            MemoryActivity::Forgotten { memory, scope, by } => {
                format!("memory #{memory} ({scope}) forgotten by {by}")
            }
        }
    }
}

/// What one consolidation did.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Consolidated {
    /// The run's row, finished.
    pub run: RunRow,
    /// The rows proposed, as stored.
    pub proposed: Vec<MemoryRecord>,
    /// Knowledge entry ids proposed.
    #[serde(default)]
    pub knowledge: Vec<u64>,
    /// Another run moved the watermark first: nothing was written.
    pub lost_race: bool,
    /// The branches whose observations the run consumed.
    #[serde(default)]
    pub branches: Vec<String>,
}

/// `text` as a comparison key: lowercase, whitespace collapsed, every
/// number `#`, the `[via]` tag the derivation prefixed dropped.
pub fn normalize(text: &str) -> String {
    let text = match text.trim_start().strip_prefix('[') {
        Some(rest) => rest.split_once("] ").map_or(text, |(_, after)| after),
        None => text,
    };
    let mut out = String::with_capacity(text.len());
    let mut in_number = false;
    let mut pending_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            in_number = false;
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        if c.is_ascii_digit() {
            if !in_number {
                out.push('#');
                in_number = true;
            }
            continue;
        }
        in_number = false;
        out.extend(c.to_lowercase());
    }
    out
}

/// The tokens similarity is judged on: the normalized words of three
/// letters or more, and each path.
fn tokens(text: &str, paths: &[String]) -> BTreeSet<String> {
    let mut set: BTreeSet<String> = normalize(text)
        .split(|c: char| !c.is_alphanumeric() && c != '#')
        .filter(|w| w.len() >= 3)
        .map(str::to_owned)
        .collect();
    set.extend(paths.iter().map(|p| format!("path:{p}")));
    set
}

/// Jaccard overlap of two token sets; 0 when either is empty.
pub fn similarity(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let shared = a.intersection(b).count() as f64;
    shared / a.union(b).count() as f64
}

/// A cluster of observations of one kind.
#[derive(Clone, Debug, PartialEq)]
pub struct Cluster {
    /// The kind every observation of the cluster has.
    pub kind: ObservationKind,
    /// Observation ids, in id order.
    pub ids: Vec<u64>,
    /// The texts, one per observation, in id order.
    pub texts: Vec<(String, u32, String)>,
    /// The paths the observations span, in first-seen order.
    pub paths: Vec<String>,
    /// The task kinds the observations span.
    pub task_kinds: Vec<TaskKind>,
    tokens: BTreeSet<String>,
}

impl Cluster {
    fn of(observation: &Observation) -> Cluster {
        let mut task_kinds = Vec::new();
        task_kinds.extend(observation.task_kind);
        Cluster {
            kind: observation.kind,
            ids: vec![observation.id],
            texts: vec![(
                observation.branch.clone(),
                observation.turn,
                observation.text.clone(),
            )],
            paths: observation.paths.clone(),
            task_kinds,
            tokens: tokens(&observation.text, &observation.paths),
        }
    }

    fn absorb(&mut self, other: Cluster) {
        self.ids.extend(other.ids);
        self.texts.extend(other.texts);
        for path in other.paths {
            if !self.paths.contains(&path) {
                self.paths.push(path);
            }
        }
        for kind in other.task_kinds {
            if !self.task_kinds.contains(&kind) {
                self.task_kinds.push(kind);
            }
        }
        self.tokens.extend(other.tokens);
        self.ids.sort_unstable();
        self.ids.dedup();
    }

    /// The first observation's text.
    pub fn first_text(&self) -> &str {
        self.texts.first().map_or("", |(_, _, text)| text.as_str())
    }
}

/// Cluster `observations`: group by `(kind, normalized text)`, then merge
/// groups of one kind whose tokens overlap by [`SIMILAR`] or more.
pub fn cluster(observations: &[Observation]) -> Vec<Cluster> {
    let mut groups: BTreeMap<(ObservationKind, String), Cluster> = BTreeMap::new();
    for observation in observations {
        let key = (observation.kind, normalize(&observation.text));
        match groups.get_mut(&key) {
            Some(cluster) => cluster.absorb(Cluster::of(observation)),
            None => {
                groups.insert(key, Cluster::of(observation));
            }
        }
    }
    let mut clusters: Vec<Cluster> = Vec::new();
    for group in groups.into_values() {
        let similar = clusters
            .iter()
            .position(|c| c.kind == group.kind && similarity(&c.tokens, &group.tokens) >= SIMILAR);
        match similar {
            Some(at) => clusters[at].absorb(group),
            None => clusters.push(group),
        }
    }
    clusters.sort_by_key(|c| c.ids.first().copied());
    clusters
}

/// The memory kind a cluster of `kind` proposes.
fn memory_kind(kind: ObservationKind) -> MemoryKind {
    match kind {
        ObservationKind::Correction => MemoryKind::Rule,
        ObservationKind::CheckFailed => MemoryKind::Pitfall,
        _ => MemoryKind::Procedure,
    }
}

/// The first [`TITLE_MAX`] characters of `text`, one line, cut at a word.
pub fn title_of(text: &str) -> String {
    let first = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("memory");
    let first = match first.strip_prefix('[') {
        Some(rest) => rest.split_once("] ").map_or(first, |(_, after)| after),
        None => first,
    };
    let clean: String = first
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let clean = clean.trim().trim_end_matches(['.', ':', ',', ';']);
    if clean.chars().count() <= TITLE_MAX {
        return clean.to_owned();
    }
    let cut: String = clean.chars().take(TITLE_MAX).collect();
    let cut = match cut.rfind(' ') {
        Some(at) if at > TITLE_MAX / 2 => cut[..at].to_owned(),
        _ => cut,
    };
    cut.trim_end_matches([':', '.', ',', ';']).to_owned()
}

/// The verbatim body of a candidate: each observation with its branch and
/// turn, within [`BODY_MAX`].
fn body_of(cluster: &Cluster) -> String {
    let mut body = String::new();
    for (kept, (branch, turn, text)) in cluster.texts.iter().enumerate() {
        let line = format!("- {branch} turn {turn}: {}\n", text.replace('\n', "\n  "));
        if body.chars().count() + line.chars().count() > BODY_MAX - 40 {
            // Count observations, not lines: an observation's text has
            // lines of its own.
            body.push_str(&format!(
                "- ...and {} more\n",
                cluster.texts.len().saturating_sub(kept)
            ));
            break;
        }
        body.push_str(&line);
    }
    body.trim_end().to_owned()
}

/// Whether `cluster` is evidence for `memory`: the same normalized text as
/// its title or body, or enough token overlap.
fn matches(cluster: &Cluster, memory: &MemoryRecord) -> bool {
    let first = normalize(cluster.first_text());
    if !first.is_empty()
        && (normalize(&memory.title) == first || normalize(&memory.body).contains(&first))
    {
        return true;
    }
    let text = format!("{}\n{}", memory.title, memory.body);
    similarity(&cluster.tokens, &tokens(&text, &memory.paths)) >= SIMILAR
}

/// What the deterministic pass makes of a scope's new observations:
/// evidence for existing rows, candidates, and the knowledge entries
/// among them. Pure: nothing is written.
pub struct Pass {
    /// How many clusters the observations formed.
    pub clusters: usize,
    /// Evidence for existing rows: `(memory id, observation ids)`.
    pub evidence: Vec<(u64, Vec<u64>)>,
    /// The rows to propose.
    pub proposals: Vec<NewMemoryRecord>,
    /// Short repository-wide candidates, for the knowledge store.
    pub knowledge: Vec<KnowledgeEntry>,
}

/// Whether a scope's short corrections may become knowledge entries.
///
/// A knowledge entry has no tenant and no owner: the knowledge store and
/// `/v1/repos/{repo}/knowledge` are per repository, and an adopted entry
/// is given to every turn on it. So only the team scope of a session
/// without a server ([`super::observe::LOCAL_TENANT`]) may feed it. On a
/// server a repository can be on several tenants' allowlists, and a
/// person's scope is private: both keep the correction as a memory row of
/// their own scope, which only they can read.
pub fn proposes_knowledge(scope: &Scope) -> bool {
    matches!(scope, Scope::Repo { tenant, .. } if tenant == super::observe::LOCAL_TENANT)
}

/// The deterministic pass over `observations` against `existing`.
pub fn pass(
    scope: &Scope,
    observations: &[Observation],
    existing: &[MemoryRecord],
    external_authors: bool,
    max_candidates: usize,
    run: u64,
    now_ms: u64,
) -> Pass {
    let considered: Vec<Observation> = observations
        .iter()
        .filter(|o| external_authors || !o.external())
        .cloned()
        .collect();
    let clusters = cluster(&considered);
    let mut evidence: Vec<(u64, Vec<u64>)> = Vec::new();
    let mut candidates: Vec<Cluster> = Vec::new();
    for cluster in clusters.iter().cloned() {
        if let Some(memory) = existing.iter().find(|m| matches(&cluster, m)) {
            match evidence.iter_mut().find(|(id, _)| *id == memory.id) {
                Some((_, ids)) => ids.extend(cluster.ids.iter().copied()),
                None => evidence.push((memory.id, cluster.ids.clone())),
            }
            continue;
        }
        if !cluster.kind.proposes() || cluster.ids.len() < CLUSTER_MIN {
            continue;
        }
        // Two candidates that match each other merge.
        let twin = candidates.iter().position(|c| {
            c.kind == cluster.kind && similarity(&c.tokens, &cluster.tokens) >= SIMILAR
        });
        match twin {
            Some(at) => candidates[at].absorb(cluster),
            None => candidates.push(cluster),
        }
    }
    candidates.truncate(max_candidates);
    let knowledge_allowed = proposes_knowledge(scope);
    let mut proposals = Vec::new();
    let mut knowledge = Vec::new();
    for cluster in candidates {
        let text = cluster.first_text();
        let plain = match text.trim_start().strip_prefix('[') {
            Some(rest) => rest.split_once("] ").map_or(text, |(_, after)| after),
            None => text,
        };
        // A short repository-wide candidate is a knowledge entry, but only
        // where the scope's repository belongs to the session alone.
        if knowledge_allowed
            && cluster.paths.is_empty()
            && cluster.kind == ObservationKind::Correction
            && plain.chars().count() <= crate::knowledge::TEXT_MAX
            && !plain.contains('\n')
        {
            knowledge.push(KnowledgeEntry {
                id: 0,
                scope: KnowledgeScope::repo(),
                text: plain.trim().to_owned(),
                source: KnowledgeSource::Consolidation {
                    run,
                    observations: cluster.ids.clone(),
                },
                status: KnowledgeStatus::Proposed,
                created_ms: now_ms,
                adopted_by: None,
                decided_ms: None,
                note: Some(format!("deterministic: {} observations", cluster.ids.len())),
            });
            continue;
        }
        proposals.push(NewMemoryRecord {
            scope: scope.clone(),
            kind: memory_kind(cluster.kind),
            title: title_of(text),
            body: body_of(&cluster),
            paths: cluster.paths.clone(),
            task_kinds: cluster.task_kinds.clone(),
            provenance: cluster.ids.clone(),
            note: Some("deterministic".into()),
            now_ms,
        });
    }
    Pass {
        clusters: clusters.len(),
        evidence,
        proposals,
        knowledge,
    }
}

/// Run one consolidation of `scope` now, as `by`, started by `trigger`.
pub(crate) fn consolidate(
    yard: &Yard,
    scope: &Scope,
    by: &str,
    trigger: &str,
) -> Result<Consolidated, Error> {
    let settings = yard.memory_settings();
    let store = yard.store();
    let memory = store.memory();
    let watermark = memory.watermark(scope)?;
    let started = now_ms();
    let run = memory.run_started(scope, watermark, by, trigger, started, started + HOLD_MS)?;
    let observations = memory.observations_since(scope, watermark, GATHER_MAX)?;
    let existing = memory.memories(&MemoryFilter {
        scope: Some(scope.clone()),
        ..MemoryFilter::default()
    })?;
    let mut pass = pass(
        scope,
        &observations,
        &existing,
        settings.external_authors,
        settings.max_candidates,
        run.id,
        started,
    );
    // The knowledge store's own duplicate rule applies to its candidates
    // (`docs/memory.md`, section 4, step 5): one it already holds, in any
    // status, is not proposed again.
    let known = store.knowledge().knowledge_entries()?;
    pass.knowledge.retain(|candidate| {
        !known
            .iter()
            .any(|e| crate::knowledge::same(e, &candidate.text, &candidate.scope))
    });
    let last = observations.last().map_or(watermark, |o| o.id);
    // The fence is renewed before the commit, as a longer pass (the
    // distiller's turn, M5) renews it while it works. A run that held its
    // scope past its hold, or whose row a later run closed as abandoned,
    // stops here instead of writing.
    let renewed_at = now_ms();
    if !memory.run_renew(&run, renewed_at, renewed_at + HOLD_MS)? {
        return Err(Error::State(format!(
            "memory run {} held its scope past its hold ({} minutes) or was closed as abandoned; \
             run the consolidation again",
            run.id,
            HOLD_MS / 60_000
        )));
    }
    let keep_ms = u64::from(settings.keep_observations_days) * 24 * 60 * 60 * 1_000;
    let counts = RunCounts {
        observations: observations.len() as u32,
        clusters: pass.clusters as u32,
        proposed: pass.proposals.len() as u32,
        merged: pass.evidence.len() as u32,
        knowledge: pass.knowledge.len() as u32,
        expired: 0,
    };
    let outcome = RunOutcome {
        watermark_written: last,
        proposals: pass.proposals,
        evidence: pass.evidence,
        knowledge: pass.knowledge,
        expire_before_ms: Some(started.saturating_sub(keep_ms)),
        counts,
        now_ms: now_ms(),
    };
    let proposed_ids = memory.run_finished(&run, &outcome)?;
    let row = memory
        .runs(Some(scope), 200)?
        .into_iter()
        .find(|r| r.id == run.id)
        .ok_or_else(|| Error::State(format!("memory run {} vanished", run.id)))?;
    let Some(ids) = proposed_ids else {
        return Ok(Consolidated {
            run: row,
            proposed: Vec::new(),
            knowledge: Vec::new(),
            lost_race: true,
            branches: Vec::new(),
        });
    };
    let mut proposed = Vec::new();
    for id in &ids {
        if let Some(record) = memory.memory(*id)? {
            proposed.push(record);
        }
    }
    let knowledge: Vec<u64> = store
        .knowledge()
        .knowledge_entries()?
        .into_iter()
        .filter(
            |e| matches!(&e.source, KnowledgeSource::Consolidation { run: r, .. } if *r == run.id),
        )
        .map(|e| e.id)
        .collect();
    // Every branch whose observations the run consumed hears of it.
    let mut per_branch: BTreeMap<String, u32> = BTreeMap::new();
    for observation in &observations {
        *per_branch.entry(observation.branch.clone()).or_default() += 1;
    }
    for (branch, count) in &per_branch {
        let activity = MemoryActivity::Consolidated {
            run: run.id,
            scope: scope.clone(),
            observations: *count,
            clusters: row.counts.clusters,
            proposed: ids.clone(),
            merged: row.counts.merged,
            by: by.to_owned(),
            trigger: trigger.to_owned(),
        };
        // A removed branch keeps no log; the run is in `memory_runs`.
        if let Ok(mut recorder) = crate::record::Recorder::open(&store, branch, None) {
            if store.read(branch).is_ok() {
                branchyard_support::best_effort(
                    "record the consolidation on the branch",
                    recorder.record(Activity::Memory(Box::new(activity))),
                );
            }
        }
    }
    Ok(Consolidated {
        run: row,
        proposed,
        knowledge,
        lost_race: false,
        branches: per_branch.into_keys().collect(),
    })
}

/// Whether `row` says another run won.
pub fn lost(row: &RunRow) -> bool {
    row.outcome.as_deref() == Some(LOST_RACE)
}

/// The scopes with observations past their watermark, for a dispatcher
/// deciding which to consolidate: every `Repo` scope with a tenant seen in
/// the store, and every `Person` scope.
pub(crate) fn due_scopes(yard: &Yard) -> Result<Vec<Scope>, Error> {
    yard.store().memory().scopes_with_observations()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_and_titles() {
        assert_eq!(
            normalize("[send] Run the  Benches with 1 MB fixture, 3 times"),
            "run the benches with # mb fixture, # times"
        );
        assert_eq!(normalize("A1B22"), "a#b#");
        assert_eq!(
            title_of("[steer] Always run cargo fmt before you finish.\nSecond line"),
            "Always run cargo fmt before you finish"
        );
        let long = "word ".repeat(40);
        let title = title_of(&long);
        assert!(title.chars().count() <= TITLE_MAX && title.ends_with("word"));
        assert_eq!(title_of("   \n"), "memory");
    }

    #[test]
    fn similarity_is_jaccard() {
        let a: BTreeSet<String> = ["run", "the", "benches"]
            .iter()
            .map(|s| (*s).into())
            .collect();
        let b: BTreeSet<String> = ["run", "the", "tests"]
            .iter()
            .map(|s| (*s).into())
            .collect();
        assert!((similarity(&a, &b) - 0.5).abs() < 1e-9);
        assert_eq!(similarity(&a, &BTreeSet::new()), 0.0);
        assert!((similarity(&a, &a) - 1.0).abs() < 1e-9);
    }
}
