//! Wire types and calls for approvals and the effect ledger: the server's
//! `/approvals` and `/effects` routes and a branch's `/undo` route. Kept in
//! its own module, like [`crate::knowledge_api`]. See `docs/effects.md`.

use branchyard::effects::reconcile::Reconciled;
use branchyard::effects::undo::{UndoOutcome, UndoPlan};
use branchyard::effects::{ApprovalAsk, EffectEntry, EffectEvent};
use branchyard::{BranchStatus, CandidateInfo, Message};
use serde::{Deserialize, Serialize};

use crate::http::encode;
use crate::{new_key, Error, Repo};

/// `GET /v1/repos/{repo}/approvals[?all=true]`: the asks waiting, or all.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ApprovalList {
    pub approvals: Vec<ApprovalAsk>,
}

/// `POST .../approvals/{id}/allow` and `.../deny`. The answer is the
/// caller's.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalAnswerRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Where the person answered: `api` (the default), `companion` or
    /// `watch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
}

/// `GET /v1/repos/{repo}/effects[?branch=B]`: the ledger, oldest first.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EffectList {
    pub effects: Vec<EffectEntry>,
}

/// `GET /v1/repos/{repo}/effects/{id}`: one entry and the events it is
/// projected from.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EffectDetail {
    pub entry: EffectEntry,
    pub events: Vec<EffectEvent>,
}

/// `POST .../effects/{id}/promote` and `.../effects/reconcile`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectActionRequest {}

/// `POST .../branches/{branch}/undo`: undo the chosen upstream effects of
/// turns after `to` (0: every turn). `only` empty: every reversible effect
/// and every staged call. The server does not rewind files; `by undo`
/// does, locally.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UndoRequest {
    #[serde(default)]
    pub to: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub only: Vec<String>,
}

/// What an undo did.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UndoReport {
    pub plan: UndoPlan,
    pub outcomes: Vec<UndoOutcome>,
}

/// `GET /v1/repos/{repo}/decisions`: what waits for a person, in the order
/// to take it. A branch parked on an approval or a question comes first,
/// the one that has waited longest before the rest, because it moves only
/// when someone answers; then the branches ready to merge, oldest first,
/// because delivery waits on them; then the stopped and the stalled, which
/// nothing waits on. Nothing here expires.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DecisionList {
    pub decisions: Vec<Decision>,
}

/// One thing a person can decide; see [`DecisionList`].
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub kind: DecisionKind,
    pub branch: String,
    /// Who waits on the person for it.
    pub waits: Waits,
    /// Since when, milliseconds since the Unix epoch, when the server
    /// knows: when the ask opened or the question was sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<u64>,
    /// The branch's status at the time.
    pub status: BranchStatus,
    /// Its candidate commit, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<CandidateInfo>,
    /// For an `approval`: the ask, as `GET …/approvals` lists it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalAsk>,
    /// For a `question`: the message, as the asker's parent's inbox holds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<Message>,
}

/// The shapes a decision takes.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    /// A running turn parked on an approval: allow or deny it.
    Approval,
    /// A question or an escalation a branch sent and nobody answered.
    Question,
    /// A ready branch: merge it, follow up, or discard it.
    Merge,
    /// A failed, interrupted or over-budget branch: resume it or let it be.
    Stop,
    /// A running turn with no harness activity for its stall window.
    Stall,
}

/// Who a decision keeps waiting.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Waits {
    /// An agent is parked mid-turn until the person answers.
    Agent,
    /// Nothing runs, but the work is not delivered until the person acts.
    Delivery,
    /// Nobody: the branch stopped, and the person decides at leisure.
    Nobody,
}

impl Repo {
    fn repo_path(&self, rest: &str) -> String {
        format!("/v1/repos/{}{rest}", encode(self.name()))
    }

    /// What waits for a person, in the order to take it; see
    /// [`DecisionList`].
    pub fn decisions(&self) -> Result<Vec<Decision>, Error> {
        Ok(self
            .client()
            .get::<DecisionList>(&self.repo_path("/decisions"))?
            .decisions)
    }

    /// The approvals waiting, or every one with `all`; like `by approvals`.
    pub fn approvals(&self, all: bool) -> Result<Vec<ApprovalAsk>, Error> {
        let path = match all {
            true => self.repo_path("/approvals?all=true"),
            false => self.repo_path("/approvals"),
        };
        Ok(self.client().get::<ApprovalList>(&path)?.approvals)
    }

    /// Allow or deny an approval as the caller.
    pub fn answer_approval(
        &self,
        id: &str,
        allow: bool,
        request: &ApprovalAnswerRequest,
    ) -> Result<ApprovalAsk, Error> {
        let verb = if allow { "allow" } else { "deny" };
        self.client().post(
            &self.repo_path(&format!("/approvals/{}/{verb}", encode(id))),
            request,
            &new_key(),
        )
    }

    /// The ledger, or one branch's; like `by effects`.
    pub fn effects(&self, branch: Option<&str>) -> Result<Vec<EffectEntry>, Error> {
        let path = match branch {
            Some(branch) => self.repo_path(&format!("/effects?branch={}", encode(branch))),
            None => self.repo_path("/effects"),
        };
        Ok(self.client().get::<EffectList>(&path)?.effects)
    }

    pub fn effect(&self, id: &str) -> Result<EffectDetail, Error> {
        self.client()
            .get(&self.repo_path(&format!("/effects/{}", encode(id))))
    }

    /// Perform a staged effect as the caller; like `by effects promote`.
    pub fn promote_effect(&self, id: &str) -> Result<EffectEntry, Error> {
        self.client().post(
            &self.repo_path(&format!("/effects/{}/promote", encode(id))),
            &EffectActionRequest::default(),
            &new_key(),
        )
    }

    /// Reconcile the ledger now; like `by effects reconcile`.
    pub fn reconcile_effects(&self) -> Result<Reconciled, Error> {
        self.client().post(
            &self.repo_path("/effects/reconcile"),
            &EffectActionRequest::default(),
            &new_key(),
        )
    }

    /// What undoing `branch` to turn `to` would do upstream; like `by undo
    /// --plan`.
    pub fn undo_plan(&self, branch: &str, to: u32) -> Result<UndoPlan, Error> {
        self.client()
            .get(&self.repo_path(&format!("/branches/{}/undo?to={to}", encode(branch))))
    }

    /// Undo `branch`'s chosen upstream effects as the caller.
    pub fn undo(&self, branch: &str, request: &UndoRequest) -> Result<UndoReport, Error> {
        self.client().post(
            &self.repo_path(&format!("/branches/{}/undo", encode(branch))),
            request,
            &new_key(),
        )
    }
}
