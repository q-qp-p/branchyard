//! Wire types and calls for memory rows, review and consolidation: the
//! server's `/memory` routes (`docs/memory.md`, sections 3 to 5). Kept in
//! its own module, like [`crate::knowledge_api`]. Memory *files* (M1's
//! bundles) have no routes: they are files in a worktree or a person's
//! folder.

use branchyard::{
    Consolidated, MemoryKind, MemoryRecord, MemoryRun, MemoryStatus, Observation, TaskKind,
};
use serde::{Deserialize, Serialize};

use crate::http::encode;
use crate::{new_key, Error, Repo};

/// Which memory a request is about: the team's (`repo`), or the caller's
/// own (`mine`), as the `scope` query parameter spells it.
pub const SCOPE_REPO: &str = "repo";
/// See [`SCOPE_REPO`].
pub const SCOPE_MINE: &str = "mine";

/// `GET /v1/repos/{repo}/memory[?scope=repo|mine][&status=...]`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryList {
    /// The rows, by id.
    pub memories: Vec<MemoryRecord>,
}

/// `GET /v1/repos/{repo}/memory/{id}/evidence`: the row and the
/// observations its provenance names, each with its author.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEvidence {
    /// The row.
    pub memory: MemoryRecord,
    /// The observations its provenance names, in id order.
    pub observations: Vec<Observation>,
}

/// `POST .../memory/{id}/approve` and `.../reject`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryDecisionRequest {
    /// Why, for a rejection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `POST .../memory/{id}/edit`: each field changes when given; an empty
/// `paths` or `task_kinds` clears it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryEditRequest {
    /// A new title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// A new body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// A new kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<MemoryKind>,
    /// New paths; an empty list clears them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    /// New task kinds; an empty list clears them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_kinds: Option<Vec<TaskKind>>,
}

/// `POST .../memory/{id}/promote`: a person's memory offered to the team.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryPromoteRequest {}

/// `POST .../memory/consolidate`: one deterministic consolidation of the
/// team's memory, or of the caller's own.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsolidateRequest {
    /// `repo` (the default) or `mine`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// `GET .../memory/runs[?scope=repo|mine]`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRuns {
    /// The runs, newest first.
    pub runs: Vec<MemoryRun>,
}

impl Repo {
    fn memory_path(&self, rest: &str) -> String {
        format!(
            "/v1/repos/{}/memory{rest}",
            encode(self.name()),
            rest = rest
        )
    }

    /// The memory rows of the team (`scope` [`SCOPE_REPO`], the default) or
    /// the caller's own ([`SCOPE_MINE`]), or those of `status`; like `by
    /// memory list --status`.
    pub fn memories(
        &self,
        scope: Option<&str>,
        status: Option<MemoryStatus>,
    ) -> Result<Vec<MemoryRecord>, Error> {
        let mut query = Vec::new();
        if let Some(scope) = scope {
            query.push(format!("scope={scope}"));
        }
        if let Some(status) = status {
            query.push(format!("status={}", status.as_str()));
        }
        let rest = match query.is_empty() {
            true => String::new(),
            false => format!("?{}", query.join("&")),
        };
        Ok(self
            .client()
            .get::<MemoryList>(&self.memory_path(&rest))?
            .memories)
    }

    /// One row; `unknown_memory` when there is none, `denied` for another person's.
    pub fn memory(&self, id: u64) -> Result<MemoryRecord, Error> {
        self.client().get(&self.memory_path(&format!("/{id}")))
    }

    /// The row and the observations it came from, each with its author.
    pub fn memory_evidence(&self, id: u64) -> Result<MemoryEvidence, Error> {
        self.client()
            .get(&self.memory_path(&format!("/{id}/evidence")))
    }

    /// Approve as the caller: the team's needs `admin` or one of the
    /// server's `memory.approvers`; a person's own needs its subject.
    pub fn approve_memory(&self, id: u64) -> Result<MemoryRecord, Error> {
        self.client().post(
            &self.memory_path(&format!("/{id}/approve")),
            &MemoryDecisionRequest::default(),
            &new_key(),
        )
    }

    /// Reject as the caller, with `reason`; the rejection sticks against new evidence.
    pub fn reject_memory(&self, id: u64, reason: Option<&str>) -> Result<MemoryRecord, Error> {
        self.client().post(
            &self.memory_path(&format!("/{id}/reject")),
            &MemoryDecisionRequest {
                reason: reason.map(str::to_owned),
            },
            &new_key(),
        )
    }

    /// Change a row's fields, each when given; an approved row's file is rewritten too.
    pub fn edit_memory(&self, id: u64, request: &MemoryEditRequest) -> Result<MemoryRecord, Error> {
        self.client().post(
            &self.memory_path(&format!("/{id}/edit")),
            request,
            &new_key(),
        )
    }

    /// Offer the caller's own memory `id` to the team: a new proposed row.
    pub fn promote_memory(&self, id: u64) -> Result<MemoryRecord, Error> {
        self.client().post(
            &self.memory_path(&format!("/{id}/promote")),
            &MemoryPromoteRequest::default(),
            &new_key(),
        )
    }

    /// Delete a row; the row as it was.
    pub fn forget_memory(&self, id: u64) -> Result<MemoryRecord, Error> {
        self.client().delete(&self.memory_path(&format!("/{id}")))
    }

    /// Run one deterministic consolidation now; like `by memory consolidate`.
    pub fn consolidate_memory(&self, scope: Option<&str>) -> Result<Consolidated, Error> {
        self.client().post(
            &self.memory_path("/consolidate"),
            &ConsolidateRequest {
                scope: scope.map(str::to_owned),
            },
            &new_key(),
        )
    }

    /// Consolidation runs, newest first.
    pub fn memory_runs(&self, scope: Option<&str>) -> Result<Vec<MemoryRun>, Error> {
        let rest = match scope {
            Some(scope) => format!("/runs?scope={scope}"),
            None => "/runs".to_owned(),
        };
        Ok(self
            .client()
            .get::<MemoryRuns>(&self.memory_path(&rest))?
            .runs)
    }
}
